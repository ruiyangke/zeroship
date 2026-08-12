import { useEffect, useState } from "react";
import { unreadNotificationCount } from "../api";
import type { RouteName } from "../App";
import { errorMessage, isUnauthenticated, toPromise, type AsyncState } from "./rpc";
import type { CurrentUser } from "./types";

const LINKS: { route: RouteName; label: string; href: string }[] = [
  { route: "bugs", label: "Bugs", href: "#/bugs" },
  { route: "new-bug", label: "New bug", href: "#/bugs/new" },
  { route: "search", label: "Advanced search", href: "#/search" },
  { route: "dashboard", label: "My dashboard", href: "#/dashboard" },
  { route: "products", label: "Products", href: "#/products" },
  { route: "reports", label: "Reports", href: "#/reports" },
];

export function UserChip({ userState }: { userState: AsyncState<CurrentUser> }) {
  if (userState.status === "loading") {
    return <span className="user-chip dim">checking session...</span>;
  }
  if (userState.status === "error") {
    if (isUnauthenticated(userState.error)) {
      return <span className="user-chip dim">not signed in</span>;
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

export function Nav({
  route,
  userState,
}: {
  route: RouteName;
  userState: AsyncState<CurrentUser>;
}) {
  const signedIn = userState.status === "ready";
  return (
    <header className="nav">
      <a className="brand" href="#/bugs">
        Issue Tracker
      </a>
      <nav className="nav-links">
        {LINKS.map((link) => (
          <a
            key={link.route}
            href={link.href}
            className={route === link.route ? "active" : undefined}
          >
            {link.label}
            {link.route === "dashboard" ? <UnreadBadge signedIn={signedIn} /> : null}
          </a>
        ))}
      </nav>
      <UserChip userState={userState} />
    </header>
  );
}
