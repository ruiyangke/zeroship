import type { ReactNode } from "react";
import { Link, useLocation } from "react-router-dom";

import { UnreadBadge, UserChip } from "./SessionChips";
import type { Session } from "./session";
import { AppShell } from "../ui/AppShell";
import { Button } from "../ui/Button";

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
  session,
  children,
}: {
  session: Session;
  children: ReactNode;
}) {
  const signedIn = session.status === "in";
  const { pathname } = useLocation();

  return (
    <AppShell>
      <AppShell.Header>
        <div
          className="flex min-w-0 flex-row flex-wrap items-center justify-between gap-2"
          style={{ inlineSize: "100%" }}
        >
          <div className="flex min-w-0 flex-row flex-wrap items-center justify-start gap-4">
            <Link
              to="/issues"
              className="text-lg font-bold tracking-tight text-ink! no-underline hover:text-accent-strong! hover:no-underline!"
            >
              Issue Tracker
            </Link>
            {/* The nav landmark is the row, with no wrapper between it and the
                links it labels. */}
            <nav
              aria-label="Primary"
              className="flex min-w-0 flex-row flex-wrap items-center justify-start gap-1"
            >
              {LINKS.filter((link) => signedIn || !link.identity).map((link) => {
                const current =
                  link.href === "/issues"
                    ? pathname === "/" ||
                      pathname === "/issues" ||
                      pathname.startsWith("/issues/")
                    : pathname === link.href || pathname.startsWith(`${link.href}/`);
                return (
                  <Link
                    key={link.href}
                    to={link.href}
                    aria-current={current ? "page" : undefined}
                    className="inline-flex items-center whitespace-nowrap rounded-lg px-2 py-1 text-md leading-snug font-medium text-ink-secondary! no-underline hover:bg-current/8 hover:text-ink! hover:no-underline! aria-[current=page]:bg-current/12 aria-[current=page]:font-semibold aria-[current=page]:text-ink!"
                  >
                    <span>{link.label}</span>
                    {link.href === "/dashboard" ? <UnreadBadge signedIn={signedIn} /> : null}
                  </Link>
                );
              })}
            </nav>
          </div>
          <div className="flex min-w-0 flex-row flex-wrap items-center justify-start gap-2">
            {/* render composes the Button onto the Link itself, while
                nativeButton={false} tells Base UI the target is not a native
                button. The styling lands on a real anchor and "New issue"
                stays a link you can middle-click.

                Only when there is someone to file as. The new-issue page
                refuses without an identity -- it says so and stops -- so
                signed out this was a primary button leading to a dead end,
                sitting beside the Sign in button in the same blue and
                competing with it for the one action that actually works. */}
            {signedIn ? (
              <Button
                variant="filled"
                nativeButton={false}
                render={<Link to="/issues/new" />}
              >
                New issue
              </Button>
            ) : null}
            <UserChip session={session} />
          </div>
        </div>
      </AppShell.Header>

      <AppShell.Main className="pt-5!">{children}</AppShell.Main>
    </AppShell>
  );
}
