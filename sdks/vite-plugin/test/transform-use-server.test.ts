/**
 * ISS-02 — file-level `"use server"` directive + wrapper-marker
 * discovery.
 *
 * A file is a server module iff its first non-comment statement is
 * the string-literal expression `"use server"`. Inside a server
 * module, only exports whose initializer is a call to one of the
 * wrapper markers (`procedure`/`query`/`mutation`/`stream`/
 * `subscription`, imported from `@zeroship/server` or `@zeroship/rpc`)
 * is registered as an RPC. Every other declaration — `export
 * function`, `export const x = 5`, plain async arrow exports —
 * stays private to the server bundle.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { transformPlugin, type TransformState } from "../src/transform.js";
import { parse as acornParse } from "acorn";

function makeCtx(envName: string) {
  return {
    environment: { name: envName },
    warnings: [] as string[],
    parse(code: string, _opts: { lang?: string }) {
      return acornParse(code, {
        ecmaVersion: 2024,
        sourceType: "module",
        allowImportExportEverywhere: true,
      });
    },
    warn(msg: string) {
      this.warnings.push(msg);
    },
  };
}

function makeState(): TransformState {
  return { serverFunctionMap: new Map(), discoveredProcedures: [] };
}

function getHandler(plugin: ReturnType<typeof transformPlugin>): any {
  return typeof (plugin.transform as any) === "function"
    ? (plugin.transform as any)
    : (plugin.transform as any).handler;
}

describe('"use server" + wrappers — discovery', () => {
  test("wrapped exports are discovered; helpers are NOT", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { procedure, query, mutation } from "@zeroship/server";

// Helpers — must remain private to the server bundle.
function _internalSink(x) { return x.toUpperCase(); }
export async function buildEmail(name) { return _internalSink(name); }
export const PI = 3.14;
export const helperArrow = async () => 42;

// Real procedures — opt in via wrappers.
export const list = query(async () => [{ id: 1 }]);
export const greet = procedure(async (name) => "hi " + name);
export const create = mutation(async (input) => input, { id: "create.v2" });
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/api.ts");

    const exportNames = state.discoveredProcedures.map((p) => p.exportName).sort();
    assert.deepEqual(
      exportNames,
      ["create", "greet", "list"],
      "only wrapped exports are discovered (helpers stay private)",
    );

    // Verify kind from wrapper.
    const byName = new Map(state.discoveredProcedures.map((p) => [p.exportName, p]));
    assert.equal(byName.get("list")?.kind, "query");
    assert.equal(byName.get("create")?.kind, "mutation");
    // procedure() defers to inferKind → "greet" matches no query prefix → mutation.
    assert.equal(byName.get("greet")?.kind, "mutation");

    // Verify wrapper config still flows through (the second wrapper arg).
    assert.equal(byName.get("create")?.config?.id, "create.v2");
  });

  test("aliased import: `query as q` works", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { query as q, mutation as m } from "@zeroship/server";

export const listFoo = q(async () => []);
export const addFoo = m(async (x) => x);
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/api.ts");

    assert.equal(state.discoveredProcedures.length, 2);
    const byName = new Map(state.discoveredProcedures.map((p) => [p.exportName, p.kind]));
    assert.equal(byName.get("listFoo"), "query");
    assert.equal(byName.get("addFoo"), "mutation");
  });

  test("import from `@zeroship/rpc` (alternate source) is recognized too", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { procedure } from "@zeroship/rpc";
export const ping = procedure(async () => "pong");
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/api.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(state.discoveredProcedures[0].exportName, "ping");
  });

  test("import from a random package is NOT recognized as a marker", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    // `procedure` imported from somewhere else is just a function call,
    // not a marker. The export does NOT become an RPC.
    const code = `"use server";
import { procedure } from "some-other-lib";
export const ping = procedure(async () => "pong");
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/api.ts");

    assert.equal(
      state.discoveredProcedures.length,
      0,
      "wrapper from unknown package is ignored",
    );
  });

  test("namespace-import (`* as zs`) is NOT recognized — must be a named import", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import * as zs from "@zeroship/server";
export const ping = zs.procedure(async () => "pong");
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/api.ts");

    // `zs.procedure(...)` is a MemberExpression callee — not bare
    // identifier — so the static symbol table doesn't match.
    assert.equal(
      state.discoveredProcedures.length,
      0,
      "namespace-imported wrapper not matched",
    );
  });

  test("stream() implies isStream: true even when handler isn't a generator", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { stream } from "@zeroship/server";
// Plain async fn that returns an iterator — wrapper still tags as stream.
export const drip = stream(async () => makeIterator());
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/api.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(state.discoveredProcedures[0].kind, "stream");
    assert.equal(state.discoveredProcedures[0].isStream, true);
  });
});
