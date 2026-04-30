// Benchmark fixture — exercises the full zeroship runtime surface.
//
// Wire (post-redesign): each export is a procedure on
// `POST /_zs/v1/<id>` with body = superjson `{ json: <input> }`.
// Single-arg dispatch: the unwrapped `json` value is the handler's
// argument (or `undefined` for no-arg procedures).
//
// Mirrors the scenarios in `crates/runtime/benches/scenarios.js` but
// goes through the real vite-plugin transform → synthetic SSR entry,
// so the dispatch path matches what real apps actually run.

// ── Sync ──────────────────────────────────────────────────────────────

export function ping() {
  return "pong";
}
ping.config = { id: "ping", kind: "query" } as const;

export function fib(n: number) {
  function go(k: number): number { return k <= 1 ? k : go(k - 1) + go(k - 2); }
  return go(n);
}
fib.config = { id: "fib", kind: "query" } as const;

// ── Async — timers, promises, external fetch ─────────────────────────

export function timeout0() {
  return new Promise((resolve) => setTimeout(() => resolve("done"), 0));
}
timeout0.config = { id: "timeout0", kind: "query" } as const;

export function promiseChain() {
  return Promise.resolve(1)
    .then((v) => v + 10)
    .then((v) => v * 2);
}
promiseChain.config = { id: "promiseChain", kind: "query" } as const;

export function promiseChainTimeout() {
  return new Promise<number>((resolve) => setTimeout(() => resolve(1), 100))
    .then((v) => v + 10)
    .then((v) => v * 2);
}
promiseChainTimeout.config = { id: "promiseChainTimeout", kind: "query" } as const;

export async function fetchExternal(url: string) {
  const resp = await fetch(url);
  await resp.json();
  return { status: resp.status, url: resp.url };
}
fetchExternal.config = { id: "fetchExternal", kind: "query" } as const;

// ── Crypto — Web Crypto API ──────────────────────────────────────────

export function uuid() {
  return crypto.randomUUID();
}
uuid.config = { id: "uuid", kind: "query" } as const;

export function randomBytes() {
  const buf = new Uint8Array(32);
  crypto.getRandomValues(buf);
  return buf.length;
}
randomBytes.config = { id: "randomBytes", kind: "query" } as const;

export async function sha256() {
  const data = new TextEncoder().encode("hello world benchmark data for hashing");
  const hash = await crypto.subtle.digest("SHA-256", data);
  return new Uint8Array(hash).length;
}
sha256.config = { id: "sha256", kind: "query" } as const;

// HMAC key cached across requests (realistic — apps import key once).
let _hmacKey: CryptoKey | null = null;
async function getHmacKey() {
  if (!_hmacKey) {
    _hmacKey = await crypto.subtle.importKey(
      "raw",
      new TextEncoder().encode("benchmark-secret-key-32bytes!!!!"),
      { name: "HMAC", hash: "SHA-256" },
      false,
      ["sign", "verify"],
    );
  }
  return _hmacKey;
}

export async function hmacSign() {
  const key = await getHmacKey();
  const sig = await crypto.subtle.sign(
    "HMAC",
    key,
    new TextEncoder().encode("message to sign for benchmark"),
  );
  return new Uint8Array(sig).length;
}
hmacSign.config = { id: "hmacSign", kind: "query" } as const;

export async function hmacVerify() {
  const key = await getHmacKey();
  const data = new TextEncoder().encode("message to sign for benchmark");
  const sig = await crypto.subtle.sign("HMAC", key, data);
  return await crypto.subtle.verify("HMAC", key, sig, data);
}
hmacVerify.config = { id: "hmacVerify", kind: "query" } as const;

let _aesKey: CryptoKey | null = null;
let _aesIv: Uint8Array | null = null;
export async function aesEncrypt() {
  if (!_aesKey) {
    _aesKey = await crypto.subtle.generateKey(
      { name: "AES-GCM", length: 256 },
      false,
      ["encrypt", "decrypt"],
    );
    _aesIv = new Uint8Array(12);
    crypto.getRandomValues(_aesIv);
  }
  const data = new TextEncoder().encode(
    "secret payload for AES-GCM encryption benchmark test",
  );
  const ct = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv: _aesIv! },
    _aesKey,
    data,
  );
  return new Uint8Array(ct).length;
}
aesEncrypt.config = { id: "aesEncrypt", kind: "query" } as const;

let _ecKp: CryptoKeyPair | null = null;
export async function ecdsaSign() {
  if (!_ecKp) {
    _ecKp = await crypto.subtle.generateKey(
      { name: "ECDSA", namedCurve: "P-256" },
      false,
      ["sign", "verify"],
    ) as CryptoKeyPair;
  }
  const sig = await crypto.subtle.sign(
    { name: "ECDSA", hash: "SHA-256" },
    _ecKp.privateKey,
    new TextEncoder().encode("ECDSA benchmark message"),
  );
  return new Uint8Array(sig).length;
}
ecdsaSign.config = { id: "ecdsaSign", kind: "query" } as const;

// ── Streaming ────────────────────────────────────────────────────────
//
// Async generator → SSE on the wire. The runtime / synthetic entry
// detects async-iter and encodes as AI-SDK Data Stream format.

export async function* drip() {
  for (let i = 0; i < 10; i++) {
    yield { i, t: Date.now() };
  }
}
drip.config = { id: "drip", kind: "stream" } as const;

// ── HTTP fall-through ────────────────────────────────────────────────
//
// Anything not on /_zs/v1/<id> goes through default.fetch (the
// synthetic entry's _zsFetch). For bench scenarios that hit raw HTTP
// (e.g. /sse, /ping) we surface a default.fetch handler here so the
// procedure-only build still answers them.

const PONG = '"pong"';

export default {
  fetch(request: Request): Response {
    const url = new URL(request.url);
    if (url.pathname === "/ping") {
      return new Response(PONG, {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }
    if (url.pathname === "/wjson") {
      return Response.json({ ok: true });
    }
    return new Response("Not Found", { status: 404 });
  },
};
