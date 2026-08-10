import assert from "node:assert/strict";
import { test } from "node:test";

import { domain, enumType, sequence, table, t } from "../src/index.js";
import { buildEnvelope } from "../src/internal/recorder.js";

test("buildEnvelope rejects async up functions and resets the recorder", () => {
  assert.throws(
    () =>
      buildEnvelope(
        {
          async up() {
            await Promise.resolve();
          },
        },
        { irVersion: 1 },
      ),
    (error: any) =>
      error.code === "ASYNC_UP_UNSUPPORTED" &&
      /must be synchronous/.test(error.message),
  );

  const next = buildEnvelope(
    {
      up() {
        table("after_async_error").create({ columns: { id: t.int() } });
      },
    },
    { irVersion: 1 },
  );
  assert.deepEqual(
    (next.ops as Array<{ op: string }>).map((op) => op.op),
    ["createTable"],
  );
});

test("buildEnvelope resets the recorder when up throws", () => {
  const original = new Error("authoring failed");
  assert.throws(
    () =>
      buildEnvelope(
        {
          up() {
            table("partial").create({ columns: { id: t.int() } });
            throw original;
          },
        },
        { irVersion: 1 },
      ),
    (error) => error === original,
  );

  assert.throws(
    () => table("outside_after_error").create({ columns: { id: t.int() } }),
    (error: any) => error.code === "OP_OUTSIDE_RECORDER",
  );

  const next = buildEnvelope(
    {
      up() {
        table("clean").create({ columns: { id: t.int() } });
      },
    },
    { irVersion: 1 },
  );
  assert.equal(next.ops.length, 1);
  assert.equal((next.ops[0] as { name: string }).name, "clean");
});

test("each migration envelope contains only operations authored by its own up", () => {
  const first = buildEnvelope(
    {
      up() {
        enumType("first_state").create({ values: ["ready"] });
      },
    },
    { irVersion: 1, nameFallback: "first" },
  );
  const second = buildEnvelope(
    {
      up() {
        sequence("second_sequence").create();
      },
    },
    { irVersion: 1, nameFallback: "second" },
  );

  assert.deepEqual(first.ops, [
    { op: "createEnum", name: "first_state", values: ["ready"] },
  ]);
  assert.deepEqual(second.ops, [
    { op: "createSequence", name: "second_sequence" },
  ]);
});

test("named-type terminals reject top-level authoring instead of leaking into another migration", () => {
  const calls = [
    () => enumType("top_level_enum").create({ values: ["ready"] }),
    () => domain("top_level_domain").create({ as: t.int() }),
    () => sequence("top_level_sequence").create(),
  ];

  for (const call of calls) {
    let thrown: any;
    try {
      call();
    } catch (error) {
      thrown = error;
    }
    assert.equal(thrown?.code, "OP_OUTSIDE_RECORDER");
    assert.match(thrown?.message ?? "", /inside up\(\)/);
  }
});

test("an authored down() is refused rather than silently replaced by a synthesised one", () => {
  assert.throws(
    () =>
      buildEnvelope(
        {
          up() {
            table("authored_down_named").create({ columns: { id: t.int() } });
          },
          down() {
            table("authored_down_named").drop();
          },
        },
        { irVersion: 1 },
      ),
    (error: any) =>
      error.code === "AUTHORED_DOWN_UNSUPPORTED" &&
      /down\(\)/.test(error.message),
  );
});

test("an authored default.down() is refused the same way the named export is", () => {
  assert.throws(
    () =>
      buildEnvelope(
        {
          default: {
            up() {
              table("authored_down_default").create({ columns: { id: t.int() } });
            },
            down() {
              table("authored_down_default").drop();
            },
          },
        },
        { irVersion: 1 },
      ),
    (error: any) => error.code === "AUTHORED_DOWN_UNSUPPORTED",
  );
});

test("a migration that declares down without authoring one still builds", () => {
  // The refusal keys on a callable body, not on the key being present: `down?`
  // is optional in the module shape, so an explicit `undefined` is the same as
  // omitting it and must not be read as an authored rollback.
  const envelope = buildEnvelope(
    {
      up() {
        table("declared_down_only").create({ columns: { id: t.int() } });
      },
      down: undefined,
      default: { down: undefined },
    },
    { irVersion: 1 },
  );
  assert.deepEqual(
    (envelope.ops as Array<{ op: string }>).map((op) => op.op),
    ["createTable"],
  );
});
