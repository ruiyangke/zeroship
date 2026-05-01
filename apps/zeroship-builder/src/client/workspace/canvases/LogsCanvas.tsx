// ─── LogsCanvas — the ledger (spec §9.5) ────────────────────────
//
// Header: filter pills + search box + auto-scroll toggle.
// Body: virtualized-ish list of log lines (timestamp · level · msg).
// Polls getAppLogs(appId) every 2s. Sticky-bottom unless the user
// has scrolled away from the bottom.

import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { getAppLogs } from "../../api";
import { FilterPill } from "../../components/FilterPill";

export interface LogsCanvasProps {
  appId: string;
}

type Filter = "all" | "info" | "warn" | "error" | "request";
type Level = Exclude<Filter, "all">;

interface ParsedLine {
  raw: string;
  level: Level;
  ts: string;
  message: string;
}

const POLL_MS = 2000;
const STICKY_THRESHOLD_PX = 60;

export function LogsCanvas({ appId }: LogsCanvasProps) {
  const [filter, setFilter] = useState<Filter>("all");
  const [search, setSearch] = useState("");
  const [autoScroll, setAutoScroll] = useState(true);

  const { data: lines, error } = useQuery({
    queryKey: ["app-logs", appId],
    // The control plane returns 404 when the app exists but has no
    // logs yet; treat that as an empty list rather than a failure.
    queryFn: () => getAppLogs(appId).catch(() => [] as string[]),
    refetchInterval: POLL_MS,
    retry: false,
  });

  const visible = useMemo(() => {
    const parsed = (lines ?? []).map(parseLine);
    const search_l = search.trim().toLowerCase();
    return parsed.filter((p) => {
      if (filter !== "all" && p.level !== filter) return false;
      if (search_l && !p.raw.toLowerCase().includes(search_l)) return false;
      return true;
    });
  }, [lines, filter, search]);

  // Sticky-bottom scroll behaviour mirrors ChatMessages.tsx — the
  // ref tracks whether the user is "near the bottom"; when true we
  // pin to bottom on every refetch, when false we leave the
  // scroll position alone so a user reading old logs isn't yanked
  // back down.
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickRef = useRef(true);

  useEffect(() => {
    if (!autoScroll || !stickRef.current) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [visible, autoScroll]);

  function onScroll() {
    const el = scrollRef.current;
    if (!el) return;
    const dist = el.scrollHeight - el.clientHeight - el.scrollTop;
    stickRef.current = dist < STICKY_THRESHOLD_PX;
  }

  return (
    <div
      data-testid="logs-canvas"
      className="h-full flex flex-col bg-paper"
    >
      <div className="px-8 pt-6 pb-3 border-b border-rule flex flex-wrap items-center gap-3">
        <div className="flex items-center gap-2">
          <FilterPill active={filter === "all"} onClick={() => setFilter("all")}>
            all
          </FilterPill>
          <FilterPill active={filter === "error"} onClick={() => setFilter("error")}>
            error
          </FilterPill>
          <FilterPill active={filter === "warn"} onClick={() => setFilter("warn")}>
            warn
          </FilterPill>
          <FilterPill active={filter === "info"} onClick={() => setFilter("info")}>
            info
          </FilterPill>
          <FilterPill active={filter === "request"} onClick={() => setFilter("request")}>
            request
          </FilterPill>
        </div>
        <input
          data-testid="logs-search"
          placeholder="search…"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          className="ml-auto px-2.5 py-1 border border-rule bg-white font-mono text-[12px] outline-none focus:border-ink min-w-[180px]"
        />
        <label className="flex items-center gap-1.5 font-serif italic text-[12.5px] text-ink-soft cursor-pointer">
          <input
            type="checkbox"
            checked={autoScroll}
            onChange={(e) => setAutoScroll(e.target.checked)}
            data-testid="logs-autoscroll"
          />
          follow
        </label>
      </div>

      <div
        ref={scrollRef}
        onScroll={onScroll}
        className="flex-1 overflow-auto px-8 py-4 min-h-0"
      >
        {error && (
          <div className="font-serif italic text-tomato">
            couldn't load logs
          </div>
        )}
        {!error && visible.length === 0 && (
          <div className="font-serif italic text-pencil">
            No logs yet — give it a deploy first.
          </div>
        )}
        <div data-testid="logs-list">
          {visible.map((line, i) => (
            <LogLine key={i} line={line} />
          ))}
        </div>
      </div>
    </div>
  );
}

function LogLine({ line }: { line: ParsedLine }) {
  const tone = toneFor(line.level);
  return (
    <div
      className="grid items-baseline gap-3 py-1 border-b border-rule-2 font-mono text-[12px]"
      style={{ gridTemplateColumns: "100px 70px 1fr" }}
    >
      <span className="text-ink-soft">{line.ts}</span>
      <span
        className={
          "font-sans text-[9.5px] uppercase tracking-[0.18em] self-center " +
          tone
        }
      >
        {line.level}
      </span>
      <span className="text-ink whitespace-pre-wrap break-words">
        {line.message}
      </span>
    </div>
  );
}

function toneFor(l: Level): string {
  switch (l) {
    case "error":   return "text-tomato font-bold";
    case "warn":    return "text-tomato font-semibold";
    case "request": return "text-ink-soft";
    default:        return "text-ink-soft";
  }
}

function parseLine(raw: string): ParsedLine {
  // The control plane returns log lines as opaque strings; we do a
  // best-effort parse to surface a level and a timestamp prefix.
  // ISO-8601 prefix? "2026-04-13T12:34:56Z foo bar"
  const tsMatch = raw.match(
    /^(\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?Z?)\s+(.*)$/,
  );
  const ts = tsMatch ? formatTs(tsMatch[1]!) : "—";
  const message = tsMatch ? tsMatch[2]! : raw;
  const lower = raw.toLowerCase();
  let level: Level = "info";
  if (/\b(error|err|fail|failed|exception|panic|fatal)\b/.test(lower)) level = "error";
  else if (/\b(warn|warning|deprecated)\b/.test(lower)) level = "warn";
  else if (/\b(get|post|put|delete|patch)\s+\/\S+|http\/[12]/.test(lower)) level = "request";
  return { raw, level, ts, message };
}

function formatTs(iso: string): string {
  // Compact HH:MM:SS suffix — full ISO timestamps are too wide for
  // the 100px column.
  const m = iso.match(/(\d{2}:\d{2}:\d{2})/);
  return m ? m[1]! : iso;
}
