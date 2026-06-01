// ─── LogsCanvas — the ledger (`docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §9.5) ────────────────────────
//
// Header: level filter (segmented Toggle.Group) + search box + follow
// (Switch) auto-scroll toggle.
// Body: a Card-framed, app-local log stream — a list of log lines
// (timestamp · level · msg) rendered in the monospace face.
// Polls getLogs(appId) every 2s — it tails the sandbox's
// `.zeroship/dev.log` (the preview-start command redirects the
// dev-server stdout/stderr there). Sticky-bottom unless the user has
// scrolled away from the bottom.

import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import {
  Banner,
  Card,
  Cluster,
  EmptyState,
  Input,
  Switch,
  Toggle,
} from "@zeroship/ui";
import { getLogs } from "../../api";
import "./LogsCanvas.css";

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

const FILTERS: Filter[] = ["all", "error", "warn", "info", "request"];

export function LogsCanvas({ appId }: LogsCanvasProps) {
  const [filter, setFilter] = useState<Filter>("all");
  const [search, setSearch] = useState("");
  const [autoScroll, setAutoScroll] = useState(true);

  const { data: lines, error } = useQuery({
    queryKey: ["app-logs", appId],
    queryFn: () => getLogs(appId),
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
    <div data-testid="logs-canvas" className="logs-canvas">
      <Cluster
        className="logs-canvas__bar"
        gap={3}
        align="center"
        justify="start"
      >
        <Toggle.Group
          aria-label="Filter logs by level"
          value={filter}
          onValueChange={(next) => setFilter(next ?? "all")}
          size="sm"
        >
          {FILTERS.map((f) => (
            <Toggle key={f} value={f}>
              {f}
            </Toggle>
          ))}
        </Toggle.Group>

        <Input
          type="search"
          data-testid="logs-search"
          aria-label="Search logs"
          placeholder="search…"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          size="sm"
          className="logs-canvas__search"
        />

        <Switch
          checked={autoScroll}
          onCheckedChange={(checked) => setAutoScroll(checked)}
          data-testid="logs-autoscroll"
          label="follow"
          size="sm"
          fieldClassName="logs-canvas__follow"
        />
      </Cluster>

      <div className="logs-canvas__body">
        <Card variant="outline" className="logs-canvas__frame">
          <div
            ref={scrollRef}
            onScroll={onScroll}
            className="logs-canvas__scroll"
          >
            {error && (
              <Banner
                intent="danger"
                title="Couldn't load logs"
                className="logs-canvas__error"
              />
            )}
            {!error && visible.length === 0 && (
              <div data-testid="logs-empty" className="logs-canvas__empty">
                <EmptyState
                  title="Quiet on this front"
                  description={
                    search || filter !== "all"
                      ? "No lines match that filter."
                      : "Open the preview to start the dev server."
                  }
                />
              </div>
            )}
            <div data-testid="logs-list" className="logs-canvas__list">
              {visible.map((line, i) => (
                <LogLine key={i} line={line} />
              ))}
            </div>
          </div>
        </Card>
      </div>
    </div>
  );
}

function LogLine({ line }: { line: ParsedLine }) {
  return (
    <div className="logs-line" data-level={line.level}>
      <span className="logs-line__ts">{line.ts}</span>
      <span className="logs-line__level">{line.level}</span>
      <span className="logs-line__msg">{line.message}</span>
    </div>
  );
}

function parseLine(raw: string): ParsedLine {
  // The dev-server log is opaque text; we do a best-effort parse to
  // surface a level and a timestamp prefix.
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
  // the timestamp column.
  const m = iso.match(/(\d{2}:\d{2}:\d{2})/);
  return m ? m[1]! : iso;
}
