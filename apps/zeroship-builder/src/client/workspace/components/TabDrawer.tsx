// ─── TabDrawer — quiet "If you're curious:" strip ────────────────
//
// Foot of the preview area. Demoted dev tabs (files / logs / env /
// settings) live here — only when the creator goes looking.

import { NavLink } from "react-router-dom";
import { cn } from "../../lib/utils";

export interface TabDrawerProps {
  appId: string;
  /** Right-side meta — usually "last shipped …". */
  meta?: React.ReactNode;
}

const TABS = [
  { to: "preview",  label: "preview"  },
  { to: "files",    label: "files"    },
  { to: "logs",     label: "logs"     },
  { to: "env",      label: "env"      },
  { to: "settings", label: "settings" },
];

export function TabDrawer({ appId, meta }: TabDrawerProps) {
  const base = `/p/${appId}`;
  return (
    <nav
      data-testid="tab-drawer"
      className="flex items-center gap-1.5 px-6 py-2 border-t border-rule bg-paper font-sans text-[10px] uppercase tracking-[0.18em] text-pencil"
    >
      <span className="font-serif italic text-[12px] normal-case tracking-normal text-pencil mr-2">
        If you're curious:
      </span>
      {TABS.map((t) => (
        <NavLink
          key={t.to}
          to={`${base}/${t.to}`}
          end
          data-testid={`tab:${t.to}`}
          className={({ isActive }) =>
            cn(
              "px-2.5 py-1 border transition-colors",
              isActive
                ? "border-rule bg-paper-2 text-ink"
                : "border-transparent hover:text-ink",
            )
          }
        >
          {t.label}
        </NavLink>
      ))}
      {meta && (
        <span className="ml-auto font-serif italic text-[11px] normal-case tracking-normal text-ink-soft">
          {meta}
        </span>
      )}
    </nav>
  );
}
