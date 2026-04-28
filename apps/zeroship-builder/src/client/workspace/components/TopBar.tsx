// ─── Workspace TopBar ───────────────────────────────────────────
// Shared header for every page that knows about a current project.
// Keeps the same shell on Home (no project) and inside /p/:id.

import { Link, useLocation } from "react-router-dom";
import {
  ChevronLeft,
  Loader2,
  PanelLeftClose,
  PanelLeft,
  CircleUser,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { BuilderStatus } from "../../builder/types";

interface Props {
  /** Current project name; null on Home / Account / Login. */
  projectName?: string | null;
  /** Live agent status; only meaningful when in a project. */
  status?: BuilderStatus;
  /** Whether the chat rail is currently visible. */
  chatOpen?: boolean;
  /** Toggle chat rail visibility. */
  onToggleChat?: () => void;
  /** Logout handler from App.tsx. */
  onLogout?: () => void;
}

export function TopBar({
  projectName,
  status,
  chatOpen,
  onToggleChat,
  onLogout,
}: Props) {
  const { pathname } = useLocation();
  const inProject = !!projectName;

  return (
    <header
      data-testid="topbar"
      className="flex items-center gap-2 px-3 h-11 border-b border-border bg-background"
    >
      {inProject ? (
        <Button asChild variant="ghost" className="h-7 px-2 text-xs">
          <Link to="/" data-testid="topbar-home">
            <ChevronLeft className="size-3 mr-1" />
            home
          </Link>
        </Button>
      ) : (
        <Link
          to="/"
          className="text-sm font-bold tracking-[0.18em] uppercase text-primary px-1.5"
          data-testid="topbar-logo"
        >
          zeroship
        </Link>
      )}

      {inProject && (
        <>
          <span className="text-muted-foreground text-xs">/</span>
          <span className="text-sm font-medium" data-testid="topbar-project">
            {projectName}
          </span>
          {status && status !== "idle" && (
            <span className="inline-flex items-center gap-1 ml-1 text-[10px] uppercase tracking-wider text-muted-foreground">
              <Loader2 className="size-3 animate-spin" />
              {status}
            </span>
          )}
        </>
      )}

      <div className="flex-1" />

      {onToggleChat && inProject && (
        <Button
          type="button"
          variant="ghost"
          onClick={onToggleChat}
          className="h-7 px-2 text-xs gap-1.5"
          title={chatOpen ? "Hide chat (⌘/)" : "Show chat (⌘/)"}
          data-testid="topbar-toggle-chat"
        >
          {chatOpen ? (
            <PanelLeftClose className="size-3" />
          ) : (
            <PanelLeft className="size-3" />
          )}
          chat
        </Button>
      )}

      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button
            type="button"
            variant="ghost"
            className="h-7 w-7 p-0"
            data-testid="topbar-account"
            title="Account"
          >
            <CircleUser className="size-4" />
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-44">
          <DropdownMenuLabel className="text-[10px] uppercase tracking-wider text-muted-foreground">
            account
          </DropdownMenuLabel>
          <DropdownMenuItem asChild>
            <Link to="/account">profile</Link>
          </DropdownMenuItem>
          {onLogout && (
            <>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                className="text-destructive"
                onSelect={onLogout}
                data-testid="topbar-logout"
              >
                logout
              </DropdownMenuItem>
            </>
          )}
        </DropdownMenuContent>
      </DropdownMenu>

      {/* hidden — used by playwright to assert which page mounted */}
      <span data-testid="topbar-route" className="hidden">
        {pathname}
      </span>
    </header>
  );
}
