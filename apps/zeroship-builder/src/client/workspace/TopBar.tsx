import type { ReactNode } from "react";
import { Link } from "react-router-dom";

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
    <header
      data-testid="topbar"
      className="grid items-center gap-4 border-b border-rule bg-paper px-6 h-12"
      style={{ gridTemplateColumns: "minmax(280px, auto) 1fr auto" }}
    >
      <div className="flex items-baseline gap-3 min-w-0">
        <Link
          to={homeHref}
          data-testid="topbar-logo"
          className="font-display italic text-lg font-medium text-ink hover:opacity-80"
        >
          zeroship<span className="text-tomato">.</span>
        </Link>
        {projectName && (
          <>
            <span className="text-rule">/</span>
            <span
              data-testid="topbar-project"
              className="font-sans text-sm font-medium text-ink truncate"
            >
              {projectName}
            </span>
          </>
        )}
      </div>

      <div className="flex items-center justify-center min-w-0">{center}</div>

      <div className="flex items-center gap-3">
        {right}
        <Link
          to="/account"
          data-testid="topbar-account"
          aria-label="Account"
          className="inline-flex h-7 w-7 items-center justify-center rounded-full bg-ink text-paper font-sans text-xs font-semibold leading-none"
        >
          {accountInitials}
        </Link>
      </div>
    </header>
  );
}
