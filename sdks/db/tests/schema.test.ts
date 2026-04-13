import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { normalizeSchema } from "../src/schema.js";
import { t } from "../src/types.js";

describe("normalizeSchema", () => {
  test("Mongoose style: String → string", () => {
    const schema = normalizeSchema({ name: { type: String } });
    assert.equal(schema.name.type, "string");
  });

  test("Mongoose style: Number → number", () => {
    const schema = normalizeSchema({ age: { type: Number } });
    assert.equal(schema.age.type, "number");
  });

  test("Mongoose style: Boolean → boolean", () => {
    const schema = normalizeSchema({ active: { type: Boolean } });
    assert.equal(schema.active.type, "boolean");
  });

  test("Mongoose style: Date → date", () => {
    const schema = normalizeSchema({ createdAt: { type: Date } });
    assert.equal(schema.createdAt.type, "date");
  });

  test("Mongoose style: Object → json", () => {
    const schema = normalizeSchema({ meta: { type: Object } });
    assert.equal(schema.meta.type, "json");
  });

  test("Mongoose style: [String] → array with items=string", () => {
    const schema = normalizeSchema({ tags: { type: [String] } });
    assert.equal(schema.tags.type, "array");
    assert.equal(schema.tags.items, "string");
  });

  test("Mongoose style: copies required, unique, index, default, min, max", () => {
    const schema = normalizeSchema({
      name: { type: String, required: true, unique: true, index: true, default: "anon", min: 1, max: 50 },
    });
    const f = schema.name;
    assert.equal(f.required, true);
    assert.equal(f.unique, true);
    assert.equal(f.index, true);
    assert.equal(f.default, "anon");
    assert.equal(f.min, 1);
    assert.equal(f.max, 50);
  });

  test("Mongoose style: enum copied", () => {
    const schema = normalizeSchema({
      role: { type: String, enum: ["user", "admin"] },
    });
    assert.deepEqual(schema.role.enum, ["user", "admin"]);
  });

  test("Mongoose style: match → pattern", () => {
    const re = /^\w+$/;
    const schema = normalizeSchema({ slug: { type: String, match: re } });
    assert.equal(schema.slug.pattern, re);
  });

  test("Builder style: t.string()", () => {
    const schema = normalizeSchema({ name: t.string() });
    assert.equal(schema.name.type, "string");
  });

  test("Builder style: t.number().required()", () => {
    const schema = normalizeSchema({ age: t.number().required() });
    assert.equal(schema.age.type, "number");
    assert.equal(schema.age.required, true);
  });

  test("Builder style: t.array(t.string())", () => {
    const schema = normalizeSchema({ tags: t.array(t.string()) });
    assert.equal(schema.tags.type, "array");
    assert.equal(schema.tags.items, "string");
  });

  test("Builder style: t.string().unique()", () => {
    const schema = normalizeSchema({ email: t.string().unique() });
    assert.equal(schema.email.unique, true);
  });

  test("Mixed: Mongoose and builder in same schema", () => {
    const schema = normalizeSchema({
      name: { type: String, required: true },
      age: t.number().min(0),
    });
    assert.equal(schema.name.type, "string");
    assert.equal(schema.name.required, true);
    assert.equal(schema.age.type, "number");
    assert.equal(schema.age.min, 0);
  });
});
