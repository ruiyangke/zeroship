import { test } from "node:test";
import assert from "node:assert/strict";
import { installSchema, type NativeDb, type SchemaProjection } from "../../../crates/zeroship-data-v8/js/testing.js";

test("decoded binary defaults reach inserts as independent native buffers", async () => {
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
  const projection: SchemaProjection = {
    collections: {
      blobs: {
        fields: {
          id: { type: "string", required: true, primaryKey: true },
          payload: { type: "bytes", required: true, default: [0, 255] },
          empty: { type: "bytes", required: true, default: [] },
        },
        indexes: [],
      },
    },
  };
  const db = native as unknown as {
    blobs: { insert(row: Record<string, unknown>): Promise<{ error: Error | null }> };
  };
  installSchema(native, projection);
  assert.equal((await db.blobs.insert({ id: "first" })).error, null);
  assert.deepEqual(writes[0].payload, new Uint8Array([0, 255]));
  assert.deepEqual(writes[0].empty, new Uint8Array());
  (writes[0].payload as Uint8Array)[0] = 99;
  assert.equal((await db.blobs.insert({ id: "second" })).error, null);
  assert.deepEqual(writes[1].payload, new Uint8Array([0, 255]));

  const beforeInvalid = writes.length;
  assert.ok((await db.blobs.insert({ id: "invalid", payload: "AP8=" })).error);
  assert.equal(writes.length, beforeInvalid);

  installSchema(native, projection);
  assert.equal((await db.blobs.insert({ id: "reinstalled" })).error, null);
  assert.deepEqual(writes.at(-1)?.payload, new Uint8Array([0, 255]));
});
