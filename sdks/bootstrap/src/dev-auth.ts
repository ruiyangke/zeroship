/**
 * Dev-tier auth provider — the self-contained `pnpm dev` implementation of the
 * platform auth contract (BFF model), the auth peer of `env.db` → SQLite and
 * `env.kv` → redb. NO gateway, NO Hydra, NO control plane.
 *
 * ## Contract parity (dev mirrors prod exactly)
 *
 * In production the **gateway** serves the same-origin `/__zs/auth/*` endpoints
 * the `@zeroship/auth` browser client drives (`crates/gateway/src/browser_auth.rs`,
 * `crates/gateway/src/auth_token.rs`), owns the `__Host-zs_app_session` cookie,
 * and HMAC-signs the resolved identity into the request-bound `ZeroShip-User`
 * header the worker decodes into server-side `env.auth.getUser()` /
 * `currentUser()`.
 *
 * This module is the DEV-TIER implementation of that exact contract:
 *
 *   - `GET  /__zs/auth/authorize`     → frictionless dev login. Instead of a
 *       cross-site hop to Hydra, it 302-redirects straight back to the app's
 *       own `/__zs/auth/popup-callback?code=…&state=…` with a dev auth code.
 *       An optional dev user-picker (multi-user config) renders an HTML form.
 *   - `GET  /__zs/auth/popup-callback`→ the SAME same-origin relay page the
 *       gateway serves (byte-for-byte): parses code/state from the query and
 *       postMessages `zs:authorization_response` over three channels.
 *   - `POST /__zs/auth/session`       → exchange the dev code for a session;
 *       mints the local `__zs_dev_session` cookie and returns `{user, expires_at}`.
 *   - `GET  /__zs/auth/session[?mint=1]` → read / re-mint; returns `{user,
 *       expires_at}` or a `401 {error:"login_required"}` envelope.
 *   - `POST /__zs/auth/signout`       → clears the cookie; `204`.
 *
 * Every wire shape — request params, the `{user, expires_at}` body with
 * snake_case `email_verified`, the `pws_`-style id, the granted `scopes`, the
 * `{error, error_description?}` envelope — is identical to prod, so the
 * `@zeroship/auth` client and the app's `currentUser()`/`env.auth` code are
 * byte-identical dev↔prod. Only the BACKEND differs (this provider vs gateway).
 *
 * ## Server-side identity injection
 *
 * The `__zs_dev_session` cookie value is `base64url(user_json) "." hex-hmac`,
 * signed with the per-dev-server secret `ZEROSHIP_DEV_AUTH_SECRET` (the Vite
 * plugin generates it and passes it to the spawned `zeroship serve` child). The
 * runtime's dev serve path (`crates/runtime/src/core/dev_auth.rs`) reads that
 * cookie BEFORE dispatch, verifies the HMAC, and threads the decoded
 * `user_json` through the SAME `call_fetch_handler_with_user` path the worker
 * uses for the gateway header — so `env.auth.getUser()` and `currentUser()`
 * resolve the dev user server-side, identical native plumbing to prod. The
 * JS↔Rust token format is byte-compatible (see `sign`/`signDevSession`).
 *
 * ## Dev-only by construction
 *
 * This module is imported ONLY by `dev-entry.ts` (`@zeroship/bootstrap/dev`),
 * which the Vite plugin's dev-bootstrap consumes. The production
 * `runtime-entry.ts` never imports it, and `vite build`'s `.zship` bundles the
 * user module + the prod runtime-entry — never `@zeroship/bootstrap/dev`. So
 * the dev-auth provider is structurally absent from any shipped worker module
 * (grep-provable). There is no runtime flag in shipped code; the dev tier lives
 * exclusively in the dev path.
 */

/** Public user projection (camelCase) — mirrors `@zeroship/auth` `User`. */
export interface DevUser {
  id: string;
  email: string | null;
  emailVerified: boolean;
  name: string | null;
  avatar: string | null;
  scopes: string[];
}

/**
 * The canonical `ZeroShip-User` wire body the gateway emits and the runtime's
 * `env.auth.getUser()` JSON-parses verbatim: snake_case `email_verified`, the
 * granted `scopes`. The dev cookie payload is exactly this JSON, so the runtime
 * sees the identical shape it sees from a gateway-signed header in prod.
 */
interface WireUser {
  id: string;
  email: string | null;
  name: string | null;
  avatar?: string | null;
  email_verified: boolean;
  scopes: string[];
}

/** One configured dev user. `id` defaults to a stable `pws_dev…` if omitted. */
export interface DevUserConfig {
  id?: string;
  email?: string | null;
  name?: string | null;
  avatar?: string | null;
  scopes?: string[];
}

/** Parsed `ZEROSHIP_DEV_AUTH` config. */
export interface DevAuthConfig {
  users: WireUser[];
  /** The user id selected by `/authorize` when none is chosen. */
  defaultUserId: string;
}

/** Cookie name — mirrors `DEV_SESSION_COOKIE` in `dev_auth.rs`. */
const DEV_SESSION_COOKIE = "__zs_dev_session";
/** Dev session lifetime (seconds). One day is plenty for a dev loop. */
const DEV_SESSION_TTL_SECS = 24 * 60 * 60;
/** RFC 4648 §5 base64url alphabet (no padding) — matches the Rust URL_SAFE_NO_PAD. */
const B64URL = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

const DEFAULT_DEV_USER: WireUser = {
  // A `pws_`-style opaque per-app pairwise subject, matching the prod shape
  // (gateway §6.2 — app code only ever sees the `pws_…`, never the `usr_…`).
  id: "pws_dev00000000000000000",
  email: "dev@localhost",
  name: "Dev User",
  avatar: null,
  email_verified: true,
  scopes: ["openid", "profile", "email"],
};

// ── base64url + hex helpers (self-contained; no btoa/Buffer) ────────────────

function base64UrlEncode(bytes: Uint8Array): string {
  let out = "";
  for (let i = 0; i < bytes.length; i += 3) {
    const b0 = bytes[i];
    const b1 = i + 1 < bytes.length ? bytes[i + 1] : 0;
    const b2 = i + 2 < bytes.length ? bytes[i + 2] : 0;
    out += B64URL[b0 >> 2];
    out += B64URL[((b0 & 0x03) << 4) | (b1 >> 4)];
    if (i + 1 < bytes.length) out += B64URL[((b1 & 0x0f) << 2) | (b2 >> 6)];
    if (i + 2 < bytes.length) out += B64URL[b2 & 0x3f];
  }
  return out;
}

function base64UrlDecode(s: string): Uint8Array {
  const lookup = new Int16Array(128).fill(-1);
  for (let i = 0; i < B64URL.length; i++) lookup[B64URL.charCodeAt(i)] = i;
  const out: number[] = [];
  let buffer = 0;
  let bits = 0;
  for (const ch of s) {
    const v = lookup[ch.charCodeAt(0)];
    if (v < 0) continue;
    buffer = (buffer << 6) | v;
    bits += 6;
    if (bits >= 8) {
      bits -= 8;
      out.push((buffer >> bits) & 0xff);
    }
  }
  return new Uint8Array(out);
}

function toHex(bytes: Uint8Array): string {
  let out = "";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

// ── dev session token: base64url(user_json) "." hex-hmac ────────────────────

async function importHmacKey(secret: string): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign", "verify"],
  );
}

/**
 * Sign `userJson` into a `__zs_dev_session` token byte-compatible with the
 * runtime's `dev_auth::sign_dev_session` (Rust): the HMAC covers the
 * base64url-encoded payload's BYTES (the ASCII of the base64url string), and
 * the tag is lowercase hex.
 */
export async function signDevSession(secret: string, userJson: string): Promise<string> {
  const payloadB64 = base64UrlEncode(new TextEncoder().encode(userJson));
  const key = await importHmacKey(secret);
  const sig = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(payloadB64));
  return `${payloadB64}.${toHex(new Uint8Array(sig))}`;
}

/** Verify + decode a dev session token. Returns the user JSON, or null. */
export async function verifyDevSession(secret: string, token: string): Promise<string | null> {
  const dot = token.indexOf(".");
  if (dot <= 0 || dot === token.length - 1) return null;
  const payloadB64 = token.slice(0, dot);
  const mac = token.slice(dot + 1);
  const key = await importHmacKey(secret);
  let macBytes: Uint8Array;
  try {
    macBytes = hexToBytes(mac);
  } catch {
    return null;
  }
  const ok = await crypto.subtle.verify(
    "HMAC",
    key,
    macBytes as unknown as ArrayBuffer,
    new TextEncoder().encode(payloadB64),
  );
  if (!ok) return null;
  try {
    return new TextDecoder().decode(base64UrlDecode(payloadB64));
  } catch {
    return null;
  }
}

function hexToBytes(hex: string): Uint8Array {
  if (hex.length % 2 !== 0) throw new Error("odd-length hex");
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) {
    const byte = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
    if (Number.isNaN(byte)) throw new Error("invalid hex");
    out[i] = byte;
  }
  return out;
}

// ── config ──────────────────────────────────────────────────────────────────

/** Coerce a configured dev user into the canonical wire shape. */
function normalizeUser(u: DevUserConfig, index: number): WireUser {
  const id =
    u.id ??
    (index === 0 ? DEFAULT_DEV_USER.id : `pws_dev${String(index).padStart(17, "0")}`);
  return {
    id,
    email: u.email ?? DEFAULT_DEV_USER.email,
    name: u.name ?? DEFAULT_DEV_USER.name,
    avatar: u.avatar ?? null,
    email_verified: true,
    scopes: u.scopes ?? [...DEFAULT_DEV_USER.scopes],
  };
}

/**
 * Parse the `ZEROSHIP_DEV_AUTH` env JSON the Vite plugin serializes. Accepts:
 *   - `{"users":[{...}],"defaultUserId"?:"..."}` — explicit list (picker when >1)
 *   - `{"user":{...}}`                            — single user
 *   - `"1"` / `"true"` / absent                   — the built-in default user
 * Returns `null` when dev-auth is explicitly disabled (`"0"`/`"false"`).
 */
export function parseDevAuthConfig(raw: string | undefined): DevAuthConfig | null {
  if (raw === "0" || raw === "false") return null;
  if (!raw || raw === "1" || raw === "true") {
    return { users: [DEFAULT_DEV_USER], defaultUserId: DEFAULT_DEV_USER.id };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return { users: [DEFAULT_DEV_USER], defaultUserId: DEFAULT_DEV_USER.id };
  }
  if (parsed && typeof parsed === "object") {
    const obj = parsed as { user?: DevUserConfig; users?: DevUserConfig[]; defaultUserId?: string };
    const list = obj.users ?? (obj.user ? [obj.user] : null);
    if (list && list.length > 0) {
      const users = list.map(normalizeUser);
      const defaultUserId =
        obj.defaultUserId && users.some((u) => u.id === obj.defaultUserId)
          ? obj.defaultUserId
          : users[0].id;
      return { users, defaultUserId };
    }
  }
  return { users: [DEFAULT_DEV_USER], defaultUserId: DEFAULT_DEV_USER.id };
}

function wireToPublic(w: WireUser): DevUser {
  return {
    id: w.id,
    email: w.email,
    emailVerified: w.email_verified,
    name: w.name,
    avatar: w.avatar ?? null,
    scopes: w.scopes,
  };
}

// ── dev auth code ledger (in-memory, per dev-server process) ─────────────────
//
// The dev `/authorize` mints an opaque code bound to the chosen user id; the
// `/session` exchange spends it. A real IdP issues an auth code + does PKCE; the
// dev tier keeps the SAME wire (the SDK still sends code + verifier) but the
// "exchange" is a local lookup. PKCE verifier is accepted-and-ignored — there
// is no upstream token endpoint to bind it to in dev.

interface DevCode {
  userId: string;
  expiresAt: number;
}
const devCodes = new Map<string, DevCode>();
const DEV_CODE_TTL_MS = 5 * 60 * 1000;

function mintCode(userId: string): string {
  const bytes = new Uint8Array(18);
  crypto.getRandomValues(bytes);
  const code = `devc_${base64UrlEncode(bytes)}`;
  devCodes.set(code, { userId, expiresAt: Date.now() + DEV_CODE_TTL_MS });
  return code;
}

function spendCode(code: string): string | null {
  const entry = devCodes.get(code);
  if (!entry) return null;
  devCodes.delete(code);
  if (entry.expiresAt < Date.now()) return null;
  return entry.userId;
}

// ── HTTP helpers ─────────────────────────────────────────────────────────────

function json(status: number, body: unknown, extraHeaders?: Record<string, string>): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", "cache-control": "no-store", ...extraHeaders },
  });
}

function errorEnvelope(status: number, error: string, description?: string): Response {
  const body: { error: string; error_description?: string } = { error };
  if (description) body.error_description = description;
  return json(status, body);
}

function sessionBody(user: WireUser): { user: WireUser; expires_at: number } {
  return { user, expires_at: Math.floor(Date.now() / 1000) + DEV_SESSION_TTL_SECS };
}

function setCookieHeader(token: string): string {
  // Dev runs on http://localhost — `Secure` would drop the cookie. `Lax`
  // SameSite + `Path=/` + `HttpOnly` mirrors the prod cookie posture as
  // closely as a localhost http origin allows. The prod `__Host-` prefix
  // (which mandates Secure) is intentionally NOT used for the dev cookie.
  return `${DEV_SESSION_COOKIE}=${token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=${DEV_SESSION_TTL_SECS}`;
}

function clearCookieHeader(): string {
  return `${DEV_SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0`;
}

function readCookie(request: Request, name: string): string | null {
  const header = request.headers.get("cookie");
  if (!header) return null;
  for (const pair of header.split(";")) {
    const trimmed = pair.trim();
    const eq = trimmed.indexOf("=");
    if (eq < 0) continue;
    if (trimmed.slice(0, eq).trim() === name) return trimmed.slice(eq + 1).trim();
  }
  return null;
}

/** The same-origin relay page the gateway serves — byte-identical behaviour. */
function popupCallbackHtml(): string {
  return (
    "<!doctype html><meta charset=utf-8><title>Sign-in</title>\n" +
    "<script>\n" +
    "(function(){\n" +
    "  var p = new URLSearchParams(location.search);\n" +
    "  var state = p.get('state');\n" +
    "  var msg = { type: 'zs:authorization_response', response:\n" +
    "    p.get('error')\n" +
    "      ? { error: p.get('error'), error_description: p.get('error_description'), state: state }\n" +
    "      : { code: p.get('code'), state: state } };\n" +
    "  try { if (window.opener) window.opener.postMessage(msg, location.origin); } catch (e) {}\n" +
    "  try { new BroadcastChannel('zs:auth').postMessage(msg); } catch (e) {}\n" +
    "  try {\n" +
    "    if (state) {\n" +
    "      localStorage.setItem('@@zsauth@@::relay::' + state, JSON.stringify(msg));\n" +
    "      localStorage.removeItem('@@zsauth@@::relay::' + state);\n" +
    "    }\n" +
    "  } catch (e) {}\n" +
    "  try { window.close(); } catch (e) {}\n" +
    "})();\n" +
    "</script>"
  );
}

/** A tiny dev user-picker form (multi-user config only). */
function userPickerHtml(users: WireUser[], query: URLSearchParams): string {
  const redirectUri = query.get("redirect_uri") ?? "/__zs/auth/popup-callback";
  const state = query.get("state") ?? "";
  const rows = users
    .map(
      (u) =>
        `<form method="GET" action="/__zs/auth/authorize">` +
        `<input type="hidden" name="dev_user" value="${escapeHtml(u.id)}">` +
        `<input type="hidden" name="state" value="${escapeHtml(state)}">` +
        `<input type="hidden" name="redirect_uri" value="${escapeHtml(redirectUri)}">` +
        `<button type="submit">${escapeHtml(u.name ?? u.id)} &lt;${escapeHtml(u.email ?? "")}&gt;</button>` +
        `</form>`,
    )
    .join("\n");
  return `<!doctype html><meta charset=utf-8><title>Dev sign-in</title>` +
    `<h1>Choose a dev user</h1>${rows}`;
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) =>
    c === "&" ? "&amp;" : c === "<" ? "&lt;" : c === ">" ? "&gt;" : c === '"' ? "&quot;" : "&#39;",
  );
}

// ── the dev-auth fetch handler ───────────────────────────────────────────────

export interface DevAuthProvider {
  /** True if `pathname` is a `/__zs/auth/*` route this provider owns. */
  handles(pathname: string): boolean;
  /** Serve a `/__zs/auth/*` request. Caller guards with `handles()` first. */
  handle(request: Request): Promise<Response>;
}

/**
 * Build the dev-auth provider from the spawn env. `getEnv` reads
 * `ZEROSHIP_DEV_AUTH` (user config) and `ZEROSHIP_DEV_AUTH_SECRET` (cookie
 * HMAC). Returns `null` when dev-auth is disabled or no secret is present
 * (e.g. someone ran `zeroship serve` by hand without the Vite plugin).
 */
export function createDevAuthProvider(
  getEnv: (name: string) => string | undefined,
): DevAuthProvider | null {
  const secret = getEnv("ZEROSHIP_DEV_AUTH_SECRET");
  if (!secret) return null;
  const config = parseDevAuthConfig(getEnv("ZEROSHIP_DEV_AUTH"));
  if (!config) return null;

  const userById = new Map(config.users.map((u) => [u.id, u] as const));

  async function handle(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname;

    if (path === "/__zs/auth/authorize") return authorize(url);
    if (path === "/__zs/auth/popup-callback") {
      return new Response(popupCallbackHtml(), {
        status: 200,
        headers: {
          "content-type": "text/html; charset=utf-8",
          "referrer-policy": "no-referrer",
          "cross-origin-opener-policy": "same-origin",
          "cache-control": "no-store",
        },
      });
    }
    if (path === "/__zs/auth/session") {
      return request.method === "POST" ? exchange(request) : sessionProbe(request, url);
    }
    if (path === "/__zs/auth/signout") return signout();

    return errorEnvelope(404, "not_found", `no dev-auth route for ${path}`);
  }

  function authorize(url: URL): Response {
    // Frictionless: pick the chosen dev user (or default) and 302 straight back
    // to the app's popup-callback with a dev code + the original state. No IdP.
    const q = url.searchParams;
    const state = q.get("state") ?? "";
    const redirectUri = q.get("redirect_uri") ?? `${url.origin}/__zs/auth/popup-callback`;

    // Multi-user config without an explicit pick → render the picker so a
    // developer can switch identities / scope sets.
    const chosen = q.get("dev_user");
    if (!chosen && config!.users.length > 1) {
      return new Response(userPickerHtml(config!.users, q), {
        status: 200,
        headers: { "content-type": "text/html; charset=utf-8", "cache-control": "no-store" },
      });
    }

    const userId = chosen && userById.has(chosen) ? chosen : config!.defaultUserId;
    const code = mintCode(userId);
    const target = new URL(redirectUri);
    target.searchParams.set("code", code);
    if (state) target.searchParams.set("state", state);
    return new Response(null, { status: 302, headers: { location: target.toString(), "cache-control": "no-store" } });
  }

  async function exchange(request: Request): Promise<Response> {
    let body: { code?: string; grant_type?: string } = {};
    try {
      body = (await request.json()) as typeof body;
    } catch {
      return errorEnvelope(400, "invalid_grant", "malformed exchange body");
    }
    const code = body.code;
    if (!code) return errorEnvelope(400, "invalid_grant", "missing code");
    const userId = spendCode(code);
    if (!userId) return errorEnvelope(400, "invalid_grant", "unknown or expired dev code");
    const user = userById.get(userId) ?? config!.users[0];
    const token = await signDevSession(secret!, JSON.stringify(user));
    return json(200, sessionBody(user), { "set-cookie": setCookieHeader(token) });
  }

  async function sessionProbe(request: Request, url: URL): Promise<Response> {
    const token = readCookie(request, DEV_SESSION_COOKIE);
    if (!token) return errorEnvelope(401, "login_required", "no dev session");
    const userJson = await verifyDevSession(secret!, token);
    if (!userJson) return errorEnvelope(401, "login_required", "invalid dev session");
    let user: WireUser;
    try {
      user = JSON.parse(userJson) as WireUser;
    } catch {
      return errorEnvelope(401, "login_required", "corrupt dev session");
    }
    // `?mint=1` re-issues a fresh cookie (silent renewal / reload recovery).
    const mint = url.searchParams.get("mint") === "1";
    const headers = mint
      ? { "set-cookie": setCookieHeader(await signDevSession(secret!, userJson)) }
      : undefined;
    return json(200, sessionBody(user), headers);
  }

  function signout(): Response {
    // Idempotent: always 204 + clear the cookie, mirroring the gateway.
    return new Response(null, {
      status: 204,
      headers: { "set-cookie": clearCookieHeader(), "cache-control": "no-store" },
    });
  }

  return {
    handles: (pathname: string) => pathname.startsWith("/__zs/auth/"),
    handle,
  };
}

/** Expose the public-user projection for tests / diagnostics. */
export { wireToPublic };
