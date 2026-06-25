# `@zeroship/migrate` — the op DSL

`@zeroship/migrate` is the no-raw-SQL, fully-structured authoring surface for
zeroship database migrations. A migration is a `.ts` module that imports the
op-functions it needs and exports a single `default { name?, up, down? }`
object. You describe schema changes (DDL) and data migrations (DML) once; the
engine lowers them per-dialect and applies them faithfully to **both Postgres
and SQLite** from one script.

There is **no raw SQL** anywhere on this surface — no `Raw` type, no `sql\`\``
escape, no string fragments. Every transform and predicate is a fluent
`(c) => Expr` callback over a closed expression AST, and the engine owns 100%
of per-dialect rendering. This is a deliberate boundary (property A): a
transform the closed surface cannot express is a hard, structured error, not a
back door to hand-written SQL.

The TypeScript authoring surface lives in `sdks/migrate/src/` (the npm
`@zeroship/migrate` package). Its engine-side twin — the recorder the Rust
runtime evaluates in V8 to turn a migration into the frozen `.ir.json` wire
artifact — lives in `crates/zeroship-migrate-js/src/migrate_ops.js`. Both emit
the identical dialect-neutral op objects; the `.ir.json` shape is the frozen
contract.

```ts
// migrations/0007_split_name.ts
import { addColumn, dropColumn, backfill, t } from "@zeroship/migrate";

export default {
  name: "split_name_column", // optional; defaults to the filename label

  up() {
    addColumn("users", "first_name", t.text()); // nullable by default
    addColumn("users", "last_name", t.text());
    backfill("users", {
      set: {
        first_name: (c) => c.fn.splitPart(c("name"), " ", 1),
        last_name: (c) => c.fn.splitPart(c("name"), " ", 2),
      },
      where: (c) => c("first_name").isNull(),
    });
    dropColumn("users", "name");
  },

  down() {
    addColumn("users", "name", t.text());
    backfill("users", {
      // c.fn.concatWs is NULL-skipping — the safe join; copy this, not `.concat`
      set: { name: (c) => c.fn.concatWs(" ", c("first_name"), c("last_name")) },
    });
    dropColumn("users", "first_name");
    dropColumn("users", "last_name");
  },
};
```

## Module shape

A migration module is a single default-exported object:

```ts
export interface Migration {
  name?: string; // optional; defaults to the filename label (e.g. "split_name")
  up(): void; // required
  down?(): void; // optional; only present when the migration is rollbackable
}
```

- `up()` is **required**; `down()` is optional.
- `up()`/`down()` are **parameterless and return `void`**. They do not execute
  SQL — the imported op-functions *record* a plain-data op onto an ambient
  per-migration recorder, synchronously (no `await`). This is the
  vitest/jest/Playwright pattern: `import { test }` then call it. The
  build/dev evaluator installs a fresh recorder before calling `up()` (and
  again before `down()`), drains the recorded op list, and renders it to the
  checksummed `.ir.json` artifact.
- Calling an op-function **outside an active recorder** — at module top level,
  or after `up()` returns (e.g. from a stray `setTimeout`) — throws a
  structured `OP_OUTSIDE_RECORDER` error. The op cannot be silently lost.

The shape mirrors the platform's `export default { schema, fetch, rpc }` deploy
idiom (see [zeroship-standard](./zeroship-standard.md)); one typed object, never
a loose top-level `export function up()` plus a stray `export const name`.

### `down()` is not auto-derived for DML or lossy DDL

The engine auto-derives a reverse for *reversible* DDL (an `addColumn`'s inverse
is a `dropColumn`, etc.). A migration is auto-reversible only if **every** op is
auto-reversible. A `backfill`/`update`/`del` (DML — no general inverse) or a
`dropColumn` (data-destroying) yields no auto-inverse, so a migration containing
one is `down: None` (irreversible) unless you hand-write `down()`. The hero
example above hand-writes `down()` for exactly this reason: it contains a
`backfill` and a `dropColumn`. The DSL never silently fabricates an inverse for
DML or lossy DDL — an author-supplied `down()` is itself a structured migration
(its own op calls), never a raw-SQL string.

## Named imports, no prefix

Every op is a **top-level named export** of `@zeroship/migrate` — there is no
`op.` object and no prefix. You import exactly the ops you use:

```ts
import { createTable, addColumn, backfill, t } from "@zeroship/migrate";
```

The complete exported vocabulary (`sdks/migrate/src/index.ts:19-49`):

| Category | Exports |
| --- | --- |
| Tables | `createTable`, `dropTable` |
| Columns | `addColumn`, `dropColumn`, `renameColumn`, `alterColumn` |
| Constraints / indexes | `addForeignKey`, `addUnique`, `addCheck`, `dropConstraint`, `createIndex`, `dropIndex` |
| DML | `insert`, `update`, `del` (not `delete` — JS reserved word), `backfill` |
| SQLite-safe rebuild | `batchAlterTable` |
| Column-type lexicon | `t` |
| `@zeroship/db` bridge | `fromDb` |
| Determinism lint | `lintDeterminism` |

There is **no importable `fn`**: the scalar-function namespace is reached
through the single expression-builder handle as `c.fn.*` (see
[The fluent expression surface](#the-fluent-expression-surface)), so a migration
never has an imported-but-unused symbol.

## Names are strings (and why)

**Every table/column/name reference is a plain `string`.** There is no generic
`S extends Schema` parameter, no `TableName<S>` / `keyof RowOf<S,T>` binding to
the live `@zeroship/db` schema. `table`, `column`, `from`, `to`, `name`,
`cursorColumn`, every `set` key, every `where`-referenced column, and every
`c("…")` argument are strings whose existence is validated at **apply time
against the real DB**, never at `tsc` time (the typing-stance prose lives in the
module header, `sdks/migrate/src/types.ts:1-11`, "§3.3 — names are plain
`string`, NOT live-schema-bound", and on the `Row`/`ScalarValue` types,
`sdks/migrate/src/types.ts:91-93`).

This is deliberate, and it is the single most important typing rule of the DSL.

**Why binding names to the live schema is wrong (the rot bug).** Migration files
are immutable historical artifacts. If `addColumn`/`update` typed their names
against the *current* schema, a migration that referenced `users.lastSeen` would
**stop compiling** after a *later* migration dropped that column — the whole
history would become un-compilable as the schema evolves, and authors would be
tempted to edit committed migrations to make them compile, changing the op list,
changing the plan checksum, and triggering a drift abort. "A migration
referencing a column that does not exist fails `tsc`" is an anti-feature: it
confuses the schema *at authoring time* with the schema *as it evolves*, and
punishes the correct behavior (never editing applied migrations).

No mature migration tool binds migration files to the live schema:

- **Kysely** deliberately uses `Kysely<any>` inside migrations — its docs state
  migrations should not be typed against the current schema, precisely because
  the schema changes over time.
- **Alembic** uses string names: `op.add_column("users", …)`.
- **Drizzle** migrations are generated SQL, not type-checked against the live
  schema.

**What IS type-checked (structural safety, preserved):**

- **Op-function argument shapes** — you cannot pass a number where a `ColumnDef`
  is expected, or omit a required argument.
- **The `t` column-type lexicon** — `t.text()` / `t.numeric()` and their
  chainable modifiers (`.notNull()` / `.default()` / `.ref()`) are typed.
- **The fluent-expression node shapes** — `c`'s methods (`.eq` / `.concat` /
  `.gt` / `c.fn.splitPart` …) have typed arities and return an `Expr`; calling a
  non-existent operator method fails `tsc`. (Method *names* are the typed builder
  API; that is not the forbidden string-vs-typed mix, because every *identifier*
  `c` references is still a plain string.)
- **Insert-row VALUE shapes** — rows are a loose `Record<string, ScalarValue>`
  by default; a caller may supply a generic `insert<R>(…)` for editor
  convenience, but `R` is never auto-derived from the live schema.

So the guarantee: op shapes, the `t` lexicon, expression node shapes, and value
kinds are typed; **names are validated at apply/render time, never `tsc`-bound to
the live schema.** The part most likely to be semantically wrong (a transform's
referenced names) is exactly what the type system cannot see — you test it with
the shadow-DB dry-run.

## The column-type lexicon (`t.*`)

Every column-type position (`createTable`, `addColumn`, `renameColumn`,
`alterColumn`) takes a chainable `ColumnDef` produced by the fluent `t.*`
lexicon. **Columns are nullable by default**; `.notNull()` is the rarer, riskier
opt-in.

The shipped factories (`sdks/migrate/src/ops.ts:197-236`):

| Factory | Column type |
| --- | --- |
| `t.id()` | a non-null `uuid` PK defaulting to `gen_random_uuid()` |
| `t.text(opts?)` | text |
| `t.string(opts?)` | string |
| `t.int(opts?)` / `t.integer(opts?)` | 32-bit integer |
| `t.bigInt(opts?)` | 64-bit integer |
| `t.float(opts?)` | floating point |
| `t.numeric(precision?, scale?, opts?)` | fixed-precision decimal (default `(38, 9)`) |
| `t.boolean(opts?)` | boolean |
| `t.timestamp(opts?)` | timestamp |
| `t.uuid(opts?)` | uuid |
| `t.bytes(opts?)` | byte array |
| `t.json(opts?)` | json |
| `t.vector(n, opts?)` | a pgvector column of dimensionality `n` |
| `t.geoPoint(opts?)` | a geo point |
| `t.ref(targetTable, opts?)` | a foreign-key reference (plain-string target) |
| `t.encrypted({ of }, opts?)` | an application-level encrypted column wrapping an inner type |

Chainable modifiers (`sdks/migrate/src/ops.ts:101-149`):

| Modifier | Effect |
| --- | --- |
| `.notNull()` | mark `NOT NULL` |
| `.default(value)` | a typed scalar literal **or** a nullary synth scalar `{ fn: "now" \| "genRandomUuid" }` — never raw SQL |
| `.primaryKey()` | mark the table primary key (implies `NOT NULL`) |
| `.unique()` | add a single-column `UNIQUE` |
| `.ref(targetTable)` | re-target the column as a foreign-key reference (plain-string target) |

Each factory also takes an options-bag overload, so the modifiers above are
equally expressible inline:

```ts
import { createTable, t } from "@zeroship/migrate";

export default {
  up() {
    createTable("orders", {
      id: t.id(),
      total: t.numeric(12, 2).notNull().default(0),
      status: t.text({ notNull: true, default: "pending" }),
      customer_id: t.ref("customers", { notNull: true }),
    });
  },
};
```

`t.ref(target)` carries the target table as a plain string — it is never bound
to the live schema (existence is validated at apply time).

## Bridging a `@zeroship/db` field (`fromDb`)

The migration DSL and the runtime `@zeroship/db` schema share **one** type
lexicon. `fromDb(field)` lifts a live-schema `@zeroship/db` `t.*` field into a
migration `ColumnDef` through the identical `ColType` path
(`sdks/migrate/src/ops.ts:257-267`), so a `t.ref("users")` declared in your app
schema lowers to the byte-identical neutral type a hand-written migration column
produces. It carries the field's nullability (`.required()` → `.notNull()`) and
uniqueness, and returns a chainable `ColumnDef` so you can still layer migration
modifiers on top. Names are never bound — a bridged `ref` keeps its target as a
plain string. A non-storage `@zeroship/db` field (a json `array`, a nested
`object`, a `union`) has no portable column type and throws
`UnsupportedColTypeError` (`sdks/migrate/src/db-lexicon.ts:51-61`) — a hard
boundary, never a silent fallback.

## The op surface

Below is the full shipped op surface. Every signature is the one exported by
`sdks/migrate/src/ops.ts`.

### DDL — tables

```ts
createTable(name, columns, build?, { schema?, ifNotExists? }?); // ops.ts:387
dropTable(table, { schema?, ifExists?, cascade? }?); // ops.ts:443
```

Every table-targeting op also accepts an optional `schema` qualifier and (where
applicable) an existence guard — see "The `schema` qualifier" and "Existence
guards" below.

`createTable` takes a column map and an optional `(b) => void` scoped builder
for table-level constraints and indexes:

```ts
createTable(
  "members",
  {
    id: t.id(),
    org_id: t.ref("orgs").notNull(),
    email: t.text().notNull(),
    role: t.text().notNull().default("member"),
  },
  (b) => {
    b.unique(["org_id", "email"], { name: "members_org_email_uq" });
    b.index(["org_id"]);
    b.check((c) => c("role").ne(""), { name: "members_role_nonempty" });
    b.foreignKey({
      columns: ["org_id"],
      references: { table: "orgs", columns: ["id"] },
      onDelete: "cascade",
    });
  },
);
```

The scoped builder methods (`sdks/migrate/src/types.ts:251-257`):
`b.index(columns, opts?)`, `b.unique(columns, opts?)`, `b.primaryKey(columns)`,
`b.check(expr, opts?)`, `b.foreignKey(spec)`.

### DDL — columns

```ts
addColumn(table, name, type); // ops.ts:448 — type is a t.* ColumnDef
dropColumn(table, column, { ifExists? }?); // ops.ts:457
renameColumn(table, from, to, type); // ops.ts:463 — see "Online rename" below
alterColumn(table, name, change); // ops.ts:470
```

`alterColumn`'s `change` carries either a new `type` (with an optional `using`
transform) **or** a `nullable` flip (`sdks/migrate/src/types.ts:201-205`):

```ts
alterColumn("orders", "total", {
  type: t.numeric(14, 2),
  using: (c) => c("total").cast("real"),
});
alterColumn("orders", "note", { nullable: true });
```

### DDL — constraints and indexes

```ts
addForeignKey(table, spec); // ops.ts:510
addUnique(table, spec); // ops.ts:515
addCheck(table, spec); // ops.ts:523 — spec.expr is a (c) => Expr
dropConstraint(table, specOrName); // ops.ts:531
createIndex(table, spec); // ops.ts:538
dropIndex(name, { table?, unique?, ifExists?, concurrently? }?); // ops.ts:557
```

```ts
addForeignKey("members", {
  columns: ["org_id"],
  references: { table: "orgs", columns: ["id"] },
  onDelete: "cascade",
});
addUnique("members", { columns: ["org_id", "email"], name: "members_org_email_uq" });
addCheck("orders", { expr: (c) => c("total").ge(0), name: "orders_total_nonneg" });
createIndex("members", { columns: ["email"], unique: true, using: "btree" });
dropIndex("members_email_idx", { table: "members", unique: true });
dropConstraint("orders", "orders_total_nonneg");
```

A constraint adder is always `(table, spec)` with `name` inside the spec and
`columns` / `references.columns` as named fields — never a transposable trailing
positional. The index method set is `btree | gin | gist | ivfflat | hnsw | fts5`
(`sdks/migrate/src/types.ts:190`). Foreign-key actions are
`cascade | restrict | setNull | setDefault | noAction`
(`sdks/migrate/src/types.ts:164`).

### DML

```ts
insert(table, { rows, onConflict? }); // ops.ts:574
update(table, { set, where?, batch? }); // ops.ts:602
del(table, { where, limit? }); // ops.ts:615 — where is mandatory
backfill(table, { set, where?, cursorColumn?, batchSize?, name? }); // ops.ts:626
```

```ts
insert("plans", {
  rows: [
    { id: "free", price_cents: 0 },
    { id: "pro", price_cents: 2900 },
  ],
});

// PG-only upsert: on a conflicting `id`, update the listed columns.
insert("plans", {
  rows: [{ id: "pro", price_cents: 3900 }],
  onConflict: { columns: ["id"], doUpdate: { price_cents: 3900 } },
});

update("orders", {
  set: { status: (c) => c.fn.upper(c("status")) },
  where: (c) => c("status").eq("pending"),
});

del("sessions", { where: (c) => c("expires_at").lt("2026-01-01T00:00:00Z") });

backfill("orders", {
  set: { total_norm: (c) => c.fn.coalesce(c("total"), 0) },
  cursorColumn: "id", // defaults to the single-column PK ("id")
  batchSize: 1000, // defaults to the engine's chosen size
});
```

- The predicate keyword is **`where` everywhere** — there is no `filter`
  synonym.
- `del`'s `where` is **mandatory** — an unguarded full-table delete is rejected
  at record time.
- `backfill` (and `update { batch }`) is a batched, per-batch-transactional,
  resumable loop that persists crash-safe cursor progress under the project
  lock. It runs on **both backends** (PG via the existing windowed executor;
  SQLite via the committed batched executor).
- Row values may be a string / safe number / `bigint` / boolean / `null` /
  `Uint8Array` / `{ decimal: "…" }`. A `bigint` (integers beyond 2^53) is
  normalized to the `{ decimal }` carrier and a `Uint8Array` to a base64
  `{ bytes }` carrier before recording, so the wire shape matches the engine's
  scalar deserializer (`sdks/migrate/src/types.ts:82-89`,
  `sdks/migrate/src/ops.ts:184-188`).

`insert`'s `onConflict` (upsert) is **Postgres-only**. There is no portable
SQLite upsert and no raw route; a SQLite-targeted `onConflict` is a hard build
error (`dialect_scope = PgOnly`), surfaced at build, never at runtime
(`sdks/migrate/src/types.ts:207-222`).

## The `schema` qualifier (profile-gated)

Every table-targeting op accepts an optional `schema` (a plain identifier string —
names-are-strings, never live-schema-bound). It selects the schema the op renders
into. Its meaning is **profile-gated**:

- **General / dbmate-like CLI (Trusted profile):** the qualifier is **honored**.
  The op renders `"schema"."table"` on Postgres. The default schema, when an op
  omits its own, is the connection default (a `--schema`/search-path flag,
  threaded as the engine's `default_schema`) → else the connection `search_path`
  head. On **SQLite** the implicit target is `main` (the app file); a `schema`
  that resolves to `main` (or to the bound project schema) renders unqualified.
  A **non-`main` schema is refused fail-closed at lower** (`SqliteSchemaUnsupported`):
  the SQLite emitter renders unqualified `main` DDL and the engine does **NOT**
  auto-`ATTACH`, so honoring a non-`main` qualifier would silently drop it and land
  the op in `main` — a silent wrong-target. Rather than that, lowering refuses; a
  non-`main` SQLite schema requires an explicit `ATTACH … AS <schema>` the operator
  arranges, never an implicit re-pin to `main`.

- **Backfill / batched-update + an explicit schema (any profile):** the resumable
  backfill executor qualifies its windowed `UPDATE` into the **deploy-time project
  schema only** (it does not consume a per-spec schema). A `backfill` (or a batched
  `update { batch }`) whose effective schema differs from the project schema is
  therefore **refused fail-closed at lower** (`BackfillSchemaUnsupported`) rather
  than silently project-pinned — inconsistent silent-wrong-schema is never the
  disposition. The one-shot `insert`/`update`/`delete`/non-batched path honors the
  schema normally; only the resumable batched path refuses until the executor
  threads a per-spec schema.

- **Platform creator deploy (Confined profile):** the project schema is **pinned**.
  An op that omits `schema`, or names the project schema, is fine; an explicit
  `schema` that differs from the project schema is **refused at validate-time,
  fail-closed**, with a structured `CROSS_SCHEMA` authoring error — *before* lower,
  earlier and friendlier than the least-privilege migrator role's `42501` and the
  parse-guard's cross-schema denial (both of which stay in force as line-2/line-3
  defenses). The confinement invariant is unchanged; this is an additional, earlier
  gate.

- **Platform-internal (Platform profile):** an explicit `schema` must be a member
  of the configured platform schema allow-list, else `CROSS_SCHEMA`.

The schema string is an identifier the engine double-quotes, so it is also
**validated for injection** at validate-time (`INVALID_SCHEMA_IDENT`): it must be a
non-empty, alpha/`_`-leading bare identifier of `[A-Za-z0-9_]` — an injection-shaped
value (`"; DROP …`, an embedded quote) is rejected on every profile.

```ts
// Trusted CLI: render into a non-default schema.
createTable("audit_log", { id: t.id() }, undefined, { schema: "reporting" });
insert("widgets", { rows: { id: 1 }, schema: "reporting" });
// Confined creator deploy: a cross-schema op is refused fail-closed.
dropTable("other_app_table", { schema: "some_other_app" }); // → CROSS_SCHEMA
```

## Existence guards (`ifExists` / `ifNotExists`)

The create/add family (`createTable`, `addColumn`, `createIndex`,
`addForeignKey`/`addUnique`/`addCheck`) carries an `ifNotExists` option; the
drop/rename/alter family (`dropTable`, `dropColumn`, `dropIndex`,
`dropConstraint`, `renameColumn`, `alterColumn`) carries an `ifExists` option. A
guard on the wrong family is a `GUARD_DIRECTION` authoring error.

> **These guards are NOT YET SUPPORTED — the option types are `false`, so passing
> `{ ifNotExists: true }` / `{ ifExists: true }` is a BUILD-TIME (`tsc`) type
> error, not a deploy-time surprise.** The full shape (IR/wire/validate) is in
> place but the executor-side probe that honors the guard is a later slice (op.*
> PR10 Part B); see the Status box below. The author-facing types
> (`IfNotExistsNotYetSupported` / `IfExistsNotYetSupported`, both the literal
> `false`) give a compile-time signal that the feature is unavailable. They widen
> back to `boolean` when Part B lands.

These are **NOT** lowered to a native `IF [NOT] EXISTS` clause. Native support is
patchy and asymmetric: Postgres has no `ADD CONSTRAINT IF NOT EXISTS` and none on
`ALTER COLUMN`/`RENAME`; SQLite has no `ADD COLUMN IF NOT EXISTS`, none on
drop-column, and none on rename. Lowering to native SQL would therefore silently
support the guard on some ops/dialects and explode on others.

Instead the engine **synthesizes** the guard uniformly via an **executor-side
catalog probe**, run under the held project advisory lock (so probe→act is
TOCTOU-free, the same interlock argument as the §2.0.3 contract): it queries the
catalog (PG `pg_catalog`/`information_schema`; SQLite `sqlite_master` + PRAGMAs),
decides **in Rust** whether the object is present, then runs the bare op or skips
it. The default semantic is **shape-verify-or-fail**, never a bare skip:

- `ifNotExists`, object **absent** → run the bare op.
- `ifNotExists`, object **present and its shape matches** the declared op → a
  journaled **satisfied** no-op (the migration still records a version — satisfied,
  not failed).
- `ifNotExists`, object **present but its shape DIFFERS** (e.g. `addColumn
  ifNotExists` where the column exists with a different type) → **FAIL CLOSED** with
  a drift error naming the divergence. It is never a silent skip over a divergent
  object.
- `ifExists`, object **present** → run the bare drop/alter.
- `ifExists`, object **absent** → a journaled satisfied no-op (a drop has no shape
  to verify — presence alone governs).

> **Status — existence guards are NOT YET honored end-to-end; authoring one is a
> hard lower-time refusal on EVERY op.** The IR shape, the JS surface (both the
> typed `ops.ts` and its engine-embedded `migrate_ops.js` twin), the validate-time
> direction check, and the wire/checksum/golden plumbing for the existence-guard
> family are in place. The executor-side catalog probe (probe →
> shape-verify-or-fail → run/skip) is the next slice. **Until it lands, passing
> `{ ifNotExists: true }` / `{ ifExists: true }` to ANY op — `createTable`,
> `addColumn`, `createIndex`, `addForeignKey`/`addUnique`/`addCheck`, `dropTable`,
> `dropColumn`, `dropIndex`, `dropConstraint`, `renameColumn`, `alterColumn*` — is
> **refused fail-closed at lower** (`ExistenceGuardNotYetSupported`).** The refusal
> is **uniform**: the guard is recorded faithfully by the DSL (the twin no longer
> silently drops it on any op — review F1) and then hard-refused at lower for every
> op, so there is never the split where some ops silently drop the guard (applying
> the bare op unconditionally — a fail-OPEN over a possibly-divergent existing
> object) while others hard-error. Do not author an existence guard expecting it to
> take effect yet: the author-facing option type is now the literal **`false`**
> (`IfNotExistsNotYetSupported` / `IfExistsNotYetSupported`), so `{ ifNotExists:
> true }` / `{ ifExists: true }` is a **build-time `tsc` error** — you get the
> signal at compile time, not as a deploy-time 422. (The `migrate_ops.js` twin and
> the IR shape still carry the boolean so Part B can light it up without a wire
> break; the runtime refusal — `ExistenceGuardNotYetSupported` — names the deferral
> and points at op.* PR10 Part B for any raw-JS deploy that bypasses `tsc`.) When
> the probe lands, the divergent-object
> shape-verify MUST fail closed (never a silent skip) and read the right catalog per
> backend (PG `pg_catalog`/`information_schema`; SQLite `sqlite_master` + PRAGMAs),
> including index/constraint guards.

### SQLite-safe rebuild (`batchAlterTable`)

```ts
batchAlterTable(table, (b) => void); // ops.ts:642
```

A SQLite ALTER that SQLite cannot do in place (drop a column on an old SQLite,
re-type, etc.) is lowered to the 12-step table rebuild. `batchAlterTable` groups
several column/constraint changes against one table so they share one rebuild
(`sdks/migrate/src/types.ts:260-267`): `b.addColumn`, `b.dropColumn`,
`b.renameColumn`, `b.alterColumn`, `b.addForeignKey`, `b.addCheck`.

## The fluent expression surface

Every expression position — a DML `set` value, a `where`, an `addCheck` body, a
partial-index `where:` — is a callback `(c) => Expr` with a **single injected
builder handle** `c`. It is never a raw string; it constructs a node of a closed
AST via an all-strings fluent builder
(`sdks/migrate/src/ops.ts:281-366`, `sdks/migrate/src/types.ts:95-156`).

**`c` is both a column accessor and the function namespace.** `c("first")`
returns a `ColRef` chain; the argument is a plain string. `c` is scoped to the
enclosing op's target table: `c("x")` resolves against that one table and
nothing else — there is no `c("other.col")` and no second-table accessor
(cross-table references are not expressible, see
[the portability boundary](#the-dml-portability-boundary)).

**Chainable operator methods** (each builds one closed-AST node; a bare JS value
passed to a method auto-wraps to a `Literal` and is bound via `$n`/`?n`, never
interpolated):

- comparison: `.eq(x)`, `.ne(x)`, `.lt(x)`, `.le(x)`, `.gt(x)`, `.ge(x)`
- boolean: `.and(e)`, `.or(e)`, `.not()`
- arithmetic: `.add(x)`, `.sub(x)`, `.mul(x)`, `.div(x)`
- string/value: `.concat(...parts)` (raw `||`, NULL-propagating),
  `.concatWs(sep, ...parts)` (NULL-skipping), `.coalesce(...)`
- null/bool tests: `.isNull()`, `.isNotNull()`, `.isTrue()`, `.isFalse()`
- cast: `.cast("text" | "integer" | "real" | "boolean" | "blob")` (the closed
  portable target set only)

**`c.fn.*` — the scalar-function namespace** (`sdks/migrate/src/ops.ts:322-357`):

- `c.fn.lower(e)`, `c.fn.upper(e)`, `c.fn.trim(e)`, `c.fn.length(e)`,
  `c.fn.abs(e)`
- `c.fn.coalesce(...)`, `c.fn.nullif(a, b)`
- `c.fn.concatWs(sep, ...parts)` — NULL-skipping concatenation, the safe form
  for joining first+last name. Engine-synthesized to be byte-identical across PG
  (`concat_ws`) and SQLite (a proven `coalesce`-folded `||`). For empty-string
  join use `c.fn.concatWs("", …)`.
- `c.fn.case([[cond, val], …], elseVal?)` — the searched `CASE` form
- `c.fn.splitPart(e, delim, n)` — the engine-synthesized portable split helper,
  within its pinned envelope (see below)
- `c.fn.now()`, `c.fn.genRandomUuid()` — DB-evaluated apply-time scalars
  (render to `now()` / `gen_random_uuid()` per dialect). Use these instead of
  baking a build-time `Date.now()` / UUID literal into the artifact.

The expression records as **dialect-neutral data, never SQL** — the engine owns
all per-dialect lowering, so the plan checksum is dialect-stable. There is no
author-named `substr` / `split_part` / `instr` / `replace`: those cross-dialect
semantics diverge, so they are simply not in the namespace. The only split
surface is the engine-pinned `c.fn.splitPart`.

### Determinism: don't bake a clock or RNG into a migration

A migration is recorded once into a committed artifact, so a `Date.now()` /
`Math.random()` / `crypto.randomUUID()` / `new Date()` in an op argument would
freeze a build-time value. `lintDeterminism(source)` is a best-effort source
scan that steers you to the structured replacement (`c.fn.now()` /
`c.fn.genRandomUuid()`); findings are warnings, not hard rejects
(`sdks/migrate/src/ops.ts:682-696`).

## The DML portability boundary

Portability is real but **bounded**, and the boundary is honest. The principle
(Alembic/Kysely): the engine owns control flow and statement assembly; the
author owns the data-transform expression — but here the author expresses that
transform only through the closed fluent AST, never raw SQL.

**Portable — works on both PG and SQLite from one op:**

- `insert` with literal rows (`onConflict` excepted — PG-only).
- `del` with a `where`.
- One-shot `update` / `backfill` whose `set` / `where` use only the closed
  fluent AST: column refs, auto-wrapped literals, arithmetic,
  comparison/boolean operators, `c.fn.case`, the allow-listed
  provably-identical scalars (`coalesce`, `nullif`, `lower`, `upper`, `trim`,
  `length`, `abs`, `.cast(<portable type>)`, `.concat`), and `c.fn.concatWs`.
- The engine-synthesized `c.fn.splitPart` helper **within its pinned envelope**.

### The `splitPart` / `concatWs` portable-expression envelope

`c.fn.splitPart(col, delim, n)` lowers to `split_part(col, 'd', n)` on Postgres
and to a pinned, exhibited `instr`/`substr` expression on SQLite, proven
byte-identical to PG against real SQLite 3.51.2. The full portability envelope —
admitting it on **both** backends — is:

- `delim` is a literal, **non-empty, single ASCII character** (one byte, code
  point < 0x80);
- `n` is a literal **positive integer** — and on the SQLite leg, `1 ≤ n ≤ 8`
  (the inline unroll grows O(2ⁿ); past 8 it can exceed SQLite's expression-depth
  limit);
- `col` is a column ref or an in-AST sub-expression.

This envelope is enforced across **two layers**, not one:

- The record-time JS grammar lint (`sdks/migrate/src/ops.ts:698-712`, the
  `splitPartGrammarLint`; mirrored at
  `crates/zeroship-migrate-js/src/migrate_ops.js:1106-1127`) rejects only the
  *dialect-neutral, clearly-malformed* shapes — a non-string or empty `delim`,
  and a non-integer or non-positive `n`. It does **not** check single-ASCII,
  multi-character, or the `1 ≤ n ≤ 8` bound (the recorder twin's own comment is
  explicit, `migrate_ops.js:1097-1104`).
- The single-ASCII delimiter and the SQLite-leg `1 ≤ n ≤ 8` bound are enforced
  by the **Rust validator**: a multi-character / non-ASCII delimiter or `n > 8`
  is *admitted on Postgres* (`dialect_scope = PgOnly`) and is a hard
  `EXPR_NOT_PORTABLE` only on the SQLite leg. That is dialect-gated, so it
  cannot live in the dialect-neutral JS lint.

The value being split *may* contain multibyte UTF-8 content — it is the
**delimiter** that is constrained to single-ASCII, because an ASCII byte never
occurs inside a UTF-8 multibyte sequence, which is precisely why the byte-wise
SQLite scan finds the same boundaries as PG's character-wise `split_part`.

`c.fn.concatWs(sep, …)` is the NULL-skipping join, engine-synthesized to render
byte-identically on both backends. Prefer it over `.concat(...)` for joining
values: `.concat` maps to `||`, whose NULL rule is documented (a NULL operand
yields NULL on both backends) — fine when you want propagation, a footgun when
you don't.

### Out of envelope is a hard error, not a silent mis-apply

A `c.fn.splitPart` call outside the envelope — a multi-character / empty /
non-ASCII delimiter, `n = 0`, negative `n`, `n > 8`, or non-literal args — is a
hard `EXPR_NOT_PORTABLE` error on the SQLite leg. The clearly-malformed shapes
(empty delimiter, non-positive / non-integer `n`) are caught earlier, at record
time, by the JS grammar lint above; the dialect-gated single-ASCII and `n ≤ 8`
bounds are caught by the Rust validator. It is **never** silently mis-split. The
structured error names the two real resolutions:

```jsonc
{
  "suggested_fix": "use a single-ASCII delimiter with 1<=n<=8, restructure to stay in-envelope (split into <=8 parts), or mark the migration PG-only (dialect_scope=PgOnly)",
  "code": "EXPR_NOT_PORTABLE",
  "op_index": 2,
  "ts_location": "migrations/0007_split_name.ts:9",
  "dialect": "sqlite",
  "reason": "c.fn.splitPart is portable only for a single-ASCII delimiter and a positive literal n in 1..8; this call is out of envelope"
}
```

## No raw-SQL escape hatch (property A)

There is no `Raw` type, no `sql\`\``, no string-fragment route — on either
dialect. Where a transform is dialect-divergent and not expressible in the closed
AST (an exotic PG-only function, a subquery/window, a cross-table reference,
an out-of-envelope split), there is **no raw fallback**. It surfaces as a hard
structured error (`UNSUPPORTED` with `kind:"expr"`/`"op"`, or
`EXPR_NOT_PORTABLE`), and the only resolutions are:

1. **Reshape** the migration into the portable surface (e.g. split into ≤ 8
   parts), or
2. **Accept `dialect_scope = PgOnly`** — the engine renders the PG form; the
   migration is then not authorable on the SQLite dev tier. `dialect_scope` is
   *derived* from the ops and the engine's allow-list version, never
   author-declared via a raw string.

The expressible surface is the supported surface. This is what keeps "one script,
both backends" honest: a migration that passes the both-backends dry-run applies
faithfully on both; a migration that cannot, fails loudly at authoring/render
time, not silently at runtime on one backend.

## Online rename

> **Status: available in dev/CLI; production deploy go-live is a planned
> follow-up.** `renameColumn` lowering works and is exercised end-to-end in
> dev/CLI and tests. The production control-plane deploy handler does **not**
> apply online renames today — see below.

`renameColumn(table, from, to, type)` records a single op that the engine lowers
to a dual-dialect online change:

- **Postgres** — an expand-contract online flow: add the new column, install a
  dual-write trigger, backfill, then (in a later phase) drop the old column. The
  app stays up throughout.
- **SQLite** — the offline 12-step table rebuild (`RenameStep::SqliteRebuild`),
  applied via the engine's rebuild path. A rebuild on a populated table is
  destructive, so it requires approval.

Both lowerings are implemented and covered by tests (the PG expand-contract and
the SQLite rebuild apply end-to-end in
`crates/zeroship-migrate/tests/ir_rename_pr2_pg.rs` /
`ir_rename_pr2_sqlite.rs`).

**What is not yet wired for production deploy.** The production control-plane
deploy handler (`crates/control/src/api.rs:702`, `run_deploy_migrations`) applies
migrations through `apply_bundle_migrations` under `Approval::None`, which
**refuses** an online rename's EXPAND phase before it can complete. The
approved-apply go-live surfaces (`apply_bundle_migrations_approved` /
`apply_bundle_ir_sqlite` with `Approval::Approved`,
`crates/control/src/deploy_migrate.rs:286`,
`crates/zeroship-migrate/src/ir_apply.rs:130`) are reachable only from tests
today. That test-only status is load-bearing and pinned by a regression test
(`production_deploy_handler_never_wires_the_unguarded_approved_go_live_surface`,
`crates/control/tests/deploy_migrate_test.rs:643`), which fails RED the instant
the approved surface is wired into a production handler.

Production go-live gating, post-PR9a:

1. The cross-deploy **pending-contract interlock** — the PG expand/contract is a
   multi-deploy flow (the contract that drops the old column owes a later
   approved deploy). As of PR9a this owed contract **IS** journaled as a durable
   obligation and **IS** fail-closed enforced across deploys: a completed EXPAND
   records the obligation (keyed on a deterministic, re-lower-stable version,
   §2.0.1), a later deploy whose ops touch the pending table is refused with
   `TABLE_HAS_PENDING_CONTRACT`, an orphaned obligation is surfaced by `status`,
   and `resolve-pending --apply|--abort` discharges it. The whole-deploy project
   advisory lock is held across the entire multi-file IR deploy, so the read-back
   is race-free. (Resolved: the interlock is implemented + enforced — the
   previously remaining orphan/blocked deterministic-keying gap is closed too.)
2. **Per-version approval scoping** — the current approved surface is a coarse,
   bundle-wide approval; production needs approval scoped to the specific
   reviewed version-ids. This is the remaining gate.

Until per-version approval scoping lands, the approved go-live surface stays
test-only (the regression test above pins it); treat online `renameColumn` as a
dev/CLI capability, not a shipped production deploy path.

## Appendix: the hero example as IR

A migration is recorded into a dialect-neutral `.ir.json` artifact (the frozen
wire contract the engine loads). For reference — and because the
bi-dialect-apply CI gate (below) applies exactly this artifact on **both**
Postgres and SQLite — here is a representative split-name migration as IR:
structurally equivalent to the hero `up()` ([Module shape](#module-shape)) — the
same two `addColumn`s, a `c.fn.splitPart` backfill, and a `dropColumn`, applying
byte-identically on PG and SQLite from this one artifact.

> This appendix is **illustrative, not the literal recording of the TS hero**.
> The hero `up()` operates on `users` and relies on the engine's defaults
> (`batchSize` 1000, an auto-derived backfill `name`); this artifact is the
> standalone form the Rust apply gate seeds and applies, so it names `people`,
> pins `batchSize: 50`, `cursorColumn: "id"`, and `name: "split_name_bf"`
> explicitly. Copy the TS hero, not this JSON — the build evaluator records the
> JSON for you (with the hero's own table and defaults).

```json
{
  "ir_version": 1,
  "name": "split_name",
  "ops": [
    { "op": "addColumn", "table": "people", "column": "first_name", "type": "text" },
    { "op": "addColumn", "table": "people", "column": "last_name", "type": "text" },
    {
      "op": "backfill",
      "table": "people",
      "cursorColumn": "id",
      "batchSize": 50,
      "name": "split_name_bf",
      "set": {
        "first_name": { "node": "fnSynth", "fn": "splitPart", "args": [
          { "node": "colRef", "name": "name" },
          { "node": "literal", "value": " " },
          { "node": "literal", "value": 1 }
        ]},
        "last_name": { "node": "fnSynth", "fn": "splitPart", "args": [
          { "node": "colRef", "name": "name" },
          { "node": "literal", "value": " " },
          { "node": "literal", "value": 2 }
        ]}
      }
    },
    { "op": "dropColumn", "table": "people", "column": "name" }
  ]
}
```

This is the one place authors see the IR — you never hand-write it; the build
evaluator records it from your `.ts`. It is shown here so the "one script, both
backends" claim is concrete and so the CI gate has a doc-sourced artifact to
apply.

**What the doc-example gates do and do not prove.** Two gates keep this doc
honest. The TS leg (`sdks/migrate/tests/doc-examples.test.ts`) compiles every
runnable typed snippet against the real `@zeroship/migrate` types (the
signature-listing blocks, which use bare param names, are excepted), so a
renamed op or a changed signature fails CI — but it only proves
**type-correctness**; the
snippets compile inside never-executed function bodies, so it does **not**
exercise record-time runtime checks (the `splitPartGrammarLint` throw, `del`'s
mandatory-`where` reject). Those runtime invariants are covered separately by
`sdks/migrate/tests/ops.test.ts` and by the Rust apply gate
(`crates/zeroship-migrate/tests/doc_hero_apply.rs`), which applies the appendix
IR byte-identically on real PG + SQLite. Do not read a green TS gate as proof a
snippet would also survive record-time.

## Further reading

- **Security / threat model** — the recorder runs untrusted creator code inside
  a kernel sandbox (seccomp + landlock + netns); the apply path runs under a
  least-privilege per-app migrator role behind a parse deny-list and an immutable
  journal. See the migration-engine threat model
  ([docs/proposals/2026-06-16-db-migration-engine-design.md §1](../proposals/2026-06-16-db-migration-engine-design.md))
  and the SQLite authorizer allow-list discipline (`§9` of the op-DSL design,
  below).
- **The op-DSL design (normative source of truth)** —
  [docs/proposals/2026-06-23-js-op-dsl-migration-design-normative.md](../proposals/2026-06-23-js-op-dsl-migration-design-normative.md):
  the IR wire contract (§2), the typing stance (§3.3), the closed expression AST
  (§3.3.1), and the authoritative DML portability boundary (§9).
- **SQLite divergences** — intentional Postgres↔SQLite differences in search,
  isolation, locking, and ordering: [sqlite-divergences.md](./sqlite-divergences.md).
- **The schema SDK** — [db.md](./db.md): the `@zeroship/db` `t.*` lexicon the
  migration lexicon mirrors and `fromDb` bridges.
