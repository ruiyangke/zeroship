import { test } from "node:test";
import assert from "node:assert/strict";
import { Collection } from "../../../crates/zeroship-data-v8/js/runtime/collection.js";
import { installSchema } from "../../../crates/zeroship-data-v8/js/testing.js";

const id = { type: "string" as const, required: true, primaryKey: true };

test("collection identity is explicit and does not invent assignments", () => {
  const fields = { id, slug: { type: "string" as const, unique: true } };
  const native = {} as ConstructorParameters<typeof Collection>[2];
  assert.doesNotThrow(() => new Collection("entries", fields, native));
  assert.equal("assign" in fields.id, false);
  assert.deepEqual(Object.keys(fields), ["id", "slug"]);
});

test("invalid identities fail before collections are published", () => {
  const invalidFields: Array<ConstructorParameters<typeof Collection>[1]> = [
    {},
    { key: id },
    { id: { ...id, primaryKey: false } },
    { id: { type: "string" as const, primaryKey: true } },
    { id: { ...id, required: false } },
    { id, tenant: id },
    { id: { type: "json", required: true, primaryKey: true } },
    { id: { ...id, encrypted: true } },
    { id: { ...id, mask: { kind: "full", classification: "pii" } } },
    { id: { ...id, assign: { by: "actor", on: "write" } } },
    { id: { ...id, assign: { by: "actor", on: "delete" } } },
  ];
  for (const fields of invalidFields) {
    const native = {} as ConstructorParameters<typeof Collection>[2];
    assert.throws(() => new Collection("invalid", fields, native), {
      code: "INVALID_COLLECTION_IDENTITY",
    });
    const collection = (fields: unknown) => ({ fields, indexes: [] });
    const projection = { collections: {
      entries: collection({ id }), invalid: collection(fields),
    } };
    assert.throws(() => installSchema(native, projection as never), {
      code: "INVALID_COLLECTION_IDENTITY",
    });
    assert.equal("entries" in native, false);
    assert.equal("invalid" in native, false);
  }
});
