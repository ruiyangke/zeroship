// ─── ProjectWorkspace TabNav ────────────────────────────────────
// Slim icon-tab strip under the top bar. Switches the right-pane
// content while leaving the chat rail intact.

import { NavLink, type To } from "react-router-dom";
import {
  MessageSquare,
  FileCode2,
  ScrollText,
  KeyRound,
  Settings as SettingsIcon,
} from "lucide-react";
import { cn } from "@/lib/utils";

interface Tab {
  to: To;
  label: string;
  icon: React.ComponentType<{ className?: string }>;
  testid: string;
}

interface Props {
  appId: string;
}

export function TabNav({ appId }: Props) {
  const base = `/p/${appId}`;
  const tabs: Tab[] = [
    { to: `${base}/chat`,     label: "chat",     icon: MessageSquare, testid: "tab-chat"     },
    { to: `${base}/files`,    label: "files",    icon: FileCode2,     testid: "tab-files"    },
    { to: `${base}/logs`,     label: "logs",     icon: ScrollText,    testid: "tab-logs"     },
    { to: `${base}/env`,      label: "env",      icon: KeyRound,      testid: "tab-env"      },
    { to: `${base}/settings`, label: "settings", icon: SettingsIcon,  testid: "tab-settings" },
  ];

  return (
    <nav
      data-testid="tabnav"
      className="flex items-center gap-0.5 px-2 h-8 border-b border-border bg-background"
    >
      {tabs.map(({ to, label, icon: Icon, testid }) => (
        <NavLink
          key={String(to)}
          to={to}
          end
          data-testid={testid}
          className={({ isActive }) =>
            cn(
              "inline-flex items-center gap-1.5 h-7 px-2 text-xs uppercase tracking-wider transition-colors",
              isActive
                ? "text-primary border-b-2 border-primary"
                : "text-muted-foreground border-b-2 border-transparent hover:text-foreground hover:border-border",
            )
          }
        >
          <Icon className="size-3" />
          {label}
        </NavLink>
      ))}
    </nav>
  );
}
