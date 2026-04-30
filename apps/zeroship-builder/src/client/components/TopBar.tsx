// ─── TopBar — atelier header bar ────────────────────────────────
//
// One row, hairline rule below. Wordmark on the left (or a crumb
// when inside a project), optional center status, account dot on
// the right. Used on every authed page.

import { Link } from "react-router-dom";
import type { ReactNode } from "react";
import { useAuth } from "../auth/AuthContext";
import { cn } from "../lib/utils";

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
    <header
      className="grid items-center gap-4 border-b border-rule bg-paper px-6 py-3"
      style={{ gridTemplateColumns: "minmax(280px, auto) 1fr auto" }}
      data-testid="topbar"
    >
      <div className="flex items-baseline gap-3 min-w-0">
        {admin ? (
          <Link
            to="/admin"
            className="font-sans text-[9.5px] uppercase tracking-[0.22em] text-tomato hover:opacity-80"
            data-testid="topbar-admin"
          >
            ADMIN
          </Link>
        ) : (
          <Link to="/" className="font-serif italic text-[18px] font-medium text-ink hover:opacity-80" data-testid="topbar-logo">
            zeroship<span className="text-tomato">.</span>
          </Link>
        )}

        {crumb?.map((c, i) => (
          <span key={i} className="flex items-baseline gap-3">
            <span className="text-rule">/</span>
            {c.to ? (
              <Link to={c.to} className="font-serif italic text-[13px] text-ink-soft hover:text-ink hover:underline">
                {c.label}
              </Link>
            ) : (
              <span className="font-serif italic text-[13px] text-ink truncate">{c.label}</span>
            )}
          </span>
        ))}

        {projectName && (
          <>
            <span className="text-rule">/</span>
            <span className="font-serif text-[16px] font-medium text-ink truncate" data-testid="topbar-project">
              {projectName}
            </span>
          </>
        )}
      </div>

      <div className="flex items-center justify-center min-w-0">
        {center}
      </div>

      <div className="flex items-center gap-3">
        {right}
        <Link
          to="/account"
          title={user?.email ?? "account"}
          data-testid="topbar-account"
          className={cn(
            "inline-flex h-7 w-7 items-center justify-center rounded-full font-sans text-[10.5px] font-semibold leading-none",
            admin ? "bg-tomato text-paper" : "bg-ink text-paper"
          )}
        >
          {initials}
        </Link>
      </div>
    </header>
  );
}
