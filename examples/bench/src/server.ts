"use server";
// Benchmark fixture — exercises the full zeroship runtime surface.
//
// Wire (post-redesign): each wrapped export is a procedure on
// `POST /_zs/v1/<id>` with body = superjson `{ json: <input> }`.
// Single-arg dispatch: the unwrapped `json` value is the handler's
// argument (or `undefined` for no-arg procedures).
//
// Mirrors the scenarios in `crates/runtime/benches/scenarios.js` but
// goes through the real vite-plugin transform → synthetic SSR entry,
// so the dispatch path matches what real apps actually run.

import { query, stream } from "@zeroship/server";

// ── Sync ──────────────────────────────────────────────────────────────

export const ping = query(() => "pong", { id: "ping" });

export const fib = query(
  (n: number) => {
    function go(k: number): number { return k <= 1 ? k : go(k - 1) + go(k - 2); }
    return go(n);
  },
  { id: "fib" },
);

// ── Async — timers, promises, external fetch ─────────────────────────

export const timeout0 = query(
  () => new Promise((resolve) => setTimeout(() => resolve("done"), 0)),
  { id: "timeout0" },
);

export const promiseChain = query(
  () => Promise.resolve(1).then((v) => v + 10).then((v) => v * 2),
  { id: "promiseChain" },
);

export const promiseChainTimeout = query(
  () =>
    new Promise<number>((resolve) => setTimeout(() => resolve(1), 100))
      .then((v) => v + 10)
      .then((v) => v * 2),
  { id: "promiseChainTimeout" },
);

export const fetchExternal = query(
  async (url: string) => {
    const resp = await fetch(url);
    await resp.json();
    return { status: resp.status, url: resp.url };
  },
  { id: "fetchExternal" },
);

// ── Crypto — Web Crypto API ──────────────────────────────────────────

export const uuid = query(() => crypto.randomUUID(), { id: "uuid" });

export const randomBytes = query(
  () => {
    const buf = new Uint8Array(32);
    crypto.getRandomValues(buf);
    return buf.length;
  },
  { id: "randomBytes" },
);

export const sha256 = query(
  async () => {
    const data = new TextEncoder().encode("hello world benchmark data for hashing");
    const hash = await crypto.subtle.digest("SHA-256", data);
    return new Uint8Array(hash).length;
  },
  { id: "sha256" },
);

// HMAC key cached across requests (realistic — apps import key once).
// `getHmacKey` is a private helper: it's not wrapped, so it's NOT
// registered as an RPC. Other server code can still call it.
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

export const hmacSign = query(
  async () => {
    const key = await getHmacKey();
    const sig = await crypto.subtle.sign(
      "HMAC",
      key,
      new TextEncoder().encode("message to sign for benchmark"),
    );
    return new Uint8Array(sig).length;
  },
  { id: "hmacSign" },
);

export const hmacVerify = query(
  async () => {
    const key = await getHmacKey();
    const data = new TextEncoder().encode("message to sign for benchmark");
    const sig = await crypto.subtle.sign("HMAC", key, data);
    return await crypto.subtle.verify("HMAC", key, sig, data);
  },
  { id: "hmacVerify" },
);

let _aesKey: CryptoKey | null = null;
let _aesIv: Uint8Array | null = null;
export const aesEncrypt = query(
  async () => {
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
  },
  { id: "aesEncrypt" },
);

let _ecKp: CryptoKeyPair | null = null;
export const ecdsaSign = query(
  async () => {
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
  },
  { id: "ecdsaSign" },
);

// ── Streaming ────────────────────────────────────────────────────────
//
// Async generator → SSE on the wire. The synthetic entry detects
// async-iter and encodes as AI-SDK Data Stream format.

export const drip = stream(
  async function* () {
    for (let i = 0; i < 10; i++) {
      yield { i, t: Date.now() };
    }
  },
  { id: "drip" },
);

// ── HTTP fall-through ────────────────────────────────────────────────
//
// Anything not on /_zs/v1/<id> goes through `default.fetch`. For bench
// scenarios that hit raw HTTP (e.g. /sse, /ping) we surface a
// `default.fetch` handler here so the procedure-only build still
// answers them.

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
