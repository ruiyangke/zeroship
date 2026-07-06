// STRUCTURAL type-safety, names-stay-strings (BINDING, §1/§3) — fluent-only.
//
// This file is a TYPE-LEVEL test: checked by `tsc --noEmit` via
// `tsconfig.types.json` (the `typecheck:types` script). It compiles cleanly IFF
// every `@ts-expect-error` line is a genuine type error and every other line is
// well-typed. A removed/weakened structural guard makes a `@ts-expect-error` line
// stop erroring → `tsc` fails with "Unused '@ts-expect-error' directive".
//
// The guarantee under test:
//   - the fluent op SHAPES (`table().…`), the `t.*` ColType builder, the
//     fluent-expression node shapes, and insert-row VALUE shapes ARE structurally
//     typed (a malformed shape / invalid ColType FAILS tsc);
//   - table/column NAMES are plain `string`, NOT bound to the live `@zeroship/db`
//     schema (a migration naming a non-existent table/column TYPE-CHECKS cleanly —
//     existence is an apply-time check, the anti-rot guarantee).

import { t as dbT } from "@zeroship/db";
// @ts-expect-error — free boolean combinators are no longer exported from the public package.
import { and as removedPkgAnd, or as removedPkgOr, not as removedPkgNot } from "@zeroship/migrate";
// @ts-expect-error — free policy helpers were deleted; use pgTable(...).policy(name).create/drop().
import { createPolicy as removedPkgCreatePolicy, dropPolicy as removedPkgDropPolicy } from "@zeroship/migrate/pg";
// @ts-expect-error — flat named-object lifecycle helpers were deleted; use schema/extension/role handles.
import { dropSchema as removedPkgDropSchema, dropExtension as removedPkgDropExtension, alterRole as removedPkgAlterRole, dropRole as removedPkgDropRole } from "@zeroship/migrate/pg";

import * as migrate from "../../src/index.js";
import {
  colTypeFromDbField,
  fromDb,
  t,
  table,
  view,
  check,
  lit,
  decimal,
  byteValue,
  type ColumnDef,
  type CheckDef,
  type DbFieldType,
  type DecimalValue,
  type BytesValue,
} from "../../src/index.js";
// @ts-expect-error — free boolean combinators are no longer exported; use chain `.and`/`.or`/`.not`.
import { and as removedAnd, or as removedOr, not as removedNot } from "../../src/index.js";
import { domain, pgTable } from "../../src/pg.js";
// The internal closed-set validation arrays (NOT part of the public `index.ts`
// surface) — imported directly for the LOW-2 element-typing assertion below.
import { MASK_CLASSIFICATIONS, MASK_KINDS, VECTOR_METRICS } from "../../src/ops.js";
import type { Classification, MaskKind, VectorMetric } from "../../src/generated/ir.js";

// ───────────────────────────────────────────────────────────────────────────
// 1. NAMES STAY STRINGS — the anti-rot guarantee (§1/§3).
// ───────────────────────────────────────────────────────────────────────────

export function antiRotMigration(): void {
  // `nonexistent_table` / `column_that_was_dropped` / `legacy_col` are not in any
  // live schema — yet every op below compiles, because names are plain strings.
  table("nonexistent_table").create({
    columns: {
      legacy_col: t.text(),
      author_id: t.ref("a_table_that_does_not_exist"),
    },
  });
  table("nonexistent_table").column("column_that_was_dropped").add({ type: t.text() });
  table("nonexistent_table").column("column_that_was_dropped").drop();
  table("nonexistent_table").column("legacy_col").setNotNull();
  table("nonexistent_table").foreignKey("fk").add({
    columns: ["author_id"],
    references: { table: "another_missing_table", columns: ["id"] },
  });
  table("nonexistent_table").foreignKey("composite_fk").add({
    columns: ["tenant_id", "author_id"],
    references: { schema: "ghost", table: "another_missing_table", columns: ["tenant_id", "author_id"] },
  });
  table("nonexistent_table").update({
    set: { legacy_col: (c) => c("a_column_no_schema_declares").concat(" suffix") },
    where: (c) => c("yet_another_missing_column").isNull(),
  });
  table("nonexistent_table").delete({ where: (c) => c("phantom_col").eq(1) });
  table("nonexistent_table").backfill({
    set: { legacy_col: (c) => c.fn.splitPart(c("phantom_col"), " ", 1) },
    where: (c) => c("phantom_col").isNotNull(),
  });
  table("nonexistent_table").insert({ rows: { phantom_col: "ok", another_phantom: 42 } });
}

// ───────────────────────────────────────────────────────────────────────────
// 2. Op SHAPES are structurally typed — a malformed shape FAILS tsc.
// ───────────────────────────────────────────────────────────────────────────

export function badOpShapes(): void {
  // @ts-expect-error — .column().add()'s `type` must be a ColumnDef, not a number.
  table("users").column("age").add({ type: 123 });

  // @ts-expect-error — .column().add()'s `type` must be a ColumnDef, not a bare string.
  table("users").column("age").add({ type: "int" });

  // @ts-expect-error — .column().add() requires the `type` field.
  table("users").column("age").add({});

  // @ts-expect-error — create columns must be a Record of ColumnDef, not numbers.
  table("users").create({ columns: { age: 123 } });

  // @ts-expect-error — table() name must be a string, not a number.
  table(42).column("age").add({ type: t.text() });

  // @ts-expect-error — .foreignKey().add() needs `references`, not a bare columns list.
  table("orders").foreignKey("fk").add({ columns: ["user_id"] });

  // @ts-expect-error — `delete` requires a `where` predicate (mandatory).
  table("users").delete({});

  // @ts-expect-error — bigint is not an authored scalar; use decimal("<n>").
  table("users").update({ set: { name: 1n } });

  // @ts-expect-error — the table-level `.rename({ to })` REQUIRES a `to` string.
  table("users").rename({});

  // @ts-expect-error — `.rename({ to })` has no `from` (that is the column-rename shape).
  table("users").rename({ from: "users", to: "people" });
}

export function pgTableBoundary(): void {
  // @ts-expect-error — PG table policies are only reachable through `pgTable()`.
  table("secrets").policy("tenant_only").create({ using: (c) => c("tenant_id").isNotNull() });

  // @ts-expect-error — deleted direct table trigger method; use `.trigger(name).create(...)`.
  table("audit_events").createTrigger({ name: "audit_events_trg", timing: "before", events: ["insert"], forEach: "row", execute: "audit_events_fn" });

  // @ts-expect-error — deleted direct table trigger method; use `.trigger(name).drop(...)`.
  table("audit_events").dropTrigger({ name: "audit_events_trg", ifExists: true });

  // @ts-expect-error — RLS is a PG table method and is not on portable `table()`.
  table("secrets").setRls({ enabled: true });

  // @ts-expect-error — exclusion constraints are a PG table method and are not on portable `table()`.
  table("bookings").exclusion("bookings_no_overlap");

  // @ts-expect-error — direct constraint validation method was deleted; use `pgTable(...).constraint(name).validate()`.
  table("line_items").validateConstraint("line_items_order_fkey");

  // @ts-expect-error — direct partition detach method was deleted; use `pgTable(...).partition(name).detach()`.
  table("events").detachPartition("events_2026_05");

  // @ts-expect-error — constraint validate is PG-only and only on the PG constraint ref.
  table("line_items").constraint("line_items_order_fkey").validate();

  table("audit_events").trigger("audit_events_trg").create({
    timing: "before",
    events: ["insert"],
    forEach: "row",
    execute: "audit_events_fn",
  });
  table("audit_events").trigger("audit_events_trg").drop({ ifExists: true });

  // @ts-expect-error — deleted direct PG policy method; use `.policy(name).create(...)`.
  pgTable("secrets").createPolicy({ name: "tenant_only", using: (c) => c("tenant_id").isNotNull() });

  // @ts-expect-error — deleted direct PG policy method; use `.policy(name).drop(...)`.
  pgTable("secrets").dropPolicy({ name: "tenant_only", ifExists: true });

  pgTable("secrets")
    .setRls({ enabled: true, forced: true })
    .policy("tenant_only").create({ using: (c) => c("tenant_id").isNotNull() })
    .policy("tenant_only").drop({ ifExists: true })
    .setRls({ enabled: false, forced: false });
  // @ts-expect-error — deleted RLS method; use `.setRls({ enabled: true })`.
  pgTable("secrets").enableRowLevelSecurity();
  // @ts-expect-error — deleted RLS method; use `.setRls({ forced: true })`.
  pgTable("secrets").forceRowLevelSecurity();
  // @ts-expect-error — deleted RLS method; use `.setRls({ enabled: false })`.
  pgTable("secrets").disableRowLevelSecurity();
  // @ts-expect-error — deleted RLS method; use `.setRls({ forced: false })`.
  pgTable("secrets").noForceRowLevelSecurity();
  pgTable("bookings").exclusion("bookings_no_overlap").add({
    using: "gist",
    elements: [{ target: "room_id", operator: "=" }],
  });
  // @ts-expect-error — deleted direct PG constraint validation method; use `.constraint(name).validate(...)`.
  pgTable("line_items").validateConstraint("line_items_order_fkey");
  pgTable("line_items").constraint("line_items_order_fkey").validate();
  // @ts-expect-error — deleted direct PG partition detach method; use `.partition(name).detach(...)`.
  pgTable("events").detachPartition("events_2026_05", { concurrently: true });
  pgTable("events").partition("events_2026_05").detach({ concurrently: true });
  pgTable("events").partition("events_2026_06").attach({
    from: ["2026-06-01T00:00:00Z"],
    to: ["2026-07-01T00:00:00Z"],
  });
}

export function viewGrammar(): void {
  view("active_users").create({
    as: (q) => q.from("users").select(["id", "email"]),
  });
  view("recent_users").create({
    as: { raw: "SELECT id, email FROM users WHERE deleted_at IS NULL" },
  });

  // @ts-expect-error — deleted duplicate spelling; use `.create({ as: { raw } })`.
  view("recent_users").createRaw({ sql: "SELECT id, email FROM users" });
}

export function partitionGrammar(): void {
  table("events").create({
    columns: { created_at: t.timestamp() },
    partitionBy: { range: ["created_at"], whenUnsupported: "collapse" },
  });
  table("events").partition("events_2026").create({
    from: ["2026-01-01"],
    to: ["2027-01-01"],
  });
  table("events").partition("events_default").create({ default: true });
  table("events").partition("events_2026").drop({ ifExists: true, cascade: true });

  // @ts-expect-error - whenUnsupported is an explicit P12 affirmation and only accepts "collapse".
  table("bad").create({ columns: { created_at: t.timestamp() }, partitionBy: { range: ["created_at"], whenUnsupported: "skip" } });

  // @ts-expect-error - null list bounds are outside the closed partition-bound value type.
  table("bad").partition("bad_null").create({ in: [null] });

  // @ts-expect-error - deleted `p` builder namespace; use `partitionBy: { range: [...] }`.
  table("bad").create({ columns: { created_at: t.timestamp() }, partitionBy: migrate.p.range(["created_at"]) });

  // @ts-expect-error - deleted free `partition(name).of(parent)` grammar; use `table(parent).partition(name)`.
  migrate.partition("events_2026").of("events");

  // @ts-expect-error - deleted free `dropPartition`; use `table(parent).partition(name).drop()`.
  migrate.dropPartition("events_2026");
}

export function indexGrammar(): void {
  table("users").index("users_email_idx").add({
    on: [
      "email",
      { column: "created_at", order: "desc" },
      { expr: (c) => c.fn.lower(c("email")) },
    ],
    unique: true,
  });

  pgTable("users").index("users_email_idx").add({
    on: [
      "email",
      { column: "created_at", order: "desc", opclass: "timestamp_ops", collation: "C", nulls: "last" },
      { expr: (c) => c.fn.lower(c("email")) },
    ],
    using: "gin",
    where: (c) => c("active").isTrue(),
    include: ["id"],
    with: { fillfactor: 90 },
    only: true,
    unique: true,
    nullsNotDistinct: true,
  });

  table("users").create({
    columns: { email: t.text(), created_at: t.timestamp() },
    indexes: [{ name: "users_email_idx", on: ["email"] }],
  });

  const oldColumnsArgs = { ["columns"]: ["email"] };
  // @ts-expect-error — `.index().add()` uses `on`, not `columns`.
  table("users").index("bad_columns").add(oldColumnsArgs);

  const indexRef = table("users").index("bad_chain");
  // @ts-expect-error — index modifiers live in `.add({ ... })`, not chain methods.
  indexRef.using;

  const oldKindKey = "kind";
  const oldExprKind = "expr";
  // @ts-expect-error — tagged expression elements are not part of the authored grammar.
  table("users").index("bad_tagged_expr").add({ on: [{ [oldKindKey]: oldExprKind, expr: (c) => c("email") }] });

  // @ts-expect-error — bare expression elements must be wrapped as `{ expr }`.
  table("users").index("bad_bare_expr").add({ on: [(c) => c("email")] });

  const oldColumnKind = "column";
  // @ts-expect-error — column object elements use `{ column }`, not `{ kind, name }`.
  table("users").index("bad_tagged_column").add({ on: [{ [oldKindKey]: oldColumnKind, name: "email" }] });

  // @ts-expect-error — `using` is PG-vendor and only reachable through `pgTable().index()`.
  table("users").index("bad_using").add({ on: ["email"], using: "gin" });

  // @ts-expect-error — partial-index `where` is PG-vendor and only reachable through `pgTable().index()`.
  table("users").index("bad_where").add({ on: ["email"], where: (c) => c("active").isTrue() });

  // @ts-expect-error — covering `include` is PG-vendor and only reachable through `pgTable().index()`.
  table("users").index("bad_include").add({ on: ["email"], include: ["id"] });

  // @ts-expect-error — storage params are PG-vendor and only reachable through `pgTable().index()`.
  table("users").index("bad_with").add({ on: ["email"], with: { fillfactor: 90 } });

  // @ts-expect-error — partition-recursion `only` is PG-vendor and only reachable through `pgTable().index()`.
  table("users").index("bad_only").add({ on: ["email"], only: true });

  // @ts-expect-error — `nullsNotDistinct` is PG-vendor and only reachable through `pgTable().index()`.
  table("users").index("bad_nulls_not_distinct").add({ on: ["email"], unique: true, nullsNotDistinct: true });

  // @ts-expect-error — element `opclass` is PG-vendor and only reachable through `pgTable().index()`.
  table("users").index("bad_opclass").add({ on: [{ column: "email", opclass: "text_pattern_ops" }] });

  // @ts-expect-error — element `collation` is PG-vendor and only reachable through `pgTable().index()`.
  table("users").index("bad_collation").add({ on: [{ column: "email", collation: "C" }] });
}

export function immutableOnlyBuilderSlots(): void {
  t.text().generated((c) => c.fn.lower(c("email")));
  pgTable("users").index("users_email_lower_idx").add({
    on: [{ expr: (c) => c.fn.lower(c("email")) }],
    where: (c) => c("active").isTrue(),
  });

  // @ts-expect-error — generated column expressions cannot use volatile c.fn.now().
  t.timestamp().generated((c) => c.fn.now());

  // @ts-expect-error — index expression elements cannot use volatile c.fn.now().
  table("users").index("bad_index_now").add({ on: [{ expr: (c) => c.fn.now() }] });

  // @ts-expect-error — partial-index predicates cannot use volatile c.fn.now().
  pgTable("users").index("bad_partial_now").add({ on: ["email"], where: (c) => c.fn.now() });

  // @ts-expect-error — generated column expressions cannot use aggregates.
  t.int().generated((c) => c.agg.count());

  // @ts-expect-error — index expression elements cannot use aggregates.
  table("users").index("bad_index_agg").add({ on: [{ expr: (c) => c.agg.count() }] });

  // @ts-expect-error — partial-index predicates cannot use aggregates.
  pgTable("users").index("bad_partial_agg").add({ on: ["email"], where: (c) => c.agg.count() });
}

// The table-level `.rename({ to })` now type-checks (the renameTable op shipped):
// a bare rename and a schema+ifExists rename, both returning the chainable handle.
export function goodTableRename(): void {
  table("users").rename({ to: "people" });
  table("users").rename({ to: "people", ifExists: true, schema: "reporting" });
}

export function tableRuntimeOptionTerminals(): void {
  table("posts").setOptions({ softDelete: true });
  table("posts").setOptions({ softDelete: false });
  table("posts", { schema: "archive" }).setOptions({ softDelete: true });
  table("posts").setOptions({ versioning: true });
  table("posts").setOptions({ versioning: false });
  table("posts", { schema: "archive" }).setOptions({ versioning: true });
  table("posts").setOptions({ strictness: "lenient" });
  table("posts").create({ columns: { title: t.text() }, options: { softDelete: true, versioning: true, strictness: "off" } });

  // @ts-expect-error — `.softDelete()` is no longer a TableHandle method.
  table("posts").softDelete();

  // @ts-expect-error — `.withVersioning()` is no longer a TableHandle method.
  table("posts").withVersioning();

  // @ts-expect-error — `.strictness()` is no longer a TableHandle method.
  table("posts").strictness("strict");

  // @ts-expect-error — create-time runtime options live under `options`.
  table("posts").create({ columns: { title: t.text() }, softDelete: true });
}

// ───────────────────────────────────────────────────────────────────────────
// 3. The `t.*` ColType builder is structurally typed.
// ───────────────────────────────────────────────────────────────────────────

export function badColTypes(): void {
  // @ts-expect-error — `t` has no `t.notARealType()` factory.
  t.notARealType();

  // @ts-expect-error — the removed `t.string` alias (canonical is t.text()).
  t.string();

  // The recorder still exposes `t.int()` in this branch; keep it type-checked
  // until the recorder twin removes the alias too.
  t.int();

  // @ts-expect-error — `.notNull()` takes no argument.
  t.text().notNull("yes");

  // @ts-expect-error — `.ref(target)` is not a ColumnDef facet; use `t.ref(target)` from the start.
  t.text().ref("users");

  // @ts-expect-error — there is no `.frobnicate()` chain modifier.
  t.text().frobnicate();

  t.numeric({ precision: 12, scale: 2 });
  t.numeric();
  t.char({ length: 3 });
  t.vector({ dimensions: 8, metric: "cosine" });

  // @ts-expect-error — `t.numeric` now takes a named options bag.
  t.numeric(12, 2);

  // @ts-expect-error — `t.char` now takes a named length payload.
  t.char(3);

  // @ts-expect-error — `t.vector` now takes a named dimensions payload.
  t.vector(8);

  // @ts-expect-error — `t.vector({ dimensions })` requires a numeric dimension.
  t.vector({ dimensions: "not a number" });

  // @ts-expect-error — the removed `{ notNull }` options-bag overload (§7).
  t.text({ notNull: true });
}

// ───────────────────────────────────────────────────────────────────────────
// 4. The fluent-expression node shapes are structurally typed.
// ───────────────────────────────────────────────────────────────────────────

export function badExprShapes(): void {
  table("users").update({
    // @ts-expect-error — there is no `.frobnicate()` operator on the expr chain.
    set: { name: (c) => c("name").frobnicate() },
  });

  table("users").update({
    // @ts-expect-error — `c.fn` has no `notARealFn` member.
    set: { name: (c) => c.fn.notARealFn(c("name")) },
  });

  table("users").update({
    // @ts-expect-error — `.cast(...)` only accepts the closed portable target set.
    set: { name: (c) => c("name").cast("jsonb") },
  });

  table("users").update({
    // @ts-expect-error — `coalesce` lives only on c.fn now (§7 dedup), not the chain.
    set: { name: (c) => c("name").coalesce("x") },
  });

  table("users").create({
    columns: { name: t.text() },
    checks: [
      // @ts-expect-error — `check(name, expr)` requires an Expr callback.
      check("bad_check", "name <> ''"),
    ],
  });
}

export function checkExpressionSurfaceTypechecks(): void {
  const pkceCheck: CheckDef = check("pkce_method_check", (c) => c("pkce_method").eq("S256"));
  table("oauth_authorization_codes").create({
    columns: {
      pkce_method: t.text().notNull(),
      user_id: t.text().notNull(),
      kind: t.text().notNull(),
      data: t.json().notNull(),
      floor_cents: t.int(),
      created_at: t.timestamp().notNull(),
      expires_at: t.timestamp().notNull(),
      active: t.boolean().notNull(),
      visible: t.boolean().notNull(),
    },
    checks: [
      pkceCheck,
      check("kind_ok", (c) => c("kind").in(["a", "b", "c"])),
      check("floor_nonneg_or_null", (c) => c("floor_cents").isNull().or(c("floor_cents").ge(lit(0)))),
      check("visible_when_active", (c) => c("active").and(c("visible").isNull().not())),
    ],
  });

  // @ts-expect-error — core CHECK builders do not expose aggregates.
  table("oauth_authorization_codes").check("no_agg").add({ expr: (c) => c.agg.count().gt(0) });

  // @ts-expect-error — core CHECK builders expose only immutable c.fn helpers.
  table("oauth_authorization_codes").check("no_now").add({ expr: (c) => c.fn.now().isNotNull() });

  // @ts-expect-error — core CHECK builders do not expose PostgreSQL vendor helpers.
  table("oauth_authorization_codes").check("no_pg").add({ expr: (c) => c.pg.regex(c("user_id"), "^usr_") });

  // @ts-expect-error — PG-only extract fields are not in the portable extract union.
  table("oauth_authorization_codes").check("no_pg_extract_field").add({ expr: (c) => c.fn.extract("epoch", c("created_at")).gt(0) });

  pgTable("oauth_authorization_codes").check("max_ttl").add({
    expr: (c) => c("expires_at").le(c("created_at").add(c.pg.interval("00:01:00"))),
  });
  pgTable("oauth_authorization_codes").check("epoch_positive").add({
    expr: (c) => c.pg.extract("epoch", c("created_at")).gt(0),
  });
  pgTable("oauth_authorization_codes").check("user_id_fmt").add({
    expr: (c) => c.pg.regex(c("user_id"), "^usr_[0-9A-Za-z]{20,40}$"),
  });
  pgTable("oauth_authorization_codes").check("data_size").add({
    expr: (c) => c.pg.pgColumnSize(c("data")).lt(1000),
  });
  table("oauth_authorization_codes").check("active_is_bool").add({
    expr: (c) => c("active").isNotNull(),
    ifNotExists: true,
  });
}

export function vendorExprSurfaceBoundaryTypechecks(): void {
  pgTable("app_secrets").policy("tenant_only").create({
    using: (c) => c("app_id").eq(c.pg.currentSetting("zeroship.tenant_app", true).cast("uuid")),
    withCheck: (c) => c("owner").eq(c.pg.currentUser()),
  });

  // @ts-expect-error — dot-spelled PG regex is vendor-only; use `c.pg.regex(c("x"), pattern)`.
  table("exprs").update({ set: { x: (c) => c("x")["matches"]("^a$") } });

  // @ts-expect-error — dot-spelled PG column size is vendor-only; use `c.pg.pgColumnSize(c("x"))`.
  table("exprs").update({ set: { x: (c) => c("x")["columnSize"]() } });

  // @ts-expect-error — current_setting is PG-vendor and lives under `c.pg`.
  table("exprs").update({ set: { x: (c) => c.fn["currentSetting"]("zeroship.tenant_app", true) } });

  // @ts-expect-error — current_user is PG-vendor and lives under `c.pg`.
  table("exprs").update({ set: { x: (c) => c.fn["currentUser"]() } });

  // @ts-expect-error — the expression builder is callable; `c.col(...)` is removed.
  table("exprs").update({ set: { x: (c) => c.col("x") } });
}

export function domainValueCheckSurfaceTypechecks(): void {
  domain("account_state").create({
    as: t.text(),
    check: (v) => v.in(["active", "past_due"]).and(v.isNotNull()),
  });
  domain("billing_period").create({
    as: t.date(),
    check: (v) => v.pg.extract("day", v).eq(1),
  });
  domain("email_domain").create({
    as: t.text(),
    check: (v) => v.fn.lower(v).like("%@%"),
  });

  domain("bad_domain_call").create({
    as: t.text(),
    // @ts-expect-error — DomainValueBuilder is the value, not a callable column accessor.
    check: (v) => v("other_column").eq("x"),
  });

  domain("bad_domain_col").create({
    as: t.text(),
    // @ts-expect-error — DomainValueBuilder exposes no general column accessor.
    check: (v) => v.col("other_column").eq("x"),
  });

  domain("bad_domain_now").create({
    as: t.timestamp(),
    // @ts-expect-error — domain checks expose only immutable v.fn helpers.
    check: (v) => v.fn.now().isNotNull(),
  });

  domain("bad_domain_agg").create({
    as: t.text(),
    // @ts-expect-error — DomainValueBuilder has no aggregate namespace.
    check: (v) => v.agg.count().gt(0),
  });
}

// ───────────────────────────────────────────────────────────────────────────
// 5. Insert-row VALUE shapes are typed (scalar kinds); the optional row generic
//    is CALLER-supplied, never auto-derived from the live schema (§1).
// ───────────────────────────────────────────────────────────────────────────

type MyRow = {
  name: string;
  count: number;
};

export function insertValueShapes(): void {
  table("users").insert<MyRow>({ rows: { name: "ada", count: 1 } });

  // @ts-expect-error — `count` must be a number per the caller-supplied generic.
  table("users").insert<MyRow>({ rows: { name: "ada", count: "not a number" } });

  table("users").insert({
    rows: { created_at: Date.now, random_id: Math.random, id: crypto.randomUUID },
  });
  table("users").create({
    columns: {
      created_at: t.timestamp().default((c) => c.fn.now()),
      random_id: t.uuid().default((c) => c.fn.genRandomUuid()),
      id: t.uuid().default((c) => c.fn.genRandomUuid()),
    },
  });

  // TypeScript cannot distinguish this from Date.now in DML values; the runtime
  // identity guard rejects it. This line intentionally typechecks.
  table("users").insert({ rows: { count: () => 42 } });

  // @ts-expect-error — native symbols are no longer valid in column default position.
  table("users").create({ columns: { created_at: t.timestamp().default(Date.now) } });

  // @ts-expect-error — native symbols are no longer valid in column default position.
  table("users").create({ columns: { random_id: t.uuid().default(Math.random) } });

  // @ts-expect-error — native symbols are no longer valid in column default position.
  table("users").create({ columns: { id: t.uuid().default(crypto.randomUUID) } });

  // @ts-expect-error — the removed `{ fn }` carrier is not a DefaultValue.
  table("users").create({ columns: { created_at: t.timestamp().default({ fn: "now" }) } });

  // @ts-expect-error — DefaultBuilder is not callable; defaults cannot reference columns.
  table("users").create({ columns: { name_copy: t.text().default((c) => c("name")) } });

  // @ts-expect-error — DefaultBuilder has no aggregate namespace.
  table("users").create({ columns: { n: t.int().default((c) => c.agg.count()) } });

  // @ts-expect-error — a default callback must return an expression, not a scalar.
  table("users").create({ columns: { count: t.int().default(() => 1) } });

  // @ts-expect-error — a function returning an object is not a native-compatible synth symbol.
  table("users").insert({ rows: { bad: () => ({ nope: true }) } });

  // @ts-expect-error — column defaults reject clearly wrong function return shapes.
  table("users").create({ columns: { bad: t.json().default(() => ({ nope: true })) } });
}

export function decimalValueShapes(): void {
  const cents: DecimalValue = decimal("9007199254740993");
  table("ledger").insert({ rows: { amount: cents } });
  table("ledger").create({ columns: { amount: t.numeric({ precision: 38, scale: 0 }).default(decimal("9007199254740993")) } });
  table("ledger").insert({
    rows: { id: 1, amount: decimal("0.00") },
    onConflict: { columns: ["id"], doUpdate: { amount: decimal("1.25") } },
  });
  lit(decimal("0.00"));

  // @ts-expect-error — bigint is not an authored scalar; use decimal("<n>").
  table("ledger").insert({ rows: { amount: 9007199254740993n } });

  // @ts-expect-error — bigint defaults are refused by the authored value union.
  table("ledger").create({ columns: { amount: t.numeric({ precision: 38, scale: 0 }).default(9007199254740993n) } });

  table("ledger").insert({
    rows: { id: 1, amount: decimal("0.00") },
    onConflict: {
      columns: ["id"],
      doUpdate: {
        // @ts-expect-error — bigint is not valid in onConflict.doUpdate values.
        amount: 9007199254740993n,
      },
    },
  });

  // @ts-expect-error — bigint is not valid in expression literals.
  lit(9007199254740993n);
}

export function byteValueShapes(): void {
  const fromString: BytesValue = byteValue("AQID");
  const fromBytes: BytesValue = byteValue(new Uint8Array([1, 2, 3]));
  table("files").insert({ rows: { raw: fromString } });
  table("files").insert({ rows: { raw: fromBytes } });
  table("files").insert({ rows: { raw: new Uint8Array([1, 2, 3]) } });
  table("files").create({ columns: { raw: t.bytes().default(byteValue("AQID")) } });
  table("files").insert({
    rows: { id: 1, raw: byteValue("AQID") },
    onConflict: { columns: ["id"], doUpdate: { raw: byteValue(new Uint8Array([1, 2, 3])) } },
  });
  lit(byteValue("AQID"));
}

// ───────────────────────────────────────────────────────────────────────────
// 6. The shared `@zeroship/db` lexicon bridge (PR5 goal A) is typed.
// ───────────────────────────────────────────────────────────────────────────

export function lexiconBridgeShapes(): void {
  const def: ColumnDef = fromDb(dbT.ref("users"));
  table("posts").column("author_id").add({ type: def });
  table("posts").create({ columns: { author_id: fromDb(dbT.string().required()) } });

  const _ct = colTypeFromDbField(dbT.json());
  void _ct;

  // @ts-expect-error — `fromDb` takes a @zeroship/db field, not a bare string.
  fromDb("text");
}

// ───────────────────────────────────────────────────────────────────────────
// 6b. EXISTENCE GUARDS — plain `boolean` (op.* PR10 Part B). These lines are
//     well-typed and the file fails tsc if any stops compiling.
// ───────────────────────────────────────────────────────────────────────────

export function existenceGuardsTypecheck(): void {
  table("t").create({ columns: { n: t.int() }, ifNotExists: true });
  table("t").column("email").add({ type: t.text(), ifNotExists: true });
  table("t").column("legacy").drop({ ifExists: true });
  table("t").column("a").setType({ to: t.bigInt() });
  table("t").constraint("c").drop({ ifExists: true });
}

// ───────────────────────────────────────────────────────────────────────────
// 6c. The IMMUTABLE t.* chain (§4) — every modifier returns a fresh ColumnDef.
// ───────────────────────────────────────────────────────────────────────────

export function immutableChainTypechecks(): void {
  const base: ColumnDef = t.text().notNull();
  const a: ColumnDef = base.unique();
  const b: ColumnDef = base.default("x");
  table("u").create({ columns: { a, b, base } });
}

// ───────────────────────────────────────────────────────────────────────────
// 7. EXHAUSTIVENESS — the bridge handles EVERY `@zeroship/db` `FieldDef.type`.
// ───────────────────────────────────────────────────────────────────────────

export function dbFieldTypeExhaustiveness(token: DbFieldType): void {
  switch (token) {
    case "string":
    case "number":
    case "boolean":
    case "date":
    case "json":
    case "bytes":
    case "geoPoint":
    case "id":
    case "ref":
    case "vector":
    case "object":
    case "union":
    case "literal":
    case "array":
    case "actor":
    case "calendarDate":
      return;
    default: {
      const _exhaustive: never = token;
      return _exhaustive;
    }
  }
}

// ───────────────────────────────────────────────────────────────────────────
// 8. LOW-2 — the internal closed-set validation arrays in `ops.ts`
//    (`MASK_KINDS` / `MASK_CLASSIFICATIONS` / `VECTOR_METRICS`) carry the CLOSED
//    wire-union element type, NOT a loose `string`.
//
//    This turns the array LITERALS into a compile-time drift guard: if the
//    engine's `MaskKind` / `Classification` / `VectorMetric` union drops a token,
//    the committed array element that no longer matches becomes a tsc error, so the
//    runtime `.includes` guard can never silently diverge from the closed enum it
//    mirrors (the SDK-side static peer of the runtime `ir-types-drift` gate).
//
//    RED PROOF: with the pre-fix `readonly string[]` arrays,
//    `(typeof MASK_KINDS)[number]` is `string`, which is NOT mutually assignable to
//    the closed `MaskKind` union — so `expectExactType<...>(true)` is a hard tsc
//    error here and `typecheck:types` fails. The change to `readonly MaskKind[]`
//    (etc.) makes the element type EXACTLY the union, so it compiles.
// ───────────────────────────────────────────────────────────────────────────

// Exact (invariant) type equality — the canonical `Equal` test. `readonly
// string[]`'s `string` element is NOT exactly the closed union, so it fails this.
type ExactEqual<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2
  ? true
  : false;

function expectExactType<A, B>(_proof: ExactEqual<A, B> extends true ? true : never): void {
  /* type-level only — never called */
}

export function closedSetArrayElementTyping(): void {
  expectExactType<(typeof MASK_KINDS)[number], MaskKind>(true);
  expectExactType<(typeof MASK_CLASSIFICATIONS)[number], Classification>(true);
  expectExactType<(typeof VECTOR_METRICS)[number], VectorMetric>(true);
}
