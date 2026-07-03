// `@zeroship/migrate` — the fluent DSL records the same frozen wire ops the
// engine recorder + golden corpus pin. The DSL's `__begin`/`__drain` ambient
// recorder (the build-evaluator seam) is driven directly so a test can assert the
// recorded op objects without the Rust V8 host. Table authoring is via the
// reusable public entry `table()`.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  t,
  table,
  comment,
  lintDeterminism,
  enumType,
  check,
  and,
  or,
  not,
  membership,
  notMembership,
  lit,
  interval,
} from "../src/index.js";
import { domain, sequence } from "../src/pg.js";
// The build-evaluator recorder seam (not part of the public surface).
import { __begin, __drain } from "../src/ops.js";

/** Record one phase's ops via the ambient recorder. */
function record(up: () => void): any[] {
  __begin();
  up();
  return __drain();
}

test("@zeroship/migrate core exports enumType and omits pg-only/old names", async () => {
  const imported = await import("@zeroship/migrate");
  assert.equal(typeof imported.enumType, "function");
  assert.equal(typeof imported.check, "function");
  assert.equal(typeof imported.membership, "function");
  assert.equal(typeof imported.interval, "function");
  assert.equal((imported as any).pgEnum, undefined);
  assert.equal((imported as any).pgDomain, undefined);
  assert.equal((imported as any).domain, undefined);
  assert.equal((imported as any).sequence, undefined);
});

test("SA-7: enumType.create rejects an empty values[] at authoring time", () => {
  assert.throws(
    () => record(() => enumType("empty_enum").create({ values: [] })),
    (e: any) => e.code === "OP_INVALID" && /non-empty string\[\]/.test(e.message),
  );
});

test("enumType is inert until a terminal records", () => {
  const ops = record(() => {
    enumType("inert_enum");
  });
  assert.deepEqual(ops, []);

  const createOps = record(() => {
    enumType("active_enum").create({ values: ["a", "b"] });
  });
  assert.deepEqual(createOps, [
    { op: "createEnum", name: "active_enum", values: ["a", "b"] },
  ]);
});

test("SA-6: sequence({ as }) rejects a type outside { int, bigInt }", () => {
  assert.throws(
    () => record(() => sequence("s").create({ as: t.text() })),
    (e: any) => e.code === "OP_INVALID" && /as must be one of int \| bigInt/.test(e.message),
  );
  // valid tokens still record
  const ops = record(() => sequence("s2").create({ as: t.bigInt() }));
  assert.equal(ops[0].as, "bigInt");
});

test("t.text() is nullable-by-default; .notNull() opts in", () => {
  const ops = record(() => {
    table("u").create({ columns: { a: t.text(), b: t.text().notNull() } });
  });
  const cols = ops[0].columns;
  assert.equal(cols[0].nullable, undefined, "t.text() omits nullable (nullable-by-default)");
  assert.equal(cols[1].nullable, false, "t.text().notNull() records nullable:false");
});

test("t.id() records a uuid PK + genRandomUuid default + top-level primaryKey", () => {
  const ops = record(() => {
    table("u").create({ columns: { id: t.id() } });
  });
  const col = ops[0].columns[0];
  assert.equal(col.type, "uuid");
  assert.equal(col.nullable, false);
  assert.deepEqual(col.default, { fn: { fn: "genRandomUuid" } });
  assert.deepEqual(ops[0].primaryKey, ["id"]);
  assert.equal(ops[0].constraints, undefined);
});

test("create() without primaryKey leaves the top-level field absent", () => {
  const ops = record(() => table("u").create({ columns: { name: t.text() } }));
  assert.equal(Object.prototype.hasOwnProperty.call(ops[0], "primaryKey"), false);
  assert.equal(ops[0].constraints, undefined);
});

test("create() with a composite primaryKey records the top-level primaryKey", () => {
  const ops = record(() =>
    table("m").create({
      columns: { a: t.uuid().notNull(), b: t.text().notNull() },
      primaryKey: ["a", "b"],
    }),
  );
  assert.deepEqual(ops[0].primaryKey, ["a", "b"]);
  assert.equal(ops[0].constraints, undefined);
});

test("C2 — create() column that is both .unique() + .primaryKey() emits NO column-level unique", () => {
  // A PRIMARY KEY already implies uniqueness, so the per-column image must NOT
  // carry `unique:true` (lock-step with the addColumn-path suppression + the
  // differ) — only the top-level primaryKey is recorded.
  const ops = record(() =>
    table("u").create({ columns: { x: t.text().unique().primaryKey() } }),
  );
  const col = ops[0].columns[0];
  assert.equal(col.name, "x");
  assert.equal(col.unique, undefined, "no redundant column-level unique alongside the pk");
  assert.deepEqual(ops[0].primaryKey, ["x"]);
  assert.equal(ops[0].constraints, undefined, "no pk is hoisted into constraints");
  // Order-independence + a plain .unique() (no pk) still emits the column-level unique.
  const ops2 = record(() =>
    table("u").create({ columns: { x: t.text().primaryKey().unique(), y: t.text().unique() } }),
  );
  assert.equal(ops2[0].columns[0].unique, undefined, "order-independent: pk column drops unique");
  assert.equal(ops2[0].columns[1].unique, true, "a non-pk .unique() column still records unique");
  assert.deepEqual(ops2[0].primaryKey, ["x"]);
  assert.equal(ops2[0].constraints, undefined, "order-independent: no pk is hoisted");
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

test(".addForeignKey() records composite/non-id references without serializing reference schema", () => {
  const ops = record(() =>
    table("billing_line_provider_refs", { schema: "zeroship" }).addForeignKey("billing_line_provider_refs_line_fk", {
      columns: ["invoice_id", "app_id", "segment_no"],
      references: {
        schema: "zeroship",
        table: "invoice_lines",
        columns: ["invoice_id", "app_id", "segment_no"],
      },
      onDelete: "cascade",
    }),
  );

  assert.deepEqual(ops[0].constraint.kind, {
    kind: "fk",
    columns: ["invoice_id", "app_id", "segment_no"],
    referencesTable: "invoice_lines",
    referencesColumns: ["invoice_id", "app_id", "segment_no"],
    onDelete: "cascade",
  });
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

test("insert row-object rejects ragged later-row keys", () => {
  assert.throws(
    () => record(() => table("t").insert({ rows: [{ a: 1 }, { a: 2, b: 2 }] } as any)),
    (e: any) => e.code === "OP_INVALID" && /ragged insert rows/.test(e.message),
  );
});

test("insert normalizes a bigint to {decimal} and Uint8Array to {bytes:base64}", () => {
  const ops = record(() =>
    table("t").insert({ rows: [{ big: 9007199254740993n, raw: new Uint8Array([1, 2, 3]) }] }),
  );
  assert.deepEqual(ops[0].rows, [[{ decimal: "9007199254740993" }, { bytes: "AQID" }]]);
  assert.doesNotThrow(() => JSON.stringify(ops[0]));
});

test("non-native function values fail closed instead of recording as JSON null", () => {
  const isInvalidFunction = (e: any) =>
    e.code === "OP_INVALID" &&
    /function values are not valid here/.test(e.message) &&
    /Date\.now \/ Math\.random \/ crypto\.randomUUID/.test(e.message);

  assert.throws(
    () => record(() => table("t").insert({ rows: [{ v: () => 42 }] } as any)),
    isInvalidFunction,
  );
  assert.throws(
    () => record(() => table("t").insert({ rows: [{ a: 1 }, { b: () => 42 }] } as any)),
    isInvalidFunction,
  );
  assert.throws(
    () => record(() => table("t").insert({ rows: [{ doc: { a: () => 42 } }] } as any)),
    isInvalidFunction,
  );
  assert.throws(
    () => record(() => table("t").insert({ rows: [{ tags: [() => 42] }] } as any)),
    isInvalidFunction,
  );
  assert.throws(
    () => record(() => table("t").insert({ rows: [{ doc: { a: Date.now } }] } as any)),
    isInvalidFunction,
  );
  assert.throws(
    () => record(() => table("t").create({ columns: { v: t.text().default((() => 1) as any) } })),
    isInvalidFunction,
  );
  assert.throws(
    () =>
      record(() =>
        table("t").create({ columns: { v: t.json().default({ doc: { a: () => 1 } } as any) } }),
      ),
    isInvalidFunction,
  );
  assert.throws(
    () =>
      record(() =>
        table("t").insert({
          rows: [{ id: 1 }],
          onConflict: { columns: ["id"], doUpdate: { v: () => 42 } as any },
        }),
      ),
    isInvalidFunction,
  );
  assert.throws(
    () =>
      record(() =>
        table("t").insert({
          rows: [{ id: 1 }],
          onConflict: { columns: ["id"], doUpdate: { doc: { a: () => 42 } } as any },
        }),
      ),
    isInvalidFunction,
  );
  assert.throws(
    () => record(() => table("t").update({ set: { v: (c) => c.fn.lower((() => "x") as any) } })),
    isInvalidFunction,
  );
  assert.throws(
    () =>
      record(() =>
        table("t").update({ set: { v: (c) => c.fn.coalesce({ doc: { a: () => 1 } } as any, "x") } }),
      ),
    isInvalidFunction,
  );
});

test("supported native function symbols still record as fnSynth", () => {
  const rows: Record<string, unknown> = {
    at: Date.now,
    random_id: Math.random,
  };
  if (globalThis.crypto?.randomUUID !== undefined) {
    rows.uuid = globalThis.crypto.randomUUID;
  }
  const ops = record(() => table("t").insert({ rows: [rows] as any }));
  const values = ops[0].rows[0];
  assert.deepEqual(values[0], { node: "fnSynth", fn: "now", args: [] });
  assert.deepEqual(values[1], { node: "fnSynth", fn: "genRandomUuid", args: [] });
  if (globalThis.crypto?.randomUUID !== undefined) {
    assert.deepEqual(values[2], { node: "fnSynth", fn: "genRandomUuid", args: [] });
  }
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

test("c.pg builds PG-only membership regex and pg_column_size nodes", () => {
  const ops = record(() =>
    table("t").create({
      columns: {
        status: t.text().notNull(),
        name: t.text().notNull(),
        data: t.json().notNull(),
      },
      checks: [
        { name: "status_any", expr: (c) => c.pg.eqAnyArray(c("status"), ["a", "b"]) },
        { name: "status_ne_all", expr: (c) => c.pg.neAllArray(c("status"), ["x"]) },
        { name: "name_shape", expr: (c) => c.pg.regex(c("name"), "^[a-z]+$") },
        { name: "data_size", expr: (c) => c.pg.columnSize(c("data")).le(8192) },
      ],
    }),
  );
  const checks = ops[0].constraints.map((c: any) => c.kind.expr);
  assert.deepEqual(checks[0], {
    node: "pgArrayMembership",
    expr: { node: "colRef", name: "status" },
    op: "eq",
    elems: ["a", "b"],
  });
  assert.deepEqual(checks[1], {
    node: "pgArrayMembership",
    expr: { node: "colRef", name: "status" },
    op: "ne",
    elems: ["x"],
  });
  assert.deepEqual(checks[2], {
    node: "pgRegexMatch",
    expr: { node: "colRef", name: "name" },
    pattern: "^[a-z]+$",
  });
  assert.deepEqual(checks[3], {
    node: "binOp",
    op: "le",
    lhs: { node: "pgColumnSize", expr: { node: "colRef", name: "data" } },
    rhs: { node: "literal", value: 8192 },
  });
});

test("check helper and expression helpers build the frozen Expr IR nodes", () => {
  const ops = record(() => {
    table("expr_checks").create({
      columns: {
        pkce_method: t.text().notNull(),
        user_id: t.text().notNull(),
        kind: t.text().notNull(),
        data: t.json().notNull(),
        subtotal_cents: t.integer().notNull(),
        credit_cents: t.integer().notNull(),
        total_cents: t.integer().notNull(),
        floor_cents: t.integer(),
        created_at: t.timestamp().notNull(),
        expires_at: t.timestamp().notNull(),
        enabled: t.boolean().notNull(),
        visible: t.boolean().notNull(),
      },
      checks: [
        check("pkce_method_check", (c) => c("pkce_method").eq("S256")),
        check("user_id_fmt", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$")),
        check("kind_ok", (c) => membership(c("kind"), ["a", "b", "c"])),
        check("data_size", (c) => c("data").columnSize().lt(262144)),
        check("total_matches", (c) => c("total_cents").eq(c("subtotal_cents").sub(c("credit_cents")))),
        check("floor_nonneg_or_null", (c) => or(c("floor_cents").isNull(), c("floor_cents").ge(0))),
        check("enabled_and_visible", (c) => and(c("enabled"), c("visible"))),
        check("expires_window", (c) => c("expires_at").le(c("created_at").add(interval("00:01:00")))),
        check("not_archived", (c) => not(c("kind").eq(lit("archived")))),
        check("kind_not_reserved", (c) => notMembership(c("kind"), ["x", "y"])),
      ],
    });
    table("expr_checks").addCheck("score_nonnegative", (c) => c("total_cents").ge(0));
  });

  const checks = ops[0].constraints.map((c: any) => c.kind.expr);
  assert.deepEqual(checks[0], {
    node: "binOp",
    op: "eq",
    lhs: { node: "colRef", name: "pkce_method" },
    rhs: { node: "literal", value: "S256" },
  });
  assert.deepEqual(checks[1], {
    node: "pgRegexMatch",
    expr: { node: "colRef", name: "user_id" },
    pattern: "^usr_[0-9A-Za-z]{20,40}$",
  });
  assert.deepEqual(checks[2], {
    node: "pgArrayMembership",
    expr: { node: "colRef", name: "kind" },
    op: "eq",
    elems: ["a", "b", "c"],
  });
  assert.deepEqual(checks[3], {
    node: "binOp",
    op: "lt",
    lhs: { node: "pgColumnSize", expr: { node: "colRef", name: "data" } },
    rhs: { node: "literal", value: 262144 },
  });
  assert.deepEqual(checks[4], {
    node: "binOp",
    op: "eq",
    lhs: { node: "colRef", name: "total_cents" },
    rhs: {
      node: "binOp",
      op: "sub",
      lhs: { node: "colRef", name: "subtotal_cents" },
      rhs: { node: "colRef", name: "credit_cents" },
    },
  });
  assert.deepEqual(checks[5], {
    node: "binOp",
    op: "or",
    lhs: { node: "unaryOp", op: "isNull", operand: { node: "colRef", name: "floor_cents" } },
    rhs: {
      node: "binOp",
      op: "ge",
      lhs: { node: "colRef", name: "floor_cents" },
      rhs: { node: "literal", value: 0 },
    },
  });
  assert.deepEqual(checks[6], {
    node: "binOp",
    op: "and",
    lhs: { node: "colRef", name: "enabled" },
    rhs: { node: "colRef", name: "visible" },
  });
  assert.deepEqual(checks[7], {
    node: "binOp",
    op: "le",
    lhs: { node: "colRef", name: "expires_at" },
    rhs: {
      node: "binOp",
      op: "add",
      lhs: { node: "colRef", name: "created_at" },
      rhs: { node: "pgIntervalLiteral", value: "00:01:00" },
    },
  });
  assert.deepEqual(checks[8], {
    node: "unaryOp",
    op: "not",
    operand: {
      node: "binOp",
      op: "eq",
      lhs: { node: "colRef", name: "kind" },
      rhs: { node: "literal", value: "archived" },
    },
  });
  assert.deepEqual(checks[9], {
    node: "pgArrayMembership",
    expr: { node: "colRef", name: "kind" },
    op: "ne",
    elems: ["x", "y"],
  });
  assert.deepEqual(ops[1], {
    op: "addConstraint",
    table: "expr_checks",
    constraint: {
      name: "score_nonnegative",
      kind: {
        kind: "check",
        expr: {
          node: "binOp",
          op: "ge",
          lhs: { node: "colRef", name: "total_cents" },
          rhs: { node: "literal", value: 0 },
        },
      },
    },
  });
});

test("c.pg builds PG-only extract and interval literal nodes", () => {
  const ops = record(() => {
    domain("billing_period").create({
      as: t.date(),
      check: (c) => c.pg.extract("day", c("VALUE")).eq(1),
    });
    table("oauth_device_codes").create({
      columns: {
        issued_at: t.timestamp().notNull(),
        expires_at: t.timestamp().notNull(),
      },
      checks: [
        {
          name: "expires_window",
          expr: (c) => c("expires_at").le(c("issued_at").add(c.pg.interval("00:01:00"))),
        },
      ],
    });
  });
  assert.equal(ops[0].as, "date");
  assert.deepEqual(ops[0].check, {
    node: "binOp",
    op: "eq",
    lhs: { node: "extract", field: "day", expr: { node: "colRef", name: "VALUE" } },
    rhs: { node: "literal", value: 1 },
  });
  assert.deepEqual(ops[1].constraints[0].kind.expr, {
    node: "binOp",
    op: "le",
    lhs: { node: "colRef", name: "expires_at" },
    rhs: {
      node: "binOp",
      op: "add",
      lhs: { node: "colRef", name: "issued_at" },
      rhs: { node: "pgIntervalLiteral", value: "00:01:00" },
    },
  });
});

test("c.pg rejects malformed text arrays and regex patterns", () => {
  assert.throws(
    () => record(() => table("t").update({ set: { x: (c) => c.pg.eqAnyArray(c("x"), []) } })),
    (e: any) => e.code === "OP_INVALID" && /non-empty string\[\]/.test(e.message),
  );
  assert.throws(
    () =>
      record(() =>
        table("t").update({ set: { x: (c) => c.pg.neAllArray(c("x"), ["ok", 7 as any]) } }),
      ),
    (e: any) => e.code === "OP_INVALID" && /must be a string/.test(e.message),
  );
  assert.throws(
    () => record(() => table("t").update({ set: { x: (c) => c.pg.regex(c("x"), "") } })),
    (e: any) => e.code === "OP_INVALID" && /pattern must be non-empty/.test(e.message),
  );
  assert.throws(
    () => record(() => table("t").update({ set: { x: (c) => c.pg.extract("month" as any, c("x")) } })),
    (e: any) => e.code === "OP_INVALID" && /field must be "day"/.test(e.message),
  );
  assert.throws(
    () => record(() => table("t").update({ set: { x: (c) => c.pg.interval("1 minute") } })),
    (e: any) => e.code === "OP_INVALID" && /HH:MM:SS/.test(e.message),
  );
});

test("index columns normalize to closed column/expression elements", () => {
  const ops = record(() =>
    table("users").index("users_email_lower_idx").add({
      columns: ["email", { kind: "expr", expr: (c) => c.fn.lower(c("email")) }],
      where: (c) => c("active").isTrue(),
    }),
  );
  assert.deepEqual(ops[0].columns, [
    { kind: "column", name: "email" },
    {
      kind: "expr",
      expr: { node: "fnCall", fn: "lower", args: [{ node: "colRef", name: "email" }] },
    },
  ]);
  assert.deepEqual(ops[0].where, {
    node: "unaryOp",
    op: "isTrue",
    operand: { node: "colRef", name: "active" },
  });
});

test("comment records closed COMMENT ON targets through handles and top-level API", () => {
  const ops = record(() => {
    table("users", { schema: "app" }).comment("User accounts");
    table("users").column("email").comment(null);
    table("users").index("users_email_idx").comment("Email lookup", { schema: "idx" });
    comment({ kind: "function", schema: "app", name: "normalize_email" }, "Normalize email");
  });
  assert.deepEqual(ops, [
    {
      op: "comment",
      target: { kind: "table", schema: "app", name: "users" },
      comment: "User accounts",
    },
    {
      // SA-5: a null comment (clear) normalizes to a dropped key — canonical IR
      // maps null→None identically, so the recorder omits the slot.
      op: "comment",
      target: { kind: "column", table: "users", name: "email" },
    },
    {
      op: "comment",
      target: { kind: "index", schema: "idx", name: "users_email_idx" },
      comment: "Email lookup",
    },
    {
      op: "comment",
      target: { kind: "function", schema: "app", name: "normalize_email" },
      comment: "Normalize email",
    },
  ]);
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
