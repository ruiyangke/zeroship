/**
 * Client-environment transform output.
 *
 * `docs/proposals/rpc.md` §5 says the client transform replaces
 * server-marked exports
 * with branded ProcedureRef stubs that:
 *
 *   - call the wire (`/_zs/v1/<wireId>`) when invoked,
 *   - carry the `__SERVER_REFERENCE` symbol (so `<form action={fn}>`
 *     and prop-passed server actions can be detected at runtime),
 *   - expose `id`, `kind`, `wire` metadata on the function value.
 *
 * The brand + metadata is delegated to `createRpcClient` from
 * `@zeroship/rpc/client`, so generated stubs share the same transport
 * and procedure branding as manually-created RPC clients.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { mkdir, writeFile, mkdtemp, rm } from "node:fs/promises";
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

async function installRpcClientStub(baseDir: string): Promise<void> {
  const pkgDir = join(baseDir, "node_modules", "@zeroship", "rpc");
  await mkdir(pkgDir, { recursive: true });
  await writeFile(
    join(pkgDir, "package.json"),
    JSON.stringify({
      name: "@zeroship/rpc",
      type: "module",
      exports: {
        "./client": "./client.js",
      },
    }, null, 2),
    "utf8",
  );
  await writeFile(
    join(pkgDir, "client.js"),
    [
      "export const __SERVER_REFERENCE = Symbol.for(\"zeroship/server-reference\");",
      "export function __makeProcedure(call, meta) {",
      "  const fn = (input, options) => call(input, options);",
      "  Object.defineProperty(fn, \"id\", { value: meta.id, enumerable: true });",
      "  Object.defineProperty(fn, \"kind\", { value: meta.kind, enumerable: true });",
      "  Object.defineProperty(fn, \"wire\", { value: meta.wire ?? \"json\", enumerable: true });",
      "  Object.defineProperty(fn, __SERVER_REFERENCE, { value: true, enumerable: false });",
      "  return fn;",
      "}",
      "export function createRpcClient() {",
      "  return {",
      "    procedure: (meta) => __makeProcedure((input, callOptions) => ({ id: meta.id, kind: meta.kind, input, callOptions }), { wire: \"json\", ...meta }),",
      "    query: (id, options) => __makeProcedure((input, callOptions) => ({ id, kind: \"query\", input, callOptions, options }), { id, kind: \"query\", wire: \"json\", ...(options ?? {}) }),",
      "    mutation: (id, options) => __makeProcedure((input, callOptions) => ({ id, kind: \"mutation\", input, callOptions, options }), { id, kind: \"mutation\", wire: \"json\", ...(options ?? {}) }),",
      "    action: (id, options) => __makeProcedure((input, callOptions) => ({ id, kind: \"action\", input, callOptions, options }), { id, kind: \"action\", wire: \"json\", ...(options ?? {}) }),",
      "    stream: (id, options) => __makeProcedure((input, callOptions) => ({ id, kind: \"stream\", input, callOptions, options }), { id, kind: \"stream\", wire: \"json\", ...(options ?? {}) }),",
      "  };",
      "}",
      "",
    ].join("\n"),
    "utf8",
  );
}

describe("client-environment transform — branded stubs", () => {
  test("imports the public procedure factory from @zeroship/rpc/client", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/rpc/server";
export const add = mutation(async (input) => input);
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/actions/todos.ts");
    assert.ok(out, "transform returned output");
    const emitted: string = out.code;
    assert.match(emitted, /import\s*\{\s*createRpcClient\s*\}\s*from\s*"@zeroship\/rpc\/client"/);
    assert.match(emitted, /const __zsRpc = createRpcClient\(\)/);
    assert.doesNotMatch(emitted, /__callProcedure/);
    assert.doesNotMatch(emitted, /__streamProcedure/);
    assert.doesNotMatch(emitted, /Symbol\.for\("zeroship\/server-reference"\)/);
  });

  test("stub carries id, kind, wire metadata through the public factory", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation, query } from "@zeroship/rpc/server";
export const list = query(async () => []);
export const remove = mutation(async (id) => id, { id: "todos.remove" });
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/actions/todos.ts");
    const emitted: string = out.code;
    // list — bare exportName as wireId, kind "query".
    assert.match(emitted, /export const list = __zsRpc\.query\("list"\)/);
    // remove — explicit wireId pinned, kind "mutation".
    assert.match(emitted, /export const remove = __zsRpc\.mutation\("todos\.remove"\)/);
  });

  test("stub uses a public mutation factory for non-stream procedures", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/rpc/server";
export const send = mutation(async (input) => input);
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/x.ts");
    const emitted: string = out.code;
    assert.match(emitted, /export const send = __zsRpc\.mutation\("send"\)/);
  });

  test("stream stub uses the public stream factory", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { stream } from "@zeroship/rpc/server";
export const drip = stream(async function* () { yield 1; });
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/x.ts");
    const emitted: string = out.code;
    assert.match(emitted, /export const drip = __zsRpc\.stream\("drip"\)/);
  });

  test("subscription stub preserves subscription metadata instead of using stream transport", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { subscription } from "@zeroship/rpc/server";
export const feed = subscription(async function* () { yield 1; }, { id: "feed.events" });
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/x.ts");
    const emitted: string = out.code;
    assert.match(
      emitted,
      /export const feed = __zsRpc\.procedure\(\{"id":"feed\.events","kind":"subscription","wire":"json"\}\)/,
    );
    assert.doesNotMatch(emitted, /__zsRpc\.stream\("feed\.events"\)/);
  });

  test("idempotent metadata reaches generated direct-call stubs", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/rpc/server";
export const save = mutation(async (input) => input, { id: "todos.save", idempotent: true });
`;
    const ctx = makeCtx("client");
    const out = getHandler(plugin).call(ctx, code, "/r/src/x.ts");
    const emitted: string = out.code;
    assert.match(emitted, /"idempotent":true/);
    assert.match(
      emitted,
      /export const save = __zsRpc\.mutation\("todos\.save", \{"idempotent":true\}\)/,
    );
  });

  test("evaluable: emitted code resolves @zeroship/rpc/client and produces a branded stub", async () => {
    // Materialize the emitted client source UNDER the vite-plugin
    // package root so Node's bare-specifier resolver walks up into the
    // workspace's `node_modules` and finds `@zeroship/rpc/client`. A
    // tmpdir outside the workspace tree won't resolve the import — and
    // a `data:` URL has no base path at all.
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/rpc/server";
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
      await installRpcClientStub(dir);
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
