/**
 * Tests for the synthetic SSR entry generator.
 *
 * After the WinterCG-symmetric refactor, the plugin owns one virtual
 * module — `virtual:zeroship/_server-entry` — that imports the user
 * module's namespace, builds a `_procedures` map at module-init time
 * by iterating the namespace's callable exports, and exposes
 * `default.{fetch, rpc}`. There is no separate registry virtual module,
 * no `_zsRegister` runtime side-effect, no static per-procedure imports.
 *
 * These tests verify:
 *   - resolveId returns the resolved id for the synthetic entry
 *   - load emits a source string with the right shape
 *   - the source is structurally clean (no leftover `_zsRegister`,
 *     `__zsRegister`, or `_rpc-registry` references)
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import {
  rpcRegistryPlugin,
  buildServerEntrySource,
  pickEntryWireId,
  SERVER_ENTRY_VIRTUAL_ID,
  SERVER_ENTRY_RESOLVED_ID,
} from "../src/rpc-registry.js";
import type { TransformState } from "../src/transform.js";

function emptyState(): TransformState {
  return { serverFunctionMap: new Map(), discoveredProcedures: [] };
}

describe("rpcRegistryPlugin — resolveId / load", () => {
  function callResolveId(plugin: ReturnType<typeof rpcRegistryPlugin>, id: string): unknown {
    const fn = plugin.resolveId as (id: string) => unknown;
    return fn.call(plugin, id);
  }
  function callLoad(plugin: ReturnType<typeof rpcRegistryPlugin>, id: string): unknown {
    const fn = plugin.load as (id: string) => unknown;
    return fn.call(plugin, id);
  }

  test("resolveId returns the resolved id for the synthetic entry id", () => {
    const plugin = rpcRegistryPlugin({
      root: "/tmp",
      userEntryRel: "src/server.ts",
      state: emptyState(),
    });
    assert.equal(
      callResolveId(plugin, SERVER_ENTRY_VIRTUAL_ID),
      SERVER_ENTRY_RESOLVED_ID,
    );
  });

  test("resolveId returns null for unrelated ids", () => {
    const plugin = rpcRegistryPlugin({
      root: "/tmp",
      userEntryRel: "src/server.ts",
      state: emptyState(),
    });
    assert.equal(callResolveId(plugin, "react"), null);
    assert.equal(callResolveId(plugin, "virtual:zeroship/_rpc-registry"), null);
  });

  test("load returns synthetic entry source importing the user module", () => {
    const plugin = rpcRegistryPlugin({
      root: "/proj",
      userEntryRel: "/proj/src/server.ts",
      state: emptyState(),
    });
    const code = callLoad(plugin, SERVER_ENTRY_RESOLVED_ID) as string;
    assert.equal(typeof code, "string");
    assert.match(code, /import \* as _zsUser from "\/proj\/src\/server\.ts"/);
    assert.match(code, /const _procedures = \{\}/);
    assert.match(code, /export default \{ fetch: _zsFetch, rpc: _zsRpc \}/);
  });

  test("load returns null for unrelated ids", () => {
    const plugin = rpcRegistryPlugin({
      root: "/tmp",
      userEntryRel: "src/server.ts",
      state: emptyState(),
    });
    assert.equal(callLoad(plugin, "some/other/module"), null);
  });
});

describe("buildServerEntrySource — runtime _procedures population", () => {
  test("emits a runtime loop that walks _zsUser's namespace", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    // The dispatch table is populated at module-init time from the user
    // module's namespace, not from a static build-time import list.
    assert.match(code, /for \(const _k of Object\.keys\(_zsUser\)\)/);
    assert.match(code, /typeof _v !== "function"/);
    // wireId resolution: explicit fn.config.id wins, default is the
    // export name. The runtime loop should encode that.
    assert.match(code, /_v\.config && typeof _v\.config\.id === "string"/);
  });

  test("source contains no _zsRegister / __zsRegister / _rpc-registry references", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    assert.ok(
      !code.includes("_zsRegister"),
      "source must not reference _zsRegister",
    );
    assert.ok(
      !code.includes("__zsRegister"),
      "source must not reference __zsRegister",
    );
    assert.ok(
      !code.includes("_rpc-registry"),
      "source must not reference _rpc-registry",
    );
  });

  test("default export is { fetch, rpc } (WinterCG-symmetric)", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    assert.match(code, /export default \{ fetch: _zsFetch, rpc: _zsRpc \}/);
  });
});

describe("pickEntryWireId — resolution order", () => {
  test("explicit fn.config.id wins", () => {
    assert.equal(
      pickEntryWireId({ exportName: "addPost", config: { id: "posts.add" } }),
      "posts.add",
    );
  });

  test("falls back to bare exportName when no config.id", () => {
    assert.equal(pickEntryWireId({ exportName: "listTodos" }), "listTodos");
  });

  test("ignores empty-string config.id (uses default)", () => {
    assert.equal(
      pickEntryWireId({ exportName: "x", config: { id: "" } }),
      "x",
    );
  });

  test("ignores non-string config.id (uses default)", () => {
    assert.equal(
      pickEntryWireId({ exportName: "x", config: { id: 42 as unknown as string } }),
      "x",
    );
  });
});
