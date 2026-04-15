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

// Import the nodeless preset from unenv at build time.
// eslint-disable-next-line @typescript-eslint/no-var-requires
const { nodeless } = require("unenv") as { nodeless: { alias: Record<string, string> } };
const unenvAliases: Record<string, string> = nodeless.alias;

// ── Custom overrides (modules where unenv is insufficient) ────────────────

const CUSTOM_PREFIX = "\0zeroship-node:";

/** Custom polyfill code for modules unenv doesn't implement. */
const customPolyfills: Record<string, string> = {
  // node:crypto — uses native Rust __cryptoHashSync/__cryptoHmacSync
  "node:crypto": `
export function createHash(algorithm) {
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

export function createHmac(algorithm, key) {
  const keyStr = typeof key === "string" ? key : new TextDecoder().decode(key);
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

export function randomUUID() { return crypto.randomUUID(); }
export function randomBytes(size) { const b = new Uint8Array(size); crypto.getRandomValues(b); return b; }
export function randomFillSync(buf) { crypto.getRandomValues(buf); return buf; }
export function randomFill(buf, ...args) { const cb = args[args.length-1]; crypto.getRandomValues(buf); cb(null, buf); }
export function randomInt(min, max) { if (max===undefined){max=min;min=0;} const a=new Uint32Array(1); crypto.getRandomValues(a); return min+(a[0]%(max-min)); }
export function getRandomValues(buf) { return crypto.getRandomValues(buf); }
export const webcrypto = crypto;
export const subtle = crypto.subtle;
export const fips = false;
export const constants = {};
export default { createHash, createHmac, randomUUID, randomBytes, randomFillSync, randomFill, randomInt, getRandomValues, webcrypto, subtle, fips, constants };
`,

  // node:timers/promises — unenv stubs this as a proxy (doesn't work)
  "node:timers/promises": `
export function setTimeout(ms, value) { return new Promise(r => globalThis.setTimeout(() => r(value), ms || 0)); }
export function setImmediate(value) { return Promise.resolve(value); }
export default { setTimeout, setImmediate };
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
      // Only for zeroship environment
      const envName = (this as any).environment?.name;
      if (envName && envName !== "zeroship") return null;

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
