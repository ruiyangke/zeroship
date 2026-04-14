/**
 * @zeroship/auth/client — Client-side auth SDK.
 *
 * Reads the current user from `window.__zs_user`, which the gateway injects
 * into every HTML response. No network calls, no tokens.
 *
 * Usage:
 *   import { auth } from "@zeroship/auth/client";
 *
 *   const user = auth.getUser();   // { id, email, name, avatar } | null
 *   if (!user) navigate("/");      // redirect to home if not logged in
 *   auth.signOut();                // redirects to platform logout page
 */

/** Authenticated user profile from the platform. */
export interface User {
  id: number;
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
 * Client-side auth — reads the user injected by the gateway into `window.__zs_user`.
 */
export const auth = {
  /**
   * Returns the authenticated user, or `null` if not logged in.
   * Reads from `window.__zs_user` — synchronous, zero cost.
   */
  getUser(): User | null {
    return (typeof window !== "undefined" && window.__zs_user) || null;
  },

  /** Returns true if a user is authenticated. */
  isLoggedIn(): boolean {
    return this.getUser() !== null;
  },

  /**
   * Signs the user out by redirecting to the platform logout page.
   * The platform clears the session cookie and redirects back.
   */
  signOut(): void {
    if (typeof window !== "undefined") {
      window.location.href = `${window.location.origin}/__auth/logout`;
    }
  },
};
