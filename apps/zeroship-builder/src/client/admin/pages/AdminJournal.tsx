// ─── Admin Journal — cross-app log stream ──────────────────────

import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { listApps, getAppLogs } from "../../api";
import { AdminShell } from "../AdminShell";
import { FilterPill } from "../../components/FilterPill";
import { LedgerRow, type LedgerLevel } from "../../components/LedgerRow";

type Filter = "all" | "info" | "warn" | "error";

interface JoinedLog {
  appId: string;
  appName: string;
  level: LedgerLevel;
  message: string;
}

export function AdminJournal() {
  const [filter, setFilter] = useState<Filter>("all");
  const { data: apps } = useQuery({ queryKey: ["admin", "apps"], queryFn: () => listApps() });

  const queries = useQuery({
    queryKey: ["admin", "journal", apps?.map((a) => a.id) ?? []],
    enabled: !!apps && apps.length > 0,
    queryFn: async (): Promise<JoinedLog[]> => {
      const all = await Promise.all(
        (apps ?? []).slice(0, 8).map(async (a) => {
          try {
            const lines = await getAppLogs(a.id);
            return lines.map((line) => ({
              appId: a.id,
              appName: a.name,
              level: detectLevel(line),
              message: line,
            }));
          } catch {
            return [];
          }
        })
      );
      return all.flat();
    },
  });

  const visible = (queries.data ?? []).filter((l) =>
    filter === "all" ? true : l.level === filter,
  );

  return (
    <AdminShell pageLabel="system journal">
      <h1 className="font-serif font-medium text-[40px] leading-[1.0] -tracking-[0.02em] mb-2">
        [Admin] <em className="italic text-tomato">System journal</em>.
      </h1>
      <p className="font-serif italic text-[15px] text-ink-soft mb-6">
        Cross-app log stream. Filter by level, app, request id.
      </p>

      <div className="flex flex-wrap items-center gap-2.5 mb-5">
        <FilterPill active={filter === "all"} onClick={() => setFilter("all")}>all</FilterPill>
        <FilterPill active={filter === "info"} onClick={() => setFilter("info")}>info</FilterPill>
        <FilterPill active={filter === "warn"} onClick={() => setFilter("warn")}>warn</FilterPill>
        <FilterPill active={filter === "error"} onClick={() => setFilter("error")}>error</FilterPill>
        <input
          aria-label="Filter journal entries"
          placeholder="Filter by app, request id…"
          className="ml-auto px-3 py-1.5 border border-rule bg-white font-serif text-[13px] min-w-[260px] outline-none focus:border-ink"
        />
      </div>

      <div>
        {visible.slice(0, 80).map((l, i) => (
          <LedgerRow
            key={i}
            num={i + 1}
            timestamp="—"
            level={l.level}
            app={<em className="italic">{l.appName}</em>}
            message={l.message}
          />
        ))}
        {visible.length === 0 && (
          <div className="font-serif italic text-pencil py-6">No events yet.</div>
        )}
      </div>
    </AdminShell>
  );
}

function detectLevel(raw: string): LedgerLevel {
  const lower = raw.toLowerCase();
  if (/\b(error|err|fail|failed|exception|panic)\b/.test(lower)) return "error";
  if (/\b(warn|warning|deprecated)\b/.test(lower)) return "warn";
  return "info";
}
