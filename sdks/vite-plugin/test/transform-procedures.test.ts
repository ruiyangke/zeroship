/**
 * Phase 1 — transform stashes per-procedure metadata.
 *
 * Server-module discovery is **purely path-based**: a file is a server
 * module iff it lives at `src/server.{ts,tsx,js,jsx}` (single-file
 * convention) or anywhere under `src/server/**` (directory convention).
 * The legacy `"use server"` directive is no longer accepted — files
 * outside the path convention pass through untouched even if they
 * declare `"use server"` at the top.
 *
 * For every discovered procedure the transform records:
 *
 *   - filePath, exportName, moduleSlug
 *   - kind (inferred from name, or 'stream' for async generators,
 *     or pulled from .config.kind if explicit)
 *   - isStream (async-generator?)
 *   - config (parsed object literal of `<fnName>.config = {...}`)
 *   - moduleConfig (parsed object literal of module-level `$config`)
 *
 * The data lives on `TransformState.discoveredProcedures`, a list the
 * manifest emitter consumes at closeBundle.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { transformPlugin, type TransformState } from "../src/transform.js";

// Mimic the Rolldown transform context shape — `parse()` is the only
// hook the transform calls. We use the runtime's `oxc` parser to keep
// behavior aligned with production. For these tests we substitute a
// quick-n-dirty acorn-like parse via JSON-schema-validator-of-ts is
// overkill; the simplest path is to invoke the plugin's parse via a
// very small acorn shim. We use Node's built-in `vm` and a minimal
// ESTree builder via `acorn`.

import { parse as acornParse } from "acorn";

// Minimal context that mimics the Vite/Rolldown plugin invocation.
function makeCtx(envName: string) {
  return {
    environment: { name: envName },
    parse(code: string, _opts: { lang?: string }) {
      return acornParse(code, {
        ecmaVersion: 2024,
        sourceType: "module",
        allowImportExportEverywhere: true,
      });
    },
  };
}

function makeState(): TransformState {
  return {
    serverFunctionMap: new Map(),
    discoveredProcedures: [],
  };
}

describe("transform — procedure metadata", () => {
  test("captures kind from `.config = { kind: 'mutation' }`", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    // Wire up resolved root via configResolved
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
export async function add(input) { return input; }
add.config = { kind: "mutation", idempotent: true };
`;
    // Cast handler — depending on Vite version it can be an object or fn.
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server/todos.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    const p = state.discoveredProcedures[0];
    assert.equal(p.exportName, "add");
    assert.equal(p.kind, "mutation");
    assert.equal(p.config?.idempotent, true);
  });

  test("infers kind: 'query' when name matches list/get/find/...", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
export async function listTodos() { return []; }
export async function getUser(id) { return { id }; }
export async function searchPosts() { return []; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server/api.ts");

    assert.equal(state.discoveredProcedures.length, 3);
    const kinds = state.discoveredProcedures.map((p) => p.kind);
    assert.deepEqual(kinds, ["query", "query", "query"]);
  });

  test("kind: 'stream' for async generators (function*)", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
export async function* logStream() { yield 1; yield 2; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server/x.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(state.discoveredProcedures[0].kind, "stream");
    assert.equal(state.discoveredProcedures[0].isStream, true);
  });

  test("captures module-level $config", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
export const $config = { auth: "user", rateLimit: { rpm: 600, per: "user" } };
export async function listTodos() { return []; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server/x.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    const p = state.discoveredProcedures[0];
    assert.equal(p.moduleConfig?.auth, "user");
    assert.deepEqual(p.moduleConfig?.rateLimit, { rpm: 600, per: "user" });
  });

  test("accepts fn.config.input/output as arbitrary expressions (Zod call)", () => {
    // The literalize() helper used to bail out on call expressions
    // anywhere in `fn.config = { ... }`, which made every other
    // config field disappear too. After the Zod-direct switch, the
    // top-level keys `input` and `output` are recognized as
    // schema declarations; their values are stored as a sentinel
    // marker so the rest of the literal still parses cleanly.
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
export async function listTodos(input) { return []; }
listTodos.config = {
  id: "listTodos",
  idempotent: true,
  input: z.object({ limit: z.number().optional() }),
  output: z.array(z.object({ id: z.string() })),
};
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server/todos.ts");

    assert.equal(state.discoveredProcedures.length, 1, "procedure recorded");
    const p = state.discoveredProcedures[0];
    // Other config fields survived even with Zod call expressions present.
    assert.equal(p.config?.id, "listTodos", "id preserved");
    assert.equal(p.config?.idempotent, true, "idempotent preserved");
    // input and output keys are present as opaque markers, not the AST.
    assert.ok(p.config?.input, "input key present");
    assert.ok(p.config?.output, "output key present");
    assert.equal(
      (p.config?.input as Record<string, unknown>).__zsSchema,
      true,
      "input is the schema marker (not the call expression)",
    );
    assert.equal(
      (p.config?.output as Record<string, unknown>).__zsSchema,
      true,
      "output is the schema marker",
    );
  });

  test("derives moduleSlug as `<dir-segments-joined-by-dash>-<basename>`", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
export async function list() { return []; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server/todos.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(state.discoveredProcedures[0].moduleSlug, "src-server-todos");
  });

  // ── Path-based discovery (replaces the legacy "use server" directive) ────
  //
  // A file is a server module iff its path matches one of:
  //   - `src/server.{ts,tsx,js,jsx}`     single-file flat layout
  //   - `src/server/**/*.{ts,tsx,js,jsx}` directory layout
  //
  // Anything outside that path is a client module and the transform
  // passes it through untouched, even when the file declares
  // `"use server"` at the top. The directive is no longer a marker.

  test("path-based: src/server.ts (single-file) IS a server module", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
export async function ping() { return "pong"; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server.ts");

    assert.equal(state.discoveredProcedures.length, 1, "ping discovered");
    assert.equal(state.discoveredProcedures[0].exportName, "ping");
  });

  test("path-based: src/server/foo/bar.ts IS a server module", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
export async function deeplyNested() { return 42; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server/foo/bar.ts");

    assert.equal(state.discoveredProcedures.length, 1, "discovered nested");
    assert.equal(state.discoveredProcedures[0].exportName, "deeplyNested");
  });

  test("path-based: src/utils/helpers.ts is NOT a server module (no discovery)", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    // Even with a "use server" directive (the v1 marker), this file is
    // not in src/server/** — the v2 transform refuses to discover it.
    const code = `"use server";
export async function shouldNotBeDiscovered() { return 1; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    const result = handler.call(ctx, code, "/r/src/utils/helpers.ts");

    assert.equal(state.discoveredProcedures.length, 0, "nothing discovered");
    assert.equal(result, null, "transform passes through (returns null)");
  });

  test("path-based: src/client/Component.tsx is NOT a server module", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
export async function pretendingToBeServer() { return 1; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    const result = handler.call(ctx, code, "/r/src/client/Component.tsx");

    assert.equal(state.discoveredProcedures.length, 0);
    assert.equal(result, null, "client component passes through");
  });

  test("path-based: ignores leading `\"use server\"` directive (no longer a marker)", () => {
    // A file inside src/server/** with a redundant "use server" directive
    // still works — the directive is just dead bytes the build strips.
    // But a file OUTSIDE src/server/** with the directive is not picked up.
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `"use server";
export async function add(input) { return input; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server/api.ts");

    // The procedure is still discovered because the file lives in
    // src/server/** — but the directive itself was ignored as a marker.
    assert.equal(state.discoveredProcedures.length, 1, "still discovered by path");
    assert.equal(state.discoveredProcedures[0].exportName, "add");
  });
});
