# `@zeroship/migrate` — the op DSL

`@zeroship/migrate` is the no-raw-SQL, fully-structured authoring surface for
zeroship database migrations. A migration is a `.ts` module that imports
`{ table, t }` and exports a single `default { name?, up, down? }` object. You
describe schema changes (DDL) and data migrations (DML) once through the fluent
`table()` handle; the engine lowers them per-dialect and applies them faithfully
to **both Postgres and SQLite** from one script.

`table(name, { schema? })` is the **sole** authoring entry. There is no flat
`createTable`/`addColumn`/… vocabulary — every operation is a method (or a
selector terminal) on the handle `table()` returns.

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
import { table, t } from "@zeroship/migrate";

export default {
  name: "split_name_column", // optional; defaults to the filename label

  up() {
    const users = table("users");
    users.column("first_name").add({ type: t.text() }); // nullable by default
    users.column("last_name").add({ type: t.text() });
    users.backfill({
      set: {
        first_name: (c) => c.fn.splitPart(c("name"), " ", 1),
        last_name: (c) => c.fn.splitPart(c("name"), " ", 2),
      },
      where: (c) => c("first_name").isNull(),
    });
    users.column("name").drop();
  },

  down() {
    const users = table("users");
    users.column("name").add({ type: t.text() });
    users.backfill({
      // c.fn.concatWs is NULL-skipping — the safe join; copy this, not `.concat`
      set: { name: (c) => c.fn.concatWs(" ", c("first_name"), c("last_name")) },
    });
    users.column("first_name").drop();
    users.column("last_name").drop();
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
  SQL — the `table()` handle's terminals *record* a plain-data op onto an ambient
  per-migration recorder, synchronously (no `await`). This is the
  vitest/jest/Playwright pattern: `import { table }` then call it. The
  build/dev evaluator installs a fresh recorder before calling `up()` (and
  again before `down()`), drains the recorded op list, and renders it to the
  checksummed `.ir.json` artifact.
- Authoring **outside an active recorder** — at module top level, or after
  `up()` returns (e.g. from a stray `setTimeout`) — throws a structured
  `OP_OUTSIDE_RECORDER` error. The op cannot be silently lost.
- A **selector that is never terminated** (`table("u").column("email")` with no
  `.add()`/`.drop()`/`.rename()`/`.alter()`) is a hard `SELECTOR_NOT_TERMINATED`
  build error at drain — never a silent no-op (see
  [Selectors must be terminated](#selectors-must-be-terminated)).

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

## The `table()` entry point

The **entire** authoring surface is reached through one import: `table` (plus the
`t` column-type lexicon). There is no flat op vocabulary and no `op.` prefix.

```ts
import { table, t } from "@zeroship/migrate";
```

The complete exported vocabulary (`sdks/migrate/src/index.ts`):

| Export | Purpose |
| --- | --- |
| `table` | the sole authoring entry — returns the reusable `TableHandle` |
| `t` | the immutable column-type lexicon |
| `fromDb` | the `@zeroship/db` field → migration `ColumnDef` bridge |
| `lintDeterminism` | the best-effort determinism source scan |

`table(name, { schema? })` returns a handle whose methods are the whole DDL+DML
surface (see [The `table()` surface](#the-table-surface)). The handle's terminals
record eagerly and return the handle, so calls chain and a handle is reusable
across statements ([Var-assign + reuse](#var-assign--reuse)).

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
are immutable historical artifacts. If `.column().add()`/`.update()` typed their
names against the *current* schema, a migration that referenced `users.lastSeen` would
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
- **Alembic** uses string names: `op.add_column("users", …)`. zeroship's
  `table("users").column(…)` likewise carries plain-string names.
- **Drizzle** migrations are generated SQL, not type-checked against the live
  schema.

**What IS type-checked (structural safety, preserved):**

- **Op argument shapes** — you cannot pass a number where a `ColumnDef`
  is expected, or omit a required field on a terminal's args object.
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

Every column-type position (`create`'s `columns`, `.column().add()`,
`.column().rename()`, `.column().alter()`) takes a chainable `ColumnDef` produced
by the fluent `t.*` lexicon. **Columns are nullable by default**; `.notNull()` is
the rarer, riskier opt-in.

The `t.*` chain is **immutable**: every modifier returns a **fresh** `ColumnDef`
rather than mutating the receiver, so a hoisted type var is safe to reuse across
columns without aliasing (see [Var-assign + reuse](#var-assign--reuse)).

The shipped factories (`sdks/migrate/src/ops.ts`):

| Factory | Column type |
| --- | --- |
| `t.id()` | a non-null `uuid` PK defaulting to `gen_random_uuid()` |
| `t.text()` | text |
| `t.integer()` | 32-bit integer |
| `t.bigInt()` | 64-bit integer |
| `t.float()` | floating point |
| `t.numeric(precision?, scale?)` | fixed-precision decimal (default `(38, 9)`) |
| `t.boolean()` | boolean |
| `t.timestamp()` | timestamp |
| `t.uuid()` | uuid |
| `t.bytes()` | byte array |
| `t.json()` | json |
| `t.vector(n)` | a pgvector column of dimensionality `n` |
| `t.geoPoint()` | a geo point |
| `t.ref(targetTable)` | a foreign-key reference (plain-string target) |
| `t.encrypted({ of })` | an application-level encrypted column wrapping an inner type |

> The `string`/`int` aliases and the `t.X({ notNull, default })` options-bag
> overload are **removed**. Use the canonical `t.text()`/`t.integer()` and the
> chain (`t.text().notNull().default("pending")`).

Chainable modifiers (`sdks/migrate/src/ops.ts`), each returning a fresh `ColumnDef`:

| Modifier | Effect |
| --- | --- |
| `.notNull()` | mark `NOT NULL` |
| `.default(value)` | a typed scalar literal **or** a nullary synth scalar `{ fn: "now" \| "genRandomUuid" }` — never raw SQL |
| `.primaryKey()` | mark the table primary key (implies `NOT NULL`) |
| `.unique()` | add a single-column `UNIQUE` |
| `.ref(targetTable)` | re-target the column as a foreign-key reference (plain-string target) |

```ts
import { table, t } from "@zeroship/migrate";

export default {
  up() {
    table("orders").create({
      columns: {
        id: t.id(),
        total: t.numeric(12, 2).notNull().default(0),
        status: t.text().notNull().default("pending"),
        customer_id: t.ref("customers").notNull(),
      },
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
(`sdks/migrate/src/ops.ts` `fromDb`), so a `t.ref("users")` declared in your app
schema lowers to the byte-identical neutral type a hand-written migration column
produces. It carries the field's nullability (`.required()` → `.notNull()`) and
uniqueness, and returns a chainable `ColumnDef` so you can still layer migration
modifiers on top. Names are never bound — a bridged `ref` keeps its target as a
plain string. A non-storage `@zeroship/db` field (a json `array`, a nested
`object`, a `union`) has no portable column type and throws
`UnsupportedColTypeError` (`sdks/migrate/src/db-lexicon.ts:51-61`) — a hard
boundary, never a silent fallback.

## The `table()` surface

`table(name, { schema? })` returns a `TableHandle`. Everything — DDL and DML — is
a method (or a selector terminal) on that handle. Every terminal takes **exactly
one named-object** argument (identity — the table name and a selector name — is
positional; payload + options are a named object), records eagerly, and **returns
the handle** so calls chain.

### The table itself

```ts
table("audit_log").create({
  columns: {
    id: t.id(),
    org_id: t.ref("orgs").notNull(),
    email: t.text().notNull(),
    role: t.text().notNull().default("member"),
  },
  primaryKey: ["org_id", "email"], // composite PK (else a single PK via t.id()/.primaryKey())
  uniques: [{ name: "members_org_email_uq", columns: ["org_id", "email"] }],
  checks: [{ name: "members_role_nonempty", expr: (c) => c("role").ne("") }],
  foreignKeys: [
    {
      name: "members_org_fk",
      columns: ["org_id"],
      references: { table: "orgs", columns: ["id"] },
      onDelete: "cascade",
    },
  ],
  indexes: [{ name: "members_org_idx", columns: ["org_id"] }],
});

table("scratch").drop({ ifExists: true, cascade: true });
```

`create({...})` is the one all-object form (no `build` callback): table-level
constraints and indexes are **fields**, and each carries a **required `name`**
(name-first, so a later migration can deterministically drop it). Foreign-key
actions are `cascade | restrict | setNull | setDefault | noAction` (the index
method set is `btree | gin | gist | ivfflat | hnsw | fts5`).

> A table-level `.rename({ to })` is **not** available: there is no `renameTable`
> op in the IR and no executor support for it (column rename is `.column().rename()`,
> see [Online rename](#online-rename)).

### Columns — the `.column(name)` selector

```ts
const orders = table("orders");
orders.column("status").add({ type: t.text().notNull().default("new") });
orders.column("legacy").drop({ ifExists: true });
orders.column("label").rename({ to: "display_label", type: t.text() }); // named ⇒ no swap
orders.column("total").alter({ type: t.numeric(14, 2), using: (c) => c("total").cast("real") });
orders.column("note").alter({ nullable: true });
```

`.column(name).add({ type })` honors **all** modifiers on `type`, including
`.unique()` (which emits a follow-on `UNIQUE` constraint) and `.primaryKey()` —
they are not silently dropped.

### Constraints — per-kind `.add`, name-keyed `.drop`

```ts
const members = table("members");
members.foreignKey("members_org_fk").add({
  columns: ["org_id"],
  references: { table: "orgs", columns: ["id"] },
  onDelete: "cascade",
});
members.unique("members_org_email_uq").add({ columns: ["org_id", "email"] });
table("orders").check("orders_total_nonneg").add({ expr: (c) => c("total").ge(0) });
table("orders").constraint("orders_total_nonneg").drop({ ifExists: true }); // kind-agnostic drop
```

`.foreignKey/.unique/.check(name)` each have one terminal, `.add(...)`;
`.constraint(name)` has one terminal, `.drop(...)` (kind-agnostic, by name).

### Indexes — the `.index(name)` selector

```ts
const members = table("members");
members.index("members_email_idx").add({ columns: ["email"], unique: true, using: "btree" });
members.index("members_email_idx").drop({ unique: true });
```

Indexes are **name-first** (the selector name), so a later migration can drop them
deterministically. `.index().drop({ unique: true })` carries `unique` because the
engine gates a UNIQUE-index drop as destructive (it silently removes a
data-integrity guarantee) — omit it for a plain, reversible drop.

### Table data — direct named DML

```ts
const plans = table("plans");

plans.insert({
  rows: [
    { id: "free", price_cents: 0 },
    { id: "pro", price_cents: 2900 },
  ],
});

// PG-only upsert: on a conflicting `id`, update the listed columns.
plans.insert({
  rows: [{ id: "pro", price_cents: 3900 }],
  onConflict: { columns: ["id"], doUpdate: { price_cents: 3900 } },
});

table("orders").update({
  set: { status: (c) => c.fn.upper(c("status")) },
  where: (c) => c("status").eq("pending"),
});

table("sessions").del({ where: (c) => c("expires_at").lt("2026-01-01T00:00:00Z") });

table("orders").backfill({
  set: { total_norm: (c) => c.fn.coalesce(c("total"), 0) },
  cursorColumn: "id", // defaults to the single-column PK ("id")
  batchSize: 1000, // defaults to the engine's chosen size
});
```

- The predicate keyword is **`where` everywhere** — there is no `filter`
  synonym.
- `.del`'s `where` is **mandatory** — an unguarded full-table delete is rejected
  at record time.
- `.backfill` (and `.update({ batch })`) is a batched, per-batch-transactional,
  resumable loop that persists crash-safe cursor progress under the project
  lock. It runs on **both backends** (PG via the existing windowed executor;
  SQLite via the committed batched executor).
- Row values may be a string / safe number / `bigint` / boolean / `null` /
  `Uint8Array` / `{ decimal: "…" }`. A `bigint` (integers beyond 2^53) is
  normalized to the `{ decimal }` carrier and a `Uint8Array` to a base64
  `{ bytes }` carrier before recording, so the wire shape matches the engine's
  scalar deserializer.
- DML carries **no existence guard** (it is not guardable); `schema` rides on the
  args object.

`.insert`'s `onConflict` (upsert) is **Postgres-only**. There is no portable
SQLite upsert and no raw route; a SQLite-targeted `onConflict` is a hard build
error (`dialect_scope = PgOnly`), surfaced at build, never at runtime.

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

- **Backfill / batched-update + an explicit schema (profile-gated):** the resumable
  backfill executor now threads a **per-spec schema** (`BackfillSpec.schema`), so a
  schema-qualified `backfill` (or a batched `update { batch }`) **runs** against
  `"schema"."table"` — the windowed `UPDATE`, the `search_path` anchor, and the
  catalog introspection all target that schema, and the progress row records it
  (`target_schema`). Which schemas are reachable is decided **upstream** by the
  same cross-schema scope gate as every other op, so confinement is unchanged:
  - **Confined (creator deploy):** the project schema is pinned; a foreign
    qualifier is refused at validate-time (`CROSS_SCHEMA`) before the backfill is
    lowered, so the spec's schema is always the project schema (the render is
    byte-identical to the pre-threading project pin).
  - **Trusted / Platform (operator CLI):** the widened scope admits the
    gate-approved schema, so the cross-schema backfill runs — under Trusted as the
    connecting/admin role (no migrator `SET ROLE`), the documented posture.

    > **Trusted backfill SQL fragments are trusted SQL.** The backfill's
    > authored `set` clause and `where` filter are interpolated into the windowed
    > `UPDATE`. Under **Confined** they run through the confined guard (deny-list
    > + cross-schema walk), so a creator's fragments are statically bounded. Under
    > **Trusted / Platform** the runner uses the **trusted guard** — the deny-list
    > and cross-schema walk are **skipped by design** (only the structural checks
    > remain: cursor-not-mutated + parse). So an operator-authored
    > `set`/`where` is run as trusted SQL, and a Trusted *cross-schema* backfill
    > runs those fragments cross-schema under that non-deny-listed guard. This
    > exactly mirrors the one-shot DML Trusted posture (operator-token-gated, the
    > operator owns the DB) and is **not** a confinement hole: creators can never
    > reach the Trusted profile, so they cannot author these fragments.
  - **SQLite (any profile):** a non-`main` schema is still refused **earlier**
    (`SqliteSchemaUnsupported`, before the backfill lower); SQLite's single `main`
    db renders the table unqualified.

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
// Trusted CLI: render into a non-default schema — set once on the handle.
const reporting = table("audit_log", { schema: "reporting" });
reporting.create({ columns: { id: t.id() } });
table("widgets", { schema: "reporting" }).insert({ rows: { id: 1 } });
// Confined creator deploy: a cross-schema op is refused fail-closed.
table("other_app_table", { schema: "some_other_app" }).drop(); // → CROSS_SCHEMA
```

## Existence guards (`ifExists` / `ifNotExists`)

The create/add family (`.create`, `.column().add`, `.index().add`,
`.foreignKey/.unique/.check().add`) carries an `ifNotExists` option; the
drop/alter family (`.drop`, `.column().drop`, `.index().drop`,
`.constraint().drop`, `.column().alter`) carries an `ifExists` option. A guard on
the wrong family is a `GUARD_DIRECTION` authoring error.

> **Supported as of op.* PR10 Part B** (executor-side catalog probe). The option
> types are plain `boolean`; the guard is honored at apply time by a probe under the
> held advisory lock + the open per-step transaction (see the semantics list and the
> fail-closed defaults below).

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

> **Supported as of op.* PR10 Part B** (executor-side catalog probe). The probe
> reads the live catalog (PG `information_schema`/`pg_catalog`; SQLite `sqlite_master`
> + PRAGMAs) inside the SAME open transaction that will run the `up`, under the
> project advisory lock the whole plan already holds — so there is no probe→act
> TOCTOU window. `decide` is pure Rust over the snapshot, never a SQL-level
> conditional. On `SatisfiedNoop` the version still lands (a journaled completed row)
> so a re-deploy skips it via normal pending computation; on `FailDrift` the txn is
> rolled back and nothing is applied or journaled. (`crates/zeroship-migrate/src/guard_probe.rs`,
> `executor.rs` PG `apply_transactional`, `backend_sqlite/mod.rs` SQLite
> `apply_up_transactional`.)
>
> **Fail-closed defaults (a shape that cannot be fully introspected fails CLOSED,
> never optimistically runs):**
> - **Constraint `ifNotExists` — KIND check plus definition refusal.** A kind clash
>   (`PRIMARY KEY` vs `UNIQUE` …) is `FailDrift` naming `kind`. A PRESENT same-name +
>   same-kind constraint is ALSO `FailDrift` (naming `definition`), NOT a silent
>   no-op: the live `pg_get_constraintdef` body cannot be byte-proven equal to the
>   IR's un-normalized constraint, so a possibly-rewritten CHECK / different FK target
>   is refused rather than skipped. The realistic `ifNotExists` use (the constraint is
>   ABSENT) still runs bare.
> - **Index `ifNotExists` over an expression / partial predicate** — `FailDrift`
>   naming `expression`: the IR `createIndex` (a column-list) cannot render a
>   byte-comparable `pg_get_expr` form, so equivalence is unprovable. A plain
>   column-list index compares `(unique, columns)` fully.
> - **SQLite type-affinity collision** — SQLite stores TEXT affinity for
>   string/date/json/ref alike, so a same-name TEXT-affinity column whose SDK facet
>   changed within one affinity is invisible to the catalog. The SQLite `ifNotExists`
>   column/table verify compares the introspected token exactly; a differing token
>   that still reduces to TEXT affinity is `FailDrift` (it cannot prove full-shape
>   equality), never an affinity-only no-op (the same limitation the SQLite drift path
>   already lives with).
>
> **`renameColumn ifExists` is refused fail-closed at lower** (`GuardProbeUnbuildable`):
> the online-rename plan step is a multi-migration shape with no single Migration the
> probe can attribute its verdict to, and `lower_rename` ALREADY mandates the live
> `from` column exist (an absent source is a hard error today — stricter than the
> guard's "absent → no-op"). The guard is refused rather than silently dropped.
> Every other guarded op (`createTable`/`addColumn`/`createIndex`/`addConstraint`
> family; `dropTable`/`dropColumn`/`dropIndex`/`dropConstraint`;
> `alterColumnType`/`alterColumnNullability`) is honored by the probe.

### SQLite-safe rebuild (automatic)

A SQLite `ALTER` that SQLite cannot do in place (drop a column on an old SQLite,
re-type, a stand-alone constraint add/drop) is lowered by the engine to the
12-step table rebuild automatically — there is no author-facing `batchAlterTable`
grouping. Author the column/constraint changes as ordinary `.column()` /
`.constraint()` terminals; the engine groups the rebuild per table at lower time.

## Selectors must be terminated

A selector (`.column(x)` / `.foreignKey(x)` / `.unique(x)` / `.check(x)` /
`.constraint(x)` / `.index(x)`) returns a sub-builder that records **only** when
its terminal (`.add` / `.drop` / `.rename` / `.alter`) is called. A forgotten
terminal would otherwise silently record nothing — so the recorder makes it a
**hard, structured error**: at `up()`/`down()` drain, any selector handed out but
never terminated throws `{ code: "SELECTOR_NOT_TERMINATED", selector, name }`.
Terminating the same selector twice throws `SELECTOR_ALREADY_TERMINATED`.

The check runs **at drain, not eagerly**, so a selector held in a variable and
terminated on a later line is fine:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  up() {
    // FINE — terminated on a later line (the guard checks at drain).
    const email = table("users").column("email");
    table("users").insert({ rows: [{ id: "u1" }] });
    email.add({ type: t.text().notNull() });
    // ERROR — `table("users").column("nickname")` with no terminal is a hard
    // SELECTOR_NOT_TERMINATED build error.
  },
};
```

## Var-assign + reuse

Both authoring styles are first-class — pick per readability. Every terminal
**returns the handle**, so calls chain; and the handle is a reusable value
(carrying only `{ name, schemaDefault }`), so it can be assigned once and reused
across statements with `{ schema }` set a single time:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  up() {
    // chained
    table("users")
      .column("a").add({ type: t.text() })
      .column("b").drop({ ifExists: true });

    // var-assigned (DRY; { schema } set once)
    const users = table("users", { schema: "app" });
    users.column("email").add({ type: t.text().notNull() });
    users.unique("uq_email").add({ columns: ["email"] });
    users.insert({ rows: [{ id: "u1", email: "a@b.co" }] });
  },
};
```

The `{ schema }` passed to `table()` is the **default schema** stamped onto every
op the handle records; a per-op `schema` (on the terminal's args) **overrides** it
for that one call, and an args bag that omits `schema` keeps the default (an absent
key never wipes it). `table("users")` with no schema records ops with **no**
`schema` key.

Because the `t.*` chain is **immutable** (every modifier returns a fresh
`ColumnDef`), a hoisted type var is safe to reuse across columns:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  up() {
    const reqText = t.text().notNull(); // hoisted, reusable
    table("users")
      .column("email").add({ type: reqText.unique() }) // email is UNIQUE
      .column("name").add({ type: reqText }); // name is NOT unique — reqText untouched
  },
};
```

## C1 — foreign-key referential actions

A `.foreignKey(name).add({...})` (and a `create({ foreignKeys: [...] })` entry)
takes optional `onDelete` / `onUpdate` of `cascade | restrict | setNull |
setDefault | noAction`, and they are **actually rendered** (`ON DELETE CASCADE`,
…). An action-free FK records byte-identically to before:

```ts
import { table } from "@zeroship/migrate";

export default {
  up() {
    table("orders").foreignKey("orders_customer_fk").add({
      columns: ["customer_id"],
      references: { table: "customers", columns: ["id"] },
      onDelete: "cascade",
    });
  },
};
```

## C2 — `.column().add()` honors `.unique()` / `.primaryKey()`

`.column(name).add({ type })` honors **every** modifier on `type`. An ADD COLUMN
has no inline `UNIQUE`, so a `t.*.unique()` / `t.*.primaryKey()` on an added column
records the column **plus** a follow-on constraint (it is not silently dropped):

```ts
import { table, t } from "@zeroship/migrate";

export default {
  up() {
    // records an addColumn AND a follow-on UNIQUE constraint on "email".
    table("users").column("email").add({ type: t.text().notNull().unique() });
  },
};
```

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
- string/value: `.concat(...parts)` (raw `||`, NULL-propagating). The
  NULL-skipping `concatWs` and `coalesce` live on `c.fn.*` only (they are not
  chain methods).
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

`table(t).column(from).rename({ to, type })` records a single op that the engine
lowers to a dual-dialect online change:

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

## Offline SQL preview (`plan`)

`zeroship-migrate plan --dir <d> --dialect <pg|sqlite>` renders the **exact
per-dialect SQL the pending migration set WOULD execute** — without a database and
without applying anything. This is the canonical Alembic `--sql` / Atlas / Flyway /
dbmate feature, here for one job: **operator go-live review**. Before approving an
`approved_versions` go-live you can read the precise SQL the deploy will run,
instead of approving blind.

It is **distinct from `validate`** (the shadow dry-run): `validate` needs a real DB
and *applies* the migration on a throwaway shadow to prove it runs; `plan`
opens **no connection** and renders the SQL statically. Use `validate` to prove it
*works*; use `plan` to review *what it does*.

The preview is a **surfacing layer**, not a second renderer: it prints back the SQL
the engine already lowers (the `Migration.up` / DML `template`). It never
re-implements rendering, so the previewed DDL/DML is byte-identical to what apply
runs.

**What renders (the offline-renderable subset).** The DB-independent ops render
their real SQL: `createTable` / `dropTable` / `addColumn` / `dropColumn` /
`addForeignKey` / `addUnique` / `addCheck` / `dropConstraint` / `createIndex` /
`dropIndex`, and one-shot `insert` / `update` / `delete` (the DML prints its
placeholder template — `$n` on Postgres, `?n` on SQLite — with a bind-count note;
bind values are bound natively, never interpolated into the SQL).

**The honest boundary — `-- [runtime-resolved]`.** Some ops cannot be faithfully
rendered offline because their SQL depends on the **live database state**. The
preview never fabricates SQL for these; it emits a clearly-labeled
`-- [runtime-resolved] …` line stating *why*:

- **online `renameColumn`** — needs the live `from` column's type/structure to
  reconcile the type and author the expand-contract dual-write (PG) or the 12-step
  rebuild (SQLite); the **backfill is windowed by PK** and the PG **contract cutover
  is partitioned across deploys**, so the exact statement stream depends on live
  state.
- **`backfill`** — a runtime windowed batch loop (statement stream depends on live
  row count / PK ranges).
- **existence-guarded ops** (`ifExists` / `ifNotExists`) — the apply is a runtime
  catalog probe + run / satisfied-noop / fail-drift decision. The **bare** DDL the
  apply would run when the probe says "run" *is* printed (it is real SQL), under the
  label — but no `IF [NOT] EXISTS` clause is invented (the engine emits none; the
  guard is a probe, not a native clause).
- **stand-alone SQLite `alterColumn*` / `addConstraint` / `dropConstraint`** —
  reconciled via the live 12-step rebuild, which needs the live table structure.

The preview's header and trailing `-- preview: N statement(s) rendered, M
runtime-resolved` summary make the offline-renderable subset and the labeled
remainder explicit. Both `.sql` (Flyway/dbmate) and `.ir.json` (creator) artifacts
in the directory are previewed.

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
