/**
 * B2 — module-init runtime validation for `t.ref()` targets.
 *
 * Exercises `validateRefTargets`, the helper that `_installSchema` calls
 * to verify every `t.ref("table")` points at a collection in the same
 * schema map. Lives in its own test file (rather than `db.test.ts`)
 * because the validation logic is pure data — it does not need the
 * runtime's `zeroship` module — and so it can run in any node env.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
// Import directly from the schema / types modules so the test bundle
// does not transitively pull in `db.ts`, which imports `env` from the
// runtime-supplied "zeroship" module (unavailable in node test env).
import { validateRefTargets } from "../src/schema.js";
import { t, schema as schemaWrap } from "../src/types.js";

describe("B2 validateRefTargets — runtime ref check", () => {
  test("accepts a ref pointing at a declared collection", () => {
    assert.doesNotThrow(() => {
      validateRefTargets({
        users: { name: t.string().required() },
        posts: { title: t.string().required(), authorId: t.ref("users") },
      });
    });
  });

  test("throws ref_target_not_found when target collection is missing", () => {
    try {
      validateRefTargets({
        posts: {
          title: t.string().required(),
          // simulates `t.ref("x" as any)` escape past TS check
          authorId: t.ref("nonexistent"),
        },
      });
      assert.fail("validateRefTargets should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string; collection?: string; field?: string; target?: string };
      assert.equal(err.code, "ref_target_not_found");
      assert.equal(err.collection, "posts");
      assert.equal(err.field, "authorId");
      assert.equal(err.target, "nonexistent");
    }
  });

  test("permits self-referencing ref (employees.managerId → employees)", () => {
    assert.doesNotThrow(() => {
      validateRefTargets({
        employees: {
          name: t.string().required(),
          managerId: t.ref("employees"),
        },
      });
    });
  });

  test("permits circular refs (users ↔ posts)", () => {
    assert.doesNotThrow(() => {
      validateRefTargets({
        users: { name: t.string().required(), favPostId: t.ref("posts") },
        posts: { title: t.string().required(), authorId: t.ref("users") },
      });
    });
  });

  test("permits ref into a SchemaBuilder-wrapped collection (softDelete)", () => {
    assert.doesNotThrow(() => {
      validateRefTargets({
        users: schemaWrap({ name: t.string().required() }).softDelete(),
        posts: { title: t.string().required(), authorId: t.ref("users") },
      });
    });
  });

  test("detects ref escape via raw FieldDef literal", () => {
    // Direct FieldDef object — bypasses the TypeBuilder but the
    // check inspects `type === "ref"` so it should still fire.
    try {
      validateRefTargets({
        posts: {
          authorId: { type: "ref", refTarget: "ghosts" } as any,
        },
      });
      assert.fail("validateRefTargets should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string; target?: string };
      assert.equal(err.code, "ref_target_not_found");
      assert.equal(err.target, "ghosts");
    }
  });

  test("non-ref TypeBuilder fields are ignored", () => {
    assert.doesNotThrow(() => {
      validateRefTargets({
        users: { name: t.string(), age: t.number(), tags: t.array(t.string()) },
      });
    });
  });
});
