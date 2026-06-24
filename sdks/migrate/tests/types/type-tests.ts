// PR5 — STRUCTURAL type-safety, names-stay-strings (BINDING, §3.3).
//
// This file is a TYPE-LEVEL test: it is checked by `tsc --noEmit` via
// `tsconfig.types.json` (the `typecheck:types` script). It compiles cleanly IFF
// every `@ts-expect-error` line is a genuine type error and every other line is
// well-typed. A removed/weakened structural guard would make a `@ts-expect-error`
// line stop erroring → `tsc` fails with "Unused '@ts-expect-error' directive".
//
// The guarantee under test (§3.3):
//   - op-function argument SHAPES, the `t.*` ColType builder, the fluent-expression
//     node shapes, and insert-row VALUE shapes ARE structurally typed (a malformed
//     shape / invalid ColType FAILS tsc);
//   - table/column NAMES are plain `string`, NOT bound to the live `@zeroship/db`
//     schema (a migration naming a non-existent table/column TYPE-CHECKS cleanly —
//     existence is an apply-time check, the anti-rot guarantee).

import { t as dbT } from "@zeroship/db";

import {
  addColumn,
  addForeignKey,
  alterColumn,
  backfill,
  colTypeFromDbField,
  createTable,
  del,
  dropColumn,
  fromDb,
  insert,
  t,
  update,
  type ColumnDef,
} from "../../src/index.js";

// ───────────────────────────────────────────────────────────────────────────
// 1. NAMES STAY STRINGS — the anti-rot guarantee (§3.3).
//
//    A real migration whose table/column names are plain strings TYPE-CHECKS
//    cleanly EVEN THOUGH none of these names exist in any `@zeroship/db` schema.
//    Names are NOT live-schema-bound; existence is validated at apply time.
// ───────────────────────────────────────────────────────────────────────────

export function antiRotMigration(): void {
  // `nonexistent_table` / `column_that_was_dropped` / `legacy_col` are not in any
  // live schema — yet every op below compiles, because names are plain strings.
  createTable("nonexistent_table", {
    legacy_col: t.text(),
    author_id: t.ref("a_table_that_does_not_exist"),
  });
  addColumn("nonexistent_table", "column_that_was_dropped", t.text());
  dropColumn("nonexistent_table", "column_that_was_dropped");
  alterColumn("nonexistent_table", "legacy_col", { nullable: false });
  addForeignKey("nonexistent_table", {
    columns: ["author_id"],
    references: { table: "another_missing_table", columns: ["id"] },
  });
  // A `c("…")` ColRef argument is a plain string too — referencing a column that
  // does not exist in any schema still type-checks; resolution is apply/render-time.
  update("nonexistent_table", {
    set: { legacy_col: (c) => c("a_column_no_schema_declares").concat(" suffix") },
    where: (c) => c("yet_another_missing_column").isNull(),
  });
  del("nonexistent_table", { where: (c) => c("phantom_col").eq(1) });
  backfill("nonexistent_table", {
    set: { legacy_col: (c) => c.fn.splitPart(c("phantom_col"), " ", 1) },
    where: (c) => c("phantom_col").isNotNull(),
  });
  // Insert-row VALUE shapes are typed (scalar kinds), but the column KEYS are
  // free strings not bound to any live row shape.
  insert("nonexistent_table", { rows: { phantom_col: "ok", another_phantom: 42 } });
}

// ───────────────────────────────────────────────────────────────────────────
// 2. Op-function argument SHAPES are structurally typed — a malformed shape
//    FAILS tsc.
// ───────────────────────────────────────────────────────────────────────────

export function badOpShapes(): void {
  // @ts-expect-error — addColumn's `type` must be a ColumnDef, not a number.
  addColumn("users", "age", 123);

  // @ts-expect-error — addColumn's `type` must be a ColumnDef, not a bare string.
  addColumn("users", "age", "int");

  // @ts-expect-error — missing the required `type` argument.
  addColumn("users", "age");

  // @ts-expect-error — createTable columns must be a Record of ColumnDef, not numbers.
  createTable("users", { age: 123 });

  // @ts-expect-error — `name` must be a string, not a number.
  addColumn(42, "age", t.text());

  // @ts-expect-error — addForeignKey spec needs `references`, not a bare columns list.
  addForeignKey("orders", { columns: ["user_id"] });

  // @ts-expect-error — `del` requires a `where` predicate (mandatory).
  del("users", {});

  // @ts-expect-error — `update.set` values must be (c) => Expr callbacks, not raw strings.
  update("users", { set: { name: "raw sql string" } });
}

// ───────────────────────────────────────────────────────────────────────────
// 3. The `t.*` ColType builder is structurally typed — an invalid type factory
//    or modifier FAILS tsc.
// ───────────────────────────────────────────────────────────────────────────

export function badColTypes(): void {
  // @ts-expect-error — `t` has no `t.notARealType()` factory.
  t.notARealType();

  // @ts-expect-error — `.notNull()` takes no argument.
  t.text().notNull("yes");

  // @ts-expect-error — `.ref(target)` requires a string target table.
  t.text().ref(123);

  // @ts-expect-error — there is no `.frobnicate()` chain modifier.
  t.text().frobnicate();

  // @ts-expect-error — `t.vector(n)` requires a numeric dimension.
  t.vector("not a number");
}

// ───────────────────────────────────────────────────────────────────────────
// 4. The fluent-expression node shapes are structurally typed — calling a
//    non-existent operator method FAILS tsc.
// ───────────────────────────────────────────────────────────────────────────

export function badExprShapes(): void {
  update("users", {
    // @ts-expect-error — there is no `.frobnicate()` operator on the expr chain.
    set: { name: (c) => c("name").frobnicate() },
  });

  update("users", {
    // @ts-expect-error — `c.fn` has no `notARealFn` member.
    set: { name: (c) => c.fn.notARealFn(c("name")) },
  });

  update("users", {
    // @ts-expect-error — `.cast(...)` only accepts the closed portable target set.
    set: { name: (c) => c("name").cast("jsonb") },
  });
}

// ───────────────────────────────────────────────────────────────────────────
// 5. Insert-row VALUE shapes are typed (scalar kinds); the optional row generic
//    is CALLER-supplied, never auto-derived from the live schema (§3.3).
// ───────────────────────────────────────────────────────────────────────────

// A `type` alias (not an `interface`) so the all-scalar shape satisfies the
// `Row = Record<string, ScalarValue>` constraint — the caller-supplied generic.
type MyRow = {
  name: string;
  count: number;
};

export function insertValueShapes(): void {
  // A caller-supplied generic constrains the VALUE shape (editor convenience).
  insert<MyRow>("users", { rows: { name: "ada", count: 1 } });

  // @ts-expect-error — `count` must be a number per the caller-supplied generic.
  insert<MyRow>("users", { rows: { name: "ada", count: "not a number" } });

  // A function value is not a ScalarValue — rejected even in the loose default row.
  // @ts-expect-error — a function is not a valid insert ScalarValue.
  insert("users", { rows: { name: () => "nope" } });
}

// ───────────────────────────────────────────────────────────────────────────
// 6. The shared `@zeroship/db` lexicon bridge (PR5 goal A) is typed: `fromDb`
//    accepts a `@zeroship/db` field and yields a migration `ColumnDef`;
//    `colTypeFromDbField` reduces a db field to a ColType.
// ───────────────────────────────────────────────────────────────────────────

export function lexiconBridgeShapes(): void {
  const def: ColumnDef = fromDb(dbT.ref("users"));
  addColumn("posts", "author_id", def);
  // Directly accepting a db field through the bridge in createTable.
  createTable("posts", { author_id: fromDb(dbT.string().required()) });

  // colTypeFromDbField reduces a db field to a ColType value.
  const _ct = colTypeFromDbField(dbT.json());
  void _ct;

  // @ts-expect-error — `fromDb` takes a @zeroship/db field, not a bare string.
  fromDb("text");
}
