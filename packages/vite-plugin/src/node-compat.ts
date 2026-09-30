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

import type { Plugin, Rolldown } from "vite";
import { RUNTIME_MODULE_SPECIFIER } from "./constants.js";
import { RUNTIME_REQUIRE_PREFIX, runtimeRequireModule } from "./zeroship-module.js";
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
 * registered in `crates/zeroship-runtime/src/core/native_modules.rs`). The
 * vite-plugin must NOT polyfill or rewrite these — the bundler keeps
 * the bare `import { X } from "node:foo"` and the runtime's module
 * loader produces a SyntheticModule with the real exports.
 */
const RUNTIME_NATIVE_MODULES = new Set([
  "node:async_hooks",
  "node:buffer",
  "node:crypto",
  "node:path",
  "node:util",
]);

/**
 * Custom polyfills for modules unenv doesn't implement well for V8.
 *
 * Each is an ordinary ES module: the worker build bundles it, dev serves it
 * through the environment's transform pipeline, and dev's dependency
 * optimizer inlines it when a dependency requires it.
 */
const customPolyfills: Record<string, string> = {
  // node:timers/promises — unenv's setInterval is a Promise, not an async
  // generator. The latter is what `for await` consumers (tRPC, langchain
  // streaming) expect.
  // The timers take no AbortSignal: one passed in options is refused by name
  // rather than silently never aborting.
  "node:timers/promises": `
function _refuseSignal(name, options) {
  if (options?.signal == null) return;
  const error = new TypeError(
    \`node:timers/promises \${name}(): the signal option is not supported in the zeroship runtime\`,
  );
  error.code = "ERR_ZEROSHIP_UNSUPPORTED_OPTION";
  throw error;
}
export function setTimeout(ms, value, options) {
  _refuseSignal("setTimeout", options);
  return new Promise((resolve) => globalThis.setTimeout(() => resolve(value), ms || 0));
}
export function setImmediate(value, options) {
  _refuseSignal("setImmediate", options);
  return Promise.resolve(value);
}
export function setInterval(ms, value, options) {
  _refuseSignal("setInterval", options);
  return (async function* () {
    while (true) {
      await new Promise((resolve) => globalThis.setTimeout(resolve, ms || 0));
      yield value;
    }
  })();
}
export default { setTimeout, setImmediate, setInterval };
`,

  // node:module — the isolate has no CommonJS loader. createRequire() returns
  // a require that answers the modules the runtime implements, by either name,
  // from their native namespaces, and throws MODULE_NOT_FOUND for anything
  // else, as Node does for a module that is not there. createRequire() itself
  // never throws, so a bundler helper that calls it at module init loads, and
  // an optional require in try/catch takes its fallback.
  "node:module": `
${[...RUNTIME_NATIVE_MODULES].map((specifier, index) => `import * as _runtime${index} from ${JSON.stringify(specifier)};`).join("\n")}
const _runtimeModules = {
${[...RUNTIME_NATIVE_MODULES].map((specifier, index) => `  ${JSON.stringify(specifier.slice("node:".length))}: _runtime${index},`).join("\n")}
};
function _runtimeModule(id) {
  const name = String(id).startsWith("node:") ? String(id).slice(5) : String(id);
  return Object.hasOwn(_runtimeModules, name) ? _runtimeModules[name] : undefined;
}
export function createRequire() {
  return function require(id) {
    const module = _runtimeModule(id);
    if (module !== undefined) return module;
    const error = new Error(
      \`Cannot find module '\${id}': the zeroship runtime has no CommonJS loader, and require \` +
        \`answers only its own modules (\${Object.keys(_runtimeModules).join(", ")})\`,
    );
    error.code = "MODULE_NOT_FOUND";
    throw error;
  };
}
export const builtinModules = Object.keys(_runtimeModules);
export function isBuiltin(id) {
  return _runtimeModule(id) !== undefined;
}
export function syncBuiltinESMExports() {}
export function Module() {}
export default { createRequire, builtinModules, isBuiltin, syncBuiltinESMExports, Module };
`,

  // node:process — re-export the Rust-set globalThis.process so bare
  // \`process\`, \`import process from "node:process"\`, and direct global
  // reads all see the same instance. Methods absent on globalThis (e.g.
  // cwd, chdir, exit, EventEmitter API) get harmless no-op shims so
  // npm packages probing \`process.cwd()\` don't crash.
  //
  // We deliberately don't try to wire the EventEmitter surface — the
  // V8 isolate has no signal-handling story, and most consumers only
  // call \`.on('SIGINT', ...)\` defensively at top level. A no-op \`on\`
  // is safer than an incomplete EventEmitter.
  "node:process": `
const _proc = globalThis.process;
const _noop = () => {};
function _bind(name, fallback) {
  const fn = _proc?.[name];
  return typeof fn === "function" ? fn.bind(_proc) : fallback;
}
export default _proc;
export const env = _proc.env;
export const argv = _proc.argv ?? [];
export const argv0 = _proc.argv0 ?? "node";
export const pid = _proc.pid ?? 0;
export const ppid = _proc.ppid ?? 0;
export const title = _proc.title ?? "zeroship";
export const versions = _proc.versions;
export const version = _proc.version;
export const platform = _proc.platform;
export const arch = _proc.arch;
export const release = _proc.release ?? { name: "node" };
export const cwd = _bind("cwd", () => "/");
export const chdir = _bind("chdir", _noop);
export const exit = _bind("exit", _noop);
export const nextTick = _bind("nextTick", (fn, ...args) => queueMicrotask(() => fn(...args)));
export const hrtime = _proc.hrtime ?? (() => [0, 0]);
export const stdout = _proc.stdout;
export const stderr = _proc.stderr;
export const stdin = _proc.stdin;
export const emitWarning = _bind("emitWarning", _noop);
export const on = _noop;
export const off = _noop;
export const once = _noop;
export const removeListener = _noop;
export const removeAllListeners = _noop;
export const listeners = () => [];
export const addListener = _noop;
export const setMaxListeners = _noop;
export const getMaxListeners = () => 10;
export const eventNames = () => [];
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

/** True if the V8 runtime owns this specifier (synthetic module), under either spelling. */
export function isRuntimeNative(specifier: string): boolean {
  return runtimeNativeSpecifier(specifier) != null;
}

/** The `node:` spelling of a module the runtime owns, the only one it answers. */
export function runtimeNativeSpecifier(specifier: string): string | null {
  const normalized = specifier.startsWith("node:") ? specifier : `node:${specifier}`;
  return RUNTIME_NATIVE_MODULES.has(normalized) ? normalized : null;
}

/**
 * True if a built worker may import `specifier` unbundled: the kernel module,
 * or a module the runtime owns under its `node:` spelling. A bare `path` is
 * not one: the runtime resolves only `node:path`.
 */
export function isRuntimeModuleSpecifier(specifier: string): boolean {
  return specifier === RUNTIME_MODULE_SPECIFIER || RUNTIME_NATIVE_MODULES.has(specifier);
}

/** Where unenv@2's `./*` subpath export points: `./dist/runtime/*.mjs`. */
function unenvModule(subpath: string): string {
  return `${UNENV_ROOT}dist/runtime/${subpath}.mjs`;
}

type BuiltinResolution = { id: string; external?: true } | { resolveFurther: string };

/**
 * How server code reaches a Node built-in, in the worker build, in dev, and
 * in dev's dependency optimizer:
 * - a module the runtime implements is imported as `node:<name>`, the only
 *   spelling the runtime answers. A CommonJS `require` of one resolves to a
 *   stand-in ES module that re-exports it (`RUNTIME_REQUIRE_PREFIX`), because
 *   the isolate has no `require`;
 * - any other built-in resolves to its polyfill, which is bundled.
 * Null when `id` is not a built-in. `resolveFurther` names a polyfill
 * specifier the caller resolves with its own resolver.
 */
function resolveNodeBuiltin(id: string, kind: string | undefined): BuiltinResolution | null {
  const native = runtimeNativeSpecifier(id);
  if (native) {
    return kind === "require-call"
      ? { id: RUNTIME_REQUIRE_PREFIX + native }
      : { id: native, external: true };
  }
  // Bare `unenv/*` specifiers — emitted by @rollup/plugin-inject for inject
  // targets like `process` → `unenv/node/process`. The user app doesn't
  // depend on unenv directly, so we resolve from vite-plugin's own copy.
  if (id.startsWith("unenv/")) return { id: unenvModule(id.slice("unenv/".length)) };
  const resolved = getNodeCompatId(id);
  if (!resolved) return null;
  // Custom polyfills → virtual module ID
  if (resolved.startsWith(CUSTOM_PREFIX)) return { id: resolved };
  if (resolved.startsWith("unenv/")) return { id: unenvModule(resolved.slice("unenv/".length)) };
  return { resolveFurther: resolved };
}

/** Load the modules `resolveNodeBuiltin` names. */
function loadNodeBuiltin(id: string): string | null {
  if (id.startsWith(RUNTIME_REQUIRE_PREFIX)) {
    return runtimeRequireModule(id.slice(RUNTIME_REQUIRE_PREFIX.length));
  }
  return getCustomPolyfillCode(id);
}

/**
 * `resolveNodeBuiltin` for dev's dependency optimizer, which pre-bundles
 * server dependencies with Rolldown and runs none of the app's Vite plugins.
 * - The kernel module stays an external under either kind of import; a
 *   require of it gets the same stand-in the worker build uses.
 * - A CommonJS `require` of a built-in resolves as it does in the worker
 *   build: left to the optimizer, it becomes `createRequire` from
 *   `node:module`, which the isolate has not got.
 * - An ES import of a built-in stays an external, which the dev environment
 *   answers.
 */
export function nodeCompatOptimizerPlugin(): Rolldown.Plugin {
  return {
    name: "zeroship:node-compat-deps",
    async resolveId(id, _importer, options) {
      if (id === RUNTIME_MODULE_SPECIFIER) {
        return options?.kind === "require-call"
          ? RUNTIME_REQUIRE_PREFIX + id
          : { id, external: true };
      }
      if (options?.kind !== "require-call") return null;
      const resolved = resolveNodeBuiltin(id, options.kind);
      if (resolved == null) return null;
      return "resolveFurther" in resolved ? this.resolve(resolved.resolveFurther) : resolved;
    },
    load(id) {
      return loadNodeBuiltin(id);
    },
  };
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

    async resolveId(id: string, _importer: string | undefined, options?: { kind?: string }) {
      // Apply to server code: the "zeroship" environment in dev and in
      // `vite build`, and the dev archive's `ssr` build. Skip the client
      // environment so a bundle that incidentally references `node:`
      // doesn't get polyfilled into the browser asset.
      const envName = (this as any).environment?.name;
      if (envName === "client") return null;
      const resolved = resolveNodeBuiltin(id, options?.kind);
      if (resolved == null) return null;
      // Fallback: let Vite resolve the polyfill specifier normally.
      return "resolveFurther" in resolved ? this.resolve(resolved.resolveFurther) : resolved;
    },

    load(id: string) {
      return loadNodeBuiltin(id);
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
 * Dev only, and gated to the zeroship environment so the browser bundle
 * stays Node-free. A built worker needs no injection: the runtime installs
 * these globals on every isolate, and an injected `process` import would
 * also shield `process.env.NODE_ENV` from the build's static replacement.
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
    apply: "serve",
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
