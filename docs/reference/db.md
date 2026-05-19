# @zeroship/db — Database SDK

## Overview

`@zeroship/db` provides a Mongoose-inspired API backed by real Postgres columns. Creators define models with familiar schema syntax, get built-in validation, query chaining, and a `{ data, error }` return pattern. No raw SQL is exposed — the SDK calls `zeroship.db.*` native primitives, which build parameterized SQL internally.

Based on Mongoose conventions (for LLM compatibility) with key improvements:
- `{ data, error }` returns instead of throwing — explicit, no try/catch needed
- `id` not `_id` — Postgres convention, no MongoDB legacy
- Per-field update operators — `{ views: { $inc: 1 } }` reads naturally
- `createdAt`/`updatedAt` as Unix ms numbers — JS-native
- Real JOINs backed by Postgres — not N+1 populate

```javascript
import { createDb } from "@zeroship/db";

const db = createDb({
  users: {
    name:  { type: String, required: true },
    email: { type: String, required: true, unique: true },
    role:  { type: String, enum: ["user", "admin"], default: "user" },
  },
});

const { data: user, error } = await db.users.insert({ name: "Alice", email: "alice@example.com" });
const { data: admins } = await db.users.find({ role: "admin" }).sort({ name: 1 }).limit(10);
```

## Architecture

```
Creator code
  │  import { createDb } from "@zeroship/db"
  ▼
@zeroship/db (JS/TS, npm package)         ← SDK layer: validation, chaining, { data, error }
  createDb(), t, naming, Collection, Query
  │
  │  calls zeroship.db.* with per-field operator format
  ▼
zeroship.db.* (Rust, frozen global)       ← native layer: security boundary, SQL generation
  zeroship.db.find(collection, filter, opts)
  zeroship.db.insert(collection, doc)
  zeroship.db.update(collection, filter, patch)
  │
  │  validates → parameterized SQL → executes
  ▼
zeroship-pg → Postgres
```

The SDK is a standard npm package; vite/rollup bundles it into the deploy's worker code. The native `zeroship.db.*` global is registered by Rust, frozen, and enforces schema isolation + parameterized queries.

### Native primitives (`zeroship.db.*`)

Low-level "syscalls" the SDK calls. SDK authors may use these directly.

```typescript
zeroship.db.find(collection, filter, opts)          → Promise<string>
zeroship.db.findOne(collection, filter)             → Promise<string | null>
zeroship.db.insert(collection, doc)                 → Promise<string>
zeroship.db.insertMany(collection, docs)            → Promise<string>
zeroship.db.update(collection, filter, patch)   → Promise<string>
zeroship.db.updateMany(collection, filter, update)  → Promise<string>
zeroship.db.delete(collection, filter)           → Promise<string>
zeroship.db.deleteMany(collection, filter)          → Promise<string>
zeroship.db.count(collection, filter)               → Promise<string>
zeroship.db.aggregate(collection, pipeline)         → Promise<string>
zeroship.db.distinct(collection, field, filter)     → Promise<string>
```

The native layer uses per-field operator format: `{ views: { $inc: 1 } }`. The SDK translates Mongoose top-level format (`{ $inc: { views: 1 } }`) before calling native.

## Security Model

No raw SQL is exposed. All queries are parameterized.

```
Apps CAN:                              Apps CANNOT:
  CRUD (insert, find, update, delete)    Raw SQL
  Aggregate (group, sum, avg)            Access other app's schema
  Full-text search ($search)             DDL (CREATE, ALTER, DROP)
  Pagination                             System tables (pg_catalog)
```

## Schema Definition

Two styles, both produce the same internal representation.

### Mongoose style (recommended for LLM compatibility)

```javascript
import { createDb } from "@zeroship/db";

const db = createDb({
  users: {
    name:     { type: String, required: true, minlength: 1, maxlength: 100 },
    email:    { type: String, required: true, unique: true, match: /^[^@]+@[^@]+$/ },
    age:      { type: Number, min: 0, max: 150 },
    role:     { type: String, enum: ["user", "admin", "moderator"], default: "user" },
    bio:      { type: String },
    settings: { type: Object },
    tags:     { type: [String] },
  },
});

// Shorthand also works (bare constructors)
const db2 = createDb({
  simple: {
    name: String,
    count: Number,
    active: Boolean,
    tags: [String],
  },
});
```

### Builder style (alternative)

```javascript
import { createDb, t } from "@zeroship/db";

const db = createDb({
  users: {
    name:     t.string().required().min(1).max(100),
    email:    t.string().required().unique().pattern(/^[^@]+@[^@]+$/),
    age:      t.number().min(0).max(150),
    role:     t.string().enum("user", "admin", "moderator").default("user"),
    bio:      t.string(),
    settings: t.json(),
    tags:     t.array(t.string()),
  },
});
```

### Type mapping

| JS Constructor | Builder | Postgres |
|---|---|---|
| `String` | `t.string()` | TEXT |
| `Number` | `t.number()` | NUMERIC |
| `Boolean` | `t.boolean()` | BOOLEAN |
| `Date` | `t.timestamp()` | TIMESTAMPTZ |
| `Object` | `t.json()` | JSONB |
| `[String]` | `t.array(t.string())` | JSONB |

### Validators

| Validator | Mongoose syntax | Builder syntax | Applies to |
|---|---|---|---|
| required | `required: true` | `.required()` | all |
| default | `default: value` | `.default(value)` | all |
| unique | `unique: true` | `.unique()` | all |
| min | `min: 0` | `.min(0)` | Number |
| max | `max: 150` | `.max(150)` | Number |
| minlength | `minlength: 1` | `.min(1)` | String (length) |
| maxlength | `maxlength: 100` | `.max(100)` | String (length) |
| match | `match: /regex/` | `.pattern(/regex/)` | String |
| enum | `enum: ["a", "b"]` | `.enum("a", "b")` | String |
| index | `index: true` | `.index()` | all |

### Auto-generated fields

Not declared by creator. Added by the database automatically:

- `id` — `SERIAL PRIMARY KEY` or `UUID PRIMARY KEY DEFAULT gen_random_uuid()`
- `createdAt` — mapped from `created_at TIMESTAMPTZ DEFAULT NOW()` — returned as Unix ms number
- `updatedAt` — mapped from `updated_at TIMESTAMPTZ DEFAULT NOW()` — returned as Unix ms number

## Return Pattern

Every SDK method returns `{ data, error }` — never throws.

```javascript
// Success
const { data, error } = await users.insert({ name: "Alice", email: "a@b.com" });
// data = { id: 1, name: "Alice", email: "a@b.com", role: "user", createdAt: 1713000000000, updatedAt: 1713000000000 }
// error = null

// Failure
const { data, error } = await users.insert({ name: "" });
// data = null
// error = { name: "ValidationError", errors: { name: { message: "required", path: "name" } } }

// Duplicate key
const { data, error } = await users.insert({ name: "Alice", email: "existing@b.com" });
// data = null
// error = { code: 11000, message: "duplicate key error: email" }
```

No try/catch needed. Errors are values, not exceptions.

## Complete API Reference

### Create

```javascript
// Insert one
const { data } = await users.insert({ name: "Alice", email: "alice@example.com" });
// → { id: 1, name: "Alice", email: "alice@example.com", role: "user", createdAt: ..., updatedAt: ... }

// Insert (alias for create)
const { data } = await users.insert({ name: "Alice", email: "alice@example.com" });

// Insert many
const { data } = await users.insertMany([
  { name: "Bob", email: "bob@example.com" },
  { name: "Carol", email: "carol@example.com" },
]);
// → [{ id: 2, ... }, { id: 3, ... }]
```

### Read

```javascript
// Find one — returns document or null
const { data } = await users.findOne({ email: "alice@example.com" });
// → { id, name, email, ... } or null

// Find by ID (shorthand)
const { data } = await users.get(1);

// Find many — returns Query (thenable), supports chaining
const { data } = await users
  .find({ role: "admin" })
  .select("name email")           // projection: only these fields
  .sort({ name: 1 })              // 1 = ASC, -1 = DESC
  .limit(20)                       // max results
  .skip(40);                       // offset
// → [{ name, email }, ...]

// Count
const { data: count } = await users.count({ role: "admin" });
// → 5

// Distinct values
const { data: roles } = await users.distinct("role");
// → ["user", "admin", "moderator"]

// Exists
const { data: exists } = await users.exists({ email: "alice@example.com" });
// → true
```

### Update

```javascript
// Update one — returns the updated document (or null if nothing matched).
// First arg is an id (shorthand for `{ id }`) or a filter object.
const { data: user } = await users.update(1, { name: "Alice Smith", role: "admin" });
if (user === null) throw new Error("not found");

// Update many — returns { matchedCount, modifiedCount }
const { data } = await users.updateMany({ role: "user" }, { role: "member" });
// → { matchedCount: 342, modifiedCount: 342 }

// Per-field atomic operators
const { data } = await products.update(1, {
  stock: { $dec: 1 },
  sold:  { $inc: 1 },
  tags:  { $push: "sale" },
});

// MongoDB-style top-level operators are also accepted (translated by the SDK)
const { data } = await products.update(1, {
  $inc: { views: 1 },
  $push: { tags: "popular" },
});

// Optimistic-lock (compound filter — match-and-update atomically)
const { data: stocked } = await products.update(
  { id: 1, stock: { $gte: 1 } },
  { stock: { $dec: 1 } }
);
if (stocked === null) throw new Error("Out of stock");
```

### Delete

```javascript
// Delete one — returns the deleted document (or null if nothing matched).
const { data: deleted } = await users.delete(1);
if (deleted === null) throw new Error("not found");

// Delete many — returns { deletedCount }
const { data } = await sessions.deleteMany({ expiresAt: { $lt: Date.now() } });
// → { deletedCount: 42 }

// Soft-delete collections — `delete` flips `deletedAt`; pass `{ hard: true }`
// to bypass the soft semantics and permanently remove the row.
await users.delete(1, { hard: true });
```

### Aggregate

```javascript
const { data } = await products.aggregate([
  { $match: { status: "active" } },
  { $group: { by: "category", count: { $count: true }, avgPrice: { $avg: "price" } } },
  { $having: { count: { $gt: 5 } } },
  { $sort: { count: -1 } },
  { $limit: 10 },
]);
// → [{ category: "food", count: 42, avgPrice: 5.5 }]

// Mongoose-style aggregate also works (SDK translates)
const { data } = await products.aggregate([
  { $match: { status: "active" } },
  { $group: { _id: "$category", count: { $sum: 1 }, avgPrice: { $avg: "$price" } } },
  { $sort: { count: -1 } },
]);
```

### Aggregation operators

```javascript
{ $count: true }                 // COUNT(*)
{ $sum: "field" }                // SUM(field)
{ $avg: "field" }                // AVG(field)
{ $min: "field" }                // MIN(field)
{ $max: "field" }                // MAX(field)
```

## Filter Operators

```javascript
// Comparison
{ field: value }                    // WHERE field = value (implicit eq)
{ field: { $gt: n } }              // WHERE field > n
{ field: { $gte: n } }             // WHERE field >= n
{ field: { $lt: n } }              // WHERE field < n
{ field: { $lte: n } }             // WHERE field <= n
{ field: { $ne: value } }          // WHERE field != value

// Inclusion
{ field: { $in: [1, 2, 3] } }      // WHERE field IN (1, 2, 3)
{ field: { $nin: [1, 2] } }        // WHERE field NOT IN (1, 2)

// Pattern
{ field: { $like: "%pattern%" } }   // WHERE field LIKE '%pattern%'
{ field: { $ilike: "%pattern%" } }  // WHERE field ILIKE '%pattern%' (case insensitive)

// Full-text search
{ field: { $search: "chocolate cake" } }

// Null
{ field: null }                     // WHERE field IS NULL
{ field: { $ne: null } }           // WHERE field IS NOT NULL

// Logical
{ $and: [f1, f2] }                 // AND
{ $or: [f1, f2] }                  // OR
{ $not: filter }                   // NOT

// Implicit AND
{ role: "admin", age: { $gte: 18 } }
// → WHERE role = 'admin' AND age >= 18
```

## Update Operators

Per-field format (native):

```javascript
{ field: value }                    // field = value (implicit set)
{ field: { $set: value } }         // field = value (explicit set)
{ field: { $inc: n } }             // field = field + n
{ field: { $dec: n } }             // field = field - n
{ field: { $mul: n } }             // field = field * n
{ field: { $push: value } }        // JSONB array append
{ field: { $pull: value } }        // JSONB array remove
{ field: { $addToSet: value } }    // JSONB array append if not present
```

Mongoose top-level format (also accepted, SDK translates):

```javascript
{ $set: { name: "Bob" } }
{ $inc: { views: 1 } }
{ $push: { tags: "new" } }
```

## Per-App Isolation

Each app gets its own Postgres schema (UUID-based):

```sql
SELECT * FROM "app-uuid"."users" WHERE ...
```

The `app_id` is injected by the Rust runtime from `env_vars`, not from user code. `globalThis.zeroship` is frozen — creators cannot override the schema.

## Validation

Runs in the SDK (JS) before every `create()`, `insertMany()`, `updateOne()`, `updateMany()`.

```javascript
// Fails validation — returns error, doesn't hit database
const { error } = await users.insert({ name: "" });
// error = { name: "ValidationError", errors: { name: { message: "name is required", path: "name" } } }

// Type mismatch
const { error } = await users.insert({ name: "Alice", age: "thirty" });
// error = { name: "ValidationError", errors: { age: { message: "age must be a number", path: "age" } } }

// On update: only validates provided fields (partial)
const { error } = await users.update({ id: 1 }, { age: -1 });
// error = { name: "ValidationError", errors: { age: { message: "age must be at least 0", path: "age" } } }
```

## Error Codes

| Code | When |
|---|---|
| `ValidationError` | Input fails schema validation |
| `11000` | Duplicate on unique field (Mongoose-compatible code) |
| DB error message | Other Postgres errors |

## Connection Architecture

```
Worker thread:
  ntex handler → V8 isolate → @zeroship/db → zeroship.db.find()
    → Rust native callback
    → validate collection + filter + operators
    → build parameterized SQL (text-format params)
    → query_text_params via Rc<Pool> (per-thread)
    → zeroship-pg → Postgres → rows → JSON → V8

Per-thread: 8 idle connections, shared across all isolates
32 threads × 8 connections = 256 max per worker
```

## Implementation

```
SDK (sdks/db/):
  src/index.ts        — export { model, t }
  src/model.ts        — model(name, schema) → Collection
  src/collection.ts   — Collection class with { data, error } methods
  src/query.ts        — Query thenable (sort/limit/skip/select)
  src/schema.ts       — normalizeSchema() for both styles
  src/types.ts        — t builder + TypeBuilder + FieldDef
  src/validate.ts     — validateDoc(), validatePartial()
  src/errors.ts       — ValidationError, mapNativeError()
  src/utils.ts        — field mapping, aggregate translation

Native (crates/plugin-db/):
  src/lib.rs          — DbPlugin (NativePlugin trait)
  src/callbacks.rs    — V8 callbacks for zeroship.db.*
  src/query.rs        — filter/update/aggregate JSON → parameterized SQL

214 SDK tests (unit + robustness)
89 native tests (69 unit + 20 integration)
```

## What's Implemented

- 11 native primitives (find, findOne, insert, insertMany, updateOne, updateMany, deleteOne, deleteMany, count, distinct, aggregate)
- Full filter operators ($eq, $ne, $gt, $gte, $lt, $lte, $in, $nin, $like, $ilike, $search, $and, $or, $not)
- Full update operators ($set, $inc, $dec, $mul, $push, $pull, $addToSet)
- Aggregation pipeline ($match, $group, $having, $sort, $limit) with $count, $sum, $avg, $min, $max
- Schema definition (both Mongoose and builder styles)
- Validation (required, type, min/max, enum, pattern)
- Query chaining (.find().sort().limit().skip().select())
- Field mapping (id↔_id equivalent, camelCase↔snake_case for auto fields)
- E2E tested through full platform pipeline

## What's Implemented

- `{ data, error }` return pattern on all Collection methods
- `createDb()` with typed collections and `const T` inference
- `findById(id)`, `exists(filter)`, `countDocuments(filter)`
- Transactions: `db.transaction(async (tx) => { ... })` with TxCollection (throws on error)
- Auto-migration: `registerModel` creates schemas/tables/columns on cold start
- TypeBuilder API: `t.string().required().min(3)` with full generic inference
- Naming strategy: `naming.snakeCase` (default), `naming.asIs`, or custom
- Typed filters (`Filter<S>`), typed updates (`UpdateExpression<S>`), typed rows (`Row<S>`)
- Aggregate pipeline translation: `$group`, `$match`, `$having`, `$sort`, `$limit`
- Accumulators: `$count`, `$sum`, `$avg`, `$min`, `$max`, `$first`

## What's Deferred

- `findOneAndUpdate` / `findOneAndDelete` — atomic read-modify-return
- Populate / lookup (JOINs)
- Soft delete (`deletedAt` field + automatic filtering)
- Cursor pagination (`{ after: lastId }` → `WHERE id > $1 LIMIT $2`)
- Upsert (`INSERT ... ON CONFLICT DO UPDATE`)
- Type-safe `select()` return type narrowing
- Transaction isolation levels (`SERIALIZABLE`, `REPEATABLE READ`)
- Realtime subscriptions
- `OpResult::Failed` in Rust runtime (proper promise rejection instead of error envelope)
- `$first` sort-order threading into `array_agg ORDER BY`
