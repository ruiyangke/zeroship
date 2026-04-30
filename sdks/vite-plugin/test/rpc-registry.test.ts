/**
 * Tests for the closure-private RPC registry virtual module.
 *
 * Replaces the old `globalThis.__zsRegistry` + `globalThis.__register`
 * leak. The new design owns the registry inside a virtual module owned
 * by the rolldown bundle. After build, no `__zsRegistry` / `__register`
 * symbol exists on globalThis.
 *
 * The plugin under test is the `rpcRegistryPlugin` factory from
 * `../src/rpc-registry.js` — it provides:
 *   - `virtual:zeroship/_rpc-registry`     closure-private registry
 *   - `virtual:zeroship/_server-entry`     synthetic SSR entry that
 *                                          re-exports user + dispatchRpc
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import {
  RPC_REGISTRY_SOURCE,
  RPC_REGISTRY_VIRTUAL_ID,
  RPC_REGISTRY_RESOLVED_ID,
  rpcRegistryPlugin,
} from "../src/rpc-registry.js";

describe("rpcRegistryPlugin", () => {
  function callResolveId(plugin: ReturnType<typeof rpcRegistryPlugin>, id: string): unknown {
    const fn = plugin.resolveId as (id: string) => unknown;
    return fn.call(plugin, id);
  }
  function callLoad(plugin: ReturnType<typeof rpcRegistryPlugin>, id: string): unknown {
    const fn = plugin.load as (id: string) => unknown;
    return fn.call(plugin, id);
  }

  test("resolveId returns the resolved id for the public specifier", () => {
    const plugin = rpcRegistryPlugin({ userEntryRel: "src/server.ts" });
    assert.equal(
      callResolveId(plugin, RPC_REGISTRY_VIRTUAL_ID),
      RPC_REGISTRY_RESOLVED_ID,
      "resolveId returns \\0-prefixed id"
    );
  });

  test("resolveId returns null for unrelated ids", () => {
    const plugin = rpcRegistryPlugin({ userEntryRel: "src/server.ts" });
    assert.equal(callResolveId(plugin, "some/other/module"), null);
    assert.equal(callResolveId(plugin, "react"), null);
  });

  test("load returns ESM source declaring _zsRegister and dispatch", () => {
    const plugin = rpcRegistryPlugin({ userEntryRel: "src/server.ts" });
    const code = callLoad(plugin, RPC_REGISTRY_RESOLVED_ID);
    assert.equal(typeof code, "string", "load returns string");
    const codeStr = code as string;
    assert.match(codeStr, /export function _zsRegister/, "exports _zsRegister");
    assert.match(codeStr, /export async function dispatch/, "exports dispatch");
  });

  test("registry is closure-private (no _registry export, no globalThis access)", async () => {
    // Data-URL import the source — confirms it's syntactically valid ESM,
    // _registry is not exposed via the namespace, and _zsRegister + dispatch
    // round-trip a registered method correctly.
    const dataUrl =
      "data:text/javascript;base64," +
      Buffer.from(RPC_REGISTRY_SOURCE, "utf8").toString("base64");
    const mod: any = await import(dataUrl);
    assert.equal(typeof mod._zsRegister, "function", "_zsRegister exported");
    assert.equal(typeof mod.dispatch, "function", "dispatch exported");
    assert.equal(mod._registry, undefined, "_registry is NOT exported");

    // Round-trip: register and dispatch.
    mod._zsRegister("doubleIt", (n: number) => n * 2);
    const result = await mod.dispatch("doubleIt", [21]);
    assert.equal(result, 42, "registered fn dispatched correctly");
  });

  test("dispatch on missing method rejects with err.status === 404", async () => {
    const dataUrl =
      "data:text/javascript;base64," +
      Buffer.from(RPC_REGISTRY_SOURCE, "utf8").toString("base64");
    const mod: any = await import(dataUrl);
    await assert.rejects(
      () => mod.dispatch("doesNotExist", []),
      (err: any) => {
        assert.equal(err.status, 404, "err.status is 404");
        assert.match(String(err.message), /Method not found/, "error message");
        return true;
      }
    );
  });

  test("source contains no globalThis or __zsRegistry references", () => {
    assert.ok(
      !RPC_REGISTRY_SOURCE.includes("globalThis"),
      "source has no `globalThis` reference"
    );
    assert.ok(
      !RPC_REGISTRY_SOURCE.includes("__zsRegistry"),
      "source has no `__zsRegistry` reference"
    );
  });
});
