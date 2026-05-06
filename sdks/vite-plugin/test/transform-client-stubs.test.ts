/**
 * RPC v2 Phase 2 — client-environment transform output.
 *
 * Per proposal §5 the client transform replaces server-marked exports
 * with branded ProcedureRef stubs that:
 *
 *   - call the wire (`/_zs/v1/<wireId>`) when invoked,
 *   - carry the `__SERVER_REFERENCE` symbol (so `<form action={fn}>`
 *     and prop-passed server actions can be detected at runtime),
 *   - expose `id`, `kind`, `wire` metadata on the function value.
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

describe("client-environment transform — branded stubs", () => {
  test("emits __SERVER_REFERENCE symbol on the stub", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/server";
export const add = mutation(async (input) => input);
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/actions/todos.ts");
    assert.ok(out, "transform returned output");
    const emitted: string = out.code;
    assert.match(emitted, /__SERVER_REFERENCE/);
    assert.match(emitted, /Symbol\.for\("zeroship\/server-reference"\)/);
    assert.match(emitted, /_f\[__SERVER_REFERENCE\] = true/);
  });

  test("stub carries id, kind, wire metadata", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation, query } from "@zeroship/server";
export const list = query(async () => []);
export const remove = mutation(async (id) => id, { id: "todos.remove" });
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/actions/todos.ts");
    const emitted: string = out.code;
    // list — bare exportName as wireId, kind "query".
    assert.match(emitted, /_f\.id = "list"/);
    assert.match(emitted, /_f\.kind = "query"/);
    // remove — explicit wireId pinned, kind "mutation".
    assert.match(emitted, /_f\.id = "todos\.remove"/);
    assert.match(emitted, /_f\.kind = "mutation"/);
    // wire is "json" by default.
    assert.match(emitted, /_f\.wire = "json"/);
  });

  test("stub forwards to __rpcUnary for non-stream procedures", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/server";
export const send = mutation(async (input) => input);
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/x.ts");
    const emitted: string = out.code;
    assert.match(emitted, /__rpcUnary\("send", input\)/);
  });

  test("stream stub forwards to __rpcStream", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { stream } from "@zeroship/server";
export const drip = stream(async function* () { yield 1; });
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/x.ts");
    const emitted: string = out.code;
    assert.match(emitted, /__rpcStream\("drip", input\)/);
    assert.match(emitted, /_f\.kind = "stream"/);
  });

  test("evaluable: stub really has the symbol set", async () => {
    // Materialize the emitted client source and import it. Mock the
    // global fetch so __rpcUnary doesn't try to hit a real endpoint.
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/server";
export const ping = mutation(async () => "pong", { id: "ping" });
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/x.ts");
    const emitted: string = out.code;

    const dataUrl = "data:text/javascript;charset=utf-8," + encodeURIComponent(emitted);
    const mod = await import(dataUrl);
    const sym = Symbol.for("zeroship/server-reference");
    assert.equal(typeof mod.ping, "function");
    assert.equal(mod.ping[sym], true, "symbol-tagged stub");
    assert.equal(mod.ping.id, "ping");
    assert.equal(mod.ping.kind, "mutation");
    assert.equal(mod.ping.wire, "json");
  });
});
