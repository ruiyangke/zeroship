/**
 * zeroship:node-compat — Routes node:* imports to unenv polyfills.
 *
 * Uses unenv's nodeless preset for most modules. Overrides:
 *   - node:crypto → custom polyfill using native __cryptoHashSync/__cryptoHmacSync
 *   - node:timers/promises → real implementation (unenv stubs it)
 *
 * For the fetchModule path (ModuleRunner), node:* are intercepted in
 * ZeroshipDevEnvironment.fetchModule() which calls getNodeCompatId().
 */

import type { Plugin } from "vite";

// ── unenv alias map ───────────────────────────────────────────────────────

// Import the nodeless preset from unenv.
import { createRequire } from "node:module";
const _require = createRequire(import.meta.url);
const { nodeless } = _require("unenv") as { nodeless: { alias: Record<string, string> } };

// Use unenv specifiers as-is — Vite resolves them via optimizeDeps.
// unenv must be installed in the user's project (added as peerDep).
const unenvAliases: Record<string, string> = nodeless.alias;

// ── Custom overrides (modules where unenv is insufficient) ────────────────

const CUSTOM_PREFIX = "\0zeroship-node:";

/** Custom polyfill code for modules unenv doesn't implement. */
const customPolyfills: Record<string, string> = {
  // node:crypto — uses native Rust __cryptoHashSync/__cryptoHmacSync
  // NOTE: Uses __vite_ssr_exports__ instead of export (code runs in eval context)
  "node:crypto": `
function createHash(algorithm) {
  const chunks = [];
  return {
    update(data) { chunks.push(typeof data === "string" ? data : new TextDecoder().decode(data)); return this; },
    digest(encoding) {
      const hex = __cryptoHashSync(algorithm, chunks.join(""));
      if (!encoding || encoding === "hex") return hex;
      if (encoding === "base64") { const b = new Uint8Array(hex.match(/.{2}/g).map(x => parseInt(x, 16))); return btoa(String.fromCharCode(...b)); }
      if (encoding === "base64url") { const b = new Uint8Array(hex.match(/.{2}/g).map(x => parseInt(x, 16))); return btoa(String.fromCharCode(...b)).replace(/\\+/g,"-").replace(/\\//g,"_").replace(/=+$/,""); }
      return new Uint8Array(hex.match(/.{2}/g).map(x => parseInt(x, 16)));
    },
    copy() { const c = createHash(algorithm); chunks.forEach(ch => c.update(ch)); return c; },
  };
}

function createHmac(algorithm, key) {
  const keyStr = typeof key === "string" ? key : Array.from(new Uint8Array(key.buffer || key)).map(b => b.toString(16).padStart(2, "0")).join("");
  const chunks = [];
  return {
    update(data) { chunks.push(typeof data === "string" ? data : new TextDecoder().decode(data)); return this; },
    digest(encoding) {
      const hex = __cryptoHmacSync(algorithm, keyStr, chunks.join(""));
      if (!encoding || encoding === "hex") return hex;
      if (encoding === "base64") { const b = new Uint8Array(hex.match(/.{2}/g).map(x => parseInt(x, 16))); return btoa(String.fromCharCode(...b)); }
      return hex;
    },
    copy() { return createHmac(algorithm, keyStr); },
  };
}

function randomUUID() { return crypto.randomUUID(); }
function randomBytes(size) { const b = new Uint8Array(size); crypto.getRandomValues(b); return b; }
function randomFillSync(buf) { crypto.getRandomValues(buf); return buf; }
function randomFill(buf, ...args) { const cb = args[args.length-1]; crypto.getRandomValues(buf); cb(null, buf); }
function randomInt(min, max) {
  if (max === undefined) { max = min; min = 0; }
  const range = max - min;
  if (range <= 0) throw new RangeError("max must be greater than min");
  const limit = Math.floor(0x100000000 / range) * range;
  let val;
  do {
    const a = new Uint32Array(1);
    crypto.getRandomValues(a);
    val = a[0];
  } while (val >= limit);
  return min + (val % range);
}
function getRandomValues(buf) { return crypto.getRandomValues(buf); }
const webcrypto = crypto;
const subtle = crypto.subtle;
const fips = false;
const constants = {};
const _default = { createHash, createHmac, randomUUID, randomBytes, randomFillSync, randomFill, randomInt, getRandomValues, webcrypto, subtle, fips, constants };

Object.assign(__vite_ssr_exports__, { createHash, createHmac, randomUUID, randomBytes, randomFillSync, randomFill, randomInt, getRandomValues, webcrypto, subtle, fips, constants, default: _default });
`,

  // node:timers/promises — unenv stubs this as a proxy (doesn't work)
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

  // node:module — unenv aliases this to `unenv/runtime/mock/proxy-cjs`,
  // which itself uses the CJS-interop helpers exported by Vite's optimizer
  // chunk. The optimizer chunk imports node:module first to wire those
  // helpers, creating a circular dependency: chunk → node:module →
  // proxy-cjs → chunk.t. By the time proxy-cjs reads `chunk.t`, the
  // helper hasn't been assigned yet, so the eval throws
  // "(0 , __vite_ssr_import_0__.t) is not a function".
  // Inline a tiny stub here to break the cycle. Real Node code that needs
  // createRequire is rare in our app surface; deepagents only reaches it
  // via fs-bundler internals that don't actually invoke require at
  // runtime.
  "node:module": `
const _noop = function() { return new Proxy(function(){}, { get: () => _noop, apply: () => undefined, construct: () => ({}) }); };
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

  // node:process — unenv's polyfill clobbers process.versions = {}, which
  // breaks libraries that read process.versions.node.split(".") during
  // top-level evaluation (e.g. @nodelib/fs.scandir bundled by deepagents).
  // Delegate to the Rust runtime's globalThis.process (installed by
  // setup_globals — see crates/runtime/src/init.rs) which already has
  // proper versions set.
  "node:process": `
const _proc = globalThis.process;
Object.assign(__vite_ssr_exports__, {
  default: _proc,
  env: _proc.env,
  argv: _proc.argv,
  argv0: _proc.argv0,
  pid: _proc.pid,
  ppid: _proc.ppid,
  title: _proc.title,
  versions: _proc.versions,
  version: _proc.version,
  platform: _proc.platform,
  arch: _proc.arch,
  release: _proc.release,
  cwd: _proc.cwd.bind(_proc),
  chdir: _proc.chdir.bind(_proc),
  exit: _proc.exit.bind(_proc),
  nextTick: _proc.nextTick.bind(_proc),
  hrtime: _proc.hrtime,
  stdout: _proc.stdout,
  stderr: _proc.stderr,
  stdin: _proc.stdin,
  emitWarning: _proc.emitWarning ? _proc.emitWarning.bind(_proc) : () => {},
  on: _proc.on.bind(_proc),
  off: _proc.off.bind(_proc),
  once: _proc.once.bind(_proc),
  removeListener: _proc.removeListener.bind(_proc),
  removeAllListeners: _proc.removeAllListeners.bind(_proc),
  listeners: _proc.listeners.bind(_proc),
  addListener: _proc.addListener.bind(_proc),
  setMaxListeners: _proc.setMaxListeners.bind(_proc),
  getMaxListeners: _proc.getMaxListeners.bind(_proc),
  eventNames: _proc.eventNames.bind(_proc),
});
`,
};

// ── Public API (used by environment.ts fetchModule override) ──────────────

/**
 * Given a node:* specifier, returns the rewritten module ID for Vite to resolve.
 * - Custom modules → virtual ID (CUSTOM_PREFIX + specifier)
 * - unenv modules → unenv package path (e.g. "unenv/runtime/node/buffer/index")
 * - Unknown → null (let Vite handle it)
 */
export function getNodeCompatId(specifier: string): string | null {
  // Normalize bare → node: prefix
  const normalized = specifier.startsWith("node:") ? specifier : `node:${specifier}`;

  // Check custom overrides first
  if (normalized in customPolyfills) return CUSTOM_PREFIX + normalized;

  // Check unenv alias
  const unenvId = unenvAliases[normalized] ?? unenvAliases[specifier];
  if (unenvId) return unenvId;

  return null;
}

/**
 * If the specifier is a custom polyfill (virtual module), return its code.
 */
export function getCustomPolyfillCode(id: string): string | null {
  if (!id.startsWith(CUSTOM_PREFIX)) return null;
  const specifier = id.slice(CUSTOM_PREFIX.length);
  return customPolyfills[specifier] ?? null;
}

// ── Vite plugin ───────────────────────────────────────────────────────────

export function nodeCompatPlugin(): Plugin {
  return {
    name: "zeroship:node-compat",
    enforce: "pre" as const,

    resolveId(id: string) {
      // Apply node-compat for the dev "zeroship" environment AND
      // the production SSR build (which doesn't have a named
      // environment). Skip for the regular client environment so a
      // bundle that incidentally references `node:` doesn't get
      // polyfilled into the browser asset.
      const envName = (this as any).environment?.name;
      if (envName === "client") return null;

      const resolved = getNodeCompatId(id);
      if (!resolved) return null;

      // Custom polyfills → virtual module ID
      if (resolved.startsWith(CUSTOM_PREFIX)) return resolved;

      // unenv paths → let Vite resolve normally
      return this.resolve(resolved);
    },

    load(id: string) {
      return getCustomPolyfillCode(id) ?? null;
    },
  };
}
