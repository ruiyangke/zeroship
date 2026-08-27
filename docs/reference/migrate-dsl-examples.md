# The migration JS-DSL — comprehensive examples guide

A practical, example-driven tour of **every** construct in the `@zeroship/migrate` authoring
surface. For the normative contract, see `docs/reference/migrate-op-dsl.md`; this guide is the
cookbook. Examples reflect the shipped API and its arg shapes as verified against
`sdks/migrate/src/{ops,types}.ts` and the engine. Where the surface is currently awkward or
limited (redundant spellings, expressiveness cliffs), this guide flags it inline
rather than papering over it.

The DSL has **one import root**:

| Import | Scope | Runs on |
| --- | --- | --- |
| `@zeroship/migrate` | Tables, columns, constraints, indexes, expressions, enums, views, partitions, triggers, domains, sequences, schemas, extensions, roles, grants, functions, RLS policies, vendor index options, `raw` | PG first; non-PG targets fail closed where no native realization exists |

Confined creator deploys reject privileged vendor ops with `VENDOR_OP_DENIED`; operator/platform
callers pass an explicit trusted capability. The import path is not a security boundary — the
engine's per-op `VendorCapability` gate is (see [§20](#20-the-raw-escape-hatch)).

---

## 1. Migration module shape

A migration is a `.ts` module exporting a `name` plus `up()` (and optionally `down()`). The
functions are parameterless and author against the ambient per-migration recorder.

```ts
import { table, t, now, uuidV4, currentSetting, currentUser, interval, concatWs } from "@zeroship/migrate";

export const name = "create_users";

export function up() {
  table("users").create({
    columns: {
      id: t.uuid().notNull().default(uuidV4()),
      email: t.text().notNull(),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
}

export function down() {
  table("users").drop({ ifExists: true });
}
```

Names are plain strings (never live-schema-bound). Every schema is placed with an explicit
`{ schema: "..." }` on platform migrations; creator migrations omit it (the confined profile pins
the project schema).

---

## 2. Tables

```ts
// Create with an explicit schema (platform style)
table("app_audit", { schema: "zeroship" }).create({
  columns: { /* … */ },
  primaryKey: ["id"],
});

table("orders").drop({ ifExists: true, cascade: true });
table("orders").rename({ to: "purchase_orders" });
table("orders").comment("customer purchase orders");
table("orders").comment(null);                       // clear the comment
```

### Table runtime options

```ts
table("posts").softDelete();                 // enable soft-delete (adds deleted_at semantics)
table("posts").softDelete({ enabled: false }); // disable
table("posts").withVersioning();             // optimistic-concurrency version column
table("posts").strictness("strict");         // TableStrictness: strict | lenient | off
table("posts").setOptions({ /* SetTableOptionsArgs */ });
```

---

## 3. The column-type lexicon (`t.*`)

Every column starts from an immutable `t.*` factory. Portable core types render on all three
dialects; PG-flavoured types map per-dialect.

```ts
// Identity / keys
t.id()                       // conventional primary-key column
t.uuid()

// Text
t.text()
t.text({ caseSensitive: false })   // case-insensitive: PG citext, SQLite NOCASE, MySQL _ci
t.char(3)                    // fixed-length CHAR(n)
t.textArray()                // text[] (PG native; SQLite TEXT; MySQL JSON)

// Numbers
t.smallInt()                 // int2
t.int()                      // int4
t.bigInt()                   // int8
t.real()                     // float4
t.double()                   // float8 / double precision — NOT an alias of t.real()
t.numeric(12, 2)             // NUMERIC(precision, scale)

// Temporal
t.timestamp()
t.date()

// Other scalars
t.boolean()
t.json()                     // jsonb
t.bytes()                    // bytea / blob
t.inet()                     // IP address (PG inet)

// Named types
t.enum("order_status")       // references an enum type (see §9)
t.domain("billing_period")   // references a domain (see §10)

// Search / spatial (vendor-mapped intent nodes)
t.vector(1536, { metric: "cosine" })   // pgvector / sqlite-vec
t.geoPoint()

// Encryption wrapper
t.encrypted({ of: t.text() })
```

### Bridging from the runtime schema

```ts
import { fromDb, now, uuidV4, currentSetting, currentUser, interval } from "@zeroship/migrate";
// Lift a live @zeroship/db field into a migration ColumnDef through the ONE shared ColType lexicon
const col = fromDb(dbField);
```

---

## 4. Column facets & defaults

Facets chain onto any `t.*` value. Order is free; each returns a `ColumnDef`.

```ts
t.text().notNull()
t.uuid().notNull().primaryKey()
t.text().unique()
t.ref("users")                               // FK column TYPE naming the target table only
t.uuid().references("users", "id")           // uuid column + typed FK facet (target table AND column)
t.bigInt().notNull().default(0)              // scalar default
t.char(3).notNull().default("usd")           // string literal default
```

### Default values — every form

```ts
// Scalars
t.bigInt().default(0)
t.text().default("pending")
t.boolean().default(true)

// Function defaults are expressions over the portable value-constructor set
t.uuid().default(uuidV4())
t.timestamp().default(now())

// Empty-container defaults
t.json().default({})                         // '{}'::jsonb
t.json().default([])                         // '[]'::jsonb
t.textArray().default([])                    // '{}'::text[]

// Arbitrary jsonb VALUE default (integers-first; keys canonicalized for checksum stability)
t.json().notNull().default({ max_sockets: 4, egress_ceiling_bytes: 10485760 })

// Sequence-backed default (PG vendor — see §11)
t.bigInt().notNull().default(nextval("orders_id_seq", { schema: "zeroship" }))
```

### Auto-incrementing keys

```ts
// Portable identity-by-default: PG GENERATED BY DEFAULT AS IDENTITY,
// SQLite INTEGER PRIMARY KEY AUTOINCREMENT, MySQL AUTO_INCREMENT
t.int().autoIncrement()

// PG identity with explicit options
t.bigInt().identity({ always: true })        // GENERATED ALWAYS AS IDENTITY
t.bigInt().identity()                        // GENERATED BY DEFAULT AS IDENTITY
```

### Generated columns, masking, encryption

```ts
t.text().generated((col) => concatWs(" ", col("first"), col("last")))        // STORED; pass { virtual: true } to invert
t.text().mask({ kind: "email" })             // MaskOptions — deterministic masking
t.encrypted({ of: t.text() }).notNull()
```

---

## 5. Altering columns (per-intent terminals)

Select a column with `.column(name)`, then use a **single-intent** terminal. There is no
`.alter({…})` bag — each change is its own op (so "type + nullable" can't silently drop one).

```ts
table("users").column("bio").add({ type: t.text() });        // add a new column
table("users").column("bio").drop();                          // drop it
table("users").column("bio").rename({ to: "biography", type: t.text() });  // rename carries the post-rename type

table("users").column("age").setType({ to: t.bigInt() });     // change type ({ using } for a cast expr)
table("users").column("email").setNotNull();                  // SET NOT NULL
table("users").column("email").dropNotNull();                 // DROP NOT NULL
table("users").column("status").setDefault("active");         // SET DEFAULT
table("users").column("status").dropDefault();                // DROP DEFAULT
table("users").column("email").comment("primary contact");
```

Adding a column and backfilling it in one flow:

```ts
table("users")
  .column("first_name").add({ type: t.text() })
  .backfill({
    set: { first_name: (col) => col("name").splitPart(" ", 1) },
    cursorColumns: ["id"],
    cursorStability: { mode: "guardUpdates" },
  });
```

---

## 6. Constraints

### Primary key

```ts
// In create()
table("t").create({ columns: { /* … */ }, primaryKey: ["id"] });
table("t").create({ columns: { /* … */ }, primaryKey: ["tenant_id", "id"] });  // composite
```

### Unique

```ts
table("users").unique("users_email_key").add({ columns: ["email"] });
```

### Check

```ts
// The one spelling — the named selector form (supports notValid / ifNotExists / schema)
table("orders").check("orders_qty_positive").add({ expr: (col) => col("qty").gt(0) });
```

### Foreign keys

```ts
// Selector form (single-column, references id by convention)
table("posts").foreignKey("posts_author_fkey")
  .add({ columns: ["author_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });

// The same selector form — composite, non-id target, cross-schema reference, deferrable
table("usage_aggregates", { schema: "zeroship" }).foreignKey("usage_aggregates_metric_fkey").add({
  columns: ["metric"],
  references: { table: "billing_metrics", columns: ["metric"], schema: "zeroship" },
  deferrable: true,
  initiallyDeferred: true,
});

table("line_items").foreignKey("line_items_order_fkey").add({
  columns: ["order_id", "tenant_id"],                       // composite
  references: { table: "orders", columns: ["id", "tenant_id"] },
  onDelete: "restrict",
  onUpdate: "cascade",
});
```

`RefAction` = `"cascade" | "restrict" | "setNull" | "setDefault" | "noAction"` (camelCase wire tags).

### Exclusion constraints (PG)

```ts
table("reservations").exclusion("no_overlap").add({
  using: "gist",
  elements: [{ target: "room_id", operator: "=" }, { target: "during", operator: "&&" }],
  deferrable: true,
});
```

### Dropping / commenting a constraint by name

```ts
table("orders").constraint("orders_qty_positive").drop({ ifExists: true });
table("orders").constraint("orders_pkey").comment("surrogate key");
```

---

## 7. Indexes

Select with `.index(name)`, then pass the target elements and optional modifiers
in `.add({…})`.

```ts
import { table } from "@zeroship/migrate";

// Basic
table("app_members").index("app_members_user_idx").add({ on: ["user_id"] });

// Composite + partial (WHERE predicate is the (col) => Expr builder, PG vendor)
table("app_session_anchors").index("app_session_anchors_user_idx")
  .add({ on: ["app_id",
  "global_user_id"],
  where: (col) => col("revoked_at").isNull() });

// Unique
table("users").index("users_email_uq").add({ on: ["email"],
  unique: true });

// Access method
table("docs").index("docs_body_gin").add({ on: ["body"],
  using: "gin" });
table("events").index("events_ts_brin").add({ on: ["occurred_at"],
  using: "brin" });
table("embeddings").index("embeddings_vec").add({ on: ["vec"],
  using: "hnsw" });

// Per-column ASC/DESC ordering (IndexElementArg)
table("posts").index("posts_created_desc")
  .add({ on: [{ column: "created_at",
  order: "desc" }] });

// Expression column
table("users").index("users_lower_email")
  .add({ on: [{ expr: (col) => col("email").lower() }] });

// Covering (INCLUDE) + storage params + ONLY (don't recurse into partitions)
table("orders").index("orders_customer_idx")
  .add({
    on: ["customer_id"],
  include: ["total",
  "status"],
  with: { fillfactor: 90 },
  only: true,
  });

// Drop / comment
table("orders").index("orders_customer_idx").drop({ ifExists: true });
```

`table().index()` accepts the full PG-first index surface: `on`, `unique`,
`ifNotExists`, `schema`, `using`, `where`, `include`, `with`, `only`,
`nullsNotDistinct`, and per-element `order`/`opclass`/`collation`/`nulls`.
Vendor options remain capability/dialect-gated by the engine and fail closed
where a target has no native realization.

---

## 8. Expressions — the `(col) => Expr` builder

Every predicate/value position (checks,
  index WHERE,
  policy USING,
  generated columns,
  backfills)
uses the same closed,
  portable expression builder. `col("col")` references a column.

```ts
// Comparisons
(col) => col("qty").gt(0)
(col) => col("price").ge(100)
(col) => col("status").eq("active")
(col) => col("deleted_at").ne(null)
(col) => col("score").lt(50)
(col) => col("score").le(50)

// Null tests
(col) => col("revoked_at").isNull()
(col) => col("email").isNotNull()

// Boolean tests
(col) => col("enabled").isTrue()
(col) => col("archived").isFalse()

// Logical composition
(col) => col("qty").gt(0).and(col("price").ge(0))
(col) => col("a").isNotNull().or(col("b").isNotNull())
(col) => col("blocked").isTrue().not()

// Set membership
(col) => col("status").in(["active",
  "past_due",
  "suspended"])
(col) => col("state").notIn(["deleted",
  "purged"])

// Arithmetic + string
(col) => col("a").add(col("b"))
(col) => col("total").sub(col("discount"))
(col) => col("qty").mul(col("unit_price"))
(col) => col("num").div(col("den"))
(col) => col("first").concat(col("last"))

// PG pattern match / size — first-class chain operators (PG-first; fail-closed
// off-PG,
  `dialect({...})` to port). Usable anywhere a chain is,
  incl. core checks.
(col) => col("email").regex("^[^@]+@[^@]+$")            // regex → `~` (PG) / `REGEXP` (MySQL)
(col) => col("payload").columnSize().lt(1048576)       // pg_column_size < 1MiB

// Cast
(col) => col("app_id").cast({ to: "uuid" })
```

### Scalar chain methods and `concatWs`

```ts
(col) => col("email").lower()
(col) => col("code").upper()
(col) => col("name").trim()
(col) => col("bio").length()
(col) => col("delta").abs()
(col) => col("nick").coalesce(col("name"))
(col) => col("a").nullif(col("b"))
(col) => concatWs(" ",
  col("first"),
  col("last"))
(col) => col("path").splitPart(
  "/",
  1)
(col) => now()
(col) => uuidV4()

// CASE expression — explicit when/then branches,
  with an optional else
(col) => col.case({ branches: [{ when: col("n").gt(0),
  then: lit("pos") }],
  else: lit("nonpos") })

// PostgreSQL-first value constructors (used in RLS policies / CHECKs);
// fail closed off-target via the validator.
(col) => currentSetting("zeroship.tenant_app",
  { missingOk: true }).cast({ to: "uuid" })
(col) => currentUser()
(col) => col("expires_at").le(col("created_at").add(interval({ days: 3 })))
```

### Literals & helpers

```ts
import { lit,
  minValue,
  maxValue,
  nextval,
  now,
  uuidV4,
  currentSetting,
  currentUser,
  interval,
} from "@zeroship/migrate";
lit(42)                       // an explicit literal node
minValue / maxValue           // partition-bound sentinels (see §12)
```

---

## 9. Enum types

```ts
import { enumType, now, uuidV4, currentSetting, currentUser, interval } from "@zeroship/migrate";

enumType("order_status").create({ values: ["pending", "paid", "shipped"], schema: "zeroship" });
enumType("order_status").comment("lifecycle of an order");
enumType("order_status").drop({ ifExists: true });

// Use it on a column
table("orders").create({ columns: { status: t.enum("order_status").notNull() }, primaryKey: ["id"] });
```

---

## 10. Domains

```ts
import {
  table,
  domain,
  t,
  now,
  uuidV4,
  currentSetting,
  currentUser,
  interval,
} from "@zeroship/migrate";

// A domain = base type + CHECK. The (col) => Expr uses the VALUE placeholder.
domain("account_state").create({
  schema: "zeroship",
  as: t.text(),
  check: (col) => col("VALUE").in(["active", "past_due", "suspended"]),
});

domain("billing_period").create({ schema: "zeroship", as: t.date() });
domain("account_state").comment("tenant account lifecycle");
domain("account_state").drop({ ifExists: true });

// Use it
table("spend_state").create({
  columns: { state: t.domain("account_state").notNull().default("active") },
  primaryKey: ["app_id"],
});
```

---

## 11. Sequences & `nextval`

```ts
import {
  sequence,
  nextval,
  t,
  now,
  uuidV4,
  currentSetting,
  currentUser,
  interval,
} from "@zeroship/migrate";

sequence("orders_id_seq").create({ schema: "zeroship", start: 1, increment: 1 });
sequence("orders_id_seq").alter({ restart: 1000 });
sequence("orders_id_seq").drop({ ifExists: true });

// Wire a sequence to a column default
table("orders", { schema: "zeroship" }).create({
  columns: { id: t.bigInt().notNull().default(nextval("orders_id_seq", { schema: "zeroship" })) },
  primaryKey: ["id"],
});
```

---

## 12. Partitioning (PG)

A partition is authored from the parent table handle:
`table(parent).partition(child)`. Range/list/hash are declared structurally on
the parent's `partitionBy`.

```ts
import { table, t, minValue, maxValue, now, uuidV4, currentSetting, currentUser, interval } from "@zeroship/migrate";

// 1) Parent declares the partition strategy at create()
table("sandbox_events",
  { schema: "zeroship" }).create({
  columns: { id: t.uuid().notNull(),
  occurred_at: t.timestamp().notNull(),
  /* … */ },
  primaryKey: ["id",
  "occurred_at"],
  partitionBy: { range: ["occurred_at"] },
  // { range } | { list } | { hash }
});

// 2) Create partitions (parent-subject). RANGE bounds:
table("sandbox_events",
  { schema: "zeroship" }).partition("sandbox_events_2026_05")
  .create({ from: ["2026-05-01 00:00:00+00"],
  to: ["2026-06-01 00:00:00+00"] });

// LIST bounds:
table("events",
  { schema: "zeroship" }).partition("events_eu").create({ in: ["de",
  "fr",
  "es"] });

// HASH bounds:
table("events",
  { schema: "zeroship" }).partition("events_h0").create({ modulus: 4,
  remainder: 0 });

// DEFAULT partition (catch-all):
table("sandbox_events",
  { schema: "zeroship" }).partition("sandbox_events_default").create({ default: true });

// Unbounded range ends use the sentinels:
table("events").partition("events_head").create({ from: [minValue],
  to: ["2026-01-01"] });
table("events").partition("events_tail").create({ from: ["2027-01-01"],
  to: [maxValue] });

// Lifecycle: detach and drop (both parent-subject)
table("sandbox_events",
  { schema: "zeroship" }).partition("sandbox_events_2026_05").detach();
table("sandbox_events",
  { schema: "zeroship" }).partition("sandbox_events_2026_05").drop();
```

**Faithfulness note:** create indexes on the *parent* (no `ONLY`) — PG auto-propagates and
auto-attaches child indexes,
  reproducing a hand-decomposed `pg_dump` exactly.

---

## 13. Views

Two forms: the portable structured `SelectAst` builder,
  and a raw `{ as: { raw } }` escape.

```ts
import { view,
  countStar,
  now,
  uuidV4,
  currentSetting,
  currentUser,
  interval,
} from "@zeroship/migrate";

// Structured (portable): the `as` callback receives a fluent SelectAst builder
view("active_users").create({
  as: (q) =>
    q.from("users")
      .select(["id", "email"])
      .where((col) => col("deleted_at").isNull())
      .orderBy(["created_at"])
      .limit(100),
});

// Joins and grouped aggregation; `materialized: true` makes it a matview.
view("order_totals").create({
  materialized: true,
  as: (q) => q
    .from("orders")
    .select([
      "customer_id",
      { kind: "expr", alias: "n", expr: () => countStar() },
      { kind: "expr", alias: "revenue", expr: (col) => col("amount").sum() },
    ])
    .where((col) => col("status").eq("paid"))
    .groupBy(["customer_id"])
    .having((col) => col("id").count().gt(5)),
});

// PostgreSQL-first aggregate coverage. SQLite/MySQL targets fail closed with
// DIALECT_UNSUPPORTED unless the expression is wrapped in dialect({...}) with
// explicit non-PG legs.
view("order_rollups").create({
  as: (q) => q
    .from("orders")
    .select([
      "customer_id",
      { kind: "expr", alias: "item_names", expr: (col) => col("item_name").stringAgg(", ") },
      { kind: "expr", alias: "order_ids", expr: (col) => col("id").arrayAgg() },
      { kind: "expr", alias: "all_fulfilled", expr: (col) => col("fulfilled").boolAnd() },
    ])
    .groupBy(["customer_id"]),
});

view("active_users").drop({ ifExists: true });
view("active_users").comment("non-deleted users");

// Raw view body for constructs outside the structured SelectAst.
view("legacy_report").create({
  as: { raw: "SELECT a.id, count(b.*) FROM a JOIN b USING (id) GROUP BY 1" },
});
```

`ViewQueryBuilder`: `from · select · join · innerJoin · leftJoin · where · groupBy · having · orderBy · limit`.

---

## 14. Row-level security & policies

```ts
import { table, currentSetting } from "@zeroship/migrate";

// Table-scoped RLS state
table("apps",
  { schema: "zeroship" }).setRls({ enabled: true,
  forced: true });
table("apps",
  { schema: "zeroship" }).setRls({ enabled: false,
  forced: false });

// Policy via the table handle
table("apps",
  { schema: "zeroship" }).policy("tenant_isolation").create({
  using: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app",
  { missingOk: true }).cast({ to: "uuid" })),
  withCheck: (col) => col("app_id").eq(currentSetting("zeroship.tenant_app",
  });
table("apps",
  { schema: "zeroship" }).policy("tenant_isolation").drop();
```

`PolicyCmd` (the `for` field) = `all | select | insert | update | delete`.

---

## 15. Triggers

```ts
import { table,
  now,
  uuidV4,
  currentSetting,
  currentUser,
  interval,
} from "@zeroship/migrate";

table("app_audit", { schema: "zeroship" }).trigger("app_audit_block_delete").create({
  timing: "before",              // before | after | insteadOf
  events: ["delete"],            // insert | update | delete (array)
  forEach: "row",                // row | statement
  execute: "app_audit_block_tamper",   // the function to EXECUTE
});

table("app_audit", { schema: "zeroship" }).trigger("app_audit_block_delete").drop();
```

---

## 16. Functions

```ts
import { createFunction, dropFunction } from "@zeroship/migrate";

createFunction({
  name: "app_audit_block_tamper",
  schema: "zeroship",
  returns: "trigger",
  language: "plpgsql",
  body: `BEGIN RAISE EXCEPTION 'audit rows are immutable'; END;`,
  });

dropFunction({ name: "app_audit_block_tamper",
  ifExists: true });
```

---

## 17. Schemas, extensions, roles, grants

```ts
import {
  schema,
  extension,
  role,
  dropOwnedBy,
  grant,
  revoke,
} from "@zeroship/migrate";

schema("zeroship").create();
schema("zeroship").drop({ cascade: true });

extension("citext").create();
extension("vector").create({ schema: "zeroship" });
extension("citext").drop({ ifExists: true });

role("app_rw").create({ login: false });
role("app_rw").setOptions({ /* ... */ });
role("app_rw").drop({ ifExists: true });
dropOwnedBy({ roles: ["app_rw"] });          // `roles` is an array

// `on` is a GrantTarget tagged union ({ table }/{ schema }/{ sequence }/… — check GrantTarget);
// `to`/`from` are string arrays.
grant({ to: ["app_rw"],
  on: { table: "orders",
  schema: "zeroship" },
  privileges: ["select",
  "insert"] });
revoke({ from: ["app_rw"],
  on: { schema: "zeroship" },
  privileges: ["usage"] });
```

---

## 18. Data & backfills (DML)

DML has no existence guard (it is unguardable). Available on any table handle:

```ts
// Insert — `rows` (a row object or array),
  optional ON CONFLICT
table("plans").insert({
  rows: [{ id: "free",
  name: "Free" },
  { id: "pro",
  name: "Pro" }],
  onConflict: { columns: ["id"],
  doUpdate: { name: "Pro" } },
  });

// Update — `set` values are EXPRESSIONS (ExprFn),
  so a scalar must be wrapped in lit()
table("plans").update({ set: { name: (col) => lit("Professional") },
  where: (col) => col("id").eq("pro") });

// Delete — the method is `del` (JS reserves `delete`); wire tag is "delete"
table("plans").del({ where: (col) => col("id").eq("legacy") });

// Backfill a column with an expression (chunked; the ordered cursorColumns tuple
// pages large tables, cursorStability keeps those components immutable)
table("users").backfill({
  set: { display_name: (col) => col("nickname").coalesce(col("name")) },
  cursorColumns: ["id"],
  cursorStability: { mode: "guardUpdates" },
  });
```

---

## 19. Comments (any object)

```ts
import { comment,
  now,
  uuidV4,
  currentSetting,
  currentUser,
  interval,
} from "@zeroship/migrate";

comment({ kind: "table", name: "orders", schema: "zeroship" }, "customer orders");
comment({ kind: "column", table: "orders", name: "total", schema: "zeroship" }, "cents");
comment({ kind: "type", name: "order_status", schema: "zeroship" }, "order lifecycle");
```

Most handles also carry a `.comment(text | null)` shortcut (tables, columns, enums, domains,
sequences, constraints, indexes, views).

---

## 20. The raw escape hatch

`raw` is the last resort for genuinely unrepresentable DDL (e.g. a `CREATE TRIGGER … BEFORE
UPDATE OF <col>` the structured trigger surface can't yet express, or a PL/pgSQL construct). It
**requires a `reason`**; the reason travels with the checksummed IR.

```ts
import { raw } from "@zeroship/migrate";

raw({
  sql: "CREATE TRIGGER t BEFORE UPDATE OF sector_identifier ON zeroship.app_oauth_clients " +
       "FOR EACH ROW EXECUTE FUNCTION zeroship.reject_sector_change()",
  reason: "column-scoped UPDATE OF is outside the current structured trigger surface",
  });
```

There are **no binds** — `raw` takes a complete SQL string. If you find yourself reaching for
`raw` for a shape the structured surface *should* cover,
  that is a gap to close in the DSL,
  not a
license to accumulate raw SQL.

---

## 21. Determinism lint

```ts
import { lintDeterminism,
  now,
  uuidV4,
  currentSetting,
  currentUser,
  interval,
} from "@zeroship/migrate";
// Best-effort source scan flagging non-deterministic authoring (e.g. Date.now() / Math.random()
// leaking into recorded values). Returns DeterminismFinding[].
const findings = lintDeterminism(sourceText);
```

---

## Portability at a glance

| Construct | PG | SQLite | MySQL |
| --- | --- | --- | --- |
| Core tables / columns / scalar types | ✅ | ✅ | ✅ |
| Portable expressions (`(col) => …`, chain scalar methods, `concatWs`) | ✅ | ✅ | ✅ |
| `t.text({ caseSensitive: false })` | citext | `COLLATE NOCASE` | `_ci` collation |
| `t.textArray()` | `text[]` | `TEXT` | `JSON` |
| Empty/jsonb-value defaults | `::jsonb` | text | `CAST(... AS JSON)` |
| `autoIncrement()` | `IDENTITY` | `AUTOINCREMENT` | `AUTO_INCREMENT` |
| Deferrable FK | ✅ | ✅ | omitted (InnoDB immediate) |
| Domains · sequences · RLS/policies · partitioning · roles · grants · `raw` | ✅ | ✖ fail-closed | ✖ fail-closed |

Postgres-only root exports such as domains, sequences, RLS/policies, roles, grants, and `raw`
fail closed on other dialects; portable root constructs render on all three with the
per-dialect mappings above.
