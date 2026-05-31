/**
 * `@zeroship/auth` (the `.` export) — the SERVER helper.
 *
 * A thin, ergonomic wrapper around the platform-injected `env.auth.*`
 * namespace. The gateway HMAC-signs the authenticated identity into a
 * `ZeroShip-User` header, the worker verifies + parses it, and the runtime
 * exposes the user via the kernel `env.auth.getUser()` / `requireUser()`
 * primitives. This package is the creator-facing surface on top.
 *
 * Authenticated identity is SERVER-SIDE only. Browser code that needs the
 * user uses the headless client (`@zeroship/auth/client`), which talks to the
 * same-origin gateway endpoints.
 *
 *   import { auth } from "@zeroship/auth";
 *   export default {
 *     async fetch(req, env) {
 *       const user = auth.getUser();      // User | null
 *       if (!user) return new Response("sign in please", { status: 401 });
 *       return new Response(`hello ${user.name ?? user.id}`);
 *     },
 *   };
 */

import { env } from "zeroship";
import type { AccessToken, GetAccessTokenOptions, User } from "./types";
import { AuthError, type AuthErrorCode } from "./types";

export type { AccessToken, GetAccessTokenOptions, User } from "./types";
export { CONTROL_PLANE_AUDIENCE } from "./types";

/** The raw `{ access_token, expires_at, scopes }` the runtime op resolves. */
interface RawAccessToken {
  access_token: string;
  expires_at: number;
  scopes: string[];
}

/**
 * Shape of the platform-injected `env.auth` namespace. Kept narrow so the SDK
 * refuses to silently accept malformed shapes — a missing `getUser` resolves
 * to `null` instead of returning whatever the callee installed.
 *
 * `getAccessToken` is the RUNTIME-MEDIATED power-token mint (R4): the Rust
 * runtime — not this JS — attaches the worker↔control `control_key` and the
 * current request's gateway-signed `ZeroShip-User` header, then POSTs the
 * control mint endpoint. App JS supplies ONLY `{ audience, scopes }`, so the
 * `control_key` is NEVER JS-visible and app code cannot assert an identity.
 */
interface EnvAuth {
  getUser?: () => User | null;
  requireUser?: () => User;
  getAccessToken?: (opts: GetAccessTokenOptions) => Promise<RawAccessToken>;
}

/** Resolve `env.auth` if the auth plugin is registered on this runtime. */
function envAuth(): EnvAuth | null {
  const ea = (env as { auth?: EnvAuth } | undefined)?.auth;
  return ea && typeof ea === "object" ? ea : null;
}

export const auth = {
  /**
   * Returns the authenticated user, or `null` if the request is anonymous.
   * Resolves against `env.auth.getUser()` — a kernel primitive backed by
   * per-request state populated from the gateway's `ZeroShip-User` header.
   */
  getUser(): User | null {
    const ea = envAuth();
    return ea?.getUser ? ea.getUser() : null;
  },

  /**
   * Returns the authenticated user, or throws "Authentication required". The
   * kernel primitive throws; the gateway/worker dispatch path translates that
   * into a 401 for the requesting client.
   */
  requireUser(): User {
    const ea = envAuth();
    if (ea?.requireUser) {
      return ea.requireUser();
    }
    throw new Error("Authentication required");
  },

  /** Returns `true` if the current request is authenticated. */
  isLoggedIn(): boolean {
    return this.getUser() !== null;
  },

  // NOTE: there is intentionally NO server-side `signOut` here. The gateway
  // only registers `POST /__zs/auth/signout` (a state-changing endpoint guarded
  // by `X-ZS-Auth` + exact-Origin); a worker handler cannot issue that POST,
  // and a 302 redirect would land the browser on a GET the gateway 405s. Sign
  // out from the browser via the headless client (`@zeroship/auth/client`):
  // `client.signOut()` POSTs with the same-origin guard the gateway requires.

  /**
   * Obtain a scoped, audience-bound, short-lived power token for the CURRENT
   * user, minted SERVER-SIDE in the control plane (R4). `audience` and `scopes`
   * are least-privilege: the call FAILS (it does not silently broaden) if a
   * requested scope is not in the grant for this app.
   *
   * This is the Supabase `service_role` / Cloudflare capability-binding shape:
   * the token is used server-side and MUST NOT reach the browser. Identity is
   * bound by the runtime (the gateway-signed `ZeroShip-User` header echoed
   * Rust-side); app JS can neither read the worker↔control `control_key` nor
   * assert an identity. An ordinary creator app cannot obtain a
   * control-audience token.
   *
   * Rejects with an {@link AuthError} whose `.code` is one of
   * `scope_required` / `consent_required` / `step_up_required` /
   * `forbidden_audience` / `login_required` / `config_error` / `server_error`.
   */
  async getAccessToken(opts: GetAccessTokenOptions): Promise<AccessToken> {
    const ea = envAuth();
    if (!ea?.getAccessToken) {
      throw new AuthError(
        "config_error",
        "getAccessToken is unavailable: the auth plugin is not registered, or this runtime has no control-plane mint configured",
      );
    }
    let raw: RawAccessToken;
    try {
      raw = await ea.getAccessToken({
        audience: opts.audience,
        scopes: opts.scopes ?? [],
      });
    } catch (err) {
      throw toAuthError(err);
    }
    return {
      accessToken: raw.access_token,
      expiresAt: raw.expires_at,
      scopes: raw.scopes ?? [],
    };
  },

  /**
   * A pre-bound `fetch` that injects the power token server-side as an
   * `Authorization: Bearer` on every outbound call — so app code never handles
   * the raw token (BFF-proxy style). The token is minted once on first use and
   * re-minted after expiry.
   *
   * ```ts
   * const callControl = auth.fetchAs({
   *   audience: CONTROL_PLANE_AUDIENCE,
   *   scopes: ["apps:read"],
   * });
   * const res = await callControl(`${CONTROL_URL}/apps`);
   * ```
   */
  fetchAs(
    opts: GetAccessTokenOptions,
  ): (input: RequestInfo | URL, init?: RequestInit) => Promise<Response> {
    let cached: AccessToken | null = null;
    const skewSecs = 10;
    const self = this;
    return async (input, init) => {
      const now = Math.floor(Date.now() / 1000);
      if (!cached || cached.expiresAt - skewSecs <= now) {
        cached = await self.getAccessToken(opts);
      }
      const headers = new Headers(init?.headers);
      headers.set("authorization", `Bearer ${cached.accessToken}`);
      return fetch(input as RequestInfo, { ...init, headers });
    };
  },
};

/**
 * Map a thrown error from the runtime op to an {@link AuthError}. The op
 * rejects with a coded error (`e.code`) mirroring the control mint's
 * `{ "error": <code> }`; we surface it as a typed `AuthError`.
 */
function toAuthError(err: unknown): AuthError {
  if (err instanceof AuthError) return err;
  const code =
    typeof err === "object" && err !== null && "code" in err
      ? String((err as { code: unknown }).code)
      : "";
  const message =
    typeof err === "object" && err !== null && "message" in err
      ? String((err as { message: unknown }).message)
      : "power-token mint failed";
  const known: AuthErrorCode[] = [
    "scope_required",
    "consent_required",
    "step_up_required",
    "forbidden_audience",
    "login_required",
    "config_error",
    "network_error",
    "server_error",
  ];
  // Map control-side codes that don't have a 1:1 AuthErrorCode.
  const mapped: Record<string, AuthErrorCode> = {
    unauthenticated: "login_required",
    unauthenticated_identity: "login_required",
    unsupported_audience: "forbidden_audience",
    not_configured: "config_error",
  };
  const finalCode: AuthErrorCode = (known as string[]).includes(code)
    ? (code as AuthErrorCode)
    : (mapped[code] ?? "server_error");
  return new AuthError(finalCode, message, { cause: err });
}

export default auth;
