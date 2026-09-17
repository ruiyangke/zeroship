# The migration DSL — worked examples

A cookbook of shipped `@zeroship/migrate` syntax. The normative contract — every
operation's full argument set, the defaults, the enforcement rules and the error
envelopes — is [`migrate-op-dsl.md`](./migrate-op-dsl.md). This page shows the
examples and defers to that reference for the contract; where an example here
and the reference disagree, the reference is right.

PostgreSQL is the first-class target; the same source also renders SQLite and
MySQL, and a construct with no native realization on a target fails closed
rather than being silently dropped. `dialect({...})` supplies an explicit
per-target leg where the targets genuinely diverge.

There is one import root, `@zeroship/migrate`. A creator deploy rejects
privileged administrator primitives — roles, grants, schemas, extensions,
functions, triggers, row-level security and policies, materialized or raw views,
and `raw` — with `VENDOR_OP_DENIED`; the sections below mark them
**operator-only**. A creator migration does not name its schema: the deploy pins
one project schema, and an explicit `schema` naming anything else is refused with
`CROSS_SCHEMA`. The `schema:` qualifier appears below only in operator-only
examples. Refusals arrive as the structured authoring error documented under
[Error and finding envelopes](./migrate-op-dsl.md#error-and-finding-envelopes).

---

## 1. Migration module shape

One forward phase: `schema()` for DDL, or `data()` paired with a recorded
`inverse()` or a non-empty `irreversible` reason.

```ts
import { now, table, t, uuidV4 } from "@zeroship/migrate";

export default {
  name: "create_users",
  schema() {
    table("users").create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        email: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
  },
};
```

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

---

## 2. Tables

```ts
import { table } from "@zeroship/migrate";

// Create. A creator omits the schema qualifier (the deploy pins the project schema).
table("app_audit").create({
  columns: { /* … */ },
  primaryKey: ["id"],
});

// Rename: a direct ALTER TABLE … RENAME TO …, not an expand-contract.
table("orders").rename({ to: "purchase_orders" });

// Drop, with an optional existence guard.
table("orders").drop({ ifExists: true, cascade: true });

// Comment; pass null to clear it.
table("orders").comment("customer purchase orders");
table("orders").comment(null);
```

```ts
import { table, t } from "@zeroship/migrate";

// A handle is reusable, so it can be assigned once and used across statements.
const orders = table("orders");
orders.column("status").add({ type: t.text() });
orders.index("orders_status_idx").add({ on: ["status"] });
```

### Table runtime options

```ts
import { table } from "@zeroship/migrate";

table("posts").setOptions({ softDelete: true });
table("posts").setOptions({ softDelete: false });
table("posts").setOptions({ versioning: true });
table("posts").setOptions({ strictness: "strict" }); // strict | lenient | off
```

The same bag is available at create time as `create({ columns, options: { … } })`.

---

## 3. The column-type lexicon (`t.*`)

```ts
import { ids, t } from "@zeroship/migrate";

// Identity / keys
ids.typeId({ prefix: "usr" }).primaryKey()
ids.ulid().notNull().unique()
t.uuid()

// Text
t.text()                          // unbounded TEXT
t.string()                        // VARCHAR(255); t.string({ length: 32 })
t.text({ caseSensitive: false })  // emits the target's native case-insensitive spellings
t.char({ length: 3 })             // fixed-length CHAR(n)
t.textArray()                     // PG text[]; SQLite TEXT; MySQL JSON

// Numbers
t.smallInt()                      // int2
t.int()                           // int4
t.bigInt()                        // int8
t.real()                          // float4
t.double()                        // float8 — not an alias of t.real()
t.numeric({ precision: 12, scale: 2 }) // default (38, 9)

// Temporal
t.timestamp()
t.date()

// Other scalars
t.boolean()
t.json()                          // jsonb
t.bytes()                         // bytea / blob
t.inet()                          // PG inet

// Named types
t.enum("order_status")            // references an enum type (see §9)
t.domain("billing_period")        // references a domain (see §10)

// Search / spatial
t.vector({ dimensions: 1536, metric: "cosine" }) // cosine | l2 | innerProduct
t.geoPoint()

// Encryption wrapper (auto-masks the field unless .mask() overrides it)
t.encrypted({ of: t.text() })
```

### Bridging from the runtime schema

```ts
import { fromDb } from "@zeroship/migrate";

// `dbField` is a @zeroship/db field definition (a FieldDef) lifted from the app's
// env.db schema. required -> .notNull(), unique -> .unique(), and the result is
// still chainable:
const col = fromDb(dbField).comment("primary contact");
```

---

## 4. Column facets & defaults

```ts
import { t } from "@zeroship/migrate";

t.text().notNull()
t.uuid().notNull().primaryKey()
t.text().unique()
t.uuid().references("users", "id")            // single-column typed FK facet
t.uuid().references("users", "id", { onDelete: "cascade", name: "orders_user_fkey" })
t.bigInt().notNull().default(0)
t.char({ length: 3 }).notNull().default("usd")
t.text().collation("bytewise")                // an intent, not a collation name
```

### Default values — every form

```ts
import { byteValue, decimal, int64, nextval, now, t, uuidV4, uuidV7 } from "@zeroship/migrate";

// Scalars
t.bigInt().default(0)
t.text().default("pending")
t.boolean().default(true)

// Exact / typed scalars beyond a JavaScript number
t.bigInt().default(int64("9007199254740993"))
t.numeric({ precision: 12, scale: 2 }).default(decimal("0.01"))
t.bytes().default(byteValue(new Uint8Array([1, 2, 3])))

// Database-evaluated value constructors
t.uuid().default(uuidV4())
t.uuid().default(uuidV7())
t.timestamp().default(now())

// Empty containers
t.json().default({})            // '{}'::jsonb on PostgreSQL
t.json().default([])            // '[]'::jsonb on PostgreSQL
t.textArray().default([])       // PG '{}'::text[]; MySQL JSON_ARRAY(); SQLite refuses

// Arbitrary JSON value default (integer leaves only)
t.json().notNull().default({ max_sockets: 4, egress_ceiling_bytes: 10485760 })

// Sequence-backed default (PostgreSQL)
t.bigInt().notNull().default(nextval("orders_id_seq"))
```

### Auto-incrementing keys

```ts
import { t } from "@zeroship/migrate";

// Portable identity-by-default:
// PG GENERATED BY DEFAULT AS IDENTITY, SQLite AUTOINCREMENT, MySQL AUTO_INCREMENT.
t.int().autoIncrement()

// PG identity with explicit options
t.bigInt().identity()                 // GENERATED BY DEFAULT AS IDENTITY
t.bigInt().identity({ always: true }) // GENERATED ALWAYS AS IDENTITY (PostgreSQL only)
```

### Generated columns, masking, encryption

```ts
import { concatWs, t } from "@zeroship/migrate";

// Generated column: STORED by default. { virtual: true } is SQLite-only.
t.text().generated((col) => concatWs(" ", col("first"), col("last")))
t.text().generated((col) => col("a").add(col("b")), { virtual: true })

// Standalone mask; kind is required and classification defaults to "pii".
t.text().mask({ kind: "email" })
t.text().mask({ kind: "last4", classification: "pci" })

// Encryption wrapper
t.encrypted({ of: t.text() }).notNull()
```

---

## 5. Altering columns (per-intent terminals)

Each change is its own single-intent terminal — there is no `.alter({…})` bag.

```ts
import { table, t } from "@zeroship/migrate";

table("users").column("bio").add({ type: t.text() });        // add a new column
table("users").column("bio").drop();                          // drop it
table("users").column("bio").rename({ to: "biography", type: t.text() });

table("users").column("age").setType({ to: t.bigInt() });     // { using } adds a cast
table("users").column("email").setNotNull();                  // SET NOT NULL
table("users").column("email").dropNotNull();                 // DROP NOT NULL
table("users").column("status").setDefault("active");         // SET DEFAULT
table("users").column("status").dropDefault();                // DROP DEFAULT
table("users").column("email").comment("primary contact");
```

Adding a column and backfilling it in one flow:

```ts
import { table, t } from "@zeroship/migrate";

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
import { table, t } from "@zeroship/migrate";

// In create()
table("items").create({ columns: { /* … */ }, primaryKey: ["id"] });
table("items").create({ columns: { /* … */ }, primaryKey: ["tenant_id", "id"] }); // composite
table("items").create({ columns: { id: t.uuid().primaryKey() } });               // shorthand

// Explicit lifecycle for an existing primary key. `expectedColumns` is an
// ordered drift precondition, not introspection.
table("items").primaryKey().add({ columns: ["id"] });
table("items").primaryKey().replace({ expectedColumns: ["id"], columns: ["tenant_id", "id"] });
table("items").primaryKey().drop({ expectedColumns: ["tenant_id", "id"] });
```

### Unique

```ts
import { table } from "@zeroship/migrate";

table("users").unique("users_email_key").add({ columns: ["email"] });
```

### Check

```ts
import { table } from "@zeroship/migrate";

// The named selector form; supports notValid / ifNotExists / schema.
table("orders").check("orders_qty_positive").add({ expr: (col) => col("qty").gt(0) });
```

Inside `create`, table-level uniques and checks are named fields:

```ts
import { check, table } from "@zeroship/migrate";

table("members").create({
  columns: { /* … */ },
  uniques: [{ name: "members_org_email_uq", columns: ["org_id", "email"] }],
  checks: [check("members_role_nonempty", (col) => col("role").ne(""))],
});
```

### Foreign keys

```ts
import { table } from "@zeroship/migrate";

// Single-column, referencing id by convention.
table("posts").foreignKey("posts_author_fkey").add({
  columns: ["author_id"],
  references: { table: "users", columns: ["id"] },
  onDelete: "cascade",
});

// Composite, deferrable.
table("usage_aggregates")
  .foreignKey("usage_aggregates_metric_fkey").add({
    columns: ["metric"],
    references: { table: "billing_metrics", columns: ["metric"] },
    deferrable: true,
    initiallyDeferred: true,
  });

// Composite with both referential actions.
table("line_items").foreignKey("line_items_order_fkey").add({
  columns: ["order_id", "tenant_id"],
  references: { table: "orders", columns: ["id", "tenant_id"] },
  onDelete: "restrict",
  onUpdate: "cascade",
});
```

### Exclusion constraints (PG)

```ts
import { table } from "@zeroship/migrate";

table("reservations").exclusion("no_overlap").add({
  using: "gist", // gist | spgist | btree
  elements: [{ target: "room_id", operator: "=" }, { target: "during", operator: "&&" }],
  deferrable: true,
});
```

### Dropping / commenting a constraint by name

```ts
import { table } from "@zeroship/migrate";

table("orders").constraint("orders_qty_positive").drop({ ifExists: true });
table("orders").constraint("orders_pkey").comment("surrogate key");
```

---

## 7. Indexes

```ts
import { table } from "@zeroship/migrate";

// Basic
table("app_members").index("app_members_user_idx").add({ on: ["user_id"] });

// Composite + partial (PostgreSQL and SQLite; MySQL has no partial indexes)
table("app_session_anchors").index("app_session_anchors_user_idx").add({
  on: ["app_id", "global_user_id"],
  where: (col) => col("revoked_at").isNull(),
});

// Unique
table("users").index("users_email_uq").add({ on: ["email"], unique: true });

// Access method: btree | brin | gin | gist | ivfflat | hnsw
table("docs").index("docs_body_gin").add({ on: ["body"], using: "gin" });
table("events").index("events_ts_brin").add({ on: ["occurred_at"], using: "brin" });
table("embeddings").index("embeddings_vec").add({ on: ["vec"], using: "hnsw" });

// Per-column ASC/DESC ordering and operator class
table("posts").index("posts_created_desc").add({ on: [{ column: "created_at", order: "desc" }] });
table("users").index("users_email_pattern").add({ on: [{ column: "email", opclass: "text_pattern_ops" }] });

// Expression element (no order/opclass/collation/null-ordering on an expression)
table("users").index("users_lower_email").add({ on: [{ expr: (col) => col("email").lower() }] });

// Covering (INCLUDE) + storage parameters + ONLY (don't recurse into partitions)
table("orders").index("orders_customer_idx").add({
  on: ["customer_id"],
  include: ["total", "status"],
  only: true,
  postgres: { fillfactor: 90 }, // vendor namespace; install zero-migrate-postgres to typecheck it
});

// Drop / comment
table("orders").index("orders_customer_idx").drop({ ifExists: true });
```

A backend option namespace such as `postgres: { … }` is contributed by an
installed vendor package (`zero-migrate-postgres`).

---

## 8. Expressions — the `(col) => Expr` builder

```ts
// Comparisons
(col) => col("qty").gt(0)
(col) => col("price").ge(100)
(col) => col("status").eq("active")
(col) => col("deleted_at").ne(null)
(col) => col("score").lt(50)
(col) => col("score").le(50)
(col) => col("qty").between(1, 10)
(col) => col("name").like("A%")
(col) => col("a").distinctFrom(col("b"))

// Null / boolean tests
(col) => col("revoked_at").isNull()
(col) => col("email").isNotNull()
(col) => col("enabled").isTrue()
(col) => col("archived").isFalse()

// Logical composition
(col) => col("qty").gt(0).and(col("price").ge(0))
(col) => col("a").isNotNull().or(col("b").isNotNull())
(col) => col("blocked").isTrue().not()

// Set membership (homogeneous scalar lists)
(col) => col("status").in(["active", "past_due", "suspended"])
(col) => col("state").notIn(["deleted", "purged"])

// Arithmetic + string concatenation (|| , NULL-propagating)
(col) => col("a").add(col("b"))
(col) => col("total").sub(col("discount"))
(col) => col("qty").mul(col("unit_price"))
(col) => col("num").div(col("den"))
(col) => col("first").concat(col("last"))

// PostgreSQL-first operators: fail closed off-target unless wrapped in
// dialect({...}).
(col) => col("email").regex("^[^@]+@[^@]+$")      // ~ on PG, REGEXP on MySQL
(col) => col("payload").columnSize().lt(1048576)  // pg_column_size < 1 MiB

// Cast to the closed scalar target set
(col) => col("app_id").cast({ to: "uuid" })       // text | int | real | boolean | bytes | uuid
```

### Scalar chain methods and `concatWs`

```ts
import {
  concatWs, countStar, currentSetting, currentUser, interval, lit, now, uuidV4,
} from "@zeroship/migrate";

(col) => col("email").lower()
(col) => col("code").upper()
(col) => col("name").trim()
(col) => col("bio").length()
(col) => col("delta").abs()
(col) => col("nick").coalesce(col("name"))
(col) => col("a").nullif(col("b"))
(col) => col("n").mod(2)
(col) => col("x").round(2)
(col) => col("x").floor()
(col) => col("x").ceil()
(col) => col("s").substr(1, 3)
(col) => col("s").replace("a", "b")
(col) => col("ts").extract("year") // year | month | day | hour | epoch | …
(col) => concatWs(" ", col("first"), col("last"))
(col) => col("path").splitPart("/", 1)
(col) => now()
(col) => uuidV4()

// Searched CASE with explicit when/then branches and an optional else.
(col) => col.case({ branches: [{ when: col("n").gt(0), then: lit("pos") }], else: lit("nonpos") })

// PostgreSQL-first value constructors; the validator fails closed off-target.
(col) => currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" })
(col) => currentUser()
(col) => col("expires_at").le(col("created_at").add(interval({ days: 3 })))

// Aggregates (views and having clauses). The PG-first four take dialect({...})
// to target SQLite/MySQL.
(col) => countStar()
(col) => col("id").count({ distinct: true })
(col) => col("amount").sum()
(col) => col("amount").avg()
(col) => col("item_name").stringAgg(", ")
(col) => col("id").arrayAgg()
(col) => col("fulfilled").boolAnd()
```

`dialect()` is the explicit portability escape. In value position the legs are
expressions; in statement position they are thunks:

```ts
import { currentUser, dialect, table } from "@zeroship/migrate";

// Value position
table("audit").update({
  set: {
    actor: dialect({ postgres: currentUser(), sqlite: "system", mysql: "system" }),
  },
});

// Statement position
dialect({
  postgres: () =>
    table("docs").index("docs_embedding_hnsw_idx").add({ on: ["embedding"], using: "hnsw" }),
});
```

### Literals & helpers

```ts
import {
  countStar, currentSetting, currentUser, decimal, dialect, int64, interval, lit,
  maxValue, minValue, nextval, now, perRow, uuidV4, uuidV7,
} from "@zeroship/migrate";

lit(42)                       // an explicit literal node
minValue                      // partition-bound sentinel (see §12)
maxValue                      // partition-bound sentinel (see §12)
nextval("orders_id_seq")      // a sequence-backed default (see §11)
perRow.uuidV7()               // a per-row generator (backfill only)
```

---

## 9. Enum types

```ts
import { enumType, table, t } from "@zeroship/migrate";

enumType("order_status").create({
  values: ["pending", "paid", "shipped"], // must be non-empty
});
enumType("order_status").comment("lifecycle of an order");
enumType("order_status").drop({ ifExists: true });

table("orders").create({
  columns: { status: t.enum("order_status").notNull() },
  primaryKey: ["id"],
});
```

---

## 10. Domains

The check callback receives the domain **value**, not a column accessor.

```ts
import { domain, table, t } from "@zeroship/migrate";

domain("account_state").create({
  as: t.text(),
  check: (v) => v.in(["active", "past_due", "suspended"]), // the VALUE, not a column
});

domain("billing_period").create({ as: t.date() });
domain("account_state").comment("tenant account lifecycle");
domain("account_state").drop({ ifExists: true });

table("spend_state").create({
  columns: { state: t.domain("account_state").notNull().default("active") },
  primaryKey: ["app_id"],
});
```

---

## 11. Sequences & `nextval`

```ts
import { nextval, sequence, table, t } from "@zeroship/migrate";

sequence("orders_id_seq").create({ start: 1, increment: 1 });
sequence("orders_id_seq").alter({ restart: 1000 });
sequence("orders_id_seq").drop({ ifExists: true });

// Wire a sequence to a column default
table("orders").create({
  columns: { id: t.bigInt().notNull().default(nextval("orders_id_seq")) },
  primaryKey: ["id"],
});
```

---

## 12. Partitioning (PG)

```ts
import { maxValue, minValue, table, t } from "@zeroship/migrate";

// 1) Parent declares the strategy at create().
table("sandbox_events").create({
  columns: { id: t.uuid().notNull(), occurred_at: t.timestamp().notNull() },
  primaryKey: ["id", "occurred_at"],
  partitionBy: { range: ["occurred_at"] }, // { range } | { list } | { hash }
});

// 2) Child partitions (parent-subject). RANGE bounds:
table("sandbox_events")
  .partition("sandbox_events_2026_05")
  .create({ from: ["2026-05-01 00:00:00+00"], to: ["2026-06-01 00:00:00+00"] });

// LIST bounds:
table("events").partition("events_eu").create({ in: ["de", "fr", "es"] });

// HASH bounds:
table("events").partition("events_h0").create({ modulus: 4, remainder: 0 });

// DEFAULT partition (catch-all):
table("sandbox_events")
  .partition("sandbox_events_default").create({ default: true });

// Unbounded range ends use the sentinels:
table("events").partition("events_head").create({ from: [minValue], to: ["2026-01-01"] });
table("events").partition("events_tail").create({ from: ["2027-01-01"], to: [maxValue] });

// Lifecycle: detach and drop (both parent-subject).
table("sandbox_events").partition("sandbox_events_2026_05").detach();
table("sandbox_events").partition("sandbox_events_2026_05").drop();

// Attach an existing table as a partition (operator-only; names the platform schema).
table("events", { schema: "zeroship" }).partition("events_eu").attach({ in: ["de"] });
```

A parent declared with
`partitionBy: { range: ["occurred_at"], whenUnsupported: "collapse" }` degrades
each child to a plain table on SQLite and MySQL instead of failing.

**Faithfulness note:** create indexes on the *parent* (without `only: true`) —
PostgreSQL propagates them to the children and attaches the child indexes
automatically.

---

## 13. Views

Two forms: the portable structured builder (creator-usable) and a raw
`{ as: { raw } }` escape, which — like `materialized: true` — is operator-only.

```ts
import { countStar, view } from "@zeroship/migrate";

// Structured (portable): the `as` callback receives a fluent query builder.
view("active_users").create({
  as: (q) =>
    q.from("users")
      .select(["id", "email"])
      .where((col) => col("deleted_at").isNull())
      .orderBy(["created_at"])
      .limit(100),
});

// Joins and grouped aggregation; materialized: true makes it a materialized view
// (operator-only).
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

// PostgreSQL-first aggregate coverage. SQLite/MySQL fail closed unless the
// expression is wrapped in dialect({...}) with explicit non-PG legs.
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

// Raw view body for constructs outside the structured builder (operator-only).
view("legacy_report").create({
  as: { raw: "SELECT a.id, count(b.*) FROM a JOIN b USING (id) GROUP BY 1" },
});
```

Join predicates use the two-argument column form, e.g.
`(col) => col("orders", "customer_id").eq(col("customers", "id"))`.

---

## 14. Row-level security & policies

**Operator-only.** A creator deploy rejects these with `VENDOR_OP_DENIED`.

```ts
import { currentSetting, table } from "@zeroship/migrate";

// Table-scoped RLS state (set at least one of enabled / forced).
table("apps", { schema: "zeroship" }).setRls({ enabled: true, forced: true });
table("apps", { schema: "zeroship" }).setRls({ enabled: false, forced: false });

table("apps", { schema: "zeroship" }).policy("tenant_isolation").create({
  using: (col) => col("app_id").eq(
    currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" }),
  ),
  withCheck: (col) => col("app_id").eq(
    currentSetting("zeroship.tenant_app", { missingOk: true }).cast({ to: "uuid" }),
  ),
});
table("apps", { schema: "zeroship" }).policy("tenant_isolation").drop();
```

---

## 15. Triggers

**Operator-only.** A creator deploy rejects these with `VENDOR_OP_DENIED`.

```ts
import { table } from "@zeroship/migrate";

// Execute an existing function (PostgreSQL renders only this form).
table("app_audit", { schema: "zeroship" }).trigger("app_audit_block_delete").create({
  timing: "before",            // before | after | insteadOf
  events: ["delete"],          // insert | update | delete | truncate (array)
  forEach: "row",              // row | statement
  execute: "app_audit_block_tamper",
});

// Author a structured body (SQLite and MySQL render this form).
table("orders").trigger("orders_block_final").create({
  timing: "before",
  events: ["delete"],
  forEach: "row",
  body: (b) => [b.raise({ level: "fail", message: "invoices are immutable" })],
});

table("app_audit", { schema: "zeroship" }).trigger("app_audit_block_delete").drop();
```

---

## 16. Functions

**Operator-only.** A creator deploy rejects these with `VENDOR_OP_DENIED`.

```ts
import { createFunction, dropFunction } from "@zeroship/migrate";

createFunction({
  name: "app_audit_block_tamper",
  schema: "zeroship",
  returns: "trigger",
  language: "procedural", // procedural | sql
  body: "BEGIN RAISE EXCEPTION 'audit rows are immutable'; END;",
});

// Optional args, replace, and volatility (volatile | stable | immutable).
createFunction({
  name: "is_positive",
  returns: "boolean",
  language: "sql",
  volatility: "immutable",
  args: [{ name: "n", type: "integer" }],
  body: "SELECT $1 > 0",
});

dropFunction({ name: "app_audit_block_tamper", ifExists: true });
```

---

## 17. Schemas, extensions, roles, grants

**Operator-only.** A creator deploy rejects these with `VENDOR_OP_DENIED`.

```ts
import {
  dropOwnedBy, extension, grant, revoke, role, schema,
} from "@zeroship/migrate";

schema("zeroship").create();
schema("zeroship").drop({ cascade: true });

extension("citext").create();
extension("vector").create({ schema: "zeroship" });
extension("citext").drop({ ifExists: true });

role("app_rw").create({ login: false });
role("app_rw").setOptions({ setSearchPath: ["zeroship"] });
role("app_rw").drop({ ifExists: true });
dropOwnedBy({ roles: ["app_rw"] }); // `roles` is an array

// `on` is a tagged target; `to` / `from` are string arrays.
grant({
  to: ["app_rw"],
  on: { kind: "table", names: ["orders"], schema: "zeroship" },
  privileges: ["select", "insert"],
});
revoke({
  from: ["app_rw"],
  on: { kind: "schema", names: ["zeroship"] },
  privileges: ["usage"],
});
```

---

## 18. Data & backfills (DML)

```ts
import { lit, perRow, table } from "@zeroship/migrate";

// Insert — `rows` is a row object or an array, with an optional ON CONFLICT.
table("plans").insert({
  rows: [
    { id: "free", name: "Free" },
    { id: "pro", name: "Pro" },
  ],
  onConflict: { columns: ["id"], doUpdate: { name: "Pro" } },
});

// Update — `set` values are scalars or expressions.
table("plans").update({
  set: { name: "Professional" },
  where: (col) => col("id").eq("pro"),
});
table("plans").update({
  set: { name: (col) => lit("Professional") },
  where: (col) => col("id").eq("pro"),
});

// Delete — `where` is mandatory (no unfiltered delete).
table("plans").delete({ where: (col) => col("id").eq("legacy") });

// Backfill a column with an expression (chunked). The ordered cursorColumns
// tuple pages large tables; cursorStability keeps those components immutable.
table("users").backfill({
  set: { display_name: (col) => col("nickname").coalesce(col("name")) },
  cursorColumns: ["id"],
  cursorStability: { mode: "guardUpdates" },
});

// A backfill may mint a fresh value per row with a perRow generator.
table("users").backfill({
  set: { public_id: perRow.uuidV7() },
  cursorColumns: ["id"],
  cursorStability: { mode: "externalInvariant", name: "users.id is write-frozen" },
});
```

---

## 19. Comments (any object)

Comments render on PostgreSQL only.

```ts
import { comment } from "@zeroship/migrate";

comment({ kind: "table", name: "orders" }, "customer orders");
comment({ kind: "column", table: "orders", name: "total" }, "cents");
comment({ kind: "type", name: "order_status" }, "order lifecycle");
```

Most handles also carry a `.comment(text | null)` shortcut; pass `null` to clear
a comment.

---

## 20. The raw escape hatch

**Operator-only.** A creator deploy rejects `raw` with `VENDOR_OP_DENIED`.

`raw` is the last resort for genuinely unrepresentable DDL. It requires a
`reason`, and the reason travels with the statement. There are no binds — `raw`
takes a complete SQL string.

```ts
import { raw } from "@zeroship/migrate";

raw({
  sql:
    "CREATE TRIGGER t BEFORE UPDATE OF sector_identifier ON zeroship.app_oauth_clients " +
    "FOR EACH ROW EXECUTE FUNCTION zeroship.reject_sector_change()",
  reason: "column-scoped UPDATE OF is outside the current structured trigger surface",
});
```

If you find yourself reaching for `raw` for a shape the structured surface
*should* cover, that is a gap to close in the DSL, not a license to accumulate
raw SQL.

---

## 21. Determinism lint

```ts
import { lintDeterminism } from "@zeroship/migrate";

// Best-effort source scan for non-deterministic authoring — Date.now(),
// Math.random(), crypto.randomUUID(), new Date(). Returns findings; the
// advisory code is NONDETERMINISTIC_OP_ARG.
const findings = lintDeterminism(sourceText);
```

Use the database-evaluated constructors (`now()`, `uuidV4()`, `uuidV7()`) or the
bare native symbol (`Date.now`, `Math.random`, `crypto.randomUUID`, no
parentheses) instead of baking a build-time value into a migration.

---

## Portability at a glance

| Construct | PostgreSQL | SQLite | MySQL |
| --- | --- | --- | --- |
| Core tables, columns, scalar types | ✅ | ✅ | ✅ |
| Portable expressions, chain methods, `concatWs` | ✅ | ✅ | ✅ |
| `t.text({ caseSensitive: false })` | `citext` | `COLLATE NOCASE` | `utf8mb4_0900_ai_ci` |
| `t.textArray()` | `text[]` | `TEXT` | `JSON` |
| JSON / empty-container defaults | `::jsonb` / `::text[]` | `'{}'` / `'[]'` text | `JSON_OBJECT()` / `JSON_ARRAY()` / `CAST(… AS JSON)` |
| `t.textArray().default([])` | ✅ | ✖ | ✅ |
| `autoIncrement()` · `identity()` | identity column | sole-PK `AUTOINCREMENT` | sole-PK `AUTO_INCREMENT` |
| `identity({ always: true })` | ✅ | ✖ | ✖ |
| Deferrable foreign key | ✅ | ✖ | ✖ |
| Enums | native type | `TEXT` + CHECK | inline `ENUM` |
| Domains (named type + CHECK) | ✅ | base type inlined; CHECK is PG-only | base type inlined; CHECK is PG-only |
| Structured views | ✅ | ✅ | ✅ |
| Materialized views | ✅ | ✖ | ✖ |
| Comments | ✅ | ✖ | ✖ |
| Sequences · `nextval` defaults | ✅ | ✖ | ✖ |
| Partitioning | ✅ | collapse or ✖ | collapse or ✖ |
| Triggers | `execute` only | `body` only | `body` only |
| RLS · policies | ✅ | ✖ | ✖ |
| Roles · grants · schemas · extensions · functions · `raw` | ✅ | ✖ | ✖ |

A ✖ means the construct is refused with a structured error, not silently
skipped. Realization names in the table are the native type / collation
spellings the engine emits; you author the intent, not the name.
