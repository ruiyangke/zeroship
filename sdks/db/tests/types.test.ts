import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t, TypeBuilder } from "../src/types.js";

describe("t type builder", () => {
  test("t.string() creates string TypeBuilder", () => {
    const tb = t.string();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb.toFieldDef().type, "string");
  });

  test("t.number() creates number TypeBuilder", () => {
    const tb = t.number();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb.toFieldDef().type, "number");
  });

  test("t.boolean() creates boolean TypeBuilder", () => {
    const tb = t.boolean();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb.toFieldDef().type, "boolean");
  });

  test("t.date() creates date TypeBuilder", () => {
    const tb = t.date();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb.toFieldDef().type, "date");
  });

  test("t.json() creates json TypeBuilder", () => {
    const tb = t.json();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb.toFieldDef().type, "json");
  });

  test("t.array(t.string()) creates array TypeBuilder with items=string", () => {
    const tb = t.array(t.string());
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb.toFieldDef().type, "array");
    assert.equal(tb.toFieldDef().items, "string");
  });

  test("t.array(t.number()) creates array TypeBuilder with items=number", () => {
    const tb = t.array(t.number());
    assert.equal(tb.toFieldDef().type, "array");
    assert.equal(tb.toFieldDef().items, "number");
  });

  test(".required() sets required and returns this", () => {
    const tb = t.string();
    const result = tb.required();
    assert.equal(result, tb);
    assert.equal(tb.toFieldDef().required, true);
  });

  test(".unique() sets unique and returns this", () => {
    const tb = t.string();
    const result = tb.unique();
    assert.equal(result, tb);
    assert.equal(tb.toFieldDef().unique, true);
  });

  test(".index() sets index and returns this", () => {
    const tb = t.string();
    const result = tb.index();
    assert.equal(result, tb);
    assert.equal(tb.toFieldDef().index, true);
  });

  test(".default() sets default and returns this", () => {
    const tb = t.string();
    const result = tb.default("hello");
    assert.equal(result, tb);
    assert.equal(tb.toFieldDef().default, "hello");
  });

  test(".min() sets min and returns this", () => {
    const tb = t.number();
    const result = tb.min(0);
    assert.equal(result, tb);
    assert.equal(tb.toFieldDef().min, 0);
  });

  test(".max() sets max and returns this", () => {
    const tb = t.number();
    const result = tb.max(100);
    assert.equal(result, tb);
    assert.equal(tb.toFieldDef().max, 100);
  });

  test(".enum() sets enum values and returns this", () => {
    const tb = t.string();
    const result = tb.enum("a", "b", "c");
    assert.equal(result, tb);
    assert.deepEqual(tb.toFieldDef().enum, ["a", "b", "c"]);
  });

  test(".pattern() sets pattern and returns this", () => {
    const re = /^[a-z]+$/;
    const tb = t.string();
    const result = tb.pattern(re);
    assert.equal(result, tb);
    assert.equal(tb.toFieldDef().pattern, re);
  });

  test("chaining multiple modifiers", () => {
    const tb = t.string().required().unique().min(1).max(50).default("none");
    const def = tb.toFieldDef();
    assert.equal(def.type, "string");
    assert.equal(def.required, true);
    assert.equal(def.unique, true);
    assert.equal(def.min, 1);
    assert.equal(def.max, 50);
    assert.equal(def.default, "none");
  });

  test("each call to t.string() produces independent instance", () => {
    const a = t.string().required();
    const b = t.string();
    assert.equal(a.toFieldDef().required, true);
    assert.equal(b.toFieldDef().required, undefined);
  });

  // B2 — t.ref()
  test("t.ref(table) creates ref TypeBuilder with refTarget", () => {
    const tb = t.ref("users");
    assert.ok(tb instanceof TypeBuilder);
    const def = tb.toFieldDef();
    assert.equal(def.type, "ref");
    assert.equal(def.refTarget, "users");
  });

  test("t.ref defaults to onDelete=restrict and onUpdate=restrict", () => {
    const def = t.ref("users").toFieldDef();
    assert.equal(def.onDelete, "restrict");
    assert.equal(def.onUpdate, "restrict");
  });

  test("t.ref defaults to deferrable=true", () => {
    const def = t.ref("users").toFieldDef();
    assert.equal(def.deferrable, true);
  });

  test("t.ref accepts onDelete override", () => {
    const def = t.ref("users", { onDelete: "cascade" }).toFieldDef();
    assert.equal(def.onDelete, "cascade");
    assert.equal(def.onUpdate, "restrict");
  });

  test("t.ref accepts deferrable=false override", () => {
    const def = t.ref("users", { deferrable: false }).toFieldDef();
    assert.equal(def.deferrable, false);
  });

  test("t.ref throws on empty table name", () => {
    assert.throws(() => t.ref(""), /non-empty table/);
  });
});
