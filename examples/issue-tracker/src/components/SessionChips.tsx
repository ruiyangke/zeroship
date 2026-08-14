// The two session chips the app shell header renders: who you are, and how
// many unread notifications you have.
//
// This file used to hold the horizontal Nav as well. AppShell's sidebar rail
// replaced it and the component stayed behind, exported and rendered nowhere
// -- the same orphan QuickSearchBox became. Navigation lives in Shell.tsx.
import { useEffect, useState } from "react";
import { SignInButton } from "@zeroship/auth/react";
import type { AuthError } from "@zeroship/auth/types";
import { unreadNotificationCount } from "../api";
import { errorMessage, isUnauthenticated, toPromise, type AsyncState } from "./rpc";
import type { CurrentUser } from "./types";


export function UserChip({ userState }: { userState: AsyncState<CurrentUser> }) {
  if (userState.status === "loading") {
    return <span className="user-chip dim">checking session...</span>;
  }
  if (userState.status === "error") {
    if (isUnauthenticated(userState.error)) {
      // A BUTTON, not the words "not signed in". The state was reported and
      // never actioned, which is the whole of the "cannot sign in" report.
      //
      // The reload is deliberate: identity here comes from the server session
      // (users.me), so the page has to ask again once the cookie exists.
      return (
        <SignInButton
          className="user-chip-signin"
          onError={(err: AuthError) => window.alert(`Sign in failed: ${err.message}`)}
        >
          Sign in
        </SignInButton>
      );
    }
    return (
      <span className="user-chip warn" title={errorMessage(userState.error)}>
        session unavailable
      </span>
    );
  }
  const { data } = userState;
  return (
    <span className="user-chip" title={data.email ?? undefined}>
      {data.name}
      {!data.isProvisioned ? <em> (no activity yet)</em> : null}
    </span>
  );
}

export function UnreadBadge({ signedIn }: { signedIn: boolean }) {
  const [count, setCount] = useState<number | null>(null);

  useEffect(() => {
    if (!signedIn) {
      setCount(null);
      return;
    }
    let cancelled = false;
    toPromise(unreadNotificationCount({}))
      .then((res) => {
        if (!cancelled) setCount(res.count);
      })
      .catch(() => {
        if (!cancelled) setCount(null);
      });
    return () => {
      cancelled = true;
    };
  }, [signedIn]);

  if (count === null || count === 0) return null;
  return <span className="unread-badge">{count}</span>;
}
