/**
 * Auth primitives (zeroship.auth.*)
 */

/** The authenticated user — decoded from the platform session by the gateway. */
interface ZeroshipAuthUser {
  id: string;
  email: string;
  name: string;
  avatar: string | null;
}

/** The zeroship.auth namespace — synchronous user context from the gateway. */
interface ZeroshipAuth {
  /** Returns the authenticated user, or null if not authenticated. */
  getUser(): ZeroshipAuthUser | null;
  /** Returns the authenticated user, or throws a 401 error if not authenticated. */
  requireUser(): ZeroshipAuthUser;
}
