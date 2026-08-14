// STRUCTURAL type-safety, names-stay-strings (BINDING) — fluent-only.
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
//   - table/column NAMES are plain `string`, NOT bound to the live `db`
//     schema (a migration naming a non-existent table/column TYPE-CHECKS cleanly —
//     existence is an apply-time check, the anti-rot guarantee).

// @ts-expect-error — free boolean combinators are no longer exported from the public package.
import { and as removedPkgAnd, or as removedPkgOr, not as removedPkgNot } from "zero-migrate";
// @ts-expect-error — free policy helpers were deleted; use table(...).policy(name).create/drop().
import { createPolicy as removedPkgCreatePolicy, dropPolicy as removedPkgDropPolicy } from "zero-migrate";
// @ts-expect-error — flat named-object lifecycle helpers were deleted; use schema/extension/role handles.
import { dropSchema as removedPkgDropSchema, dropExtension as removedPkgDropExtension, alterRole as removedPkgAlterRole, dropRole as removedPkgDropRole } from "zero-migrate";

import * as migrate from "../../src/index.js";
import {
  colTypeFromDbField,
  dbType as dbT,
  fromDb,
  ids,
  perRow,
  t,
  table,
  view,
  check,
  lit,
  int64,
  decimal,
  byteValue,
  now,
  uuidV4,
  uuidV7,
  genRandomUuid,
  currentSetting,
  currentUser,
  interval,
  countStar,
  domain,
  type ColumnDef,
  type CheckDef,
  type DbFieldType,
  type Int64Value,
  type Migration,
  type OrderedColumns,
  type BackfillSetValue,
  type IdFormats,
  type PerRowGenerator,
  type PerRowGeneratorValue,
  type PerRowGenerators,
  type PrimaryKeyOperations,
  type TypeLexicon,
  type TypeIdOptions,
  type TableForeignKey,
  type ValueFormat,
  type DecimalValue,
  type BytesValue,
} from "../../src/index.js";
// @ts-expect-error — free boolean combinators are no longer exported; use chain `.and`/`.or`/`.not`.
import { and as removedAnd, or as removedOr, not as removedNot } from "../../src/index.js";
// The internal closed-set validation arrays (NOT part of the public `index.ts`
// surface) — imported directly for the LOW-2 element-typing assertion below.
import { MASK_CLASSIFICATIONS, MASK_KINDS, VECTOR_METRICS } from "../../src/ops.js";
import type { Classification, MaskKind, VectorMetric } from "../../src/generated/ir.js";

// The public migration union keeps schema and data phases distinct, and makes
// every data migration declare how rollback is handled.
// @ts-expect-error — one migration module cannot contain both schema and data phases.
const mixedPhaseMigration: Migration = { schema() {}, data() {}, inverse() {} };
void mixedPhaseMigration;

// @ts-expect-error — a data migration must provide either inverse() or an irreversible reason.
const undeclaredDataRollback: Migration = { data() {} };
void undeclaredDataRollback;

// @ts-expect-error — irreversible must be an operator-facing reason string, not a boolean marker.
const booleanIrreversibleMigration: Migration = { data() {}, irreversible: true };
void booleanIrreversibleMigration;

// The removed universal ID shortcut must stay absent from the migration lexicon.
const migrationIdShortcutIsAbsent: "id" extends keyof TypeLexicon ? never : true = true;
void migrationIdShortcutIsAbsent;
const migrationRefShortcutIsAbsent: "ref" extends keyof TypeLexicon ? never : true = true;
void migrationRefShortcutIsAbsent;

const compositeForeignKeyColumns: OrderedColumns = ["tenant_id", "parent_id"];
const compositeForeignKey: TableForeignKey = {
  name: "child_parent_fk",
  columns: compositeForeignKeyColumns,
  references: {
    table: "parents",
    columns: ["tenant_id", "id"],
  },
  onDelete: "cascade",
};
const readonlyCompositeForeignKeys = [compositeForeignKey] as const satisfies readonly TableForeignKey[];

table("children").create({
  columns: {
    tenant_id: t.uuid().notNull(),
    parent_id: t.uuid().notNull(),
  },
  primaryKey: ["tenant_id", "parent_id"] as const,
  foreignKeys: readonlyCompositeForeignKeys,
});

// @ts-expect-error — ordered key/FK column tuples cannot be empty.
const emptyOrderedColumns: OrderedColumns = [];
void emptyOrderedColumns;

const emptyLocalCompositeForeignKey: TableForeignKey = {
  name: "empty_local_fk",
  // @ts-expect-error — a table-level foreign key requires at least one local column.
  columns: [],
  references: { table: "parents", columns: ["id"] },
};
void emptyLocalCompositeForeignKey;

const primaryKeyOperations: PrimaryKeyOperations = table("accounts").primaryKey();
primaryKeyOperations.add({ columns: ["id"] });
primaryKeyOperations.replace({
  expectedColumns: ["id"],
  columns: ["tenant_id", "account_id"],
  dropIdentityFrom: ["id"],
});
primaryKeyOperations.drop({
  expectedColumns: ["tenant_id", "account_id"],
  dropIdentityFrom: ["account_id"],
});

// @ts-expect-error — primary-key column tuples cannot be empty.
primaryKeyOperations.add({ columns: [] });
// @ts-expect-error — replace requires the exact current key as an ordered precondition.
primaryKeyOperations.replace({ columns: ["id"] });
// @ts-expect-error — replace requires the new primary-key columns.
primaryKeyOperations.replace({ expectedColumns: ["id"] });
// @ts-expect-error — drop requires the exact current key as an ordered precondition.
primaryKeyOperations.drop({});
// @ts-expect-error — dropIdentityFrom is also a non-empty ordered tuple when present.
primaryKeyOperations.drop({ expectedColumns: ["id"], dropIdentityFrom: [] });
// @ts-expect-error — lifecycle actions inherit schema from table(); they have no per-action schema.
primaryKeyOperations.add({ columns: ["id"], schema: "app" });
// @ts-expect-error — primaryKey() selects the table key and takes no identifier.
table("accounts").primaryKey("accounts_pkey");
// @ts-expect-error — changing an ID family is an explicit expand/cutover workflow, not one call.
table("accounts").changeIdType({ from: t.bigInt(), to: t.uuid() });

const identityImportTable = table("orders");
const synchronizedIdentityTable: typeof identityImportTable = identityImportTable
  .column("id")
  .synchronizeIdentity({
    schema: "app",
    writesQuiesced: "orders_import_window",
  });
void synchronizedIdentityTable;
// @ts-expect-error — synchronizeIdentity requires the named writer-quiescence acknowledgment.
identityImportTable.column("id").synchronizeIdentity({});
// @ts-expect-error — writesQuiesced is a name, not a boolean toggle.
identityImportTable.column("id").synchronizeIdentity({ writesQuiesced: true });

// ───────────────────────────────────────────────────────────────────────────
// 1. NAMES STAY STRINGS — the anti-rot guarantee.
// ───────────────────────────────────────────────────────────────────────────

export function antiRotMigration(): void {
  // `nonexistent_table` / `column_that_was_dropped` / `legacy_col` are not in any
  // live schema — yet every op below compiles, because names are plain strings.
  table("nonexistent_table").create({
    columns: {
      legacy_col: t.text(),
      author_id: t.text().references("a_table_that_does_not_exist", "id"),
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
    set: { legacy_col: (col) => col("a_column_no_schema_declares").concat(" suffix") },
    where: (col) => col("yet_another_missing_column").isNull(),
  });
  table("nonexistent_table").delete({ where: (col) => col("phantom_col").eq(1) });
  // L5 (M19): update/delete/backfill `where` accept a built ExprChain / Expr, not just a `(col) => …` callback.
  table("nonexistent_table").update({ set: { legacy_col: 1 }, where: migrate.lit(1).eq(migrate.lit(1)) });
  table("nonexistent_table").delete({ where: migrate.lit(1).eq(migrate.lit(1)) });
  table("nonexistent_table").backfill({
    set: { legacy_col: (col) => col("phantom_col").splitPart(" ", 1) },
    where: (col) => col("phantom_col").isNotNull(),
    cursorColumns: ["id"],
    cursorStability: { mode: "guardUpdates" },
  });
  table("nonexistent_table").insert({ rows: { phantom_col: "ok", another_phantom: 42 } });
  view("phantom_totals").create({
    as: (q) => q
      .from("phantom_orders")
      .select(["customer_id", () => countStar(), (col) => col("amount").sum()])
      .where((col) => col("status").eq("paid"))
      .groupBy(["customer_id"])
      .having((col) => col("id").count().gt(5)),
  });
  view("phantom_rollups").create({
    as: (q) => q
      .from("phantom_orders")
      .select([
        "customer_id",
        (col) => col("name").stringAgg(", "),
        (col) => col("id").arrayAgg(),
        (col) => col("ok").boolAnd(),
        (col) => col("ok").boolOr(),
      ])
      .groupBy(["customer_id"]),
  });
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

  // @ts-expect-error — raw bigint is not an authored scalar; use int64(...).
  table("users").update({ set: { name: 1n } });

  table("users").update({
    set: { name: "x" },
    where: (col) => col("id").isNotNull(),
    // @ts-expect-error — batched writes are spelled backfill({ cursorColumns, cursorStability, batchSize }), not update({ batch }).
    batch: { cursorColumns: ["id"], batchSize: 500 },
  });

  // @ts-expect-error — the table-level `.rename({ to })` REQUIRES a `to` string.
  table("users").rename({});

  // @ts-expect-error — `.rename({ to })` has no `from` (that is the column-rename shape).
  table("users").rename({ from: "users", to: "people" });
}

export function tableVendorSurface(): void {
  table("secrets").policy("tenant_only").create({ using: (col) => col("tenant_id").isNotNull() });

  // @ts-expect-error — deleted direct table trigger method; use `.trigger(name).create(...)`.
  table("audit_events").createTrigger({ name: "audit_events_trg", timing: "before", events: ["insert"], forEach: "row", execute: "audit_events_fn" });

  // @ts-expect-error — deleted direct table trigger method; use `.trigger(name).drop(...)`.
  table("audit_events").dropTrigger({ name: "audit_events_trg", ifExists: true });

  table("secrets").setRls({ enabled: true });

  table("bookings").exclusion("bookings_no_overlap");

  // @ts-expect-error — direct constraint validation method was deleted; use `table(...).constraint(name).validate()`.
  table("line_items").validateConstraint("line_items_order_fkey");

  // @ts-expect-error — direct partition detach method was deleted; use `table(...).partition(name).detach()`.
  table("events").detachPartition("events_2026_05");

  table("line_items").constraint("line_items_order_fkey").validate();

  table("audit_events").trigger("audit_events_trg").create({
    timing: "before",
    events: ["insert"],
    forEach: "row",
    execute: "audit_events_fn",
  });
  table("audit_events").trigger("audit_events_trg").drop({ ifExists: true });

  // @ts-expect-error — deleted direct PG policy method; use `.policy(name).create(...)`.
  table("secrets").createPolicy({ name: "tenant_only", using: (col) => col("tenant_id").isNotNull() });

  // @ts-expect-error — deleted direct PG policy method; use `.policy(name).drop(...)`.
  table("secrets").dropPolicy({ name: "tenant_only", ifExists: true });

  table("secrets")
    .setRls({ enabled: true, forced: true })
    .policy("tenant_only").create({ using: (col) => col("tenant_id").isNotNull() })
    .policy("tenant_only").drop({ ifExists: true })
    .setRls({ enabled: false, forced: false });
  // @ts-expect-error — deleted RLS method; use `.setRls({ enabled: true })`.
  table("secrets").enableRowLevelSecurity();
  // @ts-expect-error — deleted RLS method; use `.setRls({ forced: true })`.
  table("secrets").forceRowLevelSecurity();
  // @ts-expect-error — deleted RLS method; use `.setRls({ enabled: false })`.
  table("secrets").disableRowLevelSecurity();
  // @ts-expect-error — deleted RLS method; use `.setRls({ forced: false })`.
  table("secrets").noForceRowLevelSecurity();
  table("bookings").exclusion("bookings_no_overlap").add({
    using: "gist",
    elements: [{ target: "room_id", operator: "=" }],
  });
  // @ts-expect-error — deleted direct PG constraint validation method; use `.constraint(name).validate(...)`.
  table("line_items").validateConstraint("line_items_order_fkey");
  table("line_items").constraint("line_items_order_fkey").validate();
  // @ts-expect-error — deleted direct PG partition detach method; use `.partition(name).detach(...)`.
  table("events").detachPartition("events_2026_05", { concurrently: true });
  table("events").partition("events_2026_05").detach({ concurrently: true });
  table("events").partition("events_2026_06").attach({
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

  // @ts-expect-error - whenUnsupported is an explicit affirmation and only accepts "collapse".
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
      { expr: (col) => col("email").lower() },
    ],
    unique: true,
  });

  table("users").index("users_email_idx").add({
    on: [
      "email",
      // `order`, `opclass` and `collation` DO render on a column element. `nulls`
      // does not, and used to be accepted and discarded (F652), so it is gone.
      { column: "created_at", order: "desc", opclass: "timestamp_ops", collation: "C" },
      { expr: (col) => col("email").lower() },
    ],
    using: "gin",
    where: (col) => col("active").isTrue(),
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
  table("users").index("bad_tagged_expr").add({ on: [{ [oldKindKey]: oldExprKind, expr: (col) => col("email") }] });

  // @ts-expect-error — bare expression elements must be wrapped as `{ expr }`.
  table("users").index("bad_bare_expr").add({ on: [(col) => col("email")] });

  const oldColumnKind = "column";
  // @ts-expect-error — column object elements use `{ column }`, not `{ kind, name }`.
  table("users").index("bad_tagged_column").add({ on: [{ [oldKindKey]: oldColumnKind, name: "email" }] });

  table("users").index("using_idx").add({ on: ["email"], using: "gin" });

  table("users").index("where_idx").add({ on: ["email"], where: (col) => col("active").isTrue() });

  table("users").index("include_idx").add({ on: ["email"], include: ["id"] });

  table("users").index("with_idx").add({ on: ["email"], with: { fillfactor: 90 } });

  table("users").index("only_idx").add({ on: ["email"], only: true });

  table("users").index("nulls_not_distinct_idx").add({ on: ["email"], unique: true, nullsNotDistinct: true });

  table("users").index("opclass_idx").add({ on: [{ column: "email", opclass: "text_pattern_ops" }] });

  table("users").index("collation_idx").add({ on: [{ column: "email", collation: "C" }] });

  // @ts-expect-error — `.index().drop()` no longer accepts author-declared uniqueness.
  table("users").index("bad_unique_drop").drop({ unique: true });
}

export function immutableOnlyBuilderSlots(): void {
  t.text().generated((col) => col("email").lower());
  table("users").index("users_email_lower_idx").add({
    on: [{ expr: (col) => col("email").lower() }],
    where: (col) => col("active").isTrue(),
  });

  t.timestamp().generated(() => now());

  table("users").index("bad_index_now").add({ on: [{ expr: () => now() }] });

  table("users").index("bad_partial_now").add({ on: ["email"], where: () => now() });

  t.int().generated(() => countStar());

  table("users").index("bad_index_agg").add({ on: [{ expr: () => countStar() }] });

  table("users").index("bad_partial_agg").add({ on: ["email"], where: () => countStar() });

  // F652: facets the renderer discards are now TYPE errors, not silent no-ops.
  // A `{ expr, order }` element used to produce an ASCENDING index.
  table("users").index("bad_col_nulls").add({
    // @ts-expect-error — per-element `nulls` is unsupported (dialects.md).
    on: [{ column: "email", nulls: "last" }],
  });

  // `{ expr, order }` and `{ expr, opclass }` are NOT type errors, and cannot be:
  // `IndexElementArg` is a union, and TypeScript's excess-property check accepts a
  // property that exists on ANY member — `order`/`opclass` both exist on
  // `IndexColumnElementArg`. Only `nulls`, now absent from every member, is
  // catchable here. Those two are refused at RUNTIME instead; see
  // `index-element-facets-not-silent.test.ts`.
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

  // `t.string()` is now a first-class bounded `VARCHAR(N)` (default length 255),
  // no longer a removed alias — it type-checks as a valid column factory.
  t.string();
  t.string({ length: 120 });

  // The recorder still exposes `t.int()` in this branch; keep it type-checked
  // until the recorder twin removes the alias too.
  t.int();

  // @ts-expect-error — `.notNull()` takes no argument.
  t.text().notNull("yes");

  // @ts-expect-error — the old untyped migration reference factory is removed.
  t.ref("users");

  // @ts-expect-error — `.ref(target)` is not a ColumnDef facet; use `.references(table, column)`.
  t.text().ref("users");

  t.text().references("users", "id");
  t.uuid().references("users", "public_id", {
    onDelete: "cascade",
    onUpdate: "setNull",
  });

  // @ts-expect-error — the referenced column is required.
  t.text().references("users");

  // @ts-expect-error — referential actions use the closed RefAction tokens.
  t.text().references("users", "id", { onDelete: "deleteEverything" });

  // @ts-expect-error — there is no `.frobnicate()` chain modifier.
  t.text().frobnicate();

  t.numeric({ precision: 12, scale: 2 });
  t.numeric();
  t.char({ length: 3 });
  t.vector({ dimensions: 8, metric: "cosine" });

  const typeIdOptions: TypeIdOptions = { prefix: "account" };
  const idFormats: IdFormats = ids;
  const valueFormat: ValueFormat = { typeId: typeIdOptions };
  const ulidValueFormat: ValueFormat = "ulid";
  const typedId: ColumnDef = idFormats.typeId(typeIdOptions).notNull().unique().primaryKey();
  const ulid: ColumnDef = idFormats.ulid().notNull().unique().primaryKey();
  table("accounts").create({ columns: { id: typedId } });
  void valueFormat;
  void ulidValueFormat;
  void ulid;

  // @ts-expect-error — TypeID options are required.
  ids.typeId();

  // @ts-expect-error — TypeID requires an explicit prefix (the empty string is valid).
  ids.typeId({});

  // @ts-expect-error — a TypeID prefix is text.
  ids.typeId({ prefix: 42 });

  ids.ulid();

  // @ts-expect-error — ULID takes no options.
  ids.ulid({});

  // @ts-expect-error — the ULID ValueFormat wire tag is canonical lowercase.
  const invalidUlidValueFormat: ValueFormat = "ULID";
  void invalidUlidValueFormat;

  // @ts-expect-error — `t.numeric` now takes a named options bag.
  t.numeric(12, 2);

  // @ts-expect-error — `t.char` now takes a named length payload.
  t.char(3);

  // @ts-expect-error — `t.vector` now takes a named dimensions payload.
  t.vector(8);

  // @ts-expect-error — `t.vector({ dimensions })` requires a numeric dimension.
  t.vector({ dimensions: "not a number" });

  // @ts-expect-error — the removed `{ notNull }` options-bag overload.
  t.text({ notNull: true });
}

// ───────────────────────────────────────────────────────────────────────────
// 4. The fluent-expression node shapes are structurally typed.
// ───────────────────────────────────────────────────────────────────────────

export function badExprShapes(): void {
  table("users").update({
    // @ts-expect-error — there is no `.frobnicate()` operator on the expr chain.
    set: { name: (col) => col("name").frobnicate() },
  });

  table("users").update({
    // @ts-expect-error — there is no `.notARealFn()` operator on the expr chain.
    set: { name: (col) => col("name").notARealFn() },
  });

  table("users").update({
    // @ts-expect-error — `.cast(...)` takes a named `{ to }` args object.
    set: { name: (col) => col("name").cast("text") },
  });

  table("users").update({
    // @ts-expect-error — `.cast({ to })` only accepts the closed scalar ColType target set.
    set: { name: (col) => col("name").cast({ to: "blob" }) },
  });

  table("users").update({
    set: { name: (col) => col("name").coalesce("x") },
  });

  table("users").update({
    set: {
      http_status: (col) => col("http_status").in([200, 404, 500]),
      enabled: (col) => col("enabled").notIn([true, false]),
    },
  });

  table("users").update({
    // @ts-expect-error — `.in` accepts only the pinned Scalar set, not bytes.
    set: { payload: (col) => col("payload").in([byteValue("AQID")]) },
  });

  table("users").update({
    // @ts-expect-error — `.notIn` accepts only the pinned Scalar set, not objects.
    set: { kind: (col) => col("kind").notIn([{ value: "admin" }]) },
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
  const pkceCheck: CheckDef = check("pkce_method_check", (col) => col("pkce_method").eq("S256"));
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
      check("kind_ok", (col) => col("kind").in(["a", "b", "c"])),
      check("floor_nonneg_or_null", (col) => col("floor_cents").isNull().or(col("floor_cents").ge(lit(0)))),
      check("visible_when_active", (col) => col("active").and(col("visible").isNull().not())),
    ],
  });

  table("oauth_authorization_codes").check("no_agg").add({ expr: () => countStar().gt(0) });

  table("oauth_authorization_codes").check("no_now").add({ expr: () => now().isNotNull() });

  // `regex` is a first-class chain operator on the CORE check builder
  // (PostgreSQL-first; fails closed off-PG at validate-time, not tsc).
  table("oauth_authorization_codes").check("core_regex").add({ expr: (col) => col("user_id").regex("^usr_") });

  // PG-only extract fields typecheck on the core surface and fail closed off-PG at validate time.
  table("oauth_authorization_codes").check("pg_extract_field_validate_gated").add({ expr: (col) => col("created_at").extract("epoch").gt(0) });

  table("oauth_authorization_codes").check("max_ttl").add({
    expr: (col) => col("expires_at").le(col("created_at").add(interval({ minutes: 1 }))),
  });
  table("oauth_authorization_codes").check("no_core_interval").add({ expr: (col) => col("expires_at").le(interval({ minutes: 1 })) });
  // @ts-expect-error — interval takes a structured Duration, not HH:MM:SS text.
  table("oauth_authorization_codes").check("no_interval_string").add({ expr: (col) => col("expires_at").le(col("created_at").add(interval("00:01:00"))) });
  table("oauth_authorization_codes").check("epoch_positive").add({
    expr: (col) => col("created_at").extract("epoch").gt(0),
  });
  table("oauth_authorization_codes").check("user_id_fmt").add({
    expr: (col) => col("user_id").regex("^usr_[0-9A-Za-z]{20,40}$"),
  });
  table("oauth_authorization_codes").check("data_size").add({
    expr: (col) => col("data").columnSize().lt(1000),
  });
  table("oauth_authorization_codes").check("active_is_bool").add({
    expr: (col) => col("active").isNotNull(),
    ifNotExists: true,
  });
}

export function vendorExprSurfaceBoundaryTypechecks(): void {
  table("app_secrets").policy("tenant_only").create({
    using: (col) => col("app_id").eq(currentSetting("zero_migrate.tenant_app", { missingOk: true }).cast({ to: "uuid" })),
    withCheck: (col) => col("owner").eq(currentUser()),
  });

  // `regex` is a first-class chain operator (PG-first). It typechecks.
  table("exprs").update({ set: { x: (col) => col("x").regex("^a$") } });
  // @ts-expect-error — `matches` was renamed to `regex`; the old name is gone.
  table("exprs").update({ set: { x: (col) => col("x")["matches"]("^a$") } });

  // `columnSize` is a first-class chain operator (PG-first). It typechecks.
  table("exprs").update({ set: { x: (col) => col("x").columnSize() } });

  // @ts-expect-error — currentSetting is a top-level import, not a chain member.
  table("exprs").update({ set: { x: (col) => col("x")["currentSetting"]("zero_migrate.tenant_app", true) } });

  // @ts-expect-error — currentUser is a top-level import, not a chain member.
  table("exprs").update({ set: { x: (col) => col("x")["currentUser"]() } });

  // @ts-expect-error — the expression builder is callable; `col.col(...)` is removed.
  table("exprs").update({ set: { x: (col) => col.col("x") } });
}

export function domainValueCheckSurfaceTypechecks(): void {
  domain("account_state").create({
    as: t.text(),
    check: (v) => v.in(["active", "past_due"]).and(v.isNotNull()),
  });
  domain("billing_period").create({
    as: t.date(),
    check: (v) => v.extract("day").eq(1),
  });
  domain("email_domain").create({
    as: t.text(),
    check: (v) => v.lower().like("%@%"),
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
    check: () => now().isNotNull(),
  });

  domain("bad_domain_agg").create({
    as: t.text(),
    check: (v) => v.count().gt(0),
  });
}

// ───────────────────────────────────────────────────────────────────────────
// 5. Insert-row VALUE shapes are typed (scalar kinds); the optional row generic
//    is CALLER-supplied, never auto-derived from the live schema.
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
      created_at: t.timestamp().default(now()),
      random_id: t.uuid().default(uuidV4()),
      ordered_id: t.uuid().default(uuidV7()),
      legacy_id: t.uuid().default(genRandomUuid()),
      id: t.uuid().default(uuidV4()),
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
  table("users").create({ columns: { name_copy: t.text().default((col) => col("name")) } });

  table("users").create({ columns: { n: t.int().default(() => countStar()) } });

  // @ts-expect-error — a default callback must return an expression, not a scalar.
  table("users").create({ columns: { count: t.int().default(() => 1) } });

  // @ts-expect-error — a function returning an object is not a native-compatible synth symbol.
  table("users").insert({ rows: { bad: () => ({ nope: true }) } });

  // @ts-expect-error — column defaults reject clearly wrong function return shapes.
  table("users").create({ columns: { bad: t.json().default(() => ({ nope: true })) } });
}

export function perRowGeneratorShapes(): void {
  const generators: PerRowGenerators = perRow;
  const intent: PerRowGeneratorValue = generators.uuidV7();
  const setValue: BackfillSetValue = intent;
  const wireGenerator: PerRowGenerator = { typeId: { prefix: "order" } };

  table("orders").backfill({
    set: {
      uuid_v4: generators.uuidV4(),
      uuid_v7: intent,
      type_id: generators.typeId({ prefix: "order" }),
      ulid: generators.ulid(),
      database_uuid: uuidV4(),
    },
    cursorColumns: ["id"],
    cursorStability: { mode: "guardUpdates" },
  });
  void setValue;
  void wireGenerator;

  // @ts-expect-error — perRow values are apply-engine intents, not insert values.
  table("orders").insert({ rows: { public_id: intent } });

  // @ts-expect-error — perRow values are valid only in backfill({ set }).
  table("orders").update({ set: { public_id: intent } });

  // @ts-expect-error — perRow values are not column defaults.
  table("orders").create({ columns: { public_id: t.uuid().default(intent) } });

  // @ts-expect-error — perRow values are not column definitions.
  table("orders").create({ columns: { public_id: intent } });

  // @ts-expect-error — perRow does not expose an application-runtime UUID string.
  const generatedUuid: string = generators.uuidV4();
  void generatedUuid;

  // @ts-expect-error — TypeID options are required.
  generators.typeId();

  // @ts-expect-error — a TypeID prefix is text.
  generators.typeId({ prefix: 42 });

  // @ts-expect-error — perRow.ulid takes no options.
  generators.ulid({});
}

export function int64ValueShapes(): void {
  const exact: Int64Value = int64(9_007_199_254_740_993n);
  const max: Int64Value = int64("9223372036854775807");

  table("ledger").insert({ rows: { amount: exact } });
  table("ledger").update({ set: { amount: max } });
  table("ledger").backfill({
    set: { amount: int64("-9223372036854775808") },
    cursorColumns: ["id"],
    cursorStability: { mode: "guardUpdates" },
  });
  table("ledger").create({
    columns: { amount: t.bigInt().default(int64("-9223372036854775808")) },
  });
  table("ledger").insert({
    rows: { id: 1, amount: exact },
    onConflict: { columns: ["id"], doUpdate: { amount: max } },
  });
  lit(exact);

  // @ts-expect-error — int64() accepts only bigint or decimal-string input.
  int64(42);

  table("ledger").update({
    // @ts-expect-error — Int64Value does not widen the pinned `.in()` Scalar set.
    set: { amount: (col) => col("amount").in([exact]) },
  });
}

export function decimalValueShapes(): void {
  const cents: DecimalValue = decimal("9007199254740993");
  table("ledger").insert({ rows: { amount: cents } });
  table("ledger").create({ columns: { amount: t.numeric({ precision: 38, scale: 0 }).default(decimal("9007199254740993")) } });
  table("ledger").insert({
    rows: { id: 1, amount: decimal("0.00") },
    onConflict: { columns: ["id"], doUpdate: { amount: decimal("1.25") } },
  });
  table("ledger").insert({
    rows: { id: 1 },
    onConflict: {
      columns: ["id"],
      // A conflict update may assign a column that was not part of the insert row.
      doUpdate: { amount: decimal("1.25") },
    },
  });
  lit(decimal("0.00"));

  // @ts-expect-error — raw bigint is not an authored scalar; use int64(...).
  table("ledger").insert({ rows: { amount: 9007199254740993n } });

  // @ts-expect-error — raw bigint defaults are refused; wrap the value with int64(...).
  table("ledger").create({ columns: { amount: t.numeric({ precision: 38, scale: 0 }).default(9007199254740993n) } });

  table("ledger").insert({
    rows: { id: 1, amount: decimal("0.00") },
    onConflict: {
      columns: ["id"],
      doUpdate: {
        // @ts-expect-error — raw bigint is not valid in onConflict.doUpdate values.
        amount: 9007199254740993n,
      },
    },
  });

  // @ts-expect-error — raw bigint is not valid in expression literals.
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
// 6. The shared `db` lexicon bridge is typed.
// ───────────────────────────────────────────────────────────────────────────

export function lexiconBridgeShapes(): void {
  const def: ColumnDef = fromDb(dbT.ref("users"));
  table("posts").column("author_id").add({ type: def });
  table("posts").create({ columns: { author_id: fromDb(dbT.string().required()) } });

  const _ct = colTypeFromDbField(dbT.json());
  void _ct;

  // @ts-expect-error — `fromDb` takes a db field, not a bare string.
  fromDb("text");
}

// ───────────────────────────────────────────────────────────────────────────
// 6b. EXISTENCE GUARDS — plain `boolean` (op.*). These lines are
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
// 6c. The IMMUTABLE t.* chain — every modifier returns a fresh ColumnDef.
// ───────────────────────────────────────────────────────────────────────────

export function immutableChainTypechecks(): void {
  const base: ColumnDef = t.text().notNull();
  const a: ColumnDef = base.unique();
  const b: ColumnDef = base.default("x");
  table("u").create({ columns: { a, b, base } });
}

// ───────────────────────────────────────────────────────────────────────────
// 7. EXHAUSTIVENESS — the bridge handles EVERY `db` `FieldDef.type`.
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
