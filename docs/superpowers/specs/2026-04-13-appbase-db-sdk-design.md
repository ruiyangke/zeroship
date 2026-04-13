# @zeroship/db SDK Design

## Goal

A Mongoose-compatible JS/TS SDK that wraps `zeroship.db.*` native primitives. LLMs generate working code on first try because the API mirrors MongoDB/Mongoose — the most-documented database API in training data.

## Architecture

```
Creator code
  │
  │ import { model, t } from "@zeroship/db"
  ▼
@zeroship/db (JS — this SDK)
  model(), t, Collection, Query, validate
  │
  │ calls zeroship.db.* (JSON in, JSON out)
  ▼
zeroship.db.* (Rust native primitives)
  parameterized SQL → Postgres
```

The SDK is a standard npm package. The compiler (esbuild) bundles it into the .appbundle. It runs inside the V8 isolate alongside user code.

## Location

`sdks/db/` with `package.json` name `@zeroship/db`.

## Schema Definition

Two styles, both produce the same internal representation.

### Mongoose style

```js
import { model } from "@zeroship/db";

const users = model("users", {
  name:  { type: String, required: true },
  email: { type: String, required: true, unique: true },
  age:   { type: Number, min: 0, max: 150 },
  role:  { type: String, enum: ["user", "admin"], default: "user" },
  bio:   { type: String },
  tags:  { type: [String] },
  settings: { type: Object },
});
```

### Builder style

```js
import { model, t } from "@zeroship/db";

const users = model("users", {
  name:  t.string().required(),
  email: t.string().required().unique(),
  age:   t.number().min(0).max(150),
  role:  t.string().enum("user", "admin").default("user"),
  bio:   t.string(),
  tags:  t.array(t.string()),
  settings: t.json(),
});
```

### Detection

`model()` calls `normalizeSchema()` which checks each field value:
- If it has a `type` property → Mongoose style
- If it's a `TypeBuilder` instance → builder style
- Mixed is allowed (different fields can use different styles)

### Normalized internal format

```js
{
  name:  { type: "string", required: true },
  email: { type: "string", required: true, unique: true },
  age:   { type: "number", min: 0, max: 150 },
  role:  { type: "string", enum: ["user", "admin"], default: "user" },
  bio:   { type: "string" },
  tags:  { type: "array", items: "string" },
  settings: { type: "json" },
}
```

### Type mapping

| Schema type | JS constructor | Builder | Postgres |
|---|---|---|---|
| `string` | `String` | `t.string()` | TEXT |
| `number` | `Number` | `t.number()` | NUMERIC |
| `boolean` | `Boolean` | `t.boolean()` | BOOLEAN |
| `date` | `Date` | `t.date()` | TIMESTAMPTZ |
| `json` | `Object` | `t.json()` | JSONB |
| `array` | `[Type]` | `t.array(t.string())` | JSONB |

### Auto-generated fields

Not declared by the creator, expected from Postgres defaults:
- `_id` — mapped from `id` column (UUID or SERIAL)
- `createdAt` — mapped from `created_at` column
- `updatedAt` — mapped from `updated_at` column

The SDK renames these in results: `id` → `_id`, `created_at` → `createdAt`, `updated_at` → `updatedAt`.

## Collection API

`model()` returns a Collection instance. Methods mirror Mongoose:

### Create

```js
// Insert one — returns the created document
const user = await users.create({ name: "Alice", email: "alice@example.com" });
// → { _id: "uuid", name: "Alice", email: "alice@example.com", role: "user", createdAt: ..., updatedAt: ... }

// Insert many — returns array
const created = await users.insertMany([
  { name: "Bob", email: "bob@example.com" },
  { name: "Carol", email: "carol@example.com" },
]);
```

### Read

```js
// Find one
const user = await users.findOne({ email: "alice@example.com" });

// Find many — returns Query (thenable)
const admins = await users.find({ role: "admin" })
  .sort({ name: 1 })
  .limit(10)
  .skip(20)
  .select("name email");

// Count
const total = await users.countDocuments({ role: "admin" });

// Distinct
const roles = await users.distinct("role");
```

### Update

```js
// Update one — returns { matchedCount, modifiedCount }
await users.updateOne({ _id: id }, { $set: { name: "Alice S" } });

// Update many
await users.updateMany({ role: "user" }, { $set: { role: "member" } });

// Atomic operators
await products.updateOne({ _id: id }, {
  $inc: { views: 1 },
  $push: { tags: "sale" },
});
```

### Delete

```js
// Delete one — returns { deletedCount }
await users.deleteOne({ _id: id });

// Delete many
await sessions.deleteMany({ expiresAt: { $lt: Date.now() } });
```

### Aggregate

```js
const stats = await products.aggregate([
  { $match: { status: "active" } },
  { $group: { _id: "$category", count: { $sum: 1 }, avgPrice: { $avg: "$price" } } },
  { $sort: { count: -1 } },
  { $limit: 10 },
]);
```

Note: aggregate uses `_id` for the group key (MongoDB convention). The SDK translates `_id` → `by` and `$sum: 1` → `$count: true` before calling the native `zeroship.db.aggregate`.

## Query Class

`.find()` returns a Query object (thenable, not a Promise). Chains are collected, execution happens on `await`.

```js
class Query {
  sort(obj)    → Query   // { field: 1 } ASC, { field: -1 } DESC
  limit(n)     → Query
  skip(n)      → Query
  select(s)    → Query   // "name email" or ["name", "email"]
  then(resolve, reject)  // triggers _exec() → zeroship.db.find()
}
```

`select()` accepts Mongoose-style space-separated string or array.

## Validation

Runs in JS before every `create()`, `insertMany()`, `updateOne()`, `updateMany()`.

### Rules

| Constraint | Check |
|---|---|
| `required` | field present and not null/undefined |
| `type` | typeof matches (string, number, boolean) |
| `min` | string: `.length >= min`, number: `>= min` |
| `max` | string: `.length <= max`, number: `<= max` |
| `enum` | value in allowed list |
| `pattern` | string matches regex |

### On insert

Validates all fields. Applies defaults for missing optional fields.

### On update

Validates only the fields being updated (partial validation). `$inc`/`$dec`/`$mul` skip type check (they're numeric operations). `$push`/`$addToSet` validate the value against the array item type.

### Error format

```js
try {
  await users.create({ name: "" });
} catch (e) {
  e.name;     // "ValidationError"
  e.errors;   // { name: { message: "required", path: "name" } }
}
```

Matches Mongoose's `ValidationError` shape.

## Error Mapping

Map native errors to Mongoose-compatible codes:

| Native error | SDK error | `code` |
|---|---|---|
| unique_violation | duplicate key | `11000` |
| Validation failure | ValidationError | — |
| Other Postgres error | Error with message | — |

```js
try {
  await users.create({ email: "existing@example.com" });
} catch (e) {
  if (e.code === 11000) { /* handle duplicate */ }
}
```

## Field Name Mapping

The SDK maps between JS conventions (camelCase) and Postgres conventions (snake_case) for auto-generated fields only:

| JS (SDK) | Postgres (native) |
|---|---|
| `_id` | `id` |
| `createdAt` | `created_at` |
| `updatedAt` | `updated_at` |

User-defined fields are NOT mapped — they pass through as-is. If the creator defines `firstName`, that's the column name.

The mapping happens in two places:
- **Outbound** (before native call): `_id` in filters → `id`
- **Inbound** (after native result): `id` → `_id`, `created_at` → `createdAt`

## Aggregate Translation

MongoDB aggregate uses `_id` for group key and `$field` syntax. The SDK translates to our native format:

```js
// Creator writes (MongoDB style):
{ $group: { _id: "$category", count: { $sum: 1 } } }

// SDK translates to (native format):
{ $group: { by: "category", count: { $count: true } } }

// Rules:
// _id → by
// "$fieldName" → "fieldName" (strip $)
// { $sum: 1 } → { $count: true }
```

## File Structure

```
sdks/db/
  src/
    index.ts        — export { model, Schema, t }
    schema.ts       — normalizeSchema(), detect Mongoose vs builder
    types.ts        — t.string(), t.number(), t.boolean(), t.date(), t.json(), t.array()
    model.ts        — model(name, schema) → Collection
    collection.ts   — Collection class with all CRUD methods
    query.ts        — Query thenable with sort/limit/skip/select
    validate.ts     — validateDoc(), validatePartial()
    errors.ts       — ValidationError, DuplicateKeyError
    utils.ts        — field mapping (_id↔id, camelCase↔snake_case)
  package.json      — { "name": "@zeroship/db", "main": "src/index.ts" }
  tsconfig.json
```

## Testing

Unit tests in `sdks/db/src/__tests__/`:
- `schema.test.ts` — both schema styles normalize correctly
- `types.test.ts` — t builder produces correct definitions
- `validate.test.ts` — all validation rules
- `query.test.ts` — chain building, select parsing
- `collection.test.ts` — method calls produce correct native calls (mock zeroship.db.*)
- `errors.test.ts` — error mapping

Integration test: deploy an app using the SDK through the full platform pipeline.

## Deferred

- Populate / lookup (JOINs)
- Transactions
- Soft delete
- Cursor pagination
- findOneAndUpdate / findOneAndDelete
- Middleware / hooks (pre/post save)
- Virtuals
- TypeScript generics for model types
