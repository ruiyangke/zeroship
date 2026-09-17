# `@zeroship/migrate` — the op DSL

`@zeroship/migrate` is the package you import to author zeroship database
migrations. You describe each change as structured operations in a `.ts` module;
the package records them and the platform applies them. There is no SQL string in
the authoring surface.

A migration is a single `.ts` module with exactly one forward phase:

- `schema()` records DDL. The engine derives a structural inverse, so a schema
  migration declares no reverse.
- `data()` records DML and must declare either a recorded `inverse()` or a
  non-empty `irreversible` reason.

Schema and data changes cannot share one module. Every phase function is
parameterless, synchronous, and returns `void`; calling the helpers describes the
change and does not connect to a database. PostgreSQL is the first-class target;
constructs with no realization on another target fail closed unless you supply an
explicit dialect leg (see [dialect()](#dialect-at-value-and-op-position)).

```ts
// migrations/0007_create_orders.ts
import { ids, now, table, t } from "@zeroship/migrate";

export default {
  name: "create_orders", // optional; defaults to the filename label
  schema() {
    table("orders").create({
      columns: {
        id: ids.typeId({ prefix: "ord" }).primaryKey(),
        status: t.text().notNull().default("pending"),
        created_at: t.timestamp().notNull().default(now()),
      },
    });
  },
};
```

## Module shape

A migration module is one of these three default-exported shapes:

```ts
type Migration =
  | { name?: string; schema(): void }
  | { name?: string; data(): void; inverse(): void }
  | { name?: string; data(): void; irreversible: string };
```

- `schema()` accepts DDL only. Any recorded DML is refused, and the reverse is
  derived from the structural operations.
- `data()` accepts DML only. `inverse()` is recorded independently through the
  same DSL, so the reverse is checksummed and lintable. If no safe reverse
  exists, replace it with a non-empty `irreversible` explanation.
- Every phase is parameterless and synchronous. A terminal records a plain-data
  operation; it never connects to a database.
- `up()` and `down()` are not accepted. Choose `schema()` or `data()`
  explicitly; a data reverse belongs in `inverse()`. An authored phase that
  returns a promise (an `async` function) is refused with
  `ASYNC_PHASE_UNSUPPORTED`.
- `name` is optional. When omitted it falls back to the filename label.
- Authoring **outside an active phase** — at module top level, or after the phase
  returns (for example from a stray `setTimeout`) — throws `OP_OUTSIDE_RECORDER`.
  The op cannot be silently lost.
- A **selector that is never terminated** (`table("u").column("email")` with no
  terminal such as `.add()` / `.drop()` / `.rename()`) is a hard
  `SELECTOR_NOT_TERMINATED` build error when the phase ends, never a silent
  no-op. See [Selectors must be terminated](#selectors-must-be-terminated).

The former generic phase names are deliberately rejected rather than retained as
aliases.

### Data reversal is recorded or explicitly impossible

The engine never fabricates a reverse for data. A reversible data migration
records `inverse()` as its own DML stream. An irreversible data migration carries
the reason in `irreversible`; the reason appears in lint and status output so an
operator sees it before attempting rollback. Destructive DDL belongs in a
`schema()` migration, where policy and approval gates decide whether it may run.

## Authoring, ordering, and running migrations

**Authoring** uses `@zeroship/migrate`. **Running** — apply, plan, lint, status —
uses the `zero-migrate` CLI, a separate package:

```
npm install @zeroship/migrate             # what your migration files import
npm install zero-migrate-cli @zeroship/migrate pg   # PostgreSQL driver (or mysql2)
```

The CLI discovers migrations under `./migrations` (override with `--dir`) as
`*.{ts,mts,cts,js,mjs,cjs}` files, excluding `.d.ts`. **Migration order is
filename order** — the filenames are sorted and applied oldest-first, and that
sorted order is the contract both `plan` and `apply` follow. Author in that
order: `zero-migrate new <name>` scaffolds a fresh `<14-digit-timestamp>_<name>.ts`,
and the timestamp prefix (left-padded) keeps lexicographic order equal to
creation order. Never renumber or rename an already-applied file — a name is the
migration's journal identity, so a rename reads as a new version or a missing
one.

The command surface:

```
zero-migrate new <name>     scaffold a migration          (offline)
zero-migrate lint           DB-free validation, all dialects (offline)
zero-migrate plan           render pending SQL against the live database
zero-migrate apply          apply pending migrations, oldest first
zero-migrate status         reconcile the set against the applied journal
zero-migrate history        print the applied migration audit trail (~PostgreSQL)
zero-migrate rollback       undo applied versions (--to/--steps/--all)
zero-migrate resolve        resolve an online-rename pending contract
zero-migrate baseline       adopt an already-migrated database
```

`plan`, `apply`, `status`, `history`, and `rollback` read `--database-url`
(or `DATABASE_URL` / a configured `zero-migrate.toml` environment). Live
commands require at least one table-shape policy file passed with `--policy`
(there is no embedded default); destructive steps (deletes, backfills)
also require `--approve`.

The CLI (and the dev/build type-generation tooling) evaluates each exported
phase and records its operations into an ordered list; that recorded list is
what `lint`, `plan`, `apply`, and `status` operate on. An op authored outside a
phase is refused at that step (`OP_OUTSIDE_RECORDER`), never silently dropped.

A migration set is **validated** before it runs: `lint`
(and the validate step that `plan` and `apply` impose) checks the recorded
operations against every supported dialect and returns the structured error
envelope described under [Error and finding envelopes](#error-and-finding-envelopes).

## Core Entry Points

The portable authoring surface is reached through direct named exports from
`@zeroship/migrate`. There is no flat op vocabulary and no `op.` prefix.

```ts
import { table, view, enumType, comment, t, now, uuidV4 } from "@zeroship/migrate";
```

The principal exports are:

| Export | Purpose |
| --- | --- |
| `table` | table DDL/DML entry — returns the reusable `TableHandle` |
| `view` | cross-dialect view entry — returns a `ViewHandle` |
| `enumType` | portable enum entry — returns an inert `EnumHandle`; `.create({ values })` records |
| `comment` | standalone structured object comments |
| `check` | a named table-level `CHECK` definition for `create({ checks })` |
| `t` | the immutable column-type lexicon |
| `ids` | validated TypeID and ULID text-column formats |
| `perRow` | apply-time ID generators for `backfill({ set })` — `perRow.uuidV4()` / `.uuidV7()` / `.ulid()` / `.typeId({ prefix })` (see [Value-constructor signatures](#value-constructor-signatures)) |
| `dialect` | per-dialect value or whole-op escape hatch |
| `fromDb` | the `@zeroship/db` field → migration `ColumnDef` bridge |
| `lintDeterminism` | the best-effort determinism source scan |

Value constructors and helpers used in expression slots:

| Export | Purpose |
| --- | --- |
| `now()` · `uuidV4()` · `uuidV7()` · `genRandomUuid()` | database-evaluated apply-time values |
| `currentSetting(name, opts?)` · `currentUser()` | database session values |
| `interval(duration)` | a structured interval value |
| `concatWs(sep, ...parts)` | NULL-skipping concatenation (the one receiver-less scalar helper) |
| `countStar()` | receiver-less `COUNT(*)` |
| `lit(value)` | an explicit literal node |
| `nextval(name, opts?)` | a sequence-backed default |
| `int64(value)` · `decimal(value)` · `byteValue(bytes)` | widened scalar carriers |
| `minValue` · `maxValue` | ordered partition-bound sentinels |

### Value-constructor signatures

The helpers above take the following arguments and return the following values:

- `now()`, `uuidV4()`, `uuidV7()` — return a **database-evaluated apply-time
  value** (an expression node, not a frozen scalar). Use them in a column
  default or DML value; the database computes them at apply time. `uuidV4` /
  `uuidV7` are RFC 9562 UUIDv4 / UUIDv7 respectively. `genRandomUuid()` records
  the **same UUIDv4 node as `uuidV4()`**; prefer `uuidV4()`.
- `currentSetting(name, opts?)` — the SQL session value of setting `name`;
  `opts` is `{ missingOk?: boolean }` (the `missing_ok` flag on the underlying
  `current_setting`).
  `currentUser()` returns the SQL session user, with no arguments.
- `interval(duration)` — a structured interval from `duration`, an object of
  optional non-negative integer fields `{ years?, months?, days?, hours?,
  minutes?, seconds? }` (at least one required).
- `literals`: `lit(value)` builds an explicit literal node from a
  `string | number | boolean | null` (or an `int64`/`decimal`/`byteValue`
  carrier); bare JS values auto-wrap in most expression slots, so `lit` is for
  the rare slot where you must be explicit.
- `nextval(name, opts?)` — a sequence-backed **column default** referencing the
  sequence `name`; `opts` is `{ schema?: string }`. Use it in `.default(...)`,
  not as a DML value.
- `int64(value)`, `decimal(value)`, `byteValue(bytes)` — **widened scalar
  carriers** for row values and defaults where a shift beyond JS-safe-number or
  a precise decimal/byte spelling is load-bearing:
  - `int64(value: bigint | string)` — an exact signed 64-bit integer (validates
    against the signed-64 range).
  - `decimal(value: string)` — an exact decimal from a well-formed decimal
    string (for example `decimal("0.01")`); the way to carry an integer beyond
    2^53 or a fixed-scale numeric.
  - `byteValue(bytes: Uint8Array | string)` — bytes from a `Uint8Array` or a
    base64 string. A bare `Uint8Array` row value is accepted and normalized to
    the same carrier.
  - All three record a tagged, normalized IR scalar; a raw JS `bigint` is
    rejected (use `int64(...)`).
- `minValue`, `maxValue` — **sentinel constants** (not functions) marking the
  unbounded low / high end of an ordered partition bound; pass them as the
  `from`/`to` elements of `partition(name).create({ from: [minValue], to: [...] })`
  (and the other partition-bound args).
- `perRow.uuidV4()` / `.uuidV7()` / `.ulid()` / `.typeId({ prefix })` — apply-time
  generator intents valid **only** inside `backfill({ set })`; each is evaluated
  once per affected row at apply time. They are not scalars, column defaults, or
  runtime ID generators (see [Table data — direct named DML](#table-data--direct-named-dml)).

### The three CHECK shapes

`check` exists in three cooperating spellings; they are the same concept at
different positions:

- **Inline** in `create({ checks: [{ name, expr }] })` — the field form
  (see [The table itself](#the-table-itself)).
- **Top-level** `check(name, expr)` — builds that same `{ name, expr }` object
  for reuse, usually before passing it to `create({ checks })`.
- **Selector method** `table(name).check(name).add({ expr })` — adds a named
  `CHECK` to an *existing* table (see
  [Constraints](#constraints--per-kind-add-name-keyed-drop)).

`check` is not a top-level fluent selectable on its own; the table-handle method
is the only add-path, and the top-level function is a data builder.

Postgres-vendor ops are first-class root exports too: `schema`, `extension`,
`role`, `sequence`, `domain`, `grant`, `revoke`, `createFunction`,
`dropFunction`, `dropOwnedBy`, and `raw`. They are capability-gated: a confined
creator migration that reaches them receives `VENDOR_OP_DENIED`. `index` /
`foreignKey` / `check` / `unique` stay fluent methods on the table handle; they
are not top-level exports.

`table(name, { schema? })` returns a handle whose methods are the whole DDL+DML
surface (see [The `table()` surface](#the-table-surface)). The handle's terminals
record eagerly and return the handle, so calls chain and a handle is reusable
across statements (see [Var-assign + reuse](#var-assign--reuse)).

`view(name, opts?)` returns a structured select builder by default. Its query
callback supports `from`, `select`, `join` / `innerJoin` / `leftJoin`, `where`,
`groupBy`, `having`, `orderBy`, and `limit`; `groupBy` accepts column names or
expressions, and `having` may use aggregate expressions such as `countStar()`,
`col("amount").sum()`, or PostgreSQL aggregates like
`col("name").stringAgg(", ")`. A raw view body remains available for constructs
outside the structured view surface.

There is **no scalar-function namespace**: scalar functions with a natural
receiver are chain methods on the expression chain (see
[The fluent expression surface](#the-fluent-expression-surface)). Aggregates
follow the same receiver-first shape (`col("x").sum()`,
`col("x").count({ distinct: true })`). The PostgreSQL aggregates
`stringAgg(delimiter)`, `arrayAgg()`, `boolAnd()`, and `boolOr()` are chain
methods but validate fail-closed off PostgreSQL (`DIALECT_UNSUPPORTED`) unless
the value is wrapped in `dialect(...)`. `jsonb_agg`, aggregate-local `ORDER BY`,
and aggregate `FILTER` clauses are outside the current surface.

## The column-type lexicon (`t.*`)

Every column-type position (`create`'s `columns`, `.column().add()`,
`.column().rename()`, `.column().setType()`) takes a chainable `ColumnDef`
produced by the fluent `t.*` lexicon. **Columns are nullable by default**;
`.notNull()` is the rarer, riskier opt-in.

The `t.*` chain is **immutable**: every modifier returns a **fresh** `ColumnDef`
rather than mutating the receiver, so a hoisted type var is safe to reuse across
columns without aliasing (see [Var-assign + reuse](#var-assign--reuse)).

The shipped factories:

| Factory | Column type |
| --- | --- |
| `ids.typeId({ prefix })` / `ids.ulid()` | validated text storage formats; nullable and constraint-neutral until modifiers opt in |
| `t.text({ caseSensitive? })` | unbounded text |
| `t.string({ length?, caseSensitive? })` | bounded string; length defaults to 255 |
| `t.textArray()` | text array — see the note below |
| `t.char({ length })` | fixed-length character string |
| `t.smallInt()` | 16-bit integer |
| `t.int()` | 32-bit integer |
| `t.bigInt()` | 64-bit integer |
| `t.real()` | single-precision float (float4) |
| `t.double()` | double-precision float (float8) |
| `t.numeric({ precision?, scale? })` | fixed-precision decimal (default `(38, 9)`) |
| `t.boolean()` | boolean |
| `t.timestamp()` | timestamp |
| `t.date()` | SQL date |
| `t.uuid()` | uuid |
| `t.inet()` | IP network/address |
| `t.bytes()` | byte array |
| `t.json()` | json |
| `t.vector({ dimensions, metric? })` | a vector column; `metric` pins the distance metric — see [Sensitive-data facets](#sensitive-data-facets) |
| `t.geoPoint()` | a geo point |
| `t.enum(name)` / `t.domain(name)` | a reference to a declared enum or domain type |
| `t.encrypted({ of })` | an application-level encrypted column wrapping an inner type |

> `t.textArray()` records a PostgreSQL `text[]` column (JSON text on
> non-PostgreSQL targets). The descriptor type it emits is **rejected by the
> runtime database validator**, so it cannot back an app table. Use `t.json()`
> to store a list of strings.

> The `string` / `integer` / `float` aliases and the `t.X({ notNull, default })`
> options-bag overload are **removed**. Use the canonical `t.text()` / `t.int()`
> and the chain (`t.text().notNull().default("pending")`).

Chainable modifiers, each returning a fresh `ColumnDef`:

| Modifier | Effect |
| --- | --- |
| `.notNull()` | mark `NOT NULL` |
| `.default(value)` | a typed scalar literal, `now()` / `uuidV4()`, **or** a function-expression callback for composed defaults — never raw SQL |
| `.primaryKey()` | mark the table primary key (implies `NOT NULL`; the single-column shorthand) |
| `.unique()` | add a single-column `UNIQUE` |
| `.references(table, column, options?)` | a typed single-column foreign key — keeps this column's storage type and adds the target `{ table, column }` (+ optional `onDelete` / `onUpdate` / `name` / `relation`) |
| `.generated(expr, { virtual? })` | a generated/computed column; stored by default, `{ virtual: true }` on SQLite |
| `.identity({ always? })` | a SQL identity column; `BY DEFAULT` unless `always: true` |
| `.autoIncrement()` | portable auto-increment intent for integer columns (sugar for `.identity()`) |
| `.collation(intent)` | pin how the column compares, as a closed intent token — see [Column collation](#column-collation) |
| `.mask({ kind, classification? })` | declare a standalone column mask — see [Sensitive-data facets](#sensitive-data-facets) |

```ts
import { ids, table, t } from "@zeroship/migrate";

export default {
  schema() {
    table("orders").create({
      columns: {
        id: ids.typeId({ prefix: "ord" }).primaryKey(),
        total: t.numeric({ precision: 12, scale: 2 }).notNull().default(0),
        status: t.text().notNull().default("pending"),
        customer_id: ids.typeId({ prefix: "cus" })
          .notNull()
          .references("customers", "id", { relation: "customer" }),
        owner_id: t.uuid().notNull().references("users", "id", { onDelete: "cascade" }),
      },
    });
  },
};
```

`.references(...)` is a column **facet**: the column keeps the explicit storage
type or validated ID format you chose, and the facet records the full target
identity plus the optional referential actions and an explicit constraint name
(absent ⇒ `<table>_<column>_fkey`). Both halves of the target are required — a
missing target column is an `OP_INVALID` at authoring time, never a silently
table-only reference. There is no untyped `t.ref()` shortcut.

`relation` declares the ORM navigation name: the example exposes
`.with({ customer: true })` while keeping `customer_id` as the scalar foreign
key. The name must be a nonreserved output identifier, unique within its
collection, and cannot collide with a column. `name` independently names the SQL
constraint.

The reference facet is **create-table only**: `.column().add()`, `.setType()`,
and nested type positions (`t.encrypted({ of })`, a domain's base) reject a
`.references()` definition rather than dropping the reference. Add a foreign key
to an existing table with `.foreignKey(name).add({ columns, references })`,
which is also the only shape for a **composite** key.

### Sensitive-data facets

Three column facets carry intent the database catalog cannot fully recover. Each
is **closed** (an out-of-set token is rejected with `OP_INVALID` at authoring
time) and is checksum-neutral when absent.

**`ids.typeId({ prefix })` and `ids.ulid()` — validated text formats.** These
builders select storage and validation only. They do not imply `NOT NULL`, a
primary key, a default, or a generator; opt into those ordinary column facets
explicitly. A TypeID prefix is validated at author time and carried in the
schema metadata because the catalog cannot recover it.

**`t.vector({ dimensions, metric })` — vector distance metric.** Pins the
ivfflat/hnsw operator class. Closed set: `cosine | l2 | innerProduct`.
Declared-only.

**`.mask({ kind, classification? })` — standalone column mask.** The field reads
back as `MaskedValue<T>`. `kind` is **required**; `classification` is optional
and **defaults to `"pii"`**. An explicit `.mask()` on an encrypted column
overrides the encryption's automatic mask.

| Facet | Closed token set | Default |
| --- | --- | --- |
| mask `kind` | `full \| last4 \| first4 \| email \| name \| date-year \| date-decade \| none` (`none` = opt-out) | — (required) |
| mask `classification` | `public \| pii \| spi \| phi \| pci \| internal` | `pii` |
| vector `metric` | `cosine \| l2 \| innerProduct` | engine default |

```ts
import { ids, table, t } from "@zeroship/migrate";

export default {
  schema() {
    table("documents").create({
      columns: {
        id: ids.typeId({ prefix: "doc" }).primaryKey(),
        embedding: t.vector({ dimensions: 1536, metric: "cosine" }),
        ssn: t.text().mask({ kind: "last4", classification: "pci" }),
        email: t.text().mask({ kind: "email" }), // classification defaults to "pii"
      },
    });
  },
};
```

These facets also ride on `.column().add({ type })`; none silently disappears
from an added column. They survive into the generated types: the typed-id
`prefix`, the vector `metric`, and the `mask` brand flow into the generated
`env.db.ts` (see
[gen-types](#generating-types-from-the-migration-set-gen-types)).

## Column collation

**`.collation(intent)` — pin how the column compares.** It takes a closed
*intent* token, never a SQL collation name: a name is dialect-private, so the
engine spells `bytewise` as PostgreSQL `COLLATE "C"`, SQLite `COLLATE BINARY`
and MySQL `utf8mb4_0900_bin`. Reach for it where byte order is load-bearing —
cursor ranges, identity copies, and any comparison that must not move when the
database's default collation does.

| Facet | Closed token set | Default |
| --- | --- | --- |
| `collation` | `bytewise` | absent (the database's own collation) |

```ts
table("job_receipts").create({
  columns: {
    id: t.text().collation("bytewise").primaryKey(),
    app_id: t.text().collation("bytewise").notNull(),
    specification: t.text().notNull(),
  },
});
```

It is **refused**, not dropped, in four situations: on a type that cannot carry
a collation (anything but `t.text()` / `t.string()`); alongside
`caseSensitive: false`, which asks for the opposite ordering; alongside a value
format (`ids.typeId()` / `ids.ulid()`), which pins bytewise comparison as part
of its storage contract; and outside `table(...).create({ columns })` —
`.column().add()`, `.setType()`, `.rename()` and nested type positions have no
slot for the facet.

## The `table()` surface

`table(name, { schema? })` returns a table handle. Everything — DDL and DML — is
a method (or a selector terminal) on that handle. Every terminal takes **exactly
one named-object** argument (identity — the table name and a selector name — is
positional; payload + options are a named object), records eagerly, and
**returns the handle** so calls chain.

### The table itself

```ts
table("audit_log").create({
  columns: {
    id: ids.typeId({ prefix: "evt" }),
    org_id: ids.typeId({ prefix: "org" }).notNull(),
    email: t.text().notNull(),
    role: t.text().notNull().default("member"),
  },
  primaryKey: ["org_id", "email"], // composite PK; use `.primaryKey()` for one column
  uniques: [{ name: "members_org_email_uq", columns: ["org_id", "email"] }],
  checks: [{ name: "members_role_nonempty", expr: (col) => col("role").ne("") }],
  foreignKeys: [
    {
      name: "members_org_fk",
      columns: ["org_id"],
      references: { table: "orgs", columns: ["id"] },
      onDelete: "cascade",
    },
  ],
  indexes: [{ name: "members_org_idx", on: ["org_id"] }],
});

table("scratch").drop({ ifExists: true, cascade: true });

table("accounts").rename({ to: "members" }); // ALTER TABLE … RENAME TO … (PG + SQLite)
```

`create({...})` is the one all-object form (no `build` callback): table-level
constraints and indexes are **fields**, and each carries a **required `name`**
(name-first, so a later migration can deterministically drop it).

| `create` field | What it declares |
| --- | --- |
| `columns` | required; the ordered column map |
| `primaryKey` | an ordered table-level primary key (composite keys); `null` requests no key; a column's `.primaryKey()` is the single-column shorthand |
| `uniques` | named `UNIQUE` constraints: `{ name, columns }` |
| `checks` | named `CHECK` constraints: `{ name, expr }` |
| `foreignKeys` | named, ordered foreign keys: `{ name, columns, references: { table, columns }, onDelete?, onUpdate?, deferrable?, initiallyDeferred? }` |
| `exclusions` | named exclusion constraints |
| `indexes` | named indexes: `{ name, on, unique?, using?, where?, include?, only?, nullsNotDistinct? }` |
| `options` | collection runtime (lifecycle) options — see below |
| `partitionBy` | partitioning for a partitioned table |
| `ifNotExists` | existence guard for the create |
| `schema` | overrides the handle's default schema |

Foreign-key actions are `cascade | restrict | setNull | setDefault | noAction`;
the index method set is
`btree | hash | gin | gist | spgist | brin | ivfflat | hnsw`. An unsupported
combination fails closed at build time, never silently lowers to something else.

**Lifecycle options.** `create({ options })` and `table(name).setOptions({...})`
record collection runtime metadata:

| Option | Values | Default | Effect |
| --- | --- | --- | --- |
| `softDelete` | boolean | `false` | select the injected `now`-on-delete generator as the visibility marker |
| `versioning` | boolean | `false` | select the injected increment-on-write generator for optimistic concurrency |
| `strictness` | `strict \| lenient \| off` | `strict` | recorded on the collection; does not change write validation today |

`setOptions` must set at least one of the three, or it is an `OP_INVALID`. These
options select roles; the platform-injected system columns determine the
assignments, so `softDelete` and `versioning` work with no column declaration.

`table("scratch").drop({ ifExists, cascade, schema? })` drops the table;
`table(name).comment(text)` records a comment on the table; and
`table(name).primaryKey()` changes an existing table's primary key through
`add({ columns })`, `replace({ expectedColumns, columns, dropIdentityFrom? })`,
or `drop({ expectedColumns, dropIdentityFrom? })`. `replace` / `drop` name the
expected current key as a drift precondition; they change only the key
constraint.

#### `.rename({ to })` — whole-table rename

`table(name).rename({ to, ifExists?, schema? })` records a whole-table rename.
It lowers to a single, **direct** `ALTER TABLE … RENAME TO …` on both PostgreSQL
and SQLite — a fast catalog-metadata change, **not** the online column
expand-contract (a whole table has no per-column dual-write that lets it coexist
under two names). Because the change is a pure metadata rename, the engine
derives the inverse rename, so a renaming-only migration needs no authored
reverse. `ifExists` guards the source table (an `ifExists` rename of an absent
table is a satisfied no-op). This is distinct from `.column().rename()` (the
online column rename, see [Online rename](#online-rename)).

### Columns — the `.column(name)` selector

```ts
const orders = table("orders");
orders.column("status").add({ type: t.text().notNull().default("new") });
orders.column("legacy").drop({ ifExists: true });
orders.column("label").rename({ to: "display_label", type: t.text() }); // named ⇒ no swap
orders.column("total").setType({ to: t.numeric({ precision: 14, scale: 2 }), using: (col) => col("total").cast({ to: "real" }) });
orders.column("note").dropNotNull();
orders.column("note").setDefault("memo");
orders.column("note").dropDefault();
```

`.column(name).add({ type })` honors **all** modifiers on `type`, including
`.unique()` (which emits a follow-on `UNIQUE` constraint) and `.primaryKey()` —
they are not silently dropped. The other terminals are
`.drop({ ifExists? })`, `.rename({ to, type })`, `.setType({ to, using? })`,
`.setNotNull()`, `.dropNotNull()`, `.setDefault(value)`, `.dropDefault()`, and
`.comment(text)`.

### Constraints — per-kind `.add`, name-keyed `.drop`

```ts
const members = table("members");
members.foreignKey("members_org_fk").add({
  columns: ["org_id"],
  references: { table: "orgs", columns: ["id"] },
  onDelete: "cascade",
});
members.unique("members_org_email_uq").add({ columns: ["org_id", "email"] });
table("orders").check("orders_total_nonneg").add({ expr: (col) => col("total").ge(0) });
table("orders").constraint("orders_total_nonneg").drop({ ifExists: true }); // kind-agnostic drop
```

`.foreignKey(name)` / `.unique(name)` / `.check(name)` each have one terminal,
`.add(...)`; `.constraint(name)` has `.drop(...)` (kind-agnostic, by name),
`.comment(...)`, and `.validate(...)`.

### Indexes — the `.index(name)` selector

```ts
const members = table("members");
members.index("members_email_idx").add({ on: ["email"], unique: true });
members.index("members_created_idx").add({
  on: ["org_id", { column: "created_at", order: "desc" }],
});
members.index("members_email_idx").drop();

table("members").index("members_active_email_idx").add({
  on: ["email"],
  where: (col) => col("active").isTrue(),
  include: ["id"],
  using: "btree",
});
```

Indexes are **name-first** (the selector name), so a later migration can drop
them deterministically. `.index().drop()` does not accept an author-declared
`unique` flag; the engine derives whether the target index is unique from the
schema and gates a UNIQUE-index drop as destructive (it silently removes a
data-integrity guarantee). PostgreSQL-specific index options (`using`, `where`,
`include`, `with`, `only`, `nullsNotDistinct`, per-element `opclass` /
`collation`) are authored here.

### Table data — direct named DML

```ts
const plans = table("plans");

plans.insert({
  rows: [
    { id: "free", price_cents: 0 },
    { id: "pro", price_cents: 2900 },
  ],
});

// Upsert: on a conflicting `id`, update the listed columns.
plans.insert({
  rows: [{ id: "pro", price_cents: 3900 }],
  onConflict: { columns: ["id"], doUpdate: { price_cents: 3900 } },
});

table("orders").update({
  set: { status: (col) => col("status").upper() },
  where: (col) => col("status").eq("pending"),
});

table("sessions").delete({ where: (col) => col("expires_at").lt("2026-01-01T00:00:00Z") });

table("orders").backfill({
  name: "normalize_totals", // optional; defaults to `backfill_<table>`
  set: { total_norm: (col) => col("total").coalesce(0) },
  cursorColumns: ["id"], // REQUIRED ordered cursor tuple (no default)
  cursorStability: { mode: "guardUpdates" }, // REQUIRED; or { mode: "externalInvariant", name }
  batchSize: 1000, // the engine's default; omit to keep it
});
```

- The predicate keyword is **`where` everywhere** — there is no `filter`
  synonym.
- `.delete({ where })`'s `where` is **mandatory** — an unguarded full-table
  delete is rejected at record time.
- `.insert({ rows })`'s `rows` is a **single row object or an array of row
  objects** — `{ id: 1 }` and `[{ id: 1 }, { id: 2 }]` both record. A row is a
  loose `{ column: value }` record, never bound to the live schema.
- `.backfill` requires an ordered `cursorColumns` tuple and an explicit
  `cursorStability` mode. Neither is defaulted: the cursor decides which rows a
  resume revisits, and the stability mode is the invariant that keeps the cursor
  components immutable across an interrupted apply
  (`{ mode: "guardUpdates" }` installs an engine-owned guard;
  `{ mode: "externalInvariant", name }` acknowledges an operator-owned one).
  `name` optionally names the backfill for journal/status identity; omit it and
  the engine records `backfill_<table>`.
- `.backfill` is a batched, per-batch-transactional, resumable loop that persists
  crash-safe cursor progress. `batchSize` defaults to the engine's default
  (`1000`); pass a different positive integer to tune a specific backfill.
- A backfill `set` value may additionally be a `perRow.*` generator
  (`perRow.uuidV4()` / `.uuidV7()` / `.ulid()` / `.typeId({ prefix })`),
  evaluated independently for every affected row. These are reserved to
  `backfill({ set })` — they are not accepted as scalars, column defaults, or
  runtime ID generators (see [Value-constructor signatures](#value-constructor-signatures)).
- Row values may be a string / safe number / boolean / `null` / a widened
  carrier `int64(...)`, `decimal("…")`, `byteValue(...)`, or a bare
  `Uint8Array`. Use `int64(...)` or `decimal("<n>")` for integers beyond 2^53 or
  fixed-scale numeric values. A `Uint8Array` (or `byteValue(...)` input) is
  normalized to a base64 carrier before recording; a raw JS `bigint` is rejected.
- DML carries **no existence guard** (it is not guardable); `schema` rides on the
  args object.
- `.insert({ rows, onConflict })` is a structured upsert. `onConflict.doUpdate`
  renders on PostgreSQL, SQLite, and MySQL 8. On MySQL the conflict target must
  exactly match one full primary or unique index, every target column must be
  present in `rows`, and the target column must not be assigned from `doUpdate`.
  Targeted do-nothing has no exact MySQL form and is refused there.

## The `schema` qualifier (profile-gated)

Every table-targeting op accepts an optional `schema` (a plain identifier
string). It selects the schema the op renders into. Its meaning is
**profile-gated**:

- **Platform creator deploy (confined):** the project schema is **pinned**. An
  op that omits `schema`, or names the project schema, is fine; an explicit
  `schema` that differs from the project schema is **refused, fail-closed**,
  with `CROSS_SCHEMA` — before any SQL runs.
- **Platform-internal:** an explicit `schema` must be a member of the configured
  platform schema allow-list, else `CROSS_SCHEMA`.
- **Standalone / trusted CLI:** the qualifier is honored.

The default schema, when an op omits its own, is set by `table(name, { schema })`
and overridden per op; with neither, the connection default applies. On SQLite
the implicit target is `main`, and any other schema is refused.

The schema string is an identifier the engine double-quotes, so it is also
**validated for injection** (`INVALID_SCHEMA_IDENT`): it must be a non-empty,
alpha- or `_`-leading bare identifier of `[A-Za-z0-9_]`. An injection-shaped
value (an embedded quote, `"; DROP …`) is rejected on every profile.

```ts
// Trusted CLI: render into a non-default schema — set once on the handle.
const reporting = table("audit_log", { schema: "reporting" });
reporting.create({ columns: { id: t.uuid().primaryKey() } });
table("widgets", { schema: "reporting" }).insert({ rows: { id: 1 } });
// Confined creator deploy: a cross-schema op is refused fail-closed.
table("other_app_table", { schema: "some_other_app" }).drop(); // → CROSS_SCHEMA
```

## Existence guards (`ifExists` / `ifNotExists`)

The create/add family (`.create`, `.column().add`, `.index().add`,
`.foreignKey` / `.unique` / `.check().add`) carries an `ifNotExists` option; the
drop family (`.drop`, `.column().drop`, `.index().drop`, `.constraint().drop`)
carries an `ifExists` option. A guard on the wrong family is a `GUARD_DIRECTION`
authoring error.

The guard is honored at apply time. The default semantic is
**shape-verify-or-fail**, never a bare skip:

- `ifNotExists`, object **absent** → the op runs.
- `ifNotExists`, object **present and its shape matches** the declared op → a
  journaled **satisfied** no-op (the migration still records a completed
  version).
- `ifNotExists`, object **present but its shape differs** (for example a column
  that exists with a different type) → **fails closed** with a drift error
  naming the divergence. It is never a silent skip over a divergent object.
- `ifExists`, object **present** → the drop/alter runs.
- `ifExists`, object **absent** → a journaled satisfied no-op.

A shape that cannot be fully verified fails closed rather than optimistically
running: for example, a constraint guarded by `ifNotExists` whose live
definition cannot be proven byte-equal to the declared one is refused, and a
partial or expression index guarded by `ifNotExists` is refused because
equivalence is unprovable. An `ifExists` guard is not accepted on a column
rename; the rename already requires the source column to exist.

### SQLite-safe rebuild (automatic)

A SQLite `ALTER` that SQLite cannot do in place (drop a column on an old SQLite,
re-type, a stand-alone constraint add/drop) is lowered by the engine to the
12-step table rebuild automatically — there is no author-facing
`batchAlterTable` grouping. Author the column/constraint changes as ordinary
`.column()` / `.constraint()` terminals; the engine groups the rebuild per table
at lower time.

## Selectors must be terminated

A selector (`.column(x)` / `.foreignKey(x)` / `.unique(x)` / `.check(x)` /
`.constraint(x)` / `.index(x)`) returns a sub-builder that records **only** when
its terminal (`.add` / `.drop` / `.rename` / `.alter`) is called. A forgotten
terminal would otherwise silently record nothing — so it is a **hard,
structured error**: when the phase ends, a selector handed out but never
terminated throws an error whose `code` is `"SELECTOR_NOT_TERMINATED"`, with
`selector` and `name` fields (and a `suggested_fix`). Terminating the same
selector twice throws an error with code `"SELECTOR_ALREADY_TERMINATED"`. See
[Error and finding envelopes](#error-and-finding-envelopes) for the two error
shapes.

The check runs **when the phase ends, not eagerly**, so a selector held in a
variable and terminated on a later line is fine:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  schema() {
    // FINE — terminated on a later line (the guard checks when the phase ends).
    const email = table("users").column("email");
    email.add({ type: t.text().notNull() });
    // ERROR — `table("users").column("nickname")` with no terminal is a hard
    // SELECTOR_NOT_TERMINATED build error.
  },
};
```

## Error and finding envelopes

Refusals surface in **two shapes**, depending on where they are raised.

**Authoring-time errors** — thrown synchronously while the migration module runs
or the recorder drains. These are JavaScript `Error` objects (not JSON) carrying
a `code` string property plus contextual fields. The codes raised here are:

- `OP_INVALID` — a structurally invalid argument, facet, or value.
- `OP_OUTSIDE_RECORDER` — an op recorded outside an active phase.
- `SELECTOR_NOT_TERMINATED` and `SELECTOR_ALREADY_TERMINATED` — a selector ended
  unterminated or was terminated twice; both carry `selector` and `name` fields.
- `ASYNC_PHASE_UNSUPPORTED` — a phase returned a promise; carries `suggested_fix`.
- `COLTYPE_UNSUPPORTED` — a `fromDb` field has no portable column type; carries
  the offending `dbType`.

Read `err.code` for the code and `err.suggested_fix` (when present) for the
remedy; other fields name the offending selector, name, or type. `OP_INVALID` is
also raised by the validate stage for structurally-valid-but-inconsistent
hand-authored envelopes, where it uses the JSON shape below instead.

**Validate/render errors** — raised when `lint`, `plan`, or `apply` checks the
recorded operations against each supported dialect. These arrive as a structured
JSON object. The canonical envelope is:

```jsonc
{
  "suggested_fix": "…", // optional; the remedy, leads human rendering
  "code": "EXPR_NOT_PORTABLE",
  "kind": "expr",       // present only for code "UNSUPPORTED": "op" | "expr"
  "op_index": 2,        // 0-based index of the offending op in the op list
  "dialect": "sqlite",  // the dialect the rejection pertains to
  "reason": "…"         // a precise human-readable reason
}
```

Codes in this envelope include `UNSUPPORTED` (with `kind: "op" | "expr"`),
`EXPR_NOT_PORTABLE`, `DIALECT_UNSUPPORTED`, `CROSS_SCHEMA`,
`INVALID_SCHEMA_IDENT`, `GUARD_DIRECTION`, `VENDOR_OP_DENIED`, and the
column-facet / sequence / partition codes named throughout this page. A source
location (the offending migration file and position) is carried alongside when
the engine knows it, as in the `EXPR_NOT_PORTABLE` example under
[Out of envelope is a hard error](#out-of-envelope-is-a-hard-error-not-a-silent-mis-apply).

**Advisory findings** are neither thrown nor fatal. `lintDeterminism(source)`
returns a list of finding objects `{ code: "NONDETERMINISTIC_OP_ARG", accessor,
suggested_fix, reason }` — a warning you act on, never a hard reject.

## Var-assign + reuse

Both authoring styles are first-class — pick per readability. Every terminal
**returns the handle**, so calls chain; and the handle is a reusable value
(carrying only its name and default schema), so it can be assigned once and
reused across statements with `{ schema }` set a single time:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  schema() {
    // chained
    table("users")
      .column("a").add({ type: t.text() })
      .column("b").drop({ ifExists: true });

    // var-assigned (DRY; { schema } set once)
    const users = table("users", { schema: "app" });
    users.column("email").add({ type: t.text().notNull() });
    users.unique("uq_email").add({ columns: ["email"] });
  },
};
```

The `{ schema }` passed to `table()` is the **default schema** stamped onto every
op the handle records; a per-op `schema` on the terminal's args **overrides** it
for that one call, and an args bag that omits `schema` keeps the default (an
absent key never wipes it). `table("users")` with no schema records ops with
**no** `schema` key.

Because the `t.*` chain is **immutable** (every modifier returns a fresh
`ColumnDef`), a hoisted type var is safe to reuse across columns:

```ts
import { table, t } from "@zeroship/migrate";

export default {
  schema() {
    const reqText = t.text().notNull(); // hoisted, reusable
    table("users")
      .column("email").add({ type: reqText.unique() }) // email is UNIQUE
      .column("name").add({ type: reqText }); // name is NOT unique — reqText untouched
  },
};
```

## C1 — foreign-key referential actions

A `.foreignKey(name).add({...})` (and a `create({ foreignKeys: [...] })` entry)
takes optional `onDelete` / `onUpdate` of
`cascade | restrict | setNull | setDefault | noAction`, and they are **actually
rendered** (`ON DELETE CASCADE`, …). An action-free FK records no action clause:

```ts
import { table } from "@zeroship/migrate";

export default {
  schema() {
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
has no inline `UNIQUE`, so a `t.*.unique()` / `t.*.primaryKey()` on an added
column records the column **plus** a follow-on constraint (it is not silently
dropped):

```ts
import { table, t } from "@zeroship/migrate";

export default {
  schema() {
    // records an addColumn AND a follow-on UNIQUE constraint on "email".
    table("users").column("email").add({ type: t.text().notNull().unique() });
  },
};
```

## The fluent expression surface

Every expression position — a DML `set` value, a `where`, a
`check(name).add` body, a partial-index `where:` — is a callback `(col) => Expr`
with a **single injected builder handle**. It is never a raw string; it
constructs a node of a closed AST through an all-strings fluent builder.

**`col` is both a column accessor and the function namespace.** `col("first")`
returns an unqualified column reference; `col("table", "col")` returns a
qualified one. Arguments are plain strings; there is no dotted-string form like
`col("other.col")` (cross-table references remain limited by
[the portability boundary](#the-dml-portability-boundary)).

**Chainable operator methods** (each builds one closed-AST node; a bare JS value
passed to a method auto-wraps to a literal and is bound as a parameter, never
interpolated):

- comparison: `.eq(x)`, `.ne(x)`, `.lt(x)`, `.le(x)`, `.gt(x)`, `.ge(x)` —
  `null` is rejected here; use `.isNull()` / `.isNotNull()`
- predicates: `.and(...es)`, `.or(...es)`, `.not()`, `.between(low, high)`,
  `.like(pattern)`, `.in(values)`, `.notIn(values)`, `.distinctFrom(x)`
- arithmetic: `.add(x)`, `.sub(x)`, `.mul(x)`, `.div(x)`
- string/value: `.concat(...parts)` (raw `||`, NULL-propagating). The
  NULL-skipping `concatWs` is a top-level import; `coalesce` is a chain method.
- null/bool tests: `.isNull()`, `.isNotNull()`, `.isTrue()`, `.isFalse()`
- cast: `.cast({ to: "text" | "int" | "real" | "boolean" | "bytes" | "uuid" })`
  (the closed scalar target set only)
- PostgreSQL-first: `.regex(pattern)` and `.columnSize()` fail closed on other
  targets (`DIALECT_UNSUPPORTED`)

**Scalar chain methods + top-level `concatWs`**:

- `e.lower()`, `e.upper()`, `e.trim()`, `e.length()`, `e.abs()`
- `e.coalesce(...rest)`, `e.nullif(b)`
- `e.mod(b)`, `e.round(n?)`, `e.floor()`, `e.ceil()`, `e.substr(start, len?)`,
  `e.replace(from, to)`, `e.extract(field)`, `e.splitPart(delim, n)`
- `concatWs(sep, ...parts)` — NULL-skipping concatenation, the safe form for
  joining first+last name. Rendered byte-identically across PostgreSQL
  (`concat_ws`) and SQLite (a proven `coalesce`-folded `||`). For an
  empty-string join use `concatWs("", …)`.
- `col.case({ branches: [{ when: cond, then: val }, …], else?: elseVal })` —
  the searched `CASE` form

**Aggregates** (receiver-first):

- `e.count({ distinct? })`, `e.sum({ distinct? })`, `e.avg({ distinct? })`,
  `e.min({ distinct? })`, `e.max({ distinct? })`, plus the top-level
  `countStar()` for `COUNT(*)`
- PostgreSQL-first: `e.stringAgg(delimiter)`, `e.arrayAgg()`, `e.boolAnd()`,
  `e.boolOr()` fail closed on other targets unless wrapped in `dialect(...)`

**Top-level value constructors**:

- `now()`, `uuidV4()`, `uuidV7()` — database-evaluated apply-time values. Use
  these instead of baking a build-time `Date.now()` / UUID literal into the
  artifact. As an ergonomic shorthand, the **bare native symbol** (no parens)
  `Date.now`, `Math.random`, or `crypto.randomUUID` used as an op value records
  the identical database-evaluated node — `Date.now` ⇒ `now()`,
  `Math.random` / `crypto.randomUUID` ⇒ `uuidV4()`.

The expression is recorded as **dialect-neutral data, never SQL** — the engine
owns all per-dialect lowering, so the same migration behaves the same way across
backends. There is no author-named `split_part` / `instr`: those cross-dialect
semantics diverge, so the split surface is the built-in `.splitPart(...)` chain
method.

### Determinism: don't bake a clock or RNG into a migration

A migration is recorded whenever build/gen-types needs the operation stream, so
a **called** `Date.now()` / `Math.random()` / `crypto.randomUUID()` /
`new Date()` in an op argument freezes a build-time value for that recording
(almost never what you want). The package handles this by translation, not by a
gate:

- The **bare native symbol** (no parens) — `Date.now`, `Math.random`,
  `crypto.randomUUID` — records as the database-evaluated node (identical to
  `now()` / `uuidV4()`). This is the recommended way to get an apply-time value.
- A **call** (`Date.now()`) just evaluates, and the resulting scalar is recorded
  verbatim; `lintDeterminism(source)` — where `source` is the migration module's
  source text — emits an advisory **warning** finding
  (`{ code: "NONDETERMINISTIC_OP_ARG", accessor, suggested_fix, reason }`)
  steering you to the symbol / constructor form. It is a coarse whole-source
  scan (it over-flags, never under-flags) and is advisory-only, never a hard
  reject.
- A **function value** (native symbol or otherwise) nested inside a container /
  JSON op value is rejected fail-closed (a function cannot be database-evaluated
  inside a JSON literal), as is a non-native function used directly as an op
  value.

## The DML portability boundary

Portability is real but **bounded**, and the boundary is honest. The engine owns
control flow and statement assembly; you own the data-transform expression — but
you express that transform only through the closed fluent AST, never raw SQL.

**Portable — works on both PostgreSQL and SQLite from one op:**

- `insert` with literal rows.
- `delete` with a `where`.
- One-shot `update` / `backfill` whose `set` / `where` use only the closed
  fluent AST: column refs, auto-wrapped literals, arithmetic,
  comparison/boolean operators, `col.case`, the allow-listed provably-identical
  scalars (`coalesce`, `nullif`, `lower`, `upper`, `trim`, `length`, `abs`,
  `.cast({ to })`, `.concat`), and `concatWs`.
- The built-in `.splitPart` helper **within its pinned envelope**.

### The `splitPart` / `concatWs` portable-expression envelope

`col.splitPart(delim, n)` lowers to `split_part(col, 'd', n)` on PostgreSQL and
to a pinned `instr` / `substr` expression on SQLite, proven byte-identical to
PostgreSQL. The full portability envelope — admitting it on **both** backends —
is:

- `delim` is a literal, **non-empty, single ASCII character** (one byte, code
  point < 0x80);
- `n` is a literal **positive integer** — and on the SQLite leg, `1 ≤ n ≤ 8`
  (the inline unroll grows quickly; past 8 it can exceed SQLite's
  expression-depth limit);
- `col` is a column ref or an in-AST sub-expression.

The value being split *may* contain multibyte UTF-8 content — it is the
**delimiter** that is constrained to single-ASCII, because an ASCII byte never
occurs inside a UTF-8 multibyte sequence, which is why the byte-wise SQLite scan
finds the same boundaries as PostgreSQL's character-wise `split_part`.

`concatWs(sep, …)` is the NULL-skipping join, rendered byte-identically on both
backends. Prefer it over `.concat(...)` for joining values: `.concat` maps to
`||`, whose NULL rule is documented (a NULL operand yields NULL on both
backends) — fine when you want propagation, a footgun when you don't.

### Out of envelope is a hard error, not a silent mis-apply

A `.splitPart` call outside the envelope — a multi-character / empty / non-ASCII
delimiter, `n = 0`, negative `n`, `n > 8`, or non-literal args — is a hard
`EXPR_NOT_PORTABLE` error on the SQLite leg. The clearly-malformed shapes (empty
delimiter, non-positive / non-integer `n`) are caught earlier, at record time.
It is **never** silently mis-split. The structured error names the two real
resolutions:

```jsonc
{
  "suggested_fix": "use a single-ASCII delimiter with 1<=n<=8, restructure to stay in-envelope (split into <=8 parts), or mark the migration PG-only (dialect_scope=PgOnly)",
  "code": "EXPR_NOT_PORTABLE",
  "op_index": 2,
  "ts_location": "migrations/0007_split_name.ts:9",
  "dialect": "sqlite",
  "reason": ".splitPart is portable only for a single-ASCII delimiter and a positive literal n in 1..8; this call is out of envelope"
}
```

## No raw-SQL escape hatch (property A)

There is no raw-SQL **expression** surface — no raw expression type, no
SQL-tagged template literal, no string fragments — on either dialect. Where a
transform is dialect-divergent
and not expressible in the closed AST (an exotic PostgreSQL-only function, a
subquery/window, a cross-table reference, an out-of-envelope split), there is
**no raw fallback**. It surfaces as a hard structured error
(`UNSUPPORTED` with `kind: "expr"` / `"op"`, or `EXPR_NOT_PORTABLE`), and the
only resolutions are:

1. **Reshape** the migration into the portable surface (for example split into
   ≤ 8 parts), or
2. **Accept a PostgreSQL-only reach.** Author the PostgreSQL form — typically a
   `dialect({ postgres: … })` leg with no other leg and no `default` leg — and
   leave the divergent construct out of the SQLite/MySQL surface. The engine
   *derives* the migration's reach (`dialect_scope`, PostgreSQL-only) from the op
   list and the engine's allow-list version; you never declare it as a string.
   The consequence you accept is that such a migration is not authorable on the
   SQLite dev tier: applying it against a non-PostgreSQL target is refused at
   apply, never silently mis-applied on one backend.

The one deliberate whole-statement escape is `raw({ sql, reason })`, a
reason-required, trust-gated DDL operation for operator migrations. It carries
its reason with the operation. A confined creator migration cannot use it and
receives `VENDOR_OP_DENIED`.

The expressible surface is the supported surface. This is what keeps "one
script, both backends" honest: a migration that passes the both-backends dry-run
applies faithfully on both; a migration that cannot, fails loudly at
authoring/render time, not silently at runtime on one backend.

## `dialect()` at value and op position

`dialect(legs)` is the explicit portability escape. It has two modes, selected
by the leg values:

- **Expression/value position**: legs are expression values. A target with no
  own leg and no `default` leg is a hard portability error
  (`EXPR_NOT_PORTABLE`). Use this inside defaults, predicates, generated
  expressions, DML values, and other expression slots.
- **Statement/op position**: legs are thunks. Each present thunk runs and the
  ops it records are grouped under that backend id, emitted as one target-aware
  operation. A target with no own leg and no `default` leg skips the op
  entirely.

Backend ids are lowercase identifiers; the supported targets are `postgres`,
`sqlite`, and `mysql` (a `default` leg is also accepted).

```ts
import { dialect, table } from "@zeroship/migrate";

dialect({
  postgres: () => table("docs").index("docs_embedding_hnsw_idx").add({
    on: ["embedding"],
    using: "hnsw",
  }),
});
```

An explicit empty thunk is a present no-op leg for that target. An absent key is
different: absent own leg plus absent `default` means "skip" for op-level
`dialect()` and "error" for expression-level `dialect()`. Mixing thunk legs with
expression-value legs in the same call throws `OP_INVALID`; an empty leg set is
also `OP_INVALID`.

Spec-level dialectal fragments, such as wrapping one index element inside
`indexes: []` or wrapping a `ColumnDef`, are not accepted. Use op-level
`dialect()` around the whole op or op sequence.

## Names are strings (and why)

**Every table/column/name reference is a plain `string`.** There is no generic
schema parameter, no binding to the live `@zeroship/db` schema. `table`,
`column`, `from`, `to`, `name`, `cursorColumns`, every `set` key, every
`where`-referenced column, and every `col("…")` argument are strings whose
existence is validated at **apply time against the real database**, never at
type-check time.

This is deliberate, and it is the single most important typing rule of the DSL.
Migration files are immutable historical artifacts: if names were typed against
the *current* schema, a migration that referenced a column a later migration
dropped would stop compiling, the whole history would become un-compilable as
the schema evolves, and authors would be tempted to edit committed migrations —
changing the operation list and triggering a drift abort. No mature migration
tool binds migration files to the live schema.

**What IS type-checked (structural safety, preserved):**

- **Op argument shapes** — you cannot pass a number where a `ColumnDef` is
  expected, or omit a required field on a terminal's args object.
- **The `t` column-type lexicon** — `t.text()` / `t.numeric()` and their
  chainable modifiers (`.notNull()` / `.default()`) are typed.
- **The fluent-expression node shapes** — the builder's methods (`.eq` /
  `.concat` / `.gt` / `.splitPart` …) have typed arities and return an
  expression; calling a non-existent operator method fails type-checking.
  (Method names are the typed builder API; every *identifier* they reference is
  still a plain string.)
- **Insert-row value shapes** — rows are a loose record by default; a caller may
  supply a generic for editor convenience, but it is never auto-derived from the
  live schema.

So the guarantee: op shapes, the `t` lexicon, expression node shapes, and value
kinds are typed; **names are validated at apply/render time, never bound to the
live schema at author time.** The part most likely to be semantically wrong (a
transform's referenced names) is exactly what the type system cannot see — you
test it with the shadow-DB dry-run.

## Bridging a `@zeroship/db` field (`fromDb`)

The migration DSL and the runtime `@zeroship/db` schema share **one** type
lexicon. `fromDb(field)` lifts a live-schema `@zeroship/db` field into a
migration `ColumnDef` through the identical column-type path. It carries the
field's physical type and nullability (`.required()` → `.notNull()`) and returns
a chainable `ColumnDef` so you can still layer migration modifiers on top. The
bridge does not turn a runtime-schema ref into a migration foreign-key
constraint: migration references use an explicit physical type plus
`.references(table, column)`. A non-storage `@zeroship/db` field (a json array,
a nested object, a union) has no portable column type and throws
`COLTYPE_UNSUPPORTED` — a hard boundary, never a silent fallback.

## Online rename

`table(t).column(from).rename({ to, type })` records a single online column
rename. The engine lowers it to a dual-dialect online change:

- **PostgreSQL** — an expand-contract flow: add the new column, install a
  dual-write trigger, backfill, then (in a later phase) drop the old column. The
  app stays up throughout.
- **SQLite** — the offline 12-step table rebuild, applied through the engine's
  rebuild path. A rebuild on a populated table is destructive, so it requires
  approval.

**What is not yet wired for routine production deploy.** Creator-app PostgreSQL
migrations are applied by the platform's migration service. The routine apply
flow uses no approval, so it refuses an online rename's approval-gated phase
before it can complete; approved migrations go through the explicit operator
approval path instead.

Production go-live gating:

1. The cross-deploy **pending-contract interlock** — the expand/contract is a
   multi-deploy flow (the contract that drops the old column owes a later
   approved deploy). This owed contract is journaled as a durable obligation and
   enforced across deploys: a completed expand records the obligation, a later
   deploy whose ops touch the pending table is refused with
   `TABLE_HAS_PENDING_CONTRACT`, an orphaned obligation is surfaced by `status`,
   and an operator resolves it explicitly.
2. **Per-version approval scoping** — the current approved surface is a coarse,
   bundle-wide approval; production needs approval scoped to the specific
   reviewed version-ids. This is the remaining gate.

Until per-version approval scoping lands, treat online `renameColumn` as a
dev/CLI capability, not a shipped production deploy path.

## Apply-time lock safety (`lock_timeout`)

The apply path runs each migration under **two separate, deliberately-split
timeouts**:

- **`statement_timeout`** (default **60s**) — how long a statement may **run**
  once it holds its lock. A runaway DDL/DML is cancelled after this.
- **`lock_timeout`** (default **3s**, short on purpose) — how long a statement
  waits to **acquire** a lock before failing fast with `55P03
  lock_not_available`. It is **NOT** folded into `statement_timeout`.

Why the split matters: on a populated, live multi-tenant table a blocking DDL
(for example an `ALTER TABLE` taking an `ACCESS EXCLUSIVE` lock) queues behind
any long-running transaction holding a conflicting lock — and because it is
itself waiting on `ACCESS EXCLUSIVE`, every subsequent query on that table
queues behind *it*. That is a tenant-wide availability outage for the lifetime
of the wait. A **short** `lock_timeout` makes the blocked DDL fail fast and roll
back cleanly (the failure is retryable, never data-corrupting), freeing the
table immediately; retry during a quieter window. A long lock-acquisition budget
would make the outage last that long.

The 3s default is the executor-wide floor. A migration that legitimately needs
to wait longer is a planned maintenance-window change and is arranged at the
policy level, not by the migration author; the conservative fail-fast default
stays in force for every other migration in the same deploy.

## Generating types from the migration set (`gen-types`)

Migration-first: your migration set is the source of truth for the schema, and
the typed `env.db` surface is **generated from it** rather than from a separate
declared schema object on the app entry. Type generation records each migration
in version order and emits two artifacts:

- **`schema.runtime.json`** — the runtime schema descriptor:
  `{ version, collections: { [name]: { fields, options, indexes } } }`. It is
  content-addressed into the deploy artifact so the runtime can read the schema
  without re-evaluating an authoring module.
- **`env.db.ts`** — a generated `@zeroship/db` schema **module** reconstructing
  the `t.*` builder calls, wrapping collections in `schema(...)` when folded
  options/indexes exist, and declaring the single `Env.db` augmentation for the
  app.

Development regenerates the artifacts on server boot and on migration changes. A
production build regenerates them in memory and fails if the committed files
have drifted.

The declared-only facets ([Sensitive-data facets](#sensitive-data-facets))
survive the fold: the typed-id `prefix`, the vector `metric`, and the `mask`
brand all flow into the generated `env.db.ts`, so `env.db.users.email` reads
back as `MaskedValue<T>` purely from the migration history. Because `env.db.ts`
is a real `.ts` module (not a `.d.ts`), the type-checker checks it like any
source file — a generated type that does not compile is a hard build failure.

Include the generated module in the app's `tsconfig.json` and do not also add
the retired declared-schema alias:

```json
{
  "include": ["src", "generated/zeroship/env.db.ts"]
}
```

Type generation runs in-process in the dev/build tooling. See
[vite-plugin.md](./vite-plugin.md#migration-first-type-generation-gen-types)
for the build/watch wiring.

> **Postgres vendor primitives.** `@zeroship/migrate` exposes direct named
> exports and one PG-first `table()` handle, not a `pg` namespace object. The
> current vendor value exports are `schema`, `extension`, `role`, `dropOwnedBy`,
> `grant`, `revoke`, `createFunction`, `dropFunction`, `domain`, `sequence`, and
> `raw`; table-scoped vendor operations are methods on `table(name)`.
> Policies are authored as `table(name).policy(policyName).create/drop(...)`,
> and row-level security with `table(name).setRls({...})`. These are
> PostgreSQL-only and capability-gated, so they are unreachable from a confined
> creator migration.

## Offline SQL preview (`plan`)

Pending migrations can be previewed before they run. The `zero-migrate plan`
command reconciles the pending set against the live database and renders the
exact per-dialect SQL those migrations would execute; the same rendering backs
apply, so previewed SQL is what apply runs.

**The honest boundary — `-- [runtime-resolved]`.** Some operations cannot be
faithfully rendered offline because their SQL depends on the **live database
state**. The preview never fabricates SQL for these; it emits a clearly-labeled
`-- [runtime-resolved] …` line stating *why*:

- **online `renameColumn`** — needs the live `from` column's type/structure to
  reconcile the type and author the expand-contract dual-write (PostgreSQL) or
  the 12-step rebuild (SQLite); the backfill is windowed by primary key and the
  contract cutover is partitioned across deploys, so the exact statement stream
  depends on live state.
- **`backfill`** — a runtime windowed batch loop (the statement stream depends on
  live row count / key ranges).
- **existence-guarded ops** (`ifExists` / `ifNotExists`) — the apply is a
  runtime catalog probe + run / satisfied-no-op / fail-drift decision. The bare
  DDL that the apply would run when the probe says "run" *is* printed (it is real
  SQL), under the label — but no `IF [NOT] EXISTS` clause is invented (the guard
  is a probe, not a native clause).
- **stand-alone SQLite `alterColumn*` / `addConstraint` / `dropConstraint`** —
  reconciled via the live 12-step rebuild, which needs the live table structure.

The preview's header and trailing
`-- preview: N statement(s) rendered, M runtime-resolved` summary make the
offline-renderable subset and the labeled remainder explicit.

## Appendix: a data migration as IR

A migration is recorded into a dialect-neutral operation stream that the engine
loads. The following module represents a `data()` migration that backfills two
already-created columns and declares why it has no safe inverse. The preceding
schema migration that adds those columns is a separate module; a module that
mixes DDL and DML is refused.

```ts
import { table } from "@zeroship/migrate";

export default {
  name: "split_name",
  data() {
    table("people").backfill({
      name: "split_name_bf",
      set: {
        first_name: (col) => col("name").splitPart(" ", 1),
        last_name: (col) => col("name").splitPart(" ", 2),
      },
      cursorColumns: ["id"],
      cursorStability: { mode: "guardUpdates" },
      batchSize: 50,
    });
  },
  irreversible: "the original name cannot be reconstructed from split fields",
};
```

You never hand-write the recorded stream; it is derived from your `.ts`. It is
shown here so the "one script, both backends" claim is concrete.

## Further reading

- **Security / threat model** — [zeroship-migrate-guide.md](./zeroship-migrate-guide.md)
  covers the authoring sandbox and the apply path.
- **The operation stream and expression contract** —
  [zeroship-migrate-guide.md](./zeroship-migrate-guide.md) covers the recorded
  wire contract, typing stance, closed expression AST, and DML portability
  boundary.
- **SQLite divergences** — intentional PostgreSQL↔SQLite differences in search,
  isolation, locking, and ordering: [sqlite-divergences.md](./sqlite-divergences.md).
- **The schema SDK** — [db.md](./db.md): the `@zeroship/db` `t.*` lexicon the
  migration lexicon mirrors and `fromDb` bridges.