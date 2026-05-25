// sdks/vite-plugin/src/zeroship-module.ts
//
// Virtual `zeroship` module resolver for the server (zeroship) environment.
//
// The kernel synthesizes a real `zeroship` module in the V8 isolate
// (see ZEROSHIP_MODULE_JS in crates/runtime/src/init.rs) that wires
// env/waitUntil/getRequest to the native `__zs_env` / `__zs_wait_until`
// / `__zs_get_request` callbacks. That module doesn't go through Node's
// resolver, so in dev mode Vite falls back to the node `zeroship-stub`
// package which has `export const env = {}` — empty.
//
// When the stub is served into the ModuleRunner and user code does
// `import { env } from "zeroship"`, they get the empty object.
// `env.db` is undefined → @zeroship/db throws "env.db not available".
//
// This plugin intercepts the bare `zeroship` specifier in the zeroship
// environment and returns code equivalent to ZEROSHIP_MODULE_JS, so the
// user module sees the real, plugin-populated env object at runtime.

import type { Plugin } from "vite";

const VIRTUAL_ID = "zeroship";
const RESOLVED_ID = "\0virtual:zeroship-runtime";

const MODULE_CODE = `
// Virtual "zeroship" module — injected by @zeroship/vite-plugin when the
// user imports "zeroship" from server code. Mirrors the kernel's own
// ZEROSHIP_MODULE_JS so @zeroship/db, @zeroship/auth, etc. resolve
// env.db / env.auth against the plugin-populated env at runtime.
const env = Object.freeze(__zs_env());

function waitUntil(promise) {
  if (!(promise instanceof Promise)) {
    throw new TypeError("waitUntil expects a Promise");
  }
  __zs_wait_until(promise);
}

function getRequest() {
  const req = __zs_get_request();
  if (!req) {
    throw new Error("getRequest called outside a fetch handler (RPC fast-path has no Request)");
  }
  return req;
}

// runQuery / runMutation — see crates/runtime/src/core/init.rs
// ZEROSHIP_MODULE_JS for the canonical comments.
async function _runWithKind(kind, fn, args) {
  if (typeof fn !== "function") {
    throw new TypeError("runQuery/runMutation: first arg must be a procedure function");
  }
  const ek = globalThis.__zsEnterKind;
  const xk = globalThis.__zsExitKind;
  const tok = (typeof ek === "function") ? ek(kind) : -1;
  try {
    const out = fn(args);
    return (out && typeof out.then === "function") ? await out : out;
  } finally {
    if (tok >= 0 && typeof xk === "function") xk(tok);
  }
}
function runQuery(fn, args) { return _runWithKind("query", fn, args); }
function runMutation(fn, args) { return _runWithKind("mutation", fn, args); }

// Per-request accessors.
function _requireCtx(name) {
  const ctx = globalThis.__zeroshipGetRpcCtx && globalThis.__zeroshipGetRpcCtx();
  if (!ctx) throw new Error(name + ": called outside a request handler");
  return ctx;
}
function currentUser()           { return _requireCtx("currentUser").user; }
function currentRequestId()      { return _requireCtx("currentRequestId").requestId; }
function currentTraceId()        { return _requireCtx("currentTraceId").traceId; }
function currentSignal()         { return _requireCtx("currentSignal").signal; }
function currentHeaders()        { return _requireCtx("currentHeaders").headers; }
function currentIdempotencyKey() { return _requireCtx("currentIdempotencyKey").idempotencyKey; }

export {
  env, waitUntil, getRequest, runQuery, runMutation,
  currentUser, currentRequestId, currentTraceId,
  currentSignal, currentHeaders, currentIdempotencyKey,
};
`;

/**
 * Resolve `@zeroship/bootstrap` (and its `./install-schema` subpath)
 * to the framework-installed copy of the package. The user's project
 * doesn't depend on `@zeroship/bootstrap` — it's a framework-internal
 * package the dev-bootstrap loads through the ModuleRunner so the
 * `TypeBuilder` class identity matches the one the user's `t.*`
 * builders use.
 *
 * Without this resolver, ModuleRunner can't find the bootstrap package
 * via the user's `node_modules` chain (pnpm doesn't hoist).
 */
function resolveBootstrapPkgRoot(): string | undefined {
  try {
    const mainUrl = new URL(import.meta.resolve("@zeroship/bootstrap"));
    return mainUrl.pathname.replace(/[/\\]dist[/\\][^/\\]+$/, "");
  } catch {
    return undefined;
  }
}

export function zeroshipBootstrapResolverPlugin(): Plugin {
  const pkgRoot = resolveBootstrapPkgRoot();
  return {
    name: "zeroship:bootstrap-resolver",
    enforce: "pre" as const,
    resolveId(id: string) {
      if (!pkgRoot) return null;
      if (id === "@zeroship/bootstrap") {
        return `${pkgRoot}/dist/index.js`;
      }
      if (id.startsWith("@zeroship/bootstrap/")) {
        const sub = id.slice("@zeroship/bootstrap/".length);
        return `${pkgRoot}/dist/${sub}.js`;
      }
      return null;
    },
  };
}

export function zeroshipModulePlugin(): Plugin {
  return {
    name: "zeroship:virtual-module",
    enforce: "pre" as const,

    resolveId(id: string) {
      // Only intercept in the server env — on the client, `zeroship` is
      // not a resolvable module (user code should never import it in
      // the browser).
      // Note: `this.environment?.name` isn't always available in resolveId
      // depending on Vite version; we intercept in all environments and
      // rely on the V8 runtime to provide __zs_env. On the client, this
      // module is tree-shaken because client code shouldn't import it.
      if (id === VIRTUAL_ID) {
        return RESOLVED_ID;
      }
      return null;
    },

    load(id: string) {
      if (id === RESOLVED_ID) {
        return MODULE_CODE;
      }
      return null;
    },
  };
}
