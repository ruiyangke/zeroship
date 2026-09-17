import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { t, type Row, type RowInput } from "../src/index.js";
import { normalizeSchema } from "../../../crates/zeroship-data-v8/js/testing.js";

describe("assignment builders", () => {
  test("ID type carries its prefix without assigning a value", () => {
    assert.deepEqual(t.id("post").toFieldDef(), { type: "id", idPrefix: "post" });
    assert.throws(() => t.id(""), { code: "ID_INVALID_PREFIX" });
    assert.throws(() => t.id("Post-Type"), { code: "ID_INVALID_PREFIX" });
  });

  test("timestamp modifiers declare their generator and event", () => {
    assert.equal(t.timestamp().toFieldDef().assign, undefined);
    assert.deepEqual(t.timestamp().auto_now().toFieldDef().assign, { by: "now", on: "insert" });
    assert.deepEqual(t.timestamp().auto_now_on_update().toFieldDef().assign, { by: "now", on: "write" });
    assert.throws(() => t.string().auto_now(), { code: "AUTO_NOW_ON_NON_TIMESTAMP" });
    assert.throws(() => t.double().auto_now_on_update(), { code: "AUTO_NOW_ON_NON_TIMESTAMP" });
  });

  test("actor is a nullable string with an assignment", () => {
    const field = t.actor().nullable().toFieldDef();
    assert.equal(field.type, "string");
    assert.deepEqual(field.assign, { by: "actor", on: "insert" });
    assert.equal(field.writable, false);
  });

  test("ordinary names carry no implicit behavior", () => {
    for (const name of ["id", "created_at", "updated_at", "created_by", "updated_by", "version", "deleted_at"]) {
      assert.deepEqual(normalizeSchema({ [name]: t.string() }), { [name]: { type: "string" } });
    }
    const schema = { id: t.double().required(), created_at: t.string().required() };
    const input: RowInput<typeof schema> = { id: 12, created_at: "user value" };
    const row: Row<typeof schema> = input;
    assert.equal(row.created_at, "user value");
  });

  test("renamed assignments survive builder chains and remain read-only in inputs", () => {
    const schema = {
      id: t.string().assigned({ by: "typedId", on: "insert" }).required().primaryKey(),
      born: t.timestamp().auto_now().required(),
      editor: t.actor().nullable().required(),
      title: t.string().required(),
    };
    const input: RowInput<typeof schema> = { title: "hello" };
    // @ts-expect-error Assigned fields are supplied by the ORM.
    const invalid: RowInput<typeof schema> = { title: "hello", id: "chosen" };
    void invalid;
    const row: Row<typeof schema> = { ...input, id: "post_a", born: 1, editor: null };
    assert.equal(row.id, "post_a");
    assert.equal(normalizeSchema(schema).id.primaryKey, true);
  });
});
