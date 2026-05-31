/**
 * `@zeroship/auth` — shared public types for the server helper (the `.`
 * export), the headless browser client (`./client`), and the React adapter
 * (`./react`). This module is pure type/`AuthError` surface with no runtime
 * dependency on `zeroship` or the DOM, so it loads in any environment.
 *
 * The shapes mirror the gateway contract exactly (BFF model — the browser
 * receives an identity projection + an HttpOnly signed session cookie, never a
 * token in the body, and there is NO client-held access/power token anywhere on
 * this surface):
 *   - `POST /__zs/auth/session`         → `{ user, expires_at }`
 *   - `GET  /__zs/auth/session[?mint=1]` → `{ user, expires_at }`
 *   - error envelope                    → `{ error, error_description? }`
 * (see `crates/gateway/src/auth_token.rs`, `crates/gateway/src/browser_auth.rs`).
 */

/**
 * Authenticated user profile.
 *
 * `id` is the per-app pairwise subject (`pws_…`, an opaque TEXT id — NOT a
 * UUID) the gateway projects so app JS reading its own identity can never
 * correlate the user across apps (gateway §6.2/G4). `email` is the per-app
 * relay alias (`…@{relay_domain}`), `null` when the `email` scope is not
 * granted.
 */
export interface User {
  id: string;
  email: string | null;
  emailVerified: boolean;
  name: string | null;
  avatar: string | null;
  /** Scopes granted to this app for this user. */
  scopes: string[];
}

/**
 * A live session — IDENTITY ONLY: the server-validated user plus the session's
 * expiry and granted scopes. Under the BFF model the gateway custodies the
 * power token server-side and hands the browser an HttpOnly, signed
 * `__Host-zs_app_session` cookie — the live request credential, sent
 * automatically on every same-origin request. There is NO client-held bearer or
 * access token anywhere on this surface: the SPA never holds a usable token, it
 * calls its own same-origin backend and the HttpOnly cookie rides along.
 */
export interface Session {
  /** Unix seconds at which the session cookie expires. */
  expires_at: number;
  user: User;
  scopes: string[];
}

/**
 * Auth state transitions delivered to `onAuthStateChange` subscribers.
 *
 * Under the BFF model there is no client-held token, so the post-sign-in
 * re-establishment of the session/identity (re-minting the HttpOnly cookie via
 * `GET /__zs/auth/session?mint=1`) is signalled as `SESSION_REFRESHED` (the old
 * token-centric `TOKEN_REFRESHED` name is gone — there is no token to refresh).
 *
 * `RECOVERING` is emitted while a `503 client_not_provisioned` probe retries
 * under backoff (NOT `SIGNED_OUT`; the breadcrumb is kept). It resolves to
 * `SIGNED_IN` or a final `AuthError` (gateway §4.3).
 */
export type AuthChangeEvent =
  | "SIGNED_IN"
  | "SIGNED_OUT"
  | "SESSION_REFRESHED"
  | "USER_UPDATED"
  | "RECOVERING";

/** Typed `AuthError.code` values. */
export type AuthErrorCode =
  | "login_required"
  | "consent_required"
  | "interaction_required"
  | "invalid_grant"
  | "missing_code_verifier"
  | "popup_closed"
  | "popup_blocked"
  | "timeout"
  | "scope_required"
  | "invalid_state"
  | "network_error"
  | "server_error"
  | "config_error"
  /**
   * `503` from `/session` or `/token` when the per-app client is not yet
   * routable. Retryable/recovering with backoff; does NOT clear the
   * breadcrumb (gateway §4.3/§1.5).
   */
  | "client_not_provisioned";

/** Constructor options for {@link AuthError}. */
export interface AuthErrorOptions {
  status?: number;
  cause?: unknown;
}

/** The single error type every client method rejects with. */
export class AuthError extends Error {
  readonly code: AuthErrorCode;
  readonly status?: number;
  // `cause` is declared on Error in ES2022 libs; we keep an explicit field so
  // the value survives across runtimes that predate it.
  override readonly cause?: unknown;

  constructor(code: AuthErrorCode, message: string, opts?: AuthErrorOptions) {
    super(message);
    this.name = "AuthError";
    this.code = code;
    this.status = opts?.status;
    this.cause = opts?.cause;
    // Preserve the prototype chain under transpilation to ES2022/older.
    Object.setPrototypeOf(this, AuthError.prototype);
  }
}

export interface AuthClientOptions {
  /** Defaults to `location.origin`. The app's same-origin gateway host. */
  appOrigin?: string;
  /** Default `["openid","profile","email"]`. */
  scope?: string[];
  /**
   * Seconds before the cached identity snapshot's `expires_at` to proactively
   * re-mint the HttpOnly session cookie. Default `60`. There is no token cache
   * under the BFF model — this only governs when `refreshSession` re-probes the
   * cookie identity early.
   */
  refreshSkewSeconds?: number;
}

export interface SignInOptions {
  provider?: "google" | "github" | "password";
  scopes?: string[];
  /** Default `true`. Popup vs full-page redirect. */
  popup?: boolean;
  /** Where to return after a redirect (popup ignores). */
  redirectTo?: string;
  /**
   * OIDC `prompt` passthrough for step-up re-authentication. `login` forces a
   * fresh credential challenge; `consent` re-shows the consent screen. Omitted
   * by default so Hydra's SSO skip fires. Forwarded verbatim to
   * `GET /__zs/auth/authorize` → Hydra (`browser_auth.rs`).
   */
  prompt?: "login" | "consent";
}

export interface SignOutOptions {
  scope?: "local" | "global";
}

/** A consent scope descriptor (declared-scope UI, Slice 3). */
export interface Scope {
  id: string;
  label: string;
  description?: string;
}
