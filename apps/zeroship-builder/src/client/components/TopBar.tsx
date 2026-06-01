// ─── TopBar — crystal header bar ────────────────────────────────
//
// One row, separator rule below. Wordmark on the left (or a crumb
// when inside a project), optional center status, account avatar on
// the right. Used on every authed page (via PageFrame).
//
// Built over @zeroship/ui: a Cluster-based three-zone header with the
// account dot rendered as a DS Avatar wrapped in a react-router Link.
// Bespoke type/positioning lives in TopBar.css against --zs-* tokens.

import { Link } from "react-router-dom";
import type { ReactNode } from "react";
import { Avatar, Cluster } from "@zeroship/ui";
import { useAuth } from "../auth/AuthContext";
import "./TopBar.css";

export interface TopBarProps {
  /** Project name when inside /p/:appId/* — renders as a crumb. */
  projectName?: string | null;
  /** Optional center slot — usually a status strip. */
  center?: ReactNode;
  /** Right-side custom content (renders before the account dot). */
  right?: ReactNode;
  /** Admin variant — replaces wordmark with a small ADMIN badge. */
  admin?: boolean;
  /** Crumb override — array of [label, href?] pairs after the leading wordmark. */
  crumb?: { label: string; to?: string }[];
}

export function TopBar({ projectName, center, right, admin, crumb }: TopBarProps) {
  const { user } = useAuth();
  const initials = (user?.name || user?.email || "·").slice(0, 2).toUpperCase();

  return (
    <header className="topbar" data-testid="topbar">
      <Cluster gap={3} align="center" className="topbar__brand">
        {admin ? (
          <Link to="/admin" className="topbar__admin" data-testid="topbar-admin">
            ADMIN
          </Link>
        ) : (
          // Authed wordmark routes to /home (the gallery). The public
          // marketing page lives at `/` and is shown to unauthed
          // visitors only.
          <Link to="/home" aria-label="zeroship home" className="topbar__logo" data-testid="topbar-logo">
            zeroship<span className="topbar__logo-dot">.</span>
          </Link>
        )}

        {crumb?.map((c, i) => (
          <span key={i} className="topbar__crumb">
            <span className="topbar__sep" aria-hidden="true">/</span>
            {c.to ? (
              <Link to={c.to} className="topbar__crumb-link">
                {c.label}
              </Link>
            ) : (
              <span className="topbar__crumb-current">{c.label}</span>
            )}
          </span>
        ))}

        {projectName && (
          <span className="topbar__crumb">
            <span className="topbar__sep" aria-hidden="true">/</span>
            <span className="topbar__project" data-testid="topbar-project">
              {projectName}
            </span>
          </span>
        )}
      </Cluster>

      <div className="topbar__center">{center}</div>

      <Cluster gap={3} align="center" justify="end" className="topbar__right">
        {right}
        <Link
          to="/account"
          title={user?.email ?? "account"}
          aria-label="Account"
          data-testid="topbar-account"
          className="topbar__account"
          data-admin={admin ? "true" : undefined}
        >
          {user?.avatar_url ? (
            <Avatar
              size="sm"
              shape="circle"
              src={user.avatar_url}
              alt={user.name || user.email || "account"}
              fallback={initials}
            />
          ) : (
            <Avatar size="sm" shape="circle" fallback={initials} />
          )}
        </Link>
      </Cluster>
    </header>
  );
}
