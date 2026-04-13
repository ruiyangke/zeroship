import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { validateDoc, validatePartial } from "../src/validate.js";
import { ValidationError } from "../src/errors.js";
import { normalizeSchema } from "../src/schema.js";
import { t } from "../src/types.js";

describe("validateDoc", () => {
  test("passes a valid doc", () => {
    const schema = normalizeSchema({ name: t.string().required() });
    const result = validateDoc({ name: "Alice" }, schema);
    assert.equal(result.name, "Alice");
  });

  test("throws ValidationError for missing required field", () => {
    const schema = normalizeSchema({ name: t.string().required() });
    assert.throws(() => validateDoc({}, schema), ValidationError);
  });

  test("required field error has correct path and message", () => {
    const schema = normalizeSchema({ email: t.string().required() });
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
    const schema = normalizeSchema({ role: t.string().default("user") });
    const result = validateDoc({}, schema);
    assert.equal(result.role, "user");
  });

  test("does not overwrite provided value with default", () => {
    const schema = normalizeSchema({ role: t.string().default("user") });
    const result = validateDoc({ role: "admin" }, schema);
    assert.equal(result.role, "admin");
  });

  test("type check: rejects wrong type (string expected)", () => {
    const schema = normalizeSchema({ name: t.string() });
    assert.throws(() => validateDoc({ name: 42 }, schema), ValidationError);
  });

  test("type check: rejects wrong type (number expected)", () => {
    const schema = normalizeSchema({ age: t.number() });
    assert.throws(() => validateDoc({ age: "old" }, schema), ValidationError);
  });

  test("type check: rejects wrong type (boolean expected)", () => {
    const schema = normalizeSchema({ active: t.boolean() });
    assert.throws(
      () => validateDoc({ active: "yes" }, schema),
      ValidationError
    );
  });

  test("type check: rejects wrong type (array expected)", () => {
    const schema = normalizeSchema({ tags: t.array(t.string()) });
    assert.throws(
      () => validateDoc({ tags: "not-array" }, schema),
      ValidationError
    );
  });

  test("min/max on string: length < min throws", () => {
    const schema = normalizeSchema({ name: t.string().min(3) });
    assert.throws(() => validateDoc({ name: "ab" }, schema), ValidationError);
  });

  test("min/max on string: length > max throws", () => {
    const schema = normalizeSchema({ name: t.string().max(5) });
    assert.throws(
      () => validateDoc({ name: "toolongname" }, schema),
      ValidationError
    );
  });

  test("min/max on string: valid length passes", () => {
    const schema = normalizeSchema({ name: t.string().min(2).max(10) });
    assert.doesNotThrow(() => validateDoc({ name: "Alice" }, schema));
  });

  test("min/max on number: value < min throws", () => {
    const schema = normalizeSchema({ age: t.number().min(0) });
    assert.throws(() => validateDoc({ age: -1 }, schema), ValidationError);
  });

  test("min/max on number: value > max throws", () => {
    const schema = normalizeSchema({ age: t.number().max(120) });
    assert.throws(() => validateDoc({ age: 200 }, schema), ValidationError);
  });

  test("min/max on number: valid value passes", () => {
    const schema = normalizeSchema({ age: t.number().min(0).max(120) });
    assert.doesNotThrow(() => validateDoc({ age: 25 }, schema));
  });

  test("enum: valid value passes", () => {
    const schema = normalizeSchema({
      role: t.string().enum("user", "admin"),
    });
    assert.doesNotThrow(() => validateDoc({ role: "admin" }, schema));
  });

  test("enum: invalid value throws", () => {
    const schema = normalizeSchema({
      role: t.string().enum("user", "admin"),
    });
    assert.throws(
      () => validateDoc({ role: "superuser" }, schema),
      ValidationError
    );
  });

  test("pattern: valid value passes", () => {
    const schema = normalizeSchema({
      slug: t.string().pattern(/^[a-z-]+$/),
    });
    assert.doesNotThrow(() => validateDoc({ slug: "my-post" }, schema));
  });

  test("pattern: invalid value throws", () => {
    const schema = normalizeSchema({
      slug: t.string().pattern(/^[a-z-]+$/),
    });
    assert.throws(
      () => validateDoc({ slug: "My Post!" }, schema),
      ValidationError
    );
  });

  test("multiple errors collected together", () => {
    const schema = normalizeSchema({
      name: t.string().required(),
      age: t.number().required(),
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

describe("validatePartial", () => {
  test("does not require required fields", () => {
    const schema = normalizeSchema({ name: t.string().required() });
    assert.doesNotThrow(() => validatePartial({}, schema));
  });

  test("validates provided fields", () => {
    const schema = normalizeSchema({ age: t.number() });
    assert.throws(
      () => validatePartial({ age: "not-a-number" }, schema),
      ValidationError
    );
  });

  test("passes valid partial doc", () => {
    const schema = normalizeSchema({
      name: t.string().required(),
      age: t.number().required(),
    });
    assert.doesNotThrow(() => validatePartial({ age: 25 }, schema));
  });

  test("does not apply defaults", () => {
    const schema = normalizeSchema({ role: t.string().default("user") });
    const result = validatePartial({}, schema);
    assert.equal(result.role, undefined);
  });
});
