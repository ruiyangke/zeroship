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

describe("buildServerEntrySource — Stage 4 schema discovery moved to runtime", () => {
  // Stage 4 of the schema auto-discovery refactor moved the IIFE that
  // used to live in the synthetic SSR entry into the runtime bootstrap
  // (`crates/runtime/src/bootstrap/db_init.js`). The synthetic entry
  // emits NO schema-side glue at all. These tests are the negative
  // assertions that lock in the cleanup — every string that used to be
  // a positive shape assertion is now a forbidden substring.

  const PHASE_2_BINDING = new Map([
    [
      "/proj/src/server.ts::ping",
      {
        wireId: "ping",
        sourceFile: "/proj/src/server.ts",
        exportName: "ping",
        kind: "query" as const,
        marker: "file" as const,
        chain: ["/proj/src/server.ts"],
      },
    ],
  ]);

  test("namespace-walk entry contains no schema references", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    // The IIFE and all its support strings must be gone.
    assert.ok(!code.includes("_installSchema"),
      "_installSchema must not appear in the generated entry (runtime owns discovery now)");
    assert.ok(!code.includes("__zsSchemaInit"),
      "__zsSchemaInit global must not appear (runtime serializes via top-level await)");
    assert.ok(!code.includes("_zsSchemaMod"),
      "no synthetic schema-import alias should be emitted");
    assert.ok(!code.includes("@zeroship/db"),
      "synthetic entry must not statically OR dynamically import @zeroship/db");
    assert.ok(!code.includes("installOnEnvDb"),
      "no _installSchema option payload should appear");
    assert.ok(!code.includes("_zsUser.default.schema"),
      "no schema-read off _zsUser.default — runtime reads user.default.schema in bootstrap scope");
  });

  test("Phase-2 (binding-fed) entry contains no schema references", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: PHASE_2_BINDING,
    });
    assert.ok(!code.includes("_installSchema"));
    assert.ok(!code.includes("__zsSchemaInit"));
    assert.ok(!code.includes("_zsSchemaMod"));
    assert.ok(!code.includes("@zeroship/db"));
    assert.ok(!code.includes("installOnEnvDb"));
    assert.ok(!code.includes("_zsUser.default.schema"));
  });

  test("generated source parses as valid ESM", async () => {
    // A syntactic regression here would break every user's build, so
    // validate via acorn.
    const { parse: acornParse } = await import("acorn");
    for (const bindings of [undefined, PHASE_2_BINDING] as const) {
      const code = buildServerEntrySource({
        userEntryRel: "/proj/src/server.ts",
        bindings,
      });
      acornParse(code, {
        ecmaVersion: 2024,
        sourceType: "module",
        allowImportExportEverywhere: true,
      });
    }
  });

  test("auto-tx wrapper still awaits __zeroshipPlatformReady (defense-in-depth)", () => {
    // Stage 4 removes the `__zsSchemaInit` await but keeps
    // `__zeroshipPlatformReady` — the SDK chains it inside
    // `_installSchema` and pglite-socket's per-connection-in-tx
    // serialisation means we must await registerModel's DDL chain
    // BEFORE opening BEGIN. The runtime bootstrap settles the chain
    // before the kernel resolves `default.fetch`, so this is a
    // warm-path no-op — but the await guards future embeddings that
    // skip the bootstrap.
    const readyRe = /globalThis(?:\.__zeroshipPlatformReady|\["__zeroshipPlatformReady"\])/;
    for (const bindings of [undefined, PHASE_2_BINDING] as const) {
      const code = buildServerEntrySource({
        userEntryRel: "/proj/src/server.ts",
        bindings,
      });
      assert.match(code, readyRe);
      // And there must be NO __zsSchemaInit read alongside it.
      assert.ok(!/__zsSchemaInit/.test(code));
    }
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
