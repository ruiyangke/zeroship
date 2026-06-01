// ─── PublicNav — top-of-page nav for unauthed visitors ──────────
//
// The TopBar component pulls in useAuth and the account dot, which
// is right for authed pages but wrong for the public surfaces. This
// is a lighter header: wordmark on the left, nav links centred /
// right, "Sign up" emphasised. Used by Marketing, Pricing, Skills,
// Templates (public), About, Changelog, and the legal pages.
//
// Crystal skin: a real <header> band (bespoke chrome in PublicNav.css,
// all --zs-* tokens) wrapping a full-width Container + a single
// space-between Cluster. The brand + nav links are plain router
// <Link>s styled by the co-located sheet; the "Sign up" CTA is a DS
// Button rendered as the router Link via `asChild` (filled variant).
// Props, links, routing, active-state, and every data-testid are
// preserved exactly — only the presentation changed.

import { Link, useLocation } from "react-router-dom";
import { Button, Cluster, Container } from "@zeroship/ui";
import { cn } from "../lib/utils";
import "./PublicNav.css";

const LINKS: { to: string; label: string }[] = [
  { to: "/pricing", label: "Pricing" },
  { to: "/templates", label: "Templates" },
  { to: "/skills", label: "Skills" },
  { to: "/about", label: "About" },
];

export function PublicNav() {
  const { pathname } = useLocation();

  return (
    <header className="zs-public-nav" data-testid="public-nav">
      <Container size="full" padX={6}>
        <Cluster justify="between" gap={4} className="zs-public-nav__bar">
          <Link
            to="/"
            className="zs-public-nav__brand"
            data-testid="public-nav-logo"
          >
            zeroship<span className="zs-public-nav__brand-dot">.</span>
          </Link>

          <nav className="zs-public-nav__links" aria-label="Primary">
            {LINKS.map((l) => {
              const active =
                pathname === l.to || pathname.startsWith(`${l.to}/`);
              return (
                <Link
                  key={l.to}
                  to={l.to}
                  className={cn(
                    "zs-public-nav__link",
                    active && "zs-public-nav__link--active",
                  )}
                  data-active={active ? "true" : undefined}
                  data-testid={`public-nav-${l.label.toLowerCase()}`}
                >
                  {l.label}
                </Link>
              );
            })}
          </nav>

          <Cluster gap={4} className="zs-public-nav__actions">
            <Link
              to="/login"
              className="zs-public-nav__link"
              data-testid="public-nav-signin"
            >
              Sign in
            </Link>
            <Button asChild variant="filled" size="small">
              <Link to="/signup" data-testid="public-nav-signup">
                Sign up
              </Link>
            </Button>
          </Cluster>
        </Cluster>
      </Container>
    </header>
  );
}
