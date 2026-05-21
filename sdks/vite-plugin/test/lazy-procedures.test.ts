/**
 * Wave #188 — opt-in lazy procedures.
 *
 * `fn.config.lazy = true` (or the `mutation(handler, { lazy: true })`
 * wrapper option) flips a procedure to dynamic-import emission. The
 * synthetic SSR entry stops emitting the static `import * as
 * _user_TARGET_<n>_ from "./<file>"` line and replaces the dispatch
 * entry with `async (input, ctx) => (await import("./<file>"))
 * .<exportName>(input, ctx)`.
 *
 * Cold-start parses ONLY non-lazy procedures' modules; the lazy
 * module's body runs on first call. V8's dynamic-import host callback
 * (Wave #187) caches the namespace, so the second call to a lazy
 * procedure is a Map lookup, not a re-import.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";
import { parse as acornParse } from "acorn";

import { transformPlugin, type TransformState } from "../src/transform.js";
import { buildServerEntrySource } from "../src/rpc-registry.js";
import type { ServerBinding } from "../src/server-graph.js";

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
      ...(r.lazy ? { lazy: true } : {}),
    });
  }
  return out;
}

// ── Detection — fn.config.lazy = true ────────────────────────────────────

describe("lazy detection — transform.ts", () => {
  test("fn.config.lazy = true is recorded on the discovered record", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { procedure } from "@zeroship/server";
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
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/server";
export const heavyOp = mutation(async (x) => x, { id: "heavyOp", lazy: true });
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/actions/heavy.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(state.discoveredProcedures[0].lazy, true);
  });

  test("eager default: omitting lazy → record has no lazy flag", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/server";
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
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/server";
export const op = mutation(async (x) => x, { lazy: false });
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/actions/op.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(state.discoveredProcedures[0].lazy, undefined);
  });

  test("non-literal lazy expression: warns + falls back to eager", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/server";
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
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const code = `"use server";
import { mutation } from "@zeroship/server";
export const op = mutation(async (x) => x, { lazy: true });
op.config = { id: "op", lazy: false };
`;
    getHandler(plugin).call(makeCtx("ssr"), code, "/r/src/actions/op.ts");

    assert.equal(state.discoveredProcedures[0].lazy, undefined, "legacy false wins");
  });
});

// ── Emission — buildServerEntrySource ────────────────────────────────────

describe("lazy emission — buildServerEntrySource", () => {
  test("eager binding: standard static import + namespace lookup", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        {
          sourceFile: "/proj/src/actions/eager.ts",
          exportName: "fast",
          wireId: "fast",
        },
      ]),
    });
    assert.match(
      code,
      /import \* as _user_TARGET_0_ from "\/proj\/src\/actions\/eager\.ts"/,
    );
    assert.match(code, /"fast":\s*_user_TARGET_0_\.fast/);
    // No dynamic-import wrapper around USER procedures in the eager-only
    // emission. The schema-registration block (Stage 2) dynamically
    // imports `@zeroship/db/internal`; that's orthogonal to procedure
    // dispatch and must not affect cold-start procedure parsing.
    assert.equal(/await import\("\/proj\//.test(code), false);
    assert.equal(/await import\("\.\//.test(code), false);
  });

  test("lazy binding: no static import; emits dynamic-import wrapper", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        {
          sourceFile: "/proj/src/actions/wizard.ts",
          exportName: "wizard",
          wireId: "wizard",
          lazy: true,
        },
      ]),
    });
    // No static import line for the lazy file.
    assert.equal(
      /import \* as _user_TARGET_\d+_ from "\/proj\/src\/actions\/wizard\.ts"/.test(
        code,
      ),
      false,
      "lazy-only file gets no static import",
    );
    // The dispatch entry is the dynamic-import wrapper.
    assert.match(
      code,
      /"wizard":\s*async \(input, ctx\) =>\s*\(await import\("\/proj\/src\/actions\/wizard\.ts"\)\)\.wizard\(input, ctx\)/,
    );
  });

  test("mixed file: eager + lazy exports share the source file", () => {
    // Same file holds both `eager()` and `heavy({ lazy: true })`. The
    // static import emits for the eager path; the lazy path uses
    // `await import()`. V8's module cache de-dupes — the dynamic
    // import resolves to the same namespace the static import already
    // evaluated.
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        {
          sourceFile: "/proj/src/actions/mixed.ts",
          exportName: "eager",
          wireId: "eager",
        },
        {
          sourceFile: "/proj/src/actions/mixed.ts",
          exportName: "heavy",
          wireId: "heavy",
          lazy: true,
        },
      ]),
    });
    // Static import IS emitted — driven by the eager binding.
    assert.match(
      code,
      /import \* as _user_TARGET_0_ from "\/proj\/src\/actions\/mixed\.ts"/,
    );
    // Eager dispatch entry uses the namespace alias.
    assert.match(code, /"eager":\s*_user_TARGET_0_\.eager/);
    // Lazy dispatch entry uses dynamic import on the SAME file.
    assert.match(
      code,
      /"heavy":\s*async \(input, ctx\) =>\s*\(await import\("\/proj\/src\/actions\/mixed\.ts"\)\)\.heavy\(input, ctx\)/,
    );
  });

  test("synthetic-entry shape: dynamic-import arrow matches the spec", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        {
          sourceFile: "/proj/src/heavy.ts",
          exportName: "heavy",
          wireId: "heavy",
          lazy: true,
        },
      ]),
    });
    // The full arrow: `async (input, ctx) => (await import("./heavy.js")).heavy(input, ctx)`.
    assert.match(
      code,
      /async \(input, ctx\) => \(await import\("\/proj\/src\/heavy\.ts"\)\)\.heavy\(input, ctx\)/,
    );
  });

  test("strict-mode interaction: lazy is orthogonal to wireId pinning", () => {
    // A lazy procedure with bare exportName as its wireId still passes
    // through the same emission shape — strict-mode gating happens
    // upstream in build.ts (not in buildServerEntrySource). The
    // emission itself doesn't care whether wireId is a default or a
    // pin: the output is byte-identical for the two cases.
    const codeBare = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        {
          sourceFile: "/proj/src/h.ts",
          exportName: "heavy",
          wireId: "heavy", // bare default
          lazy: true,
        },
      ]),
    });
    const codePinned = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        {
          sourceFile: "/proj/src/h.ts",
          exportName: "heavy",
          wireId: "myWizard", // pinned
          lazy: true,
        },
      ]),
    });
    assert.match(
      codeBare,
      /"heavy":\s*async \(input, ctx\) => \(await import\("\/proj\/src\/h\.ts"\)\)\.heavy\(input, ctx\)/,
    );
    assert.match(
      codePinned,
      /"myWizard":\s*async \(input, ctx\) => \(await import\("\/proj\/src\/h\.ts"\)\)\.heavy\(input, ctx\)/,
    );
  });

  test("re-export chain + lazy: dynamic import targets the actual source file", () => {
    // The reference-graph walk records each binding's `sourceFile` as
    // the ULTIMATE declaring location after re-export tracing. Our
    // emission consumes that same field — so a lazy binding always
    // imports from the source-of-truth, not from any intermediate
    // re-exporting `index.ts`.
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        {
          // The walk resolved `index.ts`'s `export { heavy } from "./heavy.ts"`
          // to the declaring file.
          sourceFile: "/proj/src/actions/heavy.ts",
          exportName: "heavy",
          wireId: "heavy",
          lazy: true,
          chain: ["/proj/src/actions/index.ts", "/proj/src/actions/heavy.ts"],
        },
      ]),
    });
    // Imports the source file, NOT `index.ts`.
    assert.match(code, /await import\("\/proj\/src\/actions\/heavy\.ts"\)/);
    assert.equal(
      /await import\("\/proj\/src\/actions\/index\.ts"\)/.test(code),
      false,
      "re-exporting file is not the dynamic import target",
    );
  });

  test("multiple lazy procedures across files: deterministic emission", () => {
    // Lex-ordered file iteration — emission stability matters for
    // build cache hits. Two lazy bindings in two different files
    // each get their own dynamic-import wrapper line.
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        {
          sourceFile: "/proj/src/b.ts",
          exportName: "bigB",
          wireId: "bigB",
          lazy: true,
        },
        {
          sourceFile: "/proj/src/a.ts",
          exportName: "bigA",
          wireId: "bigA",
          lazy: true,
        },
      ]),
    });
    // Both lazy lines present.
    assert.match(code, /"bigA":[\s\S]*await import\("\/proj\/src\/a\.ts"\)/);
    assert.match(code, /"bigB":[\s\S]*await import\("\/proj\/src\/b\.ts"\)/);
    // No static imports — both files are lazy-only.
    assert.equal(/import \* as _user_TARGET_/.test(code), false);
  });
});
