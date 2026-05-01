// ─── PublicNav — top-of-page nav for unauthed visitors ──────────
//
// The TopBar component pulls in useAuth and the account dot, which
// is right for authed pages but wrong for the public surfaces. This
// is a lighter header: wordmark on the left, nav links centred /
// right, "Sign in" emphasised. Used by Marketing, Pricing, Skills,
// Templates (public), About, Changelog, and the legal pages.

import { Link, useLocation } from "react-router-dom";
import { cn } from "../lib/utils";

const LINKS: { to: string; label: string }[] = [
  { to: "/pricing", label: "Pricing" },
  { to: "/templates", label: "Templates" },
  { to: "/skills", label: "Skills" },
  { to: "/about", label: "About" },
];

export function PublicNav() {
  const { pathname } = useLocation();

  return (
    <header
      className="grid items-center gap-4 border-b border-rule bg-paper px-6 py-3"
      style={{ gridTemplateColumns: "minmax(180px, auto) 1fr auto" }}
      data-testid="public-nav"
    >
      <Link
        to="/"
        className="font-serif italic text-[18px] font-medium text-ink hover:opacity-80"
        style={{ textDecoration: "none" }}
        data-testid="public-nav-logo"
      >
        zeroship<span className="text-tomato">.</span>
      </Link>

      <nav className="flex items-center justify-center gap-7">
        {LINKS.map((l) => {
          const active = pathname === l.to || pathname.startsWith(`${l.to}/`);
          return (
            <Link
              key={l.to}
              to={l.to}
              className={cn(
                "font-sans text-[11px] uppercase tracking-[0.2em] transition-colors hover:text-ink",
                active ? "text-ink" : "text-ink-soft",
              )}
              style={{ textDecoration: "none" }}
              data-testid={`public-nav-${l.label.toLowerCase()}`}
            >
              {l.label}
            </Link>
          );
        })}
      </nav>

      <div className="flex items-center gap-3">
        <Link
          to="/login"
          className="font-sans text-[11px] uppercase tracking-[0.2em] text-ink-soft hover:text-ink transition-colors"
          style={{ textDecoration: "none" }}
          data-testid="public-nav-signin"
        >
          Sign in
        </Link>
        <Link
          to="/signup"
          className="inline-flex items-center font-sans text-[11px] uppercase tracking-[0.2em] bg-ink text-paper px-3 py-1.5 hover:opacity-90 transition-opacity"
          style={{ textDecoration: "none" }}
          data-testid="public-nav-signup"
        >
          Sign up
        </Link>
      </div>
    </header>
  );
}
