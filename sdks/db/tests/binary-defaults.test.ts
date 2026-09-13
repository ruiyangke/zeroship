import { test } from "node:test";
import assert from "node:assert/strict";
import { installSchema, type NativeDb, type RuntimeSchemaDescriptor } from "@zeroship/db/internal";

test("artifact binary defaults reach inserts as independent native buffers", async () => {
  const writes: Record<string, unknown>[] = [];
  const native = {
    collection() {
      return {
        async insert(row: Record<string, unknown>) {
          writes.push(row);
          return row;
        },
        async find() { return []; },
      };
    },
  } as unknown as NativeDb;
  const descriptor: RuntimeSchemaDescriptor = {
    version: 2,
    collections: {
      blobs: {
        fields: {
          id: { type: "string", required: true, primaryKey: true },
          payload: { type: "bytes", required: true, default: "AP8=" },
          empty: { type: "bytes", required: true, default: "" },
        },
        options: { softDelete: false, versioning: false, strictness: "strict" },
        indexes: [],
      },
    },
  };
  const original = structuredClone(descriptor);
  const db = native as unknown as {
    blobs: { insert(row: Record<string, unknown>): Promise<{ error: Error | null }> };
  };
  installSchema(native, descriptor);
  assert.equal((await db.blobs.insert({ id: "first" })).error, null);
  assert.deepEqual(writes[0].payload, new Uint8Array([0, 255]));
  assert.deepEqual(writes[0].empty, new Uint8Array());
  (writes[0].payload as Uint8Array)[0] = 99;
  assert.equal((await db.blobs.insert({ id: "second" })).error, null);
  assert.deepEqual(writes[1].payload, new Uint8Array([0, 255]));

  const beforeInvalid = writes.length;
  assert.ok((await db.blobs.insert({ id: "invalid", payload: "AP8=" })).error);
  assert.equal(writes.length, beforeInvalid);
  assert.deepEqual(descriptor, original);

  installSchema(native, descriptor);
  assert.equal((await db.blobs.insert({ id: "reinstalled" })).error, null);
  assert.deepEqual(writes.at(-1)?.payload, new Uint8Array([0, 255]));

  for (const invalid of ["%%%", "AP8", "AP8=\n", "AP9="]) {
    descriptor.collections.blobs.fields.payload.default = invalid;
    assert.throws(() => installSchema(native, descriptor), { code: "INVALID_RUNTIME_DESCRIPTOR" });
  }
  assert.equal((await db.blobs.insert({ id: "after-invalid-install" })).error, null);
  assert.deepEqual(writes.at(-1)?.payload, new Uint8Array([0, 255]));
});
