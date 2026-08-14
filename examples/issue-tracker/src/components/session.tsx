import { createContext, useContext, type ReactNode } from "react";

import { isUnauthenticated } from "./rpc";
import type { CurrentUser } from "./types";

/**
 * Who you are, asked once and answered in four states.
 *
 * Five pages each wrote the same two clauses to decide this:
 *
 *     const signedOut = userState.status === "error" && isUnauthenticated(userState.error);
 *
 * and six components each called `currentUser({})` to get the `userState` they
 * wrote it against. Measured on one visit to the issue list: `users.me` went
 * out FOUR times.
 *
 * The duplication was the smaller problem. That expression has only two
 * answers, and the question has four, so both of the other two got folded into
 * "signed in":
 *
 *   LOADING reads as signed in. Measured with `users.me` held open for four
 *   seconds: the issue list rendered the Assignee column, then removed it when
 *   the 401 landed. A visitor on a slow link watches an identity-only column
 *   appear and vanish. The header never had this bug because `UserChip`
 *   already branches on `loading` -- it is the one place that modelled the
 *   real shape.
 *
 *   A NON-AUTH FAILURE reads as signed in too. If `users.me` answers 500, the
 *   page offers every authenticated control, and each one fails on click.
 *   "We could not tell" and "you are signed in" are not the same fact.
 *
 * So the fix is not a wrapper component. A wrapper answers "may I render this
 * page at all", and only two of the five sites are asking that. The other
 * three are public pages with private PARTS -- the issue list is anonymous but
 * its advanced builder is not, the issue page is readable but not commentable,
 * the products page lists publicly and edits privately. A wrapper cannot say
 * that; it renders or it does not.
 *
 * What every site needs is the ANSWER, so this is a context plus a hook.
 * `RequireSession` below is then eight lines on top of it, for the two pages
 * that really are all-or-nothing.
 */

export type Session =
  /** `users.me` is still in flight. Assume NOTHING about identity here. */
  | { status: "loading" }
  /** Answered, and there is nobody. */
  | { status: "out" }
  /** Answered, and this is who. */
  | { status: "in"; user: CurrentUser }
  /**
   * Asked and could not be told -- a network failure or a 500, NOT a 401.
   * Deliberately its own state rather than being folded into `out`: a signed-in
   * user whose request failed is not a visitor, and showing them a "Sign in"
   * button would be a lie about why their page is empty.
   */
  | { status: "unknown"; error: unknown };

const SessionContext = createContext<Session | null>(null);

/**
 * Narrow a TanStack Query result to the four states that actually exist.
 *
 * The mapping is the app's knowledge, not the library's: `useQuery` reports
 * "error" for a 401 and for a 500 alike, and those are different facts. A 401
 * is the server ANSWERING that there is nobody; a 500 is the server failing to
 * answer. Collapsing them is what made a failed request look like a signed-out
 * visitor and offer a Sign in button as the explanation for an empty page.
 */
export function toSession(result: {
  isPending: boolean;
  isError: boolean;
  error: unknown;
  data: CurrentUser | undefined;
}): Session {
  if (result.isPending) return { status: "loading" };
  if (result.isError) {
    return isUnauthenticated(result.error)
      ? { status: "out" }
      : { status: "unknown", error: result.error };
  }
  return result.data
    ? { status: "in", user: result.data }
    : { status: "unknown", error: new Error("users.me resolved with no user") };
}

export function SessionProvider({
  session,
  children,
}: {
  session: Session;
  children: ReactNode;
}) {
  return <SessionContext.Provider value={session}>{children}</SessionContext.Provider>;
}

/**
 * The session, or a throw.
 *
 * Throwing rather than returning a default `loading` matters: a silent default
 * would let a component render outside the provider and quietly behave as
 * though the session were still arriving, forever. That is the same shape as
 * the bug this file exists to remove.
 */
export function useSession(): Session {
  const session = useContext(SessionContext);
  if (!session) {
    throw new Error("useSession must be rendered inside <SessionProvider>");
  }
  return session;
}

/**
 * True only when the server has ANSWERED and the answer is nobody.
 *
 * The name is deliberately not `signedOut`, which is what the old per-page
 * boolean was called and which invited exactly the wrong reading: `!signedIn`.
 * While loading this is false AND `isSignedIn` is false, because both are
 * claims and neither is known yet.
 */
export function isVisitor(session: Session): boolean {
  return session.status === "out";
}

export function isSignedIn(session: Session): session is { status: "in"; user: CurrentUser } {
  return session.status === "in";
}

/**
 * Render `children` only for a signed-in user; otherwise render `fallback`.
 *
 * For the all-or-nothing pages only. A page that is public with private parts
 * must ask `useSession()` directly -- wrapping it here would hide the public
 * half from the people it was made public for.
 */
export function RequireSession({
  children,
  fallback,
  pending = null,
}: {
  children: ReactNode;
  /** Shown once the server has said there is nobody. */
  fallback: ReactNode;
  /** Shown while the answer is still in flight. Defaults to nothing, which is
   *  better than briefly showing either the page or the sign-in prompt. */
  pending?: ReactNode;
}) {
  const session = useSession();
  if (session.status === "loading") return <>{pending}</>;
  if (session.status === "in") return <>{children}</>;
  return <>{fallback}</>;
}
