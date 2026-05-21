/**
 * Tests for the synthetic SSR entry generator (Stage 5b — ZS-standard).
 *
 * After 5b, the synthetic entry is a NORMALISER. Dispatch (input
 * validation, capability frame, auto-tx, stream framing, output
 * validation) lives in the runtime's `__zsDispatch`
 * (`crates/runtime/src/bootstrap/rpc_dispatch.js`). The plugin only
 * shapes the user module into `default = { schema?, fetch?, rpc? }`
 * where `rpc` is a PLAIN OBJECT (dict-shape).
 *
 * These tests verify:
 *   - resolveId returns the resolved id for the synthetic entry
 *   - load emits a source string with the new normaliser shape
 *   - the source is structurally clean (no leftover dispatcher
 *     helpers, no `_zsRegister`, no `_rpc-registry` references)
 *   - the generated source is syntactically valid ESM (Acorn parse)
 *   - the namespace-walk and Phase-2 shapes are both produced
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { parse as acornParse } from "acorn";

import {
  rpcRegistryPlugin,
  buildServerEntrySource,
  pickEntryWireId,
  SERVER_ENTRY_VIRTUAL_ID,
  SERVER_ENTRY_RESOLVED_ID,
} from "../src/rpc-registry.js";
import type { TransformState } from "../src/transform.js";
import type { ServerBinding } from "../src/server-graph.js";

function emptyState(): TransformState {
  return { serverFunctionMap: new Map(), discoveredProcedures: [] };
}

function bindingMap(rows: Array<Partial<ServerBinding>>): Map<string, ServerBinding> {
  const out = new Map<string, ServerBinding>();
  for (const r of rows) {
    const sf = r.sourceFile!;
    const en = r.exportName!;
    out.set(`${sf}::${en}`, {
      wireId: r.wireId ?? en,
      sourceFile: sf,
      exportName: en,
      kind: r.kind ?? "mutation",
      marker: r.marker ?? "file",
      chain: r.chain ?? [sf],
      ...(r.lazy ? { lazy: r.lazy } : {}),
    });
  }
  return out;
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
    // The new normaliser shape: dict-shape `default.rpc` + schema +
    // fetch keys on the default object.
    assert.match(code, /export default \{/);
    assert.match(code, /schema:\s*_zsUserDefault\.schema/);
    assert.match(code, /rpc:\s*_zsRpc/);
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

describe("buildServerEntrySource — dict-shape normaliser (namespace-walk)", () => {
  test("emits a runtime loop that walks _zsUser's namespace into _zsRpc", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    // The dispatch dict is built at module-init time from the user
    // module's namespace exports.
    assert.match(code, /for \(const _zsName of Object\.keys\(_zsUser\)\)/);
    assert.match(code, /typeof _zsFn !== "function"/);
    // wireId resolution: explicit fn.config.id wins, default = export name.
    assert.match(code, /_zsFn\.config && typeof _zsFn\.config\.id === "string"/);
  });

  test("merges user-supplied dict-shape default.rpc with named exports", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    // The user's own `default.rpc` (when an object) is the base; named
    // exports are layered ON TOP. Named-export procedures win on key
    // conflict because they're the canonical source-level declaration.
    assert.match(code, /typeof _zsUserDefault\.rpc === "object"/);
    assert.match(code, /\{\s*\.\.\._zsUserDefault\.rpc\s*\}/);
  });

  test("fetch resolution: default.fetch wins, falls through to top-level export", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    assert.match(code, /typeof _zsUserDefault\.fetch === "function"/);
    assert.match(code, /typeof _zsUser\.fetch === "function"/);
  });

  test("default export shape: { schema, fetch, rpc }", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    // Dict-shape — `rpc` is the _zsRpc OBJECT, not a function call.
    assert.match(code, /schema:\s*_zsUserDefault\.schema/);
    assert.match(code, /fetch:\s*_zsFetch/);
    assert.match(code, /rpc:\s*_zsRpc/);
  });

  test("default.rpc is an object literal, NOT a function expression", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
    });
    // The export must reference the _zsRpc identifier (an object),
    // not a function expression / arrow / call result.
    const exportMatch = code.match(/export default \{[\s\S]*?\};/);
    assert.ok(exportMatch, "must have an export default object literal");
    const block = exportMatch![0];
    // No `function`, no `=>`, no `(...)` immediately after `rpc:`.
    assert.doesNotMatch(block, /rpc:\s*function/);
    assert.doesNotMatch(block, /rpc:\s*\(.*\)\s*=>/);
    assert.doesNotMatch(block, /rpc:\s*async\s/);
  });
});

describe("buildServerEntrySource — forbidden helpers (Stage 5b cleanup)", () => {
  const PHASE_2_BINDING = bindingMap([
    { sourceFile: "/proj/src/server.ts", exportName: "ping", wireId: "ping", kind: "query" },
  ]);

  // The synthetic entry no longer generates dispatch helpers — those
  // moved to the runtime's __zsDispatch (Stage 5a). Every shape below
  // must be ABSENT from the generated source.
  //
  // Note: `_zsFetch` survives as the NAME of the resolved-fetch local
  // (e.g. `const _zsFetch = ...`). The old _zsFetch was a HELPER
  // FUNCTION; we forbid the function-definition form instead.
  const FORBIDDEN = [
    "_zsRpc(",                      // The old function-shape dispatcher call
    "_zsRpcWithAutoTx",
    "_zsRpcPost",
    "_zsRpcAndRespond",
    "function _zsFetch",            // Old WinterCG fall-through helper
    "__zsEnterKind",
    "__zsExitKind",
    "__zsBeginAutoTx",
    "__zsEndAutoTx",
    "_isAsyncIterator",
    "_isParseable",
    "_zodIssues",
    "_isZodStringSchema",
    "_procedures",
    "_zsErrResponse",
    "AI-SDK",                       // No SSE framing comment leftovers
    "Vercel AI-SDK Data Stream",
    "_zsRegister",
    "__zsRegister",
    "_rpc-registry",
    "_installSchema",               // Stage-6 legacy (also forbid the new name)
    "installSchema",
    "__zsSchemaInit",
    "_zsSchemaMod",
    "@zeroship/db",                 // No SDK imports in the entry
    "installOnEnvDb",
  ];

  for (const variant of [
    { label: "namespace-walk", bindings: undefined as Map<string, ServerBinding> | undefined },
    { label: "Phase-2 (binding-fed)", bindings: PHASE_2_BINDING },
  ]) {
    test(`${variant.label}: no leftover dispatch / schema / registry helpers`, () => {
      const code = buildServerEntrySource({
        userEntryRel: "/proj/src/server.ts",
        bindings: variant.bindings,
      });
      for (const needle of FORBIDDEN) {
        assert.ok(
          !code.includes(needle),
          `source must not reference ${needle} (${variant.label})\n--- generated ---\n${code}`,
        );
      }
    });
  }

  test("generated source parses as valid ESM (Acorn) — namespace-walk + Phase-2", () => {
    for (const bindings of [undefined, PHASE_2_BINDING] as const) {
      const code = buildServerEntrySource({
        userEntryRel: "/proj/src/server.ts",
        bindings,
      });
      // Throws on syntax error.
      acornParse(code, {
        ecmaVersion: 2024,
        sourceType: "module",
        allowImportExportEverywhere: true,
      });
    }
  });
});

describe("buildServerEntrySource — Phase-2 (binding-fed) shape", () => {
  test("empty bindings: falls back to namespace-walk shape", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: new Map(),
    });
    assert.match(code, /for \(const _zsName of Object\.keys\(_zsUser\)\)/);
  });

  test("single file, two procedures: one namespace import + dict literal", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/actions/todos.ts", exportName: "list", wireId: "todos.list", kind: "query" },
        { sourceFile: "/proj/src/actions/todos.ts", exportName: "add",  wireId: "todos.add",  kind: "mutation" },
      ]),
    });
    const imports = code.match(/import \* as _user_TARGET_\d+_ from /g) ?? [];
    assert.equal(imports.length, 1);
    assert.match(code, /"todos\.list":\s*_user_TARGET_0_\.list/);
    assert.match(code, /"todos\.add":\s*_user_TARGET_0_\.add/);
  });

  test("two files: distinct aliases, deterministic (lexicographic) order", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/b.ts", exportName: "b1" },
        { sourceFile: "/proj/src/a.ts", exportName: "a1" },
      ]),
    });
    assert.match(code, /import \* as _user_TARGET_0_ from "\/proj\/src\/a\.ts"/);
    assert.match(code, /import \* as _user_TARGET_1_ from "\/proj\/src\/b\.ts"/);
    assert.match(code, /"a1":\s*_user_TARGET_0_\.a1/);
    assert.match(code, /"b1":\s*_user_TARGET_1_\.b1/);
  });

  test("lazy bindings emit async dynamic-import wrappers", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/lazy.ts", exportName: "heavy", wireId: "heavy", lazy: true },
      ]),
    });
    // Async arrow that dynamic-imports the target and forwards.
    assert.match(
      code,
      /"heavy":\s*async \(input, ctx\) =>\s*\(await import\("\/proj\/src\/lazy\.ts"\)\)\.heavy\(input, ctx\)/,
    );
    // No static import for a lazy-only file.
    assert.doesNotMatch(code, /import \* as _user_TARGET_\d+_ from "\/proj\/src\/lazy\.ts"/);
  });

  test("default export shape on the Phase-2 entry: { schema, fetch, rpc }", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/server.ts", exportName: "ping" },
      ]),
    });
    assert.match(code, /export default \{/);
    assert.match(code, /schema:\s*_zsUserDefault\.schema/);
    assert.match(code, /fetch:\s*_zsFetch/);
    assert.match(code, /rpc:\s*_zsRpc/);
  });

  test("user dict-shape default.rpc merges with binding-derived entries", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/server.ts", exportName: "ping" },
      ]),
    });
    // The generator merges `_zsUserDefault.rpc` first, then layers
    // binding-derived entries on top via Object.assign(_zsRpc, {...}).
    assert.match(code, /typeof _zsUserDefault\.rpc === "object"/);
    assert.match(code, /Object\.assign\(_zsRpc, \{/);
  });
});

describe("buildServerEntrySource — dict-shape end-to-end", () => {
  test("namespace-walk entry: ESM-evaluable normaliser yields a dict", async () => {
    // Materialise the synthetic entry against a stub user module and
    // import it. The default export must carry a dict-shape `rpc`.
    const { mkdtemp, writeFile, rm } = await import("node:fs/promises");
    const { join } = await import("node:path");
    const { tmpdir } = await import("node:os");
    const { pathToFileURL } = await import("node:url");

    const dir = await mkdtemp(join(tmpdir(), "zsrpc-dict-"));
    try {
      const userPath = join(dir, "user.mjs");
      await writeFile(
        userPath,
        `export function ping(input) { return { pong: input }; }
         export const named = Object.assign(
           function (x) { return x * 2; },
           { config: { id: "math.double" } },
         );
         export default { fetch: (req) => new Response("hi"), schema: { todos: {} } };`,
        "utf8",
      );

      const entry = buildServerEntrySource({
        userEntryRel: pathToFileURL(userPath).href,
      });
      const entryPath = join(dir, "entry.mjs");
      await writeFile(entryPath, entry, "utf8");

      const mod = (await import(pathToFileURL(entryPath).href)) as {
        default: { rpc: Record<string, Function>; fetch: Function; schema: unknown };
      };
      const def = mod.default;
      // rpc is a plain object, not a function.
      assert.equal(typeof def.rpc, "object");
      assert.notEqual(typeof def.rpc, "function");
      // Named export keyed by export name.
      assert.equal(typeof def.rpc.ping, "function");
      // fn.config.id wins.
      assert.equal(typeof def.rpc["math.double"], "function");
      // schema surfaces.
      assert.deepEqual(def.schema, { todos: {} });
      // fetch surfaces.
      assert.equal(typeof def.fetch, "function");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("named-export procedure wins over user dict-shape default.rpc on key conflict", async () => {
    const { mkdtemp, writeFile, rm } = await import("node:fs/promises");
    const { join } = await import("node:path");
    const { tmpdir } = await import("node:os");
    const { pathToFileURL } = await import("node:url");

    const dir = await mkdtemp(join(tmpdir(), "zsrpc-merge-"));
    try {
      const userPath = join(dir, "user.mjs");
      await writeFile(
        userPath,
        // Both a named export `dup` AND a default.rpc.dup. The named
        // export must win — it's the canonical source-level declaration.
        `export function dup() { return "named"; }
         export default { rpc: { dup: () => "fromDefault" } };`,
        "utf8",
      );

      const entry = buildServerEntrySource({
        userEntryRel: pathToFileURL(userPath).href,
      });
      const entryPath = join(dir, "entry.mjs");
      await writeFile(entryPath, entry, "utf8");

      const mod = (await import(pathToFileURL(entryPath).href)) as {
        default: { rpc: Record<string, () => string> };
      };
      assert.equal(mod.default.rpc.dup(), "named");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });

  test("user-only default.rpc dict (no named exports) surfaces verbatim", async () => {
    const { mkdtemp, writeFile, rm } = await import("node:fs/promises");
    const { join } = await import("node:path");
    const { tmpdir } = await import("node:os");
    const { pathToFileURL } = await import("node:url");

    const dir = await mkdtemp(join(tmpdir(), "zsrpc-dict-only-"));
    try {
      const userPath = join(dir, "user.mjs");
      await writeFile(
        userPath,
        `export default {
           rpc: {
             a: () => "A",
             b: () => "B",
           },
         };`,
        "utf8",
      );

      const entry = buildServerEntrySource({
        userEntryRel: pathToFileURL(userPath).href,
      });
      const entryPath = join(dir, "entry.mjs");
      await writeFile(entryPath, entry, "utf8");

      const mod = (await import(pathToFileURL(entryPath).href)) as {
        default: { rpc: Record<string, () => string> };
      };
      assert.equal(mod.default.rpc.a(), "A");
      assert.equal(mod.default.rpc.b(), "B");
    } finally {
      await rm(dir, { recursive: true, force: true });
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
