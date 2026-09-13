import { afterEach, describe, test } from "node:test";
import assert from "node:assert/strict";

import { devEntry } from "../src/dev-entry.js";

const g = globalThis as typeof globalThis & {
  __zsRuntimeDescriptor?: unknown;
};

function makeDevEntry(
  envDb: Record<string, unknown> = {
    transaction: (cb: (raw: unknown) => unknown) => cb(undefined),
    collection: () => ({ find: async () => [] }),
  },
) {
  return devEntry({
    loadUserModule: async () => ({
      default: {
        rpc: {
          ping: () => "pong",
        },
      },
    }),
    getEnvDb: () => envDb as never,
    logger: { log: () => {}, error: () => {} },
    getDevAuthEnv: () => undefined,
  });
}

afterEach(() => {
  delete g.__zsRuntimeDescriptor;
});

describe("devEntry runtime descriptor install", () => {
  test("no descriptor stays schema-less and dispatches normally", async () => {
    const entry = makeDevEntry();
    const out = await Promise.resolve(entry.rpc("ping", null, {}));
    assert.equal(out, "pong");
  });

  test("a present empty descriptor is an invalid descriptor, not schema-less", async () => {
    g.__zsRuntimeDescriptor = {};
    const entry = makeDevEntry();

    await assert.rejects(
      () => Promise.resolve(entry.rpc("ping", null, {})),
      /invalid RuntimeSchemaDescriptor|expected v2 object/,
    );
  });

  test("a present non-v2 descriptor rejected by installSchema hard-errors", async () => {
    g.__zsRuntimeDescriptor = {
      version: 2,
      collections: {
        notes: {
          fields: {
            title: { type: "string" },
          },
          options: { softDelete: false, versioning: false },
          indexes: [
            { name: "by_title", fields: [123] },
          ],
        },
      },
    };
    const entry = makeDevEntry();

    await assert.rejects(
      () => Promise.resolve(entry.rpc("ping", null, {})),
      /invalid RuntimeSchemaDescriptor|indexes\[0\]/,
    );
  });

  test("preserves __proto__ as a collection name", async () => {
    const collections = Object.create(null) as Record<string, unknown>;
    collections.__proto__ = {
      fields: { id: { type: "string", required: true, primaryKey: true } },
      options: { softDelete: false, versioning: false, strictness: "strict" },
      indexes: [],
    };
    g.__zsRuntimeDescriptor = { version: 2, collections };
    const envDb = {
      transaction: (cb: (raw: unknown) => unknown) => cb(undefined),
      collection: () => ({ find: async () => [] }),
    } as Record<string, unknown>;

    const entry = makeDevEntry(envDb);
    const out = await Promise.resolve(entry.rpc("ping", null, {}));

    assert.equal(out, "pong");
    const collection = envDb.collection as (name: string) => { find(): Promise<unknown[]> };
    assert.deepEqual(await collection("__proto__").find(), []);
    assert.equal(Object.hasOwn(envDb, "__proto__"), false);
  });
});
