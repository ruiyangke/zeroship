/**
 * ISS-02 — files at the legacy `src/server/...` path that haven't
 * been migrated.
 *
 * Two cases — both yield ZERO discovered procedures (this is the
 * breaking change):
 *
 *   1. Legacy path, NO `"use server"` directive, plain exports.
 *      The transform passes the file through and emits a friendly
 *      migration warning ("missing the `\"use server\"` directive").
 *
 *   2. `"use server"` directive present, but no exports use the
 *      wrapper markers — every export is a plain helper. The
 *      transform processes the file (the directive opted it in) but
 *      finds no procedures.
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

describe("ISS-02 breaking change — unmigrated server modules", () => {
  test("legacy path, no directive, no wrappers → 0 discovered + migration warning", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    const code = `
import { z } from "@zeroship/server";

// What pre-ISS-02 code looked like — every export auto-published as RPC.
export async function listTodos() {
  return [];
}
listTodos.config = { id: "listTodos" };

export async function addTodo(input) {
  return input;
}
addTodo.config = { id: "addTodo" };
`;
    const result = getHandler(plugin).call(ctx, code, "/r/src/server/todos.ts");

    assert.equal(result, null, "transform passes through");
    assert.equal(
      state.discoveredProcedures.length,
      0,
      "no procedures discovered — file isn't a server module without the directive",
    );
    assert.equal(ctx.warnings.length, 1, "one migration warning emitted");
    assert.match(ctx.warnings[0], /\"use server\"/);
    assert.match(ctx.warnings[0], /procedure\(\)/);
    assert.match(ctx.warnings[0], /ISS-02/);
  });

  test("directive present, no wrappers → 0 discovered (wrappers required)", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    // Directive opts the file in, but no exports are wrapped — the
    // file's exports stay private (helpers, constants, types).
    const code = `"use server";

// Helpers and constants — none are wrappers.
export async function helperA(x) { return x + 1; }
export async function helperB() { return Date.now(); }
export const SOME_CONSTANT = 42;
export const arrowHelper = async () => null;
`;
    getHandler(plugin).call(ctx, code, "/r/src/lib/api.ts");

    assert.equal(
      state.discoveredProcedures.length,
      0,
      "no wrappers → no procedures, even with the directive",
    );
    // No migration warning — the directive is present, the file is
    // simply chosen not to expose any RPCs.
    assert.equal(ctx.warnings.length, 0);
  });

  test("legacy path WITH directive but no wrappers → 0 discovered, no warning", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    // Path matches the legacy convention AND directive is present;
    // since the directive is satisfied no migration warning fires.
    // No wrappers means no procedures.
    const code = `"use server";
export async function leftoverHelper() { return 1; }
`;
    getHandler(plugin).call(ctx, code, "/r/src/server/api.ts");

    assert.equal(state.discoveredProcedures.length, 0);
    assert.equal(ctx.warnings.length, 0);
  });
});
