# @zeroship/db — Database SDK

`@zeroship/db` is the database SDK for zeroship apps. You define your tables in
committed migration files under `migrations/`, and the build folds them into a
typed `env.db` whose generated module declares the `Env.db` augmentation.
Handlers call CRUD methods on `env.db.<name>` directly. There is no raw SQL
surface.

Every table a migration creates gets a fixed set of system columns, including
the `id` primary key. **You do not declare them** — see
[System columns](#system-columns).

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

// Typed by the generated env.db module.
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
first**, and the platform enforces it: a deploy whose generated schema descriptor
is not the one the app's newest applied migration produced is refused with
`409 schema_not_applied`, and nothing goes live.

```
$ zeroship deploy ./dist/app.zship --app=<id> --control=<url>

This app has committed migrations. Deploy does NOT apply them:
  zeroship migrate --app=<id> --control=<url>
Until you do, this deploy is REFUSED with 409 schema_not_applied.
```

`--app=<id>` names the app by its `app_...` identity and `--control=<url>` is
the control-plane origin. Both `deploy` and `migrate` accept both flags. With a
committed `zeroship.jsonc`, the first deploy writes the app id into `app` and
the file already carries `control`, so the flags can be omitted and `zeroship
migrate` alone is complete when run from the project directory. The 409 body
below prints its remedy as `--app` only because the file supplies the control
plane; pass `--control=<url>` when running from anywhere else.

The response body names the fix:

```json
{
  "error": "schema_not_applied",
  "deploy_descriptor_sha256": "a588c564…",
  "applied_descriptor_sha256": null,
  "remedy": "zeroship migrate --app=<id>"
}
```

There is no override. The check exists because the generated schema descriptor is
the only thing the platform consults about your schema — including which columns
are masked. If it could go live ahead of the schema it describes, a column the
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
  applied". Schema migrations have their reverse synthesized by the platform,
  while data migrations carry either a recorded `inverse()` or an explicit
  `irreversible` reason; deployment still requires the descriptor for the newest
  applied state.

### Schema and data migrations are separate

A migration module declares exactly one forward phase: `schema()` for DDL or
`data()` for DML — never both. There is no `up()` alias and no authored `down()`.

A **schema** migration needs nothing else; the platform derives its reverse from
the structure it applied:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  name: "add_notes",
  schema() {
    table("notes").create({
      columns: {
        title: t.text().notNull(),
        body: t.text(),
      },
    });
  },
};
```

A **data** migration must make its rollback posture explicit, with exactly one
of:

- `inverse()` — the reverse written in the same DSL, recorded alongside the
  forward change and checked like it; or
- `irreversible` — a non-empty string reason shown to whoever is deciding
  whether to roll back.

```ts
import { table } from "@zeroship/migrate";

export default {
  name: "normalize_order_status",
  data() {
    table("orders").update({
      set: { status: "pending" },
      where: (col) => col("status").eq("new"),
    });
  },
  inverse() {
    table("orders").update({
      set: { status: "new" },
      where: (col) => col("status").eq("pending"),
    });
  },
};
```

```ts
export default {
  name: "drop_legacy_notes",
  data() {
    table("legacy_notes").delete({ where: (col) => col("imported").eq(true) });
  },
  irreversible: "the original legacy rows are not recoverable",
};
```

The forward phase is the only required member. A migration may also set `name`;
if it omits one, the filename is used. `inverse()` and `irreversible` are
mutually exclusive, and either is mandatory on a `data()` migration.

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

The build tooling writes `generated/zeroship/env.db.ts` from your `migrations/`
files. `pnpm dev` regenerates it whenever a migration changes, and a
development-mode build (`vite build --mode development`) regenerates it on
demand. A production build does not rewrite it — it refuses to ship when the
committed module has drifted from the migrations, so commit each regeneration.

`@zeroship/types` declares the base `zeroship` runtime module. The
generated `env.db.ts` imports `@zeroship/db`'s `t`/`Db` types, reconstructs
the folded schema, and declares the single `Env.db` augmentation for the
app. This generated module is the only source of application database typing.

The root `@zeroship/db` package is the plain TypeScript SDK surface (`t`,
`schema`, `RowOf`, `Db`, etc.) for shared packages and tests. The generated
module is the sole app-level `Env.db` augmentation.

### Collection names

A collection name becomes a table name. Names may contain only ASCII
letters, digits and underscores, and must be at most **63 bytes** long. The cap
is enforced rather than left to the database, which would silently truncate a
longer name.

Three prefixes are reserved and cannot be used, case-insensitively:

| Reserved prefix | Owner |
| --- | --- |
| `pg_` | PostgreSQL system catalog |
| `sqlite_` | SQLite system catalog |
| `__zeroship` | zeroship platform |

Column names follow the same 63-byte limit and portable-identifier rule, plus
their own reservations:

- No name may start with `_` (reserved for synthetic result columns such as
  `_distance`).
- No name may end with `_masked` (reserved for the column a `.mask()` or
  `.encrypted()` field generates).
- The classification names `public`, `pii`, `spi`, `phi`, `pci` and `internal`
  are reserved exactly.
- A system column name (`id`, `created_at`, `updated_at`, `created_by`,
  `updated_by`, `version`, `deleted_at`) cannot be declared — the platform
  manages those. See [System columns](#system-columns).

**Type generation refuses invalid names too, so failures surface at build time.**
A masked field name must also leave room for the column the platform generates
for it. Invalid declarations leave the generated output unchanged.

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

Inside a transaction, `tx` mirrors `db` but every collection method returns the
bare value and throws on failure. Outside a transaction, each top-level
`db.<table>.*` call autocommits on its own.

## Schema builders

There are two `t.*` surfaces, and they are not interchangeable:

- **`@zeroship/migrate`** — what you write in `migrations/*.ts` to define
  tables. It is a DDL lexicon: `t.text()`, `t.string({ length })`,
  `t.int()`, `t.timestamp()`, `.notNull()`, `t.text().references(...)`,
  `ids.typeId(...)`.
- **`@zeroship/db`** — the runtime schema the generated `env.db` typing is
  built from, and the surface for shared packages and tests. It is a
  validation/typing lexicon: `t.string()`, `t.number()`, `.required()`,
  `t.ref("users")`, `t.array(...)`.

The rest of this section documents the migration lexicon first — that is what
you author — then the runtime lexicon.

### Migrations: the `t.*` DDL lexicon

These builders define a column. Every modifier returns a **fresh**
column definition, so a hoisted builder is safe to reuse.

| Builder | Storage | Notes |
| --- | --- | --- |
| `t.text()` | `TEXT` | Unbounded; indexable but not bounded. |
| `t.string({ length? })` | `VARCHAR(N)` | Bounded; `length` defaults to 255. |
| `t.boolean()` | `BOOLEAN` | |
| `t.int()` | `INTEGER` | 32-bit. |
| `t.bigInt()` | `BIGINT` | 64-bit. |
| `t.smallInt()` | `SMALLINT` | 16-bit. |
| `t.real()` | `REAL` | Single-precision float. |
| `t.double()` | `DOUBLE PRECISION` | Double-precision float. |
| `t.numeric({ precision?, scale? })` | `NUMERIC` | Defaults to (38, 9). |
| `t.timestamp()` | `TIMESTAMPTZ` | |
| `t.date()` | `DATE` | Calendar date. |
| `t.uuid()` | `UUID` | |
| `t.json()` | `JSONB` | |
| `t.bytes()` | `BYTEA` | |
| `t.textArray()` | `TEXT[]` on PG | JSON text on other backends. The descriptor type it emits is rejected by the runtime database validator, so use `t.json()` to store a list of strings. |
| `t.char({ length })` | `CHAR(N)` | Fixed-length. |
| `t.vector({ dimensions, metric? })` | `vector` (pgvector) | See [Vector search](#vector-search). |
| `t.geoPoint()` | `geography(POINT, 4326)` on PG | See [Geo](#geo-point--radius). |
| `t.encrypted({ of })` | ciphertext | Wraps another column type. |

Chainable modifiers: `.notNull()`, `.default(value)`, `.unique()`,
`.primaryKey()`, `.references(table, column, opts?)`, `.mask({ kind, ... })`,
`.generated(expr)`, `.identity()`, `.autoIncrement()`, `.collation(intent)`.

`.references(table, column, opts?)` declares a foreign key. `column` is the
target column and is required here; `opts` accepts `onDelete`, `onUpdate`,
`name` and `relation`:

```ts
userId: t.text().references("users", "id", { relation: "author" }),
```

### System columns

**Every table a migration creates receives seven system columns. Do not declare
them, and do not declare a primary key** — the platform injects both, and a
collision is refused at build time. The injected shape is:

| Column | Type | Assignment |
| --- | --- | --- |
| `id` | text | Generated on insert as a typed id, e.g. `note_01h...`. Sole primary key; immutable. |
| `created_at` | timestamp | Set on insert. |
| `updated_at` | timestamp | Set on insert, rewritten on every write. |
| `created_by` | text | Request actor on insert; null for anonymous writes. |
| `updated_by` | text | Request actor, rewritten on every write. |
| `version` | integer | Starts at 1; increments on every write. |
| `deleted_at` | timestamp | The soft-delete marker (see [Delete](#delete)). |

Two consequences:

- **Assigned fields are read-only.** They are excluded from typed write inputs
  and rejected if supplied. A created row's `id`, `created_at`, `created_by`
  and `version` are already correct without you passing them.
- **The typed-id prefix comes from the collection name.** A trailing `s` is
  dropped, then the first four ASCII alphanumerics are lowercased: `notes` →
  `note_`, `posts` → `post_`, `users` → `user_`. A name that yields nothing
  uses `row_`.

These behaviors are declared as descriptor metadata, not authored in the
migration. The generator vocabulary is `typedId`, `actor`, `now`,
`increment(N)` and `identity`, paired with an event (`insert`, `write` or
`delete`). There is **no migration builder for `assign` or `idPrefix`** — they
are managed by the platform, and a migration cannot override them. Every table
a migration creates is injected with one of each system assignment, so the
`now`-on-`delete` and increment-on-`write` generators the lifecycle options
select are always present.

If you build a manual runtime schema with `@zeroship/db` (for a shared package
or a test), that surface does expose them: `.assigned({ by, on })` attaches an
assignment and `t.id(prefix)` pins a typed-id prefix. See
[Runtime schema builders](#runtime-schema-builders).

### Migrations: refinements

Chainable on any migration column:

| Method | Effect |
| --- | --- |
| `.notNull()` | Column rejects null and becomes required in write inputs. |
| `.default(value \| fn)` | Value applied by the database when the column is omitted. |
| `.unique()` | Adds a `UNIQUE` index. |
| `.primaryKey()` | Marks a primary key. On the platform this is refused — the platform pins `id`. |
| `.references(table, column, opts?)` | Declares a foreign key. |
| `.identity()` / `.autoIncrement()` | Database-generated integer. |
| `.generated(expr)` | Generated/computed column. |
| `.mask(opts)` | Standalone read-time mask. |
| `.collation(intent)` | Pin byte-order comparison. |

### Runtime schema builders

`@zeroship/db`'s `schema()` and `t` describe the same fields at the runtime
type layer. You rarely write one by hand — the generated `env.db` module builds
it for you — but shared packages and tests use it directly. Its vocabulary and
modifiers differ from the migration lexicon:

| Builder | TS type | Notes |
| --- | --- | --- |
| `t.string()` | `string` | Text storage. |
| `t.integer()` | `number` | Integer. |
| `t.number()` | `number` | Floating point. |
| `t.numeric({ precision?, scale? })` | `Decimal` | Exact decimal, defaults to (38, 9). |
| `t.bigInt()` | `number \| bigint` | 64-bit. |
| `t.boolean()` | `boolean` | |
| `t.timestamp()` | `number` (Unix ms) | |
| `t.calendarDate()` | `string` (`YYYY-MM-DD`) | |
| `t.json()` | `JsonValue` | |
| `t.array(t.string())` | `string[]` | Primitive item types only. |
| `t.ref("users", opts?)` | `Id<"users">` (branded string) | Foreign key. |
| `t.object({ ... })` | nested inferred object | |
| `t.literal("login")` | `"login"` | Discriminator building block. |
| `t.union(v1, v2, ...)` | discriminated union | |
| `t.bytes()` | `Uint8Array` | |
| `t.vector(dims, opts?)` | `number[]` | |
| `t.geoPoint()` | `{ lat, lng }` | |
| `t.encrypted({ of })` | ciphertext | |
| `t.id(prefix?)` | `string` | Typed-id base, optionally pinning a prefix. |

Chainable modifiers: `.required()` (the runtime-side counterpart of
`.notNull()`), `.nullable()`, `.default(value)`, `.unique()`, `.index()`,
`.min(n)`, `.max(n)`, `.enum(...values)`, `.pattern(/regex/)`, `.primaryKey()`,
`.assigned({ by, on })`, `.references(table, opts?)`, `.mask(opts)`,
`.auto_now()`, `.auto_now_on_update()`.

A nullable field's write input is `T | undefined`, not `T | null`: passing
`null` on insert or update is a compile error. `null` is a filter shape
(`{ field: null }`), not a write value.

`t.ref(table, opts?)` is the runtime-side foreign-key builder. It takes the
target table first, then an options object with `column` (the target column,
required for a manual schema), `relation`, `onDelete`, `onUpdate` and
`deferrable`. Numeric references keep their storage type:
`t.integer().references("users", { column: "id", relation: "user" })`.

A manual runtime schema declares its own system columns, because it is not
built from a migration:

```ts
import { t, schema } from "@zeroship/db";

const users = schema({
  id: t.id("user").required().primaryKey().assigned({ by: "typedId", on: "insert" }),
  created_at: t.timestamp().required().assigned({ by: "now", on: "insert" }),
  updated_at: t.timestamp().required().assigned({ by: "now", on: "write" }),
  version: t.integer().required().default(1).assigned({ by: "increment(1)", on: "write" }),
  deleted_at: t.timestamp().assigned({ by: "now", on: "delete" }),
  email: t.string().required().unique(),
});
```

`t.id(prefix)` pins the typed-id prefix; without it the prefix is derived from
the collection name. A `data()` migration cannot declare any of this — see
[System columns](#system-columns).

`t.date()` is **not** in either surface. Use `t.timestamp()` for a
`TIMESTAMPTZ` (Unix-ms numbers at the JS layer) or `t.calendarDate()` for a
`DATE` (`YYYY-MM-DD` strings).

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

Binary fields accept and return `Uint8Array`. Text and ordinary arrays are
refused for binary columns.

```ts
const png = new Uint8Array(await file.arrayBuffer());
await db.uploads.insert({ blob: png });

const { data: rows } = await db.uploads.find({ id }).limit(1);
const back: Uint8Array | undefined = rows?.[0]?.blob;
```

`find` returns a chainable `Query`; awaiting it yields `{ data, error }`.
`limit`, `sort` and `skip` are chained on the query, not passed as options.

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

- `softDelete: true` selects the injected `now` generator on `delete` as the
  visibility marker. Deletes update the marker; reads hide marked rows. It
  works with no declaration: the platform injects the marker column and its
  `now` generator on every table.
- `versioning: true` selects the injected write increment generator for
  optimistic concurrency. A filter containing that field performs a revision
  check. It too works with no declaration, on the injected `version` column.
- `strictness: "strict" | "lenient" | "off"` is recorded on the collection.
  The default is `"strict"`; today the setting does not change write validation,
  which runs regardless.

The migration renderer rejects an enabled lifecycle option only when no
unambiguous matching generator exists. These options select roles; the injected
system columns determine the assignments, so `softDelete: true` and
`versioning: true` both work without any declaration.

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

An index name is namespaced to its collection. Field order matters — a
multi-column index covers any **leftmost prefix** of its fields, matching
B-tree semantics:

| Filter                       | Matches `by_user_done`? |
|------------------------------|-------------------------|
| `{ userId, done }`           | yes (full match)        |
| `{ userId }`                 | yes (prefix)            |
| `{ done }`                   | no (skips `userId`)     |

The runtime `schema(...)` builder has the same two forms:
`schema(...).index(name, fields)` and `schema(...).uniqueIndex(name, fields)`.
Use `uniqueIndex` for compound natural keys like `["orgId", "slug"]`.
`.index()` throws with `code = "SCHEMA_INVALID"` if `name` is empty or already
declared on the schema, or if `fields` is empty or names a field absent from
the schema.

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

Per-field `.unique()` / `.index()` still works for single-column cases — those
become one-column indexes and the warning recognises them.

**Two index limits today.** Changing the field list of a previously declared
index does not redefine it; drop the index and redeploy to change it. And a
`uniqueIndex` over a populated collection is checked when the index is built,
not at definition time — existing duplicates fail the migrate rather than the
schema.

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

A handler that takes a reference id as input must type it as the branded
`Id<"table">` (or cast a plain `string` to it); a bare `string` is a compile
error against the generated `env.db` types.

For a runtime schema, `t.ref("users", { column: "account_key", relation: "user" })`
declares the target column explicitly; an omitted target column uses `id`. The
relation name exposes the edge to `.with()`; an unnamed foreign key remains a
scalar reference. Numeric references retain their storage type: use
`t.integer().references("users", { column: "id", relation: "user" })` or
`t.bigInt().references(...)`. In a migration, the equivalent is
`t.text().references("users", "id", { relation: "user" })`. Wide integer values
keep the SDK's `number | bigint` contract.

By default, the builder emits a same-app FK without `ON DELETE`, `ON UPDATE`,
or `DEFERRABLE` clauses,
so the database's own defaults apply: `NO ACTION` for both actions, and
immediate (non-deferred) checking. On Postgres `NO ACTION` rejects a delete
that would orphan a row, the same as `RESTRICT`, but defers the check to the
end of the statement rather than firing per row.

Add `onDelete: "cascade"` to the reference options for physical cascade,
or `{ deferrable: true }` if you need cyclic refs insertable within one
transaction — that is opt-in, not the default. Cross-app targets are refused;
FKs stay inside the calling app.

### Can an FK point at another app's table?

No. A foreign key can only target a table declared in the same app. A
dot-qualified target (`t.ref("other.users")` or `.references("other.users", …)`)
and a constraint that names a table absent from your own schema are both refused
when the migration is applied. Model the relationship without a database-level
foreign key, or declare the referenced table in the same app.

Isolation between apps is enforced by the platform, not by a constraint you
declare.

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
const { data: user } = await db.users.get("user_01hxyz...");

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

`find(filter?, opts?)` takes an optional filter and an optional options object
`{ with?, actor?, unmask?, unmaskReason? }`. `sort`, `limit`, `skip`, `select`,
`with`, `after` and `paginate` are **chained** on the returned `Query`; `with`
also works as an option. `get(idOrFilter, opts?)` returns one row or `null` and
takes `{ select?, orderBy?, with?, actor?, unmask?, unmaskReason? }`. Both
return a `Result` — a plain `{ data, error }` envelope — at the top level, so
`rows` is never a bare array.

That is the SDK `db.<name>` surface. The lower-level
`env.db.collection(name).find(...)` shown under
[Explicit joins](#explicit-joins) is different: it resolves to a bare row array
and throws on failure.

Sorting is portable for text, ID/reference, integer, floating-point, and
temporal fields. Grouping and `distinct` also accept booleans and bytes.
Decimal and JSON storage do not have the same equality or ordering on
PostgreSQL and SQLite, so the database layer refuses to sort, group, or
deduplicate those fields. JSON filters still use structural equality on both
backends.

### Relations

Relations are named in the schema. A foreign-key field keeps its scalar value;
the named edge exposes the referenced row separately.

```ts
// Manual schema declaration; generated builders carry the same metadata.
const todos = {
  id: t.string().required().primaryKey(),
  userId: t.ref("users", { column: "id", relation: "user" }),
  title: t.string().required(),
};

const { data: rows } = await db.todos.find({}, { with: { user: true } });
// rows[0].userId: the original scalar foreign key
// rows[0].user: the protected users row, or null

const { data: todo } = await db.todos.get(id, { with: { user: true } });
const { data: page } = await db.todos
  .find({})
  .with({ user: true })
  .sort({ id: 1 })
  .paginate({ numItems: 20 });
```

Relations are loaded in bounded batches. A requested relation is resolved with
the target collection's own read protection and soft-delete rules applied, on
the same transaction connection as the query. The lower-level collection
handle resolves to a bare row array:

```js
const rows = await env.db.collection("todos").find({}, { with: { user: true } });
```

- Keys in `with` must be declared relation names. Foreign-key column names and
  query-defined aliases are not relation names.
- A relation name cannot collide with a column on its source collection.
- A null foreign key or missing/deleted target produces a null relation value.
- Requested relations remain in the result when `select()` narrows scalar
  fields, regardless of the order of `select()` and `with()`.
- Generated schemas infer each relation's target row type. A standalone model
  without the target schema map uses `PlainObject | null`.
- Forward relations support `true` only. Nested loading, reverse collections,
  target projections, and relation-level filters are not supported.

### Update

```ts
// By id, full row patch
const { data: u } = await db.users.update("user_01hxyz...", { name: "Alice Smith" });
if (!u) throw new Error("not found");

// By filter (returns the lowest-id match, or null)
const { data: u } = await db.users.update({ email: "alice@..." }, { role: "admin" });

// Atomic operators — per-field
await db.products.update("prod_01hxyz...", {
  stock: { $dec: 1 },
  views: { $inc: 1 },
  tags:  { $push: "sale" },
});

// MongoDB top-level shape (SDK translates)
await db.products.update("prod_01hxyz...", { $inc: { views: 1 } });

// Update many — returns counts
const { data: counts } = await db.users.updateMany(
  { role: "user" },
  { role: "member" },
);
// counts = { matchedCount: N, modifiedCount: N }

// CAS via the platform version field
const { data, error } = await db.products.update(
  { id: "prod_01hxyz...", version: 5 },
  { stock: { $dec: 1 } },
);
// error.code === "OPTIMISTIC_CONCURRENCY" when stored version != 5
```

Filtered single-row updates, deletes, restores, and purges choose the matching
row with the lowest `id`.

Bulk update, delete, restore, and purge operations affect all matching rows and
report affected-row counts without fetching the changed records. They remain
atomic; large maintenance jobs should choose explicit bounded batches. An
update that encrypts values per row rejects an oversized target set before
writing.

#### Retrying CAS updates with `withRetry`

The OCC pattern (read → compute → update with `{ version }` → retry on
`OPTIMISTIC_CONCURRENCY`) is wrapped by `withRetry`:

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

Defaults: `max: 3`, retries only the `OPTIMISTIC_CONCURRENCY` code, no backoff.
`isOptimisticLockError(e)` matches on that code, so it is true for the
`OptimisticLockError` class and for a plain `Error` carrying the same code.
Pass your own predicate to retry on additional coded errors:

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
const { data } = await db.users.delete("user_01hxyz...");

// By filter
const { data } = await db.users.delete({ email: "spam@..." });

// Many — returns counts
const { data } = await db.sessions.deleteMany({ expiresAt: { $lt: Date.now() } });
// { deletedCount: N }

// Soft delete: delete sets deleted_at instead of removing the row.
// For an explicit hard-delete, use `purge` / `purgeMany`:
await db.users.purge("user_01hxyz...");
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
relation-loading API. Column names and types come from your schema.

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

Two search modalities ride on top of the schema DSL. Each has the same shape:
declare the column with a `t.*` builder, apply a migration to build the index,
and query via `Collection.search` or `Collection.near`. Cross-backend results
have identical membership, but distances can differ in the low significand
bits — assert set membership rather than strict ordinal positions.

### Backend extension dependency

| Capability | PostgreSQL dependency | SQLite dependency |
|------------|-----------------------|-------------------|
| Vector     | `pgvector` extension  | none — bundled    |
| Geo (point + radius) | `postgis` extension | none — bundled |

On PostgreSQL both extensions must be installed. The fastest path is to run
the database on the `pgvector/pgvector:pg16` image, which ships `pgvector` and
`postgis` out of the box.

On SQLite (dev/sandbox/test only) both search paths work without an extension;
they suit local development rather than production scale.

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

`_distance` is a synthetic column the row carries back from the scan. Filtering
happens before ranking and limiting, so matching rows fill the requested result
window.

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

`_distance_m` is the great-circle distance in metres. `point: { lat, lng }` is
always the call-site contract; the platform handles each database's own
coordinate order.

Use PostgreSQL for any production-scale geo workload; the SQLite path is a full
scan intended for development.

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

- **PostgreSQL vector** — an approximate-nearest-neighbour index over the
  declared metric. Production-grade; scales to millions of rows.
- **SQLite vector** — exact search over the column, no index. Cosine and L2
  are supported; inner product returns `VECTOR_UNSUPPORTED_METRIC`. This path
  suits local development; production vector workloads use PostgreSQL.
- **PostgreSQL geo** — a PostGIS geography column with a spatial index and
  spheroid distance.
- **SQLite geo** — a full scan. Dev-tier; for production geo workloads use
  PostgreSQL.

### Error codes

`Collection.search` and `Collection.near` add the following codes on
top of the global error rail (§ Errors):

| `error.code`                       | When                                                                 |
|------------------------------------|----------------------------------------------------------------------|
| `INVALID_K`                        | `k` outside `1..=1000`. Client-side validation; the call is never sent. |
| `VECTOR_EXTENSION_MISSING`         | PostgreSQL without `pgvector`. Hint names `CREATE EXTENSION vector;` and the image swap. |
| `POSTGIS_EXTENSION_MISSING`        | PostgreSQL without `postgis`. Hint names `CREATE EXTENSION postgis;` and the image swap. |
| `VECTOR_DIMENSION_MISMATCH`        | `args.vector.length !== <declared dims>` at insert or query time.    |
| `VECTOR_UNSUPPORTED_METRIC`        | Inner-product search on SQLite.                                      |

Branch on `error.code`, never substring-match on `error.message`.

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
`$addToSet`. A bare value is treated as `$set`. The MongoDB top-level shape
(`{ $set: { ... } }`, `{ $inc: { ... } }`, etc.) is also accepted.

Each field may be assigned only once per update. Multiple operators on a field,
mixed operator/data objects, and assignments that collide after column-name
mapping are refused. Arithmetic operators require numbers or `Decimal` values;
strings, booleans, and null are refused before SQL execution. Objects supplied
through an explicit `$set` are literal data, even if their keys look like update
operators.

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
- The transaction is rolled back automatically when the callback throws;
  resolving commits. There is no `tx.commit()`.
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
  that transaction no matter where the callback's own async work resumes.
- On `pnpm dev`, SQLite allows only one transaction per process, shared across
  every app, so a second `db.transaction()` is refused immediately with
  `TRANSACTION_CONNECTION_BUSY` rather than waiting. The deployed tier allows
  concurrent transactions per app. Do not rely on `pnpm dev` to tell you whether
  concurrent transactional work is correct.
- PostgreSQL `isolationLevel` accepts `"read uncommitted"`, `"read committed"`,
  `"repeatable read"` or `"serializable"`. Omitting it uses the database default.
  SQLite accepts `"serializable"` or the default and rejects other levels with
  `unsupported_isolation_level`. A nested transaction inherits its parent's
  isolation; supplying a level for a savepoint returns `nested_isolation_level`.
- Outside the callback the result is again a `Result<R>` — the surrounding
  `transaction()` call doesn't throw.

The procedure wrappers `query()`, `mutation()`, and `action()` from
`@zeroship/rpc/server` do not open a transaction implicitly. Top-level
`db.<table>.*` calls autocommit per operation. Use explicit
`db.transaction()` when a handler needs multiple database operations to
commit or roll back as a unit.

## Live queries (`db.live`)

`db.live(queryFn)` is the query-shaped reactive API. The package also exports
the lower-level `subscribe(collection)` primitive for framework adapters and
advanced consumers. App code that wants refreshed query results should use
`db.live(...)`; `subscribe(...)` reports invalidations and leaves the re-read to
the caller.

```ts
import { subscribe } from "@zeroship/db";

const changes = subscribe("todos");
await changes.ready();
for await (const event of changes) {
  if (event.kind === "change") {
    console.log(event.op, event.pk, event.columns);
  }
}
```

The returned `Subscription` is an async iterable with idempotent `ready()` and
`close()` methods. It emits `change`, `resync`, and `closed` events. Breaking
out of the loop closes the subscription.

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

**Coarse-grained.** Subscriptions are per-table, not per-row: every
insert/update/delete on a watched table fires a rerun, even when the
row doesn't match the queryFn's filter. The auto-detected table set is
the union of all collections the `queryFn` touched during its first
execution. There is no per-document (read-set) tracking.

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

On PostgreSQL, live queries receive committed invalidations from a separately
deployed change-data-capture service. Row values stay out of the transport;
live queries re-read through the usual access controls.

The first subscriber connects lazily. `Subscription.ready()` resolves once
capture for the app is established; `db.live` waits before taking its initial
snapshot. Missing configuration or a failed startup rejects the live query.
Connection loss, queue overflow and reconnect all request a fresh snapshot.

Closing the final subscription releases the app's capture. Apps without live
queries retain none. File-backed SQLite captures commits locally without the
service.

Archiving an app keeps its existing live subscriptions; gateway routing and new
workflow admission stop.

## Errors

Every database error carries a `.code` string. **Branch on `error.code`** — it
is the one stable field. Compare it case-insensitively: the spelling is not
uniform (see [Two spellings of the same
code](#two-spellings-of-the-same-code)). Do not branch on `error.name`, and
do not substring-match `error.message`.

A few codes also have an exported class (`OptimisticLockError`,
`ValidationError`, `NotFoundError`, `NotUniqueError`). The class is a
convenience; the code is the contract, and `isOptimisticLockError(e)` matches on
`OPTIMISTIC_CONCURRENCY` rather than on the class.

### Canonical codes

| `error.code` | When |
| --- | --- |
| `VALIDATION` | Input fails schema validation. |
| `UNIQUE_VIOLATION` | Duplicate unique key. |
| `FOREIGN_KEY_VIOLATION` | Foreign-key constraint violated. |
| `NOT_NULL_VIOLATION` | A required column was null. |
| `CHECK_VIOLATION` | A check constraint failed. |
| `SERIALIZATION_FAILURE` | The transaction was aborted; retry. |
| `LOCK_NOT_AVAILABLE` | Lock contention; retry after a short backoff. |
| `ROW_DECODE_FAILED` | A result column holds malformed binary data, an unsupported physical type, or a non-finite value. SQL NULL is still valid. |
| `OPTIMISTIC_CONCURRENCY` | `update` with a CAS version that didn't match. |
| `NOT_FOUND` | A unique-row lookup matched no row. |
| `NOT_UNIQUE` | A unique-row lookup matched more than one row. |
| `SCHEMA_NOT_PROVISIONED` | The app's database was never provisioned. Run `zeroship migrate`. An app that carries a descriptor is refused earlier at deploy with `409 schema_not_applied` (see [Migrate before you deploy](#migrate-before-you-deploy)). |
| `GRANT_REVOKED` | The database refused the app's role; a terminal HTTP 403. Restore the grant before retrying. |
| `LIVE_IN_TRANSACTION` | `db.live()` was called inside a transaction. |
| `SCHEMA_INVALID` | A schema declaration is malformed (empty or duplicate index name, unknown field). |
| `MASK_POLICY_IMMUTABLE` | `defineMaskPolicy()` was called after startup completed. |
| `INVALID_MASK_CLASSIFICATION` | `defineMaskPolicy()` names a classification that is not one of the six. |
| `INVALID_MASK_POLICY_SHAPE` | The `defineMaskPolicy()` declaration is malformed. |
| `DATABASE_STARTUP_PENDING` | An unmask ran before startup finished (platform spelling `database_startup_pending`). |
| `TRANSACTION_SCOPE_EXPIRED` | A database call outlived its transaction (see [Transactions](#transactions)). |
| `TRANSACTION_CONNECTION_BUSY` | Two transaction operations overlapped on one connection, or a second transaction was started where only one is allowed. |
| `INVALID_K`, `VECTOR_EXTENSION_MISSING`, `POSTGIS_EXTENSION_MISSING`, `VECTOR_DIMENSION_MISMATCH`, `VECTOR_UNSUPPORTED_METRIC` | Vector / geo paths — see [Vector / Geo: Error codes](#error-codes). |

A platform release can add codes; branch on the code you expect, compared
case-insensitively, rather than enumerating defensively. Schema-*definition*
diagnostics (as opposed to
operation failures) surface at build time rather than as a runtime `error.code`.

### Two spellings of the same code

The spelling depends on where the error was raised. This is a real trap:

| Raised by | Spelling | Examples |
| --- | --- | --- |
| a collection operation — `insert`, `find`, `update`, and the same calls inside a transaction | SCREAMING_SNAKE | `UNIQUE_VIOLATION`, `FOREIGN_KEY_VIOLATION`, `TRANSACTION_CONNECTION_BUSY`, `SERIALIZATION_FAILURE` |
| the transaction's own lifecycle — begin, isolation level, savepoint/nesting | raw lowercase | `unsupported_isolation_level`, `nested_isolation_level`, `transaction_lanes_exhausted`, `transaction_connection_busy`, `transaction_scope_expired` |

When an error from the platform layer has a lowercase code, the SDK canonicalizes
it to SCREAMING_SNAKE. Two codes are remapped by name rather than mechanically
uppercased:

| Platform spelling | `error.code` |
| --- | --- |
| `fk_violation` | `FOREIGN_KEY_VIOLATION` |
| `concurrency_mismatch` | `OPTIMISTIC_CONCURRENCY` |

Every other code uppercases as-is (`unique_violation` → `UNIQUE_VIOLATION`). A
transaction-lifecycle error is passed through as the platform raised it, so it
keeps the platform's lowercase spelling. That is why one rule has to cover both
paths: compare `error.code` case-insensitively instead of matching a single
spelling.

```ts
const { data, error } = await db.users.update({ id, version: 5 }, { name });
if (error?.code === "OPTIMISTIC_CONCURRENCY") {
  // refetch and retry
}
```

### What survives the network

An error carries `code`, plus `hint` where the platform has a remedy to suggest
and `status` where the code has an HTTP status. A thrown error may be one of the
SDK classes (`ValidationError`, `OptimisticLockError`, `NotFoundError`,
`NotUniqueError`) or a plain `Error` with the same `code` — never depend on the
class identity.

**Nothing above reaches your end user by accident.** If a procedure lets one of
these errors escape, the platform drops `hint` and `status` and replaces an
unpublished code with `{"message":"internal error"}`. Catch it and return
something you chose:

```ts
export const rename = mutation(async ({ id, name }) => {
  const { data, error } = await db.users.update({ id }, { name });
  if (error) return { ok: false, code: error.code };
  return { ok: true, user: data };
}, { id: "users.rename" });
```

## Advanced `env.db` surface

App code rarely needs these; SDK packages use them directly.

**Creator-reachable on `env.db`:**

- `env.db.collection(name)` → the lower-level per-collection handle. Its methods
  resolve to raw values and throw on failure, unlike `db.<name>`.
- `env.db.transaction(fn, opts?)` → begins a transaction; the callback's
  resolution commits and its throw rolls back (see
  [Transactions](#transactions)).
- `env.db.from(...)` → a structured join query (see
  [Explicit joins](#explicit-joins)).

**Mask policy at startup.** `defineMaskPolicy()` submits its declaration while
the app's entry module is being evaluated, and it is fixed for that deployment
once startup completes. It cannot be reset or replaced afterwards: a late call
fails with `MASK_POLICY_IMMUTABLE`, and an unmask issued before startup finishes
fails with `database_startup_pending`. See
[`defineMaskPolicy()`](#definemaskpolicy).

## Per-app isolation

Every app has its own database schema, addressed by the platform on its behalf.
The app's identity is fixed by the platform and cannot be read or overridden by
user code. `env.db` is frozen.

## Optimistic concurrency and soft delete

The injected `version` column is the revision marker. Passing it in an update
filter performs a compare-and-swap: if the stored value differs, the update
fails with `OPTIMISTIC_CONCURRENCY`, and you should re-read and retry (see
[Retrying CAS updates](#retrying-cas-updates-with-withretry)). Omitting the
predicate permits a blind update.

```ts
const { data: post } = await db.posts.insert({ title: "First post" });
if (!post) throw new Error("insert failed");
await db.posts.update(
  { id: post.id, version: post.version },
  { title: "Edited" },
);
```

The injected `deleted_at` column is the soft-delete marker. `delete` physically
removes a row unless the collection declares the soft-delete role, in which case
it stamps `deleted_at` and reads hide the row. `restore` clears the marker;
`purge` always removes the row. Assigned columns are read-only — they are
excluded from write inputs, and `id` never changes.

## Masking

**Encryption hides at rest. Masking hides at read time.** They are
sibling concerns and compose: an `t.encrypted(...)` column without
an explicit `.mask(...)` declaration is treated as `.mask({ kind:
"full", classification: "pii" })` by default.

### Mental model

| Layer       | What it does                                  | When it runs       |
|-------------|-----------------------------------------------|--------------------|
| Encryption  | Replaces stored bytes with ciphertext (AEAD)  | Insert / update    |
| Masking     | Returns a `MaskedValue<T>` wrapper on reads   | Find / get / live  |
| Unmask      | Trades the wrapper for plaintext (audited)    | Explicit call only |

A default read of a masked column **never** decrypts: it returns the stored
mask, and the real value does not leave the database. Revealing plaintext is
always an explicit, audited operation (see [Unmasking](#unmasking)).

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
storage-side; they're independent. `.mask({ kind: "none" })` opts into
plaintext reads.

**What encryption changes.** Encryption is randomised: every write uses a fresh
nonce. Encrypted fields cannot be filtered (equality and `IN` included), sorted,
grouped, or declared unique. Masking is a separate read policy, so disabling the
mask does not make an encrypted field queryable. To find a row, filter on an
ordinary field and read the encrypted value from the returned row.

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

`MaskedValue` also exposes `masked` (the display string) and `classification`
(one of `public`, `pii`, `spi`, `phi`, `pci`, `internal`), plus `_meta` with
`{ collection, row_pk, column }`.

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
    { id: "user_01hxyz...", columns: ["ssn"] },
    { id: "user_01hxza...", columns: ["ssn", "email"] },
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

App-wide policy mapping `actor` → permitted classifications. Declare it at the
app's entry module so it is in force before any handler runs.

```ts
import { defineMaskPolicy } from "@zeroship/db";

defineMaskPolicy({
  admin: ["public", "pii", "spi", "phi", "pci", "internal"],
  support: ["public", "pii"],
  end_user: ["public"],
  // `auto` (the system actor) has uniform access UNLESS listed.
});
```

The policy is per app and per deployment; a later deployment can declare a
different one without affecting an older deployment. It is held in memory and is
not persisted, so changing it requires a redeploy.

**Fallback.** If an app never calls `defineMaskPolicy()`, only the system actor
`auto` can unmask. If it does declare a policy, `auto` keeps full access unless
the policy lists `auto` with a narrower set.

### Audit tables

| Table                              | Written by                          |
|------------------------------------|-------------------------------------|
| `__zeroship_audit_unmask`          | Every `.unmask()` call (granted or denied). |

This table lives in the per-app schema; standard isolation rules apply.

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
declared collection and has ordinary data privileges. Its name does
not make it hidden or append-only.
