/**
 * `procedure()` exports + plain helpers coexist in one server module.
 *
 * The goal is making this safe: a developer can mix
 * RPC procedures and internal helpers in the same file, and only
 * the wrapped exports become public endpoints. Helpers stay private
 * to the server bundle even though they're `export`-ed at the module
 * level (they're imported by other server modules; the manifest
 * never sees them).
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

describe('helper + procedure coexistence in a single "use server" file', () => {
  test("only wrapped exports are RPC; helpers remain private", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    // Realistic server-module shape: a few RPC procedures alongside
    // helpers used by other server modules.
    const code = `"use server";
import { procedure, query, mutation } from "@zeroship/rpc/server";

// ── Helpers (NOT RPCs) ───────────────────────────────────────────
function _hashSecret(secret) {
  // imagine: crypto.subtle.digest(...)
  return secret + "_hashed";
}

export async function hashSecret(input) {
  // exported so OTHER server files can import it; never reached over HTTP.
  return _hashSecret(input);
}

export const ADMIN_ROLE = "admin";

export const tokenFor = async (userId) => "tok_" + userId;

// ── RPC procedures (wrapped) ─────────────────────────────────────
export const listSessions = query(async () => [{ id: 1 }]);
export const revokeSession = mutation(async (sid) => ({ revoked: sid }), {
  id: "session.revoke",
});
export const provision = procedure(async (req) => ({ provisioned: req }));
`;
    getHandler(plugin).call(ctx, code, "/r/src/api/auth.ts");

    const exportNames = state.discoveredProcedures.map((p) => p.exportName).sort();
    assert.deepEqual(
      exportNames,
      ["listSessions", "provision", "revokeSession"],
      "helpers (hashSecret, tokenFor, ADMIN_ROLE) excluded; only wrappers discovered",
    );

    // Verify config came through untouched.
    const byName = new Map(state.discoveredProcedures.map((p) => [p.exportName, p]));
    assert.equal(byName.get("revokeSession")?.kind, "mutation");
    assert.equal(byName.get("revokeSession")?.config?.id, "session.revoke");
    assert.equal(byName.get("listSessions")?.kind, "query");
    // Generic procedure() defaults to mutation unless config.kind says otherwise.
    assert.equal(byName.get("provision")?.kind, "mutation");
  });

  test("re-export of an internal helper does NOT publish it (no `export *` footgun)", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    // The original path-based auto-publish footgun:
    // `export * from "./helpers"`
    // silently published every helper as `/__zeroship/v1/<helperName>`.
    // Now: the re-exported helpers are not wrapper calls in THIS
    // file's AST, so they're not registered.
    const code = `"use server";
import { procedure } from "@zeroship/rpc/server";

export * from "./_helpers";
export { default as anonymous } from "./_helpers";

// The only registered procedure is the one explicitly wrapped here.
export const real = procedure(async () => 42);
`;
    getHandler(plugin).call(ctx, code, "/r/src/api.ts");

    const exportNames = state.discoveredProcedures.map((p) => p.exportName);
    assert.deepEqual(
      exportNames,
      ["real"],
      "re-exports from other modules are not auto-published",
    );
  });

  test("default export is ignored — only the wrapped named exports become RPCs", () => {
    const state = makeState();
    const plugin = transformPlugin("/_rpc", state);
    (plugin.configResolved as (c: unknown) => void).call(plugin, { root: "/r" });

    const ctx = makeCtx("ssr");
    // Apps may export a `default.fetch` for HTTP fall-through. The
    // discovery pass ignores `export default` regardless of shape.
    const code = `"use server";
import { procedure } from "@zeroship/rpc/server";

export const ping = procedure(async () => "pong");

export default {
  fetch(req) {
    return new Response("hi");
  },
};
`;
    getHandler(plugin).call(ctx, code, "/r/src/api.ts");

    assert.equal(state.discoveredProcedures.length, 1);
    assert.equal(state.discoveredProcedures[0].exportName, "ping");
  });
});
