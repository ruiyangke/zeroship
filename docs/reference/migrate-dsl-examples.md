# The migration JS-DSL — comprehensive examples guide

A practical, example-driven tour of **every** construct in the `@zeroship/migrate` authoring
surface. For the normative contract, see `docs/reference/migrate-op-dsl.md`; this guide is the
cookbook. Examples reflect the shipped API and its arg shapes as verified against
`sdks/migrate/src/{ops,types,pg}.ts` and the engine. Where the surface is currently awkward or
limited (redundant spellings, a decorative core/vendor split, expressiveness cliffs), this guide
flags it inline rather than papering over it.

The DSL has **two roots**:

| Import | Scope | Runs on |
| --- | --- | --- |
| `@zeroship/migrate` | Portable core — tables, columns, constraints, indexes, expressions, enums, views, partitions, triggers | PG · SQLite · MySQL |
| `@zeroship/migrate/pg` | Postgres vendor — domains, sequences, schemas, extensions, roles, grants, functions, RLS policies, `raw` | PG only (fail-closed elsewhere) |

Confined creator deploys reject every `/pg` op with `VENDOR_OP_DENIED`; operator/platform callers
pass an explicit trusted capability. The `/pg` subpath is not a security boundary — it is an
honesty boundary (see [§20](#20-the-raw-escape-hatch)).

---

## 1. Migration module shape

A migration is a `.ts` module exporting a `name` plus `up()` (and optionally `down()`). The
functions are parameterless and author against the ambient per-migration recorder.

```ts
import { table, t } from "@zeroship/migrate";

export const name = "create_users";

export function up() {
  table("users").create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      email: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
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
table("posts").softDelete(false);            // disable
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
t.integer()                  // int4  (t.int() is a true alias of this)
t.bigInt()                   // int8
t.real()                     // float4
t.float()                    // float8 / double precision — NOT an alias of t.real()
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
import { fromDb } from "@zeroship/migrate";
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
t.uuid().ref("users")                        // inline FK to users(id)
t.bigInt().notNull().default(0)              // scalar default
t.char(3).notNull().default("usd")           // string literal default
```

### Default values — every form

```ts
// Scalars
t.bigInt().default(0)
t.text().default("pending")
t.boolean().default(true)

// Function defaults (portable set)
t.uuid().default({ fn: "genRandomUuid" })
t.timestamp().default({ fn: "now" })

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
t.text().generated((c) => c.fn.concatWs(" ", c("first"), c("last")))   // STORED; pass { virtual: true } to invert
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
  .backfill({ set: { first_name: (c) => c.fn.splitPart(c("name"), " ", 1) } });
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
// Inline shorthand
table("orders").addCheck("orders_qty_positive", (c) => c("qty").gt(0));

// Named selector form (supports ifNotExists / schema)
table("orders").check("orders_qty_positive").add({ expr: (c) => c("qty").gt(0) });
```

### Foreign keys

```ts
// Selector form (single-column, references id by convention)
table("posts").foreignKey("posts_author_fkey")
  .add({ columns: ["author_id"], references: { table: "users", columns: ["id"] }, onDelete: "cascade" });

// addForeignKey — composite, non-id target, cross-schema reference, deferrable
table("usage_aggregates", { schema: "zeroship" }).addForeignKey("usage_aggregates_metric_fkey", {
  columns: ["metric"],
  references: { table: "billing_metrics", columns: ["metric"], schema: "zeroship" },
  deferrable: true,
  initiallyDeferred: true,
});

table("line_items").addForeignKey("line_items_order_fkey", {
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

Select with `.index(name)`, chain optional modifiers, then `.add({…})`.

```ts
// Basic
table("app_members").index("app_members_user_idx").add({ columns: ["user_id"] });

// Composite + partial (WHERE predicate is the (c) => Expr builder)
table("app_session_anchors").index("app_session_anchors_user_idx")
  .add({ columns: ["app_id", "global_user_id"], where: (c) => c("revoked_at").isNull() });

// Unique
table("users").index("users_email_uq").add({ columns: ["email"], unique: true });

// Access method
table("docs").index("docs_body_fts").add({ columns: ["body"], using: "gin" });
table("events").index("events_ts_brin").add({ columns: ["occurred_at"], using: "brin" });
table("embeddings").index("embeddings_vec").add({ columns: ["vec"], using: "hnsw" });

// Per-column ASC/DESC ordering (IndexElementArg)
table("posts").index("posts_created_desc")
  .add({ columns: [{ kind: "column", name: "created_at", order: "desc" }] });

// Expression column
table("users").index("users_lower_email")
  .add({ columns: [{ kind: "expr", expr: (c) => c.fn.lower(c("email")) }] });

// Covering (INCLUDE) + storage params + ONLY (don't recurse into partitions)
table("orders").index("orders_customer_idx")
  .include(["total", "status"])
  .with({ fillfactor: 90 })
  .only()
  .add({ columns: ["customer_id"] });

// Drop / comment
table("orders").index("orders_customer_idx").drop({ ifExists: true });
```

`IndexMethod` = `btree | gin | gist | brin | ivfflat | hnsw | fts5` (`hash`/`spgist` not yet in the union).

---

## 8. Expressions — the `(c) => Expr` builder

Every predicate/value position (checks, index WHERE, policy USING, generated columns, backfills)
uses the same closed, portable expression builder. `c("col")` references a column.

```ts
// Comparisons
(c) => c("qty").gt(0)
(c) => c("price").ge(100)
(c) => c("status").eq("active")
(c) => c("deleted_at").ne(null)
(c) => c("score").lt(50)
(c) => c("score").le(50)

// Null tests
(c) => c("revoked_at").isNull()
(c) => c("email").isNotNull()

// Boolean tests
(c) => c("enabled").isTrue()
(c) => c("archived").isFalse()

// Logical composition (also free functions: and / or / not)
import { and, or, not, membership, notMembership } from "@zeroship/migrate";
(c) => and(c("qty").gt(0), c("price").ge(0))
(c) => or(c("a").isNotNull(), c("b").isNotNull())
(c) => not(c("blocked").isTrue())

// Set membership  (= ANY / <> ALL)
(c) => membership(c("status"), ["active", "past_due", "suspended"])
(c) => notMembership(c("state"), ["deleted", "purged"])

// Arithmetic + string
(c) => c("a").add(c("b"))
(c) => c("total").sub(c("discount"))
(c) => c("qty").mul(c("unit_price"))
(c) => c("num").div(c("den"))
(c) => c("first").concat(c("last"))

// Pattern match / size / cast
(c) => c("email").matches("^[^@]+@[^@]+$")     // regex
(c) => c("payload").columnSize().lt(1048576)   // pg_column_size < 1MiB
(c) => c("app_id").cast("uuid")
```

### Function namespace (`c.fn.*`)

```ts
(c) => c.fn.lower(c("email"))
(c) => c.fn.upper(c("code"))
(c) => c.fn.trim(c("name"))
(c) => c.fn.length(c("bio"))
(c) => c.fn.abs(c("delta"))
(c) => c.fn.coalesce(c("nick"), c("name"))
(c) => c.fn.nullif(c("a"), c("b"))
(c) => c.fn.concatWs(" ", c("first"), c("last"))
(c) => c.fn.splitPart(c("path"), "/", 1)
(c) => c.fn.now()
(c) => c.fn.genRandomUuid()

// CASE expression — [when, then] TUPLES, else is a positional 2nd arg
(c) => c.fn.case([[c("n").gt(0), lit("pos")]], lit("nonpos"))

// PG-flavoured settings (used in RLS policies) — reachable via c.fn on the pg-aware builder
(c) => c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")
(c) => c.fn.currentUser()
```

### Literals & helpers

```ts
import { lit, interval, p, minValue, maxValue, nextval } from "@zeroship/migrate";
lit(42)                       // an explicit literal node
interval("72:00:00")          // an interval literal — HH:MM:SS form only (not "7 days")
p                             // the partition-bound builder namespace (see §12)
```

---

## 9. Enum types

```ts
import { enumType } from "@zeroship/migrate";

enumType("order_status").create({ values: ["pending", "paid", "shipped"], schema: "zeroship" });
enumType("order_status").comment("lifecycle of an order");
enumType("order_status").drop({ ifExists: true });

// Use it on a column
table("orders").create({ columns: { status: t.enum("order_status").notNull() }, primaryKey: ["id"] });
```

---

## 10. Domains (`/pg`)

```ts
import { domain } from "@zeroship/migrate/pg";
import { t, membership } from "@zeroship/migrate";

// A domain = base type + CHECK. The (c) => Expr uses the VALUE placeholder.
domain("account_state").create({
  schema: "zeroship",
  as: t.text(),
  check: (c) => membership(c("VALUE"), ["active", "past_due", "suspended"]),
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

## 11. Sequences & `nextval` (`/pg` + core)

```ts
import { sequence } from "@zeroship/migrate/pg";
import { nextval, t } from "@zeroship/migrate";

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

A partition is a first-class relation authored with the **child-subject** grammar
`partition(child).of(parent)`. Range/list/hash are declared on the parent's `partitionBy`.

```ts
import { table, partition, dropPartition, p } from "@zeroship/migrate";

// 1) Parent declares the partition strategy at create()
table("sandbox_events", { schema: "zeroship" }).create({
  columns: { id: t.uuid().notNull(), occurred_at: t.timestamp().notNull(), /* … */ },
  primaryKey: ["id", "occurred_at"],
  partitionBy: p.range(["occurred_at"]),      // p.range | p.list | p.hash
});

// 2) Attach partitions (child-subject). RANGE bounds:
partition("sandbox_events_2026_05", { schema: "zeroship" }).of("sandbox_events")
  .forValues({ from: ["2026-05-01 00:00:00+00"], to: ["2026-06-01 00:00:00+00"] });

// LIST bounds:
partition("events_eu", { schema: "zeroship" }).of("events").forValues({ in: ["de", "fr", "es"] });

// HASH bounds:
partition("events_h0", { schema: "zeroship" }).of("events").forValues({ modulus: 4, remainder: 0 });

// DEFAULT partition (catch-all):
partition("sandbox_events_default", { schema: "zeroship" }).of("sandbox_events").asDefault();

// Unbounded range ends use the sentinels:
import { minValue, maxValue } from "@zeroship/migrate";
partition("events_head").of("events").forValues({ from: [minValue], to: ["2026-01-01"] });
partition("events_tail").of("events").forValues({ from: ["2027-01-01"], to: [maxValue] });

// Lifecycle: detach (parent-subject) and drop (child-subject)
table("sandbox_events", { schema: "zeroship" }).detachPartition("sandbox_events_2026_05");
dropPartition("sandbox_events_2026_05", { schema: "zeroship" });
```

**Faithfulness note:** create indexes on the *parent* (no `ONLY`) — PG auto-propagates and
auto-attaches child indexes, reproducing a hand-decomposed `pg_dump` exactly.

---

## 13. Views

Two forms: the portable structured `SelectAst` builder, and a PG `createRaw` escape.

```ts
import { view } from "@zeroship/migrate";

// Structured (portable): the `as` callback receives a fluent SelectAst builder
view("active_users").create({
  as: (q) =>
    q.from("users")
      .select(["id", "email"])
      .where((c) => c("deleted_at").isNull())
      .orderBy(["created_at"])
      .limit(100),
});

// Joins; `materialized: true` makes it a matview.
// ⚠️ Expressiveness cliff: the expression builder currently rejects table-qualified
//    column refs (`c("orders.customer_id")` fails the strict identifier gate), so a
//    join ON predicate is effectively unwritable structurally today — use createRaw for
//    joined/aggregated views until the builder gains qualified refs.
view("order_totals").create({
  materialized: true,
  as: (q) => q.from("orders").select(["id", "total"]).where((c) => c("total").gt(0)),
});

view("active_users").drop({ ifExists: true });
view("active_users").comment("non-deleted users");

// Raw view (PG escape). NOTE: createRaw takes { sql, columns?, materialized?, schema? } —
// there is NO `reason` field on raw views (unlike pg.raw for DDL).
view("legacy_report").createRaw({ sql: "SELECT a.id, count(b.*) FROM a JOIN b USING (id) GROUP BY 1" });
```

`ViewQueryBuilder`: `from · select · join · innerJoin · leftJoin · where · orderBy · limit`.

---

## 14. Row-level security & policies (`/pg`)

```ts
// Table-scoped RLS toggles
table("apps", { schema: "zeroship" }).enableRowLevelSecurity();
table("apps", { schema: "zeroship" }).forceRowLevelSecurity();
table("apps", { schema: "zeroship" }).disableRowLevelSecurity();
table("apps", { schema: "zeroship" }).noForceRowLevelSecurity();

// Policy via the table handle
table("apps", { schema: "zeroship" }).createPolicy({
  name: "tenant_isolation",
  using: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")),
  withCheck: (c) => c("app_id").eq(c.fn.currentSetting("zeroship.tenant_app", true).cast("uuid")),
});
table("apps", { schema: "zeroship" }).dropPolicy({ name: "tenant_isolation" });

// Standalone policy functions are also available from @zeroship/migrate/pg:
import { createPolicy, dropPolicy } from "@zeroship/migrate/pg";
createPolicy({ name: "p", table: "apps", schema: "zeroship", for: "select", using: (c) => /* … */ });
```

`PolicyCmd` (the `for` field) = `all | select | insert | update | delete`.

---

## 15. Triggers

```ts
table("app_audit", { schema: "zeroship" }).createTrigger({
  name: "app_audit_block_delete",
  timing: "before",              // before | after | insteadOf
  events: ["delete"],            // insert | update | delete (array)
  forEach: "row",                // row | statement
  execute: "app_audit_block_tamper",   // the function to EXECUTE
});

table("app_audit", { schema: "zeroship" }).dropTrigger({ name: "app_audit_block_delete" });
```

---

## 16. Functions (`/pg`)

```ts
import { createFunction, dropFunction } from "@zeroship/migrate/pg";

createFunction({
  name: "app_audit_block_tamper",
  schema: "zeroship",
  returns: "trigger",
  language: "plpgsql",
  body: `BEGIN RAISE EXCEPTION 'audit rows are immutable'; END;`,
});

dropFunction({ name: "app_audit_block_tamper", schema: "zeroship", ifExists: true });
```

---

## 17. Schemas, extensions, roles, grants (`/pg`)

```ts
import { schema, dropSchema, extension, dropExtension,
         role, alterRole, dropRole, dropOwnedBy, grant, revoke } from "@zeroship/migrate/pg";

schema({ name: "zeroship" });
dropSchema({ name: "zeroship", cascade: true });

extension({ name: "citext" });
extension({ name: "vector", schema: "zeroship" });
dropExtension({ name: "citext", ifExists: true });

role({ name: "app_rw", login: false });
alterRole({ name: "app_rw", /* … */ });
dropRole({ name: "app_rw", ifExists: true });
dropOwnedBy({ roles: ["app_rw"] });          // `roles` is an array

// `on` is a GrantTarget tagged union ({ table }/{ schema }/{ sequence }/… — check GrantTarget);
// `to`/`from` are string arrays.
grant({ to: ["app_rw"], on: { table: "orders", schema: "zeroship" }, privileges: ["select", "insert"] });
revoke({ from: ["app_rw"], on: { schema: "zeroship" }, privileges: ["usage"] });
```

---

## 18. Data & backfills (DML)

DML has no existence guard (it is unguardable). Available on any table handle:

```ts
// Insert — `rows` (a row object or array), optional ON CONFLICT
table("plans").insert({
  rows: [{ id: "free", name: "Free" }, { id: "pro", name: "Pro" }],
  onConflict: { columns: ["id"], doUpdate: { name: "Pro" } },
});

// Update — `set` values are EXPRESSIONS (ExprFn), so a scalar must be wrapped in lit()
table("plans").update({ set: { name: (c) => lit("Professional") }, where: (c) => c("id").eq("pro") });

// Delete — the method is `del` (JS reserves `delete`); wire tag is "delete"
table("plans").del({ where: (c) => c("id").eq("legacy") });

// Backfill a column with an expression (chunked; cursorColumn for large tables)
table("users").backfill({
  set: { display_name: (c) => c.fn.coalesce(c("nickname"), c("name")) },
  cursorColumn: "id",
});
```

---

## 19. Comments (any object)

```ts
import { comment } from "@zeroship/migrate";

comment({ kind: "table", name: "orders", schema: "zeroship" }, "customer orders");
comment({ kind: "column", table: "orders", name: "total", schema: "zeroship" }, "cents");
comment({ kind: "type", name: "order_status", schema: "zeroship" }, "order lifecycle");
```

Most handles also carry a `.comment(text | null)` shortcut (tables, columns, enums, domains,
sequences, constraints, indexes, views).

---

## 20. The raw escape hatch (`/pg`)

`raw` is the last resort for genuinely unrepresentable DDL (e.g. a `CREATE TRIGGER … BEFORE
UPDATE OF <col>` the structured trigger surface can't yet express, or a PL/pgSQL construct). It
**requires a `reason`** — the boundary is honest and counted, not aspirational.

```ts
import { raw } from "@zeroship/migrate/pg";

raw({
  sql: "CREATE TRIGGER t BEFORE UPDATE OF sector_identifier ON zeroship.app_oauth_clients " +
       "FOR EACH ROW EXECUTE FUNCTION zeroship.reject_sector_change()",
  reason: "column-scoped UPDATE OF is outside the current structured trigger surface",
});
```

There are **no binds** — `raw` takes a complete SQL string. If you find yourself reaching for
`raw` for a shape the structured surface *should* cover, that is a gap to close in the DSL, not a
license to accumulate raw SQL.

---

## 21. Determinism lint

```ts
import { lintDeterminism } from "@zeroship/migrate";
// Best-effort source scan flagging non-deterministic authoring (e.g. Date.now() / Math.random()
// leaking into recorded values). Returns DeterminismFinding[].
const findings = lintDeterminism(sourceText);
```

---

## Portability at a glance

| Construct | PG | SQLite | MySQL |
| --- | --- | --- | --- |
| Core tables / columns / scalar types | ✅ | ✅ | ✅ |
| Portable expressions (`(c) => …`, `c.fn.*`) | ✅ | ✅ | ✅ |
| `t.text({ caseSensitive: false })` | citext | `COLLATE NOCASE` | `_ci` collation |
| `t.textArray()` | `text[]` | `TEXT` | `JSON` |
| Empty/jsonb-value defaults | `::jsonb` | text | `CAST(... AS JSON)` |
| `autoIncrement()` | `IDENTITY` | `AUTOINCREMENT` | `AUTO_INCREMENT` |
| Deferrable FK | ✅ | ✅ | omitted (InnoDB immediate) |
| Domains · sequences · RLS/policies · partitioning · roles · grants · `raw` | ✅ | ✖ fail-closed | ✖ fail-closed |

Anything in `@zeroship/migrate/pg` is Postgres-only and fails closed on other dialects; anything
in `@zeroship/migrate` renders on all three (with the per-dialect mappings above).
