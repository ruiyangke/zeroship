/**
 * Gateway transport — the wire calls the SDK makes against the SAME-ORIGIN
 * `/__zeroship/auth/*` endpoints. Every shape here matches the gateway contract
 * exactly (`crates/gateway/src/auth_token.rs`, `crates/gateway/src/browser_auth.rs`).
 *
 *   - `GET  /__zeroship/auth/authorize`  — query params: code_challenge (S256),
 *     code_challenge_method=S256, state, nonce, scope, redirect_uri, prompt?,
 *     idp_hint? (the provider hint from `SignInOptions.provider`).
 *   - `POST /__zeroship/auth/session`    — the code→session exchange (the BFF reshape
 *     MERGED `/token` into `/session`; the old `POST /__zeroship/auth/token` route is
 *     GONE). Body `{grant_type:'authorization_code', code, code_verifier,
 *     redirect_uri?}` + `X-ZS-Auth`. → `{user, expires_at}` ONLY (no
 *     access_token / token_type / scope / id_token); identity travels in the
 *     HttpOnly signed `__Host-zeroship_app_session` cookie, never the body.
 *   - `GET  /__zeroship/auth/session`    — `{user, expires_at}`; with `?mint=1`
 *     (+`X-ZS-Auth`) re-signs a fresh session cookie from the anchor and returns
 *     `{user, expires_at}` (still no token in the body).
 *   - `POST /__zeroship/auth/signout`    — `{scope:'local'|'global'}` + `X-ZS-Auth` → 204.
 *
 * BFF model: the gateway never hands the browser a power token. The live
 * request credential is the HttpOnly, signed `__Host-zeroship_app_session` cookie,
 * which rides EVERY same-origin request automatically via
 * `credentials: 'include'` — there is nothing for app JS to attach as a Bearer.
 * So `exchangeCode`/`sessionMint` parse ONLY `{user, expires_at}` (the gateway
 * emits no token) and build an IDENTITY-only {@link Session}; the cookie is the
 * credential.
 *
 * The custom `X-ZS-Auth` header is the primary, browser-version-independent
 * same-origin defense; the gateway also exact-matches `Origin`. Both are
 * `credentials: 'include'` so the signed `__Host-zeroship_app_session` cookie (the
 * live credential) + the `__Host-zeroship_app_anchor` anchor (HttpOnly; the SDK never
 * reads its name) + the breadcrumb cookie ride along.
 */

import { AuthError, type AuthErrorCode, type Session, type User } from "../types";

const X_ZS_AUTH = "X-ZS-Auth";

/** Raw `user` projection the gateway returns (snake_case `email_verified`). */
interface WireUser {
  id: string;
  email: string | null;
  email_verified?: boolean;
  name: string | null;
  avatar: string | null;
  scopes?: string[];
}

/**
 * The merged `/__zeroship/auth/session` body (BFF model) — identity projection ONLY.
 * NO `access_token` / `token_type` / `scope` / `id_token`: under the BFF model
 * the credential is the HttpOnly signed cookie, never the body. `user` carries
 * the relay-swapped email + `pws_` id + the granted `scopes`; `expires_at` is
 * the cookie's Unix-seconds expiry.
 */
interface SessionResponse {
  user: WireUser;
  expires_at: number;
}

interface WireError {
  error?: string;
  error_description?: string;
}

/** Map the gateway error envelope to a typed {@link AuthError}. */
function mapError(status: number, body: WireError | null, fallback: string): AuthError {
  const raw = body?.error ?? "";
  const desc = body?.error_description ?? raw ?? fallback;
  const known: Record<string, AuthErrorCode> = {
    login_required: "login_required",
    consent_required: "consent_required",
    interaction_required: "interaction_required",
    invalid_grant: "invalid_grant",
    client_not_provisioned: "client_not_provisioned",
    // 403 from the gateway scope gate (auth-sdk Slice 3c, §5.3 / RFC 6750
    // §3.1). The body is `{"error":"scope_required","scope":"<space-joined>"}`;
    // the WWW-Authenticate header uses the RFC token `insufficient_scope`, but
    // the SDK contract code is `scope_required`.
    scope_required: "scope_required",
  };
  let code: AuthErrorCode = known[raw] ?? "server_error";
  // 503 maps to the retryable provisioning signal regardless of detail string.
  if (status === 503 && code === "server_error") code = "client_not_provisioned";
  if (status === 401 && code === "server_error") code = "login_required";
  return new AuthError(code, desc, { status });
}

/** Normalize a wire user to the public {@link User} (camelCase emailVerified). */
function normalizeUser(w: WireUser): User {
  return {
    id: w.id,
    email: w.email,
    emailVerified: w.email_verified ?? false,
    name: w.name,
    avatar: w.avatar,
    scopes: w.scopes ?? [],
  };
}

async function readError(res: Response, fallback: string): Promise<AuthError> {
  let body: WireError | null = null;
  try {
    body = (await res.json()) as WireError;
  } catch {
    body = null;
  }
  return mapError(res.status, body, fallback);
}

export class Transport {
  constructor(
    private readonly appOrigin: string,
    private readonly fetchImpl: typeof fetch,
  ) {}

  /** Build the `GET /__zeroship/auth/authorize` URL the popup/redirect navigates to. */
  authorizeUrl(params: {
    challenge: string;
    state: string;
    nonce: string;
    scope: string[];
    redirectUri: string;
    prompt?: string;
    /** Provider hint (`google`/`github`/`password`) → Hydra `idp_hint`. */
    idpHint?: string;
  }): string {
    const q = new URLSearchParams({
      code_challenge: params.challenge,
      code_challenge_method: "S256",
      state: params.state,
      nonce: params.nonce,
      scope: params.scope.join(" "),
      redirect_uri: params.redirectUri,
    });
    if (params.prompt) q.set("prompt", params.prompt);
    // `SignInOptions.provider` → `idp_hint` (Fix 5). The gateway parses it and
    // forwards it to Hydra so the login UI routes to the named upstream IdP.
    if (params.idpHint) q.set("idp_hint", params.idpHint);
    return `${this.appOrigin}/__zeroship/auth/authorize?${q.toString()}`;
  }

  /** The default popup-callback redirect URI on the app's own origin. */
  redirectUri(): string {
    return `${this.appOrigin}/__zeroship/auth/popup-callback`;
  }

  private async fetchJson(
    path: string,
    init: RequestInit,
    fallbackMsg: string,
  ): Promise<Response> {
    let res: Response;
    try {
      res = await this.fetchImpl(`${this.appOrigin}${path}`, {
        credentials: "include",
        ...init,
      });
    } catch (cause) {
      throw new AuthError("network_error", fallbackMsg, { cause });
    }
    return res;
  }

  /**
   * `POST /__zeroship/auth/session` — exchange the code (+ PKCE verifier) for a
   * session (BFF slice R1b merged the old `/token` route here). The gateway runs
   * the code→token exchange, stores the refresh family server-side, ISSUES the
   * signed `__Host-zeroship_app_session` cookie, and sets the anchor + breadcrumb. The
   * response body is `{user, expires_at}` ONLY — no token. The cookie (set on
   * this response, HttpOnly) is the live credential and rides every subsequent
   * same-origin request automatically.
   */
  async exchangeCode(input: {
    code: string;
    codeVerifier: string;
    redirectUri: string;
    nowSecs: number;
  }): Promise<Session> {
    const res = await this.fetchJson(
      "/__zeroship/auth/session",
      {
        method: "POST",
        headers: {
          [X_ZS_AUTH]: "1",
          "content-type": "application/json",
        },
        body: JSON.stringify({
          grant_type: "authorization_code",
          code: input.code,
          code_verifier: input.codeVerifier,
          redirect_uri: input.redirectUri,
        }),
      },
      "session exchange request failed",
    );
    if (!res.ok) throw await readError(res, "session exchange failed");
    const body = (await res.json()) as SessionResponse;
    return this.toSession(body);
  }

  /**
   * Build the IDENTITY-only SDK {@link Session} from the cookie-only `/session`
   * body. Under the BFF model there is no client-held token at all — the
   * HttpOnly cookie is the credential; `expires_at` mirrors the cookie's
   * lifetime so the client can proactively re-mint before it lapses.
   */
  private toSession(body: SessionResponse): Session {
    const user = normalizeUser(body.user);
    return {
      expires_at: body.expires_at,
      user,
      scopes: user.scopes,
    };
  }

  /**
   * `GET /__zeroship/auth/session` (no mint) — the server-validated user only. Used
   * by `getUser()` and the unconditional reload probe.
   */
  async session(): Promise<{ user: User }> {
    const res = await this.fetchJson(
      "/__zeroship/auth/session",
      { method: "GET" },
      "session request failed",
    );
    if (!res.ok) throw await readError(res, "session probe failed");
    const body = (await res.json()) as SessionResponse;
    return { user: normalizeUser(body.user) };
  }

  /**
   * `GET /__zeroship/auth/session?mint=1` — reload recovery / silent renewal. The
   * gateway rotates the server-held anchor family and RE-SIGNS a fresh
   * `__Host-zeroship_app_session` cookie (set on this response); the body is
   * `{user, expires_at}` ONLY — no token. Requires `X-ZS-Auth`.
   */
  async sessionMint(): Promise<Session> {
    const res = await this.fetchJson(
      "/__zeroship/auth/session?mint=1",
      { method: "GET", headers: { [X_ZS_AUTH]: "1" } },
      "session mint request failed",
    );
    if (!res.ok) throw await readError(res, "session mint failed");
    const body = (await res.json()) as SessionResponse;
    if (body.expires_at == null || body.user == null) {
      throw new AuthError("server_error", "mint response missing user/expires_at", {
        status: res.status,
      });
    }
    return this.toSession(body);
  }

  /** `POST /__zeroship/auth/signout` — revoke + clear. 204 on success (idempotent). */
  async signout(scope: "local" | "global"): Promise<void> {
    const res = await this.fetchJson(
      "/__zeroship/auth/signout",
      {
        method: "POST",
        headers: {
          [X_ZS_AUTH]: "1",
          "content-type": "application/json",
        },
        body: JSON.stringify({ scope }),
      },
      "signout request failed",
    );
    // 204 No Content is the success shape; any 2xx is acceptable.
    if (!res.ok) throw await readError(res, "signout failed");
  }
}
