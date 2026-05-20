# @zeroship/db — Database SDK

`@zeroship/db` is the database SDK for zeroship apps. You declare a typed
schema once with `createDb({ ... })`, get a fully-typed `db` object back,
and call CRUD methods on its collections. Behind the scenes the SDK calls
into the native `env.db` v8_class surface (registered by the Rust runtime);
no raw SQL is exposed to user code.

```ts
import { createDb, t } from "@zeroship/db";

const db = createDb({
  users: {
    name:  t.string().required().max(100),
    email: t.string().required().unique(),
    role:  t.string().enum("user", "admin").default("user"),
  },
});

const { data: alice, error } = await db.users.insert({
  name: "Alice", email: "alice@example.com",
});
if (error) throw error;

const { data: admins } = await db.users
  .find({ role: "admin" })
  .sort({ name: 1 })
  .limit(10);
```

## Two return contracts

The same Collection class is used in two contexts. The contract differs:

| Context                       | Return shape                              | On failure              |
|-------------------------------|-------------------------------------------|-------------------------|
| `db.users.insert(...)` (top)  | `Promise<Result<T>>` — `{ data, error }`  | `error` is non-null     |
| Inside `db.transaction(tx)`   | `Promise<T>` — bare value (no envelope)   | throws                  |

```ts
// Top-level — explicit error handling
const { data, error } = await db.users.insert({ name: "Alice" });
if (error) return error;

// Inside a transaction — throws, no Result envelope
const result = await db.transaction(async (tx) => {
  const u = await tx.users.insert({ name: "Alice" });   // bare Row<S>
  await tx.todos.insert({ userId: u.id, title: "..." });
  return u;
});
// `result` itself is a Result<R>: `{ data: u, error: null }` or `{ data: null, error }`.
```

The `tx.<table>` wrapper is a JS-side throw-style adapter around the
outer Collection. Routing the CRUD call to the transaction connection
happens in Rust via a `TX_CONN` thread-local; the JS adapter only flips
the surface from Result to throw.

## Schema builders

Use the `t.*` factories. Every builder is chainable.

### Field types

| Builder                | TS type                       | Postgres        |
|------------------------|-------------------------------|-----------------|
| `t.string()`           | `string`                      | TEXT            |
| `t.number()`           | `number`                      | NUMERIC         |
| `t.boolean()`          | `boolean`                     | BOOLEAN         |
| `t.timestamp()`        | `number` (Unix ms)            | TIMESTAMPTZ     |
| `t.calendarDate()`     | `string` (`YYYY-MM-DD`)       | DATE            |
| `t.json()`             | `Record<string, unknown>`     | JSONB           |
| `t.array(t.string())`  | `string[]`                    | JSONB           |
| `t.ref("users")`       | `Id<"users">` (branded num)   | INTEGER + FK    |
| `t.object({ ... })`    | nested inferred object        | JSONB           |
| `t.literal("login")`   | `"login"`                     | underlying type |
| `t.union(v1, v2, ...)` | discriminated union           | flat columns    |

`t.date()` is **not** in the surface — use `t.timestamp()` for a TIMESTAMPTZ
(Unix-ms numbers at the JS layer) or `t.calendarDate()` for a Postgres DATE
(`YYYY-MM-DD` strings).

### Refinements

All chainable on any field:

| Method                | Effect                                                       |
|-----------------------|--------------------------------------------------------------|
| `.required()`         | Marks the key non-optional in `RowInput<S>` and `Row<S>`.    |
| `.unique()`           | Adds a `UNIQUE` index.                                       |
| `.index()`            | Adds a non-unique index.                                     |
| `.default(value\|fn)` | Default applied on insert when key is absent.                |
| `.min(n)`             | Number minimum / string minimum length.                      |
| `.max(n)`             | Number maximum / string maximum length.                      |
| `.enum(...values)`    | Restricts the value to a fixed set.                          |
| `.pattern(/regex/)`   | Regex constraint on string values.                           |

### Auto-generated columns

You never declare these; every collection has them:

- `id: number` — `BIGINT PRIMARY KEY`, auto-assigned on insert.
- `createdAt: number` — Unix-ms set on insert.
- `updatedAt: number` — Unix-ms set on every update.

When `softDelete: true` is enabled (see below), a fourth column is added:

- `deletedAt: number | null` — Unix-ms when soft-deleted; default null.

### Per-collection options via `schema()`

```ts
import { createDb, schema, t } from "@zeroship/db";

const db = createDb({
  todos: schema({
    title: t.string().required(),
    done:  t.boolean().default(false),
  }).softDelete().withVersioning(),
});
```

- `schema({...}).softDelete()` — `delete()` / `deleteMany()` set `deletedAt`
  instead of removing the row. Reads filter `deletedAt IS NULL` automatically.
  Pass `{ hard: true }` to bypass.
- `schema({...}).withVersioning()` — adds a `version` column. `update()`
  with `{ version: N }` in the filter is a CAS guard: on mismatch the call
  rejects with `OptimisticLockError`; on success `version` is incremented
  atomically.
- `schema({...}).strictness("strict" | "lenient" | "off")` — deploy-time
  data-validation policy.

### Named indexes

Declare named, multi-column indexes the way Convex does — they document
the queries you intend to run and the SDK warns you when a filter walks
the table without hitting one.

```ts
const db = createDb({
  todos: schema({
    userId: t.ref("users"),
    done:   t.boolean().default(false),
    email:  t.string(),
  })
    .index("by_email",       ["email"])
    .index("by_user_done",   ["userId", "done"]),
});
```

Each declaration becomes a CONCURRENTLY-built Postgres index named
`"<collection>__<name>"` (e.g. `"todos__by_user_done"`). Field order
matters — a multi-column index covers any **leftmost prefix** of its
fields, matching Postgres B-tree semantics:

| Filter                       | Matches `by_user_done`? |
|------------------------------|-------------------------|
| `{ userId, done }`           | yes (full match)        |
| `{ userId }`                 | yes (prefix)            |
| `{ done }`                   | no (skips `userId`)     |

`schema(...).uniqueIndex(name, fields)` is the same builder but emits a
`UNIQUE` index — useful for compound natural keys like `["orgId", "slug"]`.

**Validation at definition time.** `.index(name, fields)` throws with
`code = "schema_invalid"` if `name` is empty or already declared on the
schema, or if `fields` is empty or references a field absent from the
schema. The auto-generated columns (`id`, `createdAt`, `updatedAt`,
`deletedAt` under soft-delete, `version` under versioning) are accepted.

**Runtime warning.** Outside `NODE_ENV=production`, calling
`find()` / `get()` / `deleteMany()` with a filter whose keys don't form
a prefix of any declared index emits a one-time `console.warn` naming
the available indexes:

```
[@zeroship/db] unindexed query on "todos" — filter keys [done] match
no declared index. Declared indexes: by_email, by_user_done. Add
.index("by_X", ["done"]) to the schema, or filter by a prefix of an
existing index.
```

Per-field `.unique()` / `.index()` on `TypeBuilder` still works for
single-column cases — those desugar to one-column indexes and the
warning recognises them.

**Future work.** Changing the field list of a previously declared index
(e.g. `["userId"]` → `["userId", "done"]`) is currently a no-op: the
orchestrator's `CREATE INDEX … IF NOT EXISTS` keeps the old definition.
To swap an index in place today, drop it manually and redeploy.
Multi-column `uniqueIndex` constraints are emitted but the deploy-time
data-validation policy doesn't yet pre-check existing data for
duplicates on a non-empty table — adding a `uniqueIndex` against a
populated collection will succeed or fail at index-build time
depending on the data.

## Branded ids

`t.ref("users")` produces `Id<"users">` — a branded `number`. Two ref types
backed by different tables are mutually incompatible at compile time:

```ts
const userId: Id<"users"> = ...;
const todoId: Id<"todos"> = ...;
db.todos.get(userId);  // ✗ compile error
db.todos.get(todoId);  // ✓
```

Each collection exposes its own `Id` and `RowInput` type:

```ts
type UserId = typeof db.users.Id;          // = Id<"users">
type UserInsert = typeof db.users.RowInput; // = RowInput<usersSchema>

function addUser(input: UserInsert): Promise<UserId | undefined> { ... }
```

The accessors are type-only — at runtime they return `null`.

## Collection CRUD

Every method returns `Result<T>` outside a transaction. Inside `db.transaction`
the matching `tx.<table>` method returns `T` and throws.

### Create

```ts
// Single row
const { data: user } = await db.users.insert({ name: "Alice" });

// Many rows
const { data: users } = await db.users.insertMany([
  { name: "Bob" },
  { name: "Carol" },
]);

// Upsert — insert, or update on conflict
const { data: user } = await db.users.upsert(
  { email: "alice@example.com", name: "Alice" },
  { conflictFields: ["email"] },
);
```

### Read

```ts
// Single row by id (number is a shorthand for `{ id }`)
const { data: user } = await db.users.get(1);

// Single row by filter
const { data: user } = await db.users.get({ email: "alice@example.com" });

// Multiple rows — Query is thenable
const { data: admins } = await db.users
  .find({ role: "admin" })
  .sort({ name: 1 })          // 1 = ASC, -1 = DESC
  .limit(20)
  .skip(40);

// Cursor pagination — id-only seek (legacy helper)
const { data: next } = await db.users.find({}).sort({ id: 1 }).after(lastId);

// Cursor pagination — full envelope (recommended)
// Pass cursor: null for the first page; pass back continueCursor to advance.
// isDone flips true when the underlying store returns fewer than numItems+1
// rows. The cursor is opaque base64-JSON and bound to the orderBy used.
const { data: p1 } = await db.users
  .find({})
  .sort({ createdAt: -1 })
  .paginate({ cursor: null, numItems: 20 });
// p1 = { page, continueCursor, isDone }

if (!p1!.isDone) {
  const { data: p2 } = await db.users
    .find({})
    .sort({ createdAt: -1 })
    .paginate({ cursor: p1!.continueCursor, numItems: 20 });
}

// Projection
const { data: emails } = await db.users.find({}).select(["email"]);

// Count
const { data: n } = await db.users.count({ role: "admin" });

// Existence
const { data: exists } = await db.users.exists({ email: "alice@example.com" });

// Distinct
const { data: roles } = await db.users.distinct("role");
```

### Update

```ts
// By id, full row patch
const { data: u } = await db.users.update(1, { name: "Alice Smith" });
if (!u) throw new Error("not found");

// By filter (returns the first match, or null)
const { data: u } = await db.users.update({ email: "alice@..." }, { role: "admin" });

// Atomic operators — per-field
await db.products.update(1, {
  stock: { $dec: 1 },
  views: { $inc: 1 },
  tags:  { $push: "sale" },
});

// MongoDB top-level shape (SDK translates)
await db.products.update(1, { $inc: { views: 1 } });

// Update many — returns counts
const { data: counts } = await db.users.updateMany(
  { role: "user" },
  { role: "member" },
);
// counts = { matchedCount: N, modifiedCount: N }

// CAS via versioning (when withVersioning() is on)
const { data, error } = await db.products.update(
  { id: 1, version: 5 },
  { stock: { $dec: 1 } },
);
// error instanceof OptimisticLockError when stored version != 5
```

#### Retrying CAS updates with `withRetry`

The OCC pattern (read → compute → update with `{ version }` → retry on
`OptimisticLockError`) is wrapped by `withRetry`:

```ts
import { withRetry, isOptimisticLockError } from "@zeroship/db";

const updated = await withRetry(async () => {
  const { data: cur } = await db.products.get(id);
  if (!cur) throw new Error("not found");
  const { data, error } = await db.products.update(
    { id, version: cur.version },
    { stock: { $dec: 1 } },
  );
  if (error) throw error;
  return data;
});
```

Defaults: `max: 3`, retries only `OptimisticLockError`, no backoff. Pass
your own predicate to retry on additional coded errors:

```ts
await withRetry(fn, {
  max: 5,
  on: (e) => isOptimisticLockError(e) ||
             (e as { code?: string }).code === "serialization_failure",
  backoff: (attempt) => attempt * 10, // 10ms, 20ms, ...
});
```

`withRetry` does not swallow errors — the final attempt's throw bubbles
up to the caller.

### Delete

```ts
// By id — returns the deleted row (or null)
const { data } = await db.users.delete(1);

// By filter
const { data } = await db.users.delete({ email: "spam@..." });

// Many — returns counts
const { data } = await db.sessions.deleteMany({ expiresAt: { $lt: Date.now() } });
// { deletedCount: N }

// Soft delete: with `softDelete()` on the schema, delete sets deletedAt
// Pass { hard: true } to permanently remove the row.
await db.users.delete(1, { hard: true });
```

### Aggregate

```ts
const { data } = await db.orders.aggregate([
  { $match: { status: "paid" } },
  { $group: { by: "country", count: { $count: true }, total: { $sum: "amount" } } },
  { $having: { count: { $gt: 10 } } },
  { $sort:  { total: -1 } },
  { $limit: 20 },
]);
```

Supported accumulators: `$count`, `$sum`, `$avg`, `$min`, `$max`, `$first`.

## Filter operators

- Comparison: `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`
- Inclusion: `$in`, `$nin`
- String: `$like`, `$ilike`, `$search` (full-text)
- Null shape: `field: null`, `{ $ne: null }`, `{ $exists: true }`
- Logical: `$and`, `$or`, `$not` (each takes an array of sub-filters,
  except `$not` which takes one)

```ts
{ role: "admin", age: { $gte: 18 } }   // implicit AND
{ $or: [{ role: "admin" }, { role: "moderator" }] }
{ name: { $ilike: "%alice%" } }
```

## Update operators

Per-field (preferred): `$set`, `$inc`, `$dec`, `$mul`, `$push`, `$pull`,
`$addToSet`. A bare value is treated as `$set`. MongoDB top-level shape
(`{ $set: { ... } }`, `{ $inc: { ... } }`, etc.) is also accepted; the
SDK translates before dispatch.

## Transactions

```ts
const { data, error } = await db.transaction(async (tx) => {
  const u = await tx.users.insert({ name: "Alice" });
  await tx.todos.insert({ userId: u.id, title: "buy milk" });
  return u.id;
}, { isolationLevel: "serializable" });
```

- The callback receives a `tx` object that mirrors `db`. Methods on
  `tx.<table>` return the raw value (`Row<S>`, `number`, …) and throw on
  error — there is no Result envelope.
- The transaction is rolled back automatically when the callback throws
  or when the runtime drops the wrapper without commit/rollback.
- `isolationLevel` accepts `"readCommitted"` (default), `"repeatableRead"`,
  or `"serializable"`.
- Outside the callback the result is again a `Result<R>` — the surrounding
  `transaction()` call doesn't throw.

The procedure wrappers `query()`, `mutation()`, and `action()` from
`@zeroship/server` auto-open a tx around each request:
- `query()` runs the body in a `READ ONLY` tx.
- `mutation()` runs it in a `SERIALIZABLE` tx.
- `action()` runs it without a tx (actions are long-lived and may call
  `fetch()`); use `runMutation`/`runQuery` from inside an action to write.

Inside such a wrapper, all `db.<table>.*` calls are routed to the active
tx connection via the Rust `TX_CONN` thread-local — so the `Result`-shape
surface keeps working without changing the calling convention.

## Migrations (`@zeroship/migrations`)

Schema changes are immediate — `createDb` calls `registerModel` which
adds tables and columns idempotently on cold start. **Data backfills**
are the asynchronous part: a separate orchestrator iterates rows in
batches with resume, dry-run, cancel, and a dead-letter queue.

```ts
import { defineMigration, migrations } from "@zeroship/migrations";

export const backfillRole = defineMigration({
  name: "users.backfill_role",
  collection: "users",
  batchSize: 200,
  migrateOne: async (row) => {
    if (row.role == null) return { role: "user" };
    return undefined;                     // skip — already migrated
    // return null;                       // dead-letter this row
  },
});

// Run it
const { data } = await migrations.run(backfillRole);
// data.status: "applied" | "applied_with_dead_letter" | "failed" | "cancelled"
// data.processed, data.cursor, data.deadLetters

// Dry-run — every UPDATE rolls back; audit state is not advanced
await migrations.run(backfillRole, { dryRun: true });

// Reset from a cancelled/failed run
await migrations.run(backfillRole, { reset: true });

// Inspect / cancel by name
const { data: status } = await migrations.status(backfillRole);
if (status?.status === "running") await migrations.cancel(backfillRole);
```

`migrateOne` semantics:
- return an object → patch the row
- return `undefined` → skip, no change
- return `null` → dead-letter the row (audited, not patched)

Errors have a `.code` string property:

| Code                         | When                                                        |
|------------------------------|-------------------------------------------------------------|
| `migration_already_running`  | Another worker holds the advisory lock.                     |
| `migration_cancelled`        | The audit row was cancelled before this run completed.      |
| `migration_not_active`       | The Migration wrapper has been finalised/cancelled/reset.   |
| `migration_not_cancellable`  | Audit row is already in a terminal state.                   |
| `no_active_migration`        | Internal — `commitBatch`/`fetchBatch` outside of a run.     |

## Reactive subscriptions

```ts
import { env } from "zeroship";

const sub = env.db.openSubscription("messages");
for await (const ev of sub) {
  // ev.kind === "change" | "resync" | "closed"
  if (ev.kind === "change") console.log(ev.op, ev.pk, ev.columns);
}
```

Calling `openSubscription` is synchronous — it merely registers a slot
in the per-isolate broker. The wrapper's GC finalizer releases the slot
as a safety net; explicit `sub.close()` is preferred.

## Live queries (`db.live`)

`db.live(queryFn)` wraps the raw subscription stream into a
query-shaped reactive primitive: it runs the `queryFn` once, yields the
result, subscribes to the relevant table(s), and yields a fresh result
array whenever a watched table changes.

```ts
const live = db.live(() => db.todos.find({ userId: "x" }));

for await (const todos of live) {
  // Initial result, then a fresh array on every relevant change.
}

// Explicit teardown:
live.close();
```

The `queryFn` may return either a `Query` builder (chainable, thenable —
the `Result<T[]>` is unwrapped automatically) or a raw `Promise<T[]>`.

**Coarse-grained.** v1 subscribes per-table, not per-row: every
insert/update/delete on a watched table fires a rerun, even when the
row doesn't match the queryFn's filter. The auto-detected table set is
the union of all collections the `queryFn` touched during its first
execution. Read-set narrowing (Convex-style per-document tracking) is
future work.

**Explicit `tables` escape.** If `queryFn` returns a raw `Promise<T[]>`
that never goes through a `Collection` method (e.g. it transforms data
fetched elsewhere), table auto-detection finds nothing. Pass
`{ tables: [...] }` to wire the subscriptions manually:

```ts
db.live(async () => transform(await fetch(...)), { tables: ["todos"] });
```

**Inside a transaction.** Live queries outlive any single request, so
calling `db.live` inside the callback to `db.transaction(tx => ...)`
throws synchronously with `code = "live_in_transaction"`. Open the
live query before (or after) the tx.

**Cleanup.** `close()` is idempotent and cancels every underlying
subscription. The iterator's `return()` (invoked by `for await ...
break` or an early `throw`) also calls `close()` automatically.

## Errors

Errors carry a `.code` property where applicable:

| `error.code`                | When                                                |
|-----------------------------|-----------------------------------------------------|
| `ValidationError`           | Input fails schema validation.                      |
| `11000`                     | Duplicate unique-key violation.                     |
| `OptimisticLockError`       | `update` with a CAS version that didn't match.     |
| `migration_*` (see above)   | Migration lifecycle errors.                         |

Use the property directly — never substring-match on `error.message`.

```ts
const { data, error } = await db.users.update({ id, version: 5 }, { name });
if (error?.name === "OptimisticLockError") {
  // refetch and retry
}
```

## Native surface (advanced)

The SDK calls into a small native surface registered as `env.db` by the
Rust DbPlugin. App code rarely needs it; SDK packages and special
ops use it directly.

- `env.db.collection(name)` → `Collection` wrapper
- `env.db.beginTransaction({ isolationLevel? })` → `Transaction` wrapper
- `env.db.openSubscription(name)` → `Subscription` wrapper
- `env.db.migrations.{start,status,cancel,reset}` → Migration ops
- `env.db.registerModel(name, schema)` → idempotent DDL

Type contracts live in `sdks/types/db.d.ts`. The runtime implementation
lives in `crates/plugin-db/`.

## Per-app isolation

Every app gets its own Postgres schema (UUID-based):

```sql
SELECT * FROM "<app-uuid>"."users" WHERE ...
```

The `app_id` is injected by the runtime from `env_vars`; user code can
neither read nor override it. `env.db` is frozen.
