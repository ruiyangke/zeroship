"use server";

/**
 * @zeroship/auth — Server-side auth SDK.
 *
 * The platform manages authentication. The app just reads the current user.
 * No tokens, no passwords, no login logic.
 *
 * Usage:
 *   import { auth } from "@zeroship/auth";
 *
 *   const user = auth.getUser();       // { id, email, name, avatar } | null
 *   const user = auth.requireUser();   // throws 401 if not authenticated
 */

/** Authenticated user profile from the platform. */
export interface User {
  id: number;
  email: string;
  name: string;
  avatar: string | null;
}

/**
 * Server-side auth — reads the authenticated user from the request context.
 * The gateway validates the JWT and injects the user before your code runs.
 */
export const auth = {
  /**
   * Returns the authenticated user, or `null` if the request is not authenticated.
   * Zero cost — reads from request context, no network call.
   */
  getUser(): User | null {
    return zeroship.auth.getUser() as User | null;
  },

  /**
   * Returns the authenticated user, or throws a 401 error.
   * The gateway intercepts the 401 and redirects the browser to the login page.
   */
  requireUser(): User {
    return zeroship.auth.requireUser() as User;
  },
};
