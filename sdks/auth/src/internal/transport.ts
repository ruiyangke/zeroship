/**
 * Gateway transport — the wire calls the SDK makes against the SAME-ORIGIN
 * `/__zs/auth/*` endpoints. Every shape here matches the gateway contract
 * exactly (`crates/gateway/src/auth_token.rs`, `crates/gateway/src/browser_auth.rs`).
 *
 *   - `GET  /__zs/auth/authorize`  — query params: code_challenge (S256),
 *     code_challenge_method=S256, state, nonce, scope, redirect_uri, prompt?.
 *   - `POST /__zs/auth/token`      — `{grant_type:'authorization_code', code,
 *     code_verifier, redirect_uri?}` + `X-ZS-Auth`. → `{access_token,
 *     token_type, expires_in, scope, user}`.
 *   - `GET  /__zs/auth/session`    — `{user}`; with `?mint=1` (+`X-ZS-Auth`) →
 *     `{user, access_token, token_type, expires_in, expires_at}`.
 *   - `POST /__zs/auth/signout`    — `{scope:'local'|'global'}` + `X-ZS-Auth` → 204.
 *
 * The custom `X-ZS-Auth` header is the primary, browser-version-independent
 * same-origin defense; the gateway also exact-matches `Origin`. Both are
 * `credentials: 'include'` so the `__Host-zs_app_session` anchor + breadcrumb
 * cookies ride along.
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

interface TokenResponse {
  access_token: string;
  token_type: "Bearer";
  expires_in: number;
  scope?: string;
  user: WireUser;
}

interface SessionResponse {
  user: WireUser;
  access_token?: string;
  token_type?: "Bearer";
  expires_in?: number;
  expires_at?: number;
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

  /** Build the `GET /__zs/auth/authorize` URL the popup/redirect navigates to. */
  authorizeUrl(params: {
    challenge: string;
    state: string;
    nonce: string;
    scope: string[];
    redirectUri: string;
    prompt?: string;
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
    return `${this.appOrigin}/__zs/auth/authorize?${q.toString()}`;
  }

  /** The default popup-callback redirect URI on the app's own origin. */
  redirectUri(): string {
    return `${this.appOrigin}/__zs/auth/popup-callback`;
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
   * `POST /__zs/auth/token` — exchange the code (+ PKCE verifier) for a
   * session. The gateway runs the code→token exchange, stores the refresh
   * family server-side, mints the wrapper, and sets the anchor + breadcrumb.
   */
  async exchangeCode(input: {
    code: string;
    codeVerifier: string;
    redirectUri: string;
    nowSecs: number;
  }): Promise<Session> {
    const res = await this.fetchJson(
      "/__zs/auth/token",
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
      "token exchange request failed",
    );
    if (!res.ok) throw await readError(res, "token exchange failed");
    const body = (await res.json()) as TokenResponse;
    const user = normalizeUser(body.user);
    const scopes = body.scope ? body.scope.split(/\s+/).filter(Boolean) : user.scopes;
    return {
      access_token: body.access_token,
      expires_at: input.nowSecs + body.expires_in,
      token_type: "Bearer",
      user,
      scopes,
    };
  }

  /**
   * `GET /__zs/auth/session` (no mint) — the server-validated user only. Used
   * by `getUser()` and the unconditional reload probe.
   */
  async session(): Promise<{ user: User }> {
    const res = await this.fetchJson(
      "/__zs/auth/session",
      { method: "GET" },
      "session request failed",
    );
    if (!res.ok) throw await readError(res, "session probe failed");
    const body = (await res.json()) as SessionResponse;
    return { user: normalizeUser(body.user) };
  }

  /**
   * `GET /__zs/auth/session?mint=1` — reload recovery / silent renewal. Mints
   * a fresh wrapper from the server-held anchor family. Requires `X-ZS-Auth`.
   */
  async sessionMint(): Promise<Session> {
    const res = await this.fetchJson(
      "/__zs/auth/session?mint=1",
      { method: "GET", headers: { [X_ZS_AUTH]: "1" } },
      "session mint request failed",
    );
    if (!res.ok) throw await readError(res, "session mint failed");
    const body = (await res.json()) as SessionResponse;
    if (!body.access_token || body.expires_at == null) {
      throw new AuthError("server_error", "mint response missing access_token", {
        status: res.status,
      });
    }
    const user = normalizeUser(body.user);
    return {
      access_token: body.access_token,
      expires_at: body.expires_at,
      token_type: "Bearer",
      user,
      scopes: user.scopes,
    };
  }

  /** `POST /__zs/auth/signout` — revoke + clear. 204 on success (idempotent). */
  async signout(scope: "local" | "global"): Promise<void> {
    const res = await this.fetchJson(
      "/__zs/auth/signout",
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
