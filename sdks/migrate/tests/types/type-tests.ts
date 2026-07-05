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

import {
  colTypeFromDbField,
  fromDb,
  t,
  table,
  check,
  and,
  or,
  not,
  membership,
  lit,
  decimal,
  byteValue,
  interval,
  type ColumnDef,
  type CheckDef,
  type DbFieldType,
  type DecimalValue,
  type BytesValue,
} from "../../src/index.js";
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

  // @ts-expect-error — `update.set` values must be (c) => Expr callbacks, not raw strings.
  table("users").update({ set: { name: "raw sql string" } });

  // @ts-expect-error — the table-level `.rename({ to })` REQUIRES a `to` string.
  table("users").rename({});

  // @ts-expect-error — `.rename({ to })` has no `from` (that is the column-rename shape).
  table("users").rename({ from: "users", to: "people" });
}

export function indexGrammar(): void {
  table("users").index("users_email_idx").add({
    on: [
      "email",
      { column: "created_at", order: "desc", opclass: "timestamp_ops", collation: "C" },
      { expr: (c) => c.fn.lower(c("email")) },
    ],
    using: "btree",
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
}

// The table-level `.rename({ to })` now type-checks (the renameTable op shipped):
// a bare rename and a schema+ifExists rename, both returning the chainable handle.
export function goodTableRename(): void {
  table("users").rename({ to: "people" });
  table("users").rename({ to: "people", ifExists: true, schema: "reporting" });
}

export function tableRuntimeOptionTerminals(): void {
  table("posts").softDelete();
  table("posts").softDelete({ enabled: false });
  table("posts").softDelete({ enabled: true, schema: "archive" });
  table("posts").withVersioning();
  table("posts").withVersioning({ enabled: false });
  table("posts").withVersioning({ enabled: true, schema: "archive" });

  // @ts-expect-error — `.softDelete()` no longer accepts a positional boolean.
  table("posts").softDelete(false);

  // @ts-expect-error — `.withVersioning()` no longer accepts a positional boolean.
  table("posts").withVersioning(true);
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

  // @ts-expect-error — `.ref(target)` requires a string target table.
  t.text().ref(123);

  // @ts-expect-error — there is no `.frobnicate()` chain modifier.
  t.text().frobnicate();

  // @ts-expect-error — `t.vector(n)` requires a numeric dimension.
  t.vector("not a number");

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
      check("max_ttl", (c) => c("expires_at").le(c("created_at").add(interval("00:01:00")))),
      check("user_id_fmt", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$")),
      check("kind_ok", (c) => membership(c("kind"), ["a", "b", "c"])),
      check("data_size", (c) => c("data").columnSize().lt(262144)),
      check("floor_nonneg_or_null", (c) => or(c("floor_cents").isNull(), c("floor_cents").ge(lit(0)))),
      check("visible_when_active", (c) => and(c("active"), not(c("visible").isNull()))),
    ],
  });
  table("oauth_authorization_codes").check("active_is_bool").add({
    expr: (c) => c("active").isNotNull(),
    ifNotExists: true,
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
      created_at: t.timestamp().default(Date.now),
      random_id: t.uuid().default(Math.random),
      id: t.uuid().default(crypto.randomUUID),
    },
  });

  // TypeScript cannot distinguish this from Date.now; the runtime identity guard
  // rejects it. This line intentionally typechecks.
  table("users").insert({ rows: { count: () => 42 } });
  table("users").create({ columns: { count: t.int().default(() => 1) } });

  // @ts-expect-error — a function returning an object is not a native-compatible synth symbol.
  table("users").insert({ rows: { bad: () => ({ nope: true }) } });

  // @ts-expect-error — column defaults reject clearly wrong function return shapes.
  table("users").create({ columns: { bad: t.json().default(() => ({ nope: true })) } });
}

export function decimalValueShapes(): void {
  const cents: DecimalValue = decimal("9007199254740993");
  table("ledger").insert({ rows: { amount: cents } });
  table("ledger").create({ columns: { amount: t.numeric(38, 0).default(decimal("9007199254740993")) } });
  table("ledger").insert({
    rows: { id: 1, amount: decimal("0.00") },
    onConflict: { columns: ["id"], doUpdate: { amount: decimal("1.25") } },
  });
  lit(decimal("0.00"));

  // @ts-expect-error — bigint is not an authored scalar; use decimal("<n>").
  table("ledger").insert({ rows: { amount: 9007199254740993n } });

  // @ts-expect-error — bigint defaults are refused by the authored value union.
  table("ledger").create({ columns: { amount: t.numeric(38, 0).default(9007199254740993n) } });

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
