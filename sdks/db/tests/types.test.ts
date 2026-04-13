import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t, TypeBuilder } from "../src/types.js";

describe("t type builder", () => {
  test("t.string() creates string TypeBuilder", () => {
    const tb = t.string();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb._def.type, "string");
  });

  test("t.number() creates number TypeBuilder", () => {
    const tb = t.number();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb._def.type, "number");
  });

  test("t.boolean() creates boolean TypeBuilder", () => {
    const tb = t.boolean();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb._def.type, "boolean");
  });

  test("t.date() creates date TypeBuilder", () => {
    const tb = t.date();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb._def.type, "date");
  });

  test("t.json() creates json TypeBuilder", () => {
    const tb = t.json();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb._def.type, "json");
  });

  test("t.array(t.string()) creates array TypeBuilder with items=string", () => {
    const tb = t.array(t.string());
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb._def.type, "array");
    assert.equal(tb._def.items, "string");
  });

  test("t.array(t.number()) creates array TypeBuilder with items=number", () => {
    const tb = t.array(t.number());
    assert.equal(tb._def.type, "array");
    assert.equal(tb._def.items, "number");
  });

  test(".required() sets required and returns this", () => {
    const tb = t.string();
    const result = tb.required();
    assert.equal(result, tb);
    assert.equal(tb._def.required, true);
  });

  test(".unique() sets unique and returns this", () => {
    const tb = t.string();
    const result = tb.unique();
    assert.equal(result, tb);
    assert.equal(tb._def.unique, true);
  });

  test(".index() sets index and returns this", () => {
    const tb = t.string();
    const result = tb.index();
    assert.equal(result, tb);
    assert.equal(tb._def.index, true);
  });

  test(".default() sets default and returns this", () => {
    const tb = t.string();
    const result = tb.default("hello");
    assert.equal(result, tb);
    assert.equal(tb._def.default, "hello");
  });

  test(".min() sets min and returns this", () => {
    const tb = t.number();
    const result = tb.min(0);
    assert.equal(result, tb);
    assert.equal(tb._def.min, 0);
  });

  test(".max() sets max and returns this", () => {
    const tb = t.number();
    const result = tb.max(100);
    assert.equal(result, tb);
    assert.equal(tb._def.max, 100);
  });

  test(".enum() sets enum values and returns this", () => {
    const tb = t.string();
    const result = tb.enum("a", "b", "c");
    assert.equal(result, tb);
    assert.deepEqual(tb._def.enum, ["a", "b", "c"]);
  });

  test(".pattern() sets pattern and returns this", () => {
    const re = /^[a-z]+$/;
    const tb = t.string();
    const result = tb.pattern(re);
    assert.equal(result, tb);
    assert.equal(tb._def.pattern, re);
  });

  test("chaining multiple modifiers", () => {
    const tb = t.string().required().unique().min(1).max(50).default("none");
    assert.equal(tb._def.type, "string");
    assert.equal(tb._def.required, true);
    assert.equal(tb._def.unique, true);
    assert.equal(tb._def.min, 1);
    assert.equal(tb._def.max, 50);
    assert.equal(tb._def.default, "none");
  });

  test("each call to t.string() produces independent instance", () => {
    const a = t.string().required();
    const b = t.string();
    assert.equal(a._def.required, true);
    assert.equal(b._def.required, undefined);
  });
});
