// The migration path accepts the db SDK's structured types. `t.object` lowers
// to a JSON column, `t.literal` to its primitive plus an equality CHECK, and
// `t.union` flat-expands into one nullable column per variant-wide field plus
// the discriminator IN-list and per-variant NOT NULL CHECKs.

import assert from "node:assert/strict";
import { test } from "node:test";

import { colTypeFromDbField, dbType as dbT, fromDb, t, table } from "../src/index.js";
import { __begin, __drain } from "../src/ops.js";

function createTableOps() {
  return __drain()[0] as {
    columns: Array<Record<string, unknown>>;
    constraints?: Array<Record<string, unknown>>;
  };
}

test("t.object lowers to a JSON column", () => {
  assert.equal(colTypeFromDbField(dbT.object({ bio: dbT.string() })), "json");
  __begin();
  table("profiles").create({
    columns: { profile: fromDb(dbT.object({ bio: dbT.string(), age: dbT.double() })) },
  });
  const op = createTableOps();
  assert.equal(op.columns.length, 1);
  assert.equal(op.columns[0].name, "profile");
  assert.equal(op.columns[0].type, "json");
});

test("t.literal lowers to its primitive plus an equality CHECK", () => {
  assert.equal(colTypeFromDbField(dbT.literal("login")), "text");
  assert.equal(colTypeFromDbField(dbT.literal(7)), "double");
  assert.equal(colTypeFromDbField(dbT.literal(true)), "boolean");
  __begin();
  table("events").create({ columns: { kind: fromDb(dbT.literal("login")) } });
  const op = createTableOps();
  assert.equal(op.columns[0].type, "text");
  assert.equal(op.constraints?.length, 1);
  const check = op.constraints![0];
  assert.equal(check.name, "events_kind_lit_chk");
  assert.deepEqual(check.kind, {
    kind: "check",
    expr: {
      node: "binOp",
      op: "eq",
      lhs: { node: "colRef", name: "kind" },
      rhs: { node: "literal", value: "login" },
    },
  });
});

test("t.union flat-expands into nullable columns, an IN-list and per-variant CHECKs", () => {
  assert.equal(
    colTypeFromDbField(
      dbT.union(dbT.object({ kind: dbT.literal("a"), x: dbT.string() }), dbT.object({ kind: dbT.literal("b"), y: dbT.string() })),
    ),
    "json",
  );
  __begin();
  table("events").create({
    columns: {
      payload: fromDb(
        dbT.union(
          dbT.object({ kind: dbT.literal("login"), userId: dbT.double().required(), ip: dbT.string().required() }),
          dbT.object({ kind: dbT.literal("error"), message: dbT.string().required(), stack: dbT.string() }),
          dbT.object({ kind: dbT.literal("metric"), name: dbT.string().required(), value: dbT.double().required() }),
        ),
      ),
    },
  });
  const op = createTableOps();
  const names = op.columns.map((c) => c.name);
  assert.deepEqual(names, ["kind", "userId", "ip", "message", "stack", "name", "value"]);
  assert.equal(op.columns[0].nullable, false, "discriminator is NOT NULL");
  for (const column of op.columns.slice(1)) {
    assert.notEqual(column.nullable, false, `${String(column.name)} stays nullable`);
  }
  const byName = new Map(op.constraints!.map((c) => [c.name, c.kind]));
  assert.ok(byName.has("events_kind_enum_chk"), "discriminator IN-list");
  assert.ok(byName.has("events_kind_login_chk"), "login variant CHECK");
  assert.ok(byName.has("events_kind_error_chk"), "error variant CHECK");
  assert.ok(byName.has("events_kind_metric_chk"), "metric variant CHECK");
  // A variant with only optional non-discriminator fields has no CHECK.
  assert.equal(
    op.constraints!.filter((c) => c.name === "events_kind_error_chk").length,
    1,
  );
});

test("a union variant field shared with the same type is deduplicated", () => {
  __begin();
  table("events").create({
    columns: {
      payload: fromDb(
        dbT.union(
          dbT.object({ kind: dbT.literal("a"), shared: dbT.string() }),
          dbT.object({ kind: dbT.literal("b"), shared: dbT.string() }),
        ),
      ),
    },
  });
  const op = createTableOps();
  assert.deepEqual(op.columns.map((c) => c.name), ["kind", "shared"]);
});

test("positions without a CHECK slot refuse a union/literal ColumnDef", () => {
  __begin();
  assert.throws(
    () => table("x").column("kind").add({ type: fromDb(dbT.literal("login")) }),
    /t\.literal\(\.\.\.\).*table\(\.\.\.\)\.create/,
  );
  assert.throws(
    () =>
      table("x")
        .column("payload")
        .setType({ to: fromDb(dbT.union(dbT.object({ kind: dbT.literal("a") }), dbT.object({ kind: dbT.literal("b") }))) }),
    /t\.union\(\.\.\.\).*table\(\.\.\.\)\.create/,
  );
  assert.throws(() => t.array(fromDb(dbT.object({ a: dbT.string() }))), /string elements only/);
  assert.throws(() => t.array(fromDb(dbT.literal("x"))), /literal element has no native array storage/);
});

test("a union expansion that collides with a declared column is refused", () => {
  __begin();
  assert.throws(
    () =>
      table("events").create({
        columns: {
          kind: fromDb(dbT.string()),
          payload: fromDb(
            dbT.union(
              dbT.object({ kind: dbT.literal("a"), x: dbT.string() }),
              dbT.object({ kind: dbT.literal("b"), y: dbT.string() }),
            ),
          ),
        },
      }),
    /already declared/,
  );
});
