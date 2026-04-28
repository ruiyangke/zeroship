// ─── TopBar ─────────────────────────────────────────────────────
// Builder header: app identity, deploy/status indicator, code-toggle.

import { Code2, Loader2, ChevronLeft, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Link } from "react-router-dom";
import type { BuilderStatus } from "../types";

interface Props {
  appName: string | null;
  status: BuilderStatus;
  showCode: boolean;
  onToggleCode: () => void;
  onReset: () => void;
}

export function TopBar({ appName, status, showCode, onToggleCode, onReset }: Props) {
  return (
    <header className="border-b border-border bg-background px-3 py-2 flex items-center gap-2">
      <Button asChild variant="ghost" className="h-7 px-2 text-xs">
        <Link to="/apps">
          <ChevronLeft className="size-3 mr-1" /> apps
        </Link>
      </Button>

      <div className="text-xs font-mono text-muted-foreground">/</div>

      <div className="flex items-center gap-2">
        <span className="text-sm font-medium">{appName ?? "new project"}</span>
        <StatusPill status={status} />
      </div>

      <div className="flex-1" />

      <Button
        type="button"
        variant="ghost"
        onClick={onReset}
        title="Clear conversation"
        className="h-7 px-2 text-xs gap-1.5 text-muted-foreground hover:text-foreground"
      >
        <Trash2 className="size-3" />
      </Button>

      <Button
        type="button"
        variant={showCode ? "outline" : "ghost"}
        onClick={onToggleCode}
        className="h-7 px-2.5 text-xs gap-1.5"
      >
        <Code2 className="size-3" />
        {showCode ? "hide code" : "show code"}
      </Button>
    </header>
  );
}

function StatusPill({ status }: { status: BuilderStatus }) {
  if (status === "idle") {
    return (
      <span className="inline-flex items-center gap-1 text-[10px] font-mono uppercase tracking-wider text-muted-foreground">
        <span className="size-1.5 rounded-full bg-emerald-500" /> idle
      </span>
    );
  }
  const label = status === "thinking" ? "thinking" : status === "deploying" ? "deploying" : status === "calling-tool" ? "tool" : "error";
  const color = status === "error" ? "bg-destructive" : status === "deploying" ? "bg-amber-500" : "bg-primary";
  return (
    <span className="inline-flex items-center gap-1 text-[10px] font-mono uppercase tracking-wider text-foreground">
      <span className={`size-1.5 rounded-full ${color} ${status !== "error" ? "animate-pulse" : ""}`} />
      <Loader2 className="size-2.5 animate-spin" /> {label}
    </span>
  );
}
