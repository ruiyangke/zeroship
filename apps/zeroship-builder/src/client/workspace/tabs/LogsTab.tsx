// ─── LogsTab — live tail ────────────────────────────────────────
// Polls /api/apps/:id/logs every 2s and renders the most recent
// lines with a sticky-bottom auto-scroll the user can detach by
// scrolling up.

import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef } from "react";
import { getAppLogs } from "../../api";
import { useWorkspace } from "../ProjectWorkspace";
import { Loader2, ScrollText } from "lucide-react";

export function LogsTab() {
  const { appId } = useWorkspace();
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickRef = useRef(true);

  const { data: lines, isLoading, error } = useQuery({
    queryKey: ["app-logs", appId],
    queryFn: () => getAppLogs(appId).catch(() => [] as string[]),
    refetchInterval: 2000,
  });

  useEffect(() => {
    if (!stickRef.current) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lines]);

  function onScroll() {
    const el = scrollRef.current;
    if (!el) return;
    const distance = el.scrollHeight - el.clientHeight - el.scrollTop;
    stickRef.current = distance < 60;
  }

  return (
    <div data-testid="logs-tab" className="h-full flex flex-col">
      <div className="border-b border-border bg-muted/30 px-3 py-1.5 flex items-center gap-2 text-xs font-mono text-muted-foreground">
        <ScrollText className="size-3" />
        logs
        {isLoading && <Loader2 className="size-3 animate-spin ml-1" />}
        <span className="ml-auto text-muted-foreground/60">
          {lines?.length ?? 0} lines · polled every 2s
        </span>
      </div>
      <div
        ref={scrollRef}
        onScroll={onScroll}
        className="flex-1 overflow-auto bg-background text-[12px] font-mono leading-snug px-3 py-2"
      >
        {error ? (
          <div className="text-destructive">{(error as Error).message}</div>
        ) : !lines || lines.length === 0 ? (
          <div className="text-muted-foreground">
            no logs yet — make a request to the deployed app to see something here.
          </div>
        ) : (
          lines.map((line, i) => (
            <div key={i} className="whitespace-pre-wrap break-all">
              {line}
            </div>
          ))
        )}
      </div>
    </div>
  );
}
