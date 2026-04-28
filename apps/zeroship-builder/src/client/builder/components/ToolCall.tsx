// ─── ToolCall ────────────────────────────────────────────────────
// Collapsible card showing one tool invocation: name, status,
// inputs, and the truncated output. Click to expand.

import { useState } from "react";
import { ChevronRight, Loader2, CheckCircle2, XCircle, Wrench } from "lucide-react";
import type { ToolEvent } from "../types";

const OUTPUT_PREVIEW = 240;

interface Props {
  tool: ToolEvent;
}

export function ToolCall({ tool }: Props) {
  const [open, setOpen] = useState(false);
  const status = tool.error ? "error" : tool.done ? "done" : "running";

  const Icon = status === "running" ? Loader2 : status === "error" ? XCircle : CheckCircle2;
  const iconClass =
    status === "running"
      ? "animate-spin text-muted-foreground"
      : status === "error"
        ? "text-destructive"
        : "text-emerald-500";

  return (
    <div className="my-1 border border-border/50 bg-muted/30 text-xs font-mono">
      <button
        type="button"
        onClick={() => setOpen(!open)}
        className="flex items-center w-full px-2 py-1.5 gap-2 hover:bg-muted/60 transition-colors text-left bg-transparent border-none cursor-pointer"
      >
        <ChevronRight
          className={`size-3 transition-transform ${open ? "rotate-90" : ""}`}
        />
        <Wrench className="size-3 text-muted-foreground shrink-0" />
        <span className="flex-1 truncate text-foreground">{tool.name}</span>
        <Icon className={`size-3 shrink-0 ${iconClass}`} />
      </button>

      {open && (
        <div className="px-2 pb-2 pt-1 border-t border-border/40 space-y-1.5">
          {tool.input !== undefined && (
            <div>
              <div className="text-[10px] uppercase tracking-wider text-muted-foreground mb-0.5">
                input
              </div>
              <pre className="text-[10.5px] whitespace-pre-wrap break-all max-h-40 overflow-auto bg-background/60 px-1.5 py-1 border border-border/30">
                {safeStringify(tool.input)}
              </pre>
            </div>
          )}
          {tool.output !== undefined && (
            <div>
              <div className="text-[10px] uppercase tracking-wider text-muted-foreground mb-0.5">
                output
              </div>
              <pre className="text-[10.5px] whitespace-pre-wrap break-all max-h-60 overflow-auto bg-background/60 px-1.5 py-1 border border-border/30">
                {tool.output.length > OUTPUT_PREVIEW * 4
                  ? tool.output.slice(0, OUTPUT_PREVIEW * 4) + "\n…"
                  : tool.output}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function safeStringify(v: unknown): string {
  if (typeof v === "string") return v;
  try { return JSON.stringify(v, null, 2); } catch { return String(v); }
}
