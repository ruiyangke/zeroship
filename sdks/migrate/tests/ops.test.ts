// `@zeroship/migrate` — the npm DSL records the same frozen wire ops the engine
// recorder + golden corpus pin. The DSL's `__begin`/`__drain` ambient recorder
// (the build-evaluator seam) is driven directly so a test can assert the recorded
// op objects without the Rust V8 host.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  addColumn,
  addForeignKey,
  backfill,
  createTable,
  del,
  insert,
  t,
  update,
  lintDeterminism,
} from "../src/index.js";
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
    createTable("u", { a: t.text(), b: t.text().notNull() });
  });
  const cols = ops[0].columns;
  assert.equal(cols[0].nullable, undefined, "t.text() omits nullable (nullable-by-default)");
  assert.equal(cols[1].nullable, false, "t.text().notNull() records nullable:false");
});

test("t.id() records a uuid PK + genRandomUuid default + hoisted pk constraint", () => {
  const ops = record(() => {
    createTable("u", { id: t.id() });
  });
  const col = ops[0].columns[0];
  assert.equal(col.type, "uuid");
  assert.equal(col.nullable, false);
  assert.deepEqual(col.default, { fn: { fn: "genRandomUuid" } });
  assert.deepEqual(ops[0].constraints, [{ kind: { kind: "pk", columns: ["id"] } }]);
});

test("createTable object-literal and (b) => … overload record the same op", () => {
  const a = record(() => createTable("u", { id: t.uuid().notNull(), email: t.text() }));
  const b = record(() => createTable("u", { id: t.uuid().notNull(), email: t.text() }, () => {}));
  assert.deepEqual(a[0], b[0]);
});

test("addColumn carries a fluent ColumnDef's modifiers", () => {
  const ops = record(() => addColumn("u", "status", t.text().notNull().default("new")));
  assert.deepEqual(ops[0], {
    op: "addColumn",
    table: "u",
    column: "status",
    type: "text",
    nullable: false,
    default: { literal: { value: "new" } },
  });
});

test("addForeignKey field order is irrelevant (named fields, not positionals)", () => {
  const a = record(() =>
    addForeignKey("orders", {
      columns: ["customer_id"],
      references: { table: "customers", columns: ["id"] },
      name: "fk",
    }),
  );
  const b = record(() =>
    addForeignKey("orders", {
      name: "fk",
      references: { columns: ["id"], table: "customers" },
      columns: ["customer_id"],
    }),
  );
  assert.deepEqual(a[0], b[0]);
  assert.equal(a[0].constraint.kind.kind, "fk");
  assert.equal(a[0].constraint.kind.referencesTable, "customers");
});

test("insert row-object normalizes to columns + positional rows", () => {
  const ops = record(() =>
    insert("t", { rows: [{ code: 1, label: "a" }, { code: 2, label: "b" }] }),
  );
  assert.deepEqual(ops[0].columns, ["code", "label"]);
  assert.deepEqual(ops[0].rows, [[1, "a"], [2, "b"]]);
});

test("insert normalizes a bigint to {decimal} and Uint8Array to {bytes:base64}", () => {
  const ops = record(() =>
    insert("t", {
      rows: [{ big: 9007199254740993n, raw: new Uint8Array([1, 2, 3]) }],
    }),
  );
  // bigint -> {decimal:"…"} (the IrScalar large-int carrier; a bare bigint THROWS at
  // JSON.stringify), Uint8Array -> {bytes: base64} (Rust IrScalar hard-rejects the
  // {"0":…} array-index spelling).
  assert.deepEqual(ops[0].rows, [[{ decimal: "9007199254740993" }, { bytes: "AQID" }]]);
  // The recorded op must be JSON-serializable (a raw bigint would throw here).
  assert.doesNotThrow(() => JSON.stringify(ops[0]));
});

test("a column default carries a bigint/Uint8Array through the same IrScalar carrier", () => {
  const ops = record(() =>
    createTable("t", {
      big: t.numeric(38, 0).default(9007199254740993n),
      raw: t.bytes().default(new Uint8Array([255, 0])),
    }),
  );
  const cols = ops[0].columns;
  assert.deepEqual(cols[0].default, { literal: { value: { decimal: "9007199254740993" } } });
  assert.deepEqual(cols[1].default, { literal: { value: { bytes: "/wA=" } } });
});

test("onConflict.doUpdate normalizes bigint/Uint8Array scalar assignments", () => {
  const ops = record(() =>
    insert("t", {
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
    update("t", {
      set: { x: (c) => c.fn.now() },
      where: (c) => c("id").isNotNull(),
      batch: { cursorColumn: "id", batchSize: 500 },
    }),
  );
  assert.deepEqual(ops[0].batch, { cursorColumn: "id", batchSize: 500 });
});

test("del records the 'delete' wire tag and requires where", () => {
  const ops = record(() => del("t", { where: (c) => c("code").isNull(), limit: 5 }));
  assert.equal(ops[0].op, "delete");
  assert.equal(ops[0].limit, 5);
  assert.throws(() => record(() => del("t", {} as any)), /where is mandatory/);
});

test("the (c) => Expr builder constructs the closed AST", () => {
  const ops = record(() =>
    update("t", {
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
  const ops = record(() => backfill("u", { set: { x: (c) => c.fn.now() } }));
  assert.equal(ops[0].cursorColumn, "id");
  assert.equal(typeof ops[0].batchSize, "number");
  assert.equal(ops[0].name, "backfill_u");
  assert.equal(ops[0].set.x.fn, "now");
});

test("c.fn.splitPart grammar lint rejects an empty delimiter / non-positive n", () => {
  const isExprNotPortable = (e: any) => e.code === "EXPR_NOT_PORTABLE";
  assert.throws(() => record(() => update("u", { set: { x: (c) => c.fn.splitPart(c("n"), "", 1) } })), isExprNotPortable);
  assert.throws(() => record(() => update("u", { set: { x: (c) => c.fn.splitPart(c("n"), " ", 0) } })), isExprNotPortable);
  // In-envelope records fine.
  const ops = record(() => update("u", { set: { x: (c) => c.fn.splitPart(c("n"), " ", 1) } }));
  assert.equal(ops[0].set.x.fn, "splitPart");
});

test("calling an op outside a recorder throws OP_OUTSIDE_RECORDER", () => {
  __begin();
  __drain(); // close the recorder
  assert.throws(
    () => createTable("u", { id: t.id() }),
    (e: any) => e.code === "OP_OUTSIDE_RECORDER",
  );
});

test("determinism lint flags Date.now()/Math.random()/new Date(); clean source is empty", () => {
  for (const frag of ["Date.now()", "Math.random()", "crypto.randomUUID()", "new Date()"]) {
    const findings = lintDeterminism(`insert("t", { rows: [{ v: ${frag} }] });`);
    assert.ok(findings.length >= 1, `${frag} must be flagged`);
    assert.equal(findings[0].code, "NONDETERMINISTIC_OP_ARG");
  }
  assert.deepEqual(lintDeterminism(`backfill("t", { set: { v: c => c.fn.now() } });`), []);
});

test("determinism lint is a coarse whole-source scan: a clock accessor outside any op arg is (intentionally) flagged", () => {
  // §4.3 specifies "syntactically appearing inside an op-function argument", but the
  // shipped lint is a deliberate fail-SAFE whole-source regex scan (over-flags, never
  // under-flags). Pin that chosen behavior: a `new Date()` in a comment / a non-op
  // helper still trips the lint. This is the contract, not a bug.
  const inComment = lintDeterminism(`// audited at new Date()\ncreateTable("t", { id: t.id() });`);
  assert.ok(
    inComment.some((f) => f.accessor.includes("new Date")),
    "the coarse scan flags a clock accessor even in a comment (fail-safe over-flag)",
  );
  const inHelper = lintDeterminism(`function audit() { return Date.now(); }\ninsert("t", { rows: [{ a: 1 }] });`);
  assert.ok(
    inHelper.some((f) => f.accessor.includes("Date.now")),
    "the coarse scan flags a clock accessor in a non-op helper (fail-safe over-flag)",
  );
});
