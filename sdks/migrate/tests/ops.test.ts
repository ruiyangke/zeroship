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
  p,
  partition,
  dropPartition,
  minValue,
  maxValue,
  nextval,
  comment,
  lintDeterminism,
  enumType,
  check,
  and,
  or,
  not,
  lit,
  decimal,
  byteValue,
  interval,
  dialect,
} from "../src/index.js";
import { domain, sequence } from "../src/pg.js";
// The build-evaluator recorder seam (not part of the public surface).
import { __begin, __drain } from "../src/ops.js";
// The engine-embedded recorder is now the COMPILED artifact
// (`dist/embedded-recorder.js`) the `zeroship-migrate` crate `include_str!`s —
// the same `tsup` build output of `src/{ops,pg}.ts`. Importing it here (instead
// of the deleted `migrate_ops.js` twin) makes this an artifact-identity oracle:
// the SDK source and the shipped engine artifact record byte-identically.
import {
  __begin as engBegin,
  __drain as engDrain,
  t as engT,
  table as engTable,
  nextval as engNextval,
  decimal as engDecimal,
  byteValue as engByteValue,
} from "../dist/embedded-recorder.js";

/** Record one phase's ops via the ambient recorder. */
function record(up: () => void): any[] {
  __begin();
  up();
  return __drain();
}

function recordEngine(up: (api: { table: any; t: any; nextval: any; decimal: any; byteValue: any }) => void): any[] {
  engBegin();
  up({ table: engTable, t: engT, nextval: engNextval, decimal: engDecimal, byteValue: engByteValue });
  return engDrain();
}

test("@zeroship/migrate core exports enumType and omits pg-only/old names", async () => {
  const imported = await import("@zeroship/migrate");
  assert.equal(typeof imported.enumType, "function");
  assert.equal(typeof imported.check, "function");
  assert.equal((imported as any).membership, undefined);
  assert.equal((imported as any).notMembership, undefined);
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

test("t.text({ caseSensitive:false }) records the caseSensitive facet", () => {
  const ops = record(() => {
    table("u").create({
      columns: {
        plain: t.text(),
        insensitive: t.text({ caseSensitive: false }),
        explicitDefault: t.text({ caseSensitive: true }),
      },
    });
    table("u").column("email").add({ type: t.text({ caseSensitive: false }) });
  });
  const cols = ops[0].columns;
  assert.equal(cols[0].caseSensitive, undefined, "t.text() omits caseSensitive");
  assert.equal(cols[1].caseSensitive, false, "caseSensitive:false records the facet");
  assert.equal(cols[2].caseSensitive, undefined, "caseSensitive:true is byte-identical");
  assert.equal(ops[1].caseSensitive, false, "addColumn carries the text facet too");
});

test("public and engine recorders match for t.text({ caseSensitive:false })", () => {
  const publicOps = record(() => {
    table("u").create({ columns: { email: t.text({ caseSensitive: false }) } });
    table("u").column("nickname").add({ type: t.text({ caseSensitive: false }) });
  });
  const engineOps = recordEngine(({ table, t }) => {
    table("u").create({ columns: { email: t.text({ caseSensitive: false }) } });
    table("u").column("nickname").add({ type: t.text({ caseSensitive: false }) });
  });
  assert.deepEqual(publicOps, engineOps);
  assert.equal(publicOps[0].columns[0].caseSensitive, false);
  assert.equal(publicOps[1].caseSensitive, false);
});

test("t.textArray() records the textArray column type", () => {
  const ops = record(() => {
    table("u").create({ columns: { scopes: t.textArray().notNull() } });
  });
  const col = ops[0].columns[0];
  assert.equal(col.type, "textArray");
  assert.equal(col.nullable, false);
});

test("t.id() records a uuid PK + genRandomUuid default + top-level primaryKey", () => {
  const ops = record(() => {
    table("u").create({ columns: { id: t.id() } });
  });
  const col = ops[0].columns[0];
  assert.equal(col.type, "uuid");
  assert.equal(col.nullable, false);
  assert.deepEqual(col.default, { expr: { node: "fnSynth", fn: "genRandomUuid", args: [] } });
  assert.deepEqual(ops[0].primaryKey, ["id"]);
  assert.equal(ops[0].constraints, undefined);
});

test("default expression callbacks record IrDefault::Expr", () => {
  const ops = record(() => {
    table("u").create({
      columns: {
        created_at: t.timestamp().notNull().default((c) => c.fn.now()),
      },
    });
    table("u").column("updated_at").setDefault((c) => c.fn.now());
  });
  const expr = { node: "fnSynth", fn: "now", args: [] };
  assert.deepEqual(ops[0].columns[0].default, { expr });
  assert.deepEqual(ops[1].value, { expr });
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

test("table runtime option terminals take object args and default enabled true", () => {
  const publicOps = record(() => {
    table("posts").softDelete();
    table("posts").softDelete({ enabled: false, schema: "archive" });
    table("posts").withVersioning();
    table("posts", { schema: "app" }).withVersioning({ enabled: false });
  });
  const engineOps = recordEngine(({ table }) => {
    table("posts").softDelete();
    table("posts").softDelete({ enabled: false, schema: "archive" });
    table("posts").withVersioning();
    table("posts", { schema: "app" }).withVersioning({ enabled: false });
  });

  assert.deepEqual(publicOps, engineOps);
  assert.deepEqual(publicOps, [
    {
      op: "setTableOptions",
      table: "posts",
      options: { softDelete: true },
    },
    {
      op: "setTableOptions",
      table: "posts",
      options: { softDelete: false },
      schema: "archive",
    },
    {
      op: "setTableOptions",
      table: "posts",
      options: { versioning: true },
    },
    {
      op: "setTableOptions",
      table: "posts",
      options: { versioning: false },
      schema: "app",
    },
  ]);
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

test("t.int().autoIncrement() records identity always:false and matches engine recorder", () => {
  const publicOps = record(() => {
    table("u").create({
      columns: {
        id: t.int().autoIncrement().primaryKey(),
        seq: t.bigInt().autoIncrement(),
      },
    });
    table("u").column("next_id").add({ type: t.int().autoIncrement() });
  });
  const engineOps = recordEngine(({ table, t }) => {
    table("u").create({
      columns: {
        id: t.int().autoIncrement().primaryKey(),
        seq: t.bigInt().autoIncrement(),
      },
    });
    table("u").column("next_id").add({ type: t.int().autoIncrement() });
  });

  assert.deepEqual(publicOps, engineOps);
  assert.deepEqual(publicOps[0].columns[0].identity, { always: false });
  assert.deepEqual(publicOps[0].columns[1].identity, { always: false });
  assert.deepEqual(publicOps[1].identity, { always: false });
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

test(".default(nextval(name,{schema})) emits IrDefault::Nextval", () => {
  const createOps = record(() => {
    table("audit_events").create({
      columns: {
        id: t.bigInt().notNull().default(nextval("audit_events_id_seq", { schema: "zeroship" })),
      },
    });
  });
  assert.deepEqual(createOps[0].columns[0].default, {
    nextval: { name: "audit_events_id_seq", schema: "zeroship" },
  });

  const addOps = record(() => {
    table("audit_events").column("id").add({
      type: t.bigInt().default(nextval("audit_events_id_seq")),
    });
  });
  assert.deepEqual(addOps[0].default, {
    nextval: { name: "audit_events_id_seq" },
  });
});

test("public and engine recorders match for nextval defaults", () => {
  const publicOps = record(() => {
    table("audit_events").create({
      columns: {
        id: t.bigInt().notNull().default(nextval("audit_events_id_seq", { schema: "zeroship" })),
      },
    });
    table("audit_events").column("id").add({
      type: t.bigInt().default(nextval("audit_events_id_seq")),
    });
  });
  const engineOps = recordEngine(({ table, t, nextval }) => {
    table("audit_events").create({
      columns: {
        id: t.bigInt().notNull().default(nextval("audit_events_id_seq", { schema: "zeroship" })),
      },
    });
    table("audit_events").column("id").add({
      type: t.bigInt().default(nextval("audit_events_id_seq")),
    });
  });
  assert.deepEqual(publicOps, engineOps);
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

test("C2 — primary key is create-time only: .column().add({ type: t.uuid().primaryKey() }) records NO pk follow-on", () => {
  // The always-refused user PRIMARY KEY constraint shape is deleted; `.primaryKey()`
  // on an added column records only the addColumn (no addConstraint(pk)). PKs are
  // authored at create time via `create({ primaryKey })` / a create() column facet.
  const ops = record(() => table("u").column("id").add({ type: t.uuid().primaryKey() }));
  assert.equal(ops.length, 1, "an addColumn only — no pk follow-on");
  assert.equal(ops[0].op, "addColumn");
  assert.ok(
    !ops.some((o) => o.op === "addConstraint"),
    "no addConstraint(pk) is recorded for an added column",
  );
});

test("C2 — .column().add({ type: t.text().unique().primaryKey() }) records the unique follow-on (no pk shape)", () => {
  // With the pk constraint shape gone, the `.unique()` follow-on is unconditional:
  // the added column emits addColumn + addConstraint(unique).
  const ops = record(() => table("u").column("id").add({ type: t.text().unique().primaryKey() }));
  assert.equal(ops.length, 2, "an addColumn + the unique add");
  assert.equal(ops[0].op, "addColumn");
  assert.equal(ops[1].op, "addConstraint");
  assert.deepEqual(ops[1].constraint, { kind: { kind: "unique", columns: ["id"] } });
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

test(".foreignKey().add() records composite/non-id references without serializing reference schema", () => {
  const ops = record(() =>
    table("billing_line_provider_refs", { schema: "zeroship" }).foreignKey("billing_line_provider_refs_line_fk").add({
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

test("foreignKey().add deferrable flags emit, omit when unset, and match engine recorder", () => {
  const publicOps = record(() => {
    table("orders").foreignKey("orders_customer_fk").add({
      columns: ["customer_id"],
      references: { table: "customers", columns: ["id"] },
      deferrable: true,
      initiallyDeferred: true,
    });
    table("orders").create({
      columns: { owner_id: t.text() },
      foreignKeys: [
        {
          name: "orders_owner_fk",
          columns: ["owner_id"],
          references: { table: "owners", columns: ["id"] },
          deferrable: true,
        },
      ],
    });
    table("orders").foreignKey("orders_plain_fk").add({
      columns: ["plain_id"],
      references: { table: "plain", columns: ["id"] },
    });
  });
  const engineOps = recordEngine(({ table, t }) => {
    table("orders").foreignKey("orders_customer_fk").add({
      columns: ["customer_id"],
      references: { table: "customers", columns: ["id"] },
      deferrable: true,
      initiallyDeferred: true,
    });
    table("orders").create({
      columns: { owner_id: t.text() },
      foreignKeys: [
        {
          name: "orders_owner_fk",
          columns: ["owner_id"],
          references: { table: "owners", columns: ["id"] },
          deferrable: true,
        },
      ],
    });
    table("orders").foreignKey("orders_plain_fk").add({
      columns: ["plain_id"],
      references: { table: "plain", columns: ["id"] },
    });
  });

  assert.deepEqual(publicOps, engineOps);
  assert.equal(publicOps[0].constraint.kind.deferrable, true);
  assert.equal(publicOps[0].constraint.kind.initiallyDeferred, true);
  assert.equal(publicOps[1].constraints[0].kind.deferrable, true);
  assert.equal(publicOps[1].constraints[0].kind.initiallyDeferred, undefined);
  assert.ok(!("deferrable" in publicOps[2].constraint.kind));
  assert.ok(!("initiallyDeferred" in publicOps[2].constraint.kind));
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

test("insert normalizes decimal() to {decimal} and Uint8Array to {bytes:base64}", () => {
  const ops = record(() =>
    table("t").insert({ rows: [{ big: decimal("9007199254740993"), raw: new Uint8Array([1, 2, 3]) }] }),
  );
  assert.deepEqual(ops[0].rows, [[{ decimal: "9007199254740993" }, { bytes: "AQID" }]]);
  assert.doesNotThrow(() => JSON.stringify(ops[0]));
});

test("decimal() validates decimal strings and records byte-identical IR", () => {
  const ops = record(() => {
    table("t").insert({ rows: [{ price: decimal("0.00") }] });
    table("t").create({ columns: { price: t.numeric(12, 2).default(decimal("-10.50")) } });
    table("t").insert({
      rows: [{ id: 1 }],
      onConflict: { columns: ["id"], doUpdate: { price: decimal("9007199254740993") } as any },
    });
    table("t").check("price_chk").add({ expr: (c) => c("price").ge(decimal("0.00")) });
    lit(decimal("1.25"));
  });

  assert.deepEqual(ops[0].rows, [[{ decimal: "0.00" }]]);
  assert.deepEqual(ops[1].columns[0].default, { literal: { value: { decimal: "-10.50" } } });
  assert.deepEqual(ops[2].onConflict.doUpdate, { price: { decimal: "9007199254740993" } });
  assert.deepEqual(ops[3].constraint.kind.expr.rhs.value, { decimal: "0.00" });

  assert.throws(
    () => decimal("1."),
    (e: any) => e.code === "OP_INVALID" && /well-formed decimal string/.test(e.message) && /decimal\("<n>"\)/.test(e.message),
  );
  assert.throws(
    () => decimal("1e3"),
    (e: any) => e.code === "OP_INVALID" && /well-formed decimal string/.test(e.message) && /decimal\("<n>"\)/.test(e.message),
  );
});

test("byteValue() validates bytes inputs and records byte-identical IR", () => {
  const ops = record(() => {
    table("t").insert({
      rows: [
        {
          raw: new Uint8Array([1, 2, 3]),
          fromBytes: byteValue(new Uint8Array([1, 2, 3])),
          fromString: byteValue("AQID"),
        },
      ],
    });
    table("t").create({ columns: { raw: t.bytes().default(byteValue("AQID")) } });
    table("t").insert({
      rows: [{ id: 1 }],
      onConflict: { columns: ["id"], doUpdate: { raw: byteValue(new Uint8Array([1, 2, 3])) } as any },
    });
    table("t").check("raw_chk").add({ expr: (c) => c("raw").eq(byteValue("AQID")) });
    lit(byteValue("AQID"));
  });

  assert.deepEqual(ops[0].rows, [[{ bytes: "AQID" }, { bytes: "AQID" }, { bytes: "AQID" }]]);
  assert.deepEqual(ops[1].columns[0].default, { literal: { value: { bytes: "AQID" } } });
  assert.deepEqual(ops[2].onConflict.doUpdate, { raw: { bytes: "AQID" } });
  assert.deepEqual(ops[3].constraint.kind.expr.rhs.value, { bytes: "AQID" });

  assert.throws(
    () => byteValue("not base64?"),
    (e: any) => e.code === "OP_INVALID" && /well-formed base64 string/.test(e.message) && /byteValue/.test(e.message),
  );
});

test("public and engine recorders match for decimal() scalar values", () => {
  const pub = record(() => {
    table("t").insert({ rows: [{ price: decimal("0.00") }] });
    table("t").create({ columns: { price: t.numeric(12, 2).default(decimal("0.00")) } });
  });
  const eng = recordEngine(({ table, t, decimal }) => {
    table("t").insert({ rows: [{ price: decimal("0.00") }] });
    table("t").create({ columns: { price: t.numeric(12, 2).default(decimal("0.00")) } });
  });
  assert.deepEqual(pub, eng);
});

test("public and engine recorders match for byteValue() scalar values", () => {
  const pub = record(() => {
    table("t").insert({ rows: [{ raw: byteValue("AQID") }] });
    table("t").create({ columns: { raw: t.bytes().default(byteValue(new Uint8Array([1, 2, 3]))) } });
  });
  const eng = recordEngine(({ table, t, byteValue }) => {
    table("t").insert({ rows: [{ raw: byteValue("AQID") }] });
    table("t").create({ columns: { raw: t.bytes().default(byteValue(new Uint8Array([1, 2, 3]))) } });
  });
  assert.deepEqual(pub, eng);
});

test("bigint and the removed {decimal} carrier fail closed at record time", () => {
  const isBigintRefusal = (e: any) =>
    e.code === "OP_INVALID" && e.message.includes('bigint is not a value — use decimal("<n>")');
  const isCarrierRefusal = (e: any) =>
    e.code === "OP_INVALID" && e.message.includes('the { decimal } carrier is removed — use decimal("<n>")');

  assert.throws(
    () => record(() => table("t").insert({ rows: [{ big: 9007199254740993n }] } as any)),
    isBigintRefusal,
  );
  assert.throws(
    () => record(() => table("t").create({ columns: { big: t.numeric(38, 0).default(9007199254740993n as any) } })),
    isBigintRefusal,
  );
  assert.throws(
    () =>
      record(() =>
        table("t").insert({
          rows: [{ id: 1 }],
          onConflict: { columns: ["id"], doUpdate: { big: 9007199254740993n } as any },
        }),
      ),
    isBigintRefusal,
  );
  assert.throws(
    () => record(() => table("t").insert({ rows: [{ price: { decimal: "0.00" } }] } as any)),
    isCarrierRefusal,
  );
  assert.throws(
    () => record(() => table("t").create({ columns: { price: t.numeric(12, 2).default({ decimal: "0.00" } as any) } })),
    isCarrierRefusal,
  );
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
    () => record(() => table("t").create({ columns: { v: t.text().default({ fn: "now" } as any) } })),
    (e: any) => e.code === "OP_INVALID" && /old `\{ fn: \.\.\. \}`/.test(e.message),
  );
  assert.throws(
    () => record(() => table("t").create({ columns: { v: t.timestamp().default(Date.now as any) } })),
    (e: any) => e.code === "OP_INVALID" && /bare native-symbol default forms are removed/.test(e.message),
  );
  assert.throws(
    () =>
      record(() =>
        table("t").create({
          columns: { v: t.text().default((() => ({ node: "colRef", column: "name" })) as any) },
        }),
      ),
    (e: any) => e.code === "OP_INVALID" && /column default cannot reference a column/.test(e.message),
  );
  assert.throws(
    () =>
      record(() =>
        table("t").create({ columns: { v: t.int().default((() => ({ node: "agg", func: "count" })) as any) } }),
      ),
    (e: any) => e.code === "OP_INVALID" && /column default cannot use an aggregate/.test(e.message),
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

test("a column default carries decimal()/Uint8Array through the same IrScalar carrier", () => {
  const ops = record(() =>
    table("t").create({
      columns: {
        big: t.numeric(38, 0).default(decimal("9007199254740993")),
        raw: t.bytes().default(new Uint8Array([255, 0])),
      },
    }),
  );
  const cols = ops[0].columns;
  assert.deepEqual(cols[0].default, { literal: { value: { decimal: "9007199254740993" } } });
  assert.deepEqual(cols[1].default, { literal: { value: { bytes: "/wA=" } } });
});

test("empty object and array defaults record as container defaults", () => {
  const ops = record(() =>
    table("t").create({
      columns: {
        settings: t.json().default({}),
        events: t.json().default([]),
        scopes: t.textArray().default([]),
      },
    }),
  );
  const cols = ops[0].columns;
  assert.deepEqual(cols[0].default, { container: "object" });
  assert.deepEqual(cols[1].default, { container: "array" });
  assert.deepEqual(cols[2].default, { container: "array" });
});

test("non-empty JSON defaults record as IrDefault::Json with sorted keys", () => {
  const ab = record(() =>
    table("t").create({
      columns: {
        v: t.json().default({ a: 1, b: 2, nested: { a: 2, z: 1 } }),
      },
    }),
  );
  const ba = record(() =>
    table("t").create({
      columns: {
        v: t.json().default({ nested: { z: 1, a: 2 }, b: 2, a: 1 }),
      },
    }),
  );
  const expected = { json: { a: 1, b: 2, nested: { a: 2, z: 1 } } };
  assert.deepEqual(ab[0].columns[0].default, expected);
  assert.deepEqual(ba[0].columns[0].default, expected);
  assert.equal(
    JSON.stringify(ab[0].columns[0].default),
    JSON.stringify(ba[0].columns[0].default),
    "object-key insertion order must be canonicalized for checksum stability",
  );
});

test("JSON default float values are rejected", () => {
  const message = "json default values support integers only (floats not yet supported)";
  assert.throws(
    () => record(() => table("t").create({ columns: { v: t.json().default({ x: 1.5 }) } })),
    (e: any) => e.code === "OP_INVALID" && e.message.includes(message),
  );
});

test("public and engine recorders match for JSON value defaults", () => {
  const pub = record(() =>
    table("t").create({
      columns: {
        v: t.json().default({ b: 2, a: 1, nested: { z: 1, a: 2 } }),
        arr: t.json().default([1, { b: 2, a: 1 }]),
      },
    }),
  );
  const eng = recordEngine(({ table, t }) =>
    table("t").create({
      columns: {
        v: t.json().default({ b: 2, a: 1, nested: { z: 1, a: 2 } }),
        arr: t.json().default([1, { b: 2, a: 1 }]),
      },
    }),
  );
  assert.deepEqual(pub, eng);
  assert.deepEqual(pub[0].columns[0].default, {
    json: { a: 1, b: 2, nested: { a: 2, z: 1 } },
  });
  assert.deepEqual(pub[0].columns[1].default, {
    json: [1, { a: 1, b: 2 }],
  });
});

test("empty container defaults record byte-identically to engine recorder", () => {
  const pub = record(() =>
    table("t").create({
      columns: {
        settings: t.json().default({}),
        events: t.json().default([]),
        scopes: t.textArray().default([]),
      },
    }),
  );
  const eng = recordEngine(({ table, t }) =>
    table("t").create({
      columns: {
        settings: t.json().default({}),
        events: t.json().default([]),
        scopes: t.textArray().default([]),
      },
    }),
  );
  assert.deepEqual(pub, eng);
});

test("onConflict.doUpdate normalizes decimal()/Uint8Array scalar assignments", () => {
  const ops = record(() =>
    table("t").insert({
      rows: [{ id: 1 }],
      onConflict: { columns: ["id"], doUpdate: { big: decimal("9007199254740993"), raw: new Uint8Array([7]) } as any },
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
  const ops = record(() => table("t").delete({ where: (c) => c("code").isNull(), limit: 5 }));
  assert.equal(ops[0].op, "delete");
  assert.equal(ops[0].limit, 5);
  assert.throws(() => record(() => table("t").delete({} as any)), /where is mandatory/);
});

test("the (c) => Expr builder constructs the closed AST", () => {
  const ops = record(() =>
    table("t").update({
      set: {
        a: (c) => c("x").add(1).mul(2).cast("integer"),
        b: (c) => c.fn.concatWs(" ", c("p"), c("q")),
        d: (c) => c.case({ branches: [{ when: c("x").lt(0), then: c("y") }], else: c("z") }),
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

test("c.case validates the object branch shape", () => {
  assert.throws(
    () =>
      record(() =>
        table("t").update({
          set: { x: (c) => c.case({ branches: [] }) },
          where: (c) => c("id").isNotNull(),
        }),
      ),
    /c\.case\(\{ branches: \[\{ when, then \}\], else\? \}\): branches must be a non-empty array/,
  );
  assert.throws(
    () =>
      record(() =>
        table("t").update({
          set: { x: (c) => c.case({ branches: [[c("a"), c("b")]] as any }) },
          where: (c) => c("id").isNotNull(),
        }),
      ),
    /c\.case\(\{ branches: \[\{ when, then \}\], else\? \}\): branches\[0\] must be an object with when and then/,
  );
});

test("eq(null)/ne(null) are record-time errors steering to isNull()/isNotNull() (P4)", () => {
  assert.throws(
    () => record(() => table("t").check("c_eq").add({ expr: (c) => c("a").eq(null) })),
    /eq\(null\) is always UNKNOWN in SQL — use isNull\(\)/,
  );
  assert.throws(
    () => record(() => table("t").check("c_ne").add({ expr: (c) => c("a").ne(null) })),
    /ne\(null\) is always UNKNOWN in SQL — use isNotNull\(\)/,
  );
  // the steer target itself still records without error
  const ops = record(() => table("t").check("c_ok").add({ expr: (c) => c("a").isNull() }));
  assert.equal(ops.length, 1);
  assert.equal(JSON.stringify(ops[0]).includes('"isNull"'), true);
});

test("the two-arg c('table','col') records a qualified colRef; one-arg stays unqualified", () => {
  // §3.4 the join-ON fix: `c("orders", "customer_id")` records a colRef carrying
  // an optional `table`; `c("id")` records the pre-qualification unqualified shape
  // (no `table` key at all — byte-identical to today).
  const ops = record(() =>
    table("t").update({
      set: {
        // qualified two-arg form on both sides of the predicate-shaped value
        q: (c) => c("orders", "customer_id"),
        // one-arg form is untouched
        u: (c) => c("id"),
        // c.col two-arg mirror
        cq: (c) => c.col("users", "id"),
      },
    }),
  );
  const set = ops[0].set;
  assert.deepEqual(set.q, { node: "colRef", table: "orders", name: "customer_id" });
  assert.deepEqual(set.cq, { node: "colRef", table: "users", name: "id" });
  // Unqualified: no `table` property is emitted (compact wire shape).
  assert.deepEqual(set.u, { node: "colRef", name: "id" });
  assert.equal("table" in set.u, false);
});

test("c.pg builds PG-only regex and pg_column_size nodes", () => {
  const ops = record(() =>
    table("t").create({
      columns: {
        status: t.text().notNull(),
        name: t.text().notNull(),
        data: t.json().notNull(),
      },
      checks: [
        { name: "name_shape", expr: (c) => c.pg.regex(c("name"), "^[a-z]+$") },
        { name: "data_size", expr: (c) => c.pg.columnSize(c("data")).le(8192) },
      ],
    }),
  );
  const checks = ops[0].constraints.map((c: any) => c.kind.expr);
  assert.deepEqual(checks[0], {
    node: "pgRegexMatch",
    expr: { node: "colRef", name: "name" },
    pattern: "^[a-z]+$",
  });
  assert.deepEqual(checks[1], {
    node: "binOp",
    op: "le",
    lhs: { node: "pgColumnSize", expr: { node: "colRef", name: "data" } },
    rhs: { node: "literal", value: 8192 },
  });
});

test("portable between/like/in/notIn/distinctFrom chain builders record the right nodes", () => {
  // §3.4 portable predicate nodes. `between`/`like` render identical syntax on
  // all three dialects; `in`/`notIn` are portably named while preserving PG's
  // ANY/ALL render; `distinctFrom` is portably named but per-dialect rendered
  // (PG/SQLite `IS DISTINCT FROM` vs MySQL `NOT (x <=> y)`) — the engine owns it.
  const ops = record(() =>
    table("t").update({
      set: {
        b: (c) => c("age").between(18, 65),
        l: (c) => c("name").like("A%"),
        i: (c) => c("status").in(["a", "b"]),
        ni: (c) => c("status").notIn(["x"]),
        empty: (c) => c("status").in([]),
        d: (c) => c("a").distinctFrom(c("b")),
      },
    }),
  );
  const set = ops[0].set;
  assert.deepEqual(set.b, {
    node: "between",
    operand: { node: "colRef", name: "age" },
    low: { node: "literal", value: 18 },
    high: { node: "literal", value: 65 },
  });
  assert.deepEqual(set.l, {
    node: "like",
    operand: { node: "colRef", name: "name" },
    pattern: { node: "literal", value: "A%" },
  });
  assert.deepEqual(set.i, {
    node: "inList",
    expr: { node: "colRef", name: "status" },
    elems: ["a", "b"],
    negated: false,
  });
  assert.deepEqual(set.ni, {
    node: "inList",
    expr: { node: "colRef", name: "status" },
    elems: ["x"],
    negated: true,
  });
  assert.deepEqual(set.empty, {
    node: "inList",
    expr: { node: "colRef", name: "status" },
    elems: [],
    negated: false,
  });
  assert.deepEqual(set.d, {
    node: "distinctFrom",
    left: { node: "colRef", name: "a" },
    right: { node: "colRef", name: "b" },
  });
});

test("dialect() records the Layer-2 per-dialect value escape in canonical leg order", () => {
  // §3.4 the one Layer-2 escape. Each leg is a full expression; the legs record
  // in full in the `dialect` node in canonical order (default, pg, sqlite, mysql).
  const ops = record(() =>
    table("t").update({
      set: {
        // all three explicit legs, no default
        u: () => dialect({ pg: lit("A"), sqlite: lit("B"), mysql: lit("C") }),
        // default + one explicit leg
        d: () => dialect({ default: lit(0), pg: lit(1) }),
      },
    }),
  );
  const set = ops[0].set;
  assert.deepEqual(set.u, {
    node: "dialect",
    pg: { node: "literal", value: "A" },
    sqlite: { node: "literal", value: "B" },
    mysql: { node: "literal", value: "C" },
  });
  // Canonical leg order: default serializes first.
  assert.deepEqual(Object.keys(set.d), ["node", "default", "pg"]);
  assert.deepEqual(set.d, {
    node: "dialect",
    default: { node: "literal", value: 0 },
    pg: { node: "literal", value: 1 },
  });
});

test("dialect() rejects an empty leg set at record time", () => {
  assert.throws(() => record(() => table("t").update({ set: { x: () => dialect({}) } })), /at least one leg/);
});

test("c.agg builders record the portable aggregate node (count(*)/sum/distinct)", () => {
  // §3.4/§3.6 portable aggregate nodes. count()/sum/avg/min/max render identically
  // on all three dialects; count() (no arg) is COUNT(*); { distinct: true } sets the
  // flag. `distinct` is skipped on the wire when false (byte-minimal).
  const ops = record(() =>
    table("t").update({
      set: {
        n: (c) => c.agg.count(),
        s: (c) => c.agg.sum(c("x")),
        d: (c) => c.agg.count(c("x"), { distinct: true }),
        a: (c) => c.agg.avg(c("x")),
      },
    }),
  );
  const set = ops[0].set;
  // count(*) — no arg, no distinct key (skip-if-false).
  assert.deepEqual(set.n, { node: "agg", func: "count" });
  assert.deepEqual(set.s, {
    node: "agg",
    func: "sum",
    arg: { node: "colRef", name: "x" },
  });
  assert.deepEqual(set.d, {
    node: "agg",
    func: "count",
    arg: { node: "colRef", name: "x" },
    distinct: true,
  });
  assert.deepEqual(set.a, {
    node: "agg",
    func: "avg",
    arg: { node: "colRef", name: "x" },
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
        subtotal_cents: t.int().notNull(),
        credit_cents: t.int().notNull(),
        total_cents: t.int().notNull(),
        floor_cents: t.int(),
        created_at: t.timestamp().notNull(),
        expires_at: t.timestamp().notNull(),
        enabled: t.boolean().notNull(),
        visible: t.boolean().notNull(),
      },
      checks: [
        check("pkce_method_check", (c) => c("pkce_method").eq("S256")),
        check("user_id_fmt", (c) => c("user_id").matches("^usr_[0-9A-Za-z]{20,40}$")),
        check("kind_ok", (c) => c("kind").in(["a", "b", "c"])),
        check("data_size", (c) => c("data").columnSize().lt(262144)),
        check("total_matches", (c) => c("total_cents").eq(c("subtotal_cents").sub(c("credit_cents")))),
        check("floor_nonneg_or_null", (c) => or(c("floor_cents").isNull(), c("floor_cents").ge(0))),
        check("enabled_and_visible", (c) => and(c("enabled"), c("visible"))),
        check("expires_window", (c) => c("expires_at").le(c("created_at").add(interval("00:01:00")))),
        check("not_archived", (c) => not(c("kind").eq(lit("archived")))),
        check("kind_not_reserved", (c) => c("kind").notIn(["x", "y"])),
      ],
    });
    table("expr_checks").check("score_nonnegative").add({ expr: (c) => c("total_cents").ge(0) });
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
    node: "inList",
    expr: { node: "colRef", name: "kind" },
    elems: ["a", "b", "c"],
    negated: false,
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
      rhs: { node: "pgInterval", duration: "00:01:00" },
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
    node: "inList",
    expr: { node: "colRef", name: "kind" },
    elems: ["x", "y"],
    negated: true,
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
    lhs: { node: "extract", field: "day", from: { node: "colRef", name: "VALUE" } },
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
      rhs: { node: "pgInterval", duration: "00:01:00" },
    },
  });
});

test("inList rejects malformed text arrays and c.pg rejects regex patterns", () => {
  assert.throws(
    () => record(() => table("t").update({ set: { x: (c) => c("x").in(["ok", 7 as any]) } })),
    (e: any) => e.code === "OP_INVALID" && /must be a string/.test(e.message),
  );
  assert.throws(
    () => record(() => table("t").update({ set: { x: (c) => c("x").notIn([""]) } })),
    (e: any) => e.code === "OP_INVALID" && /must be non-empty/.test(e.message),
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
      on: ["email", { expr: (c) => c.fn.lower(c("email")) }],
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

test("immutable-only slots reject forced volatile, aggregate, and vendor nodes at record time", () => {
  assert.throws(
    () =>
      record(() =>
        table("users").create({
          columns: {
            created_day: t.timestamp().generated({ node: "fnSynth", fn: "now", args: [] } as any),
          },
        }),
      ),
    (e: any) => e.code === "OP_INVALID" && /generated column expression/.test(e.message) && /now is volatile/.test(e.message),
  );

  assert.throws(
    () =>
      record(() =>
        table("users").index("users_bad_agg_idx").add({
          on: [{ expr: { node: "agg", func: "count" } as any }],
        }),
      ),
    (e: any) => e.code === "OP_INVALID" && /index expression element/.test(e.message) && /aggregates/.test(e.message),
  );

  assert.throws(
    () =>
      record(() =>
        table("users").create({
          columns: { email: t.text() },
          indexes: [{
            name: "users_bad_partial_idx",
            on: ["email"],
            where: { node: "fnSynth", fn: "genRandomUuid", args: [] } as any,
          }],
        }),
      ),
    (e: any) => e.code === "OP_INVALID" && /partial index predicate/.test(e.message) && /genRandomUuid is volatile/.test(e.message),
  );

  assert.throws(
    () =>
      record(() =>
        table("bookings").exclusion("bookings_bad_excl").add({
          using: "gist",
          elements: [{ target: "room", operator: "=" }],
          where: { node: "fnCall", fn: "currentUser", args: [] } as any,
        }),
      ),
    (e: any) => e.code === "OP_INVALID" && /exclusion predicate/.test(e.message) && /currentUser/.test(e.message),
  );
});

test("index column order records DESC and omits ASC/default order", () => {
  const ops = record(() =>
    table("events").index("events_created_desc_idx").add({
      on: [
        { column: "tenant_id", order: "asc" },
        { column: "created_at", order: "desc" },
      ],
    }),
  );
  assert.deepEqual(ops[0].columns, [
    { kind: "column", name: "tenant_id" },
    { kind: "column", name: "created_at", order: "desc" },
  ]);
  assert.throws(
    () =>
      record(() =>
        table("events").index("events_bad_order_idx").add({
          on: [{ column: "created_at", order: "latest" as any }],
        }),
      ),
    (e: any) => e.code === "OP_INVALID" && /order must be "asc" or "desc"/.test(e.message),
  );
});

test("index records PG-vendor nullsNotDistinct + per-element opclass/collation", () => {
  const ops = record(() =>
    table("accounts").index("accounts_email_uq").add({
      on: [{ column: "email", opclass: "text_pattern_ops", collation: "C" }],
      unique: true,
      nullsNotDistinct: true,
    }),
  );
  assert.equal(ops[0].nullsNotDistinct, true);
  assert.deepEqual(ops[0].columns, [
    { kind: "column", name: "email", opclass: "text_pattern_ops", collation: "C" },
  ]);
});

test("index omits nullsNotDistinct/opclass/collation when absent (byte-neutral)", () => {
  const ops = record(() =>
    table("accounts").index("accounts_email_idx").add({ on: ["email"] }),
  );
  assert.equal("nullsNotDistinct" in ops[0], false);
  assert.deepEqual(ops[0].columns, [{ kind: "column", name: "email" }]);
});

test("createTable inline index carries nullsNotDistinct + element facets", () => {
  const ops = record(() =>
    table("accounts").create({
      columns: { email: t.text() },
      indexes: [
        {
          name: "accounts_email_uq",
          on: [{ column: "email", opclass: "text_pattern_ops" }],
          unique: true,
          nullsNotDistinct: true,
        },
      ],
    }),
  );
  assert.equal(ops[0].indexes[0].nullsNotDistinct, true);
  assert.deepEqual(ops[0].indexes[0].columns, [
    { kind: "column", name: "email", opclass: "text_pattern_ops" },
  ]);
});

test("partitionBy records range/list/hash specs on createTable", () => {
  const ops = record(() => {
    table("events_range").create({
      columns: { ts: t.timestamp() },
      partitionBy: p.range(["ts"]),
    });
    table("events_list").create({
      columns: { region: t.text() },
      partitionBy: p.list(["region"]),
    });
    table("events_hash").create({
      columns: { tenant_id: t.text() },
      partitionBy: p.hash(["tenant_id"]),
    });
  });

  assert.deepEqual(ops, [
    {
      op: "createTable",
      name: "events_range",
      columns: [{ name: "ts", type: "timestamp" }],
      partitionBy: { kind: "range", columns: ["ts"] },
    },
    {
      op: "createTable",
      name: "events_list",
      columns: [{ name: "region", type: "text" }],
      partitionBy: { kind: "list", columns: ["region"] },
    },
    {
      op: "createTable",
      name: "events_hash",
      columns: [{ name: "tenant_id", type: "text" }],
      partitionBy: { kind: "hash", columns: ["tenant_id"] },
    },
  ]);
});

test("partition() records range and default createPartition ops", () => {
  const ops = record(() => {
    partition("events_2026_05", { schema: "app" }).of("events").forValues({
      from: [minValue, "2026-05-01T00:00:00Z", 1],
      to: ["2026-06-01T00:00:00Z", maxValue, 31],
    }, { ifNotExists: true });
    partition("events_default").of("events").asDefault();
  });

  assert.deepEqual(ops, [
    {
      op: "createPartition",
      name: "events_2026_05",
      of: "events",
      bounds: {
        kind: "range",
        from: [
          { kind: "minValue" },
          { kind: "string", value: "2026-05-01T00:00:00Z" },
          { kind: "int", value: 1 },
        ],
        to: [
          { kind: "string", value: "2026-06-01T00:00:00Z" },
          { kind: "maxValue" },
          { kind: "int", value: 31 },
        ],
      },
      schema: "app",
      existenceGuard: "ifNotExists",
    },
    {
      op: "createPartition",
      name: "events_default",
      of: "events",
      bounds: { kind: "default" },
    },
  ]);
});

test("partition() records list and hash createPartition ops", () => {
  const ops = record(() => {
    partition("orders_us").of("orders").forValues({ in: ["US", 840] });
    partition("orders_h1").of("orders").forValues({ modulus: 4, remainder: 1 });
  });

  assert.deepEqual(ops, [
    {
      op: "createPartition",
      name: "orders_us",
      of: "orders",
      bounds: {
        kind: "list",
        values: [
          { kind: "string", value: "US" },
          { kind: "int", value: 840 },
        ],
      },
    },
    {
      op: "createPartition",
      name: "orders_h1",
      of: "orders",
      bounds: { kind: "hash", modulus: 4, remainder: 1 },
    },
  ]);
});

test("table().detachPartition records parent-subject detachPartition", () => {
  const ops = record(() =>
    table("events", { schema: "app" }).detachPartition("events_2026_05", {
      concurrently: true,
    }),
  );

  assert.deepEqual(ops, [
    {
      op: "detachPartition",
      parent: "events",
      name: "events_2026_05",
      schema: "app",
      concurrently: true,
    },
  ]);
});

test("dropPartition records child-subject dropPartition", () => {
  const ops = record(() =>
    dropPartition("events_2026_05", { schema: "app", ifExists: true, cascade: true }),
  );

  assert.deepEqual(ops, [
    {
      op: "dropPartition",
      name: "events_2026_05",
      schema: "app",
      existenceGuard: "ifExists",
      cascade: true,
    },
  ]);
});

test("index builder records include/with/brin/only", () => {
  const ops = record(() =>
    table("events").index("events_ts_brin_idx").add({
      on: ["ts"],
      using: "brin",
      include: ["tenant_id"],
      with: { pagesPerRange: 32 },
      only: true,
    }),
  );

  assert.deepEqual(ops, [
    {
      op: "createIndex",
      table: "events",
      columns: [{ kind: "column", name: "ts" }],
      name: "events_ts_brin_idx",
      using: "brin",
      include: ["tenant_id"],
      with: { pagesPerRange: 32 },
      only: true,
    },
  ]);
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

test("c.fn.{mod,round,floor,ceil,substr,replace} record the right portable fnCall node", () => {
  const ops = record(() =>
    table("t").update({
      set: {
        m: (c) => c.fn.mod(c("n"), 3),
        r1: (c) => c.fn.round(c("x")),
        r2: (c) => c.fn.round(c("x"), 2),
        fl: (c) => c.fn.floor(c("x")),
        ce: (c) => c.fn.ceil(c("x")),
        s2: (c) => c.fn.substr(c("s"), 1),
        s3: (c) => c.fn.substr(c("s"), 1, 3),
        rp: (c) => c.fn.replace(c("s"), "a", "b"),
      },
    }),
  );
  const set = ops[0].set;
  // Every one is a portable `fnCall` node (NOT `fnSynth`).
  assert.deepEqual(set.m, {
    node: "fnCall",
    fn: "mod",
    args: [{ node: "colRef", name: "n" }, { node: "literal", value: 3 }],
  });
  assert.equal(set.r1.node, "fnCall");
  assert.equal(set.r1.fn, "round");
  assert.equal(set.r1.args.length, 1);
  // Optional precision arg is recorded only when supplied.
  assert.equal(set.r2.args.length, 2);
  assert.deepEqual(set.r2.args[1], { node: "literal", value: 2 });
  assert.equal(set.fl.fn, "floor");
  assert.equal(set.ce.fn, "ceil");
  assert.equal(set.s2.fn, "substr");
  assert.equal(set.s2.args.length, 2);
  assert.equal(set.s3.args.length, 3);
  assert.deepEqual(set.rp, {
    node: "fnCall",
    fn: "replace",
    args: [
      { node: "colRef", name: "s" },
      { node: "literal", value: "a" },
      { node: "literal", value: "b" },
    ],
  });
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
