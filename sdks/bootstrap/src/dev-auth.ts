/**
 * Dev-tier auth provider — the self-contained `pnpm dev` implementation of the
 * platform auth contract (BFF model), the auth peer of `env.db` → SQLite and
 * `env.kv` → redb. NO gateway, NO Hydra, NO control plane.
 *
 * ## Contract parity (dev mirrors prod exactly)
 *
 * In production the **gateway** serves the same-origin `/__zeroship/auth/*` endpoints
 * the `@zeroship/auth` browser client drives (`crates/gateway/src/browser_auth.rs`,
 * `crates/gateway/src/auth_token.rs`), owns the `__Host-zeroship_app_session` cookie,
 * and HMAC-signs the resolved identity into the request-bound `ZeroShip-User`
 * header the worker decodes into server-side `env.auth.getUser()` /
 * `currentUser()`.
 *
 * This module is the DEV-TIER implementation of that exact contract:
 *
 *   - `GET  /__zeroship/auth/authorize`     → frictionless dev login. Instead of a
 *       cross-site hop to Hydra, it 302-redirects straight back to the app's
 *       own `/__zeroship/auth/popup-callback?code=…&state=…` with a dev auth code.
 *       An optional dev user-picker (multi-user config) renders an HTML form.
 *   - `GET  /__zeroship/auth/popup-callback`→ the SAME same-origin relay page the
 *       gateway serves (byte-for-byte): parses code/state from the query and
 *       postMessages `zs:authorization_response` over three channels.
 *   - `POST /__zeroship/auth/session`       → exchange the dev code for a session;
 *       mints the local `__zeroship_dev_session` cookie and returns `{user, expires_at}`.
 *   - `GET  /__zeroship/auth/session[?mint=1]` → read / re-mint; returns `{user,
 *       expires_at}` or a `401 {error:"login_required"}` envelope.
 *   - `POST /__zeroship/auth/signout`       → clears the cookie; `204`.
 *
 * Every wire shape — request params, the `{user, expires_at}` body with
 * snake_case `email_verified`, the `pws_`-style id, the granted `scopes`, the
 * `{error, error_description?}` envelope — is identical to prod, so the
 * `@zeroship/auth` client and the app's `currentUser()`/`env.auth` code are
 * byte-identical dev↔prod. Only the BACKEND differs (this provider vs gateway).
 *
 * ## Server-side identity injection
 *
 * The `__zeroship_dev_session` cookie value is `base64url(user_json) "." hex-hmac`,
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
  /**
   * The password this dev user signs in with. The dev login form prefills it
   * (one-click sign-in) but still *validates* it on submit, so the
   * `invalid_credentials` failure path is exercisable in dev exactly as in
   * prod. Defaults to {@link DEFAULT_DEV_PASSWORD} when omitted.
   */
  password?: string;
}

/** Parsed `ZEROSHIP_DEV_AUTH` config. */
export interface DevAuthConfig {
  users: WireUser[];
  /** The user id pre-selected by the `/authorize` login form. */
  defaultUserId: string;
  /** `id → password` for credential validation on the login-form POST. */
  passwords: Record<string, string>;
}

/** Cookie name — mirrors `DEV_SESSION_COOKIE` in `dev_auth.rs`. */
const DEV_SESSION_COOKIE = "__zeroship_dev_session";
/**
 * Dev login CSRF cookie — the double-submit peer of the hidden `csrf` form
 * field, mirroring prod's `__Host-zsidp_csrf` (`crates/auth/src/csrf.rs`). The
 * GET form sets it and embeds the same token; the POST compares them.
 */
const DEV_CSRF_COOKIE = "__zeroship_dev_csrf";
/**
 * The well-known password every dev user signs in with unless the `devAuth`
 * config overrides it per user. It is *not* a secret — the dev login form
 * prefills it in plain sight; it exists only so the credential-validation path
 * (and its `invalid_credentials` failure arm) is real in dev.
 */
export const DEFAULT_DEV_PASSWORD = "dev";
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
 * Sign `userJson` into a `__zeroship_dev_session` token byte-compatible with the
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

/** Coerce a configured dev user into the canonical wire shape + its password. */
function normalizeUser(u: DevUserConfig, index: number): { wire: WireUser; password: string } {
  const id =
    u.id ??
    (index === 0 ? DEFAULT_DEV_USER.id : `pws_dev${String(index).padStart(17, "0")}`);
  return {
    wire: {
      id,
      email: u.email ?? DEFAULT_DEV_USER.email,
      name: u.name ?? DEFAULT_DEV_USER.name,
      avatar: u.avatar ?? null,
      email_verified: true,
      scopes: u.scopes ?? [...DEFAULT_DEV_USER.scopes],
    },
    password: u.password ?? DEFAULT_DEV_PASSWORD,
  };
}

/** The built-in default user as a `{ wire, password }` pair. */
function defaultConfig(): DevAuthConfig {
  return {
    users: [DEFAULT_DEV_USER],
    defaultUserId: DEFAULT_DEV_USER.id,
    passwords: { [DEFAULT_DEV_USER.id]: DEFAULT_DEV_PASSWORD },
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
  if (!raw || raw === "1" || raw === "true") return defaultConfig();
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return defaultConfig();
  }
  if (parsed && typeof parsed === "object") {
    const obj = parsed as { user?: DevUserConfig; users?: DevUserConfig[]; defaultUserId?: string };
    const list = obj.users ?? (obj.user ? [obj.user] : null);
    if (list && list.length > 0) {
      const normalized = list.map(normalizeUser);
      const users = normalized.map((n) => n.wire);
      const passwords: Record<string, string> = {};
      for (const n of normalized) passwords[n.wire.id] = n.password;
      const defaultUserId =
        obj.defaultUserId && users.some((u) => u.id === obj.defaultUserId)
          ? obj.defaultUserId
          : users[0].id;
      return { users, defaultUserId, passwords };
    }
  }
  return defaultConfig();
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

/**
 * Resolve a SAME-ORIGIN popup-callback target. A cross-origin (or unparseable)
 * `redirect_uri` falls back to the canonical same-origin callback — the dev
 * peer of the gateway's exact-`redirect_uri`-match guard (open-redirect /
 * code-exfiltration defense), so dev mirrors prod's rigor.
 */
function safeRedirectUri(candidate: string | null, origin: string): string {
  const fallback = `${origin}/__zeroship/auth/popup-callback`;
  if (!candidate) return fallback;
  try {
    const u = new URL(candidate, origin);
    return u.origin === origin ? u.toString() : fallback;
  } catch {
    return fallback;
  }
}

/** Mint a random CSRF token (URL-safe). */
function mintCsrfToken(): string {
  const bytes = new Uint8Array(18);
  crypto.getRandomValues(bytes);
  return base64UrlEncode(bytes);
}

/** Short-lived double-submit CSRF cookie for the dev login form. */
function setCsrfCookieHeader(token: string): string {
  return `${DEV_CSRF_COOKIE}=${token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600`;
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
    "  // Primary: postMessage to the launcher window — opener (popup leg) if\n" +
    "  // present-and-distinct, else parent (iframe leg). targetOrigin pinned to\n" +
    "  // location.origin (the app origin), NEVER '*'.\n" +
    "  var tgt = (window.opener && window.opener !== window) ? window.opener\n" +
    "          : (window.parent  && window.parent  !== window) ? window.parent : null;\n" +
    "  try { if (tgt) tgt.postMessage(msg, location.origin); } catch (e) {}\n" +
    "  // Fallback (opener/parent severed by COOP, or full-page redirect): both\n" +
    "  // channels are SAME-ORIGIN, so no cross-origin exposure.\n" +
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

/**
 * The dev login screen — the same-origin in-iframe form the developer signs in
 * through. It mirrors prod's `login.html` (heading, error banner, email +
 * password + CSRF, submit) and is **prefilled** with the selected dev user's
 * credentials so sign-in is one click — but it is NOT auto-submitted, so the
 * developer exercises the real form → POST → callback flow (and can clear the
 * fields to test the `invalid_credentials` path) exactly as a prod user does.
 *
 * Multi-user configs render an email `<select>`; a small inline script
 * re-prefills the password when the selection changes (the dev passwords are
 * well-known, not secret).
 */
function loginFormHtml(args: {
  users: WireUser[];
  passwords: Record<string, string>;
  defaultUserId: string;
  state: string;
  redirectUri: string;
  csrf: string;
  error?: string;
}): string {
  const { users, passwords, defaultUserId, state, redirectUri, csrf, error } = args;
  const def = users.find((u) => u.id === defaultUserId) ?? users[0];
  const defEmail = def.email ?? "";
  const defPassword = passwords[def.id] ?? DEFAULT_DEV_PASSWORD;

  const errorBanner = error ? `<div class="error" role="alert">${escapeHtml(error)}</div>` : "";

  let emailControl: string;
  let pickerScript = "";
  if (users.length > 1) {
    const options = users
      .map((u) => {
        const email = u.email ?? "";
        const label = u.name ? `${u.name} <${email}>` : email || u.id;
        const sel = u.id === def.id ? " selected" : "";
        return `<option value="${escapeHtml(email)}"${sel}>${escapeHtml(label)}</option>`;
      })
      .join("");
    emailControl =
      `<label>Dev user<select name="email" id="zs-email" autocomplete="username">${options}</select></label>`;
    // email → password map (dev passwords are well-known; prefilled in plain
    // sight already). Repopulate the password field on selection change.
    const emailToPassword: Record<string, string> = {};
    for (const u of users) emailToPassword[u.email ?? ""] = passwords[u.id] ?? DEFAULT_DEV_PASSWORD;
    pickerScript =
      `<script>(function(){var M=${embedJson(emailToPassword)};` +
      `var e=document.getElementById('zs-email'),p=document.getElementById('zs-password');` +
      `e.addEventListener('change',function(){p.value=M[e.value]||'';});})();</script>`;
  } else {
    emailControl =
      `<label>Email<input type="email" name="email" id="zs-email" autocomplete="username" value="${escapeHtml(defEmail)}" required></label>`;
  }

  return (
    "<!doctype html><meta charset=utf-8><title>Sign in · zeroship (dev)</title>" +
    "<style>body{font:15px system-ui,sans-serif;max-width:22rem;margin:3rem auto;padding:0 1rem}" +
    "label{display:block;margin:.6rem 0}input,select{display:block;width:100%;padding:.4rem;margin-top:.2rem}" +
    "button{margin-top:1rem;padding:.5rem 1rem}.error{background:#fde;border:1px solid #c66;color:#900;padding:.5rem;border-radius:4px}" +
    ".dev-note{color:#888;font-size:.8rem;margin-top:1.2rem}</style>" +
    `<h2>Sign in (dev)</h2>${errorBanner}` +
    `<form method="POST" action="/__zeroship/auth/authorize">` +
    `<input type="hidden" name="csrf" value="${escapeHtml(csrf)}">` +
    `<input type="hidden" name="state" value="${escapeHtml(state)}">` +
    `<input type="hidden" name="redirect_uri" value="${escapeHtml(redirectUri)}">` +
    emailControl +
    `<label>Password<input type="password" name="password" id="zs-password" autocomplete="current-password" value="${escapeHtml(defPassword)}" required></label>` +
    `<button type="submit">Sign in</button>` +
    `</form>` +
    `<p class="dev-note">Dev sign-in — credentials are prefilled and validated locally (no gateway, no Hydra). Edit them to exercise the failure path.</p>` +
    pickerScript
  );
}

/** Embed an object as a `<script>`-safe JSON literal (escapes `<`). */
function embedJson(value: unknown): string {
  return JSON.stringify(value).replace(/</g, "\\u003c");
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) =>
    c === "&" ? "&amp;" : c === "<" ? "&lt;" : c === ">" ? "&gt;" : c === '"' ? "&quot;" : "&#39;",
  );
}

// ── the dev-auth fetch handler ───────────────────────────────────────────────

export interface DevAuthProvider {
  /** True if `pathname` is a `/__zeroship/auth/*` route this provider owns. */
  handles(pathname: string): boolean;
  /** Serve a `/__zeroship/auth/*` request. Caller guards with `handles()` first. */
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

    if (path === "/__zeroship/auth/authorize") {
      return request.method === "POST" ? submitLogin(request, url) : renderLoginForm(url);
    }
    if (path === "/__zeroship/auth/popup-callback") {
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
    if (path === "/__zeroship/auth/session") {
      return request.method === "POST" ? exchange(request) : sessionProbe(request, url);
    }
    if (path === "/__zeroship/auth/signout") return signout();

    return errorEnvelope(404, "not_found", `no dev-auth route for ${path}`);
  }

  /**
   * `GET /authorize` — render the dev login form (NOT a frictionless 302).
   * Prefilled with the default user's credentials + a fresh CSRF token; the
   * developer clicks "Sign in" to POST it. Same UX shape as prod's framed
   * `auth.zeroship.ai/login`, served same-origin in-iframe.
   */
  function renderLoginForm(url: URL, error?: string, status = 200): Response {
    const q = url.searchParams;
    const state = q.get("state") ?? "";
    const redirectUri = safeRedirectUri(q.get("redirect_uri"), url.origin);
    const csrf = mintCsrfToken();
    const html = loginFormHtml({
      users: config!.users,
      passwords: config!.passwords,
      defaultUserId: config!.defaultUserId,
      state,
      redirectUri,
      csrf,
      error,
    });
    return new Response(html, {
      status,
      headers: {
        "content-type": "text/html; charset=utf-8",
        "cache-control": "no-store",
        "set-cookie": setCsrfCookieHeader(csrf),
      },
    });
  }

  /**
   * `POST /authorize` — validate the submitted dev credentials (CSRF + email +
   * password). On success mint a dev code and 302 to the popup-callback (the
   * old frictionless happy path). On failure re-render the form with the same
   * `invalid email or password` banner + 401 that prod surfaces — so the
   * failure path is real in dev.
   */
  async function submitLogin(request: Request, url: URL): Promise<Response> {
    let form: URLSearchParams;
    try {
      form = new URLSearchParams(await request.text());
    } catch {
      return renderLoginForm(url, "invalid request", 400);
    }
    const state = form.get("state") ?? "";
    const redirectUri = safeRedirectUri(form.get("redirect_uri"), url.origin);
    // Preserve the submitted state/redirect_uri on any re-render.
    const reUrl = new URL(url.toString());
    reUrl.searchParams.set("state", state);
    reUrl.searchParams.set("redirect_uri", redirectUri);

    // 1. CSRF: form token must equal the double-submit cookie.
    const cookieCsrf = readCookie(request, DEV_CSRF_COOKIE);
    const formCsrf = form.get("csrf");
    if (!cookieCsrf || !formCsrf || cookieCsrf !== formCsrf) {
      return renderLoginForm(reUrl, "invalid request", 400);
    }

    // 2. Credentials: email must match a configured dev user and the password
    //    must match that user's configured/default dev password.
    const email = form.get("email") ?? "";
    const password = form.get("password") ?? "";
    const user = config!.users.find((u) => (u.email ?? "") === email);
    if (!user || config!.passwords[user.id] !== password) {
      return renderLoginForm(reUrl, "invalid email or password", 401);
    }

    // 3. Success: mint a dev code bound to the user + 302 to the callback.
    const code = mintCode(user.id);
    const target = new URL(redirectUri);
    target.searchParams.set("code", code);
    if (state) target.searchParams.set("state", state);
    return new Response(null, {
      status: 302,
      headers: { location: target.toString(), "cache-control": "no-store" },
    });
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
    handles: (pathname: string) => pathname.startsWith("/__zeroship/auth/"),
    handle,
  };
}

/** Expose the public-user projection for tests / diagnostics. */
export { wireToPublic };
