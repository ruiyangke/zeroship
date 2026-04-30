// Benchmark scenarios — exercises the full zeroship runtime surface.
//
// Three exports, one file:
//
//   1. Named exports (`ping`, `fib`, `sha256`, etc.) — invoked via RPC.
//      The runtime bootstrap dispatches `POST /_zs/v1/<name>` to these
//      with the request body parsed as a superjson `{ json: <input> }`
//      envelope (single-arg dispatch). This is what the vite-plugin
//      transform produces for real apps.
//
//   2. `dispatchRpc(method, [input])` — the kernel fast-path entry.
//      Receives the unwrapped input as a 1-element args array; looks
//      the function up by name and dispatches. Without this export
//      the runtime falls through to `default.fetch`, defeating the
//      bench (default.fetch's catch-all returns the request envelope,
//      not the procedure's result).
//
//   3. `export default { fetch, fetchFast }` — WinterCG-style HTTP
//      handler for everything not on `/_zs/v1/`. Handles `/sse`,
//      WebSocket upgrades, the `/ping` /`/wping` /`/wjson` HTTP-only
//      endpoints used by separate bench scenarios.
//
// Both shapes coexist in one module; the bootstrap routes based on path.
// The same file is loaded by the Node.js bench servers (node_server.js etc.)
// via a thin shim that adapts the named exports into JSON-RPC dispatch.

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

export function ping() {
    return "pong";
}

export function fib(n) {
    function go(k) { return k <= 1 ? k : go(k - 1) + go(k - 2); }
    return go(n);
}

// ---------------------------------------------------------------------------
// Async — timers, promises, external fetch
// ---------------------------------------------------------------------------

export function timeout0() {
    return new Promise((resolve) => setTimeout(() => resolve("done"), 0));
}

export function promiseChain() {
    return Promise.resolve(1)
        .then((v) => v + 10)
        .then((v) => v * 2);
}

export function promiseChainTimeout() {
    return new Promise((resolve) => setTimeout(() => resolve(1), 100))
        .then((v) => v + 10)
        .then((v) => v * 2);
}

export async function fetchExternal(url) {
    const resp = await fetch(url);
    await resp.json();
    return { status: resp.status, url: resp.url };
}

// ---------------------------------------------------------------------------
// Crypto — Web Crypto API
// ---------------------------------------------------------------------------

export function uuid() {
    return crypto.randomUUID();
}

export function randomBytes() {
    const buf = new Uint8Array(32);
    crypto.getRandomValues(buf);
    return buf.length;
}

export async function sha256() {
    const data = new TextEncoder().encode("hello world benchmark data for hashing");
    const hash = await crypto.subtle.digest("SHA-256", data);
    return new Uint8Array(hash).length;
}

// HMAC key cached across requests (realistic — apps import key once).
let _hmacKey = null;
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

export async function hmacVerify() {
    const key = await getHmacKey();
    const data = new TextEncoder().encode("message to sign for benchmark");
    const sig = await crypto.subtle.sign("HMAC", key, data);
    return await crypto.subtle.verify("HMAC", key, sig, data);
}

// AES-GCM cached key + IV.
let _aesKey = null;
let _aesIv = null;
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
        { name: "AES-GCM", iv: _aesIv },
        _aesKey,
        data,
    );
    return new Uint8Array(ct).length;
}

// ECDSA P-256 cached keypair.
let _ecKp = null;
export async function ecdsaSign() {
    if (!_ecKp) {
        _ecKp = await crypto.subtle.generateKey(
            { name: "ECDSA", namedCurve: "P-256" },
            false,
            ["sign", "verify"],
        );
    }
    const sig = await crypto.subtle.sign(
        { name: "ECDSA", hash: "SHA-256" },
        _ecKp.privateKey,
        new TextEncoder().encode("ECDSA benchmark message"),
    );
    return new Uint8Array(sig).length;
}

// ---------------------------------------------------------------------------
// dispatchRpc — kernel fast-path entry.
//
// The runtime kernel's bootstrap (init.rs::BOOTSTRAP_JS) calls
// `user.dispatchRpc(method, args)` for /_zs/v1/<method> requests
// before falling through to default.fetch. `args` is a 1-element
// array `[input]` — the unwrapped superjson `{ json: <input> }` body.
// Real apps get this auto-emitted by the vite-plugin's synthetic
// entry; the bench fixture provides it inline so the named exports
// above are reachable on the wire.

const _scenarios = {
    ping, fib,
    timeout0, promiseChain, promiseChainTimeout, fetchExternal,
    uuid, randomBytes, sha256,
    hmacSign, hmacVerify, aesEncrypt, ecdsaSign,
};

export async function dispatchRpc(method, args) {
    const fn = _scenarios[method];
    if (typeof fn !== "function") {
        throw Object.assign(new Error("Method not found: " + method), {
            status: 404, code: "NOT_FOUND",
        });
    }
    return await fn.apply(null, Array.isArray(args) ? args : [args]);
}

// ---------------------------------------------------------------------------
// HTTP handler — WinterCG `default.fetch` contract.
//
// Runs for every request the runtime bootstrap doesn't route to `/_rpc/*`:
//   - WebSocket upgrade (Upgrade: websocket)
//   - SSE streaming (/sse?chunks=N&delay=M&size=K)
//   - Fallback echo for any other path (/hello, /, etc.)
// ---------------------------------------------------------------------------

function handleWebSocket() {
    const { 0: client, 1: server } = new WebSocketPair();
    server.accept();
    server.addEventListener("message", (event) => server.send(event.data));
    return new Response(null, { status: 101, webSocket: client });
}

function handleSse(url) {
    const chunks = parseInt(url.searchParams.get("chunks") || "100", 10);
    const delayMs = parseInt(url.searchParams.get("delay") || "0", 10);
    const size = parseInt(url.searchParams.get("size") || "50", 10);

    const payload = "x".repeat(size);
    const encoder = new TextEncoder();
    const stream = new ReadableStream({
        async start(controller) {
            for (let i = 0; i < chunks; i++) {
                const frame = `data: ${JSON.stringify({
                    i,
                    t: Date.now(),
                    d: payload,
                })}\n\n`;
                controller.enqueue(encoder.encode(frame));
                if (delayMs > 0) {
                    await new Promise((r) => setTimeout(r, delayMs));
                }
            }
            controller.enqueue(encoder.encode("data: [DONE]\n\n"));
            controller.close();
        },
    });

    return new Response(stream, {
        status: 200,
        headers: {
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache",
        },
    });
}

// Cheapest possible "pong" response — same byte payload as the RPC path's
// `"pong"` (JSON string, 6 bytes incl. quotes) but constructed via the
// standard Response API. Used to measure the fetch() dispatch overhead on
// its own, with no URL/body work by the handler.
const PONG_RESPONSE_BYTES = new TextEncoder().encode('"pong"');

// Pre-rendered headers object for /ping (string table cached by V8).
const PING_HEADERS = { "Content-Type": "application/json" };

export default {
    // zeroship extension: fast HTTP dispatch. Receives raw (method, url,
    // body, env) — no Request construction, no user-side URL parse. Must
    // return one of:
    //   - a plain `{ status, headers, body }` object → HTTP response
    //   - a string/Uint8Array → 200 OK with that body
    //   - `null` → fall through to the WinterCG `fetch()` handler below
    // This path is ~5x faster than `fetch()` for simple routes because
    // it skips the Request/Response allocations and the URL parser.
    fetchFast(method, url, body, env) {
        // Cheap path check: look for "/ping" exactly at the path start.
        // We receive the full url (e.g. "http://host:port/ping") so we
        // pull the path via a single indexOf("/", 8) to skip the scheme.
        const pathStart = url.indexOf("/", 8);
        if (pathStart < 0) return null;
        // Grab up to ? or # or end.
        const qIdx = url.indexOf("?", pathStart);
        const hIdx = url.indexOf("#", pathStart);
        let pathEnd = url.length;
        if (qIdx >= 0 && qIdx < pathEnd) pathEnd = qIdx;
        if (hIdx >= 0 && hIdx < pathEnd) pathEnd = hIdx;
        const path = url.slice(pathStart, pathEnd);

        if (method === "GET" && path === "/ping") {
            return { status: 200, headers: PING_HEADERS, body: '"pong"' };
        }
        // fall through to fetch() for /sse, WebSocket, and everything else
        return null;
    },

    async fetch(request) {
        const url = new URL(request.url);

        // RPC dispatch: /_zs/v1/<id> with superjson `{ json: <input> }`
        // body. Mirrors what the vite-plugin's synthetic entry does for
        // real apps. Inlined here because the bench fixture is loaded
        // verbatim — no plugin transform.
        if (url.pathname.startsWith("/_zs/v1/")) {
            const id = decodeURIComponent(url.pathname.slice("/_zs/v1/".length));
            const fn = _scenarios[id];
            if (typeof fn !== "function") {
                return new Response(
                    JSON.stringify({ message: "Method not found: " + id, name: "Error", code: "NOT_FOUND" }),
                    { status: 404, headers: { "content-type": "application/json" } },
                );
            }
            let input;
            const text = await request.text();
            if (text) {
                try {
                    const env = JSON.parse(text);
                    input = env && typeof env === "object" && "json" in env ? env.json : env;
                } catch {
                    return new Response(
                        JSON.stringify({ message: "Invalid JSON body", name: "Error", code: "INVALID_ARGUMENT" }),
                        { status: 400, headers: { "content-type": "application/json" } },
                    );
                }
            }
            let result = fn(input);
            if (result && typeof result.then === "function") result = await result;
            return new Response(
                JSON.stringify({ json: result === undefined ? null : result }),
                { status: 200, headers: { "content-type": "application/json" } },
            );
        }

        if (request.headers.get("upgrade") === "websocket") {
            return handleWebSocket();
        }
        if (url.pathname === "/sse") {
            return handleSse(url);
        }
        // WinterCG fetch-path counterparts to fetchFast /ping.
        //   /wping — minimal Response with pre-encoded Uint8Array body
        //   /wjson — Response.json (the common AI-generated idiom)
        // Measured separately so we can see the Response construction
        // + inspection cost in isolation.
        if (url.pathname === "/wping") {
            return new Response(PONG_RESPONSE_BYTES, {
                status: 200,
                headers: { "Content-Type": "application/json" },
            });
        }
        if (url.pathname === "/wjson") {
            return Response.json({ ok: true });
        }
        // /ping via fetch() (shouldn't be reached — fetchFast handles it).
        if (url.pathname === "/ping") {
            return new Response(PONG_RESPONSE_BYTES, {
                status: 200,
                headers: { "Content-Type": "application/json" },
            });
        }
        return Response.json({ method: request.method, url: request.url });
    },
};

