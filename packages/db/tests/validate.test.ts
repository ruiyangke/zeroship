import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { validateDoc, checkPartial } from "../../../crates/zeroship-data-v8/js/runtime/validate.js";
import { ValidationError } from "../src/errors.js";
import { fieldsOf } from "./_install-helper.js";
import { t } from "../src/index.js";

describe("validateDoc", () => {
  test("passes a valid doc", () => {
    const schema = fieldsOf({ name: t.string().required() });
    const result = validateDoc({ name: "Alice" }, schema);
    assert.equal(result.name, "Alice");
  });

  test("throws ValidationError for missing required field", () => {
    const schema = fieldsOf({ name: t.string().required() });
    assert.throws(() => validateDoc({}, schema), ValidationError);
  });

  test("required field error has correct path and message", () => {
    const schema = fieldsOf({ email: t.string().required() });
    try {
      validateDoc({}, schema);
      assert.fail("should have thrown");
    } catch (e) {
      assert.ok(e instanceof ValidationError);
      assert.ok("email" in e.errors);
      assert.equal(e.errors.email.path, "email");
    }
  });

  test("applies default for missing optional field", () => {
    const schema = fieldsOf({ role: t.string().default("user") });
    const result = validateDoc({}, schema);
    assert.equal(result.role, "user");
  });

  test("does not overwrite provided value with default", () => {
    const schema = fieldsOf({ role: t.string().default("user") });
    const result = validateDoc({ role: "admin" }, schema);
    assert.equal(result.role, "admin");
  });

  test("type check: rejects wrong type (string expected)", () => {
    const schema = fieldsOf({ name: t.string() });
    assert.throws(() => validateDoc({ name: 42 }, schema), ValidationError);
  });

  test("type check: rejects wrong type (number expected)", () => {
    const schema = fieldsOf({ age: t.double() });
    assert.throws(() => validateDoc({ age: "old" }, schema), ValidationError);
  });

  test("type check: rejects wrong type (boolean expected)", () => {
    const schema = fieldsOf({ active: t.boolean() });
    assert.throws(
      () => validateDoc({ active: "yes" }, schema),
      ValidationError
    );
  });

  test("type check: rejects wrong type (array expected)", () => {
    const schema = fieldsOf({ tags: t.array(t.string()) });
    assert.throws(
      () => validateDoc({ tags: "not-array" }, schema),
      ValidationError
    );
  });

  test("min/max on string: length < min throws", () => {
    const schema = fieldsOf({ name: t.string().min(3) });
    assert.throws(() => validateDoc({ name: "ab" }, schema), ValidationError);
  });

  test("min/max on string: length > max throws", () => {
    const schema = fieldsOf({ name: t.string().max(5) });
    assert.throws(
      () => validateDoc({ name: "toolongname" }, schema),
      ValidationError
    );
  });

  test("min/max on string: valid length passes", () => {
    const schema = fieldsOf({ name: t.string().min(2).max(10) });
    assert.doesNotThrow(() => validateDoc({ name: "Alice" }, schema));
  });

  test("min/max on number: value < min throws", () => {
    const schema = fieldsOf({ age: t.double().min(0) });
    assert.throws(() => validateDoc({ age: -1 }, schema), ValidationError);
  });

  test("min/max on number: value > max throws", () => {
    const schema = fieldsOf({ age: t.double().max(120) });
    assert.throws(() => validateDoc({ age: 200 }, schema), ValidationError);
  });

  test("min/max on number: valid value passes", () => {
    const schema = fieldsOf({ age: t.double().min(0).max(120) });
    assert.doesNotThrow(() => validateDoc({ age: 25 }, schema));
  });

  // The number arm rejected only non-numbers and NaN. `typeof Infinity === "number"`
  // and `isNaN(Infinity)` is false, so both infinities passed validation and reached
  // the wire - where `JSON.stringify(Infinity)` is `null` (measured on node v22.22.2).
  //
  // On a NOT NULL column that surfaces as a database error, which is at least loud.
  // On a nullable one the row silently holds NULL: the creator wrote a number,
  // validation approved it, and the value is gone with nothing reported. NaN was
  // already rejected, and NaN and Infinity fail identically on the wire - so the
  // existing guard covered one half of one problem.
  //
  // A `min`/`max` bound does not close this: bounds are optional, and `Infinity > max`
  // only catches a column that declared a max. The unbounded column is the common case.
  test("number: rejects Infinity, which JSON encodes as null", () => {
    const schema = fieldsOf({ score: t.double() });
    assert.throws(() => validateDoc({ score: Infinity }, schema), ValidationError);
  });

  test("number: rejects -Infinity", () => {
    const schema = fieldsOf({ score: t.double() });
    assert.throws(() => validateDoc({ score: -Infinity }, schema), ValidationError);
  });

  // POSITIVE CONTROL. The two assertions above are satisfied by a number arm that
  // rejects every number, so pin that finite values - including the boundary the
  // guard is most likely to get wrong - still pass.
  test("number: finite values still pass, including MAX_VALUE", () => {
    const schema = fieldsOf({ score: t.double() });
    assert.doesNotThrow(() => validateDoc({ score: 0 }, schema));
    assert.doesNotThrow(() => validateDoc({ score: -1.5 }, schema));
    assert.doesNotThrow(() => validateDoc({ score: Number.MAX_VALUE }, schema));
  });

  test("enum: valid value passes", () => {
    const schema = fieldsOf({
      role: t.string().enum("user", "admin"),
    });
    assert.doesNotThrow(() => validateDoc({ role: "admin" }, schema));
  });

  test("enum: invalid value throws", () => {
    const schema = fieldsOf({
      role: t.string().enum("user", "admin"),
    });
    assert.throws(
      () => validateDoc({ role: "superuser" }, schema),
      ValidationError
    );
  });

  test("pattern: valid value passes", () => {
    const schema = fieldsOf({
      slug: t.string().pattern(/^[a-z-]+$/),
    });
    assert.doesNotThrow(() => validateDoc({ slug: "my-post" }, schema));
  });

  test("pattern: invalid value throws", () => {
    const schema = fieldsOf({
      slug: t.string().pattern(/^[a-z-]+$/),
    });
    assert.throws(
      () => validateDoc({ slug: "My Post!" }, schema),
      ValidationError
    );
  });

  test("multiple errors collected together", () => {
    const schema = fieldsOf({
      name: t.string().required(),
      age: t.double().required(),
    });
    try {
      validateDoc({}, schema);
      assert.fail("should have thrown");
    } catch (e) {
      assert.ok(e instanceof ValidationError);
      assert.ok("name" in e.errors);
      assert.ok("age" in e.errors);
    }
  });
});

describe("validateDoc — array and enum edge cases", () => {
  // I1: enum check should not fire on arrays
  test("enum check does not run on array fields", () => {
    // `.enum()`'s generic constraint (`Values extends readonly (T &
    // (string|number))[]`, src/types.ts) is scalar-only by design -
    // `t.array(t.string())`'s `T` is `string[]`, which does not overlap
    // `string | number`, so the public typed API correctly refuses this
    // call. The test's INTENT is defensive: verify the VALIDATOR skips
    // enum checks even if `_def.enum` somehow ends up set on an array
    // field (a shape the typed builder API cannot itself produce). The
    // `any` cast is the deliberate off-contract construction that shape
    // requires, not a general weakening of `.enum()`'s scalar-only type.
    const schema = fieldsOf({ tags: (t.array(t.string()) as any).enum("a", "b") });
    // Array value should not be rejected by enum (enum is for scalar types only)
    assert.doesNotThrow(() => validateDoc({ tags: ["x", "y"] }, schema));
  });

  // I2: array item type validation
  test("array item validation: rejects wrong item type (string array, number given)", () => {
    const schema = fieldsOf({ tags: t.array(t.string()) });
    assert.throws(
      () => validateDoc({ tags: ["ok", 42] }, schema),
      ValidationError
    );
  });

  test("array item validation: accepts all correct items", () => {
    const schema = fieldsOf({ scores: t.array(t.double()) });
    assert.doesNotThrow(() => validateDoc({ scores: [1, 2, 3] }, schema));
  });

  test("array item validation: empty array always passes", () => {
    const schema = fieldsOf({ tags: t.array(t.string()) });
    assert.doesNotThrow(() => validateDoc({ tags: [] }, schema));
  });

  test("array item validation: rejects boolean item in number array", () => {
    const schema = fieldsOf({ scores: t.array(t.double()) });
    assert.throws(
      () => validateDoc({ scores: [1, true] }, schema),
      ValidationError
    );
  });
});

describe("checkPartial", () => {
  test("does not require required fields", () => {
    const schema = fieldsOf({ name: t.string().required() });
    assert.doesNotThrow(() => checkPartial({}, schema));
  });

  test("validates provided fields", () => {
    const schema = fieldsOf({ age: t.double() });
    assert.throws(
      () => checkPartial({ age: "not-a-number" }, schema),
      ValidationError
    );
  });

  test("passes valid partial doc", () => {
    const schema = fieldsOf({
      name: t.string().required(),
      age: t.double().required(),
    });
    assert.doesNotThrow(() => checkPartial({ age: 25 }, schema));
  });

  test("does not apply defaults", () => {
    const schema = fieldsOf({ role: t.string().default("user") });
    // checkPartial is void — just verify it doesn't throw
    assert.doesNotThrow(() => checkPartial({}, schema));
  });
});
