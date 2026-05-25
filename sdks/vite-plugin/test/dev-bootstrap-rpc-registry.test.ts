import { describe, test } from "node:test";
import assert from "node:assert/strict";

import "../../bootstrap/src/dispatcher.js";
import { createDevRpcRegistry } from "../src/dev-bootstrap/rpc-registry.js";

type DispatchFn = (
  rpcDict: Record<string, unknown>,
  name: string,
  input: unknown,
  ctx: unknown,
) => Promise<unknown>;

async function dispatch(
  registry: ReturnType<typeof createDevRpcRegistry>["registry"],
  name: string,
): Promise<unknown> {
  const fn = (globalThis as { __zsDispatch?: DispatchFn }).__zsDispatch;
  assert.equal(typeof fn, "function", "__zsDispatch must be installed");
  return fn!(Object.fromEntries(registry), name, null, {});
}

describe("dev RPC registry", () => {
  test("module replacement drops stale wire ids after a rename", async () => {
    const rpc = createDevRpcRegistry();
    const file = "/app/src/server.ts";

    rpc.replaceModule(file, {
      oldName: async () => "old",
    });
    assert.equal(await dispatch(rpc.registry, "oldName"), "old");

    rpc.replaceModule(file, {
      newName: async () => "new",
    });

    await assert.rejects(
      () => dispatch(rpc.registry, "oldName"),
      (err: unknown) =>
        typeof err === "object" &&
        err !== null &&
        (err as { code?: string }).code === "NOT_FOUND",
    );
    assert.equal(await dispatch(rpc.registry, "newName"), "new");
  });

  test("module pruning makes deleted handlers stop resolving", async () => {
    const rpc = createDevRpcRegistry();
    const file = "/app/src/server.ts";

    rpc.replaceModule(file, {
      removedLater: async () => "live",
    });
    assert.equal(await dispatch(rpc.registry, "removedLater"), "live");

    rpc.pruneModule(file);

    await assert.rejects(
      () => dispatch(rpc.registry, "removedLater"),
      (err: unknown) =>
        typeof err === "object" &&
        err !== null &&
        (err as { code?: string }).code === "NOT_FOUND",
    );
  });
});
