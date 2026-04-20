/**
 * @zeroship/auth — Universal auth SDK (works in both server and client).
 *
 * The platform manages authentication. The app just reads the current user.
 * No tokens, no passwords, no login logic.
 *
 * Usage:
 *   import { auth } from "@zeroship/auth";
 *
 *   const user = auth.getUser();       // { id, email, name, avatar } | null
 *   const user = auth.requireUser();   // throws if not authenticated
 *   auth.signOut();                    // redirects to platform logout (client only)
 *
 * ## Server-side status (kernel-cut transition)
 *
 * Between PR 1 D1 (deleted the `globalThis.zeroship` facade) and the
 * future AuthPlugin wiring, the server-side `env.auth` namespace is NOT
 * populated. Server calls to `getUser()` fall back to `null` and
 * `requireUser()` throws "Authentication required". The client-side
 * path (`window.__zs_user`) remains fully functional for SSR'd HTML.
 *
 * TODO(PR 4+): re-enable server-side getUser by either (a) adding an
 * AuthPlugin that exposes `env.auth.getUser()` via the registrar, or
 * (b) reading the gateway-injected user directly off a request-scoped
 * context exposed through the `zeroship` module. The orphan callbacks
 * in `crates/runtime/src/auth.rs` still exist and can be re-wired.
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

declare global {
  interface Window {
    __zs_user?: User | null;
  }
}

/**
 * Shape the SDK expects on `env.auth` when an AuthPlugin is registered.
 * Kept narrow so the server lookup doesn't silently accept malformed
 * shapes — calls that miss `getUser` fall through to the null path.
 */
interface EnvAuth {
  getUser?: () => User | null;
  requireUser?: () => User;
}

/** Resolve `env.auth` if an auth plugin is registered on this runtime. */
function envAuth(): EnvAuth | null {
  const ea = (env as { auth?: EnvAuth } | undefined)?.auth;
  return ea && typeof ea === "object" ? ea : null;
}

/**
 * Auth — works in both server and client contexts.
 *
 * - Server (`"use server"` modules): reads from `env.auth.getUser()`
 *   if an AuthPlugin is registered. Currently no such plugin exists on
 *   the kernel-cut branch (see file-level TODO), so server calls
 *   degrade to returning null / throwing "Authentication required".
 * - Client (React components): reads from `window.__zs_user`
 *   (injected by the gateway into the SSR'd HTML).
 */
export const auth = {
  /**
   * Returns the authenticated user, or `null` if not authenticated.
   * Zero cost — no network call in either context.
   */
  getUser(): User | null {
    // Server: env.auth namespace (populated by AuthPlugin when registered).
    const ea = envAuth();
    if (ea?.getUser) {
      return ea.getUser();
    }
    // Client: gateway-injected window global
    if (typeof window !== "undefined" && window.__zs_user) {
      return window.__zs_user;
    }
    return null;
  },

  /**
   * Returns the authenticated user, or throws an error.
   * On the server, the gateway intercepts the 401 and redirects to the login page.
   */
  requireUser(): User {
    // Server: use env.auth.requireUser which throws with 401 status.
    const ea = envAuth();
    if (ea?.requireUser) {
      return ea.requireUser();
    }
    // Fallback (both no-AuthPlugin server and client): manual null check.
    const user = this.getUser();
    if (!user) throw new Error("Authentication required");
    return user;
  },

  /** Returns true if a user is authenticated. */
  isLoggedIn(): boolean {
    return this.getUser() !== null;
  },

  /**
   * Signs the user out by redirecting to the platform logout page.
   * Client-only — no-op on the server.
   */
  signOut(): void {
    if (typeof window !== "undefined") {
      window.location.href = "/__auth/logout";
    }
  },
};
