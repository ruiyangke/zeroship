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
  up() {
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
  applied", so rolling code back over a schema change means rolling the schema
  forward - there is no reverse.

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
app. Do not add `@zeroship/db/env` or a `zeroship-schema` path alias; that
declared-schema typing path is retired.

The root `@zeroship/db` package remains the plain TypeScript SDK surface
(`t`, `schema`, `RowOf`, `Db`, etc.) for shared packages and tests; only the
app-level `Env.db` augmentation moved to generated code.

### Collection names

Collection names created by migrations become physical table names. Keep them
ASCII alphanumeric plus underscores, at most 63 bytes, and avoid the reserved
prefixes `pg_`, `__zero_migrate`, and `__zeroship`. The data-plane and
schema-query validators refuse these names, and declarative migration loading
calls the engine validator before lowering emits SQL. The offline `loadVerify`
surface reports this as `ok: false`; managed HTTP apply surfaces validation
failures as 422, not as a literal authoring-time 400.

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
| `t.number()`           | `number`                      | NUMERIC         |
| `t.boolean()`          | `boolean`                     | BOOLEAN         |
| `t.timestamp()`        | `number` (Unix ms)            | TIMESTAMPTZ     |
| `t.calendarDate()`     | `string` (`YYYY-MM-DD`)       | DATE            |
| `t.json()`             | `Record<string, unknown>`     | JSONB           |
| `t.array(t.string())`  | `string[]`                    | JSONB           |
| `t.ref("users")`       | `Id<"users">` (branded string) | TEXT + FK      |
| `t.object({ ... })`    | nested inferred object        | JSONB           |
| `t.literal("login")`   | `"login"`                     | underlying type |
| `t.union(v1, v2, ...)` | discriminated union           | flat columns    |
| `t.bytes()`            | `string` (base64)             | BYTEA           |

`t.date()` is **not** in the surface — use `t.timestamp()` for a TIMESTAMPTZ
(Unix-ms numbers at the JS layer) or `t.calendarDate()` for a Postgres DATE
(`YYYY-MM-DD` strings).

`t.bytes()` is the one field whose JS type is not the shape it stores. The
column is BYTEA and holds RAW BYTES; the value you write and the value you read
back are the **base64 encoding** of those bytes, because `serde_json::Value`
has no binary variant and every `env.db` argument crosses a JSON boundary. So a
round trip is `base64(x)` in, `base64(x)` out, and `x` on disk: encode once,
never twice:

```ts
const toBase64 = (bytes: Uint8Array) => {
  // Chunked: `String.fromCharCode(...bytes)` spreads every byte as an
  // argument and blows the call stack on a real file.
  let s = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(s);
};

const png = new Uint8Array(await file.arrayBuffer());
await env.db.uploads.insert({ blob: toBase64(png) });

const row = await env.db.uploads.find({ id }, { limit: 1 });
const back = Uint8Array.from(atob(row[0].blob), (c) => c.charCodeAt(0));
```

A value that is not a base64 string is rejected at the write boundary with
`invalid_bytes_arg` rather than stored. Passing raw binary, a byte array, or
base64 with embedded newlines is an error, not a second encoding.

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

You never declare these; every collection has them. They're the
platform "system fields" — full documentation lives in the
[System fields](#system-fields) section below:

- `id: string` — `TEXT PRIMARY KEY`, typed_id (`<prefix>_<base62(uuidv7)>`), platform-minted. The `<prefix>` is auto-derived from the collection name; override it with `id: t.id("blog")` — see [Typed-id prefixes](#typed-id-prefixes).
- `created_at: number` — Unix-ms timestamp at INSERT.
- `updated_at: number` — Unix-ms timestamp at INSERT; bumped on every UPDATE.
- `created_by: string | null` — session actor at INSERT (`null` for system writes).
- `updated_by: string | null` — session actor at every UPDATE.
- `version: number` — `1` at INSERT; auto-bumped on every UPDATE; supports optimistic concurrency.
- `deleted_at: number | null` — `null` (live) by default; `delete()` stamps the current Unix-ms time.

These seven columns are added by the platform on every table created
through the schema DSL. Soft delete and the physical `version` column
are runtime-owned system-field behavior, not opt-in schema features. See
[System fields](#system-fields) for the full semantics.

### Typed-id prefixes

Every row's `id` is a **typed_id**: `<prefix>_<base62(uuidv7)>`. The
22-character body is a UUIDv7 — globally unique and sortable by creation
time. The prefix is a human-readable type tag; it carries **no**
uniqueness (uniqueness lives entirely in the UUIDv7 body).

**Default — auto-derived from the collection name.** With no
declaration, the prefix is computed from the collection name: strip a
trailing `s` (when the name is longer than one character), lowercase,
take the first 4 ASCII alphanumerics. An empty result falls back to
`row`.

| Collection   | Auto prefix | Example id              |
| ------------ | ----------- | ----------------------- |
| `posts`      | `post`      | `post_01HXY3Z9PQR2…`    |
| `users`      | `user`      | `user_01HXY3Z9PQR2…`    |
| `categories` | `cate`      | `cate_01HXY3Z9PQR2…`    |

**Override — `id: t.id({ prefix })`.** Declare the prefix explicitly in the
migration that creates the table:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_posts",
  up() {
    table("posts").create({
      columns: {
        id: t.id({ prefix: "blog" }), // rows get ids like blog_<22 base62 chars>
        title: t.text().notNull(),
      },
    });
  },
};
```

`id: t.id({ prefix: "blog" })` is a **prefix declaration** for the system `id`
column — it does not emit a second column, and it is the only sanctioned
way to name `id` in a schema (you otherwise never declare `id`). The
prefix must match `^[a-z][a-z0-9_]*$`.

**`usr` is reserved.** It is the platform user-id prefix, so
`t.id({ prefix: "usr" })` is rejected (and the auto-derivation will never
produce it either — e.g. a collection named `usrs` derives `usrs`, not `usr`).

**Ordering.** The UUIDv7 body is encoded as a fixed-width base62 string
using the runtime's ordered alphabet, so lexicographic `id` order
preserves creation order. Use `.sort({ id: -1 })` for stable
newest-first feeds and `.sort({ id: 1 })` for oldest-first pagination.
`created_at` is still the display/filter timestamp, but SQLite's
`CURRENT_TIMESTAMP` has second-level granularity and can tie under quick
dev inserts.

### Per-collection options

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_todos",
  up() {
    table("todos").create({
      columns: {
        title: t.text().notNull(),
        done: t.boolean().default(false),
      },
      strictness: "strict",
      indexes: [{ name: "by_done", columns: ["done"] }],
    });
  },
};
```

- `softDelete: true` — not required for normal CRUD. The
  runtime creates `deleted_at` on every table, `delete()` soft-deletes,
  and read paths hide deleted rows through the system-fields layer.
- `versioning: true` — not required to create or bump the
  physical `version` field. The runtime creates `version` on every table
  and treats `update({ id, version: N }, ...)` as a CAS guard. The SDK
  flag currently controls the higher-level `OptimisticLockError`
  mapping for wrapper-side CAS helpers while this pre-launch surface is
  being simplified.
- `strictness: "strict" | "lenient" | "off"` — deploy-time data-validation
  policy. The default is `strict`; set this in the migration if you need
  `lenient` or `off`. The authoring types live in `sdks/migrate/src/types.ts`
  and the deploy-time gate lives in the migration engine and DB plugin.

### Named indexes

Declare named, multi-column indexes the way Convex does — they document
the queries you intend to run and the SDK warns you when a filter walks
the table without hitting one.

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_todos_indexes",
  up() {
    table("todos").create({
      columns: {
        userId: t.ref("users"),
        done: t.boolean().default(false),
        email: t.text(),
      },
      indexes: [
        { name: "by_email", columns: ["email"] },
        { name: "by_user_done", columns: ["userId", "done"] },
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
schema. The auto-generated columns (`id`, `created_at`, `updated_at`,
`created_by`, `updated_by`, `version`, `deleted_at`) are accepted.

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

`t.ref("users")` is also the foreign-key builder. By default it emits a
same-app FK with NO `ON DELETE`, `ON UPDATE`, or `DEFERRABLE` clause at all,
so the database's own defaults apply: `NO ACTION` for both actions, and
immediate (non-deferred) checking. On Postgres `NO ACTION` rejects a delete
that would orphan a row, the same as `RESTRICT`, but defers the check to the
end of the statement rather than firing per row.

Override with `t.ref("users", { onDelete: "cascade" })` for physical cascade,
or `{ deferrable: true }` if you need cyclic refs insertable within one
transaction — that is opt-in, not the default. Cross-app targets are refused;
FKs stay inside the calling app. See `sdks/db/src/types.ts` for the builder,
and the next section for what does the refusing, which is not what this page
said until 2026-08-20.

### Can an FK point at another app's table?

No, and it is worth being exact about which code makes that true, because two
plausible-looking answers are wrong.

**It is not `crates/zeroship-plugin-db/src/cross_app_fk.rs`.** That file holds a
`reject_cross_app_fk` validator that scans `refTarget` for an `<other_app>.`
prefix and returns `cross_app_fk_forbidden`, and it is the mechanism this page
used to cite. As of 2026-08-20 it has **no production call site**: one caller
lives in a `#[cfg(any(test, feature = "test-helpers"))]` module, the other in a
function whose every caller is a `#[cfg(test)]` test. Production
`registerModel` issues no DDL on either backend, so it never runs. Nor is it
`crates/zeroship-schema`, whose FK builders are reached only from that same
cfg-gated pipeline; the migration engine carries its own copy of that renderer
and does not depend on the crate.

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

The `ForeignKeyReference.schema` field in `sdks/migrate/src/types.ts` is an
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

// By filter (returns the first match, or null)
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

Supported accumulators: `$count`, `$sum`, `$avg`, `$min`, `$max`, `$first`.

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

Declare a column with `t.vector(dims, opts?)`:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_docs",
  up() {
    table("docs").create({
      columns: {
        title: t.text().notNull(),
        embedding: t.vector(1536, { metric: "cosine" }), // dims in 1..=16000
      },
    });
  },
};
```

- `dims` is **required** and must lie in `1..=16000` (pgvector's
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
specialised to the metric). On SQLite this is the distance reported
by the `sqlite-vec` `vec0` virtual table — the `MATCH` operator
returns rows joined back to the base collection by `rowid`, with
`v.distance` aliased as `_distance`.

### Geo (point + radius)

Declare a geo column with `t.geoPoint()`:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "create_stores",
  up() {
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

### Where the search index comes from

**Your migration builds it, not the first query.** Declaring
`t.vector(dims, { metric })` or `t.geoPoint()` makes the index part of
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
- **SQLite vector** — `sqlite-vec` `vec0` virtual table, statically
  compiled into the binary via the `sqlite-vec` Rust crate (no `.so`
  shipping; the bundled-SQLite invariant is preserved). SIMD distance
  + native dimension validation + `MATCH` query operator. The base
  collection keeps a `BLOB` column for the vector payload; AFTER
  triggers mirror writes into the `<collection>__vec_<column>` vec0
  vtable so reads can JOIN base <-> vec0 on `rowid` and rank by
  `MATCH` distance. Metric is pinned at vtable-creation time
  (`distance_metric=cosine|l2`); **inner product is not supported on
  SQLite** — vec0 supports cosine + L2 only, and `metric:
  "inner_product"` surfaces as a typed `VECTOR_UNSUPPORTED_METRIC`
  error. Use PG (pgvector `vector_ip_ops`) for production inner-
  product workloads.

  **Not yet built by the migration toolchain.** The `vec0` vtable and
  its triggers are the one search object nothing creates for you today:
  on SQLite a `.vector()` field currently migrates to a `BLOB` column
  with a plain index, and `.search()` against it fails rather than
  falling back to a scan. Vector search on the `pnpm dev` tier is
  therefore not usable yet; run vector workloads against PG.
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

- Comparison: `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`
- Inclusion: `$in`, `$nin`
- String: `$like`, `$ilike`
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

Postgres live queries use logical decoding. Each app has one shared
publication and one logical slot for each worker process that currently has
local subscribers. A process-wide broker routes the decoded event to matching
subscriptions in every isolate thread in that worker. Separate worker
containers each consume their own slot, so a write handled by any isolate or
container reaches every subscribing container.

The first local subscription starts CDC lazily. `Subscription.ready()` does
not resolve until publication and worker-slot provisioning succeeds and
Postgres accepts `START_REPLICATION`. `db.live` waits for that handshake before
it emits its initial result. A missing logical-WAL configuration, connection
failure, or invalid replication object therefore rejects the live query; it
cannot silently degrade to a static one-shot result. If a running consumer
later exits, the worker logs one app-scoped error and closes that app's local
subscriptions.

Lazy startup is deliberate. Deploy-time provisioning would reserve a logical
slot and a replication connection in every worker for every deployed app,
including apps that never call `db.live`. With lazy startup, apps without live
queries pay no CDC connection, slot, WAL-retention, or decode cost. The tradeoff
is that the first live query pays the provisioning and startup latency.

Closing the last subscription in a worker stops its consumer and drops that
worker's slot. Other workers keep their independent slots and the shared
publication. When an app is deleted, each worker notices it leaving the route
feed, stops its local consumers, and drops the slots it owns for that app; the
teardown is idempotent and retried when a removal poll fails. Deleting an app
does NOT drop the publication, the app's schema, or its per-app role: those
outlive the app today. See
`docs/proposals/2026-08-20-deleted-app-schema-lifecycle.md`.

## Errors

Errors carry a `.code` property where applicable:

| `error.code`                | When                                                |
|-----------------------------|-----------------------------------------------------|
| `VALIDATION`                | Input fails schema validation.                      |
| `UNIQUE_VIOLATION`          | Duplicate unique-key violation.                     |
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

`registerModel`, `setMaskPolicy`, and the
`migrations` / `replication` namespaces are **not** properties of
`env.db`. They live on a `DbPlatform` capability handle the runtime sets
on `env.db` under a **V8 private symbol** and hands only to
`@zeroship/bootstrap`'s runtime-entry. They are unreachable from app
code:

- `env.db.__platform` (string access) throws `PLATFORM_INTERNAL_ONLY`.
- The handle is invisible to `Object.keys` / `getOwnPropertyNames` /
  `getOwnPropertySymbols` / `Reflect.ownKeys` / `for..in` / JSON — a
  `v8::Private` slot is not a JS property and cannot be keyed from JS.
- Schema registration happens automatically at app boot: the platform
  installer reads the generated runtime descriptor and registers each
  collection via the platform handle. You never call `registerModel` yourself.

Public type contracts live in `sdks/types/db.d.ts` (which no longer
declares the platform-internal classes — those moved to
`@zeroship/bootstrap`'s framework-internal `internal.d.ts`). The runtime
implementation lives in `crates/zeroship-plugin-db/` (`v8_classes/db.rs`,
`v8_classes/db_platform.rs`).

## Per-app isolation

Every app gets its own Postgres schema (UUID-based):

```sql
SELECT * FROM "<app-uuid>"."users" WHERE ...
```

The `app_id` is injected by the runtime from `env_vars`; user code can
neither read nor override it. `env.db` is frozen.

## System fields

Every table the platform creates carries seven platform-managed
columns. You never declare them — they're prepended to every
`CREATE TABLE` and populated automatically on INSERT / UPDATE / DELETE.
Three implicit B-tree indexes ride along (`deleted_at`, `updated_at`,
`created_by`) to keep the hot paths cheap.

| Column        | Type (PG)       | Default            | Set by             |
|---------------|-----------------|--------------------|--------------------|
| `id`          | `TEXT` PK       | platform-minted    | typed_id (`<prefix>_<base62(uuidv7)>`); prefix auto-derived or set via [`t.id("prefix")`](#typed-id-prefixes) |
| `created_at`  | `TIMESTAMPTZ`   | `NOW()` at INSERT  | DB default         |
| `updated_at`  | `TIMESTAMPTZ`   | `NOW()` at INSERT, bumped on every UPDATE | runtime UPDATE builder |
| `created_by`  | `TEXT` NULL     | `null` if no actor | session actor at INSERT |
| `updated_by`  | `TEXT` NULL     | `null` if no actor | session actor at every UPDATE |
| `version`     | `INTEGER`       | `1` at INSERT      | runtime UPDATE builder bumps by 1 |
| `deleted_at`  | `TIMESTAMPTZ` NULL | `null` (live)   | `delete()` stamps `NOW()` |

The field names are **reserved**. Declaring a user field named `id`,
`created_at`, `updated_at`, `created_by`, `updated_by`, `version`, or
`deleted_at` is rejected — but **not at deploy time, and not always with a
readable error**. Deploy succeeds. What you get instead, measured:

- The app **builds and boots clean**. Nothing warns.
- Every `env.db` call then fails with a `500`, from schema validation:
  `collection '<name>' declares field '<field>', which collides with an
  injected policy column`.

There is a friendlier `code: "RESERVED_SYSTEM_FIELD_NAME"` with a hint listing
the reserved set, but it is raised by the boot-time schema installer, not by
deploy, and the validation above can refuse the schema before you ever see it.
Treat the reserved set as something to avoid up front rather than something the
platform will tell you about at a useful moment.

Which layer owns this rejection is being reworked: the reserved-name list is
going away in favour of the injected-column policy as the single authority. The
names above stay reserved either way — only the mechanism and the error you see
will change.

The one sanctioned exception is
`id: t.id("prefix")`, which declares the typed-id prefix for the system
`id` column rather than overriding it — see
[Typed-id prefixes](#typed-id-prefixes).

### Reading system fields

`Row<S>` includes the system fields automatically — you don't have to
declare them, and they show up on every row you read:

```ts
const { data: user } = await db.users.get(userId);
user.id;          // string ("usr_…")
user.created_at;  // number (Unix ms)
user.updated_at;  // number (Unix ms)
user.created_by;  // string | null
user.updated_by;  // string | null
user.version;     // number  (1 on a freshly-inserted row)
user.deleted_at;  // number | null  (null = live)
```

`created_by` / `updated_by` are nullable. The platform stamps them from
the request's session actor, but background writes and migrations may
have no actor in scope — in those cases the columns stay `null`.

### Optimistic concurrency via `version`

Every UPDATE auto-bumps `version` by 1 and stamps `updated_at = NOW()`.
Add `version: N` to the update filter to turn the call into an
optimistic-concurrency check:

```ts
const { data: post } = await db.posts.get(postId);
// post.version === 5

// CAS update — succeeds only if the row's stored version is still 5:
const { data, error } = await db.posts.update(
  { id: postId, version: 5 },
  { title: "edited" },
);
if (error instanceof OptimisticLockError) {
  // Someone else updated the row between our read and our write.
  // Re-read and retry.
}
```

The native dispatch composes `UPDATE … SET title = $1, version =
version + 1, updated_at = NOW() WHERE id = $id AND version = 5`.
Affected-rows = 0 surfaces as the runtime's optimistic-concurrency error,
which the SDK rethrows as `OptimisticLockError` (`code:
"OPTIMISTIC_CONCURRENCY"`, `retryable: true`) so the standard
`instanceof OptimisticLockError` check keeps working.

The physical column and native CAS behavior exist on every collection.
The current `withVersioning()` builder flag is only an SDK-side hint for
typed wrapper error mapping; it is not what creates the `version` column.

Omitting `version` from the filter is last-writer-wins — the UPDATE
still bumps `version` by 1 but doesn't refuse on a concurrent edit.
Use the [`withRetry`](#retrying-cas-updates-with-withretry) helper to
wrap the read-compute-update loop.

### Soft delete: `delete` / `purge` / `restore`

`delete()` is a soft-delete. Calling it sets `deleted_at = NOW()`,
bumps `version`, and leaves the row in storage:

```ts
await db.posts.delete(postId);
// row.deleted_at is now a timestamp; row is no longer returned by find()
```

`find()` / `count()` / `exists()` / `distinct()` / `aggregate()` all
auto-filter `WHERE deleted_at IS NULL`. The native option for internal
callers is `include_deleted: true`; the public SDK's include-deleted
read helper is still being simplified. Use `restore()` and `purge()`
for explicit lifecycle operations today.

To remove a row from storage permanently (GDPR-erase, compliance), use
`purge()`:

```ts
await db.posts.purge(postId);       // single row, returns the row that was removed
await db.posts.purgeMany({ … });    // bulk; returns { purgedCount: N }
```

To bring a soft-deleted row back, use `restore()`:

```ts
await db.posts.restore(postId);     // clears deleted_at, bumps version + updated_at
await db.posts.restoreMany({ … });  // bulk; returns { restoredCount: N }
```

`purge()` and `restore()` are CDC events too — subscribers see a
`delete` event from `purge()` and an `update` event (with `deleted_at`
flipping back to null) from `restore()`.

### Lifecycle worked example

```ts
// 1. Create a row. id is platform-minted; the rest is server-side.
const { data: post } = await db.posts.insert({
  title: "First post",
  body:  "…",
});
// post.id         === "post_01HXY…"
// post.created_at === <now>
// post.updated_at === <now>
// post.created_by === <session actor id> | null
// post.version    === 1
// post.deleted_at === null

// 2. Update with optimistic concurrency.
const { data: edited } = await db.posts.update(
  { id: post.id, version: post.version },
  { title: "Renamed" },
);
// edited.updated_at >  post.updated_at
// edited.updated_by === <session actor id> | null
// edited.version    === 2

// 3. Soft-delete. The row stays in storage; find() hides it.
await db.posts.delete(post.id);
const { data: visible } = await db.posts.get(post.id);
// visible === null  (filtered out)

// 4. Restore. deleted_at clears; version + updated_at bump again.
const { data: restored } = await db.posts.restore(post.id);
// restored.deleted_at === null
// restored.version    === 4   (delete bumped to 3; restore bumped to 4)

// 5. Purge. Row gone from storage. No restore is possible after this.
await db.posts.purge(post.id);
```

The full design lives in `docs/archive/platform-system-fields.md` (shipped; archived).

Implementation anchors:

- System columns and implicit indexes are emitted in `crates/zeroship-schema/src/query.rs`.
- Read filtering, soft delete, restore, and purge dispatch live in `crates/zeroship-plugin-db/src/crud/mod.rs`.
- Schema revalidation treats platform system fields as desired physical columns before diffing, so persistent dev databases under `.zeroship/` do not look destructive after a restart.

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
- **`find({ ssn: { $gt: v } })` compares masks**, so it cannot narrow the real
  value. This is the point: a range filter plus `orderBy` plus `limit` used to
  binary-search a value the caller could not read, with no authorization check
  on the path and nothing written to `__zeroship_audit_unmask`.
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
  up() {
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

### Drift detection

A weekly per-app cron samples 1% of rows per masked column,
recomputes the mask from the live ciphertext, and writes a row to
`__zeroship_audit_mask_drift` if the stored `<col>_masked` doesn't
match. P6+ surfaces drift counts to the operator dashboard; until
then, query the table directly:

```sql
SELECT collection, column_name, row_pk, stored_masked, expected_masked
  FROM "<app_id>".__zeroship_audit_mask_drift
  ORDER BY created_at DESC
  LIMIT 100;
```

A persistent drift means a `.mask({ kind })` change landed without
the row being rewritten — usually that's a P5.5-PR-6b migration
that hasn't finished. The drift row carries enough context to
re-run the rewrite cron for the affected slice.

### Audit tables

| Table                              | Written by                          |
|------------------------------------|-------------------------------------|
| `__zeroship_audit_unmask`          | Every `.unmask()` call (granted or denied). |
| `__zeroship_audit_mask_drift`      | Drift detection cron, on mismatch.  |

Both tables live in the per-app schema; standard isolation rules
apply (`SELECT * FROM "<app>".__zeroship_audit_unmask`).

## Encrypted and Masked Fields (Shipped Reference)

This section resolves `docs/archive/sensitive-field-masking.md` against the shipped implementation in `sdks/db/src/types.ts`, `crates/zeroship-schema/src/query.rs`, `crates/zeroship-plugin-db/src/crud/mask_pass.rs`, `crates/zeroship-plugin-db/src/v8_classes/masked_value.rs`, `crates/zeroship-plugin-db/src/crud/unmask.rs`, `sdks/db/src/collection/masking.ts`, `sdks/db/src/policy.ts`, and `crates/zeroship-plugin-db/src/crud/mask_backfill.rs`.

Every masked field owns TWO physical columns. The field's OWN column holds the MASK, as bare `TEXT` carrying none of the declared constraints; a hidden sibling holds the REAL value, and carries the declared type and every constraint. Both are written atomically, and a default read serves the field's own column, so it serves the mask (`crates/zeroship-schema/src/query.rs`, `crates/zeroship-plugin-db/src/crud/mask_pass.rs`).

The layout used to be the other way round - plaintext under the field's name, the mask in a `<field>_masked` sibling that the SELECT aliased back. That made the SELECT the only mask-aware surface: the WHERE builder takes no schema and could not substitute, so `find({ ssn: { $gt: v } })` compared against plaintext and repeated probes binary-searched a value the caller could not read, unauthorized and unaudited. The flip makes the ignorant path the safe path - a builder that has never heard of masking names the column with the natural name, and that column is the mask. The sibling's name is `__zs_raw__<field>`, which `validate_field_name` refuses, so no filter, projection, sort, conflict probe or write-document key can name it either. `t.encrypted(...)` applies the fail-safe default mask at builder time, so an encrypted field without an explicit `.mask(...)` behaves as if it were declared with `.mask({ kind: "full", classification: "pii" })`; `.mask({ kind: "none" })` is the explicit opt-out that suppresses the sibling column and the masked read wrapper (`sdks/db/src/types.ts`, `crates/zeroship-schema/src/query.rs`).

On writes, `apply_mask_on_write` computes the mask from plaintext, not from a later read-path decrypt, and a separate relocation stage - the ONE stage that owns physical placement, running after the encryption and bytes passes - moves the finished value to the raw column and writes the mask into the field's own. Encrypted columns use the encryption pass sidechannel, plain masked columns read directly from `row[col]`, `null` and absent values relocate nothing and write no mask, and `kind: "none"` skips the field entirely (`crates/zeroship-plugin-db/src/crud/mask_pass.rs`). The shipped built-ins are `full`, `last4`, `first4`, `email`, `name`, `date-year`, `date-decade`, and `none` (`sdks/db/src/types.ts`, `crates/zeroship-plugin-db/src/crud/mask_pass.rs`).

Default reads surface `MaskedValue<T>`, not plaintext. The Rust read path wraps a masked cell in the `__zsmask__` sentinel shape, then the runtime rehydrates that sentinel into a native `MaskedValue` v8 class before user code sees the row (`crates/zeroship-plugin-db/src/crud/mask_pass.rs`, `crates/zeroship-plugin-db/src/v8_classes/masked_value.rs`, `sdks/db/src/types.ts`). The shipped surface is intentionally coercion-safe: `masked` and `classification` are readable, `_meta` carries `{ collection, row_pk, column }`, and `toString()` / `toJSON()` return the masked string (`crates/zeroship-plugin-db/src/v8_classes/masked_value.rs`, `sdks/db/src/types.ts`).

Plaintext reveal is always explicit. `await row.ssn.unmask({ actor?, reason? })` reveals one field on one row, and `await row.ssn.unmask(["ssn", "dob"], { actor, reason })` fans out across multiple columns on the same row (`crates/zeroship-plugin-db/src/v8_classes/masked_value.rs`, `sdks/db/src/types.ts`). `Collection.bulkUnmask()` is the shipped multi-row path; it maps `(id, columns)` pairs to the native collection op and is atomic, so one unauthorized `(row, column)` pair rejects the whole call (`sdks/db/src/collection/masking.ts`, `crates/zeroship-plugin-db/src/crud/unmask.rs`). The per-query hint `find(..., { unmask: [...], actor, reason })` promotes only the listed columns to plaintext while leaving other masked columns wrapped, and it writes audit only after a successful query (`crates/zeroship-plugin-db/src/crud/unmask.rs`, `sdks/db/src/types.ts`).

`defineMaskPolicy()` is the app-scoped authorization declaration for unmasking. It validates the six shipped classifications (`public`, `pii`, `spi`, `phi`, `pci`, `internal`), stores a single pending role-to-classification map for bootstrap to flush, and replaces rather than merges when called again in the same isolate (`sdks/db/src/policy.ts`). If an app never calls `defineMaskPolicy()`, the fallback is strict: only the `auto` actor can unmask. If the app does declare a policy, `auto` still keeps full access unless the policy explicitly lists `auto` with a narrower set (`sdks/db/src/policy.ts`).

Two sentinel formats are shipped. `__zsmask__` is the read-side wire sentinel for a masked value payload (`sdks/db/src/types.ts`, `crates/zeroship-plugin-db/src/crud/mask_pass.rs`, `crates/zeroship-plugin-db/src/v8_classes/masked_value.rs`). `__zsmask:kind=<kind>,classification=<class>` is the schema/introspection sentinel attached to the field's own (masked) column, so the diff and backfill paths can recover mask metadata from the live database definition (`crates/zeroship-schema/src/query.rs`, `crates/zeroship-plugin-db/src/crud/mask_backfill.rs`).
