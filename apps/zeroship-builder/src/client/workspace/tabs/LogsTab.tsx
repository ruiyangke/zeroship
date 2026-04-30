// ─── LogsTab — the ledger ───────────────────────────────────────

import { useState, useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { getAppLogs } from "../../api";
import { useWorkspace } from "../ProjectWorkspace";
import { TabDrawer } from "../components/TabDrawer";
import { FilterPill } from "../../components/FilterPill";
import { LedgerRow, LedgerCode, type LedgerLevel } from "../../components/LedgerRow";

type Filter = "all" | "info" | "warn" | "error";

export function LogsTab() {
  const { appId } = useWorkspace();
  const [filter, setFilter] = useState<Filter>("all");

  const { data: lines, isLoading, error } = useQuery({
    queryKey: ["logs", appId],
    queryFn: () => getAppLogs(appId).catch(() => [] as string[]),
    refetchInterval: 4000,
  });

  const visible = useMemo(() => {
    const all = (lines ?? []).map(parseLine);
    return filter === "all" ? all : all.filter((l) => l.level === filter);
  }, [lines, filter]);

  return (
    <div className="h-full flex flex-col" data-testid="logs-tab">
      <div className="flex-1 px-10 pt-8 pb-0 overflow-auto">
        <div className="flex items-baseline gap-2.5 mb-6">
          <FilterPill active={filter === "all"} onClick={() => setFilter("all")}>all</FilterPill>
          <FilterPill active={filter === "info"} onClick={() => setFilter("info")}>info</FilterPill>
          <FilterPill active={filter === "warn"} onClick={() => setFilter("warn")}>warn</FilterPill>
          <FilterPill active={filter === "error"} onClick={() => setFilter("error")}>error</FilterPill>
          <span className="ml-auto font-serif italic text-ink-soft text-[13px]">
            today · last {visible.length} events
          </span>
        </div>

        {isLoading && <div className="font-serif italic text-pencil">loading…</div>}
        {error && <div className="font-serif italic text-tomato">couldn't load logs</div>}
        {!isLoading && visible.length === 0 && (
          <div className="font-serif italic text-pencil">No events yet — once your app runs, they'll appear here.</div>
        )}

        <div>
          {visible.map((log, i) => (
            <LedgerRow
              key={i}
              num={i + 1}
              timestamp="—"
              level={log.level}
              message={renderMessage(log.message)}
            />
          ))}
        </div>
      </div>
      <TabDrawer appId={appId} />
    </div>
  );
}

interface ParsedLine {
  level: LedgerLevel;
  message: string;
}

function parseLine(raw: string): ParsedLine {
  const lower = raw.toLowerCase();
  if (/\b(error|err|fail|failed|exception|panic)\b/.test(lower)) return { level: "error", message: raw };
  if (/\b(warn|warning|deprecated)\b/.test(lower)) return { level: "warn", message: raw };
  return { level: "info", message: raw };
}

function renderMessage(msg: string): React.ReactNode {
  const parts = msg.split(/(`[^`]+`)/g);
  return parts.map((p, i) =>
    p.startsWith("`") && p.endsWith("`")
      ? <LedgerCode key={i}>{p.slice(1, -1)}</LedgerCode>
      : <span key={i}>{p}</span>
  );
}
