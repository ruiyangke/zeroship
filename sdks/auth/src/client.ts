/**
 * `@zeroship/auth/client` — the headless browser client.
 *
 * `createAuthClient(options)` returns an {@link AuthClient} (Auth0/Supabase
 * parity) that drives the same-origin gateway endpoints
 * (`crates/gateway/src/auth_token.rs`, `crates/gateway/src/browser_auth.rs`):
 *
 *   signInWithOAuth → openPopup (sync) → GET /__zs/auth/authorize →
 *     relay postMessage → exchangeCodeForSession (POST /__zs/auth/token)
 *   getSession   — cache only, no network
 *   getUser      — GET /__zs/auth/session (always probes)
 *   refreshSession / silent renewal — GET /__zs/auth/session?mint=1 under navigator.locks
 *   onAuthStateChange — SIGNED_IN | SIGNED_OUT | TOKEN_REFRESHED | USER_UPDATED | RECOVERING
 *   signOut      — POST /__zs/auth/signout
 *   checkSession — breadcrumb-gated rehydration on init
 *
 * Every DOM/Web-API touchpoint is injectable via {@link ClientEnv} so the real
 * cache / locks / popup / relay / transport logic is unit-tested against fakes.
 */

import {
  AuthError,
  type AuthChangeEvent,
  type AuthClientOptions,
  type ICache,
  type Session,
  type SignInOptions,
  type SignOutOptions,
  type User,
} from "./types";
import {
  CacheManager,
  InMemoryCache,
  LocalStorageCache,
} from "./internal/cache";
import { Breadcrumb } from "./internal/breadcrumb";
import { Transport } from "./internal/transport";
import { TransactionManager, type Transaction } from "./internal/transaction";
import { generatePkce } from "./internal/pkce";
import { openPopup, runPopup } from "./internal/popup";
import { listenForRelay } from "./internal/relay";
import { RefreshLock } from "./internal/locks";
import { resolveEnv, type ClientEnv, type ResolvedEnv } from "./internal/env";

const DEFAULT_SCOPES = ["openid", "profile", "email"];
const DEFAULT_REFRESH_SKEW = 60;
/** 503 client_not_provisioned backoff schedule (ms), gateway §4.3. */
const PROVISIONING_BACKOFF_MS = [500, 1000, 2000, 4000, 8000];

type Listener = (event: AuthChangeEvent, session: Session | null) => void;

export interface AuthClient {
  /** Interactive sign-in. Resolves with the Session on success. */
  signInWithOAuth(opts?: SignInOptions): Promise<Session>;
  /** Phase-1: launches the popup to the hosted password page. */
  signInWithPassword(opts?: { scopes?: string[]; popup?: boolean }): Promise<Session>;

  /** Exchange an authorization code (popup relay / redirect callback) for a session. */
  exchangeCodeForSession(code: string, state?: string): Promise<Session>;

  /** Cheap, local. Returns the cached session or null. No network. */
  getSession(): Promise<Session | null>;
  /** Server-validated user via `GET /__zs/auth/session` (always probes). */
  getUser(): Promise<User | null>;
  /** Force a refresh from the server-held anchor family (`/session?mint=1`). */
  refreshSession(): Promise<Session>;
  /** Breadcrumb-gated rehydration after reload (first-party `/session?mint=1`). */
  checkSession(): Promise<Session | null>;

  /** True if a non-expired session is cached. Cache-derived, no network. */
  isAuthenticated(): boolean;
  /** Does the current session carry this scope? */
  hasScope(scope: string): boolean;
  /** Step-up: acquire a token with additional scopes via interactive popup. */
  requestScopes(scopes: string[]): Promise<Session>;
  /** Returns a valid access token, refreshing if needed (lock-serialized). */
  getAccessToken(): Promise<string>;
  /** Step-up via popup specifically (Auth0 `getAccessTokenWithPopup` parity). */
  getAccessTokenWithPopup(opts?: { scopes?: string[] }): Promise<string>;

  onAuthStateChange(cb: Listener): { unsubscribe(): void };

  signOut(opts?: SignOutOptions): Promise<void>;
}

function nowSecs(): number {
  return Math.floor(Date.now() / 1000);
}

class AuthClientImpl implements AuthClient {
  private readonly env: ResolvedEnv;
  private readonly appOrigin: string;
  private readonly scope: string[];
  private readonly refreshSkew: number;
  private readonly transport: Transport;
  private readonly cacheManager: CacheManager;
  private readonly breadcrumb: Breadcrumb;
  private readonly txns: TransactionManager;
  private readonly lock: RefreshLock;
  private readonly listeners = new Set<Listener>();

  /** Synchronously-readable cached session (mirror of the cache entry). */
  private current: Session | null = null;
  /** Coalesce concurrent `getAccessToken`/`refreshSession` minters. */
  private inflightMint: Promise<Session> | null = null;
  /** Run the unconditional first probe exactly once per page load. */
  private firstProbeDone = false;

  constructor(options: AuthClientOptions, injected?: ClientEnv) {
    this.env = resolveEnv(injected);
    this.appOrigin = options.appOrigin ?? this.env.location?.origin ?? "";
    this.scope = options.scope ?? DEFAULT_SCOPES;
    this.refreshSkew = options.refreshSkewSeconds ?? DEFAULT_REFRESH_SKEW;
    this.transport = new Transport(this.appOrigin, this.env.fetch);

    const backend: ICache =
      options.cache ??
      (options.cacheLocation === "localstorage"
        ? new LocalStorageCache(this.env.local)
        : new InMemoryCache());
    this.cacheManager = new CacheManager(backend, this.appOrigin);
    this.breadcrumb = new Breadcrumb(this.env.cookies, this.appOrigin);
    this.txns = new TransactionManager(this.env.session);
    this.lock = new RefreshLock(`zs.refresh.${this.appOrigin}`, this.env.locks);
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

  private async store(session: Session, event: AuthChangeEvent): Promise<Session> {
    this.current = session;
    await this.cacheManager.setSession(session);
    this.breadcrumb.set();
    this.emit(event, session);
    return session;
  }

  private async forget(emitSignedOut: boolean): Promise<void> {
    this.current = null;
    await this.cacheManager.clear();
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
    // forwards it to Hydra verbatim. `provider` (google/github/password) is
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

  async signInWithPassword(opts: { scopes?: string[]; popup?: boolean } = {}): Promise<Session> {
    // Phase-1: the hosted page handles the password form; the popup flow is
    // identical to OAuth with provider=password.
    return this.signInWithOAuth({ provider: "password", scopes: opts.scopes, popup: opts.popup });
  }

  async requestScopes(scopes: string[]): Promise<Session> {
    // Step-up: interactive popup with the union of current + requested scopes.
    // `prompt: "consent"` re-shows the consent screen so the newly requested
    // scopes are explicitly granted instead of being SSO-skipped.
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
    if (this.current && this.current.expires_at > nowSecs()) return this.current;
    // Hydrate the synchronous mirror from the cache entry if present.
    const entry = await this.cacheManager.getEntry(this.scope, nowSecs());
    if (entry) {
      this.current = CacheManager.toSession(entry);
      return this.current;
    }
    return null;
  }

  async getUser(): Promise<User | null> {
    // Always probes (force:true) — never trusts a browser-decoded token.
    try {
      const { user } = await this.transport.session();
      await this.cacheManager.setUser(user);
      if (this.current && this.userChanged(this.current.user, user)) {
        this.current = { ...this.current, user };
        await this.cacheManager.setSession(this.current);
        this.emit("USER_UPDATED", this.current);
      }
      return user;
    } catch (e) {
      if (e instanceof AuthError && e.code === "login_required") {
        this.breadcrumb.clear();
        await this.forget(this.current != null);
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

  async refreshSession(): Promise<Session> {
    return this.mintUnderLock();
  }

  async getAccessToken(): Promise<string> {
    const skew = nowSecs() + this.refreshSkew;
    if (this.current && this.current.expires_at > skew) {
      return this.current.access_token;
    }
    const session = await this.mintUnderLock();
    return session.access_token;
  }

  async getAccessTokenWithPopup(opts: { scopes?: string[] } = {}): Promise<string> {
    // Auth0 parity: always an INTERACTIVE step-up. Run the popup consent flow
    // for the requested (or current) scopes and return the freshly-minted token.
    // The caller MUST invoke this inside a user gesture so the popup is not blocked.
    const session = await this.requestScopes(opts.scopes ?? this.scope);
    return session.access_token;
  }

  /**
   * Mint a fresh session from the server-held anchor family
   * (`/session?mint=1`), serialized under navigator.locks and coalesced across
   * concurrent callers. A `login_required` clears the breadcrumb + signs out.
   */
  private mintUnderLock(): Promise<Session> {
    if (this.inflightMint) return this.inflightMint;
    const run = this.lock
      .run(async () => {
        // Re-check inside the lock: a sibling tab may have just minted.
        const fresh = await this.cacheManager.getEntry(this.scope, nowSecs() + this.refreshSkew);
        if (fresh) {
          const s = CacheManager.toSession(fresh);
          this.current = s;
          return s;
        }
        const session = await this.transport.sessionMint();
        return this.store(session, "TOKEN_REFRESHED");
      })
      .catch(async (e) => {
        if (e instanceof AuthError && e.code === "login_required") {
          this.breadcrumb.clear();
          await this.forget(this.current != null);
        }
        throw e;
      })
      .finally(() => {
        this.inflightMint = null;
      });
    this.inflightMint = run;
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
    // This spares a cached/hydrated caller an extra mint on init.
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
          await this.forget(false);
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
    await this.forget(wasSignedIn);
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
  type ICache,
  type Session,
  type SignInOptions,
  type SignOutOptions,
  type User,
} from "./types";
