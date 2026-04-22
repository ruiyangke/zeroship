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
