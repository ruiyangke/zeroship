// JWT Validator — HMAC-SHA256 sign/verify using WebCrypto.
//
// Two surfaces:
//   - `"use server"` exports (`sign`, `verify`, `hash`) auto-wired as RPC.
//   - `export default { fetch }` provides a REST-style HTTP surface so
//     non-browser clients (curl, webhooks) can hit the same logic.
//
// `env.JWT_SECRET` is the signing key. Set via:
//   zeroship secret set JWT_SECRET=<random> --app=<uuid>

"use server";

import { env } from "zeroship";

function b64UrlEncode(buf) {
  const bytes = new Uint8Array(buf);
  let str = "";
  for (let i = 0; i < bytes.length; i++) str += String.fromCharCode(bytes[i]);
  return btoa(str).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function b64UrlDecode(str) {
  let s = str.replace(/-/g, "+").replace(/_/g, "/");
  while (s.length % 4) s += "=";
  const binary = atob(s);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

async function hmacKey(secret, usage) {
  return crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    usage,
  );
}

function secretOrThrow() {
  const s = env.JWT_SECRET;
  if (!s) throw new Error("JWT_SECRET not configured");
  return s;
}

export async function sign(payload) {
  const secret = secretOrThrow();
  const header = { alg: "HS256", typ: "JWT" };
  const enc = new TextEncoder();
  const h = b64UrlEncode(enc.encode(JSON.stringify(header)));
  const p = b64UrlEncode(enc.encode(JSON.stringify(payload)));
  const signingInput = `${h}.${p}`;
  const key = await hmacKey(secret, ["sign"]);
  const sig = await crypto.subtle.sign("HMAC", key, enc.encode(signingInput));
  return `${signingInput}.${b64UrlEncode(sig)}`;
}

export async function verify(token) {
  const secret = secretOrThrow();
  const parts = token.split(".");
  if (parts.length !== 3) throw new Error("Invalid JWT format");
  const [h, p, s] = parts;
  const signingInput = `${h}.${p}`;
  const key = await hmacKey(secret, ["verify"]);
  const ok = await crypto.subtle.verify(
    "HMAC",
    key,
    b64UrlDecode(s),
    new TextEncoder().encode(signingInput),
  );
  if (!ok) throw new Error("Invalid signature");
  return JSON.parse(new TextDecoder().decode(b64UrlDecode(p)));
}

export async function hash(data) {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(data));
  const bytes = new Uint8Array(digest);
  let hex = "";
  for (let i = 0; i < bytes.length; i++) hex += ("0" + bytes[i].toString(16)).slice(-2);
  return hex;
}

export default {
  async fetch(request) {
    const url = new URL(request.url);
    if (request.method === "POST" && url.pathname === "/sign") {
      const payload = await request.json();
      return Response.json({ token: await sign(payload) });
    }
    if (request.method === "GET" && url.pathname === "/verify") {
      const token = url.searchParams.get("token") ?? "";
      try {
        return Response.json({ valid: true, claims: await verify(token) });
      } catch (e) {
        return Response.json({ valid: false, reason: e.message }, { status: 400 });
      }
    }
    return Response.json({
      routes: ["POST /sign", "GET /verify?token=...", "RPC: sign/verify/hash"],
    }, { status: 404 });
  },
};
