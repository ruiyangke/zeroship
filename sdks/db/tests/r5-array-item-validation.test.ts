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
 * time with `code: "INVALID_ARRAY_ITEM"`. Closes off a feature
 * (`t.array(t.ref(...))`) that was never implemented end-to-end; a
 * future round can extend support intentionally if the use case lands.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "@zeroship/db";
import { validateDoc } from "../src/validate.js";
import { normalizeSchema } from "@zeroship/bootstrap/install-schema";
import { ValidationError } from "../src/errors.js";
import { validateArrayPushOps } from "../src/collection.js";

describe("R5 MINOR — t.array() rejects non-primitive item types", () => {
  test("rejects t.array(t.ref('users'))", () => {
    try {
      t.array(t.ref("users"));
      assert.fail("t.array(t.ref(...)) should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "INVALID_ARRAY_ITEM");
      assert.match(err.message, /ref/);
    }
  });

  test("rejects t.array(t.object({...}))", () => {
    try {
      t.array(t.object({ name: t.string() }));
      assert.fail("t.array(t.object(...)) should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "INVALID_ARRAY_ITEM");
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
      assert.equal(err.code, "INVALID_ARRAY_ITEM");
      assert.match(err.message, /union/);
    }
  });

  test("rejects t.array(t.literal('x'))", () => {
    try {
      t.array(t.literal("x"));
      assert.fail("t.array(t.literal(...)) should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "INVALID_ARRAY_ITEM");
      assert.match(err.message, /literal/);
    }
  });

  test("rejects nested t.array(t.array(t.string()))", () => {
    try {
      t.array(t.array(t.string()));
      assert.fail("nested t.array should have thrown");
    } catch (e) {
      const err = e as Error & { code?: string };
      assert.equal(err.code, "INVALID_ARRAY_ITEM");
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
      assert.equal(err.code, "INVALID_ARRAY_ITEM");
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

/**
 * R6 MINOR regression — R5 admitted `"json"` and `"calendarDate"` to
 * `PRIMITIVE_ITEM_TYPES` at declaration time, but the runtime array-item
 * validators in `validate.ts` and `collection.ts` (`validateArrayPushOps`)
 * only branched on the original four primitives, so the two new types
 * passed through unvalidated. Cover both validators here.
 */
describe("R6 MINOR — t.array(t.calendarDate()) validates each item", () => {
  const schema = normalizeSchema({ days: t.array(t.calendarDate()) });

  test("accepts a list of YYYY-MM-DD strings", () => {
    assert.doesNotThrow(() =>
      validateDoc({ days: ["2026-01-01", "2026-12-31"] }, schema),
    );
  });

  test("rejects an item that is not a string", () => {
    assert.throws(
      () => validateDoc({ days: ["2026-01-01", 42] }, schema),
      ValidationError,
    );
  });

  test("rejects an item that is not a valid calendar date", () => {
    assert.throws(
      () => validateDoc({ days: ["2026-13-01"] }, schema),
      ValidationError,
    );
    assert.throws(
      () => validateDoc({ days: ["2026-02-31"] }, schema),
      ValidationError,
    );
    assert.throws(
      () => validateDoc({ days: ["not a date"] }, schema),
      ValidationError,
    );
  });

  test("$push value must be a valid calendar date", () => {
    assert.throws(
      () => validateArrayPushOps({ $push: { days: "2026-13-99" } }, schema),
      ValidationError,
    );
    assert.throws(
      () => validateArrayPushOps({ $push: { days: 42 } }, schema),
      ValidationError,
    );
    assert.doesNotThrow(() =>
      validateArrayPushOps({ $push: { days: "2026-01-01" } }, schema),
    );
  });

  test("$addToSet value must be a valid calendar date", () => {
    assert.throws(
      () => validateArrayPushOps({ $addToSet: { days: "garbage" } }, schema),
      ValidationError,
    );
    assert.doesNotThrow(() =>
      validateArrayPushOps({ $addToSet: { days: "2026-06-15" } }, schema),
    );
  });
});

describe("R6 MINOR — t.array(t.json()) validates each item", () => {
  const schema = normalizeSchema({ data: t.array(t.json()) });

  test("accepts JSON-serialisable items (objects, arrays, scalars, null)", () => {
    assert.doesNotThrow(() =>
      validateDoc(
        {
          data: [
            { hello: "world" },
            [1, 2, 3],
            "string",
            42,
            true,
            null,
            { nested: { a: [1, { b: "ok" }] } },
          ],
        },
        schema,
      ),
    );
  });

  test("rejects items containing functions", () => {
    assert.throws(
      () => validateDoc({ data: [() => 42] }, schema),
      ValidationError,
    );
  });

  test("rejects items containing symbols", () => {
    assert.throws(
      () => validateDoc({ data: [Symbol("x")] }, schema),
      ValidationError,
    );
  });

  test("rejects items containing nested functions", () => {
    assert.throws(
      () => validateDoc({ data: [{ ok: 1, bad: () => "no" }] }, schema),
      ValidationError,
    );
    assert.throws(
      () => validateDoc({ data: [[1, 2, [3, () => 4]]] }, schema),
      ValidationError,
    );
  });

  test("rejects items containing bigints (JSON.stringify throws on these)", () => {
    assert.throws(
      () => validateDoc({ data: [123n] }, schema),
      ValidationError,
    );
  });

  test("rejects items containing cyclic structures", () => {
    const cyc: Record<string, unknown> = { name: "loop" };
    cyc.self = cyc;
    assert.throws(
      () => validateDoc({ data: [cyc] }, schema),
      ValidationError,
    );
  });

  test("$push value must be JSON-serialisable", () => {
    assert.throws(
      () => validateArrayPushOps({ $push: { data: () => 1 } }, schema),
      ValidationError,
    );
    assert.doesNotThrow(() =>
      validateArrayPushOps({ $push: { data: { ok: true } } }, schema),
    );
  });

  test("$addToSet value must be JSON-serialisable", () => {
    assert.throws(
      () => validateArrayPushOps({ $addToSet: { data: Symbol("x") } }, schema),
      ValidationError,
    );
    assert.doesNotThrow(() =>
      validateArrayPushOps({ $addToSet: { data: { ok: true } } }, schema),
    );
  });
});
