/**
 * zeroship:node-compat — routes `node:*` imports to unenv@2 polyfills.
 *
 * unenv@2 exposes a single `defineEnv()` entry that returns:
 *   - `alias`:   Record<string, string>  — `node:foo` → `unenv/node/foo`
 *   - `inject`:  Record<string, string|[id,name]> — bare globals
 *                 (Buffer, process, …) to inject as imports
 *   - `polyfill`: string[] — modules to side-effect import
 *   - `external`: string[] — modules to leave as-is
 *
 * We consume all four:
 *   - `alias` drives the resolver below (`getNodeCompatId`)
 *   - `inject` is wired by `nodeInjectPlugin()` so bare references like
 *     `Buffer.from(...)` resolve to `import { Buffer } from "node:buffer"`,
 *     unifying the bare global and the explicit-import paths onto the
 *     same class.
 */

import type { Plugin } from "vite";
import { fileURLToPath } from "node:url";
import { defineEnv } from "unenv";
// `@rollup/plugin-inject` ships dual ESM/CJS; the static type pulls in
// the namespace export, not the callable factory. Import as namespace
// then unwrap `default` so the runtime gets the function regardless of
// resolver mode (esm/cjs/bundler).
import * as inject from "@rollup/plugin-inject";

// Where unenv lives on disk. `import.meta.resolve("unenv/package.json")`
// resolves from this plugin's package, then we strip the trailing
// `package.json` to get the unenv root. User apps that depend on
// `@zeroship/vite-plugin` get unenv@2 hoisted into pnpm's store but
// not into their own `node_modules` — so bare `unenv/*` imports won't
// resolve from their app dir. We resolve them from here instead.
const UNENV_ROOT = fileURLToPath(
  new URL("./", import.meta.resolve("unenv/package.json")),
);

// ── unenv@2 environment ───────────────────────────────────────────────────

// `nodeCompat: true` (default) wires the alias + inject maps for Node
// builtins. `npmShims: true` adds entries for popular npm packages that
// duplicate Node APIs (e.g. `node-fetch` → fetch).
const { env } = defineEnv({ nodeCompat: true, npmShims: true });

const unenvAliases = env.alias;

// Rewrite inject targets that point at `unenv/node/<X>` modules so they
// go through our `node:<X>` resolver instead. This routes them through
// our custom-override path (`node:process` delegates to the Rust-set
// `globalThis.process`) and ensures bare-global, `import "node:process"`,
// and `globalThis.process` all land on the *same* instance.
//
// `Buffer` already uses `["node:buffer", "Buffer"]` upstream. We extend
// the same convention to `process` (which unenv ships as `"unenv/node/
// process"` — a different module than the one our `node:process`
// override exposes).
const unenvInject: Record<string, string | readonly string[] | false> = {
  ...env.inject,
  process: ["node:process", "default"],
};

// ── Custom overrides (modules where unenv@2 isn't the right fit) ──────────

const CUSTOM_PREFIX = "\0zeroship-node:";

/**
 * Specifiers the V8 runtime resolves itself (native synthetic modules
 * registered in `crates/runtime/src/core/native_modules.rs`). The
 * vite-plugin must NOT polyfill or rewrite these — the bundler keeps
 * the bare `import { X } from "node:foo"` and the runtime's module
 * loader produces a SyntheticModule with the real exports.
 */
const RUNTIME_NATIVE_MODULES = new Set([
  "node:async_hooks",
  "node:crypto",
  "node:util",
]);

/** Custom polyfill code for modules unenv doesn't implement well for V8. */
const customPolyfills: Record<string, string> = {
  // node:crypto and node:async_hooks are now resolved as native V8
  // SyntheticModules by the runtime itself (see
  // crates/runtime/src/core/native_modules.rs). The vite plugin used
  // to ship a virtual module that re-exported `globalThis.__zsAsyncHooks`
  // / `globalThis.__zeroship_node_crypto`; the runtime owns those
  // specifiers directly now, so the shim entries are gone — the
  // resolver below falls through `getNodeCompatId` returning null,
  // and Vite forwards the bare `node:*` import to the runtime
  // unmodified.

  // node:timers/promises — unenv's setInterval is a Promise, not an async
  // generator. The latter is what `for await` consumers (tRPC, langchain
  // streaming) expect.
  "node:timers/promises": `
function _setTimeout(ms, value) { return new Promise(r => globalThis.setTimeout(() => r(value), ms || 0)); }
function _setImmediate(value) { return Promise.resolve(value); }
async function* _setInterval(ms, value) {
  while (true) {
    await new Promise(r => globalThis.setTimeout(r, ms || 0));
    yield value;
  }
}
Object.assign(__vite_ssr_exports__, { setTimeout: _setTimeout, setImmediate: _setImmediate, setInterval: _setInterval, default: { setTimeout: _setTimeout, setImmediate: _setImmediate, setInterval: _setInterval } });
`,

  // node:module — unenv@2's createRequire returns a notImplemented
  // thrower, which breaks packages that probe createRequire at module
  // init (notably @vercel/oidc, transitive via @ai-sdk/gateway). The
  // probe results get template-literal'd into a userAgent string, so a
  // self-referential proxy with a Symbol.toPrimitive of "" satisfies
  // both `mod.fn()` chains and string coercion without throwing.
  // Real createRequire usage is rare in our app surface — this is a
  // soft mock, not a feature.
  "node:module": `
const _noopProxy = new Proxy(function(){}, {
  get(_t, prop) {
    if (prop === Symbol.toPrimitive) return () => "";
    if (prop === "toString" || prop === "valueOf") return () => "";
    if (prop === Symbol.iterator) return function*(){};
    return _noopProxy;
  },
  apply: () => _noopProxy,
  construct: () => ({}),
});
const _noop = function() { return _noopProxy; };
function createRequire() { return _noop; }
function builtinModules() { return []; }
function isBuiltin() { return false; }
function syncBuiltinESMExports() {}
const _default = { createRequire, builtinModules: [], isBuiltin, syncBuiltinESMExports };
Object.assign(__vite_ssr_exports__, {
  default: _default,
  createRequire,
  builtinModules: [],
  isBuiltin,
  syncBuiltinESMExports,
  Module: function(){},
});
`,

  // node:process — re-export the Rust-set globalThis.process so bare
  // `process`, `import process from "node:process"`, and direct global
  // reads all see the same instance. Methods absent on globalThis (e.g.
  // cwd, chdir, exit, EventEmitter API) get harmless no-op shims so
  // npm packages probing `process.cwd()` don't crash.
  //
  // We deliberately don't try to wire the EventEmitter surface — the
  // V8 isolate has no signal-handling story, and most consumers only
  // call `.on('SIGINT', ...)` defensively at top level. A no-op `on`
  // is safer than an incomplete EventEmitter.
  "node:process": `
const _proc = globalThis.process;
const _noop = () => {};
const _emitter = { on: _noop, off: _noop, once: _noop, removeListener: _noop, removeAllListeners: _noop, listeners: () => [], addListener: _noop, setMaxListeners: _noop, getMaxListeners: () => 10, eventNames: () => [] };
function _bind(name, fallback) {
  const fn = _proc?.[name];
  if (typeof fn === "function") return fn.bind(_proc);
  return fallback;
}
const _exports = {
  default: _proc,
  env: _proc.env,
  argv: _proc.argv ?? [],
  argv0: _proc.argv0 ?? "node",
  pid: _proc.pid ?? 0,
  ppid: _proc.ppid ?? 0,
  title: _proc.title ?? "zeroship",
  versions: _proc.versions,
  version: _proc.version,
  platform: _proc.platform,
  arch: _proc.arch,
  release: _proc.release ?? { name: "node" },
  cwd: _bind("cwd", () => "/"),
  chdir: _bind("chdir", _noop),
  exit: _bind("exit", _noop),
  nextTick: _bind("nextTick", (fn, ...a) => queueMicrotask(() => fn(...a))),
  hrtime: _proc.hrtime ?? (() => [0, 0]),
  stdout: _proc.stdout,
  stderr: _proc.stderr,
  stdin: _proc.stdin,
  emitWarning: _bind("emitWarning", _noop),
  ..._emitter,
};
Object.assign(__vite_ssr_exports__, _exports);
`,
};

// ── Public API (used by environment.ts fetchModule override) ──────────────

/**
 * Given a `node:*` specifier, return the rewritten module ID for Vite to
 * resolve. Custom overrides win; unenv@2 aliases handle the rest.
 *
 * - Runtime-native (e.g. `node:async_hooks`) → null + caller marks
 *   external so the bare specifier survives into the bundle and the V8
 *   runtime resolves it itself.
 * - Custom modules → virtual ID (CUSTOM_PREFIX + specifier)
 * - unenv modules  → unenv specifier (e.g. `unenv/node/buffer`)
 * - Unknown        → null (let Vite handle it)
 */
export function getNodeCompatId(specifier: string): string | null {
  const normalized = specifier.startsWith("node:") ? specifier : `node:${specifier}`;

  // Runtime-native: don't rewrite. Caller (resolveId / fetchModule)
  // checks `isRuntimeNative` separately to decide whether to mark
  // external or pass through.
  if (RUNTIME_NATIVE_MODULES.has(normalized)) return null;

  // Custom overrides first.
  if (normalized in customPolyfills) return CUSTOM_PREFIX + normalized;

  // unenv@2 alias map keys are both `node:foo` and `foo`.
  const unenvId = unenvAliases[normalized] ?? unenvAliases[specifier];
  if (unenvId) return unenvId;

  return null;
}

/** True if the V8 runtime owns this specifier (synthetic module). */
export function isRuntimeNative(specifier: string): boolean {
  const normalized = specifier.startsWith("node:") ? specifier : `node:${specifier}`;
  return RUNTIME_NATIVE_MODULES.has(normalized);
}

/** Source code for a custom polyfill virtual module, or null. */
export function getCustomPolyfillCode(id: string): string | null {
  if (!id.startsWith(CUSTOM_PREFIX)) return null;
  const specifier = id.slice(CUSTOM_PREFIX.length);
  return customPolyfills[specifier] ?? null;
}

/**
 * unenv@2 inject map — drives the @rollup/plugin-inject step that
 * rewrites bare references like `Buffer.from(...)` and `process.env`
 * into explicit imports of `node:buffer` / `node:process`. After the
 * alias step those route to the same `unenv/node/*` modules, so bare
 * and explicit reads land on the *same class*.
 *
 * Drops `false` entries (which signal "do not inject this name") and
 * narrows the value type for `@rollup/plugin-inject`, which doesn't
 * accept `false`.
 */
export function getInjectMap(): Record<string, string | [string, string]> {
  const out: Record<string, string | [string, string]> = {};
  for (const [k, v] of Object.entries(unenvInject)) {
    if (v === false) continue;
    if (typeof v === "string") out[k] = v;
    else if (Array.isArray(v) && v.length === 2)
      out[k] = [v[0] as string, v[1] as string];
  }
  return out;
}

// ── Vite plugins ──────────────────────────────────────────────────────────

export function nodeCompatPlugin(): Plugin {
  return {
    name: "zeroship:node-compat",
    enforce: "pre" as const,

    async resolveId(id: string) {
      // Apply for the dev "zeroship" environment AND the production SSR
      // build (which doesn't have a named environment). Skip the client
      // environment so a bundle that incidentally references `node:`
      // doesn't get polyfilled into the browser asset.
      const envName = (this as any).environment?.name;
      if (envName === "client") return null;

      // Runtime-native specifier: keep the bare import in the bundle
      // so the V8 runtime's module loader sees it and resolves it via
      // SyntheticModule.
      if (isRuntimeNative(id)) return { id, external: true };

      // Bare `unenv/*` specifiers — emitted by @rollup/plugin-inject
      // for inject targets like `process` → `unenv/node/process`. The
      // user app doesn't depend on unenv directly, so we resolve from
      // vite-plugin's own copy.
      if (id.startsWith("unenv/")) {
        const sub = id.slice("unenv/".length);
        // unenv@2's `./*` subpath export maps to `./dist/runtime/*.mjs`.
        return `${UNENV_ROOT}dist/runtime/${sub}.mjs`;
      }

      const resolved = getNodeCompatId(id);
      if (!resolved) return null;

      // Custom polyfills → virtual module ID
      if (resolved.startsWith(CUSTOM_PREFIX)) return resolved;

      // unenv paths → resolve from vite-plugin's own copy (see above).
      if (resolved.startsWith("unenv/")) {
        const sub = resolved.slice("unenv/".length);
        return `${UNENV_ROOT}dist/runtime/${sub}.mjs`;
      }

      // Fallback: let Vite resolve normally.
      return this.resolve(resolved);
    },

    load(id: string) {
      return getCustomPolyfillCode(id) ?? null;
    },
  };
}

/**
 * Wraps `@rollup/plugin-inject` to rewrite bare references to
 * `Buffer`, `process`, `global`, `setImmediate`, `clearImmediate` into
 * explicit imports from `node:buffer` / `node:process` / `node:timers`,
 * which our `nodeCompatPlugin` then resolves to unenv@2 modules. Net
 * effect: bare-global and explicit-import paths land on the *same*
 * class — no more duplicate `Buffer`/`process` impls.
 *
 * Gated to the SSR/zeroship environments so the browser bundle stays
 * Node-free.
 */
export function nodeInjectPlugin(): Plugin {
  // Normalize ESM/CJS export shape — the namespace import has the
  // callable factory under `.default` for some resolvers.
  const injectFn = ((inject as any).default ?? inject) as (
    opts: Record<string, string | [string, string]>,
  ) => { transform?: (code: string, id: string) => any };
  const inner = injectFn(getInjectMap());
  const innerTransform = inner.transform;

  return {
    name: "zeroship:node-inject",
    transform(code: string, id: string): any {
      // Skip the client bundle — Buffer/process don't belong in the
      // browser asset.
      const envName = (this as any).environment?.name;
      if (envName === "client") return null;
      // Skip non-JS files; the inject plugin bails on these anyway,
      // but the AST walk costs CPU.
      if (/\.(css|html|json)(\?|$)/.test(id)) return null;
      if (typeof innerTransform !== "function") return null;
      return innerTransform.call(this as any, code, id);
    },
  };
}
