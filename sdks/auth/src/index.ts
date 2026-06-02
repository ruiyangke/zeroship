/**
 * @zeroship/auth — thin wrapper around the platform-injected `env.auth.*`
 * namespace.
 *
 * After Phase 3 of the auth-server migration the gateway HMAC-signs the
 * authenticated identity into a `ZeroShip-User` header, the worker
 * verifies + parses that header, and the runtime exposes the resulting
 * user via the kernel `env.auth.getUser()` / `requireUser()` primitives.
 * This package is the creator-facing ergonomic surface on top.
 *
 * Authenticated identity is server-side only. Client-side React code that
 * needs the user must call back through a server fetch handler / RPC; the
 * previous `window.__zs_user` browser fallback has been removed.
 *
 * Usage:
 *
 *   import { auth } from "@zeroship/auth";
 *
 *   export default {
 *     async fetch(req, env) {
 *       const user = auth.getUser();        // User | null
 *       if (!user) return new Response("sign in please", { status: 401 });
 *       return new Response(`hello ${user.name ?? user.id}`);
 *     },
 *   };
 *
 *   // Or short-circuit with the throw helper:
 *   const user = auth.requireUser();        // throws if unauthenticated
 */

import { env } from "zeroship";

/** Authenticated user profile from the platform. */
export interface User {
  id: string;
  email: string;
  name: string;
  avatar: string | null;
  emailVerified: boolean;
}

/**
 * Shape of the platform-injected `env.auth` namespace. Kept narrow so the
 * SDK refuses to silently accept malformed shapes — a missing `getUser`
 * resolves to `null` instead of returning whatever the callee installed.
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
   * Returns the authenticated user, or throws "Authentication required".
   * The kernel primitive throws an Error; the gateway/worker dispatch path
   * translates that into a 401 for the requesting client.
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

  /**
   * Trigger sign-out. Returns a 302 Response the handler should return
   * directly; the gateway's `/__zs/auth/signout` endpoint clears the
   * per-app session cookie and redirects to the OIDC end-session flow.
   */
  signOut(returnTo?: string): Response {
    const location = `/__zs/auth/signout${
      returnTo ? `?return=${encodeURIComponent(returnTo)}` : ""
    }`;
    return new Response(null, {
      status: 302,
      headers: { Location: location },
    });
  },
};

export default auth;
