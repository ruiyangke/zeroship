import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { parse as acornParse } from "acorn";

import { transformPlugin, type TransformState } from "../src/transform.js";
import { bindingMap, buildEntryFixture } from "./helpers/server-entry-fixture.js";

// ── Test harness ──────────────────────────────────────────────────────────

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

// ── Detection — fn.config.lazy = true ────────────────────────────────────

describe("lazy detection — transform.ts", () => {
  test("fn.config.lazy = true is recorded on the discovered record", () => {
    const state = makeState();
    const plugin = transformPlugin(state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { procedure } from "@zeroship/rpc/server";
export const wizard = procedure(async (input) => input);
wizard.config = { id: "wizard", kind: "mutation", lazy: true };
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/actions/wizard.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    const p = state.discoveredProcedures[0];
    assert.equal(p.exportName, "wizard");
    assert.equal(p.lazy, true, "lazy flag carried on the record");
    // Other config keys still flow through.
    assert.equal(p.config?.id, "wizard");
    assert.equal(p.config?.kind, "mutation");
  });

  test("wrapper-form: mutation(handler, { lazy: true }) is recorded", () => {
    const state = makeState();
    const plugin = transformPlugin(state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/rpc/server";
export const heavyOp = mutation(async (x) => x, { id: "heavyOp", lazy: true });
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/actions/heavy.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(state.discoveredProcedures[0].lazy, true);
  });

  test("eager default: omitting lazy → record has no lazy flag", () => {
    const state = makeState();
    const plugin = transformPlugin(state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/rpc/server";
export const fast = mutation(async (x) => x);
fast.config = { id: "fast" };
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/actions/fast.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(
      state.discoveredProcedures[0].lazy,
      undefined,
      "no lazy field when not opted in",
    );
  });

  test("lazy: false explicitly → record stays eager", () => {
    const state = makeState();
    const plugin = transformPlugin(state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/rpc/server";
export const op = mutation(async (x) => x, { lazy: false });
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/actions/op.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(state.discoveredProcedures[0].lazy, undefined);
  });

  test("non-literal lazy expression: warns + falls back to eager", () => {
    const state = makeState();
    const plugin = transformPlugin(state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/rpc/server";
const useLazy = process.env.LAZY === "1";
export const op = mutation(async (x) => x);
op.config = { id: "op", lazy: useLazy };
`;
    const ctx = makeCtx("ssr");
    getHandler(plugin).call(ctx, code, "/r/src/actions/op.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    // Stays eager — non-literal can't be statically determined.
    assert.equal(state.discoveredProcedures[0].lazy, undefined);
    // A warning was emitted naming the offending procedure.
    assert.equal(ctx.warnings.length, 1);
    assert.match(ctx.warnings[0], /non-literal/);
    assert.match(ctx.warnings[0], /\bop\b/);
  });

  test("legacy assignment wins over wrapper-arg lazy on conflict", () => {
    // Legacy `<fn>.config = { ... }` is the explicit / late-binding
    // surface. When both are present and disagree, legacy wins —
    // matches the overall config-merge precedence the transform uses
    // for `id` / `kind` / etc.
    const state = makeState();
    const plugin = transformPlugin(state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/rpc/server";
export const op = mutation(async (x) => x, { lazy: true });
op.config = { id: "op", lazy: false };
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/actions/op.ts");

    assert.equal(state.discoveredProcedures[0].lazy, undefined, "legacy false wins");
  });
});


describe("lazy entries return procedures to native dispatch", () => {
  test("module loading preserves frozen metadata without invoking the handler", async (t) => {
    const fixture = await buildEntryFixture(t, {
      bindings: bindingMap([{ sourceFile: "./actions/heavy.mjs", exportName: "heavy", wireId: "__proto__", lazy: true }]),
      files: {
        "state.mjs": "export const state = { loaded: 0, invoked: 0, parsed: 0 };",
        "user.mjs": "import { state } from './state.mjs'; export default { rpc: { inspect: () => ({ ...state }) } };",
        "actions/heavy.mjs": `
          import { state } from '../state.mjs';
          state.loaded++;
          const input = Object.freeze({
            parse(value) {
              if (value == null || typeof value.amount !== 'number') throw Error('amount required');
              state.parsed++;
              return value.amount * 2;
            },
          });
          const config = Object.freeze({ kind: 'mutation', input, outputIsString: false });
          export const heavy = Object.assign((input, ctx) => {
            state.invoked++;
            return { input, requestId: ctx.requestId };
          }, { config });
          heavy.identity = heavy;
          Object.freeze(heavy);
        `,
      },
    });
    const { rpc } = (await fixture.load()).default;
    assert.deepEqual(rpc.inspect(), { loaded: 0, invoked: 0, parsed: 0 });
    assert.ok(Object.hasOwn(rpc, "__proto__"));
    assert.equal(typeof rpc.__proto__, "object");
    assert.deepEqual(Object.keys(rpc.__proto__), ["load"]);
    const [procedure, same] = await Promise.all([rpc.__proto__.load(), rpc.__proto__.load()]);
    assert.equal(procedure, same);
    assert.equal(procedure, procedure.identity);
    assert.ok(Object.isFrozen(procedure));
    assert.ok(Object.isFrozen(procedure.config));
    assert.equal(procedure.config.kind, "mutation");
    assert.equal(procedure.config.outputIsString, false);
    assert.deepEqual(rpc.inspect(), { loaded: 1, invoked: 0, parsed: 0 });
    assert.throws(() => procedure.config.input.parse({ amount: "invalid" }), /amount required/);
    const input = procedure.config.input.parse({ amount: 3 });
    assert.deepEqual(procedure(input, { requestId: "request" }), { input: 6, requestId: "request" });
    assert.deepEqual(rpc.inspect(), { loaded: 1, invoked: 1, parsed: 1 });
    assert.equal(await rpc.__proto__.load(), procedure);
    assert.deepEqual(
      fixture.artifact.inputs["entry.mjs"].imports
        .filter(entry => entry.path === "actions/heavy.mjs")
        .map(entry => entry.kind),
      ["dynamic-import"],
    );
  });

  test("eager and lazy exports share their module instance", async (t) => {
    const fixture = await buildEntryFixture(t, {
      bindings: bindingMap([
        { sourceFile: "./mixed.mjs", exportName: "fast" },
        { sourceFile: "./mixed.mjs", exportName: "heavy", lazy: true },
      ]),
      files: {
        "user.mjs": "export default {};",
        "mixed.mjs": `
          const identity = {};
          let calls = 0;
          export const fast = () => ({ identity, calls, heavy });
          export const heavy = Object.assign(() => { calls++; return identity; }, { config: { kind: 'query' } });
        `,
      },
    });
    const { rpc } = (await fixture.load()).default;
    const before = rpc.fast();
    const procedure = await rpc.heavy.load();
    assert.equal(procedure, before.heavy);
    assert.equal(rpc.fast().calls, 0);
    assert.equal(procedure(), before.identity);
    assert.equal(rpc.fast().calls, 1);
    assert.deepEqual(
      fixture.artifact.inputs["entry.mjs"].imports
        .filter(entry => entry.path === "mixed.mjs")
        .map(entry => entry.kind).sort(),
      ["dynamic-import", "import-statement"],
    );
  });

  test("lazy bindings use declaring modules and arbitrary export and RPC names", async (t) => {
    const fixture = await buildEntryFixture(t, {
      bindings: bindingMap([
        { sourceFile: "./actions/b.mjs", exportName: 'quoted "export"', wireId: "constructor", lazy: true },
        { sourceFile: "./actions/a.mjs", exportName: "actual", wireId: "rpc with spaces", lazy: true },
      ]),
      files: {
        "user.mjs": "export default {};",
        "actions/index.mjs": "throw Error('a re-exporting module must not be evaluated');",
        "actions/a.mjs": "export const actual = Object.assign(() => 'A', { config: { kind: 'query' } });",
        "actions/b.mjs": 'const actual = () => "B"; export { actual as "quoted \\"export\\"" };',
      },
    });
    const { rpc } = (await fixture.load()).default;
    assert.equal((await rpc.constructor.load())(), "B");
    assert.equal((await rpc["rpc with spaces"].load())(), "A");
    assert.deepEqual(
      fixture.artifact.inputs["entry.mjs"].imports
        .filter(entry => entry.kind === "dynamic-import")
        .map(entry => entry.path),
      ["actions/a.mjs", "actions/b.mjs"],
    );
    assert.equal(Object.hasOwn(fixture.artifact.inputs, "actions/index.mjs"), false);
  });

  test("module evaluation failure rejects loading without affecting an eager procedure", async (t) => {
    const fixture = await buildEntryFixture(t, {
      bindings: bindingMap([
        { sourceFile: "./ready.mjs", exportName: "ready" },
        { sourceFile: "./failure.mjs", exportName: "failure", lazy: true },
      ]),
      files: {
        "user.mjs": "export default {};",
        "ready.mjs": "export const ready = () => 'ready';",
        "failure.mjs": "throw Error('cannot initialize'); export const failure = () => 'unreachable';",
      },
    });
    const { rpc } = (await fixture.load()).default;
    assert.equal(rpc.ready(), "ready");
    await assert.rejects(rpc.failure.load(), /cannot initialize/);
    assert.equal(rpc.ready(), "ready");
  });
});
