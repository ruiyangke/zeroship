/**
 * **P7** — `t.id(prefix)` typed-id prefix declaration.
 *
 * Covers the SDK-builder fence on the declared prefix:
 * - `t.id("blog")` returns a builder carrying `{ type: "id", idPrefix: "blog" }`.
 * - `t.id("usr")` throws (ID_RESERVED_PREFIX) — `usr` is the platform
 *   user-id prefix and must never be a creator prefix.
 * - `t.id("")` and `t.id("1bad")` throw (ID_INVALID_PREFIX).
 * - `t.id()` (no arg) returns `{ type: "id" }` with no idPrefix (auto-derive).
 *
 * Mirrors the Rust-side fence in `crates/plugin-db/src/query.rs`
 * (`validate_id_prefix`).
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "@zeroship/db";

describe("P7 — t.id(prefix) fence", () => {
  test("t_id_blog_carries_idPrefix", () => {
    const def = t.id("blog").toFieldDef();
    assert.equal(def.type, "id");
    assert.equal(def.idPrefix, "blog");
  });

  test("t_id_no_arg_omits_idPrefix", () => {
    const def = t.id().toFieldDef();
    assert.equal(def.type, "id");
    assert.equal(def.idPrefix, undefined);
  });

  test("t_id_usr_is_reserved", () => {
    assert.throws(
      () => t.id("usr"),
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
      () => t.id(""),
      (err: unknown) => {
        assert.equal((err as { code?: string }).code, "ID_INVALID_PREFIX");
        return true;
      },
    );
  });

  test("t_id_malformed_prefix_throws", () => {
    assert.throws(
      () => t.id("1bad"),
      (err: unknown) => {
        assert.equal((err as { code?: string }).code, "ID_INVALID_PREFIX");
        return true;
      },
    );
  });
});
