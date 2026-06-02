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
 *   - `POST /__zeroship/auth/session`         → `{ user, expires_at }`
 *   - `GET  /__zeroship/auth/session[?mint=1]` → `{ user, expires_at }`
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
 * `__Host-zeroship_app_session` cookie — the live request credential, sent
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
 * `GET /__zeroship/auth/session?mint=1`) is signalled as `SESSION_REFRESHED` (the old
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
  /**
   * The framed `auth.zeroship.ai/login` rejected the supplied password. It is
   * surfaced via the relay's `{error, error_description, state}` envelope and
   * mapped through here, so the modal can show a "wrong email or password"
   * message; the auth origin re-renders the framed login for an in-frame retry.
   */
  | "invalid_credentials"
  /** `400` from the gateway/auth on a malformed/missing field. */
  | "invalid_request"
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
  /**
   * The auth-service origin (e.g. `https://auth.zeroship.ai`). Used ONLY as the
   * same-site sanity check for the immersive iframe gate: the iframe is selected
   * for `provider:'password'` only when `eTLD+1(appOrigin) === eTLD+1(authOrigin)`.
   * The SDK cannot infer same-site from `appOrigin` alone (it is the console's
   * own origin; the cross-site hop to the auth service happens server-side in
   * the gateway 302), so this explicit input is required. Unset ⇒ never iframe.
   */
  authOrigin?: string;
  /**
   * Opt in to the immersive in-page login iframe for the first-party password
   * UI. Default `false` → the popup window is used everywhere. The iframe is
   * chosen ONLY when `immersive === true` AND `appOrigin`/`authOrigin` are
   * same-site (§6.5); a misconfigured or cross-site surface falls back to the
   * working popup. The platform console sets `immersive: true` +
   * `authOrigin: 'https://auth.zeroship.ai'`; creator-app / custom-domain builds
   * leave both unset.
   */
  immersive?: boolean;
  /**
   * Resolve the DOM host the immersive login `<iframe>` is mounted INTO (the
   * modal's host slot). When it returns an element, the iframe fills that slot
   * so the surrounding modal chrome — title + accessible close button — stays
   * ABOVE it and the cancel affordance is reachable (§8/§10.5). When it returns
   * `null` (or is unset) the iframe mounts as a bare full-viewport overlay on
   * `document.body` (the headless, modal-less default). The React `AuthModal`
   * wires this to its host-slot ref automatically; standalone callers driving
   * the immersive flow themselves set it to their own container.
   */
  iframeMount?: () => Element | null;
  /**
   * Resolve the per-flow USER-CANCEL promise for the immersive login iframe.
   * The React `AuthModal` returns a promise that resolves when the user
   * dismisses the modal, so the in-flight sign-in rejects `popup_closed` and the
   * iframe is torn down (§8). Unset ⇒ the flow only settles on the relay or the
   * 60-second timeout (an iframe has no `closed` event).
   */
  iframeCancelled?: () => Promise<void> | undefined;
}

export interface SignInOptions {
  /**
   * The login UI to drive. `'password'` selects the platform's own first-party
   * login (`auth.zeroship.ai/login`): an in-page iframe on the same-site console
   * when `immersive` is enabled, a popup window everywhere else. `'google'` /
   * `'github'` are federated providers, always a popup window (their IdP refuses
   * to be framed). All three are forwarded to Hydra as `idp_hint`. Whatever the
   * surface, the credential is never handled by app/console JS — the iframe and
   * the popup both isolate it inside the auth origin.
   */
  provider?: "password" | "google" | "github";
  scopes?: string[];
  /** Default `true`. Popup vs full-page redirect. */
  popup?: boolean;
  /** Where to return after a redirect (popup ignores). */
  redirectTo?: string;
  /**
   * OIDC `prompt` passthrough for step-up re-authentication. `login` forces a
   * fresh credential challenge; `consent` re-shows the consent screen. Omitted
   * by default so Hydra's SSO skip fires. Forwarded verbatim to
   * `GET /__zeroship/auth/authorize` → Hydra (`browser_auth.rs`).
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
