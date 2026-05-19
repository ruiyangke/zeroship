/**
 * Synthetic SSR entry fed by a `ServerBinding` map.
 *
 * When the plugin has run the reference-graph walk it hands the
 * generator a `Map<string, ServerBinding>`. The output emits one
 * namespace import per target file and a static `_procedures` literal
 * keyed by wireId. `docs/proposals/rpc.md` §5 requires the
 * generated code to stay structurally compatible with `__dispatchRpc`;
 * until the upstream stub ships, we keep the older inline dispatch
 * behavior with a TODO.
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
    // Fallback shape — runtime loop over _zsUser.
    assert.match(code, /for \(const _k of Object\.keys\(_zsUser\)\)/);
  });

  test("single file, two procedures: emits one namespace import + a procedures map", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/actions/todos.ts", exportName: "list", wireId: "todos.list", kind: "query" },
        { sourceFile: "/proj/src/actions/todos.ts", exportName: "add",  wireId: "todos.add",  kind: "mutation" },
      ]),
    });
    // One per-target namespace import.
    const imports = code.match(/import \* as _user_TARGET_\d+_ from /g) ?? [];
    assert.equal(imports.length, 1);
    // Map carries both wireIds.
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

  test("default export shape carries fetch + rpc per docs/proposals/rpc.md §5", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/server.ts", exportName: "ping", wireId: "ping" },
      ]),
    });
    assert.match(code, /export default \{[\s\S]*rpc:[\s\S]*fetch:/);
  });

  test("TODO marker for upstream __dispatchRpc", () => {
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/server.ts", exportName: "ping" },
      ]),
    });
    // The upstream `__dispatchRpc` shim isn't yet exported by
    // `@zeroship/server/runtime`. The synthetic entry leaves a TODO so
    // a future upgrade is grep-able.
    assert.match(code, /TODO\(rpc-v2\)/);
    assert.match(code, /__dispatchRpc/);
  });

  test("user fetch fall-through preserved", () => {
    // The synthetic entry's `default.fetch` should forward non-RPC
    // requests to the user's own `default.fetch` when present, else
    // route through `_zsFetch` (which 404s for unknown paths).
    const code = buildServerEntrySource({
      userEntryRel: "/proj/src/server.ts",
      bindings: bindingMap([
        { sourceFile: "/proj/src/server.ts", exportName: "ping" },
      ]),
    });
    assert.match(code, /_userFetch/);
    assert.match(code, /_zs\/v1\//);
  });
});
