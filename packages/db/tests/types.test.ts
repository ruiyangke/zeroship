import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/index.js";
import { TypeBuilder } from "../../../crates/zeroship-data-v8/js/testing.js";

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

  test("t.timestamp() creates a timestamp-typed TypeBuilder (TIMESTAMPTZ)", () => {
    const tb = t.timestamp();
    assert.ok(tb instanceof TypeBuilder);
    assert.equal(tb.toFieldDef().type, "timestamp");
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

  // The receiver is untouched: these assert the corrected contract. Their earlier
  // form (`result === tb`, receiver mutated) encoded the aliasing bug - see
  // `typebuilder-aliasing.test.ts` for the cross-builder case.
  test(".required() applies required to a derived builder", () => {
    const tb = t.string();
    const result = tb.required();
    assert.notEqual(result, tb);
    assert.equal(result.toFieldDef().required, true);
    assert.equal(tb.toFieldDef().required, undefined);
  });

  test(".unique() applies unique to a derived builder", () => {
    const tb = t.string();
    const result = tb.unique();
    assert.notEqual(result, tb);
    assert.equal(result.toFieldDef().unique, true);
    assert.equal(tb.toFieldDef().unique, undefined);
  });

  test(".index() applies index to a derived builder", () => {
    const tb = t.string();
    const result = tb.index();
    assert.notEqual(result, tb);
    assert.equal(result.toFieldDef().index, true);
    assert.equal(tb.toFieldDef().index, undefined);
  });

  test(".default() applies default to a derived builder", () => {
    const tb = t.string();
    const result = tb.default("hello");
    assert.notEqual(result, tb);
    assert.equal(result.toFieldDef().default, "hello");
    assert.equal(tb.toFieldDef().default, undefined);
  });

  test(".min() applies min to a derived builder", () => {
    const tb = t.number();
    const result = tb.min(0);
    assert.notEqual(result, tb);
    assert.equal(result.toFieldDef().min, 0);
    assert.equal(tb.toFieldDef().min, undefined);
  });

  test(".max() applies max to a derived builder", () => {
    const tb = t.number();
    const result = tb.max(100);
    assert.notEqual(result, tb);
    assert.equal(result.toFieldDef().max, 100);
    assert.equal(tb.toFieldDef().max, undefined);
  });

  test(".enum() applies enum values to a derived builder", () => {
    const tb = t.string();
    const result = tb.enum("a", "b", "c");
    assert.notEqual(result, tb);
    assert.deepEqual(result.toFieldDef().enum, ["a", "b", "c"]);
    assert.equal(tb.toFieldDef().enum, undefined);
  });

  test(".pattern() applies pattern to a derived builder", () => {
    const re = /^[a-z]+$/;
    const tb = t.string();
    const result = tb.pattern(re);
    assert.notEqual(result, tb);
    assert.equal(result.toFieldDef().pattern, re);
    assert.equal(tb.toFieldDef().pattern, undefined);
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

  test("t.ref omits default action policies", () => {
    const def = t.ref("users").toFieldDef();
    assert.equal("onDelete" in def, false);
    assert.equal("onUpdate" in def, false);
  });

  test("t.ref omits default deferrable policy", () => {
    const def = t.ref("users").toFieldDef();
    assert.equal("deferrable" in def, false);
  });

  test("t.ref accepts onDelete override", () => {
    const def = t.ref("users", { onDelete: "cascade" }).toFieldDef();
    assert.equal(def.onDelete, "cascade");
    assert.equal("onUpdate" in def, false);
    assert.equal("deferrable" in def, false);
  });

  test("t.ref accepts explicit onUpdate restrict and deferrable=true", () => {
    const def = t.ref("users", { onUpdate: "restrict", deferrable: true }).toFieldDef();
    assert.equal(def.onUpdate, "restrict");
    assert.equal(def.deferrable, true);
  });

  test("t.ref accepts deferrable=false override", () => {
    const def = t.ref("users", { deferrable: false }).toFieldDef();
    assert.equal(def.deferrable, false);
  });

  test("t.ref throws on empty table name", () => {
    assert.throws(() => t.ref(""), /non-empty table/);
  });
});
