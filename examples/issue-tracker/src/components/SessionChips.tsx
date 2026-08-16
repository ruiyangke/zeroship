// The two session chips the app shell header renders: who you are, and how
// many unread notifications you have.
//
// This file used to hold the horizontal Nav as well. The sidebar rail replaced
// it and the component stayed behind, exported and rendered nowhere -- the same
// orphan QuickSearchBox became. The rail is gone now and the links are back in
// the header, but they did NOT come back here: navigation lives in Shell.tsx,
// which is the one place that knows what the frame is.
import { useAuth } from "@zeroship/auth/react";
import { Button, Menu } from "@zeroship/ui";
import { errorMessage } from "./rpc";
import type { Session } from "./session";
import { useUnreadNotificationCount } from "../lib/queries";


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
      size="sm"
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

export function UserChip({ session }: { session: Session }) {
  if (session.status === "loading") {
    return <span className="dim whitespace-nowrap text-base text-ink-muted">checking session...</span>;
  }
  if (session.status === "out") {
    return <SignInAction />;
  }
  if (session.status === "unknown") {
    // NOT a Sign in button. The server failed to answer rather than answering
    // "nobody", and offering sign-in here would explain an empty page with a
    // reason that is not the reason.
    return (
      <span className="warn whitespace-nowrap text-base text-warning" title={errorMessage(session.error)}>
        session unavailable
      </span>
    );
  }
  const data = session.user;
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
      <Menu.Trigger render={<Button variant="plain" size="sm" />}>
        <span className="whitespace-nowrap text-base text-ink-secondary" title={email ?? undefined}>
          {name}
          {!provisioned ? <em className="text-ink-muted not-italic"> (no activity yet)</em> : null}
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
  // A QUERY, not a bespoke fetch. This was a useState + useEffect keyed on
  // [signedIn], which meant marking a notification read updated the dashboard
  // and left this number untouched until the next sign-in: the mutation
  // invalidated notifications.unreadCount and nothing was subscribed to it.
  // Reading through the cache is what connects the two.
  const query = useUnreadNotificationCount({ enabled: signedIn });
  const count = signedIn && query.isSuccess ? query.data.count : null;

  if (count === null || count === 0) return null;
  return (
    <span className="ml-1 inline-flex rounded-full bg-danger px-2 text-xs font-bold text-white">
      {count}
    </span>
  );
}
