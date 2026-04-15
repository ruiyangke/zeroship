/**
 * zeroship:node-compat — Virtual module polyfills for Node.js APIs.
 *
 * When the zeroship environment encounters `import { X } from "node:crypto"`,
 * this plugin resolves it to a virtual module that provides the API using
 * Web APIs available in the V8 runtime (WebCrypto, ReadableStream, etc.).
 *
 * Only active for the "zeroship" environment — Node.js environments
 * resolve node: modules natively.
 */

import type { Plugin } from "vite";

const COMPAT_PREFIX = "\0node-compat:";

/** Map of node: specifiers → polyfill source code. */
const polyfills: Record<string, string> = {

  // ── node:crypto ──────────────────────────────────────────────────────
  "node:crypto": `
// Pure JS SHA-256 for sync createHash (WebCrypto is async-only)
const K = new Uint32Array([
  0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
  0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
  0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
  0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
  0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
  0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
  0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
  0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2
]);

function sha256(data) {
  const bytes = typeof data === "string" ? new TextEncoder().encode(data) : new Uint8Array(data);
  let h0=0x6a09e667,h1=0xbb67ae85,h2=0x3c6ef372,h3=0xa54ff53a,h4=0x510e527f,h5=0x9b05688c,h6=0x1f83d9ab,h7=0x5be0cd19;
  const len = bytes.length;
  const bitLen = len * 8;
  const padded = new Uint8Array(((len + 9 + 63) & ~63));
  padded.set(bytes);
  padded[len] = 0x80;
  const dv = new DataView(padded.buffer);
  dv.setUint32(padded.length - 4, bitLen, false);
  const w = new Uint32Array(64);
  for (let off = 0; off < padded.length; off += 64) {
    for (let i = 0; i < 16; i++) w[i] = dv.getUint32(off + i * 4, false);
    for (let i = 16; i < 64; i++) {
      const s0 = ((w[i-15]>>>7)|(w[i-15]<<25)) ^ ((w[i-15]>>>18)|(w[i-15]<<14)) ^ (w[i-15]>>>3);
      const s1 = ((w[i-2]>>>17)|(w[i-2]<<15)) ^ ((w[i-2]>>>19)|(w[i-2]<<13)) ^ (w[i-2]>>>10);
      w[i] = (w[i-16] + s0 + w[i-7] + s1) | 0;
    }
    let a=h0,b=h1,c=h2,d=h3,e=h4,f=h5,g=h6,h=h7;
    for (let i = 0; i < 64; i++) {
      const S1 = ((e>>>6)|(e<<26)) ^ ((e>>>11)|(e<<21)) ^ ((e>>>25)|(e<<7));
      const ch = (e & f) ^ (~e & g);
      const t1 = (h + S1 + ch + K[i] + w[i]) | 0;
      const S0 = ((a>>>2)|(a<<30)) ^ ((a>>>13)|(a<<19)) ^ ((a>>>22)|(a<<10));
      const maj = (a & b) ^ (a & c) ^ (b & c);
      const t2 = (S0 + maj) | 0;
      h=g; g=f; f=e; e=(d+t1)|0; d=c; c=b; b=a; a=(t1+t2)|0;
    }
    h0=(h0+a)|0; h1=(h1+b)|0; h2=(h2+c)|0; h3=(h3+d)|0; h4=(h4+e)|0; h5=(h5+f)|0; h6=(h6+g)|0; h7=(h7+h)|0;
  }
  return new Uint8Array(new Uint32Array([h0,h1,h2,h3,h4,h5,h6,h7]).buffer).reduce((s,b) => {
    const dv2 = new DataView(new ArrayBuffer(4));
    return s;
  }, "");
}

function toHex(buf) {
  return Array.from(new Uint8Array(buf)).map(b => b.toString(16).padStart(2, '0')).join('');
}

function sha256Hex(data) {
  const bytes = typeof data === "string" ? new TextEncoder().encode(data) : new Uint8Array(data);
  let h0=0x6a09e667,h1=0xbb67ae85,h2=0x3c6ef372,h3=0xa54ff53a,h4=0x510e527f,h5=0x9b05688c,h6=0x1f83d9ab,h7=0x5be0cd19;
  const len = bytes.length;
  const bitLen = len * 8;
  const padded = new Uint8Array(((len + 9 + 63) & ~63));
  padded.set(bytes);
  padded[len] = 0x80;
  const dv = new DataView(padded.buffer);
  dv.setUint32(padded.length - 4, bitLen, false);
  const w = new Uint32Array(64);
  for (let off = 0; off < padded.length; off += 64) {
    for (let i = 0; i < 16; i++) w[i] = dv.getUint32(off + i * 4, false);
    for (let i = 16; i < 64; i++) {
      const s0 = ((w[i-15]>>>7)|(w[i-15]<<25)) ^ ((w[i-15]>>>18)|(w[i-15]<<14)) ^ (w[i-15]>>>3);
      const s1 = ((w[i-2]>>>17)|(w[i-2]<<15)) ^ ((w[i-2]>>>19)|(w[i-2]<<13)) ^ (w[i-2]>>>10);
      w[i] = (w[i-16] + s0 + w[i-7] + s1) | 0;
    }
    let a=h0,b=h1,c=h2,d=h3,e=h4,f=h5,g=h6,h=h7;
    for (let i = 0; i < 64; i++) {
      const S1 = ((e>>>6)|(e<<26)) ^ ((e>>>11)|(e<<21)) ^ ((e>>>25)|(e<<7));
      const ch = (e & f) ^ (~e & g);
      const t1 = (h + S1 + ch + K[i] + w[i]) | 0;
      const S0 = ((a>>>2)|(a<<30)) ^ ((a>>>13)|(a<<19)) ^ ((a>>>22)|(a<<10));
      const maj = (a & b) ^ (a & c) ^ (b & c);
      const t2 = (S0 + maj) | 0;
      h=g; g=f; f=e; e=(d+t1)|0; d=c; c=b; b=a; a=(t1+t2)|0;
    }
    h0=(h0+a)|0; h1=(h1+b)|0; h2=(h2+c)|0; h3=(h3+d)|0; h4=(h4+e)|0; h5=(h5+f)|0; h6=(h6+g)|0; h7=(h7+h)|0;
  }
  const result = new DataView(new ArrayBuffer(32));
  result.setUint32(0,h0,false); result.setUint32(4,h1,false); result.setUint32(8,h2,false); result.setUint32(12,h3,false);
  result.setUint32(16,h4,false); result.setUint32(20,h5,false); result.setUint32(24,h6,false); result.setUint32(28,h7,false);
  return toHex(new Uint8Array(result.buffer));
}

export function createHash(algorithm) {
  if (algorithm !== "sha256" && algorithm !== "sha-256") {
    throw new Error("node-compat: createHash only supports sha256, got " + algorithm);
  }
  let chunks = [];
  return {
    update(data, encoding) {
      if (typeof data === "string") chunks.push(data);
      else chunks.push(new TextDecoder().decode(data));
      return this;
    },
    digest(encoding) {
      const input = chunks.join("");
      const hex = sha256Hex(input);
      if (encoding === "hex") return hex;
      if (encoding === "base64") {
        const bytes = new Uint8Array(hex.match(/.{2}/g).map(b => parseInt(b, 16)));
        return btoa(String.fromCharCode(...bytes));
      }
      // Return as Uint8Array for no encoding
      return new Uint8Array(hex.match(/.{2}/g).map(b => parseInt(b, 16)));
    },
    copy() { return createHash(algorithm).update(chunks.join("")); },
  };
}

export function randomUUID() { return crypto.randomUUID(); }

export function randomBytes(size) {
  const buf = new Uint8Array(size);
  crypto.getRandomValues(buf);
  // Add Buffer-like methods
  buf.toString = function(encoding) {
    if (encoding === "hex") return toHex(this);
    if (encoding === "base64") return btoa(String.fromCharCode(...this));
    return new TextDecoder().decode(this);
  };
  return buf;
}

export function randomFillSync(buf) {
  crypto.getRandomValues(buf);
  return buf;
}

export function getRandomValues(buf) {
  return crypto.getRandomValues(buf);
}

export default { createHash, randomUUID, randomBytes, randomFillSync, getRandomValues };
`,

  // ── node:buffer ──────────────────────────────────────────────────────
  "node:buffer": `
function toHex(buf) {
  return Array.from(new Uint8Array(buf)).map(b => b.toString(16).padStart(2, '0')).join('');
}

class Buffer extends Uint8Array {
  static from(input, encoding) {
    if (typeof input === "string") {
      if (encoding === "base64") {
        const binary = atob(input);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
        return new Buffer(bytes.buffer);
      }
      if (encoding === "hex") {
        const bytes = new Uint8Array(input.match(/.{2}/g).map(b => parseInt(b, 16)));
        return new Buffer(bytes.buffer);
      }
      return new Buffer(new TextEncoder().encode(input).buffer);
    }
    if (input instanceof ArrayBuffer) return new Buffer(input);
    if (ArrayBuffer.isView(input)) return new Buffer(input.buffer, input.byteOffset, input.byteLength);
    if (Array.isArray(input)) return new Buffer(new Uint8Array(input).buffer);
    return new Buffer(input);
  }

  static alloc(size, fill) {
    const buf = new Buffer(size);
    if (fill !== undefined) buf.fill(typeof fill === "string" ? fill.charCodeAt(0) : fill);
    return buf;
  }

  static allocUnsafe(size) { return new Buffer(size); }

  static isBuffer(obj) { return obj instanceof Buffer; }

  static concat(list, length) {
    if (!length) length = list.reduce((s, b) => s + b.length, 0);
    const result = new Buffer(length);
    let offset = 0;
    for (const buf of list) {
      result.set(buf, offset);
      offset += buf.length;
    }
    return result;
  }

  static byteLength(str, encoding) {
    if (typeof str !== "string") return str.length ?? str.byteLength ?? 0;
    return new TextEncoder().encode(str).length;
  }

  toString(encoding) {
    if (encoding === "hex") return toHex(this);
    if (encoding === "base64") return btoa(String.fromCharCode(...this));
    if (encoding === "base64url") return btoa(String.fromCharCode(...this)).replace(/\\+/g, '-').replace(/\\//g, '_').replace(/=+$/, '');
    return new TextDecoder().decode(this);
  }

  toJSON() { return { type: "Buffer", data: Array.from(this) }; }

  write(str, offset, length, encoding) {
    const bytes = new TextEncoder().encode(str);
    this.set(bytes.subarray(0, length ?? bytes.length), offset ?? 0);
    return Math.min(bytes.length, length ?? bytes.length);
  }

  readUInt32BE(offset) { return new DataView(this.buffer, this.byteOffset).getUint32(offset, false); }
  readUInt32LE(offset) { return new DataView(this.buffer, this.byteOffset).getUint32(offset, true); }
  writeUInt32BE(value, offset) { new DataView(this.buffer, this.byteOffset).setUint32(offset, value, false); }
  writeUInt32LE(value, offset) { new DataView(this.buffer, this.byteOffset).setUint32(offset, value, true); }
}

// Make Buffer available globally
globalThis.Buffer = Buffer;

export { Buffer };
export default { Buffer };
`,

  // ── node:path ────────────────────────────────────────────────────────
  "node:path": `
export const sep = "/";
export const delimiter = ":";

export function join(...parts) {
  return parts.filter(Boolean).join("/").replace(/\\/\\/+/g, "/");
}

export function resolve(...parts) {
  let resolved = "";
  for (let i = parts.length - 1; i >= 0; i--) {
    resolved = parts[i] + "/" + resolved;
    if (parts[i].startsWith("/")) break;
  }
  return normalize(resolved);
}

export function normalize(p) {
  const parts = p.split("/").filter(Boolean);
  const result = [];
  for (const part of parts) {
    if (part === "..") result.pop();
    else if (part !== ".") result.push(part);
  }
  return (p.startsWith("/") ? "/" : "") + result.join("/");
}

export function basename(p, ext) {
  const base = p.split("/").pop() || "";
  if (ext && base.endsWith(ext)) return base.slice(0, -ext.length);
  return base;
}

export function dirname(p) {
  const parts = p.split("/");
  parts.pop();
  return parts.join("/") || ".";
}

export function extname(p) {
  const base = basename(p);
  const dot = base.lastIndexOf(".");
  return dot > 0 ? base.slice(dot) : "";
}

export function isAbsolute(p) { return p.startsWith("/"); }

export function relative(from, to) {
  const fromParts = from.split("/").filter(Boolean);
  const toParts = to.split("/").filter(Boolean);
  let common = 0;
  while (common < fromParts.length && common < toParts.length && fromParts[common] === toParts[common]) common++;
  const up = Array(fromParts.length - common).fill("..");
  return [...up, ...toParts.slice(common)].join("/") || ".";
}

export const posix = { join, resolve, normalize, basename, dirname, extname, isAbsolute, relative, sep, delimiter };
export default { join, resolve, normalize, basename, dirname, extname, isAbsolute, relative, sep, delimiter, posix };
`,

  // ── node:async_hooks ─────────────────────────────────────────────────
  "node:async_hooks": `
class AsyncLocalStorage {
  #store = undefined;

  getStore() { return this.#store; }

  run(store, fn, ...args) {
    const prev = this.#store;
    this.#store = store;
    try { return fn(...args); }
    finally { this.#store = prev; }
  }

  enterWith(store) { this.#store = store; }

  disable() { this.#store = undefined; }
}

export { AsyncLocalStorage };
export default { AsyncLocalStorage };
`,

  // ── node:util ────────────────────────────────────────────────────────
  "node:util": `
export function inspect(obj, opts) {
  try { return JSON.stringify(obj, null, 2); }
  catch { return String(obj); }
}

export function format(...args) {
  if (args.length === 0) return "";
  let str = String(args[0]);
  let i = 1;
  str = str.replace(/%[sdifjoO%]/g, (match) => {
    if (match === "%%") return "%";
    if (i >= args.length) return match;
    const arg = args[i++];
    switch (match) {
      case "%s": return String(arg);
      case "%d": case "%i": return Number(arg).toString();
      case "%f": return parseFloat(arg).toString();
      case "%j": case "%o": case "%O": try { return JSON.stringify(arg); } catch { return "[Circular]"; }
      default: return match;
    }
  });
  while (i < args.length) str += " " + String(args[i++]);
  return str;
}

export function promisify(fn) {
  return (...args) => new Promise((resolve, reject) => {
    fn(...args, (err, result) => err ? reject(err) : resolve(result));
  });
}

export function deprecate(fn, msg) { return fn; }
export function inherits(ctor, superCtor) { Object.setPrototypeOf(ctor.prototype, superCtor.prototype); }
export function types() { return {}; }
export function debuglog() { return () => {}; }
export class TextDecoder extends globalThis.TextDecoder {}
export class TextEncoder extends globalThis.TextEncoder {}

export default { inspect, format, promisify, deprecate, inherits, types, debuglog, TextDecoder, TextEncoder };
`,

  // ── node:timers/promises ─────────────────────────────────────────────
  "node:timers/promises": `
export function setTimeout(ms, value) {
  return new Promise(resolve => globalThis.setTimeout(() => resolve(value), ms));
}
export function setInterval(ms, value) { return setTimeout(ms, value); }
export function setImmediate(value) { return Promise.resolve(value); }
export default { setTimeout, setInterval, setImmediate };
`,

  // ── node:timers ──────────────────────────────────────────────────────
  "node:timers": `
export const setTimeout = globalThis.setTimeout;
export const clearTimeout = globalThis.clearTimeout;
export const setInterval = globalThis.setInterval;
export const clearInterval = globalThis.clearInterval;
export const setImmediate = (fn, ...args) => globalThis.setTimeout(() => fn(...args), 0);
export const clearImmediate = globalThis.clearTimeout;
export default { setTimeout, clearTimeout, setInterval, clearInterval, setImmediate, clearImmediate };
`,

  // ── node:stream/web ──────────────────────────────────────────────────
  "node:stream/web": `
export const ReadableStream = globalThis.ReadableStream;
export const WritableStream = globalThis.WritableStream ?? class WritableStream {};
export const TransformStream = globalThis.TransformStream ?? class TransformStream {};
export default { ReadableStream, WritableStream, TransformStream };
`,

  // ── node:stream ──────────────────────────────────────────────────────
  "node:stream": `
import { ReadableStream, WritableStream, TransformStream } from "node:stream/web";

class EventEmitter {
  #listeners = {};
  on(event, fn) { (this.#listeners[event] ??= []).push(fn); return this; }
  off(event, fn) { const l = this.#listeners[event]; if (l) this.#listeners[event] = l.filter(f => f !== fn); return this; }
  emit(event, ...args) { for (const fn of this.#listeners[event] ?? []) fn(...args); return true; }
  once(event, fn) { const wrapper = (...args) => { this.off(event, wrapper); fn(...args); }; return this.on(event, wrapper); }
  removeAllListeners(event) { if (event) delete this.#listeners[event]; else this.#listeners = {}; return this; }
  addListener(event, fn) { return this.on(event, fn); }
  removeListener(event, fn) { return this.off(event, fn); }
}

export class Readable extends EventEmitter {
  constructor(opts) { super(); this.readable = true; }
  pipe(dest) { return dest; }
  read() { return null; }
  destroy() {}
}

export class Writable extends EventEmitter {
  constructor(opts) { super(); this.writable = true; }
  write(chunk) { return true; }
  end() {}
  destroy() {}
}

export class Duplex extends EventEmitter {
  constructor(opts) { super(); this.readable = true; this.writable = true; }
}

export class PassThrough extends Duplex {}

export { EventEmitter, ReadableStream, WritableStream, TransformStream };
export default { Readable, Writable, Duplex, PassThrough, EventEmitter, ReadableStream, WritableStream, TransformStream };
`,

  // ── node:events ──────────────────────────────────────────────────────
  "node:events": `
class EventEmitter {
  #listeners = {};
  #maxListeners = 10;
  on(event, fn) { (this.#listeners[event] ??= []).push(fn); return this; }
  off(event, fn) { const l = this.#listeners[event]; if (l) this.#listeners[event] = l.filter(f => f !== fn); return this; }
  emit(event, ...args) { for (const fn of this.#listeners[event] ?? []) fn(...args); return this.#listeners[event]?.length > 0; }
  once(event, fn) { const w = (...a) => { this.off(event, w); fn(...a); }; return this.on(event, w); }
  removeAllListeners(event) { if (event) delete this.#listeners[event]; else this.#listeners = {}; return this; }
  addListener(event, fn) { return this.on(event, fn); }
  removeListener(event, fn) { return this.off(event, fn); }
  listenerCount(event) { return this.#listeners[event]?.length ?? 0; }
  listeners(event) { return [...(this.#listeners[event] ?? [])]; }
  setMaxListeners(n) { this.#maxListeners = n; return this; }
  getMaxListeners() { return this.#maxListeners; }
  prependListener(event, fn) { (this.#listeners[event] ??= []).unshift(fn); return this; }
  prependOnceListener(event, fn) { const w = (...a) => { this.off(event, w); fn(...a); }; return this.prependListener(event, w); }
  eventNames() { return Object.keys(this.#listeners); }
  rawListeners(event) { return this.listeners(event); }
  static EventEmitter = EventEmitter;
}

export { EventEmitter };
export default EventEmitter;
`,

  // ── node:assert / node:assert/strict ─────────────────────────────────
  "node:assert": `
function assert(value, message) { if (!value) throw new Error(message ?? "Assertion failed"); }
assert.ok = assert;
assert.equal = (a, b, msg) => { if (a != b) throw new Error(msg ?? a + " != " + b); };
assert.strictEqual = (a, b, msg) => { if (a !== b) throw new Error(msg ?? a + " !== " + b); };
assert.notEqual = (a, b, msg) => { if (a == b) throw new Error(msg ?? a + " == " + b); };
assert.notStrictEqual = (a, b, msg) => { if (a === b) throw new Error(msg ?? a + " === " + b); };
assert.deepEqual = assert.deepStrictEqual = (a, b, msg) => { if (JSON.stringify(a) !== JSON.stringify(b)) throw new Error(msg ?? "deepEqual failed"); };
assert.throws = (fn, msg) => { try { fn(); throw new Error(msg ?? "Expected error"); } catch(e) { if (e.message === (msg ?? "Expected error")) throw e; } };
assert.fail = (msg) => { throw new Error(msg ?? "Assert.fail"); };
export default assert;
export { assert };
`,

  "node:assert/strict": `export { default, assert } from "node:assert";`,

  // ── node:fs / node:fs/promises (stub — throws on actual use) ────────
  "node:fs": `
function notImpl(name) { return () => { throw new Error("node:fs." + name + " is not available in zeroship runtime"); }; }
export const readFileSync = notImpl("readFileSync");
export const writeFileSync = notImpl("writeFileSync");
export const existsSync = () => false;
export const mkdtempSync = notImpl("mkdtempSync");
export const statSync = notImpl("statSync");
export const readdirSync = notImpl("readdirSync");
export default { readFileSync, writeFileSync, existsSync, mkdtempSync, statSync, readdirSync };
`,

  "node:fs/promises": `
async function notImpl(name) { throw new Error("node:fs/promises." + name + " is not available in zeroship runtime"); }
export const readFile = () => notImpl("readFile");
export const writeFile = () => notImpl("writeFile");
export const stat = () => notImpl("stat");
export const readdir = () => notImpl("readdir");
export const mkdir = () => notImpl("mkdir");
export const rm = () => notImpl("rm");
export default { readFile, writeFile, stat, readdir, mkdir, rm };
`,

  // ── node:os ──────────────────────────────────────────────────────────
  "node:os": `
export const EOL = "\\n";
export function platform() { return "linux"; }
export function arch() { return "x64"; }
export function tmpdir() { return "/tmp"; }
export function homedir() { return "/"; }
export function hostname() { return "zeroship"; }
export function cpus() { return []; }
export function totalmem() { return 0; }
export function freemem() { return 0; }
export function type() { return "Linux"; }
export function release() { return "0.0.0"; }
export function networkInterfaces() { return {}; }
export default { EOL, platform, arch, tmpdir, homedir, hostname, cpus, totalmem, freemem, type, release, networkInterfaces };
`,

  // ── node:url ─────────────────────────────────────────────────────────
  "node:url": `
export const URL = globalThis.URL;
export const URLSearchParams = globalThis.URLSearchParams;
export function parse(urlStr) { try { const u = new URL(urlStr); return u; } catch { return null; } }
export function format(urlObj) { return String(urlObj); }
export function resolve(from, to) { return new URL(to, from).href; }
export function fileURLToPath(url) { return url.replace("file://", ""); }
export function pathToFileURL(path) { return new URL("file://" + path); }
export default { URL, URLSearchParams, parse, format, resolve, fileURLToPath, pathToFileURL };
`,

  // ── node:http (stub) ─────────────────────────────────────────────────
  "node:http": `
export const METHODS = ["GET","HEAD","POST","PUT","DELETE","PATCH","OPTIONS"];
export const STATUS_CODES = {200:"OK",201:"Created",204:"No Content",301:"Moved",302:"Found",304:"Not Modified",400:"Bad Request",401:"Unauthorized",403:"Forbidden",404:"Not Found",500:"Internal Server Error"};
export class Agent {}
export default { METHODS, STATUS_CODES, Agent };
`,

  // ── node:https (stub) ────────────────────────────────────────────────
  "node:https": `export { default, METHODS, STATUS_CODES, Agent } from "node:http";`,

  // ── node:process ─────────────────────────────────────────────────────
  "node:process": `
const p = globalThis.process ?? { env: {}, version: "v20.0.0", platform: "linux", arch: "x64", argv: [], pid: 1, exit() {} };
export default p;
export const env = p.env;
export const version = p.version;
export const platform = p.platform ?? "linux";
export const argv = p.argv ?? [];
export const pid = p.pid ?? 1;
export const exit = p.exit ?? (() => {});
export const nextTick = (fn, ...args) => queueMicrotask(() => fn(...args));
export const stdout = { write(s) { console.log(s); } };
export const stderr = { write(s) { console.error(s); } };
`,

  // ── node:worker_threads (stub) ───────────────────────────────────────
  "node:worker_threads": `
export const isMainThread = true;
export const parentPort = null;
export const workerData = null;
export class Worker {}
export class BroadcastChannel { constructor() {} postMessage() {} close() {} }
export default { isMainThread, parentPort, workerData, Worker, BroadcastChannel };
`,

  // ── node:diagnostics_channel (stub) ──────────────────────────────────
  "node:diagnostics_channel": `
class Channel { subscribe() {} unsubscribe() {} get hasSubscribers() { return false; } }
export function channel() { return new Channel(); }
export default { channel, Channel };
`,

  // ── node:perf_hooks ──────────────────────────────────────────────────
  "node:perf_hooks": `
export const performance = globalThis.performance;
export default { performance };
`,
};

// Also handle bare specifiers (without node: prefix)
const bareAliases: Record<string, string> = {
  "crypto": "node:crypto",
  "buffer": "node:buffer",
  "path": "node:path",
  "util": "node:util",
  "events": "node:events",
  "stream": "node:stream",
  "stream/web": "node:stream/web",
  "assert": "node:assert",
  "assert/strict": "node:assert/strict",
  "os": "node:os",
  "url": "node:url",
  "http": "node:http",
  "https": "node:https",
  "timers": "node:timers",
  "timers/promises": "node:timers/promises",
  "async_hooks": "node:async_hooks",
  "process": "node:process",
  "fs": "node:fs",
  "fs/promises": "node:fs/promises",
  "worker_threads": "node:worker_threads",
  "diagnostics_channel": "node:diagnostics_channel",
  "perf_hooks": "node:perf_hooks",
};

export function nodeCompatPlugin(): Plugin {
  return {
    name: "zeroship:node-compat",
    enforce: "pre" as const,

    resolveId(id: string) {
      // Only polyfill in the zeroship environment
      if ((this as any).environment?.name !== "zeroship") return null;

      // node: prefixed
      if (id in polyfills) return COMPAT_PREFIX + id;

      // bare specifiers (e.g. "crypto" → "node:crypto")
      const mapped = bareAliases[id];
      if (mapped && mapped in polyfills) return COMPAT_PREFIX + mapped;

      return null;
    },

    load(id: string) {
      if (!id.startsWith(COMPAT_PREFIX)) return null;
      const moduleId = id.slice(COMPAT_PREFIX.length);
      const code = polyfills[moduleId];
      if (!code) return null;
      return { code, moduleSideEffects: true };
    },
  };
}
