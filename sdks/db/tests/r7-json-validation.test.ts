/**
 * R7 regression — top-level `t.json()` validation + tightened
 * `isJsonSerializable` predicate.
 *
 *  M1. `checkField` had no `type === "json"` case, so a top-level
 *      `t.json()` field accepted any value (functions, symbols, cycles).
 *      Closed by adding the case, mirroring the array-item branch.
 *
 *  m2. `isJsonSerializable` accepted `Map`/`Set`/typed-arrays because
 *      `Object.values(...)` is empty on those built-ins — they would
 *      silently round-trip as `{}` through JSON.stringify. Tightened
 *      with an explicit instance-of check + a plain-object prototype
 *      guard so the predicate matches the JSON wire shape.
 */
import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/types.js";
import { validateDoc, isJsonSerializable } from "../src/validate.js";
import { normalizeSchema } from "../src/schema.js";
import { ValidationError } from "../src/errors.js";

describe("R7 M1 — top-level t.json() validates the value", () => {
  const schema = normalizeSchema({ payload: t.json() });

  test("accepts plain JSON values (object, array, scalar, null)", () => {
    assert.doesNotThrow(() => validateDoc({ payload: { ok: 1 } }, schema));
    assert.doesNotThrow(() => validateDoc({ payload: [1, 2, 3] }, schema));
    assert.doesNotThrow(() => validateDoc({ payload: "hello" }, schema));
    assert.doesNotThrow(() => validateDoc({ payload: 42 }, schema));
    assert.doesNotThrow(() => validateDoc({ payload: null }, schema));
  });

  test("rejects a top-level function", () => {
    assert.throws(
      () => validateDoc({ payload: () => 1 }, schema),
      ValidationError,
    );
  });

  test("rejects a top-level symbol", () => {
    assert.throws(
      () => validateDoc({ payload: Symbol("x") }, schema),
      ValidationError,
    );
  });

  test("rejects a top-level bigint", () => {
    assert.throws(
      () => validateDoc({ payload: 123n }, schema),
      ValidationError,
    );
  });

  test("rejects an object containing a nested function", () => {
    assert.throws(
      () => validateDoc({ payload: { fn: () => 1 } }, schema),
      ValidationError,
    );
  });

  test("rejects a cyclic structure", () => {
    const cyc: Record<string, unknown> = { name: "loop" };
    cyc.self = cyc;
    assert.throws(
      () => validateDoc({ payload: cyc }, schema),
      ValidationError,
    );
  });
});

describe("R7 m2 — isJsonSerializable rejects Map/Set/typed-arrays/etc.", () => {
  test("rejects Map (Object.values is empty — would silently serialise as {})", () => {
    assert.equal(isJsonSerializable(new Map([["a", 1]])), false);
    assert.equal(isJsonSerializable(new Map()), false);
  });

  test("rejects Set", () => {
    assert.equal(isJsonSerializable(new Set([1, 2, 3])), false);
    assert.equal(isJsonSerializable(new Set()), false);
  });

  test("rejects typed arrays (Uint8Array etc.)", () => {
    assert.equal(isJsonSerializable(new Uint8Array([1, 2, 3])), false);
    assert.equal(isJsonSerializable(new Float32Array(2)), false);
    assert.equal(isJsonSerializable(new Int16Array(4)), false);
  });

  test("rejects ArrayBuffer + DataView", () => {
    const buf = new ArrayBuffer(8);
    assert.equal(isJsonSerializable(buf), false);
    assert.equal(isJsonSerializable(new DataView(buf)), false);
  });

  test("rejects RegExp", () => {
    assert.equal(isJsonSerializable(/abc/g), false);
  });

  test("rejects Promise", () => {
    assert.equal(isJsonSerializable(Promise.resolve(1)), false);
  });

  test("rejects class instances (non-plain-object prototypes)", () => {
    class Foo {
      bar = 1;
    }
    assert.equal(isJsonSerializable(new Foo()), false);
  });

  test("accepts plain objects, arrays, primitives, null", () => {
    assert.equal(isJsonSerializable({ a: 1, b: { c: [1, 2] } }), true);
    assert.equal(isJsonSerializable([1, 2, 3]), true);
    assert.equal(isJsonSerializable("hi"), true);
    assert.equal(isJsonSerializable(42), true);
    assert.equal(isJsonSerializable(true), true);
    assert.equal(isJsonSerializable(null), true);
  });

  test("accepts Date instances (JSON.stringify produces ISO string)", () => {
    assert.equal(isJsonSerializable(new Date()), true);
  });

  test("accepts Object.create(null) containers", () => {
    const bare = Object.create(null);
    bare.a = 1;
    assert.equal(isJsonSerializable(bare), true);
  });

  test("array containing a Map is rejected", () => {
    const schema = normalizeSchema({ data: t.array(t.json()) });
    assert.throws(
      () => validateDoc({ data: [new Map([["k", "v"]])] }, schema),
      ValidationError,
    );
  });

  test("top-level json field containing a Set is rejected", () => {
    const schema = normalizeSchema({ payload: t.json() });
    assert.throws(
      () => validateDoc({ payload: new Set([1, 2, 3]) }, schema),
      ValidationError,
    );
  });
});
