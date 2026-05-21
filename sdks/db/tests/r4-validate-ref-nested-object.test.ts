/**
 * R4 MINOR-3 regression — `validateRefTargets` walked top-level fields
 * and top-level union variants, but stopped at object boundaries. A
 * `t.ref("ghost")` hidden inside `t.object({...}).shape` passed the
 * runtime safety net even when "ghost" was not declared. The fix:
 * extract a `walkFieldDef` recursion that descends into `fd.shape`
 * (for `type === "object"`) and `fd.variants` (for `type === "union"`)
 * at any depth.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { validateRefTargets } from "../src/schema.js";
import { t } from "../src/types.js";

describe("R4 MINOR-3 — validateRefTargets recursion into t.object()", () => {
  test("rejects t.ref('ghost') nested inside t.object({...})", () => {
    // Before the fix, the validator walked top-level fields and union
    // variants but stopped at object boundaries — a ref hidden inside
    // `t.object({...}).shape` slipped through.
    try {
      validateRefTargets({
        posts: {
          meta: t.object({ author: t.ref("ghost") }),
        },
      });
      assert.fail("validateRefTargets should have caught the nested ref");
    } catch (e) {
      const err = e as Error & { code?: string; collection?: string; field?: string; target?: string };
      assert.equal(err.code, "ref_target_not_found");
      assert.equal(err.target, "ghost");
      assert.equal(err.collection, "posts");
    }
  });

  test("permits t.ref() nested inside t.object() when target IS declared", () => {
    assert.doesNotThrow(() => {
      validateRefTargets({
        users: { name: t.string().required() },
        posts: {
          meta: t.object({ authorId: t.ref("users") }),
        },
      });
    });
  });

  test("recurses through deeply-nested t.object() shapes", () => {
    try {
      validateRefTargets({
        posts: {
          meta: t.object({
            audit: t.object({
              by: t.ref("ghost"),
            }),
          }),
        },
      });
      assert.fail("validateRefTargets should have caught the deeply-nested ref");
    } catch (e) {
      const err = e as Error & { code?: string; target?: string };
      assert.equal(err.code, "ref_target_not_found");
      assert.equal(err.target, "ghost");
    }
  });
});
