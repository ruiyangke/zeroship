/**
 * `@zeroship/auth/client` — the headless browser client.
 *
 * `createAuthClient(options)` returns an {@link AuthClient} (Auth0/Supabase-shaped,
 * BFF model) that drives the same-origin gateway endpoints
 * (`crates/gateway/src/auth_token.rs`, `crates/gateway/src/browser_auth.rs`):
 *
 *   signInWithOAuth → popup window (federated / non-same-site) OR an in-page
 *     iframe (the same-site console password UI, when `immersive`) →
 *     GET /__zeroship/auth/authorize → relay postMessage →
 *     exchangeCodeForSession (POST /__zeroship/auth/session)
 *   getSession   — cache only, no network (the in-memory identity snapshot)
 *   getUser      — GET /__zeroship/auth/session (always probes)
 *   refreshSession — GET /__zeroship/auth/session?mint=1 (re-mints the HttpOnly cookie)
 *   onAuthStateChange — SIGNED_IN | SIGNED_OUT | SESSION_REFRESHED | USER_UPDATED | RECOVERING
 *   signOut      — POST /__zeroship/auth/signout
 *   checkSession — breadcrumb-gated rehydration on init
 *
 * BFF INVARIANT: this client never hands a usable access/power token to browser
 * JS. There is no `getAccessToken`, no token cache, no client-held bearer. The
 * SPA authenticates its own-app requests with the HttpOnly `__Host-zeroship_app_session`
 * cookie (sent automatically, same-origin); a {@link Session} is identity only
 * (`{ user, expires_at, scopes }`). The client holds at most an in-memory
 * IDENTITY snapshot (no token) so `getSession()` can answer without a round-trip.
 *
 * Every DOM/Web-API touchpoint is injectable via {@link ClientEnv} so the real
 * popup / relay / transport / breadcrumb logic is unit-tested against fakes.
 */

import {
  AuthError,
  type AuthChangeEvent,
  type AuthClientOptions,
  type Session,
  type SignInOptions,
  type SignOutOptions,
  type User,
} from "./types";
import { Breadcrumb } from "./internal/breadcrumb";
import { Transport } from "./internal/transport";
import { TransactionManager, type Transaction } from "./internal/transaction";
import { generatePkce } from "./internal/pkce";
import { openPopup, runPopup } from "./internal/popup";
import { createIframe, runIframe } from "./internal/iframe";
import { listenForRelay } from "./internal/relay";
import { resolveEnv, sameSite, type ClientEnv, type ResolvedEnv } from "./internal/env";

const DEFAULT_SCOPES = ["openid", "profile", "email"];
const DEFAULT_REFRESH_SKEW = 60;
/** 503 client_not_provisioned backoff schedule (ms), gateway §4.3. */
const PROVISIONING_BACKOFF_MS = [500, 1000, 2000, 4000, 8000];

type Listener = (event: AuthChangeEvent, session: Session | null) => void;

export interface AuthClient {
  /**
   * Interactive sign-in. `provider:'password'` drives the platform's own
   * first-party login UI — the immersive in-page iframe on the same-site console
   * (when `immersive` is enabled), a popup window everywhere else;
   * `provider:'google'|'github'` are federated popup windows. Either way the
   * credential is isolated inside the auth origin and the SDK only ever receives
   * the relayed OAuth code. Resolves with the {@link Session} on success.
   */
  signInWithOAuth(opts?: SignInOptions): Promise<Session>;

  /** Exchange an authorization code (popup/iframe relay / redirect callback) for a session. */
  exchangeCodeForSession(code: string, state?: string): Promise<Session>;

  /** Cheap, local. Returns the in-memory identity snapshot or null. No network. */
  getSession(): Promise<Session | null>;
  /** Server-validated user via `GET /__zeroship/auth/session` (always probes). */
  getUser(): Promise<User | null>;
  /** Re-mint the HttpOnly session cookie + identity from the server-held anchor (`/session?mint=1`). */
  refreshSession(): Promise<Session>;
  /** Breadcrumb-gated rehydration after reload (first-party `/session?mint=1`). */
  checkSession(): Promise<Session | null>;

  /** True if a non-expired identity snapshot is held. Cache-derived, no network. */
  isAuthenticated(): boolean;
  /** Does the current session carry this scope? */
  hasScope(scope: string): boolean;
  /** Step-up: re-consent additional scopes via interactive popup. */
  requestScopes(scopes: string[]): Promise<Session>;

  onAuthStateChange(cb: Listener): { unsubscribe(): void };

  signOut(opts?: SignOutOptions): Promise<void>;
}

function nowSecs(): number {
  return Math.floor(Date.now() / 1000);
}

class AuthClientImpl implements AuthClient {
  private readonly env: ResolvedEnv;
  private readonly appOrigin: string;
  /** The auth-service origin (same-site sanity check for the iframe gate). */
  private readonly authOrigin: string;
  /** Opt-in to the immersive in-page login iframe for `provider:'password'`. */
  private readonly immersive: boolean;
  private readonly scope: string[];
  private readonly refreshSkew: number;
  private readonly transport: Transport;
  private readonly breadcrumb: Breadcrumb;
  private readonly txns: TransactionManager;
  private readonly listeners = new Set<Listener>();

  /**
   * The in-memory IDENTITY snapshot (NO token) — the source of truth for
   * `getSession()` / `isAuthenticated()` / `hasScope()`. Set only from a server
   * round-trip (exchange / mint / getUser) or cleared on sign-out. Never
   * persisted to storage; an XSS payload can read who the user is on this app
   * but there is no credential here to steal.
   */
  private current: Session | null = null;
  /** Coalesce concurrent `refreshSession` cookie re-mints within this tab. */
  private inflightRefresh: Promise<Session> | null = null;
  /** Run the unconditional first probe exactly once per page load. */
  private firstProbeDone = false;

  constructor(options: AuthClientOptions, injected?: ClientEnv) {
    // `options.iframeMount` / `options.iframeCancelled` (the React `AuthModal`'s
    // host-slot resolver + close-affordance cancel signal) feed the env's
    // default iframe factory so the immersive `<iframe>` mounts INSIDE the modal
    // slot — not as a full-viewport overlay that would cover the close button
    // (§8/§10.5) — and so dismissing the modal rejects the in-flight flow
    // (`popup_closed`). An explicitly injected env (tests/SSR) still wins
    // per-field; the option only supplies a field the injected env did not.
    this.env = resolveEnv(
      options.iframeMount || options.iframeCancelled
        ? {
            ...injected,
            iframeMount: injected?.iframeMount ?? options.iframeMount,
            iframeCancelled: injected?.iframeCancelled ?? options.iframeCancelled,
          }
        : injected,
    );
    this.appOrigin = options.appOrigin ?? this.env.location?.origin ?? "";
    this.authOrigin = options.authOrigin ?? "";
    this.immersive = options.immersive ?? false;
    this.scope = options.scope ?? DEFAULT_SCOPES;
    this.refreshSkew = options.refreshSkewSeconds ?? DEFAULT_REFRESH_SKEW;
    this.transport = new Transport(this.appOrigin, this.env.fetch);
    this.breadcrumb = new Breadcrumb(this.env.cookies, this.appOrigin);
    this.txns = new TransactionManager(this.env.session);
  }

  // ── state plumbing ─────────────────────────────────────────────────────

  private emit(event: AuthChangeEvent, session: Session | null): void {
    for (const cb of this.listeners) {
      try {
        cb(event, session);
      } catch {
        // a faulty listener must not break the auth flow
      }
    }
  }

  private store(session: Session, event: AuthChangeEvent): Session {
    this.current = session;
    this.breadcrumb.set();
    this.emit(event, session);
    return session;
  }

  private forget(emitSignedOut: boolean): void {
    this.current = null;
    if (emitSignedOut) this.emit("SIGNED_OUT", null);
  }

  onAuthStateChange(cb: Listener): { unsubscribe(): void } {
    this.listeners.add(cb);
    return {
      unsubscribe: () => {
        this.listeners.delete(cb);
      },
    };
  }

  // ── sign-in ────────────────────────────────────────────────────────────

  async signInWithOAuth(opts: SignInOptions = {}): Promise<Session> {
    const usePopup = opts.popup !== false;
    const scopes = opts.scopes ?? this.scope;
    // `prompt` is an OIDC passthrough for step-up (login/consent); the gateway
    // forwards it to Hydra verbatim. `provider` (password/google/github) is
    // threaded through as the Hydra `idp_hint` so the login UI can route to /
    // pre-select the named upstream IdP (Fix 5 — it is no longer dropped).
    const prompt = opts.prompt;
    const provider = opts.provider;

    if (!usePopup) {
      // Full-page redirect: persist the transaction, then navigate the popup-
      // less flow by setting window.location. Out of scope to await here.
      const { url } = await this.beginFlow(scopes, opts.redirectTo, prompt, provider);
      // A redirect flow hands control to the browser; resolve is never reached.
      (this.env.window as unknown as { location: { href: string } }).location.href = url;
      return new Promise<Session>(() => {
        /* navigation in progress */
      });
    }

    // The immersive iframe is selected — BEFORE the synchronous `openPopup`
    // gesture dance — ONLY for our first-party password UI, when opted in, AND
    // when the console is same-site with the auth service (§6.5). A federated
    // provider, a non-same-site surface, or `immersive:false` falls through to
    // the popup. The iframe has no popup-blocker constraint, so it does NOT need
    // the synchronous gesture: build the URL first, then create the frame with
    // the URL preset as `src`. A missing iframe factory (no DOM) returns
    // `undefined` here and the flow falls through to the popup.
    const useImmersive =
      provider === "password" && this.immersive && sameSite(this.appOrigin, this.authOrigin);
    if (useImmersive) {
      const viaFrame = await this.runImmersiveIframe(scopes, prompt, provider);
      if (viaFrame) return viaFrame;
      // else: no iframe factory — fall through to the popup launcher below.
    }

    // POPUP: open SYNCHRONOUSLY first (before the async URL build) to dodge blockers.
    const popup = openPopup(this.env);
    if (!popup) {
      throw new AuthError("popup_blocked", "the browser blocked the sign-in popup");
    }

    let url: string;
    let txn: Transaction;
    try {
      const begun = await this.beginFlow(scopes, undefined, prompt, provider);
      url = begun.url;
      txn = begun.txn;
    } catch (e) {
      try {
        popup.close();
      } catch {
        /* ignore */
      }
      throw e;
    }

    // Filter the origin-shared relay by THIS flow's state so a concurrent flow
    // or a stale message cannot cross-deliver a code to the wrong flow (MAJOR fix).
    const relay = listenForRelay(this.env, this.appOrigin, txn.state);
    const response = await runPopup(this.env, popup, url, relay);
    return this.completeFlow(response.code, response.state, response.error, response.error_description, txn);
  }

  /**
   * Drive the immersive in-page login iframe (the same-site console password
   * path). Builds the authorize URL FIRST (no popup-blocker gesture constraint),
   * then mounts the iframe with the URL pre-set as `src` and runs the SAME
   * relay → code-exchange dance as the popup. Returns `null` (no flow started,
   * the transaction is rolled back) when the environment has no iframe factory
   * (no DOM) so the caller can fall through to the popup. The `<iframe>` framing
   * the cross-origin `auth.zeroship.ai/login` keeps the credential out of
   * console JS.
   */
  private async runImmersiveIframe(
    scopes: string[],
    prompt: string | undefined,
    provider: SignInOptions["provider"],
  ): Promise<Session | null> {
    const { url, txn } = await this.beginFlow(scopes, undefined, prompt, provider);
    const frame = createIframe(this.env, url);
    if (!frame) {
      // No iframe factory (no DOM) — roll back the txn and let the caller fall
      // through to the popup launcher (no infinite re-entry into this method).
      this.txns.remove(txn.state);
      return null;
    }
    // Same origin-shared relay, filtered by THIS flow's state. The app-origin
    // callback posts to `window.parent` (the console top) inside the frame.
    const relay = listenForRelay(this.env, this.appOrigin, txn.state);
    const response = await runIframe(this.env, frame, url, relay);
    return this.completeFlow(
      response.code,
      response.state,
      response.error,
      response.error_description,
      txn,
    );
  }

  async requestScopes(scopes: string[]): Promise<Session> {
    // Step-up: interactive popup with the union of current + requested scopes.
    // `prompt: "consent"` re-shows the consent screen so the newly requested
    // scopes are explicitly granted instead of being SSO-skipped. The grant is
    // recorded server-side (control.oauth_grants); the browser still receives
    // identity only — never a token for the new scopes.
    const union = Array.from(new Set([...(this.current?.scopes ?? this.scope), ...scopes]));
    return this.signInWithOAuth({ scopes: union, popup: true, prompt: "consent" });
  }

  /** Mint a transaction + authorize URL (popup opened separately, beforehand). */
  private async beginFlow(
    scopes: string[],
    _redirectTo: string | undefined,
    prompt: string | undefined,
    provider: SignInOptions["provider"] | undefined,
  ): Promise<{ url: string; txn: Transaction }> {
    const pkce = await generatePkce(this.env.crypto);
    const redirectUri = this.transport.redirectUri();
    const txn: Transaction = {
      verifier: pkce.verifier,
      state: pkce.state,
      nonce: pkce.nonce,
      redirect_uri: redirectUri,
      scopes,
      createdAt: Date.now(),
    };
    this.txns.put(txn);
    const url = this.transport.authorizeUrl({
      challenge: pkce.challenge,
      state: pkce.state,
      nonce: pkce.nonce,
      scope: scopes,
      redirectUri,
      prompt,
      idpHint: provider,
    });
    return { url, txn };
  }

  /** Resolve a relay response into a Session (or reject with a typed error). */
  private async completeFlow(
    code: string | undefined,
    state: string | undefined,
    error: string | undefined,
    errorDescription: string | undefined,
    txn: Transaction,
  ): Promise<Session> {
    if (error) {
      // Clear the transaction — this flow is finished (failed).
      if (state) this.txns.remove(state);
      else this.txns.remove(txn.state);
      const code2 =
        error === "login_required"
          ? "login_required"
          : error === "consent_required"
            ? "consent_required"
            : error === "interaction_required"
              ? "interaction_required"
              : "server_error";
      throw new AuthError(code2 as never, errorDescription ?? error);
    }
    if (!state || state !== txn.state) {
      this.txns.remove(txn.state);
      throw new AuthError("invalid_state", "authorization response state did not match");
    }
    if (!code) {
      this.txns.remove(txn.state);
      throw new AuthError("server_error", "authorization response carried no code");
    }
    return this.exchangeCodeForSession(code, state);
  }

  // ── code exchange ──────────────────────────────────────────────────────

  async exchangeCodeForSession(code: string, state?: string): Promise<Session> {
    // Recover the transaction (and PKCE verifier) by state. Falls back to a
    // single pending transaction when no state is supplied (redirect callback).
    let txn: Transaction | null = null;
    if (state) {
      txn = this.txns.get(state);
    } else {
      const pending = this.txns.pending();
      txn = pending.length === 1 ? pending[0] : null;
    }
    if (!txn) {
      throw new AuthError(
        "missing_code_verifier",
        "no PKCE transaction for this authorization code",
      );
    }

    const session = await this.transport.exchangeCode({
      code,
      codeVerifier: txn.verifier,
      redirectUri: txn.redirect_uri,
      nowSecs: nowSecs(),
    });
    // The transaction is spent — clear it (completion).
    this.txns.remove(txn.state);
    return this.store(session, "SIGNED_IN");
  }

  // ── read paths ─────────────────────────────────────────────────────────

  async getSession(): Promise<Session | null> {
    // Cache-only: the in-memory identity snapshot. No network, no token.
    if (this.current && this.current.expires_at > nowSecs()) return this.current;
    return null;
  }

  async getUser(): Promise<User | null> {
    // Always probes (force:true) — never trusts a stale in-memory snapshot.
    try {
      const { user } = await this.transport.session();
      if (this.current && this.userChanged(this.current.user, user)) {
        this.current = { ...this.current, user };
        this.emit("USER_UPDATED", this.current);
      }
      return user;
    } catch (e) {
      if (e instanceof AuthError && e.code === "login_required") {
        this.breadcrumb.clear();
        this.forget(this.current != null);
        return null;
      }
      throw e;
    }
  }

  private userChanged(a: User, b: User): boolean {
    return (
      a.id !== b.id ||
      a.email !== b.email ||
      a.name !== b.name ||
      a.avatar !== b.avatar ||
      a.emailVerified !== b.emailVerified ||
      a.scopes.join(" ") !== b.scopes.join(" ")
    );
  }

  isAuthenticated(): boolean {
    return this.current != null && this.current.expires_at > nowSecs();
  }

  hasScope(scope: string): boolean {
    return this.current?.scopes.includes(scope) ?? false;
  }

  // ── refresh / silent renewal ───────────────────────────────────────────

  /**
   * Re-mint the HttpOnly session cookie + refresh the identity snapshot from
   * the server-held anchor family (`/session?mint=1`), coalesced across
   * concurrent in-tab callers. There is NO token to refresh — this re-probes
   * the cookie identity. A `login_required` clears the breadcrumb + signs out.
   * Cross-tab thundering-herd is already coalesced by the gateway's per-anchor
   * mint single-flight, so no client-side cross-tab lock is needed.
   */
  refreshSession(): Promise<Session> {
    if (this.inflightRefresh) return this.inflightRefresh;
    const run = this.transport
      .sessionMint()
      .then((session) => this.store(session, "SESSION_REFRESHED"))
      .catch((e) => {
        if (e instanceof AuthError && e.code === "login_required") {
          this.breadcrumb.clear();
          this.forget(this.current != null);
        }
        throw e;
      })
      .finally(() => {
        this.inflightRefresh = null;
      });
    this.inflightRefresh = run;
    return run;
  }

  // ── init / reload recovery ─────────────────────────────────────────────

  /**
   * Breadcrumb-gated rehydration. The FIRST `checkSession()` per page load
   * probes `/session?mint=1` UNCONDITIONALLY (so a cleared breadcrumb cannot
   * permanently suppress recovery); subsequent calls early-return anonymous
   * when the breadcrumb is absent. A `503 client_not_provisioned` keeps the
   * breadcrumb and retries under bounded backoff (RECOVERING), never SIGNED_OUT.
   */
  async checkSession(): Promise<Session | null> {
    const unconditional = !this.firstProbeDone;
    this.firstProbeDone = true;

    // Short-circuit: a non-expired in-memory session already reflects server
    // state (it can only have been set by a server round-trip or a just-
    // completed exchange), so even the unconditional first probe is redundant.
    if (this.current && this.current.expires_at > nowSecs()) {
      return this.current;
    }

    if (!unconditional && !this.breadcrumb.isPresent()) {
      // Repeat probe with no breadcrumb — stay anonymous, no network.
      return this.getSession();
    }
    return this.probeWithBackoff();
  }

  private async probeWithBackoff(): Promise<Session | null> {
    let recovering = false;
    for (let attempt = 0; ; attempt++) {
      try {
        const session = await this.transport.sessionMint();
        return this.store(session, "SIGNED_IN");
      } catch (e) {
        if (!(e instanceof AuthError)) throw e;
        if (e.code === "login_required") {
          // Definitive: clear the breadcrumb and go cleanly anonymous.
          this.breadcrumb.clear();
          this.forget(false);
          return null;
        }
        if (e.code === "client_not_provisioned") {
          // 503 — retryable. Keep the breadcrumb; emit RECOVERING once.
          if (!recovering) {
            recovering = true;
            this.emit("RECOVERING", null);
          }
          if (attempt < PROVISIONING_BACKOFF_MS.length) {
            await this.delay(PROVISIONING_BACKOFF_MS[attempt]);
            continue;
          }
          // Backoff exhausted — surface the typed error, breadcrumb intact.
          throw e;
        }
        // network_error / server_error — surface without clearing breadcrumb.
        throw e;
      }
    }
  }

  private delay(ms: number): Promise<void> {
    return new Promise((resolve) => {
      this.env.window.setTimeout(resolve, ms);
    });
  }

  // ── sign-out ───────────────────────────────────────────────────────────

  async signOut(opts: SignOutOptions = {}): Promise<void> {
    const scope = opts.scope === "global" ? "global" : "local";
    const wasSignedIn = this.current != null;
    try {
      await this.transport.signout(scope);
    } catch {
      // The network revoke is BEST-EFFORT: the gateway is idempotent and the
      // local clear below is authoritative for this device. A network failure
      // must not strand the user in a signed-in local state, so we swallow it
      // and complete the local sign-out (intent always wins).
    }
    this.breadcrumb.clear();
    this.forget(wasSignedIn);
  }
}

/** Construct a headless {@link AuthClient}. `injected` is for tests/SSR. */
export function createAuthClient(
  options: AuthClientOptions = {},
  injected?: ClientEnv,
): AuthClient {
  return new AuthClientImpl(options, injected);
}

export type { ClientEnv } from "./internal/env";
export {
  AuthError,
  type AuthChangeEvent,
  type AuthClientOptions,
  type AuthErrorCode,
  type Session,
  type SignInOptions,
  type SignOutOptions,
  type User,
} from "./types";
