// Shared benchmark scenarios — loaded by both V8 server and Node.js server.
// Each method is an exported function: function(params...) -> result | Promise<result>

export function ping() { return "pong"; }

export function fib(n) {
    function fib(n) { return n <= 1 ? n : fib(n - 1) + fib(n - 2); }
    return fib(n);
}

export function timeout0() {
    return new Promise(function(resolve) {
        setTimeout(function() { resolve("done"); }, 0);
    });
}

export function promiseChain() {
    return Promise.resolve(1)
        .then(function(v) { return v + 10; })
        .then(function(v) { return v * 2; });
}

export function promiseChainTimeout() {
    return new Promise(function(resolve) {
        setTimeout(function() { resolve(1); }, 100);
    }).then(function(v) { return v + 10; })
      .then(function(v) { return v * 2; });
}

export async function fetchExternal(url) {
    var resp = await fetch(url);
    var data = await resp.json();
    return { status: resp.status, url: resp.url };
}

// =========================================================================
// Crypto scenarios
// =========================================================================

export function uuid() {
    return crypto.randomUUID();
}

export function randomBytes() {
    var buf = new Uint8Array(32);
    crypto.getRandomValues(buf);
    return buf.length;
}

export async function sha256() {
    var data = new TextEncoder().encode("hello world benchmark data for hashing");
    var hash = await crypto.subtle.digest("SHA-256", data);
    return new Uint8Array(hash).length;
}

// HMAC key cached across requests (realistic — apps import key once)
var _hmacKey = null;
export async function hmacSign() {
    if (!_hmacKey) {
        _hmacKey = await crypto.subtle.importKey("raw",
            new TextEncoder().encode("benchmark-secret-key-32bytes!!!!"),
            { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]);
    }
    var sig = await crypto.subtle.sign("HMAC", _hmacKey,
        new TextEncoder().encode("message to sign for benchmark"));
    return new Uint8Array(sig).length;
}

export async function hmacVerify() {
    if (!_hmacKey) {
        _hmacKey = await crypto.subtle.importKey("raw",
            new TextEncoder().encode("benchmark-secret-key-32bytes!!!!"),
            { name: "HMAC", hash: "SHA-256" }, false, ["sign", "verify"]);
    }
    var data = new TextEncoder().encode("message to sign for benchmark");
    var sig = await crypto.subtle.sign("HMAC", _hmacKey, data);
    return await crypto.subtle.verify("HMAC", _hmacKey, sig, data);
}

// AES-GCM cached key
var _aesKey = null;
var _aesIv = null;
export async function aesEncrypt() {
    if (!_aesKey) {
        _aesKey = await crypto.subtle.generateKey(
            { name: "AES-GCM", length: 256 }, false, ["encrypt", "decrypt"]);
        _aesIv = new Uint8Array(12);
        crypto.getRandomValues(_aesIv);
    }
    var data = new TextEncoder().encode("secret payload for AES-GCM encryption benchmark test");
    var ct = await crypto.subtle.encrypt({ name: "AES-GCM", iv: _aesIv }, _aesKey, data);
    return new Uint8Array(ct).length;
}

// ECDSA P-256 cached keypair
var _ecKp = null;
// =========================================================================
// HTTP handler (onRequest) — also handles WebSocket upgrades
// =========================================================================

export function onRequest(req) {
    var url = new URL(req.url);

    // WebSocket upgrade
    if (req.headers.get("upgrade") === "websocket") {
        var pair = new WebSocketPair();
        var client = pair[0];
        var server = pair[1];

        server.accept();
        server.addEventListener("message", function(event) {
            server.send(event.data);
        });

        return new Response(null, { status: 101, webSocket: client });
    }

    // SSE streaming endpoint: /sse?chunks=N&delay=M
    if (url.pathname === "/sse") {
        var chunks = parseInt(url.searchParams.get("chunks") || "100");
        var delayMs = parseInt(url.searchParams.get("delay") || "0");
        var chunkSize = parseInt(url.searchParams.get("size") || "50");

        var payload = "x".repeat(chunkSize);
        var stream = new ReadableStream({
            async start(controller) {
                for (var i = 0; i < chunks; i++) {
                    controller.enqueue(
                        new TextEncoder().encode("data: " + JSON.stringify({ i: i, t: Date.now(), d: payload }) + "\n\n")
                    );
                    if (delayMs > 0) {
                        await new Promise(function(r) { setTimeout(r, delayMs); });
                    }
                }
                controller.enqueue(new TextEncoder().encode("data: [DONE]\n\n"));
                controller.close();
            }
        });

        return new Response(stream, {
            status: 200,
            headers: {
                "Content-Type": "text/event-stream",
                "Cache-Control": "no-cache",
            },
        });
    }

    return new Response(JSON.stringify({ method: req.method, url: req.url }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
    });
}

// =========================================================================
// ECDSA scenario
// =========================================================================

export async function ecdsaSign() {
    if (!_ecKp) {
        _ecKp = await crypto.subtle.generateKey(
            { name: "ECDSA", namedCurve: "P-256" }, false, ["sign", "verify"]);
    }
    var sig = await crypto.subtle.sign({ name: "ECDSA", hash: "SHA-256" },
        _ecKp.privateKey, new TextEncoder().encode("ECDSA benchmark message"));
    return new Uint8Array(sig).length;
}
