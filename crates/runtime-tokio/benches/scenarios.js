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
// HTTP handler (onRequest)
// =========================================================================

export function onRequest(req) {
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
