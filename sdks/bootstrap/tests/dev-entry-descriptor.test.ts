import { afterEach, describe, test } from "node:test";
import assert from "node:assert/strict";

import { devEntry } from "../src/dev-entry.js";

const g = globalThis as typeof globalThis & {
  __zsRuntimeDescriptor?: unknown;
};

function makeDevEntry() {
  return devEntry({
    loadUserModule: async () => ({
      default: {
        rpc: {
          ping: () => "pong",
        },
      },
    }),
    getEnvDb: () => ({
      registerModel: () => Promise.resolve(),
      transaction: (cb: (raw: unknown) => unknown) => cb(undefined),
      collection: () => ({ find: async () => [] }),
    }),
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
      /invalid RuntimeSchemaDescriptor|expected v1 object/,
    );
  });

  test("a present v1-ish descriptor rejected by installSchema hard-errors", async () => {
    g.__zsRuntimeDescriptor = {
      version: 1,
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
});
