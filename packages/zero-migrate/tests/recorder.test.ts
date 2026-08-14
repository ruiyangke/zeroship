import assert from "node:assert/strict";
import { test } from "node:test";

import { enumType, sequence, table, t } from "../src/index.js";
import {
  buildEnvelope,
  recordMigration,
  type MigrationModule,
} from "../src/internal/recorder.js";

const SCHEMA_AND_DATA_MESSAGE =
  "host recorder: schema and data changes must be separate migrations; " +
  "export schema() and data() from different migration modules";
const MISSING_PHASE_MESSAGE =
  "host recorder: the migration module exports neither schema() nor data(); " +
  "export exactly one of schema() or data()";
const UP_MESSAGE =
  "host recorder: up() is no longer supported; use schema() for DDL or data() " +
  "for DML, in separate migration modules";
const DOWN_MESSAGE =
  "migration authors a down() function, which the recorder does not capture; " +
  "rollback runs the engine's synthesised inverse, so the authored body would " +
  "never execute; inverse() on a data() migration is the supported way to " +
  "declare a recorded reverse";
const DATA_REVERSE_REQUIRED_MESSAGE =
  "host recorder: a data() migration must declare exactly one of inverse() or " +
  "irreversible with a non-empty reason";
const DATA_REVERSE_CONFLICT_MESSAGE =
  "host recorder: a data() migration cannot declare both inverse() and " +
  "irreversible; they are mutually exclusive";
const DATA_ONLY_MEMBERS_MESSAGE =
  "host recorder: inverse() and irreversible may only be declared on a data() migration";
const IRREVERSIBLE_REASON_MESSAGE =
  "host recorder: irreversible must be a non-empty string explaining why this " +
  "data() migration cannot be reversed; boolean true is not a reason, and " +
  "lint/status need that text during a rollback decision";

function captureError(action: () => unknown): Error {
  try {
    action();
  } catch (error: unknown) {
    assert.ok(error instanceof Error);
    return error;
  }
  assert.fail("expected the recorder to refuse the migration");
}

function assertRefusal(mod: MigrationModule, message: string): void {
  const error = captureError(() => buildEnvelope(mod, { irVersion: 1 }));
  assert.equal(error.message, message);
}

test("rule 1: schema() and data() must be separate migrations", () => {
  const modules: MigrationModule[] = [
    {
      schema() {},
      data() {},
      inverse() {},
    },
    {
      default: {
        schema() {},
        data() {},
        inverse() {},
      },
    },
    {
      schema() {},
      default: {
        data() {},
        inverse() {},
      },
    },
  ];

  for (const mod of modules) assertRefusal(mod, SCHEMA_AND_DATA_MESSAGE);
});

test("rule 2: a migration must export schema() or data()", () => {
  const modules: MigrationModule[] = [{}, { default: {} }];
  for (const mod of modules) assertRefusal(mod, MISSING_PHASE_MESSAGE);
});

test("rule 3: up() is refused with schema() and data() replacements", () => {
  const modules: MigrationModule[] = [
    { up() {} },
    { default: { up() {} } },
  ];
  for (const mod of modules) assertRefusal(mod, UP_MESSAGE);
});

test("rule 4: down() is refused with inverse() as the supported data reverse", () => {
  const modules: MigrationModule[] = [
    {
      schema() {},
      down() {},
    },
    {
      default: {
        schema() {},
        down() {},
      },
    },
  ];
  for (const mod of modules) assertRefusal(mod, DOWN_MESSAGE);
});

test("rule 5: data() requires inverse() or an irreversible reason", () => {
  const modules: MigrationModule[] = [
    { data() {} },
    { default: { data() {} } },
  ];
  for (const mod of modules) assertRefusal(mod, DATA_REVERSE_REQUIRED_MESSAGE);
});

test("rule 6: inverse() and irreversible are mutually exclusive", () => {
  const modules: MigrationModule[] = [
    {
      data() {},
      inverse() {},
      irreversible: "the source rows no longer exist",
    },
    {
      default: {
        data() {},
        inverse() {},
        irreversible: "the source rows no longer exist",
      },
    },
  ];
  for (const mod of modules) assertRefusal(mod, DATA_REVERSE_CONFLICT_MESSAGE);
});

test("rule 7: inverse() and irreversible require data()", () => {
  const modules: MigrationModule[] = [
    {
      schema() {},
      inverse() {},
    },
    {
      default: {
        schema() {},
        inverse() {},
      },
    },
    {
      schema() {},
      irreversible: "the source rows no longer exist",
    },
    {
      default: {
        schema() {},
        irreversible: "the source rows no longer exist",
      },
    },
  ];
  for (const mod of modules) assertRefusal(mod, DATA_ONLY_MEMBERS_MESSAGE);
});

test("rule 8: irreversible must be a non-empty reason string", () => {
  const modules: MigrationModule[] = [
    {
      data() {},
      irreversible: true,
    },
    {
      default: {
        data() {},
        irreversible: "",
      },
    },
    {
      data() {},
      irreversible: "   ",
    },
  ];
  for (const mod of modules) assertRefusal(mod, IRREVERSIBLE_REASON_MESSAGE);
});

test("schema() records the same ordered op stream formerly recorded by up()", () => {
  const schema = () => {
    enumType("migration_state").create({ values: ["ready"] });
    sequence("migration_sequence").create();
  };
  const expected = [
    { op: "createEnum", name: "migration_state", values: ["ready"] },
    { op: "createSequence", name: "migration_sequence" },
  ];

  const named = buildEnvelope({ schema }, { irVersion: 1 });
  const defaultExport = buildEnvelope({ default: { schema } }, { irVersion: 1 });

  assert.deepEqual(named.ops, expected);
  assert.deepEqual(defaultExport.ops, expected);
});

test("data() and inverse() record independent ordered op streams", () => {
  const executions: string[] = [];
  const recorded = recordMigration(
    {
      data() {
        executions.push("data");
        const widgets = table("widgets");
        widgets.insert({ rows: { id: 1 } });
        widgets.insert({ rows: { id: 2 } });
      },
      default: {
        inverse() {
          executions.push("inverse");
          const widgets = table("widgets");
          widgets.delete({ where: (col) => col("id").eq(2) });
          widgets.delete({ where: (col) => col("id").eq(1) });
        },
      },
    },
    { irVersion: 1 },
  );

  assert.deepEqual(executions, ["data", "inverse"]);
  assert.deepEqual(recorded.envelope.ops, [
    { op: "insert", table: "widgets", columns: ["id"], rows: [[1]] },
    { op: "insert", table: "widgets", columns: ["id"], rows: [[2]] },
  ]);
  assert.deepEqual(recorded.inverseOps, [
    {
      op: "delete",
      table: "widgets",
      where: {
        node: "binOp",
        op: "eq",
        lhs: { node: "colRef", name: "id" },
        rhs: { node: "literal", value: 2 },
      },
    },
    {
      op: "delete",
      table: "widgets",
      where: {
        node: "binOp",
        op: "eq",
        lhs: { node: "colRef", name: "id" },
        rhs: { node: "literal", value: 1 },
      },
    },
  ]);
});

test("data() carries its irreversible reason", () => {
  const reason = "the upstream identifiers cannot be reconstructed";
  const recorded = recordMigration(
    {
      default: {
        data() {
          table("events").insert({ rows: { id: 1 } });
        },
        irreversible: reason,
      },
    },
    { irVersion: 1 },
  );

  assert.equal(recorded.irreversible, reason);
  assert.equal(recorded.inverseOps, undefined);
  assert.deepEqual(recorded.envelope.ops, [
    { op: "insert", table: "events", columns: ["id"], rows: [[1]] },
  ]);
});

test("the emitted IR envelope JSON retains exactly ir_version, name, and ops", () => {
  const envelope = buildEnvelope(
    {
      data() {
        table("events").insert({ rows: { id: 1 } });
      },
      inverse() {
        table("events").delete({ where: (col) => col("id").eq(1) });
      },
    },
    { irVersion: 7, nameFallback: "json_shape" },
  );
  const json: unknown = JSON.parse(JSON.stringify(envelope));
  assert.ok(typeof json === "object" && json !== null);
  assert.deepEqual(Object.keys(json), ["ir_version", "name", "ops"]);
});

test("an async schema() is refused and resets the recorder", () => {
  const error = captureError(() =>
    buildEnvelope(
      {
        async schema() {
          await Promise.resolve();
        },
      },
      { irVersion: 1 },
    ),
  );
  assert.match(error.message, /must be synchronous/);

  const outside = captureError(() =>
    table("outside_after_async_error").create({ columns: { id: t.int() } }),
  );
  assert.ok("code" in outside);
  assert.equal(outside.code, "OP_OUTSIDE_RECORDER");

  const next = buildEnvelope(
    {
      schema() {
        table("after_async_error").create({ columns: { id: t.int() } });
      },
    },
    { irVersion: 1 },
  );
  assert.equal(next.ops.length, 1);
});

test("a throwing schema() aborts and leaves the recorder clean", () => {
  const original = new Error("authoring failed");
  const thrown = captureError(() =>
    buildEnvelope(
      {
        schema() {
          table("partial").create({ columns: { id: t.int() } });
          throw original;
        },
      },
      { irVersion: 1 },
    ),
  );
  assert.equal(thrown, original);

  const outside = captureError(() =>
    table("outside_after_error").create({ columns: { id: t.int() } }),
  );
  assert.ok("code" in outside);
  assert.equal(outside.code, "OP_OUTSIDE_RECORDER");

  const next = buildEnvelope(
    {
      schema() {
        table("clean").create({ columns: { id: t.int() } });
      },
    },
    { irVersion: 1 },
  );
  assert.equal(next.ops.length, 1);
  assert.deepEqual(next.ops[0], {
    op: "createTable",
    name: "clean",
    columns: [{ name: "id", type: "int" }],
  });
});
