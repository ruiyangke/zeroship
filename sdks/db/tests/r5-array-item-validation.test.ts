/**
 * R5 MINOR regression — `t.array(items)` used an unchecked
 * `as PrimitiveTypeName` cast (sdks/db/src/types.ts:584-587 pre-fix)
 * that silently corrupted the schema when callers passed `t.ref(...)`
 * or `t.object({...})` as items: the resulting `FieldDef` reported
 * `items: "ref"` / `items: "object"` (not a valid `PrimitiveTypeName`),
 * dropped `refTarget` and the nested object `shape`, and was skipped by
 * `validateRefTargets`' recursion (which descends `shape` / `variants`,
 * not array items).
 *
 * Option A fix: reject non-primitive item types at schema-declaration
 * time with `code: "invalid_array_item"`. Closes off a feature
 * (`t.array(t.ref(...))`) that was never implemented end-to-end; a
 * future round can extend support intentionally if the use case lands.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/types.js";

describe("R5 MINOR — t.array() rejects non-primitive item types", () => {
  test("rejects t.array(t.ref('users'))", () => {
    try {
      t.array(t.ref("users"));
      assert.fail("t.array(t.ref(...)) should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "invalid_array_item");
      assert.match(err.message, /ref/);
    }
  });

  test("rejects t.array(t.object({...}))", () => {
    try {
      t.array(t.object({ name: t.string() }));
      assert.fail("t.array(t.object(...)) should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "invalid_array_item");
      assert.match(err.message, /object/);
    }
  });

  test("rejects t.array(t.union(...))", () => {
    try {
      t.array(
        t.union(
          t.object({ kind: t.literal("a"), x: t.string() }),
          t.object({ kind: t.literal("b"), y: t.number() }),
        ),
      );
      assert.fail("t.array(t.union(...)) should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "invalid_array_item");
      assert.match(err.message, /union/);
    }
  });

  test("rejects t.array(t.literal('x'))", () => {
    try {
      t.array(t.literal("x"));
      assert.fail("t.array(t.literal(...)) should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "invalid_array_item");
      assert.match(err.message, /literal/);
    }
  });

  test("rejects nested t.array(t.array(t.string()))", () => {
    try {
      t.array(t.array(t.string()));
      assert.fail("nested t.array should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "invalid_array_item");
      assert.match(err.message, /array/);
    }
  });

  test("rejects t.array(<non-TypeBuilder>) with the same code", () => {
    try {
      // @ts-expect-error — deliberately passing a non-builder
      t.array({ type: "string" });
      assert.fail("non-TypeBuilder item should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "invalid_array_item");
    }
  });

  test("still accepts all primitive item builders", () => {
    // Sanity — Option A must not regress any current usage.
    assert.equal(t.array(t.string()).toFieldDef().items, "string");
    assert.equal(t.array(t.number()).toFieldDef().items, "number");
    assert.equal(t.array(t.boolean()).toFieldDef().items, "boolean");
    assert.equal(t.array(t.timestamp()).toFieldDef().items, "date");
    assert.equal(t.array(t.json()).toFieldDef().items, "json");
    assert.equal(t.array(t.calendarDate()).toFieldDef().items, "calendarDate");
  });
});
