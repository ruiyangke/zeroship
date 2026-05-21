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

describe("buildServerEntrySource — Stage 2 schema auto-registration", () => {
  test("entry-default path (no schemaImportSpec): no extra import, falls back to _zsUser.default.schema", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    // No extra static `import * as _zsSchemaMod` line.
    assert.equal(
      /import \* as _zsSchemaMod from /.test(code),
      false,
      "no schema-side import when entry-fallback case",
    );
    // The fallback path keys off `_zsUser.default.schema`.
    assert.match(code, /_zsUser\.default\.schema/);
    // The async IIFE that calls `_installSchema` is present.
    assert.match(code, /await import\("@zeroship\/db"\)/);
    assert.match(code, /_installSchema/);
    assert.match(code, /installOnEnvDb:\s*true/);
  });

  test("split-file path (schemaImportSpec set): emits extra import + reads from it first", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      schemaImportSpec: "/proj/src/schema.ts",
    });
    // Extra namespace import for the schema module.
    assert.match(
      code,
      /import \* as _zsSchemaMod from "\/proj\/src\/schema\.ts"/,
    );
    // Resolution prefers _zsSchemaMod, with _zsUser.default.schema as fallback.
    assert.match(code, /_zsSchemaMod\.default\?\.schema/);
    assert.match(code, /_zsUser\.default\.schema/);
    // Calls the registration helper through the dynamic import.
    assert.match(code, /_installSchema/);
    assert.match(code, /installOnEnvDb:\s*true/);
  });

  test("generated source is syntactically valid JS for both schema paths", async () => {
    // Both shapes must parse — a typo in the emitter would break every
    // user's build, so we statically validate via acorn.
    const { parse: acornParse } = await import("acorn");
    for (const spec of [undefined, "/proj/src/schema.ts"]) {
      const code = buildServerEntrySource({
        userEntryRel: "/proj/src/server.ts",
        schemaImportSpec: spec,
      });
      acornParse(code, {
        ecmaVersion: 2024,
        sourceType: "module",
        allowImportExportEverywhere: true,
      });
    }
  });

  test("entry-default path assigns the IIFE to a global so rolldown can't tree-shake it", () => {
    // Regression: when no static schema-side import is emitted, rolldown's
    // static-folding step otherwise concludes the IIFE's side effects
    // (`console.error`, `_zsRegFn(...)` whose return is discarded) are
    // pure and removes the entire block. Anchoring the IIFE's promise to
    // `globalThis.__zsSchemaInit` (either dot or bracket-indexed
    // — both are equivalent for rolldown's side-effect tracking) makes
    // the side-effect visible across the SSR build's module boundary.
    // The sentinel string lives in the SDK's `internal-globals` so a
    // rename surfaces at TS compile time on both ends.
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    assert.match(
      code,
      /globalThis(?:\.__zsSchemaInit|\["__zsSchemaInit"\])\s*=\s*\(async/,
    );
  });

  test("entry-default resolution does not include a statically false guard", () => {
    // If the emitted source contains `typeof undefined !== "undefined"`,
    // rolldown will fold the entire short-circuit chain into the
    // right-hand fallback AND then conclude the whole IIFE is dead. Keep
    // the entry-default emission free of any `${schemaModRef}` references
    // (which become the literal `undefined`).
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    assert.equal(
      /typeof\s+undefined\s*!==/.test(code),
      false,
      "no statically-false guard in the entry-default emission",
    );
  });

  test("generated _zsRpcWithAutoTx awaits __zsSchemaInit BEFORE __zeroshipPlatformReady", () => {
    // Cold-start race regression: in the auto-discovery path
    // `__zeroshipPlatformReady` is set INSIDE `_installSchema`, which itself
    // runs inside the IIFE that publishes `__zsSchemaInit`. If the first
    // request lands before that IIFE's dynamic import resolves,
    // `__zeroshipPlatformReady` is still undefined and the await is a no-op
    // — letting auto-tx open against an unregistered schema. The fix is to
    // await `__zsSchemaInit` first. Verify the ordering in both the
    // namespace-walk (runtime) entry AND the Phase-2 binding-fed entry.
    // Match either `globalThis.__zsSchemaInit` (dot) or
    // `globalThis["__zsSchemaInit"]` (bracket) — the generator switched
    // to bracket-indexed reads to interpolate the SDK's typed sentinel
    // names instead of stale string literals.
    const initRe = /globalThis(?:\.__zsSchemaInit|\["__zsSchemaInit"\]);/;
    const readyRe = /globalThis(?:\.__zeroshipPlatformReady|\["__zeroshipPlatformReady"\]);/;
    for (const spec of [undefined, "/proj/src/schema.ts"] as const) {
      const code = buildServerEntrySource({
        userEntryRel: "/proj/src/server.ts",
        schemaImportSpec: spec,
      });
      const initMatch = code.match(initRe);
      const readyMatch = code.match(readyRe);
      assert.ok(initMatch, "expected `globalThis.__zsSchemaInit;` read");
      assert.ok(readyMatch, "expected `globalThis.__zeroshipPlatformReady;` read");
      const initIdx = initMatch.index ?? -1;
      const readyIdx = readyMatch.index ?? -1;
      assert.ok(
        initIdx > 0 && readyIdx > 0 && initIdx < readyIdx,
        `__zsSchemaInit must be awaited BEFORE __zeroshipPlatformReady (init=${initIdx}, ready=${readyIdx}, spec=${spec})`,
      );
    }

    // The Phase-2 (binding-fed) emission must also order the two awaits.
    const phase2 = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: new Map([
        [
          "/proj/src/server.ts::ping",
          {
            wireId: "ping",
            sourceFile: "/proj/src/server.ts",
            exportName: "ping",
            kind: "query",
            marker: "file",
            chain: ["/proj/src/server.ts"],
          },
        ],
      ]),
    });
    const initMatch2 = phase2.match(initRe);
    const readyMatch2 = phase2.match(readyRe);
    const initIdx2 = initMatch2?.index ?? -1;
    const readyIdx2 = readyMatch2?.index ?? -1;
    assert.ok(initIdx2 > 0 && readyIdx2 > 0 && initIdx2 < readyIdx2,
      "Phase 2 entry must await __zsSchemaInit before __zeroshipPlatformReady");
  });

  test("split-file path in binding-fed (Phase 2) emission emits the schema import + block", () => {
    // Imported lazily to avoid pulling ServerBinding's type machinery
    // into this lightweight test file's hot path.
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      schemaImportSpec: "/proj/src/schema.ts",
      bindings: new Map([
        [
          "/proj/src/server.ts::ping",
          {
            wireId: "ping",
            sourceFile: "/proj/src/server.ts",
            exportName: "ping",
            kind: "query",
            marker: "file",
            chain: ["/proj/src/server.ts"],
          },
        ],
      ]),
    });
    // Phase 2 entry MUST include the same schema-registration block.
    assert.match(
      code,
      /import \* as _zsSchemaMod from "\/proj\/src\/schema\.ts"/,
    );
    assert.match(code, /await import\("@zeroship\/db"\)/);
    assert.match(code, /_installSchema/);
    assert.match(code, /installOnEnvDb:\s*true/);
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
