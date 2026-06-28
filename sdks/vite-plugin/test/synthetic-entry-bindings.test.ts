/**
 * Synthetic SSR entry fed by a `ServerBinding` map (Phase-2 shape).
 *
 * When the plugin has run the reference-graph walk it hands the
 * generator a `Map<string, ServerBinding>`. The output emits one
 * namespace import per target file and a static `_zsRpc` dict literal
 * keyed by wireId.
 *
 * After Stage 5b the dispatch dict is PURE DATA — no dispatcher helpers
 * are generated into the synthetic entry. Bootstrap's shared fetch handler
 * and `__zsDispatch` consume it.
 */

import { test, describe } from "node:test";
import assert from "node:assert/strict";

import { buildServerEntrySource } from "../src/rpc-registry.js";
import type { ServerBinding } from "../src/server-graph.js";

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

describe("buildServerEntrySource — binding-fed emission", () => {
  test("empty bindings: falls back to the namespace-walk shape", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: new Map(),
    });
    assert.match(code, /for \(const _zsName of Object\.keys\(_zsUser\)\)/);
  });

  test("single file, two procedures: emits one namespace import + a procedures dict", () => {
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

  test("two files: distinct aliases per file, deterministic order", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/b.ts", exportName: "b1" },
        { sourceFile: "/proj/src/a.ts", exportName: "a1" },
      ]),
    });
    // Files are sorted lexicographically before alias assignment.
    assert.match(code, /import \* as _user_TARGET_0_ from "\/proj\/src\/a\.ts"/);
    assert.match(code, /import \* as _user_TARGET_1_ from "\/proj\/src\/b\.ts"/);
    assert.match(code, /"a1":\s*_user_TARGET_0_\.a1/);
    assert.match(code, /"b1":\s*_user_TARGET_1_\.b1/);
  });

  test("default export shape carries fetch + dict-shape rpc only", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/server.ts", exportName: "ping", wireId: "ping" },
      ]),
    });
    // Dict-shape — `rpc` is the `_zsRpc` OBJECT, not a function call.
    assert.match(code, /export default \{[\s\S]*fetch:[\s\S]*rpc:\s*_zsRpc/);
    assert.doesNotMatch(code, /\bschema:\s*/);
    assert.doesNotMatch(code, /__zsDeclaredSchema/);
    assert.doesNotMatch(code, /_zsUserDefault\.schema/);
  });

  test("user fetch fall-through is shaped through default.fetch", () => {
    // The normaliser picks `user.default.fetch` first, falling back to
    // a top-level `fetch` export. The bootstrap fetch handler routes
    // non-/__zeroship/v1/ traffic through that user handler.
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/server.ts", exportName: "ping" },
      ]),
    });
    assert.match(code, /const _zsTopLevelFetch = Reflect\.get\(_zsUser, "fetch"\)/);
    assert.match(code, /typeof _zsUserDefault\.fetch === "function"/);
    assert.match(code, /typeof _zsTopLevelFetch === "function"/);
  });

  test("lazy bindings emit dynamic-import wrappers (Wave #188)", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/lazy.ts", exportName: "slow", wireId: "slow", lazy: true },
      ]),
    });
    assert.match(
      code,
      /"slow":\s*async \(input, ctx\) =>\s*\(await import\("\/proj\/src\/lazy\.ts"\)\)\.slow\(input, ctx\)/,
    );
  });
});
