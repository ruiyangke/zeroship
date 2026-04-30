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

// @zeroship/server is the npm package that provides server-side SDK hooks
// (useQuery, useStream, etc.). It may not yet be installed. We intercept the
// import and provide a stub that returns an empty hook set so the static
// `import` injected by the SSR transform doesn't fail the module load.
const ZS_SERVER_VIRTUAL_ID = "@zeroship/server";
const ZS_SERVER_RESOLVED_ID = "\0virtual:zeroship-server-stub";
const ZS_SERVER_STUB = `
// Stub for @zeroship/server — injected by @zeroship/vite-plugin when the
// real package is not installed. Provides a no-op __makeServerProcedure
// so SSR transforms can import it safely.
export function __makeServerProcedure(fn, meta) {
  return {};
}
`;

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

export { env, waitUntil, getRequest };
`;

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
      // Provide a stub for @zeroship/server when the real package isn't
      // installed. The SSR transform emits a static `import` of this module
      // so the module MUST resolve or the load fails before __register runs.
      if (id === ZS_SERVER_VIRTUAL_ID) {
        return ZS_SERVER_RESOLVED_ID;
      }
      return null;
    },

    load(id: string) {
      if (id === RESOLVED_ID) {
        return MODULE_CODE;
      }
      if (id === ZS_SERVER_RESOLVED_ID) {
        return ZS_SERVER_STUB;
      }
      return null;
    },
  };
}
