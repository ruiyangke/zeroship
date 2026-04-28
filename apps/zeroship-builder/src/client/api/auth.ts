// Re-export server-function auth API under the same names the
// dashboard's React code imports.

export {
  register,
  login,
  logout,
  userinfo,
  type AuthUser,
  type UserInfo,
} from "../../server/auth";

/** Same-origin URL the browser navigates to in order to start the
 *  Google OAuth dance. The platform gateway forwards /auth/* to the
 *  control plane. Pure string manipulation — kept on the client so
 *  it stays a sync getter (callers compose it into anchor `href`s). */
export function googleStartUrl(
  returnTo: string = window.location.pathname + window.location.search,
): string {
  const safe = returnTo && returnTo.startsWith("/") && !returnTo.startsWith("//")
    ? returnTo : "/";
  const q = new URLSearchParams({ return: safe }).toString();
  return `/auth/google/start?${q}`;
}

/** Match the dashboard's AuthError shape. The server function
 *  proxy throws a plain Error with `.status` attached. */
export class AuthError extends Error {
  status: number;
  constructor(message: string, status: number) {
    super(message);
    this.name = "AuthError";
    this.status = status;
  }
}
