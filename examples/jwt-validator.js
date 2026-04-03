// JWT Validator — demonstrates Web Crypto for HMAC-SHA256 JWT sign/verify
// Tests: importKey, sign, verify, digest, TextEncoder/TextDecoder, btoa/atob

// Base64URL encode/decode (JWT uses base64url, not standard base64)
function base64UrlEncode(buf) {
    var bytes = new Uint8Array(buf);
    var str = "";
    for (var i = 0; i < bytes.length; i++) str += String.fromCharCode(bytes[i]);
    return btoa(str).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function base64UrlDecode(str) {
    str = str.replace(/-/g, "+").replace(/_/g, "/");
    while (str.length % 4) str += "=";
    var binary = atob(str);
    var bytes = new Uint8Array(binary.length);
    for (var i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
}

// Create a JWT token
async function createJWT(payload, secret) {
    var header = { alg: "HS256", typ: "JWT" };
    var enc = new TextEncoder();

    var headerB64 = base64UrlEncode(enc.encode(JSON.stringify(header)));
    var payloadB64 = base64UrlEncode(enc.encode(JSON.stringify(payload)));
    var signingInput = headerB64 + "." + payloadB64;

    var key = await crypto.subtle.importKey(
        "raw", enc.encode(secret),
        { name: "HMAC", hash: "SHA-256" },
        false, ["sign"]
    );

    var sig = await crypto.subtle.sign("HMAC", key, enc.encode(signingInput));
    var sigB64 = base64UrlEncode(sig);

    return signingInput + "." + sigB64;
}

// Verify and decode a JWT token
async function verifyJWT(token, secret) {
    var parts = token.split(".");
    if (parts.length !== 3) throw new Error("Invalid JWT format");

    var signingInput = parts[0] + "." + parts[1];
    var signature = base64UrlDecode(parts[2]);
    var enc = new TextEncoder();

    var key = await crypto.subtle.importKey(
        "raw", enc.encode(secret),
        { name: "HMAC", hash: "SHA-256" },
        false, ["verify"]
    );

    var valid = await crypto.subtle.verify("HMAC", key, signature, enc.encode(signingInput));
    if (!valid) throw new Error("Invalid signature");

    var payloadJson = new TextDecoder().decode(base64UrlDecode(parts[1]));
    return JSON.parse(payloadJson);
}

// RPC methods
export async function sign(payload, secret) {
    return await createJWT(JSON.parse(payload), secret);
}

export async function verify(token, secret) {
    return await verifyJWT(token, secret);
}

// Also expose a content hash utility
export async function hash(data) {
    var digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(data));
    var bytes = new Uint8Array(digest);
    var hex = "";
    for (var i = 0; i < bytes.length; i++) hex += ("0" + bytes[i].toString(16)).slice(-2);
    return hex;
}
