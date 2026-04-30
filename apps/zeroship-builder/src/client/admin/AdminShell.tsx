// ─── AdminShell — frame for /admin/* pages ──────────────────────
//
// Shares paper/ink/tomato but motifs lean ledger: card-catalog
// density, hairline tables, IDs in mono. ADMIN badge replaces the
// wordmark; account dot is tomato-tinted.

import { Outlet, NavLink } from "react-router-dom";
import type { ReactNode } from "react";
import { TopBar } from "../components/TopBar";
import { cn } from "../lib/utils";

interface AdminLink { to: string; label: string }
const LINKS: AdminLink[] = [
  { to: "/admin",          label: "library" },
  { to: "/admin/apps",     label: "apps" },
  { to: "/admin/users",    label: "users" },
  { to: "/admin/revenue",  label: "revenue" },
  { to: "/admin/journal",  label: "journal" },
];

export function AdminShell({ pageLabel, crumb, children }: {
  pageLabel: string;
  crumb?: { label: string; to?: string }[];
  children?: ReactNode;
}) {
  return (
    <div className="min-h-screen flex flex-col">
      <TopBar
        admin
        crumb={[
          { label: "library", to: "/admin" },
          ...(crumb ?? []),
        ]}
      />
      <nav
        data-testid="admin-nav"
        className="flex items-center gap-1 px-6 py-1.5 border-b border-rule bg-paper-2"
      >
        {LINKS.map((l) => (
          <NavLink
            key={l.to}
            to={l.to}
            end={l.to === "/admin"}
            className={({ isActive }) =>
              cn(
                "px-3 py-1.5 font-sans text-[10.5px] uppercase tracking-[0.18em] transition-colors",
                isActive ? "text-ink border-b-[1.5px] border-tomato" : "text-pencil hover:text-ink"
              )
            }
          >
            {l.label}
          </NavLink>
        ))}
        <span className="ml-auto font-serif italic text-[12px] text-pencil">{pageLabel}</span>
      </nav>
      <main className="flex-1 overflow-auto px-10 py-10 mx-auto w-full" style={{ maxWidth: 1280 }}>
        {children ?? <Outlet />}
      </main>
    </div>
  );
}
