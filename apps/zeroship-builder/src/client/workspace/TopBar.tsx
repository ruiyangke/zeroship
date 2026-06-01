// ─── Workspace TopBar — crystal header bar ──────────────────────────
//
// The WORKSPACE top bar (distinct from components/TopBar). Three zones:
// brand/crumb (start), an optional center status strip, and a right
// zone for workspace actions (tier toggle, tour button, live-URL chip
// — all supplied by WorkspaceShell via the `right` slot) plus the
// account dot.
//
// Built over @zeroship/ui: a Cluster-based three-zone header, the brand
// trail expressed as DS Breadcrumbs (wordmark → project), and the
// account dot as a DS Avatar wrapped in a react-router Link. Bespoke
// type / positioning lives in TopBar.css against --zs-* tokens.

import type { ReactNode } from "react";
import { Link } from "react-router-dom";
import { Avatar, Breadcrumbs, Cluster } from "@zeroship/ui";
import "./TopBar.css";

export interface TopBarProps {
  projectName?: string | null;
  /** Visible only when in a workspace; clicking jumps back home. */
  homeHref?: string;
  /** Center slot — usually a status strip (deploying, etc.). */
  center?: ReactNode;
  /** Right-side custom content rendered before the account dot. */
  right?: ReactNode;
  /** Account avatar / initials. Click → /account. */
  accountInitials?: string;
}

export function TopBar({
  projectName,
  homeHref = "/",
  center,
  right,
  accountInitials = "·",
}: TopBarProps) {
  return (
    // Three zones: brand, center (canvas pills / status), right
    // (actions). On phones the brand column collapses and the center
    // slot scrolls horizontally if it overflows; the wider brand column
    // returns on desktop (handled in TopBar.css).
    <header data-testid="topbar" className="ws-topbar">
      <Breadcrumbs
        className="ws-topbar__brand"
        separator={<span className="ws-topbar__sep">/</span>}
      >
        <Breadcrumbs.Item>
          <Breadcrumbs.Link asChild>
            <Link
              to={homeHref}
              data-testid="topbar-logo"
              aria-label="zeroship home"
              className="ws-topbar__logo"
            >
              zeroship<span className="ws-topbar__logo-dot">.</span>
            </Link>
          </Breadcrumbs.Link>
        </Breadcrumbs.Item>
        {projectName ? (
          <Breadcrumbs.Item className="ws-topbar__project-item">
            <Breadcrumbs.Page
              data-testid="topbar-project"
              className="ws-topbar__project"
            >
              {projectName}
            </Breadcrumbs.Page>
          </Breadcrumbs.Item>
        ) : null}
      </Breadcrumbs>

      <div className="ws-topbar__center">{center}</div>

      <Cluster gap={3} align="center" justify="end" className="ws-topbar__right">
        {right}
        <Link
          to="/account"
          data-testid="topbar-account"
          aria-label="Account"
          className="ws-topbar__account"
        >
          <Avatar size="sm" shape="circle" fallback={accountInitials} />
        </Link>
      </Cluster>
    </header>
  );
}
