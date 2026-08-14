// The two session chips the app shell header renders: who you are, and how
// many unread notifications you have.
//
// This file used to hold the horizontal Nav as well. The sidebar rail replaced
// it and the component stayed behind, exported and rendered nowhere -- the same
// orphan QuickSearchBox became. The rail is gone now and the links are back in
// the header, but they did NOT come back here: navigation lives in Shell.tsx,
// which is the one place that knows what the frame is.
import { useEffect, useState } from "react";
import { useAuth } from "@zeroship/auth/react";
import { Button, Menu } from "@zeroship/ui";
import { unreadNotificationCount } from "../api";
import { errorMessage, isUnauthenticated, toPromise, type AsyncState } from "./rpc";
import type { CurrentUser } from "./types";


/**
 * The way in, wearing the app's clothes.
 *
 * @zeroship/auth ships SignInButton, but it renders an unstyled <button> --
 * next to the design system's filled "New issue" it read as a browser default
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
  return <AccountMenu name={data.name} email={data.email ?? null} provisioned={data.isProvisioned} />;
}

/**
 * Who you are, and the way back out.
 *
 * The name was a <span>. Signing in was added and signing out was not, so
 * the app could take an identity and never put one down -- on a shared
 * machine the only way out was clearing the cookie by hand.
 *
 * A menu rather than a second header button: sign-out is not something you
 * want one mis-click away from, and the name is the natural thing to press
 * when you want to act on your account.
 */
function AccountMenu({
  name,
  email,
  provisioned,
}: {
  name: string;
  email: string | null;
  provisioned: boolean;
}) {
  const { signOut } = useAuth();
  return (
    <Menu>
      {/* render, not a nested button: Menu.Trigger renders a bare <button>,
          which with no app rule for buttons is the browser's grey box -- next
          to the filled "New issue" it looked like a disabled control. Base UI
          lets the trigger BE the design system's button instead. Plain, so
          the name reads as text you can press rather than a second action
          competing with the primary one. */}
      <Menu.Trigger render={<Button variant="plain" size="small" />}>
        <span className="user-chip" title={email ?? undefined}>
          {name}
          {!provisioned ? <em> (no activity yet)</em> : null}
        </span>
      </Menu.Trigger>
      <Menu.Portal>
        <Menu.Popup>
          {/* Group wraps BOTH, because GroupLabel outside a Group throws and
              takes the whole page with it -- the header rendered nothing at
              all until this was composed the way the component documents.

              The email had no home but a title attribute, discoverable by
              hovering and by nothing else. Here it says WHICH account you
              are about to sign out of. */}
          <Menu.Group>
            {email ? <Menu.GroupLabel>{email}</Menu.GroupLabel> : null}
            <Menu.Item
              onClick={() => {
                // Not awaited, for the same reason sign-in is not: the
                // client may open a window inside the gesture. The session
                // change publishes and App refetches users.me from it.
                signOut().catch((err: unknown) => {
                  window.alert(`Sign out failed: ${err instanceof Error ? err.message : String(err)}`);
                });
              }}
            >
              Sign out
            </Menu.Item>
          </Menu.Group>
        </Menu.Popup>
      </Menu.Portal>
    </Menu>
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
