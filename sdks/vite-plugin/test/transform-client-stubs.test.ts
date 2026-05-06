/**
 * Client-environment transform output.
 *
 * `docs/proposals/rpc-v2.md` §5 says the client transform replaces
 * server-marked exports
 * with branded ProcedureRef stubs that:
 *
 *   - call the wire (`/_zs/v1/<wireId>`) when invoked,
 *   - carry the `__SERVER_REFERENCE` symbol (so `<form action={fn}>`
 *     and prop-passed server actions can be detected at runtime),
 *   - expose `id`, `kind`, `wire` metadata on the function value.
 *
 * The brand + metadata is delegated to `__makeProcedure` from
 * `@zeroship/rpc-client` — the transform emits one `__makeProcedure`
 * call per server export and pulls the brand symbol from the same
 * package, so RSC-style detectors and dev-tools can re-derive the
 * brand without duplicating the `Symbol.for` call.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { writeFile, mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

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
  test("imports __makeProcedure + __SERVER_REFERENCE from @zeroship/rpc-client", () => {
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
    // Single import line pulls both names from the canonical home;
    // the transform no longer inlines the Symbol.for call.
    assert.match(
      emitted,
      /import\s*\{\s*__makeProcedure\s*,\s*__SERVER_REFERENCE\s*\}\s*from\s*"@zeroship\/rpc-client"/,
    );
    assert.doesNotMatch(emitted, /Symbol\.for\("zeroship\/server-reference"\)/);
  });

  test("stub carries id, kind, wire metadata via __makeProcedure", () => {
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
    assert.match(emitted, /export const list = __makeProcedure\(/);
    assert.match(emitted, /"id":"list"/);
    assert.match(emitted, /"kind":"query"/);
    // remove — explicit wireId pinned, kind "mutation".
    assert.match(emitted, /export const remove = __makeProcedure\(/);
    assert.match(emitted, /"id":"todos\.remove"/);
    assert.match(emitted, /"kind":"mutation"/);
    // wire is "json" by default.
    assert.match(emitted, /"wire":"json"/);
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
    assert.match(emitted, /"kind":"stream"/);
  });

  test("evaluable: emitted code resolves @zeroship/rpc-client and produces a branded stub", async () => {
    // Materialize the emitted client source UNDER the vite-plugin
    // package root so Node's bare-specifier resolver walks up into the
    // workspace's `node_modules` and finds `@zeroship/rpc-client`. A
    // tmpdir outside the workspace tree won't resolve the import — and
    // a `data:` URL has no base path at all.
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

    // import.meta.url ⇒ .../sdks/vite-plugin/test/...; the parent of
    // `test/` (the package root) carries the workspace `node_modules`.
    const here = new URL(".", import.meta.url);
    const dir = await mkdtemp(new URL("zs-stub-", here).pathname);
    const file = join(dir, "stub.mjs");
    try {
      await writeFile(file, emitted, "utf8");
      const mod: any = await import(pathToFileURL(file).href);
      const sym = Symbol.for("zeroship/server-reference");
      assert.equal(typeof mod.ping, "function");
      assert.equal(mod.ping[sym], true, "symbol-tagged stub");
      assert.equal(mod.ping.id, "ping");
      assert.equal(mod.ping.kind, "mutation");
      assert.equal(mod.ping.wire, "json");
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});
