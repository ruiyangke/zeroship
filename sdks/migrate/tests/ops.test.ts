// `@zeroship/migrate` — the fluent DSL records the same frozen wire ops the
// engine recorder + golden corpus pin. The DSL's `__begin`/`__drain` ambient
// recorder (the build-evaluator seam) is driven directly so a test can assert the
// recorded op objects without the Rust V8 host. Authoring is via the SOLE public
// entry `table()`.

import assert from "node:assert/strict";
import { test } from "node:test";

import { t, table, lintDeterminism } from "../src/index.js";
// The build-evaluator recorder seam (not part of the public surface).
import { __begin, __drain } from "../src/ops.js";

/** Record one phase's ops via the ambient recorder. */
function record(up: () => void): any[] {
  __begin();
  up();
  return __drain();
}

test("t.text() is nullable-by-default; .notNull() opts in", () => {
  const ops = record(() => {
    table("u").create({ columns: { a: t.text(), b: t.text().notNull() } });
  });
  const cols = ops[0].columns;
  assert.equal(cols[0].nullable, undefined, "t.text() omits nullable (nullable-by-default)");
  assert.equal(cols[1].nullable, false, "t.text().notNull() records nullable:false");
});

test("t.id() records a uuid PK + genRandomUuid default + hoisted pk constraint", () => {
  const ops = record(() => {
    table("u").create({ columns: { id: t.id() } });
  });
  const col = ops[0].columns[0];
  assert.equal(col.type, "uuid");
  assert.equal(col.nullable, false);
  assert.deepEqual(col.default, { fn: { fn: "genRandomUuid" } });
  assert.deepEqual(ops[0].constraints, [{ kind: { kind: "pk", columns: ["id"] } }]);
});

test("create() with a composite primaryKey records the table-level pk", () => {
  const ops = record(() =>
    table("m").create({
      columns: { a: t.uuid().notNull(), b: t.text().notNull() },
      primaryKey: ["a", "b"],
    }),
  );
  assert.deepEqual(ops[0].constraints, [{ kind: { kind: "pk", columns: ["a", "b"] } }]);
});

test(".column().add() carries a fluent ColumnDef's modifiers", () => {
  const ops = record(() =>
    table("u").column("status").add({ type: t.text().notNull().default("new") }),
  );
  assert.deepEqual(ops[0], {
    op: "addColumn",
    table: "u",
    column: "status",
    type: "text",
    nullable: false,
    default: { literal: { value: "new" } },
  });
});

test("C2 — .column().add({ type: t.text().unique() }) emits the column + a follow-on unique", () => {
  const ops = record(() => table("u").column("email").add({ type: t.text().notNull().unique() }));
  assert.equal(ops.length, 2, "an addColumn + a follow-on addConstraint(unique)");
  assert.equal(ops[0].op, "addColumn");
  assert.equal(ops[0].column, "email");
  // The inline `unique:true` is NOT recorded on the ADD COLUMN (no inline UNIQUE);
  // it becomes a separate ADD CONSTRAINT.
  assert.equal(ops[0].unique, undefined);
  assert.equal(ops[1].op, "addConstraint");
  assert.deepEqual(ops[1].constraint, { kind: { kind: "unique", columns: ["email"] } });
});

test("C2 — .column().add({ type: t.uuid().primaryKey() }) emits the column + a follow-on pk", () => {
  const ops = record(() => table("u").column("id").add({ type: t.uuid().primaryKey() }));
  assert.equal(ops.length, 2);
  assert.equal(ops[0].op, "addColumn");
  assert.equal(ops[0].nullable, false, "a PK column is NOT NULL");
  assert.equal(ops[1].op, "addConstraint");
  assert.deepEqual(ops[1].constraint, { kind: { kind: "pk", columns: ["id"] } });
});

test("C2 — .column().add({ type: t.text().unique().primaryKey() }) suppresses the redundant unique", () => {
  // A PRIMARY KEY already implies uniqueness, so the follow-on UNIQUE is redundant
  // DDL — only the pk add is recorded (no extra addConstraint(unique)).
  const ops = record(() => table("u").column("id").add({ type: t.text().unique().primaryKey() }));
  assert.equal(ops.length, 2, "an addColumn + ONLY the pk add (no redundant unique)");
  assert.equal(ops[0].op, "addColumn");
  assert.equal(ops[1].op, "addConstraint");
  assert.deepEqual(
    ops[1].constraint,
    { kind: { kind: "pk", columns: ["id"] } },
    "the single follow-on constraint is the pk, not a redundant unique",
  );
  // Order-independence: .primaryKey().unique() suppresses the unique too.
  const ops2 = record(() => table("u").column("id").add({ type: t.text().primaryKey().unique() }));
  assert.equal(ops2.length, 2, "order-independent: still no redundant unique");
  assert.deepEqual(ops2[1].constraint, { kind: { kind: "pk", columns: ["id"] } });
});

test(".foreignKey().add() field order is irrelevant (named fields, not positionals)", () => {
  const a = record(() =>
    table("orders").foreignKey("fk").add({
      columns: ["customer_id"],
      references: { table: "customers", columns: ["id"] },
    }),
  );
  const b = record(() =>
    table("orders").foreignKey("fk").add({
      references: { columns: ["id"], table: "customers" },
      columns: ["customer_id"],
    }),
  );
  assert.deepEqual(a[0], b[0]);
  assert.equal(a[0].constraint.kind.kind, "fk");
  assert.equal(a[0].constraint.kind.referencesTable, "customers");
});

test("C1 — .foreignKey().add({ onDelete }) emits onDelete/onUpdate; absent ⇒ omitted", () => {
  const withAction = record(() =>
    table("orders").foreignKey("fk").add({
      columns: ["customer_id"],
      references: { table: "customers", columns: ["id"] },
      onDelete: "cascade",
      onUpdate: "setNull",
    }),
  );
  assert.equal(withAction[0].constraint.kind.onDelete, "cascade");
  assert.equal(withAction[0].constraint.kind.onUpdate, "setNull");

  const noAction = record(() =>
    table("orders").foreignKey("fk").add({
      columns: ["customer_id"],
      references: { table: "customers", columns: ["id"] },
    }),
  );
  assert.ok(
    !("onDelete" in noAction[0].constraint.kind) && !("onUpdate" in noAction[0].constraint.kind),
    "an action-free FK omits onDelete/onUpdate (checksum neutrality)",
  );
});

test("insert row-object normalizes to columns + positional rows", () => {
  const ops = record(() =>
    table("t").insert({ rows: [{ code: 1, label: "a" }, { code: 2, label: "b" }] }),
  );
  assert.deepEqual(ops[0].columns, ["code", "label"]);
  assert.deepEqual(ops[0].rows, [[1, "a"], [2, "b"]]);
});

test("insert normalizes a bigint to {decimal} and Uint8Array to {bytes:base64}", () => {
  const ops = record(() =>
    table("t").insert({ rows: [{ big: 9007199254740993n, raw: new Uint8Array([1, 2, 3]) }] }),
  );
  assert.deepEqual(ops[0].rows, [[{ decimal: "9007199254740993" }, { bytes: "AQID" }]]);
  assert.doesNotThrow(() => JSON.stringify(ops[0]));
});

test("a column default carries a bigint/Uint8Array through the same IrScalar carrier", () => {
  const ops = record(() =>
    table("t").create({
      columns: {
        big: t.numeric(38, 0).default(9007199254740993n),
        raw: t.bytes().default(new Uint8Array([255, 0])),
      },
    }),
  );
  const cols = ops[0].columns;
  assert.deepEqual(cols[0].default, { literal: { value: { decimal: "9007199254740993" } } });
  assert.deepEqual(cols[1].default, { literal: { value: { bytes: "/wA=" } } });
});

test("onConflict.doUpdate normalizes bigint/Uint8Array scalar assignments", () => {
  const ops = record(() =>
    table("t").insert({
      rows: [{ id: 1 }],
      onConflict: { columns: ["id"], doUpdate: { big: 9007199254740993n, raw: new Uint8Array([7]) } as any },
    }),
  );
  assert.deepEqual(ops[0].onConflict.doUpdate, {
    big: { decimal: "9007199254740993" },
    raw: { bytes: "Bw==" },
  });
});

test("update accepts a batch knob (parity with the engine recorder)", () => {
  const ops = record(() =>
    table("t").update({
      set: { x: (c) => c.fn.now() },
      where: (c) => c("id").isNotNull(),
      batch: { cursorColumn: "id", batchSize: 500 },
    }),
  );
  assert.deepEqual(ops[0].batch, { cursorColumn: "id", batchSize: 500 });
});

test("del records the 'delete' wire tag and requires where", () => {
  const ops = record(() => table("t").del({ where: (c) => c("code").isNull(), limit: 5 }));
  assert.equal(ops[0].op, "delete");
  assert.equal(ops[0].limit, 5);
  assert.throws(() => record(() => table("t").del({} as any)), /where is mandatory/);
});

test("the (c) => Expr builder constructs the closed AST", () => {
  const ops = record(() =>
    table("t").update({
      set: {
        a: (c) => c("x").add(1).mul(2).cast("integer"),
        b: (c) => c.fn.concatWs(" ", c("p"), c("q")),
        d: (c) => c.fn.case([[c("x").lt(0), c("y")]], c("z")),
      },
      where: (c) => c("x").gt(0).and(c("y").isNotNull()),
    }),
  );
  const set = ops[0].set;
  assert.equal(set.a.node, "cast");
  assert.equal(set.a.target, "integer");
  assert.equal(set.a.operand.op, "mul");
  assert.equal(set.b.node, "fnSynth");
  assert.equal(set.b.fn, "concatWs");
  assert.equal(set.d.node, "case");
  assert.equal(ops[0].where.op, "and");
});

test("backfill defaults cursorColumn to 'id' and batchSize to the engine default", () => {
  const ops = record(() => table("u").backfill({ set: { x: (c) => c.fn.now() } }));
  assert.equal(ops[0].cursorColumn, "id");
  assert.equal(typeof ops[0].batchSize, "number");
  assert.equal(ops[0].name, "backfill_u");
  assert.equal(ops[0].set.x.fn, "now");
});

test("c.fn.splitPart grammar lint rejects an empty delimiter / non-positive n", () => {
  const isExprNotPortable = (e: any) => e.code === "EXPR_NOT_PORTABLE";
  assert.throws(() => record(() => table("u").update({ set: { x: (c) => c.fn.splitPart(c("n"), "", 1) } })), isExprNotPortable);
  assert.throws(() => record(() => table("u").update({ set: { x: (c) => c.fn.splitPart(c("n"), " ", 0) } })), isExprNotPortable);
  const ops = record(() => table("u").update({ set: { x: (c) => c.fn.splitPart(c("n"), " ", 1) } }));
  assert.equal(ops[0].set.x.fn, "splitPart");
});

test("authoring outside a recorder throws OP_OUTSIDE_RECORDER", () => {
  __begin();
  __drain(); // close the recorder
  assert.throws(
    () => table("u").create({ columns: { id: t.id() } }),
    (e: any) => e.code === "OP_OUTSIDE_RECORDER",
  );
});

test("determinism lint flags Date.now()/Math.random()/new Date(); clean source is empty", () => {
  for (const frag of ["Date.now()", "Math.random()", "crypto.randomUUID()", "new Date()"]) {
    const findings = lintDeterminism(`table("t").insert({ rows: [{ v: ${frag} }] });`);
    assert.ok(findings.length >= 1, `${frag} must be flagged`);
    assert.equal(findings[0].code, "NONDETERMINISTIC_OP_ARG");
  }
  assert.deepEqual(lintDeterminism(`table("t").backfill({ set: { v: c => c.fn.now() } });`), []);
});

test("determinism lint is a coarse whole-source scan (over-flags, never under-flags)", () => {
  const inComment = lintDeterminism(`// audited at new Date()\ntable("t").create({ columns: { id: t.id() } });`);
  assert.ok(
    inComment.some((f) => f.accessor.includes("new Date")),
    "the coarse scan flags a clock accessor even in a comment (fail-safe over-flag)",
  );
  const inHelper = lintDeterminism(`function audit() { return Date.now(); }\ntable("t").insert({ rows: [{ a: 1 }] });`);
  assert.ok(
    inHelper.some((f) => f.accessor.includes("Date.now")),
    "the coarse scan flags a clock accessor in a non-op helper (fail-safe over-flag)",
  );
});
