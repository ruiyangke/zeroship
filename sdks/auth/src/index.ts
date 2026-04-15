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
 */

/** Authenticated user profile from the platform. */
export interface User {
  id: string;
  email: string;
  name: string;
  avatar: string | null;
}

declare global {
  interface Window {
    __zs_user?: User | null;
  }
}

/**
 * Auth — works in both server and client contexts.
 *
 * - Server (`"use server"` modules): reads from V8 native context (injected by gateway)
 * - Client (React components): reads from `window.__zs_user` (injected by gateway into HTML)
 */
export const auth = {
  /**
   * Returns the authenticated user, or `null` if not authenticated.
   * Zero cost — no network call in either context.
   */
  getUser(): User | null {
    // Server: V8 native context (gateway decoded JWT and injected user)
    if (typeof zeroship !== "undefined") {
      return zeroship.auth.getUser() as User | null;
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
    // Server: use native requireUser which throws with 401 status
    if (typeof zeroship !== "undefined") {
      return zeroship.auth.requireUser() as User;
    }
    // Client: check window global
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
