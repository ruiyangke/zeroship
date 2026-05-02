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
      // Three slots: brand, center (canvas pills / status), right
      // (actions). On phones we collapse the brand column and let the
      // center slot scroll horizontally if it overflows. On desktop
      // the original 280px brand column returns.
      className="grid items-center gap-2 sm:gap-4 border-b border-rule bg-paper px-3 sm:px-6 h-12 grid-cols-[auto_1fr_auto] md:grid-cols-[minmax(280px,auto)_1fr_auto]"
    >
      <div className="flex items-baseline gap-2 sm:gap-3 min-w-0">
        <Link
          to={homeHref}
          data-testid="topbar-logo"
          aria-label="zeroship home"
          className="font-display italic text-lg font-medium text-ink hover:opacity-80 focus:outline-2 focus:outline-tomato focus:outline-offset-2 rounded-sm"
        >
          zeroship<span className="text-tomato">.</span>
        </Link>
        {projectName && (
          <>
            <span className="text-rule hidden sm:inline">/</span>
            <span
              data-testid="topbar-project"
              className="font-sans text-sm font-medium text-ink truncate hidden sm:inline"
            >
              {projectName}
            </span>
          </>
        )}
      </div>

      <div className="flex items-center justify-center min-w-0 overflow-x-auto">{center}</div>

      <div className="flex items-center gap-2 sm:gap-3">
        {right}
        <Link
          to="/account"
          data-testid="topbar-account"
          aria-label="Account"
          className="inline-flex h-7 w-7 items-center justify-center rounded-full bg-ink text-paper font-sans text-xs font-semibold leading-none focus:outline-2 focus:outline-tomato focus:outline-offset-2"
        >
          {accountInitials}
        </Link>
      </div>
    </header>
  );
}
