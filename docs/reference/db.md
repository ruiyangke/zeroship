# @zeroship/db — Database SDK

`@zeroship/db` is the database SDK for zeroship apps. The schema source of
truth is the committed op.* migration set. `@zeroship/vite-plugin` records
`migrations/*.ts` in-process through its pure-JS recorder, folds the resulting
IR envelopes through `zeroship-migrate-node`, and emits
`generated/zeroship/env.db.ts`, which installs the TypeScript `Env.db`
augmentation, plus `schema.runtime.json`, which the runtime consumes at boot.
Handlers call CRUD methods on `env.db.<name>` directly.
Behind the scenes the SDK calls into the native `env.db` v8_class surface
(registered by the Rust runtime); no raw SQL is exposed to user code.

```ts
// migrations/20260628000000_initial_schema.ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "initial_schema",
  schema() {
    table("users").create({
      columns: {
        name: t.text().notNull(),
        email: t.text().notNull().unique(),
        role: t.text().default("user"),
      },
    });
  },
};
```

```ts
// src/index.ts — your app's entry module
import { env } from "zeroship";
import { mutation, query } from "@zeroship/rpc/server";

// Typed by generated/zeroship/env.db.ts.
const db = env.db;

export const addUser = mutation(async ({ name, email }: { name: string; email: string }) => {
  const { data: alice, error } = await db.users.insert({ name, email });
  if (error) throw error;
  return alice;
});

export const listAdmins = query(async () => {
  const { data, error } = await db.users
    .find({ role: "admin" })
    .sort({ name: 1 })
    .limit(10);
  if (error) throw error;
  return data ?? [];
});
```

## Migrate before you deploy

`zeroship deploy` ships code; it does not touch the database. `zeroship migrate`
applies the migration set. **For an app with migrations, the migrate has to come
first**, and the control plane enforces it: a deploy whose generated
`schema.runtime.json` is not the one the app's newest applied migration produced
is refused with `409 schema_not_applied`, and nothing goes live.

```
$ zeroship deploy ./dist/app.zship --app=<id>

This app has committed migrations. Deploy does NOT apply them:
  zeroship migrate --app=<id> --control=<url>
Until you do, this deploy is REFUSED with 409 schema_not_applied.
```

The response body names the fix:

```json
{
  "error": "schema_not_applied",
  "deploy_descriptor_sha256": "a588c564…",
  "applied_descriptor_sha256": null,
  "remedy": "zeroship migrate --app=<id>"
}
```

There is no override. The check exists because the generated descriptor is the
only thing the runtime consults about your schema - including which columns are
masked. If it could go live ahead of the DDL it describes, a column the
descriptor calls masked would be served as the plain value it still holds, and
nothing downstream would notice.

Three consequences worth knowing before you meet them:

- **A brand-new database app takes two commands.** `zeroship migrate` will not
  create an app that does not exist and `zeroship deploy` is what creates it, so
  the first run is deploy (409) -> migrate -> deploy. Every run after that is
  migrate -> deploy.
- **An artifact built WITHOUT its migrations is refused too**, with
  `409 schema_descriptor_missing`, once the app has any applied schema. Shipping
  it would boot the app with `env.db` uninstalled over a live database.
- **You cannot deploy an older build across a migration boundary.** The
  comparison is against the NEWEST applied migration, not "any migration ever
  applied". Schema migrations carry an engine-synthesized structural inverse,
  while data migrations carry either a recorded `inverse()` or an explicit
  `irreversible` reason; deployment still requires the descriptor for the newest
  applied state.

### TypeScript: typed `env.db`

To make `env.db.<name>` strongly typed against the folded migration set,
commit the generated artifacts and include the generated module in your
project's `tsconfig.json`:

```json
{
  "compilerOptions": {
    "types": ["@zeroship/types"]
  },
  "include": ["src", "generated/zeroship/env.db.ts"]
}
```

`@zeroship/types` declares the base `zeroship` runtime module. The
generated `env.db.ts` imports `@zeroship/db`'s `t`/`Db` types, reconstructs
the folded schema, and declares the single `Env.db` augmentation for the
app. This generated module is the only source of application database typing.

The root `@zeroship/db` package is the plain TypeScript SDK surface (`t`,
`schema`, `RowOf`, `Db`, etc.) for shared packages and tests. The generated
module is the sole app-level `Env.db` augmentation.

### Collection names

Collection names created by migrations become physical table names. Keep them
ASCII alphanumeric plus underscores and within the backend identifier limit.
Creator-authored migrations reserve backend catalog prefixes and `__zeroship`
to avoid DDL collisions. The runtime ORM may address a prefixed table already
declared by its trusted descriptor; the prefix does not change runtime access.

**Type generation refuses invalid names too, so failures surface at build time.**
Column names follow the portable identifier and reserved-prefix rules. Masked
storage names come from the generated runtime descriptor. A masked field name
must also leave room for its hidden raw storage name. Invalid declarations
leave `generated/zeroship/` unchanged.

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
happens in Rust via the per-isolate `ThreadDbContext::tx_conn` slot
(formerly a `TX_CONN` thread-local, folded into `ThreadDbContext` in
Stage 8d-R4); the JS adapter only flips the surface from Result to
throw.

## Schema builders

Use the `t.*` factories. Every builder is chainable.

### Field types

| Builder                | TS type                       | Postgres        |
|------------------------|-------------------------------|-----------------|
| `t.string()`           | `string`                      | TEXT            |
| `t.number()`           | `number`                      | DOUBLE PRECISION |
| `t.numeric({ precision, scale })` | `Decimal` string      | NUMERIC         |
| `t.bigInt()`           | `number` or `bigint`           | BIGINT          |
| `t.boolean()`          | `boolean`                     | BOOLEAN         |
| `t.timestamp()`        | `number` (Unix ms)            | TIMESTAMPTZ     |
| `t.calendarDate()`     | `string` (`YYYY-MM-DD`)       | DATE            |
| `t.json()`             | `Record<string, unknown>`     | JSONB           |
| `t.array(t.string())`  | `string[]`                    | JSONB           |
| `t.ref("users")`       | `Id<"users">` (branded string) | TEXT + FK      |
| `t.object({ ... })`    | nested inferred object        | JSONB           |
| `t.literal("login")`   | `"login"`                     | underlying type |
| `t.union(v1, v2, ...)` | discriminated union           | flat columns    |
| `t.bytes()`            | `Uint8Array`                 | BYTEA           |

`t.date()` is **not** in the surface — use `t.timestamp()` for a TIMESTAMPTZ
(Unix-ms numbers at the JS layer) or `t.calendarDate()` for a Postgres DATE
(`YYYY-MM-DD` strings).

Scalar timestamps accept integral Unix milliseconds, valid `Date` objects, or
ISO timestamp strings and return Unix milliseconds. Strings must name a real
calendar date; an omitted time means midnight and an omitted timezone means
UTC. Fractional seconds are floored to the containing millisecond. The resulting
UTC date must fit the positive `YYYY-MM-DD` calendar, including early years.
Invalid dates, fractional millisecond numbers, and out-of-range instants are
rejected before writes.

Calendar dates use positive Gregorian years in the fixed-width `YYYY-MM-DD`
form. Reads preserve that string, including early years; invalid dates and
datetime strings are rejected. Calendar dates carry no timezone.

Wide integers accept safe integer numbers or bigint values within the database
integer range. Reads return numbers within the safe integer range and bigint
values beyond it. Convert bigint explicitly when returning a JSON response.

Binary fields accept and return `Uint8Array`. The V8 adapter captures the bytes
into an owned native value before asynchronous execution. Both database backends
bind those bytes directly; Rust model fields use `Vec<u8>`.

```ts
const png = new Uint8Array(await file.arrayBuffer());
await env.db.uploads.insert({ blob: png });

const rows = await env.db.uploads.find({ id }, { limit: 1 });
const back: Uint8Array = rows[0].blob;
```

Text and ordinary arrays are refused for binary columns.

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

### Generated columns

Migration policy can supply columns with assignment generators. The resulting
`schema.runtime.json` declares those fields, their types, primary keys and
lifecycle roles. Rust and TypeScript use that same descriptor; `Row<S>` contains
only fields declared by `S`.

Every ORM collection must declare a required `id` field as its sole primary key.
The descriptor must include it explicitly; collection setup never injects it.
Generation remains optional and uses the field's assignment metadata. Composite
business keys use unique constraints. Once inserted, `id` cannot change through
updates, upsert conflicts, or lifecycle generators. ID generators run on insertion.
Projections and aggregate results may omit `id`; protected reads retain it internally
for decryption and unmasking.

An assignment names a generator (`typedId`, `actor`, `now`, `increment(N)` or
`identity`) and an event (`insert`, `write` or `delete`). The ORM supplies typed
IDs and request actors. Database defaults initialize timestamps, counters and
identity columns; write expressions update timestamps and counters. Anonymous
writes assign a null actor. Assigned fields are excluded from typed write inputs.
Encrypted inserts reserve a database-generated identity before encryption, within
the write's transaction.

Names such as `created_at`, `version` and `deleted_at` have no intrinsic behavior.
Without assignment metadata, they are ordinary columns. See [Column assignments](#column-assignments).

### Typed-id prefixes

A field assigned by `typedId` receives a UUIDv7 encoded with a readable prefix.
The ORM uses the field's `idPrefix` when declared, otherwise derives one from the
collection name. The scalar `id` type alone does not request generation.

### Per-collection options

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_todos",
  schema() {
    table("todos").create({
      columns: {
        title: t.text().notNull(),
        done: t.boolean().default(false),
      },
      options: { strictness: "strict" },
      indexes: [{ name: "by_done", on: ["done"] }],
    });
  },
};
```

- `softDelete: true` selects the declared `now` generator on `delete` as the
  visibility marker. Deletes update the marker; reads hide marked rows.
- `versioning: true` selects the declared write increment generator for
  optimistic concurrency. A filter containing that field performs a revision check.

- `strictness: "strict" | "lenient" | "off"` records the deploy-time data-validation
  policy. The descriptor preserves it; deployment enforcement is not wired yet.

The migration renderer rejects enabled lifecycle options without an unambiguous
matching generator. These options select roles; generators determine assignments.

### Named indexes

Declare named, multi-column indexes the way Convex does — they document
the queries you intend to run and the SDK warns you when a filter walks
the table without hitting one.

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_todos_indexes",
  schema() {
    table("todos").create({
      columns: {
        userId: t.text().references("users", "id"),
        done: t.boolean().default(false),
        email: t.text(),
      },
      indexes: [
        { name: "by_email", on: ["email"] },
        { name: "by_user_done", on: ["userId", "done"] },
      ],
    });
  },
};
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
`code = "SCHEMA_INVALID"` if `name` is empty or already declared on the
schema, or if `fields` is empty or references a field absent from the
schema, including any explicitly declared generated fields.

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

`t.ref("users")` produces `Id<"users">` — a branded `string`. Two ref types
backed by different tables are mutually incompatible at compile time:

```ts
const userId: Id<"users"> = ...;
const todoId: Id<"todos"> = ...;
const wrong: Id<"todos"> = userId; // compile error
const correct: Id<"todos"> = todoId;
```

Each collection exposes its own `Id` and `RowInput` type:

```ts
type UserId = typeof db.users.Id;          // = Id<"users">
type UserInsert = typeof db.users.RowInput; // = RowInput<usersSchema>

function addUser(input: UserInsert): Promise<UserId | undefined> { ... }
```

The accessors are type-only; do not read them at runtime. `typeof collection.Id`
and `InferId<typeof collection>` follow the schema's underlying ID type. Numeric
IDs work with `get`, mutation shorthand, relation loading, and `bulkUnmask`.
The map returned by `bulkUnmask` uses the caller's ID values as keys.

For a manual schema, declare the foreign-key target column explicitly:
`t.ref("users", { column: "account_key" })`. Migration-generated builders carry
the target column from `schema.runtime.json`. Relation loading without an
explicit column uses the target collection's `id`.

By default, the builder emits a same-app FK without `ON DELETE`, `ON UPDATE`,
or `DEFERRABLE` clauses,
so the database's own defaults apply: `NO ACTION` for both actions, and
immediate (non-deferred) checking. On Postgres `NO ACTION` rejects a delete
that would orphan a row, the same as `RESTRICT`, but defers the check to the
end of the statement rather than firing per row.

Add `onDelete: "cascade"` to the reference options for physical cascade,
or `{ deferrable: true }` if you need cyclic refs insertable within one
transaction — that is opt-in, not the default. Cross-app targets are refused;
FKs stay inside the calling app. See `sdks/db/src/types.ts` for the builder,
and the next section for what does the refusing, which is not what this page
said until 2026-08-20.

### Can an FK point at another app's table?

No, and it is worth being exact about which code makes that true, because two
plausible-looking answers are wrong.

**It was not `crates/zeroship-data-v8/src/cross_app_fk.rs (DELETED)`, and that file no
longer exists.** It held a `reject_cross_app_fk` validator scanning `refTarget`
for an `<other_app>.` prefix and returning `cross_app_fk_forbidden`, and it is
the mechanism this page used to cite. It had **no production call site** -
only integration tests, calling it directly to pin a refusal nothing reached -
and it was deleted on 2026-09-02 rather than wired, because the data plane no
longer emits DDL and so has nowhere to wire it to. Nor is it
`crates/zeroship-data-orm/src/sql`; migration validation belongs to the
migration engine.

**Schema is applied by the migration engine at deploy**, and that is where the
answer lives. Four things hold there, in order:

1. A dot-qualified target on a COLUMN-level ref (`t.ref("other.users")`, or
   `.references("other.users", ...)`) is refused at author time by
   `reject_cross_app_ref`
   (`crates/zeroship-migrate-core/src/render/declarative.rs:4606`,
   reached from the op-DSL lower path at `declarative.rs:3491`), which raises
   `CrossAppFkForbidden`.
2. A TABLE-level foreign-key constraint gets no such prefix check. It does not
   escape anyway: the renderer qualifies every `REFERENCES` with the schema it
   was called FOR (`fk_definition_for_dialect`, `declarative.rs:4502`), so
   `"other.users"` renders as `"<app>"."other.users"` -- a table name inside
   the app's own schema. The differ then rejects it as
   `CrossAppFkTargetMissing` because no such table is declared or live
   (`declarative.rs:6080`). So this case is caught, but as a missing target
   rather than as a boundary violation.
3. An op-level `schema:` qualifier naming another schema is refused
   fail-closed under `SchemaScope::Single`
   (`crates/zeroship-migrate-core/src/model/validate.rs`,
   `CODE_CROSS_SCHEMA`), and the rendered SQL is swept again by the guard's
   `check_cross_schema`
   (`crates/zeroship-migrate-postgres/src/guard/sql.rs:1801`).
4. Underneath all of it, `crates/zeroship-migrate-server` derives the target schema from the
   app id server-side (`src/apply.rs:413`) -- no author input reaches it -- and
   applies under a `NOLOGIN`/`NOSUPERUSER` per-app role whose `search_path` and
   grants reach that schema only (`src/provisioning.rs:104-208`).

The `ForeignKeyReference.schema` field in `packages/zero-migrate/src/types.ts` is an
authoring hint only. It is never serialised into the IR, and when the enclosing
op carries an explicit schema a mismatch throws `OP_INVALID`; when it does not,
the hint is accepted and discarded.

What none of this covers: two apps that the control plane assigns the same
`app_id`. Isolation there is a control-plane property, not a rendering one.
The differ's own comment at `declarative.rs:6074` makes the matching point
about inbound-FK consent.

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

Upsert matches a unique key owned by the application. Every `conflictFields`
entry must name a declared field supplied in the document, without duplicates.
Platform-assigned fields, including `id`, cannot be conflict keys. A new row gets
a generated identity; a conflict preserves the existing identity. Use
`db.users.update(userId, patch)` to change a row identified by its ID.

### Read

```ts
// Single row by id (string is a shorthand for `{ id }`)
const { data: user } = await db.users.get("usr_01hxyz...");

// Single row by filter
const { data: user } = await db.users.get({ email: "alice@example.com" });

// Multiple rows — Query is thenable
const { data: admins } = await db.users
  .find({ role: "admin" })
  .sort({ name: 1 })          // 1 = ASC, -1 = DESC
  .limit(20)
  .skip(40);

// Cursor pagination — id-only seek helper
const { data: next } = await db.users.find({}).sort({ id: 1 }).after(lastId);

// Cursor pagination — full envelope (recommended)
// Pass cursor: null for the first page; pass back continueCursor to advance.
// isDone flips true when the underlying store returns fewer than numItems+1
// rows. The cursor is opaque base64-JSON and bound to the orderBy used.
// Prefer id order for stable feeds; typed ids are lexicographically sortable.
const { data: p1 } = await db.users
  .find({})
  .sort({ id: -1 })
  .paginate({ cursor: null, numItems: 20 });
// p1 = { page, continueCursor, isDone }

if (!p1!.isDone) {
  const { data: p2 } = await db.users
    .find({})
    .sort({ id: -1 })
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

Sorting is portable for text, ID/reference, integer, floating-point, and
temporal fields. Grouping and `distinct` also accept booleans and bytes.
Decimal and JSON storage do not have the same native equality or ordering on
PostgreSQL and SQLite, so the ORM refuses to sort, group, or deduplicate those
fields. JSON filters still use structural equality on both backends.

### Relations — `with: { fk: true }`

`find()` / `get()` accept an optional `with: { <fkField>: true }` option
that eager-loads referenced rows in a single batched roundtrip per
relation. No more N+1.

```ts
// Before — N+1: one users.get(id) per todo
const { data: todos } = await db.todos.find({});
for (const t of todos!) {
  const { data: u } = await db.users.get(t.userId);
  // ...
}

// After — exactly two roundtrips total, regardless of the page size
const { data: todos } = await db.todos.find({}, { with: { userId: true } });
// each todo: { id, projectId, title, userId: { id, email, name } | null }
```

The same option is available on `get(id)` and as a chainable `.with(...)`
method on the lazy `Query` returned by `find()`:

```ts
const { data: todo }  = await db.todos.get(id, { with: { userId: true } });
const { data: rows }  = await db.todos.find({}).with({ userId: true });
const { data: page }  = await db.todos
  .find({})
  .sort({ id: 1 })
  .with({ userId: true })
  .paginate({ cursor: null, numItems: 20 });
```

Multiple relations are allowed in one call — each fires its own single
batched fetch:

```ts
const { data } = await db.todos.find({}, {
  with: { userId: true, projectId: true },
});
// data[0].userId    → { id, email, name } | null
// data[0].projectId → { id, name }        | null
```

#### Rules

- The `with` key MUST be a `t.ref(...)` field declared on the parent
  schema. `with: { id: true }`, `with: { title: true }`, or any
  unknown key rejects with `"... is not a t.ref field on \"<table>\""`.
- The joined row REPLACES the FK string id at the same key. To keep
  both the FK and the joined row, declare the relation on a separate
  field name (e.g. `user: t.ref("users")` instead of `userId`) — the
  joined row lands at `user`, and its `.id` is the original FK.
- `null` FK values stay null after the join. FK values that point to a
  deleted / missing target row also resolve to null.
- FK ids are deduplicated before the wire call: `N` todos with the same
  `userId` produce one `WHERE id IN (?)` with one entry.

#### v1 limitations (future work)

- **Standalone models degrade.** When `env.db` is schema-typed through
  `generated/zeroship/env.db.ts`, joined fields narrow to the referenced row type.
  Standalone `model()` callers that do not carry a parent schema map
  still degrade joined fields to `PlainObject | null`.
- **No projection narrowing.** Drizzle / Prisma support
  `with: { user: { columns: ["email"] } }` to project the joined row.
  v1 always fetches every column of the target.
- **No relation-level filters.** Drizzle's
  `with: { posts: { where: ... } }` narrows the join. v1 has no
  equivalent — use a top-level filter on the parent instead.
- **Single-level only.** Nested relations
  (`with: { userId: { with: { teamId: true } } }`) are not supported in
  v1. Compose two finds in JS, or wait for v2.

### Update

```ts
// By id, full row patch
const { data: u } = await db.users.update("usr_01hxyz...", { name: "Alice Smith" });
if (!u) throw new Error("not found");

// By filter (returns the lowest-id match, or null)
const { data: u } = await db.users.update({ email: "alice@..." }, { role: "admin" });

// Atomic operators — per-field
await db.products.update("prd_01hxyz...", {
  stock: { $dec: 1 },
  views: { $inc: 1 },
  tags:  { $push: "sale" },
});

// MongoDB top-level shape (SDK translates)
await db.products.update("prd_01hxyz...", { $inc: { views: 1 } });

// Update many — returns counts
const { data: counts } = await db.users.updateMany(
  { role: "user" },
  { role: "member" },
);
// counts = { matchedCount: N, modifiedCount: N }

// CAS via the platform version field
const { data, error } = await db.products.update(
  { id: "prd_01hxyz...", version: 5 },
  { stock: { $dec: 1 } },
);
// error instanceof OptimisticLockError when stored version != 5
```

Filtered single-row updates, deletes, restores, and purges choose the matching
row with the lowest `id`. This keeps the result stable across database plans.

Bulk update, delete, restore, and purge operations affect all matching rows and
report database affected-row counts without fetching the changed records.
They remain atomic; large maintenance jobs should choose explicit bounded
batches. Updates that encrypt values separately for each target row retain the
ORM's encrypted-target cap and reject an oversized target set before writing.

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
  on: (e) =>
    isOptimisticLockError(e) ||
    (typeof e === "object" &&
      e !== null &&
      "code" in e &&
      e.code === "SERIALIZATION_FAILURE"),
  backoff: (attempt) => attempt * 10, // 10ms, 20ms, ...
});
```

`withRetry` does not swallow errors — the final attempt's throw bubbles
up to the caller.

### Delete

```ts
// By id — returns the deleted row (or null)
const { data } = await db.users.delete("usr_01hxyz...");

// By filter
const { data } = await db.users.delete({ email: "spam@..." });

// Many — returns counts
const { data } = await db.sessions.deleteMany({ expiresAt: { $lt: Date.now() } });
// { deletedCount: N }

// Soft delete: delete sets deleted_at instead of removing the row.
// For an explicit hard-delete, use `purge` / `purgeMany`:
await db.users.purge("usr_01hxyz...");
await db.users.purgeMany({ email: { $like: "spam-%" } });
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

Supported accumulators: `$count`, `$sum`, `$avg`, `$min`, `$max`.

## Explicit joins

Alias collections and construct a structured query with `env.db.from`:

```ts
import { eq } from "@zeroship/db";

const o = env.db.orders.as("o");
const c = env.db.customers.as("c");
const { data, error } = await env.db.from(o)
  .leftJoin(c, eq(o.columns.customerId, c.columns.id))
  .where(eq(o.columns.status, "paid"))
  .select({ order: o.row(), customer: c.optionalRow() })
  .orderBy(o.columns.id.asc())
  .limit(pageSize)
  .all();
```

Each result contains an order and either a customer or `null`. A matched row
whose selected fields are null still produces an object. Use `innerJoin` when
only matching combinations should be returned. Joins can be chained and can
refer to the same collection through distinct aliases. Every source must use
the same database handle, and each join requires a connecting column equality.

The query preserves matching row combinations, including repeated parents.
Sorting and pagination apply to those combinations. `with` remains the separate
relation-loading API. Column names and types come from the generated descriptor;
the native adapter accepts structured expressions and bound values.

Named scalar projections can select columns or `count`, `sum`, `avg`, `min`,
and `max` expressions. Use `groupBy` for grouping keys and `having` for aggregate
predicates. `count(column, true)` counts distinct values; `count()` counts rows.
`sum` and `avg` accept integer and number columns. `min` and `max` accept ordered
scalar columns except decimal, bytes, and boolean. Protected columns cannot be
join keys or aggregate operands.

Inside a transaction use `tx.from(...)`; `.all()` returns the rows directly and
throws on failure. The query retains the transaction scope and cannot execute
after the callback has settled. Live queries record every participating
collection, including those with no matching rows.

## Vector / Geo

Two search modalities ride on top of the schema DSL. Each has the
same shape: declare the column with a `t.*` builder;
deploy registers the appropriate index; query via `Collection.search`
or `Collection.near`. Cross-backend membership is identical, but PG and
Rust floating-point distance values can differ in the low significand bits.
Assert set membership rather than strict ordinal positions.

### Backend extension dependency

| Capability | PG dependency               | SQLite dependency         |
|------------|-----------------------------|---------------------------|
| Vector     | `pgvector` extension        | none — bundled            |
| Geo (point + radius) | `postgis` extension | none — bundled            |
| Polygon ops          | `postgis` extension | **not supported** (PG-only) |

The fastest path on PG is to swap the database image to
`pgvector/pgvector:pg16`, which ships both `pgvector` AND `postgis`
out of the box — see [`docs/runbooks/docker-compose.md`](../runbooks/docker-compose.md)
for the operator-action runbook.

On SQLite (dev/sandbox/test only), vector search routes through the
`sqlite-vec` extension (statically compiled via the `sqlite-vec`
Rust crate — no `.so` shipping, no amalgamation fork; the bundled
SQLite invariant is preserved). Geo search uses a pure-Rust haversine
flat scan — see "Backend coverage" below for the dev-scale ceiling.

### Vector search

Declare a column with `t.vector({ dimensions, metric? })`:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_docs",
  schema() {
    table("docs").create({
      columns: {
        title: t.text().notNull(),
        embedding: t.vector({ dimensions: 1536, metric: "cosine" }),
      },
    });
  },
};
```

- `dimensions` is **required** and must lie in `1..=16000` (pgvector's
  hard ceiling). The SDK validates the literal at schema-parse time
  and again at insert (`code: "VECTOR_DIMENSION_MISMATCH"` when
  `vector.length !== dims`).
- `opts.metric` selects the distance function: `"cosine"` (default),
  `"l2"` (Euclidean), or `"innerProduct"` (negated dot product;
  higher dot product → smaller "distance"). The metric is fixed at
  index creation — changing the metric requires `DROP INDEX` + redeploy.

Query with `Collection.search({ vector, k, ... })`:

```ts
const emb: number[] = await embed("rust async runtimes"); // your model
const { data, error } = await db.docs.search({
  vector: emb,
  k: 10,                       // 1..=1000 (default 10)
  metric: "cosine",            // optional, must match the index
  column: "embedding",         // optional when only one vector column declared
  filter: { language: "en" },  // optional WHERE clause — composes with the ANN scan
});
// data: (Row<S> & { _distance: number })[]
```

`_distance` is a synthetic column the row carries back from the scan.
On PG this is `col <-> $query` (pgvector's distance operator
specialised to the metric). SQLite uses sqlite-vec's scalar distance
functions on the stored BLOB column. Filtering happens before ranking and
limiting the result, so matching rows fill the requested result window.

### Geo (point + radius)

Declare a geo column with `t.geoPoint()`:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_stores",
  schema() {
    table("stores").create({
      columns: {
        name: t.text().notNull(),
        loc: t.geoPoint(), // {lat, lng}
      },
    });
  },
};
```

On PG this maps to `geography(POINT, 4326)`. On SQLite this maps to a
packed `BLOB` (16 bytes — two little-endian `f64`s, `(lat, lng)`)
with a `CHECK (length("loc") = 16)` constraint. Both backends store
WGS84 (EPSG:4326) coordinates; the `(lat, lng)` ordering is the SDK
contract regardless of backend.

Query with `Collection.near`:

```ts
const { data } = await db.stores.near({
  field:  "loc",
  point:  { lat: 51.5074, lng: -0.1278 },   // London
  radius: 1000,                              // metres
  filter: { open: true },                    // optional WHERE clause
  limit:  20,                                // 1..=1000 (default 100)
});
// data: (Row<S> & { _distance_m: number })[]
```

`_distance_m` is the great-circle distance in metres. On PG this is
`ST_Distance(loc, ST_MakePoint($lng, $lat)::geography)` (note:
PostGIS takes lng-lat, but the SDK swaps for you — `point: {lat, lng}`
is always the call site contract). On SQLite this is the Rust
haversine computed during the full-scan post-filter.

**Polygon ops are PG-only.** Passing a polygon to a SQLite backend
rejects with `code: "POLYGON_OPS_PG_ONLY"`. Use PG for any
production-scale geo workload.

### Search inside a transaction

`search()` and `near()` issued inside `db.transaction(fn)` run on that
transaction's own connection, so they see rows the same transaction has
already written and nobody else's uncommitted work:

```ts
await db.transaction(async (tx) => {
  await tx.docs.insert({ embedding, title: "fresh" });
  const { data } = await tx.docs.search({ vector: embedding, k: 5 });
  // `fresh` is in `data` - it was written on this connection
});
```

Roll the transaction back and the row is gone, exactly as an ordinary
`find` would report. This is the same read-your-own-writes rule the rest
of `env.db` follows; there is no separate snapshot for the search family.

### Where the search index comes from

**Your migration builds it, not the first query.** Declaring
`t.vector({ dimensions, metric })` or `t.geoPoint()` makes the index part of
your schema, and it is created when you apply migrations — the same
moment your table is. Nothing is built lazily at runtime, so the first
`.search()` after a deploy is as fast as the thousandth, and a query
never blocks behind an index build.

The consequence to know about: **if you add a `.vector()` or
`.geoPoint()` field and do not apply a migration, the column exists
with no index behind it.** Searches still return correct results — they
fall back to a sequential scan — but they get slower as the collection
grows. `zeroship migrate` is what turns that back into an indexed
search. There is no runtime path that notices and repairs it.

### Backend coverage

- **PG vector** — `pgvector` `ivfflat` index over the declared metric's
  operator class (`vector_cosine_ops` / `vector_l2_ops` /
  `vector_ip_ops`). Production-grade; scales to millions of rows.
- **SQLite vector** — exact search over the migrated BLOB column using
  statically linked sqlite-vec scalar distance functions. No virtual table,
  trigger, or runtime DDL is required. Cosine and L2 are supported;
  inner product returns `VECTOR_UNSUPPORTED_METRIC`. This path suits local
  development; production vector workloads use PostgreSQL indexes.
- **PG geo** — PostGIS `geography(POINT, 4326)` + GiST index; spheroid
  distance via `ST_DWithin` / `ST_Distance`.
- **SQLite geo** — packed `(lat, lng)` BLOB + full-scan haversine
  (~30 LOC of trig). Dev-tier ceiling; for production geo workloads
  use PG.

### Error codes

`Collection.search` and `Collection.near` add the following codes on
top of the global error rail (§ Errors):

| `error.code`                       | When                                                                 |
|------------------------------------|----------------------------------------------------------------------|
| `INVALID_K`                        | `k` outside `1..=1000`. Client-side validation; the native side never sees the call. |
| `VECTOR_EXTENSION_MISSING`         | PG without `pgvector`. Hint mentions `CREATE EXTENSION vector;` and the `pgvector/pgvector:pg16` image swap. |
| `POSTGIS_EXTENSION_MISSING`        | PG without `postgis`. Hint mentions `CREATE EXTENSION postgis;` and the same image swap. |
| `VECTOR_DIMENSION_MISMATCH`        | `args.vector.length !== <declared dims>` at insert or query time.    |
| `POLYGON_OPS_PG_ONLY`              | A polygon was passed to `.near` on a SQLite backend.                 |

All five carry a stable `.code` — branch on the code, never substring-match
on `error.message`.

## Filter operators

- Equality: `$eq`, `$ne`, `$in`, `$nin` on ordinary scalar and JSON fields
- Ordering: `$gt`, `$gte`, `$lt`, `$lte` on text, integer, floating-point,
  temporal, ID, and reference fields
- Pattern: `$like`, `$ilike` on text-backed fields
- Null shape: `field: null`, `{ $ne: null }`, `{ $exists: true }`
- Logical: `$and`, `$or`, `$not` (each takes an array of sub-filters,
  except `$not` which takes one)

JSON equality is structural on both backends: object key order and equivalent
number spellings do not affect the result. Use an explicit operator such as
`{ payload: { $eq: value } }` for JSON so object keys beginning with `$` cannot
be mistaken for filter operators. Vector and geographic fields use `search`
and `near`; ordinary comparisons on them are refused.

```ts
{ role: "admin", age: { $gte: 18 } }   // implicit AND
{ $or: [{ role: "admin" }, { role: "moderator" }] }
{ name: { $ilike: "%alice%" } }
```

## Update operators

Per-field (preferred): `$set`, `$inc`, `$dec`, `$mul`, `$push`, `$pull`,
`$addToSet`. A bare value is treated as `$set`. MongoDB top-level shape
(`{ $set: { ... } }`, `{ $inc: { ... } }`, etc.) is also accepted by the
shared ORM. The SDK maps field names to columns while preserving this grammar.

Each field may be assigned only once per update. Multiple operators on a field,
mixed operator/data objects, and assignments that collide after column-name
mapping are refused. Arithmetic operators require native numbers or Rust
`Decimal` values; strings, booleans, and null are refused before SQL execution.
Objects supplied through an explicit `$set` are literal data, even if their
keys look like update operators.

Array operators treat the operand as a complete element. `$push` appends it,
`$pull` removes every structurally equal element, and `$addToSet` appends it
only if no equal element exists. Object key order does not affect equality;
array order and JSON types do. Numeric values compare by value, so `1` equals
`1.0`, while `true` and `"1"` are distinct. A nested array is kept as an element
and is never flattened into the destination array. The element must satisfy
the declared array item type.

```ts
// For a t.array(t.json()) field:
await tx.notes.update({ id }, { items: { $push: ["a", "b"] } });
await tx.notes.update({ id }, { items: { $addToSet: { active: true } } });
await tx.notes.update({ id }, { items: { $pull: null } });
```

These updates are atomic on PostgreSQL and SQLite and preserve the order and
types of the remaining elements. JSON null is a valid JSON-array element.
A null array field stays null; initialize it with `$set: []` before appending.

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
- A transaction owns exactly **one** connection, so its operations cannot
  overlap: `await` each call inside the callback before starting the next.
  `Promise.all([tx.a.insert(...), tx.b.insert(...)])` runs them concurrently
  on that one connection and the losing branch is refused with
  `TRANSACTION_CONNECTION_BUSY`.
  On `pnpm dev` the same code has a second cause, because SQLite has one
  transaction connection per process: starting a `db.transaction()` while
  *any* transaction is open — including one belonging to another app in the
  same dev process — is refused with it too. The error message distinguishes
  the two; the deployed tier only ever raises the overlapping-operations one.
- Everything a callback starts must also **finish** inside it. A promise the
  callback never awaits keeps running after the transaction settles, and the
  database calls it then makes belong to a transaction that no longer exists;
  they are refused with `TRANSACTION_SCOPE_EXPIRED` rather than being committed
  on their own.
- Which transaction an operation belongs to is decided by where the call was
  made, not by what happens to be open at the time. A plain `db.<table>.*`
  call is its own unit of work even while another request holds a transaction
  open for the same app, and a call made inside a callback still belongs to
  that transaction on any branch of the callback's own async work.
  The previous bullet now holds on **both** tiers: SQLite keeps a separate
  connection for the one open transaction, so an ordinary write issued while a
  transaction is open is its own unit of work there too. What still differs is
  how many transactions can be open at once — on `pnpm dev` the answer is one
  per process, shared across every app, and a second `db.transaction()` is
  refused immediately with `TRANSACTION_CONNECTION_BUSY` rather than waiting
  for a connection. Do not rely on `pnpm dev` to tell you whether concurrent
  transactional work is correct — see
  [sqlite-divergences.md](./sqlite-divergences.md#current-differences).
- `isolationLevel` accepts `"read uncommitted"`, `"read committed"` (default),
  `"repeatable read"` or `"serializable"` — the SQL spellings, with a space.
  This page documented `"readCommitted"` / `"repeatableRead"` until 2026-08-10;
  those are the spellings the runtime happens to accept, but they are NOT in the
  published type (`ZeroshipIsolationLevel`), so a TypeScript caller writing what
  this page said got `TS2820: Type '"repeatableRead"' is not assignable ... Did
  you mean '"repeatable read"'?` The type is the contract a creator hits first,
  so the page now matches it.
  Note the level only affects the deployed tier: SQLite validates the value and
  then runs a plain `BEGIN`. See
  [sqlite-divergences.md](./sqlite-divergences.md#current-differences), which
  also records what that divergence has and has not been measured under.
- Outside the callback the result is again a `Result<R>` — the surrounding
  `transaction()` call doesn't throw.

The procedure wrappers `query()`, `mutation()`, and `action()` from
`@zeroship/rpc/server` do not open a transaction implicitly. Top-level
`db.<table>.*` calls autocommit per operation. Use explicit
`db.transaction()` when a handler needs multiple database operations to
commit or roll back as a unit.

## Live queries (`db.live`)

`db.live(queryFn)` is the creator-facing reactive API. The lower-level
broker subscription primitive exists for framework-internal consumers;
app code should use `db.live(...)`, not `openSubscription()`.

`db.live(queryFn)` wraps the raw subscription stream into a
query-shaped reactive primitive. It first runs `queryFn` only to discover
the watched tables. It then opens and arms those subscriptions, reruns the
query, and yields that fresh result. Later changes yield fresh result arrays.
The discovery result is never exposed as an apparently live snapshot.

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
throws synchronously with `code = "LIVE_IN_TRANSACTION"`. Open the
live query before (or after) the tx.

**Cleanup.** `close()` is idempotent and cancels every underlying
subscription. The iterator's `return()` (invoked by `for await ...
break` or an early `throw`) also calls `close()` automatically.

### Distributed delivery and lifecycle

PostgreSQL live queries receive committed invalidations from the separately
deployed CDC relay. The relay owns a slot for each subscribed app and shares
capture across worker connections. Each worker's ORM broker distributes those
invalidations to its local Rust and V8 subscriptions. Row values stay out of the
transport; live queries re-read through the usual ORM access controls.

The first local subscriber connects lazily. `Subscription.ready()` resolves
after the relay authenticates the worker and PostgreSQL accepts capture for the
app's migration-owned publication. `db.live` waits before taking its initial
snapshot. Missing relay configuration or failed startup rejects the live query.
Connection loss, queue overflow and reconnect request a fresh snapshot.

Closing the final local subscription disconnects that worker. The relay releases
the app's slot after its final connected subscriber leaves. Apps without live
queries retain no relay capture. File-backed SQLite captures commits locally and
uses the same ORM broker without a relay service.

Archiving an app retains its worker version-feed entry and existing live
subscriptions. Gateway routing and new workflow admission stop. Publication,
schema and role deletion remain separate privileged lifecycle operations.

## Errors

Errors carry a `.code` property where applicable:

| `error.code`                | When                                                |
|-----------------------------|-----------------------------------------------------|
| `VALIDATION`                | Input fails schema validation.                      |
| `UNIQUE_VIOLATION`          | Duplicate unique-key violation.                     |
| `ROW_DECODE_FAILED`         | A result column contains malformed binary data, an unsupported physical type, or a non-finite value that the native data contract cannot represent. The error identifies the column; SQL NULL remains a valid value. |
| `OPTIMISTIC_CONCURRENCY`    | `update` with a CAS version that didn't match.     |
| `SCHEMA_NOT_PROVISIONED`    | The app's database was never provisioned: its per-app Postgres role does not exist. Run `zeroship migrate` for the app. Reachable only for an app deployed WITHOUT a generated descriptor - one that carries a descriptor is refused at deploy with `409 schema_not_applied` instead (see [Migrate before you deploy](#migrate-before-you-deploy)). |
| `GRANT_REVOKED`             | PostgreSQL refused the transaction's per-app role because the worker login no longer holds that grant. This is a terminal HTTP 403; restore the database grant before retrying. |
| `MIGRATION_*` (see above)   | Migration lifecycle errors.                         |
| `INVALID_K`, `VECTOR_EXTENSION_MISSING`, `POSTGIS_EXTENSION_MISSING`, `VECTOR_DIMENSION_MISMATCH`, `POLYGON_OPS_PG_ONLY` | Vector / geo paths - see [Vector / Geo: Error codes](#error-codes). |

Use the property directly — never substring-match on `error.message`.

```ts
const { data, error } = await db.users.update({ id, version: 5 }, { name });
if (error?.name === "OptimisticLockError") {
  // refetch and retry
}
```

## Native surface (advanced)

The SDK calls into a small native surface registered as `env.db` by the
Rust DbPlugin. App code rarely needs it; SDK packages use it directly.

**Creator-reachable on `env.db`:**

- `env.db.collection(name)` → `Collection` wrapper (per-collection CRUD)
- `env.db.transaction(fn, opts?)` → native transaction orchestrator
  (begin/commit/rollback/nested-savepoint owned in Rust; throw to abort,
  resolve to commit — see [Transactions](#transactions))

**Platform-internal — NOT on `env.db` (P9 §8 `__platform` capability gate):**

`setMaskPolicy` lives on a `DbPlatform` capability handle the runtime sets
on `env.db` under a **V8 private symbol** and hands only to
`@zeroship/bootstrap`'s runtime-entry. The handle is unreachable from app
code. Replication operations exist only in the relay service:

- `env.db.__platform` (string access) throws `PLATFORM_INTERNAL_ONLY`.
- The handle is invisible to `Object.keys` / `getOwnPropertyNames` /
  `getOwnPropertySymbols` / `Reflect.ownKeys` / `for..in` / JSON — a
  `v8::Private` slot is not a JS property and cannot be keyed from JS.
- Runtime boot validates the generated descriptor and asks the data adapter to publish
  its complete collection field-map set natively before creator modules run.
  `installSchema` reads the same descriptor only to plant typed JavaScript
  `Collection` wrappers.

Public type contracts live in `sdks/types/db.d.ts` (which no longer
declares the platform-internal classes — those moved to
`@zeroship/bootstrap`'s framework-internal `internal.d.ts`). The runtime
implementation lives in `crates/zeroship-data-v8/` (`v8_classes/db.rs`,
`v8_classes/db_platform.rs`).

## Per-app isolation

Every app gets its own Postgres schema (UUID-based):

```sql
SELECT * FROM "<app-uuid>"."users" WHERE ...
```

The `app_id` is injected by the runtime from `env_vars`; user code can
neither read nor override it. `env.db` is frozen.

## Column assignments

The migration renderer preserves effective policy assignments in
`schema.runtime.json`. The ORM resolves them per collection, without a global
field-name list or runtime policy copy. Lifecycle columns can be renamed with
their assignments and roles; the primary key remains `id`.

| Descriptor metadata | Runtime behavior |
| --- | --- |
| `assign: { by: "typedId", on: "insert" }` | Generate an identifier on insertion. |
| `assign: { by: "actor", on: "write" }` | Stamp the request actor on insertion and writes. |
| `assign: { by: "now", on: "write" }` | Use the database clock on insertion and writes. |
| `assign: { by: "increment(N)", on: "write" }` | Initialize from the database default, then increment on writes. |
| `primaryKey: true` | Required on `id`, the collection's sole primary key. |
| `concurrency: true` | Interpret the field's equality predicate as a revision check. |
| `softDelete: true` | Use the field as the deletion marker and read-visibility filter. |

For a descriptor with revision `revision` and deletion marker `removed`,
identity stays `id` while lifecycle operations follow the declared roles:

```ts
const { data: post } = await db.posts.insert({ title: "First post" });
if (!post) throw new Error("insert failed");
await db.posts.update(
  { id: post.id, revision: post.revision },
  { title: "Edited" },
);
await db.posts.delete(post.id);
await db.posts.restore(post.id);
await db.posts.purge(post.id);
```

A stale revision returns an optimistic-concurrency error. Omitting the revision
predicate permits a blind update; declared write generators still run.
`delete` physically removes rows when no soft-delete role is declared. With that
role, it applies delete assignments and hides the row. `restore` clears delete
assignments and runs write assignments. `purge` always removes the row.

Rust uses these same semantics through `Database` and its typed collections.
`schema!` reads the deployment descriptor; generated write capabilities exclude
assigned fields. The V8 bridge forwards operations to this ORM.

Implementation: `crates/zeroship-data-orm/src/assignments.rs`,
`crates/zeroship-data-orm/src/crud/assignment_pass.rs`,
`crates/zeroship-data-orm/src/sql/lifecycle.rs`.

## Masking

**Encryption hides at rest. Masking hides at read time.** They are
sibling concerns and compose: an `t.encrypted(...)` column without
an explicit `.mask(...)` declaration is treated as `.mask({ kind:
"full", classification: "pii" })` by default. The full design lives
in `docs/archive/sensitive-field-masking.md` (shipped; archived).

### Mental model

| Layer       | What it does                                  | When it runs       |
|-------------|-----------------------------------------------|--------------------|
| Encryption  | Replaces stored bytes with ciphertext (AEAD)  | Insert / update    |
| Masking     | Returns a `MaskedValue<T>` wrapper on reads   | Find / get / live  |
| Unmask      | Trades the wrapper for plaintext (audited)    | Explicit call only |

A default read of a masked column **never** decrypts. The platform stores a
pre-computed mask in the field's **own** column and the real value in a hidden
sibling, so a plain `SELECT "ssn"` returns the mask. The column holding the real
value never leaves the database on a default read, and the column-derivation key
is never consulted.

#### Querying a masked column

Every query surface - `find`, `orderBy`, `select`, `distinct`, `aggregate` -
sees the **mask**, because that is what the column with the field's name
contains. Three consequences, all deliberate:

- **`find({ ssn: "123-45-6789" })` matches nothing.** Equality by real value is
  not a query any more; it is an `unmask()` call, which is where the
  authorization check and the audit row live.
- **Range filters on a masked field are refused.** Equality and pattern filters
  compare the visible mask.
- **`orderBy: { ssn: 1 }` sorts by the mask**, and `aggregate`'s `$group.by`
  buckets by the mask - one bucket per distinct MASK, not per distinct value.

`.unique()` and `.index()` still mean what they say: they are declared about the
real value and the platform builds them on the column that holds it, so two rows
whose SSNs differ but whose masks collide (`***-**-1234`) both insert, and two
rows with the SAME SSN still conflict.

If you need to look a row up by its real value, that is
`unmask`-shaped work, not filter-shaped work - the mask is not a lossy index
into the plaintext and no query can treat it as one.

### Schema declaration

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_users_sensitive_fields",
  schema() {
    table("users").create({
      columns: {
        name: t.text().notNull(),
        email: t.encrypted({ of: t.text() })
          .mask({ kind: "email", classification: "pii" }),
        ssn: t.encrypted({ of: t.text() })
          .mask({ kind: "last4", classification: "spi" }),
        dob: t.encrypted({ of: t.text() })
          .mask({ kind: "dateYear", classification: "phi" }),
      },
    });
  },
};
```

`t.encrypted({ of })` without `.mask({...})` is shorthand for
`.mask({ kind: "full", classification: "pii" })`. `t.text().mask({
kind: "full", classification: "public" })` (mask without encryption)
is also valid — masking is the read-side; encryption is the
storage-side; they're independent.

### The eight mask kinds

| Kind         | Input            | Output             | Use case                       |
|--------------|------------------|--------------------|--------------------------------|
| `full`       | `"123-45-6789"`  | `"***"`            | Default — safest                |
| `last4`      | `"123-45-6789"`  | `"***-**-6789"`    | Card tail / SSN tail            |
| `first4`     | `"4111222233334444"` | `"4111************"` | Card BIN visible       |
| `email`      | `"alice@x.com"`  | `"a***@x.com"`     | Identifiable but not enumerable |
| `name`       | `"Alice Smith"`  | `"A. S***"`        | Initials, last name marked      |
| `dateYear`   | `"1990-05-12"`   | `"1990-**-**"`     | Year-only for analytics         |
| `dateDecade` | `"1990-05-12"`   | `"199?-**-**"`     | Decade-only — coarser           |
| `none`       | `"x"`            | `"x"`              | Opt-out — read returns bare `T` |

`full` emits exactly three asterisks whatever the input length, so it does
not disclose how long the original value was. `last4` and `first4` preserve
non-alphanumeric characters in place, which is why the separators survive in
the `last4` row above; they count only alphanumerics when deciding what to
keep.

`null` plaintext passes through as `null` (no mask). Empty string
becomes `""`. Numbers and `Uint8Array` are supported by `full`.

The string-oriented kinds are **not** type-checked, at deploy or at
write. Nothing is rejected and nothing is thrown if a kind meets a value
whose shape it did not expect. What happens instead differs by kind, and
the difference matters:

| Kind | On an unexpected shape |
| --- | --- |
| `email`, `dateYear`, `dateDecade` | falls back to `"***"` — full redaction |
| `name` | still emits initials: a single token becomes `"4***"` |
| `first4`, `last4` | no shape check at all — reveals 4 alphanumerics by position |

Only the first row fails safe. `name`, `first4` and `last4` reveal
characters from whatever column they are pointed at, so a mask attached
to the wrong field can disclose the leading or trailing characters of a
value you meant to protect — a `last4` aimed at a date yields its last
four digits just as readily as a card's.

Verify masks by reading a masked row back, not by deploying successfully.
A successful deploy establishes nothing about whether a kind suits the
column it is attached to.

### The six classifications

| Classification | Scope                                          |
|----------------|------------------------------------------------|
| `public`       | Display names, public profile data — visible to all. |
| `pii`          | Email, address, phone, DOB. Default for `t.encrypted()`. |
| `spi`          | CPRA "sensitive PI": SSN, biometric, driver's licence. |
| `phi`          | HIPAA scope: medical records, diagnosis.              |
| `pci`          | PCI-DSS scope: card numbers, CVV.                     |
| `internal`     | Platform-internal metadata.                           |

`defineMaskPolicy()` (below) grants unmask rights per role per
classification.

### Reading masked

Default — the row carries `MaskedValue<T>` for masked columns:

```ts
const { data: user } = await env.db.users.get(id);
user.ssn;             // MaskedValue<string>
user.ssn.toString();  // "***-**-6789"
user.ssn.toJSON();    // "***-**-6789"  (Response.json safe)
user.name;            // "Alice"  (non-masked, bare string)
```

`MaskedValue` is intentionally NOT transparent — `tsc` rejects
`user.ssn.length` so a leak path that "just works" at runtime
doesn't compile.

### Unmasking

Single column on a row (writes one `__zeroship_audit_unmask` row):

```ts
const plain = await user.ssn.unmask({
  actor: "support_agent",
  reason: "verify ticket #12345",
});
```

Multiple columns on the same row, atomic (the FIRST unauthorised
column fails the whole call):

```ts
const [ssn, dob] = await user.unmask(["ssn", "dob"], {
  actor: "tax_handler",
  reason: "1099 generation",
});
```

Bulk unmask across rows (single RPC; atomic):

```ts
const plains = await env.db.users.bulkUnmask(
  [
    { id: "usr_01hxyz...", columns: ["ssn"] },
    { id: "usr_01hxza...", columns: ["ssn", "email"] },
  ],
  { actor, reason },
);
```

Per-query unmask hint (auth check runs once before the SELECT;
hinted columns arrive as bare `T`, others as `MaskedValue<T>`):

```ts
// Per-query unmask hint via the SDK Query — applies the `unmask`
// authorisation upfront and the row carries plaintext for the listed
// columns when allowed by the per-app mask policy.
const { data: user } = await env.db.users
  .find({ id }, { unmask: ["ssn"], actor, unmaskReason: reason })
  .first();
user.ssn;    // "123-45-6789"  (plaintext, hint applied)
user.email;  // MaskedValue<string>  (not hinted)
```

### `defineMaskPolicy()`

App-wide policy mapping `actor` → permitted classifications.
Recommended pattern: declare at the app's entry module so every
isolate sees the same policy on boot.

```ts
import { defineMaskPolicy } from "@zeroship/db";

await defineMaskPolicy(env.db, {
  admin: ["public", "pii", "spi", "phi", "pci", "internal"],
  support: ["public", "pii"],
  end_user: ["public"],
  // `auto` (the system actor) has uniform access UNLESS listed.
});
```

Policy is keyed by app id — app A's policy never leaks to app B's
isolate. Two policies under the same app id replace, never merge.

### Audit tables

| Table                              | Written by                          |
|------------------------------------|-------------------------------------|
| `__zeroship_audit_unmask`          | Every `.unmask()` call (granted or denied). |

This table lives in the per-app schema; standard isolation rules
apply (`SELECT * FROM "<app>".__zeroship_audit_unmask`).

**The unmask audit row is written OUTSIDE your transaction, on purpose.**
An `unmask()` or `find({ unmask })` issued inside `db.transaction(fn)`
reads its plaintext on the transaction's own connection - so it sees
rows the same transaction has just written - but the audit row commits
independently. Roll the transaction back and the audit row stays:

```ts
await db.transaction(async (tx) => {
  const u = await tx.users.insert({ ssn: "123-45-6789" });
  await u.ssn.unmask({ actor });   // reads the row the tx just wrote
  throw new Error("abort");        // the row is gone; the audit row is not
});
```

That is the contract, not an artefact. A denied attempt must not be
erasable by rolling back the transaction it was made in, and a granted
one records that plaintext left the database - which a rollback does
not undo. The table has no transactional relationship to the rows it
names; it stores their ids as text and holds no foreign key into them.
Like every table in the creator schema, it is addressable through a
declared ORM collection and has ordinary data privileges. Its name does
not make it hidden or append-only.

## Encrypted and Masked Fields (Shipped Reference)

This section resolves `docs/archive/sensitive-field-masking.md` against the shipped implementation in `sdks/db/src/types.ts`, `crates/zeroship-data-orm/src/sql/mapping.rs`, `crates/zeroship-data-orm/src/protection/mask_pass.rs`, `crates/zeroship-data-v8/src/v8_classes/masked_value.rs`, `crates/zeroship-data-orm/src/protection/unmask.rs`, `sdks/db/src/collection/masking.ts`, `sdks/db/src/policy.ts`.

The migration engine records physical placement in each field's runtime
`storage` mapping. Default reads use `storage.valueColumn`; authorized unmasking
and protected writes use `storage.rawColumn`. The raw column holds the real
value and its constraints; the visible column holds the mask. The ORM validates
that the raw column is inaccessible to ordinary creator queries.

`t.encrypted(...)` applies a full mask with `pii` classification by default.
`.mask({ kind: "none" })` opts into plaintext reads and suppresses masked
storage. Both operations follow the runtime descriptor rather than naming a
mask column from a suffix (`crates/zeroship-data-orm/src/sql/mapping.rs`,
`crates/zeroship-data-orm/src/protection/mask_pass.rs`).

On writes, `apply_mask_on_write` computes the mask from plaintext. After the
encryption and byte passes, relocation follows the descriptor's `storage`
mapping: the finished value moves to `storage.rawColumn` and the mask remains
in `storage.valueColumn`. Null and absent values relocate nothing, and
`kind: "none"` skips masking (`crates/zeroship-data-orm/src/protection/mask_pass.rs`).
The mask kinds are defined together in `sdks/db/src/types.ts` and
`crates/zeroship-data-orm/src/protection/mask_pass.rs`.

Default reads surface `MaskedValue<T>`, not plaintext. The Rust read path wraps a masked cell in the `__zsmask__` sentinel shape, then the runtime rehydrates that sentinel into a native `MaskedValue` v8 class before user code sees the row (`crates/zeroship-data-orm/src/protection/mask_pass.rs`, `crates/zeroship-data-v8/src/v8_classes/masked_value.rs`, `sdks/db/src/types.ts`). The shipped surface is intentionally coercion-safe: `masked` and `classification` are readable, `_meta` carries `{ collection, row_pk, column }`, and `toString()` / `toJSON()` return the masked string (`crates/zeroship-data-v8/src/v8_classes/masked_value.rs`, `sdks/db/src/types.ts`).

Plaintext reveal is always explicit. `await row.ssn.unmask({ actor?, reason? })` reveals one field on one row, and `await row.ssn.unmask(["ssn", "dob"], { actor, reason })` fans out across multiple columns on the same row (`crates/zeroship-data-v8/src/v8_classes/masked_value.rs`, `sdks/db/src/types.ts`). `Collection.bulkUnmask()` is the shipped multi-row path; it maps `(id, columns)` pairs to the native collection op and is atomic, so one unauthorized `(row, column)` pair rejects the whole call (`sdks/db/src/collection/masking.ts`, `crates/zeroship-data-orm/src/protection/unmask.rs`). The per-query hint `find(..., { unmask: [...], actor, reason })` promotes only the listed columns to plaintext while leaving other masked columns wrapped, and it writes audit only after a successful query (`crates/zeroship-data-orm/src/protection/unmask.rs`, `sdks/db/src/types.ts`).

`defineMaskPolicy()` is the app-scoped authorization declaration for unmasking. It validates the classifications (`public`, `pii`, `spi`, `phi`, `pci`, `internal`) and snapshots a pending role-to-classification map for bootstrap to install. Declarations may be replaced during startup; after the startup flush, further calls fail with `MASK_POLICY_IMMUTABLE`. The policy is held in memory for the app and deployment. No database backend persists it, and changes require redeployment (`sdks/db/src/policy.ts`). If an app never calls `defineMaskPolicy()`, the fallback is strict: only the `auto` actor can unmask. If the app does declare a policy, `auto` still keeps full access unless the policy explicitly lists `auto` with a narrower set (`sdks/db/src/policy.ts`).

`__zsmask__` is the read-side wire sentinel for a masked value payload (`sdks/db/src/types.ts`, `crates/zeroship-data-orm/src/protection/mask_pass.rs`, `crates/zeroship-data-v8/src/v8_classes/masked_value.rs`). Catalog sentinels record stored protection. The mask marker carries its kind and classification; the encryption marker records only that protection is present from the ORM's perspective. Runtime type and storage behavior always come from the installed descriptor (`crates/zeroship-data-orm/src/sql/mask_codec.rs`).

Column encryption is always randomised. Each write uses a fresh nonce and
binds authentication to the app, collection, column and row identity.
Encrypted fields cannot be filtered (including
equality and `IN`), sorted, grouped, or declared unique. Masking remains a
separate read policy; disabling the mask does not enable encrypted queries.
Use an ordinary field to locate a row before reading or updating its encrypted
values. These rules apply to Rust callers and worker TypeScript alike.

Runtime descriptors carry the logical plaintext type and an encryption flag:

```json
{ "type": "number", "encrypted": true, "mask": { "kind": "full", "classification": "pii" } }
```

The SDK declaration is `t.encrypted({ of: t.number() })`; `t.encrypted()`
selects string plaintext. Rust writes, reads and unmasking share a native
plaintext codec selected by `type`. Binary values remain native buffers.
The physical catalog marks the column as encrypted. It is not a second source
of runtime type metadata.

The host supplies a project encryption key and explicit app-to-project bindings
through `DbServiceConfig.project_keys`. Every encrypted column in that project
uses the same key. The ORM reads no column keys from environment variables or
tenant tables.

Control generates and persists the project key in `zeroship.project_data_keys`,
wrapped with its secret-storage master key and authenticated against the project
identity. Workers hydrate their shared key source through the authenticated
`/internal/apps/{app_id}/data-key` endpoint before loading an app. Key material
never enters the app environment, runtime descriptor or deployment bundle.
Wrapping-key rotation preserves the data key and rewraps it when next read.

The standalone development host keeps its own project key in
`.zeroship/private/project-data-key.json`. It survives runtime restarts and is
shared by apps served from that local project directory. Keep this private file
with local database backups; it is separate from the deployed project's key.
