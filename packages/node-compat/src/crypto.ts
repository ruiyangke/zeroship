// node:crypto polyfill for zeroship V8 runtime.
//
// Uses native Rust ops for hash/hmac (registered as __cryptoHashSync/__cryptoHmacSync
// on the global object by the runtime). Falls back to WebCrypto for randomness.

declare function __cryptoHashSync(algorithm: string, data: string): string;
declare function __cryptoHmacSync(algorithm: string, key: string, data: string): string;

// ── Hash ──────────────────────────────────────────────────────────────────

interface HashInstance {
  update(data: string | Uint8Array, encoding?: string): HashInstance;
  digest(encoding?: string): string | Uint8Array;
  copy(): HashInstance;
}

export function createHash(algorithm: string): HashInstance {
  const chunks: string[] = [];
  const instance: HashInstance = {
    update(data: string | Uint8Array): HashInstance {
      chunks.push(typeof data === "string" ? data : new TextDecoder().decode(data));
      return instance;
    },
    digest(encoding?: string): any {
      const hex = __cryptoHashSync(algorithm, chunks.join(""));
      if (!encoding || encoding === "hex") return hex;
      if (encoding === "base64") return hexToBase64(hex);
      if (encoding === "base64url") return hexToBase64(hex).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
      if (encoding === "buffer" || !encoding) return hexToBytes(hex);
      return hex;
    },
    copy(): HashInstance {
      const c = createHash(algorithm);
      for (const chunk of chunks) c.update(chunk);
      return c;
    },
  };
  return instance;
}

// ── HMAC ──────────────────────────────────────────────────────────────────

export function createHmac(algorithm: string, key: string | Uint8Array): HashInstance {
  const keyStr = typeof key === "string" ? key : new TextDecoder().decode(key);
  const chunks: string[] = [];
  const instance: HashInstance = {
    update(data: string | Uint8Array): HashInstance {
      chunks.push(typeof data === "string" ? data : new TextDecoder().decode(data));
      return instance;
    },
    digest(encoding?: string): any {
      const hex = __cryptoHmacSync(algorithm, keyStr, chunks.join(""));
      if (!encoding || encoding === "hex") return hex;
      if (encoding === "base64") return hexToBase64(hex);
      if (encoding === "base64url") return hexToBase64(hex).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
      return hex;
    },
    copy(): HashInstance {
      const c = createHmac(algorithm, keyStr);
      for (const chunk of chunks) c.update(chunk);
      return c;
    },
  };
  return instance;
}

// ── Random ────────────────────────────────────────────────────────────────

export function randomUUID(): string { return crypto.randomUUID(); }

export function randomBytes(size: number): Uint8Array {
  const buf = new Uint8Array(size);
  crypto.getRandomValues(buf);
  return buf;
}

export function randomFillSync<T extends ArrayBufferView>(buf: T): T {
  crypto.getRandomValues(buf);
  return buf;
}

export function randomFill(buf: ArrayBufferView, cb: (err: Error | null, buf: ArrayBufferView) => void): void;
export function randomFill(buf: ArrayBufferView, offset: number, cb: (err: Error | null, buf: ArrayBufferView) => void): void;
export function randomFill(buf: ArrayBufferView, ...args: any[]): void {
  const cb = args[args.length - 1] as (err: Error | null, buf: ArrayBufferView) => void;
  crypto.getRandomValues(buf);
  cb(null, buf);
}

export function randomInt(min: number, max?: number): number {
  if (max === undefined) { max = min; min = 0; }
  const range = max - min;
  const arr = new Uint32Array(1);
  crypto.getRandomValues(arr);
  return min + (arr[0] % range);
}

export function getRandomValues<T extends ArrayBufferView>(buf: T): T {
  return crypto.getRandomValues(buf);
}

// ── WebCrypto ─────────────────────────────────────────────────────────────

export const webcrypto = crypto;
export const subtle = crypto.subtle;

// ── Stubs (throw on use) ──────────────────────────────────────────────────

function notImpl(name: string): (...args: any[]) => never {
  return () => { throw new Error(`crypto.${name} is not implemented in zeroship runtime`); };
}

export const fips = false;
export const constants = {};
export const createCipheriv = notImpl("createCipheriv");
export const createDecipheriv = notImpl("createDecipheriv");
export const createSign = notImpl("createSign");
export const createVerify = notImpl("createVerify");
export const createDiffieHellman = notImpl("createDiffieHellman");
export const createECDH = notImpl("createECDH");
export const checkPrime = notImpl("checkPrime");
export const checkPrimeSync = notImpl("checkPrimeSync");
export const generatePrime = notImpl("generatePrime");
export const generatePrimeSync = notImpl("generatePrimeSync");
export const generateKey = notImpl("generateKey");
export const generateKeySync = notImpl("generateKeySync");
export const generateKeyPair = notImpl("generateKeyPair");
export const generateKeyPairSync = notImpl("generateKeyPairSync");
export const pbkdf2 = notImpl("pbkdf2");
export const pbkdf2Sync = notImpl("pbkdf2Sync");
export const scrypt = notImpl("scrypt");
export const scryptSync = notImpl("scryptSync");
export const sign = notImpl("sign");
export const verify = notImpl("verify");

// ── Helpers ───────────────────────────────────────────────────────────────

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < hex.length; i += 2) {
    bytes[i / 2] = parseInt(hex.substring(i, i + 2), 16);
  }
  return bytes;
}

function hexToBase64(hex: string): string {
  const bytes = hexToBytes(hex);
  return btoa(String.fromCharCode(...bytes));
}

// ── Default export ────────────────────────────────────────────────────────

export default {
  createHash, createHmac, randomUUID, randomBytes, randomFillSync, randomFill,
  randomInt, getRandomValues, webcrypto, subtle, fips, constants,
};
