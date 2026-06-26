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
  type ColumnDef,
  type DbFieldType,
} from "../../src/index.js";

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
  table("nonexistent_table").column("legacy_col").alter({ nullable: false });
  table("nonexistent_table").foreignKey("fk").add({
    columns: ["author_id"],
    references: { table: "another_missing_table", columns: ["id"] },
  });
  table("nonexistent_table").update({
    set: { legacy_col: (c) => c("a_column_no_schema_declares").concat(" suffix") },
    where: (c) => c("yet_another_missing_column").isNull(),
  });
  table("nonexistent_table").del({ where: (c) => c("phantom_col").eq(1) });
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

  // @ts-expect-error — `del` requires a `where` predicate (mandatory).
  table("users").del({});

  // @ts-expect-error — `update.set` values must be (c) => Expr callbacks, not raw strings.
  table("users").update({ set: { name: "raw sql string" } });

  // @ts-expect-error — the table-level `.rename({ to })` REQUIRES a `to` string.
  table("users").rename({});

  // @ts-expect-error — `.rename({ to })` has no `from` (that is the column-rename shape).
  table("users").rename({ from: "users", to: "people" });
}

// The table-level `.rename({ to })` now type-checks (the renameTable op shipped):
// a bare rename and a schema+ifExists rename, both returning the chainable handle.
export function goodTableRename(): void {
  table("users").rename({ to: "people" });
  table("users").rename({ to: "people", ifExists: true, schema: "reporting" });
}

// ───────────────────────────────────────────────────────────────────────────
// 3. The `t.*` ColType builder is structurally typed.
// ───────────────────────────────────────────────────────────────────────────

export function badColTypes(): void {
  // @ts-expect-error — `t` has no `t.notARealType()` factory.
  t.notARealType();

  // @ts-expect-error — the removed `t.string` alias (canonical is t.text()).
  t.string();

  // @ts-expect-error — the removed `t.int` alias (canonical is t.integer()).
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

  // @ts-expect-error — a function is not a valid insert ScalarValue.
  table("users").insert({ rows: { name: () => "nope" } });
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
  table("t").create({ columns: { n: t.integer() }, ifNotExists: true });
  table("t").column("email").add({ type: t.text(), ifNotExists: true });
  table("t").column("legacy").drop({ ifExists: true });
  table("t").column("a").alter({ type: t.bigInt() });
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
