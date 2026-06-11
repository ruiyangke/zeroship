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
import type { User } from "./types";

export type { User } from "./types";

/**
 * Shape of the platform-injected `env.auth` namespace. Kept narrow so the SDK
 * refuses to silently accept malformed shapes — a missing `getUser` resolves
 * to `null` instead of returning whatever the callee installed.
 */
interface EnvAuth {
  getUser?: () => User | null;
  requireUser?: () => User;
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
   * Returns the authenticated user, or throws an "Authentication required"
   * error carrying `status: 401` (+ `code: "unauthenticated"`). The kernel
   * primitive throws the same status-bearing shape; the gateway/worker
   * dispatch path reads `.status` and surfaces a clean 401 (a 4xx, so the
   * 5xx body-sanitizer does NOT mask the message). A status-less throw would
   * default to 500 and be masked as "internal error" (ISS-67).
   */
  requireUser(): User {
    const ea = envAuth();
    if (ea?.requireUser) {
      return ea.requireUser();
    }
    throw Object.assign(new Error("Authentication required"), {
      status: 401,
      code: "unauthenticated",
    });
  },

  /** Returns `true` if the current request is authenticated. */
  isLoggedIn(): boolean {
    return this.getUser() !== null;
  },

  // NOTE: there is intentionally NO server-side `signOut` here. The gateway
  // only registers `POST /__zeroship/auth/signout` (a state-changing endpoint guarded
  // by `X-ZS-Auth` + exact-Origin); a worker handler cannot issue that POST,
  // and a 302 redirect would land the browser on a GET the gateway 405s. Sign
  // out from the browser via the headless client (`@zeroship/auth/client`):
  // `client.signOut()` POSTs with the same-origin guard the gateway requires.
};

export default auth;
