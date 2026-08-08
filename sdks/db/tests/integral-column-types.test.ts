/**
 * The runtime descriptor's integral column tokens, and the fail-open hole that
 * hid them.
 *
 * `installSchema` treats descriptor v1 as the only runtime schema source (the
 * declared TypeBuilder argument is ignored), so the tokens the generator emits
 * — `int`, `integer`, `bigInt`, `float` — are what `validateDoc` actually sees.
 * None of them is a `TypeName`, and the type dispatch had no final `else`, so
 * they matched nothing and the value passed through unvalidated. Measured
 * before the fix: a field declared `int` accepted the string `"abc"`, while the
 * same field declared `number` rejected it.
 *
 * These tests pin both halves. The unknown-type guard is the one that
 * generalises: without it, any future drift between the generator's vocabulary
 * and this SDK's silently disables validation for a whole column rather than
 * failing.
 */
import { describe, it } from "node:test";
import assert from "node:assert/strict";

import { validateDoc } from "../src/validate";

function validate(type: string, value: unknown): { ok: true; doc: unknown } | { ok: false; message: string } {
  try {
    return { ok: true, doc: validateDoc({ q: value } as never, { q: { type, required: true } } as never) };
  } catch (e) {
    return { ok: false, message: (e as Error).message };
  }
}

describe("integral column types from the runtime descriptor", () => {
  it("rejects a non-number in an int column", () => {
    // The exact case that passed before the fix.
    const r = validate("int", "abc");
    assert.equal(r.ok, false);
  });

  it("rejects a fractional value in an integral column", () => {
    // Postgres assignment-casts 1.5 to 2 in an INTEGER column, so accepting it
    // stores a number the creator did not write.
    for (const type of ["int", "integer", "bigInt"]) {
      const r = validate(type, 1.5);
      assert.equal(r.ok, false, `${type} accepted 1.5`);
      assert.match((r as { message: string }).message, /whole number/);
    }
  });

  it("rejects a bigInt beyond exact JS representation", () => {
    // BIGINT spans 64 bits; a JS number stops being exact above 2^53, so by the
    // time such a value arrives it is already the wrong number.
    const r = validate("bigInt", 2 ** 60);
    assert.equal(r.ok, false);
    assert.match((r as { message: string }).message, /represent exactly/);
  });

  it("accepts whole numbers in integral columns and fractions in float", () => {
    // The positive half: the guard must not reject valid writes.
    for (const type of ["int", "integer", "bigInt"]) {
      assert.equal(validate(type, 42).ok, true, `${type} rejected 42`);
    }
    assert.equal(validate("float", 1.5).ok, true);
    assert.equal(validate("number", 1.5).ok, true);
  });
});

describe("unknown field types fail closed", () => {
  it("throws rather than accepting anything for a type this SDK does not know", () => {
    const r = validate("totallyMadeUp", 1);
    assert.equal(r.ok, false);
    assert.match((r as { message: string }).message, /unknown field type/);
  });

  it("still passes TypeName members that have no validation branch", () => {
    // vector / geoPoint / bytes / actor are deliberately not field-validated
    // here. They belong to the union, so the unknown-type guard must not catch
    // them - otherwise this fix trades one outage for another.
    assert.equal(validate("bytes", "blob").ok, true);
    assert.equal(validate("vector", [1, 2]).ok, true);
    assert.equal(validate("geoPoint", { lat: 1, lon: 2 }).ok, true);
    assert.equal(validate("actor", "usr_1").ok, true);
  });
});
