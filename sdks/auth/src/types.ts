/**
 * `@zeroship/auth` — shared public types for the server helper (the `.`
 * export), the headless browser client (`./client`), and the React adapter
 * (`./react`). This module is pure type/`AuthError` surface with no runtime
 * dependency on `zeroship` or the DOM, so it loads in any environment.
 *
 * The shapes mirror the gateway contract exactly:
 *   - `POST /__zs/auth/token`    → `{ access_token, token_type, expires_in, scope, user }`
 *   - `GET  /__zs/auth/session`  → `{ user }` or (with `?mint=1`)
 *                                   `{ user, access_token, token_type, expires_in, expires_at }`
 *   - error envelope             → `{ error, error_description? }`
 * (see `crates/gateway/src/auth_token.rs`, `crates/gateway/src/browser_auth.rs`).
 */

/**
 * Authenticated user profile.
 *
 * `id` is the per-app pairwise subject (`pws_…`, an opaque TEXT id — NOT a
 * UUID) the gateway projects so app JS decoding its own access token can
 * never correlate the user across apps (gateway §6.2/G4). `email` is the
 * per-app relay alias (`…@{relay_domain}`), `null` when the `email` scope is
 * not granted.
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
 * A live session: the gateway per-app WRAPPER access token plus the
 * server-validated user. The wrapper `sub` is the `pws_`, never the global
 * UUID, so decoding `access_token` cannot correlate the user across apps.
 */
export interface Session {
  /** Gateway wrapper access token. Sent as `Authorization: Bearer <token>`. */
  access_token: string;
  /**
   * Present only with `useRefreshTokens` and when NOT held in the Web Worker.
   * In the default `server_anchor` mode the browser holds no refresh token —
   * the server-held anchor family is the source of truth.
   */
  refresh_token?: string;
  /** Unix seconds at which `access_token` expires. */
  expires_at: number;
  token_type: "Bearer";
  user: User;
  scopes: string[];
}

/**
 * Auth state transitions delivered to `onAuthStateChange` subscribers.
 *
 * `RECOVERING` is emitted while a `503 client_not_provisioned` probe retries
 * under backoff (NOT `SIGNED_OUT`; the breadcrumb is kept). It resolves to
 * `SIGNED_IN` or a final `AuthError` (gateway §4.3).
 */
export type AuthChangeEvent =
  | "SIGNED_IN"
  | "SIGNED_OUT"
  | "TOKEN_REFRESHED"
  | "USER_UPDATED"
  | "RECOVERING";

/** Typed `AuthError.code` values. */
export type AuthErrorCode =
  | "login_required"
  | "consent_required"
  | "interaction_required"
  | "invalid_grant"
  | "missing_refresh_token"
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

export type CacheLocation = "memory" | "localstorage";

/**
 * Pluggable cache backend. `InMemoryCache` (default) and `LocalStorageCache`
 * implement this; a creator may supply their own via `AuthClientOptions.cache`.
 * Methods may be sync or async — the `CacheManager` awaits both.
 */
export interface ICache {
  set<T>(key: string, value: T): Promise<void> | void;
  get<T>(key: string): Promise<T | undefined> | (T | undefined);
  remove(key: string): Promise<void> | void;
  allKeys?(): Promise<string[]> | string[];
}

export interface AuthClientOptions {
  /** Defaults to `location.origin`. The app's same-origin gateway host. */
  appOrigin?: string;
  /** Default `"memory"`. Ignored when `cache` is supplied. */
  cacheLocation?: CacheLocation;
  /** Custom cache backend (overrides `cacheLocation`). */
  cache?: ICache;
  /** Default `false`. When `true` (+ `memory`), refresh tokens live in a Web Worker. */
  useRefreshTokens?: boolean;
  /** Default `["openid","profile","email"]`. */
  scope?: string[];
  /** Seconds before expiry to proactively refresh. Default `60`. */
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
