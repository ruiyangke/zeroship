import type { ReactNode } from "react";
import { Link, NavLink } from "react-router-dom";
import { AppShell, Button, Cluster } from "@zeroship/ui";

import { UnreadBadge, UserChip } from "./SessionChips";
import type { AsyncState } from "./rpc";
import type { CurrentUser } from "./types";

/**
 * The application frame: one header band over the page, and no rail.
 *
 * The rail is gone and its four destinations sit in the header beside the
 * brand. This app has been here before -- it ran a horizontal nav, replaced it
 * with the rail, and is now back in a row -- so what is different this time.
 * The nav that lost the argument carried SIX links and grew a badge on one of
 * them; this one carries four, because advanced search became a modal over the
 * list and took its own destination and its results page with it. Four short
 * labels fit a row with room to spare, where six wrapped and crowded.
 *
 * The other half is horizontal budget. The rail was 240px of permanent width
 * in front of a page whose main content is a 1440px issue table with ten
 * columns, and it spent that width on four words -- a column that reserves
 * space forever to say what a row of the header says for free. The links are
 * short, the destinations are few, and the table is wide; that combination is
 * what the rail was the wrong shape for.
 *
 * AppShell SURVIVES, minus Body and Sidebar. With nothing in the rail those
 * two are a Split row wrapping a single column, and the rest of the shell is
 * what this app was actually using: a full-height frame that pins the header
 * and gives Main the scroll, the banner/main landmarks, and the baked-in
 * skip-to-content link. Header and Main are the only parts left, so they are
 * children of the root directly -- Body is required only when there is a rail
 * for it to lay out.
 */

/**
 * `identity: true` means the destination cannot work without a session, and it
 * is read off `src/server/config.ts` rather than off the route name.
 *
 * Only the dashboard qualifies. It is built from `notifications.list`,
 * `cc.listMine`, `votes.listMine`, `watchers.list` and `flags.listRequests`,
 * every one of them `auth: "user"`, and the page already answers a signed-out
 * visitor with a sign-in prompt. Offering the link anyway is an invitation to
 * a page whose whole content is a refusal.
 *
 * Issues, Products and Reports stay: `issues.search`, `products.list` and the
 * five `reports.*` procedures are all `auth: "anon", publiclyAccessible: true`.
 * Anonymous browsing is the point of this tracker, and hiding those would be a
 * worse bug than the one this fixes.
 */
const LINKS: { label: string; href: string; identity?: boolean }[] = [
  { label: "Issues", href: "/issues" },
  { label: "My dashboard", href: "/dashboard", identity: true },
  { label: "Products", href: "/products" },
  { label: "Reports", href: "/reports" },
];

export function Shell({
  userState,
  children,
}: {
  userState: AsyncState<CurrentUser>;
  children: ReactNode;
}) {
  const signedIn = userState.status === "ready";

  return (
    <AppShell>
      <AppShell.Header>
        <Cluster justify="between" align="center" style={{ inlineSize: "100%" }}>
          <Cluster align="center" gap={4}>
            <Link to="/issues" className="brand">
              Issue Tracker
            </Link>
            {/* asChild: the nav landmark IS the row, so there is no wrapper
                div between <nav> and the links it labels. */}
            <Cluster asChild align="center" gap={1}>
              <nav aria-label="Primary">
                {LINKS.filter((link) => signedIn || !link.identity).map((link) => (
                  <NavLink
                    key={link.href}
                    to={link.href}
                    className="nav-link"
                    // NavLink sets aria-current="page" itself, so the styling
                    // rule and the accessible state cannot drift apart -- and
                    // the app no longer threads a route name down here to work
                    // out which link is current.
                  >
                    <span>{link.label}</span>
                    {link.href === "/dashboard" ? <UnreadBadge signedIn={signedIn} /> : null}
                  </NavLink>
                ))}
              </nav>
            </Cluster>
          </Cluster>
          <Cluster align="center" gap={2}>
            {/* asChild, not render: this library composes onto the child element,
                so the button styling lands on a real anchor and "New issue"
                stays a link you can middle-click.

                Only when there is someone to file as. The new-issue page
                refuses without an identity -- it says so and stops -- so
                signed out this was a primary button leading to a dead end,
                sitting beside the Sign in button in the same blue and
                competing with it for the one action that actually works. */}
            {signedIn ? (
              <Button variant="filled" size="small" asChild>
                <Link to="/issues/new">New issue</Link>
              </Button>
            ) : null}
            <UserChip userState={userState} />
          </Cluster>
        </Cluster>
      </AppShell.Header>

      <AppShell.Main>{children}</AppShell.Main>
    </AppShell>
  );
}
