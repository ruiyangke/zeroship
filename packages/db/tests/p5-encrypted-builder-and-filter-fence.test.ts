import { test } from "node:test";
import assert from "node:assert/strict";
import { t } from "../src/index.js";
import { installSchemaForTest } from "./_install-helper.js";

function makeMockNative() {
  // Mock native that records calls. We never hit the body for the
  // rejection tests; the fence throws synchronously before any
  // `find`/`updateMany`/etc. native call.
  const noop = async () => null;
  const noopList = async () => [];
  const noopCount = async () => 0;
  const collection = (): unknown => ({
    find: noopList,
    insert: noop,
    insertMany: noopList,
    update: noop,
    updateMany: noopCount,
    delete: noop,
    deleteMany: noopCount,
    upsert: noop,
    count: noopCount,
    distinct: noopList,
    aggregate: noopList,
    search: noopList,
    near: noopList,
  });
  return {
    collection,
  } as unknown as Parameters<typeof installSchemaForTest>[1]["native"];
}


test("encrypted metadata uses the logical field type", () => {
  assert.deepEqual(t.string().encrypted().toFieldDef(), {
    type: "string", encrypted: true, mask: { kind: "full", classification: "pii" },
  });
  assert.deepEqual(t.double().encrypted().toFieldDef(), {
    type: "number", encrypted: true, mask: { kind: "full", classification: "pii" },
  });
  assert.deepEqual(t.bytes().encrypted().toFieldDef(), {
    type: "bytes", encrypted: true, mask: { kind: "full", classification: "pii" },
  });
});

test("encrypted fields reject unique constraints and unsupported plaintext types", () => {
  assert.throws(() => t.string().encrypted().unique(), { code: "UNIQUE_ENCRYPTED_UNSUPPORTED" });
  for (const field of [t.boolean(), t.object({ a: t.string() }), t.ref("users")]) {
    assert.throws(
      () => (field as unknown as { encrypted(): unknown }).encrypted(),
      { code: "ENCRYPTED_TYPE_UNSUPPORTED" },
    );
  }
});

test("an encrypted field cannot be a foreign key in either chain order", () => {
  assert.throws(
    () => t.string().references("users").encrypted(),
    { code: "ENCRYPTED_ON_REF_UNSUPPORTED" },
  );
  assert.throws(
    () => t.string().encrypted().references("users"),
    { code: "ENCRYPTED_ON_REF_UNSUPPORTED" },
  );
  // The control: an unencrypted reference still builds.
  assert.ok(t.string().references("users"));
});

function dbWithEncrypted() {
  return installSchemaForTest({ users: { name: t.string(), secret: t.string().encrypted() } }, { native: makeMockNative() });
}

for (const value of ["secret", null, { $eq: "secret" }, { $in: ["A", "B"] }, { $gt: "X" }, { $like: "X%" }, { $exists: true }]) {
  test(`encrypted filter is rejected: ${JSON.stringify(value)}`, () => {
    const db = dbWithEncrypted();
    for (const filter of [{ secret: value }, { $or: [{ secret: value }, { name: "x" }] }, { $and: [{ $not: { secret: value } }] }]) {
      assert.throws(() => db.users.find(filter as never), { code: "ENCRYPTED_FIELD_NOT_FILTERABLE" });
    }
  });
}

test("count and distinct reject encrypted values", async () => {
  const db = dbWithEncrypted();
  await assert.rejects(() => db.users.count({ secret: "X" } as never), { code: "ENCRYPTED_FIELD_NOT_FILTERABLE" });
  await assert.rejects(() => db.users.distinct("secret" as never), { code: "DISTINCT_ON_ENCRYPTED_FIELD_UNSUPPORTED" });
});

test("plain fields remain filterable beside encrypted fields", () => {
  assert.ok(dbWithEncrypted().users.find({ name: "alice" }));
});
