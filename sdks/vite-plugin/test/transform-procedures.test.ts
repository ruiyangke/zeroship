/**
 * Phase 1 — transform stashes per-procedure metadata.
 *
 * Server-module discovery is via the file-level `"use server"`
 * directive (ISS-02 — the path convention is gone). A file is a server
 * module iff its first non-comment statement is the string-literal
 * expression `"use server"`. Files outside that gate pass through
 * untouched.
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
// hook the transform calls. We use Acorn here so the test's AST shape
// matches what dev-mode Rolldown produces (acorn sets `.directive` on
// directive-prologue ExpressionStatements; the transform's detector
// handles both shapes).
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
    // Capture pluginContext.warn() calls so tests can assert on them.
    warnings: [] as string[],
    warn(msg: string) {
      this.warnings.push(msg);
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
    const code = `"use server";
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
    const code = `"use server";
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
    const code = `"use server";
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
    const code = `"use server";
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
    const code = `"use server";
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
    const code = `"use server";
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

  // ── Directive-based discovery (ISS-02) ────────────────────────────────
  //
  // A file is a server module iff its FIRST non-comment statement is
  // the string-literal expression `"use server"`. The path convention
  // (`src/server.{ts,tsx,js,jsx}`, `src/server/**/*.{ts,...}`) is dead.

  test("directive: top-of-file `\"use server\"` opts the file in", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `"use server";
export async function ping() { return "pong"; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    // Path is irrelevant — even files outside src/server/ are eligible.
    handler.call(ctx, code, "/r/src/api/ping.ts");

    assert.equal(state.discoveredProcedures.length, 1, "ping discovered");
    assert.equal(state.discoveredProcedures[0].exportName, "ping");
  });

  test("directive: works for files anywhere in the project", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `"use server";
export async function deeplyNested() { return 42; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    // Outside src/server/ — discovered solely because of the directive.
    handler.call(ctx, code, "/r/src/lib/api.ts");

    assert.equal(state.discoveredProcedures.length, 1, "discovered by directive");
    assert.equal(state.discoveredProcedures[0].exportName, "deeplyNested");
  });

  test("directive: missing → file passes through (no discovery)", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    // Even at the legacy path, with no directive the file passes through.
    const code = `
export async function shouldNotBeDiscovered() { return 1; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    const result = handler.call(ctx, code, "/r/src/server/api.ts");

    assert.equal(state.discoveredProcedures.length, 0, "nothing discovered");
    assert.equal(result, null, "transform passes through (returns null)");
  });

  test("directive: legacy path without directive emits a migration warning", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
export async function unmigrated() { return 1; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server/api.ts");

    // Friendly migration hint for ISS-02: the file lives at the
    // legacy server-module path but the directive is missing.
    assert.equal(ctx.warnings.length, 1, "one warning emitted");
    assert.match(
      ctx.warnings[0],
      /missing the `"use server"` directive/,
      "warning mentions the directive",
    );
    assert.match(
      ctx.warnings[0],
      /ISS-02/,
      "warning references the issue id",
    );
  });

  test("directive: each legacy-path file warns once even on repeat transforms (HMR)", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `export async function unmigrated() { return 1; }`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/server/api.ts");
    handler.call(ctx, code, "/r/src/server/api.ts");
    handler.call(ctx, code, "/r/src/server/api.ts");

    assert.equal(ctx.warnings.length, 1, "warning de-duped per file path");
  });

  test("directive: NOT a directive when not at body[0] (semicolon, var first)", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    // "use server" appears, but it's NOT body[0] — it follows a var
    // declaration. Per ECMAScript Directive Prologue rules, this is
    // not a directive; the file is a regular client module.
    const code = `
const meaningful = true;
"use server";
export async function nope() { return 1; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    const result = handler.call(ctx, code, "/r/src/lib/x.ts");

    assert.equal(state.discoveredProcedures.length, 0, "not a server module");
    assert.equal(result, null);
  });

  test("directive: leading line + block comments are tolerated before the directive", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `// File header comment
/* multi-line block
   comment */
"use server";
export async function ping() { return "pong"; }
`;
    const handler =
      typeof (plugin.transform as any) === "function"
        ? (plugin.transform as any)
        : (plugin.transform as any).handler;
    handler.call(ctx, code, "/r/src/api/x.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(state.discoveredProcedures[0].exportName, "ping");
  });
});
