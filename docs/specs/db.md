# appbase:db — Database Module

## Overview

`appbase:db` provides a document-style API backed by real Postgres columns. Creators define models with a type-safe builder (`t.string().required()`), get automatic schema migrations on deploy, built-in validation, and a MongoDB-like query API. Raw SQL is available as an escape hatch.

```javascript
import { model, t, db } from "appbase:db";

const users = model("users", {
  name:  t.string().required(),
  email: t.string().required().unique(),
  age:   t.number(),
  role:  t.string().default("user"),
});

const user = await users.insert({ name: "Alice", email: "alice@example.com" });
const admins = await users.find({ role: "admin" }).sort({ name: 1 }).limit(10);
```

## Module Resolution

`appbase:db` is a built-in module resolved by the V8 module system. When the runtime encounters `import ... from "appbase:db"`, it loads the embedded `db.js` polyfill which calls native Rust callbacks.

```
import { model, t, db } from "appbase:db"
  → V8 resolve_callback detects "appbase:" prefix
  → loads embed/db.js (compiled into binary)
  → db.js calls __dbQuery, __dbExecute, __dbInsert, etc. (native callbacks)
  → native callbacks use appbase-pg → Postgres
```

## Schema Definition

### Type Builder (`t`)

```javascript
// Primitive types
t.string()              // TEXT
t.text()                // TEXT (alias)
t.number()              // NUMERIC
t.integer()             // INTEGER
t.boolean()             // BOOLEAN
t.date()                // TIMESTAMPTZ
t.json()                // JSONB (for unstructured data)
t.array(t.string())     // JSONB array (validated as array of type)

// Reference (foreign key)
t.ref(otherModel)       // UUID REFERENCES other_table(id)

// Modifiers (chainable, returns new builder — immutable)
.required()             // NOT NULL
.unique()               // UNIQUE constraint
.index()                // CREATE INDEX on this column
.default(value)         // DEFAULT value in Postgres
.check(expr)            // CHECK constraint (raw SQL expression)

// Validation modifiers (enforced at runtime before insert/update)
.min(n)                 // string: min length, number: min value
.max(n)                 // string: max length, number: max value
.pattern(regex)         // string: must match regex
.enum("a", "b", "c")   // must be one of these values
.trim()                 // auto-trim whitespace before insert
```

### Model Definition

```javascript
import { model, t } from "appbase:db";

const users = model("users", {
  name:     t.string().required().min(1).max(100),
  email:    t.string().required().unique().pattern(/^[^@]+@[^@]+$/),
  age:      t.number().integer().min(0).max(150),
  role:     t.string().enum("user", "admin", "moderator").default("user"),
  bio:      t.text(),
  avatar:   t.string(),
  settings: t.json(),
  tags:     t.array(t.string()),
});
```

Every model gets these columns automatically (not declared by the creator):
- `id` — `UUID PRIMARY KEY DEFAULT gen_random_uuid()`
- `created_at` — `TIMESTAMPTZ DEFAULT NOW()`
- `updated_at` — `TIMESTAMPTZ DEFAULT NOW()` (updated on every UPDATE)

### Generated SQL

The model above generates:

```sql
CREATE TABLE app_{app_id}.users (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name       TEXT NOT NULL CHECK (length(name) >= 1 AND length(name) <= 100),
    email      TEXT NOT NULL UNIQUE,
    age        INTEGER CHECK (age >= 0 AND age <= 150),
    role       TEXT DEFAULT 'user' CHECK (role IN ('user', 'admin', 'moderator')),
    bio        TEXT,
    avatar     TEXT,
    settings   JSONB DEFAULT '{}',
    tags       JSONB DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

Real columns. Real types. Real indexes. JSONB only for `t.json()` and `t.array()`.

## Per-App Isolation

Each app gets its own Postgres schema (not database):

```sql
CREATE SCHEMA app_abc123;
SET search_path TO app_abc123;
```

Apps cannot access each other's tables. The runtime sets `search_path` before every query based on the app_id from the request context.

## CRUD API

### Insert

```javascript
const user = await users.insert({
  name: "Alice",
  email: "alice@example.com",
});
// → { id: "uuid", name: "Alice", email: "alice@example.com", role: "user", created_at: "...", ... }

// Insert many
const created = await users.insertMany([
  { name: "Bob", email: "bob@example.com" },
  { name: "Carol", email: "carol@example.com" },
]);
// → [{ id: "uuid", ... }, { id: "uuid", ... }]
```

Validation runs before INSERT. Missing required fields, wrong types, constraint violations → throws with a clear error message before hitting Postgres.

### Find

```javascript
// Find one
const user = await users.findOne({ email: "alice@example.com" });
// → { id, name, email, ... } or null

// Find many with chaining
const results = await users
  .find({ role: "admin" })
  .sort({ name: 1 })     // 1 = ASC, -1 = DESC
  .limit(20)
  .skip(40);              // offset for pagination
// → [{ id, name, ... }, ...]

// Cursor-based pagination (preferred over skip)
const page = await users
  .find({ role: "user" })
  .sort({ created_at: -1 })
  .after(lastCursor)      // created_at of last item from previous page
  .limit(20);
```

### Update

```javascript
// Update by filter
await users.update(
  { id: "uuid" },
  { name: "Alice Smith", role: "admin" }
);
// → { updated: 1 }

// Atomic operations
await products.update(
  { id: "uuid" },
  {
    stock: { $inc: -1 },        // decrement
    sold: { $inc: 1 },          // increment
    tags: { $push: "sale" },    // append to array
    oldTags: { $pull: "new" },  // remove from array
  }
);

// Update with optimistic lock (conditional)
const result = await products.update(
  { id: "uuid", stock: { $gte: 1 } },  // only if stock >= 1
  { stock: { $dec: 1 } }
);
if (result.updated === 0) throw new Error("Out of stock");
```

### Delete

```javascript
await users.delete({ id: "uuid" });
// → { deleted: 1 }

// Delete many
await sessions.delete({ expires_at: { $lt: new Date() } });
// → { deleted: 42 }
```

### Count / Exists

```javascript
const total = await users.count({ role: "admin" });
// → 5

const exists = await users.exists({ email: "alice@example.com" });
// → true
```

## Filter Operators

```javascript
// Comparison
{ field: value }                    // equality: WHERE field = value
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
{ field: { $ilike: "%pattern%" } }  // WHERE field ILIKE '%pattern%' (case-insensitive)

// Null
{ field: null }                     // WHERE field IS NULL
{ field: { $ne: null } }           // WHERE field IS NOT NULL

// Logical
{ $and: [filter1, filter2] }       // AND
{ $or: [filter1, filter2] }        // OR
{ $not: filter }                   // NOT

// Multiple conditions on same document (implicit AND)
{ role: "admin", age: { $gte: 18 } }
// → WHERE role = 'admin' AND age >= 18
```

## Update Operators

```javascript
// Set fields
{ field: value }                    // SET field = value (implicit $set)
{ field: { $set: value } }         // SET field = value (explicit)

// Numeric
{ field: { $inc: n } }             // SET field = field + n
{ field: { $dec: n } }             // SET field = field - n
{ field: { $mul: n } }             // SET field = field * n

// Array (for JSONB array columns)
{ field: { $push: value } }        // append to array
{ field: { $pull: value } }        // remove from array
{ field: { $addToSet: value } }    // append only if not present
```

## Transactions

```javascript
await db.transaction(async (tx) => {
  // tx provides the same collection API
  const order = await tx.orders.insert({
    customer: userId,
    total: 0,
    status: "pending",
  });

  let total = 0;
  for (const item of cart) {
    const result = await tx.products.update(
      { id: item.productId, stock: { $gte: item.qty } },
      { stock: { $dec: item.qty } }
    );
    if (result.updated === 0) throw new Error("Out of stock");
    total += item.price * item.qty;
  }

  await tx.orders.update({ id: order.id }, { total });

  return order;
  // auto-commits on return
  // auto-rollbacks on throw
});
```

Transactions use Postgres `BEGIN`/`COMMIT`/`ROLLBACK`. All operations within `tx` run on the same database connection.

## Raw SQL

```javascript
// For complex queries the document API can't express
const result = await db.query(
  "SELECT category, COUNT(*), AVG(price) FROM products GROUP BY category HAVING COUNT(*) > 5",
  []
);
// → { rows: [{ category: "food", count: 12, avg: 5.50 }, ...], rowCount: 3 }

const affected = await db.execute(
  "UPDATE products SET price = price * 1.1 WHERE category = $1",
  ["electronics"]
);
// → { rowCount: 42 }
```

Raw SQL always runs within the app's schema (`search_path = app_{app_id}`). Apps cannot escape their schema.

## Validation

### Automatic (on every insert/update)

```javascript
const users = model("users", {
  name:  t.string().required().min(1).max(100),
  email: t.string().required().unique(),
  age:   t.number().integer().min(0),
});

await users.insert({ name: "", email: "a@b.com" });
// Error: { field: "name", message: "must be at least 1 character" }

await users.insert({ name: "Alice", email: "a@b.com", age: "thirty" });
// Error: { field: "age", message: "expected number, got string" }

await users.insert({ name: "Alice" });
// Error: { field: "email", message: "required" }
```

Validation runs in the native callback before SQL is generated. Errors are returned as structured objects, not SQL error strings.

### Explicit validation

```javascript
// Validate without inserting
const result = users.validate({ name: "", age: -1 });
// {
//   ok: false,
//   errors: [
//     { field: "name", message: "must be at least 1 character" },
//     { field: "email", message: "required" },
//     { field: "age", message: "must be >= 0" },
//   ]
// }

// Partial validation (for updates)
const result = users.validate({ age: -1 }, { partial: true });
// { ok: false, errors: [{ field: "age", message: "must be >= 0" }] }
```

### Zod interop (optional)

```javascript
import { z } from "zod";  // npm install zod (creator's choice)

// Generate a Zod schema from the model
const UserInput = users.toZod(z);
// Equivalent to: z.object({ name: z.string().min(1).max(100), email: z.string(), ... })

// Use Zod's ecosystem (transforms, refinements, etc.)
const parsed = UserInput.parse(input);
```

## Auto-Migration

On every deploy, the platform compares the old model definition with the new one and generates migration SQL.

### Safe migrations (automatic)

| Change | SQL generated |
|---|---|
| Add field | `ALTER TABLE ADD COLUMN` |
| Add field with default | `ALTER TABLE ADD COLUMN ... DEFAULT` |
| Add index | `CREATE INDEX` |
| Remove index | `DROP INDEX` |
| Add new model (collection) | `CREATE TABLE` |
| Change default value | `ALTER TABLE ALTER COLUMN SET DEFAULT` |
| Add check constraint | `ALTER TABLE ADD CONSTRAINT` |

### Dangerous migrations (blocked with error)

| Change | Risk | Workaround |
|---|---|---|
| Remove required field | Data loss | Make optional first, then remove |
| Change field type | Cast failure | Add new field, migrate data, remove old |
| Add required field without default | Existing rows fail | Add with default, then remove default |
| Rename field | Data loss | Add new, copy, remove old |
| Delete model (collection) | Data loss | Manual: `db.execute("DROP TABLE ...")` |

The platform stores the previous model definition per app. On deploy, it diffs old → new and generates ALTER statements. If a dangerous change is detected, the deploy fails with an error explaining how to do it safely.

## Indexes

```javascript
// Single-field index
const products = model("products", {
  category: t.string().index(),        // CREATE INDEX on category
  price:    t.number(),
});

// Compound index (declared on the model)
const orders = model("orders", {
  customer: t.ref(users).required(),
  status:   t.string().required(),
  created_at: t.date(),
}, {
  indexes: [
    { fields: ["customer", "status"] },                    // compound index
    { fields: ["created_at"], order: "DESC" },             // descending
    { fields: ["status"], where: "status != 'archived'" }, // partial index
  ]
});
```

## References (Foreign Keys)

```javascript
const users = model("users", {
  name: t.string().required(),
});

const posts = model("posts", {
  title:  t.string().required(),
  author: t.ref(users).required(),        // FK to users.id
  parent: t.ref("posts"),                 // self-reference (replies)
});

// Insert with reference
const post = await posts.insert({
  title: "Hello",
  author: user.id,       // pass the UUID
});

// Query with reference (no auto-join — explicit)
const post = await posts.findOne({ id: "..." });
const author = await users.findOne({ id: post.author });

// For joins, use raw SQL
const result = await db.query(`
  SELECT p.title, u.name as author_name
  FROM posts p JOIN users u ON p.author = u.id
  WHERE p.id = $1
`, [postId]);
```

References create real Postgres foreign keys with `ON DELETE` behavior:
- `t.ref(users)` → `ON DELETE SET NULL` (nullable ref)
- `t.ref(users).required()` → `ON DELETE CASCADE` (required ref — delete parent deletes children)

## Connection Architecture

```
Worker thread:
  ├─ ntex HTTP handler receives /dispatch/{app_id}
  ├─ V8 isolate executes app code
  │   └─ import { db } from "appbase:db"
  │       └─ db.query() → __dbQuery native callback
  │           └─ RuntimeState.db_pool (Rc<Pool>, per-thread)
  │               └─ SET search_path TO app_{app_id}
  │               └─ Execute SQL
  │               └─ Return rows to V8
  └─ Response

Per-thread pool (Rc-based, appbase-pg):
  - 8 idle connections shared across all isolates on the thread
  - Connection acquired → SET search_path → query → return connection
  - 32 threads × 8 connections = 256 max Postgres connections per worker
```

## Metering

Every database operation increments billing counters via the `Meter` trait:

```
insert()      → meter.increment("db.writes", 1)
insertMany(n) → meter.increment("db.writes", n)
find()        → meter.increment("db.reads", 1)
findOne()     → meter.increment("db.reads", 1)
update()      → meter.increment("db.writes", 1)
delete()      → meter.increment("db.writes", 1)
count()       → meter.increment("db.reads", 1)
db.query()    → meter.increment("db.queries", 1)
db.execute()  → meter.increment("db.queries", 1)
```

Row-based metering (for finer billing):
```
find() returns 50 rows → meter.increment("db.rows_read", 50)
update() affects 3 rows → meter.increment("db.rows_written", 3)
```

## Implementation Structure

```
Runtime changes:
  embed/db.js               JS polyfill (model, t, collection API → native callbacks)
  src/db_callbacks.rs       Native V8 callbacks (__dbQuery, __dbInsert, __dbFind, etc.)
  src/query_builder.rs      Translates filters/updates to SQL
  src/schema_manager.rs     Model definition → CREATE TABLE / ALTER TABLE
  src/validator.rs           Type + constraint validation before SQL

Control plane changes:
  On deploy: read model definitions from app code → diff with stored schema → migrate

Worker changes:
  Inject Postgres Pool into RuntimeState (per-thread, Rc-based)
```
