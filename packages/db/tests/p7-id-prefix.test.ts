/**
 * **P7** — `t.typedId(prefix)` typed-id prefix declaration.
 *
 * Covers the SDK-builder fence on the declared prefix:
 * - `t.typedId("blog")` returns a builder carrying
 *   `{ type: "string", maxLength: 36, idPrefix: "blog" }`.
 * - `t.typedId("usr")` throws (ID_RESERVED_PREFIX) — `usr` is the platform
 *   user-id prefix and must never be a creator prefix.
 * - `t.typedId("")` and `t.typedId("1bad")` throw (ID_INVALID_PREFIX).
 * - `t.typedId()` (no arg) returns the bounded string with no idPrefix.
 *
 * Mirrors the ORM SQL mapping's `validate_id_prefix` fence.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/index.js";

describe("P7 — t.typedId(prefix) fence", () => {
  test("t_id_blog_carries_idPrefix", () => {
    const def = t.typedId("blog").toFieldDef();
    assert.equal(def.type, "string");
    assert.equal(def.maxLength, 36);
    assert.equal(def.idPrefix, "blog");
  });

  test("t_id_no_arg_omits_idPrefix", () => {
    const def = t.typedId().toFieldDef();
    assert.equal(def.type, "string");
    assert.equal(def.maxLength, 36);
    assert.equal(def.idPrefix, undefined);
  });

  test("t_id_usr_is_reserved", () => {
    assert.throws(
      () => t.typedId("usr"),
      (err: unknown) => {
        assert.ok(err instanceof Error);
        assert.equal((err as { code?: string }).code, "ID_RESERVED_PREFIX");
        assert.match(err.message, /reserved for platform user ids/);
        return true;
      },
    );
  });

  test("t_id_empty_prefix_throws", () => {
    assert.throws(
      () => t.typedId(""),
      (err: unknown) => {
        assert.equal((err as { code?: string }).code, "ID_INVALID_PREFIX");
        return true;
      },
    );
  });

  test("t_id_malformed_prefix_throws", () => {
    assert.throws(
      () => t.typedId("1bad"),
      (err: unknown) => {
        assert.equal((err as { code?: string }).code, "ID_INVALID_PREFIX");
        return true;
      },
    );
  });
});
