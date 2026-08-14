// The two session chips the app shell header renders: who you are, and how
// many unread notifications you have.
//
// This file used to hold the horizontal Nav as well. AppShell's sidebar rail
// replaced it and the component stayed behind, exported and rendered nowhere
// -- the same orphan QuickSearchBox became. Navigation lives in Shell.tsx.
import { useEffect, useState } from "react";
import { useAuth } from "@zeroship/auth/react";
import { Button } from "@zeroship/ui";
import { unreadNotificationCount } from "../api";
import { errorMessage, isUnauthenticated, toPromise, type AsyncState } from "./rpc";
import type { CurrentUser } from "./types";


/**
 * The way in, wearing the app's clothes.
 *
 * @zeroship/auth ships SignInButton, but it renders an unstyled <button> --
 * next to the design system's filled "New bug" it read as a browser default
 * someone forgot about. It also takes no asChild, so it cannot lend its
 * behaviour to a DS Button.
 *
 * The behaviour worth copying is one line, and it is the line that matters:
 * signInWithOAuth is called SYNCHRONOUSLY inside the click, never awaited,
 * so the popup opens within the user gesture. Await it and the browser
 * blocks the popup.
 */
function SignInAction() {
  const { signInWithOAuth } = useAuth();
  return (
    <Button
      variant="filled"
      size="small"
      onClick={() => {
        signInWithOAuth().catch((err: unknown) => {
          window.alert(`Sign in failed: ${err instanceof Error ? err.message : String(err)}`);
        });
      }}
    >
      Sign in
    </Button>
  );
}

export function UserChip({ userState }: { userState: AsyncState<CurrentUser> }) {
  if (userState.status === "loading") {
    return <span className="user-chip dim">checking session...</span>;
  }
  if (userState.status === "error") {
    if (isUnauthenticated(userState.error)) {
      return <SignInAction />;
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
