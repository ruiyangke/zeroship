import { useState, type ReactNode } from "react";
import { AppShell, Badge, Button, Cluster, Stack } from "@zeroship/ui";

import type { RouteName } from "../App";
import { UnreadBadge, UserChip } from "./SessionChips";
import type { AsyncState } from "./rpc";
import type { CurrentUser } from "./types";

/**
 * The application frame, built on `@zeroship/ui`'s AppShell.
 *
 * Replaces a hand-rolled horizontal nav bar. The old one put six links in a
 * row across the top and grew a badge on one of them; the rail gives each
 * destination a full row, keeps the current one visibly selected, and leaves
 * the header for identity and the one action worth having everywhere.
 *
 * AppShell.Body is not optional -- it is the Split row that holds the rail and
 * Main and owns the collapse gate. Sidebar and Main as bare children of the
 * root render without either.
 */

const LINKS: { route: RouteName; label: string; href: string }[] = [
  { route: "bugs", label: "Bugs", href: "#/bugs" },
  { route: "dashboard", label: "My dashboard", href: "#/dashboard" },
  { route: "products", label: "Products", href: "#/products" },
  { route: "reports", label: "Reports", href: "#/reports" },
];

export function Shell({
  route,
  userState,
  children,
}: {
  route: RouteName;
  userState: AsyncState<CurrentUser>;
  children: ReactNode;
}) {
  const [sidebarOpen, setSidebarOpen] = useState(true);
  const signedIn = userState.status === "ready";

  return (
    <AppShell sidebarOpen={sidebarOpen} onSidebarOpenChange={setSidebarOpen}>
      <AppShell.Header>
        <Cluster justify="between" align="center" style={{ inlineSize: "100%" }}>
          <Cluster align="center" gap={2}>
            <Button
              variant="plain"
              size="small"
              aria-expanded={sidebarOpen}
              aria-label="Toggle navigation"
              onClick={() => setSidebarOpen((open) => !open)}
            >
              ☰
            </Button>
            <a href="#/bugs" className="brand">
              Issue Tracker
            </a>
          </Cluster>
          <Cluster align="center" gap={2}>
            {/* asChild, not render: this library composes onto the child element,
                so the button styling lands on a real anchor and "New bug"
                stays a link you can middle-click.

                Only when there is someone to file as. The new-bug page
                refuses without an identity -- it says so and stops -- so
                signed out this was a primary button leading to a dead end,
                sitting beside the Sign in button in the same blue and
                competing with it for the one action that actually works. */}
            {signedIn ? (
              <Button variant="filled" size="small" asChild>
                <a href="#/bugs/new">New bug</a>
              </Button>
            ) : null}
            <UserChip userState={userState} />
          </Cluster>
        </Cluster>
      </AppShell.Header>

      <AppShell.Body>
        <AppShell.Sidebar asChild>
          <nav aria-label="Primary">
            <Stack gap={1}>
              {LINKS.map((link) => (
                <a
                  key={link.route}
                  href={link.href}
                  className="rail-link"
                  // The current page is marked on the element, not by swapping
                  // class names, so the styling rule and the accessible state
                  // cannot drift apart.
                  aria-current={route === link.route ? "page" : undefined}
                >
                  <Cluster justify="between" align="center">
                    <span>{link.label}</span>
                    {link.route === "dashboard" ? (
                      <UnreadBadge signedIn={signedIn} />
                    ) : null}
                  </Cluster>
                </a>
              ))}
            </Stack>
          </nav>
        </AppShell.Sidebar>

        <AppShell.Main>{children}</AppShell.Main>
      </AppShell.Body>
    </AppShell>
  );
}

export { Badge };
