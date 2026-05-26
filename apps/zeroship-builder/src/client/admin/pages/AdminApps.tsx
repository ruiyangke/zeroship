// ─── Admin Apps — directory of every app ───────────────────────

import { useState } from "react";
import { Link } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { listApps } from "../../api";
import { AdminShell } from "../AdminShell";
import { FilterPill } from "../../components/FilterPill";
import { StatusPill } from "../../components/Kpi";

type Filter = "all" | "live" | "draft";

export function AdminApps() {
  const { data: apps } = useQuery({ queryKey: ["admin", "apps"], queryFn: () => listApps() });
  const [filter, setFilter] = useState<Filter>("all");
  const [q, setQ] = useState("");

  const filtered = (apps ?? []).filter((a) => {
    if (filter === "live"  && !a.deploy_hash) return false;
    if (filter === "draft" && a.deploy_hash) return false;
    if (q && !a.name.toLowerCase().includes(q.toLowerCase()) && !a.id.toLowerCase().includes(q.toLowerCase())) return false;
    return true;
  });

  return (
    <AdminShell pageLabel="apps · directory">
      <h1 className="font-serif font-medium text-[40px] leading-[1.0] -tracking-[0.02em] mb-2">
        [Admin] <em className="italic text-tomato">Apps</em>.
      </h1>
      <p className="font-serif italic text-[15px] text-ink-soft mb-6">
        Every app on the platform — searchable, filterable, openable.
      </p>

      <div className="flex flex-wrap items-center gap-2.5 mb-5">
        <FilterPill active={filter === "all"} onClick={() => setFilter("all")}>all · {apps?.length ?? 0}</FilterPill>
        <FilterPill active={filter === "live"} onClick={() => setFilter("live")}>
          live · {(apps ?? []).filter((a) => !!a.deploy_hash).length}
        </FilterPill>
        <FilterPill active={filter === "draft"} onClick={() => setFilter("draft")}>
          draft · {(apps ?? []).filter((a) => !a.deploy_hash).length}
        </FilterPill>
        <input
          aria-label="Search apps"
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder="Search by name or ID…"
          className="ml-auto px-3 py-1.5 border border-rule bg-white font-serif text-[13px] min-w-[260px] outline-none focus:border-ink"
        />
      </div>

      <table className="w-full font-serif text-[14.5px]">
        <thead>
          <tr>
            <Th width="60px">№</Th>
            <Th>Name</Th>
            <Th>Plan</Th>
            <Th>Status</Th>
            <Th>Last update</Th>
            <Th></Th>
          </tr>
        </thead>
        <tbody>
          {filtered.map((a, i) => (
            <tr key={a.id} className="hover:bg-paper-2">
              <Td><span className="text-tomato italic" style={{ fontFeatureSettings: '"lnum" 1' }}>{i + 1}</span></Td>
              <Td>
                <span className="font-serif font-medium">{a.name}</span>
                <br />
                <span className="font-mono text-[11px] text-pencil">{a.id.slice(0, 18)}…</span>
              </Td>
              <Td>{a.plan_id}</Td>
              <Td><StatusPill status={a.deploy_hash ? "live" : "draft"} /></Td>
              <Td>{fmtAgo(a.updated_at)}</Td>
              <Td>
                <Link to={`/admin/apps/${a.id}`} className="text-tomato italic font-serif" style={{ textDecoration: "none" }}>
                  open →
                </Link>
              </Td>
            </tr>
          ))}
          {filtered.length === 0 && (
            <tr><td colSpan={6} className="py-6 font-serif italic text-pencil">No apps match.</td></tr>
          )}
        </tbody>
      </table>

      <div className="mt-4 font-serif italic text-ink-soft text-[13px] flex justify-between">
        <span>Showing {filtered.length} of {apps?.length ?? 0}</span>
      </div>
    </AdminShell>
  );
}

function Th({ children, width }: { children?: React.ReactNode; width?: string }) {
  return (
    <th
      className="text-left px-3 py-2.5 font-sans text-[10px] uppercase tracking-[0.18em] text-pencil font-semibold border-b border-rule"
      style={{ width }}
    >
      {children}
    </th>
  );
}
function Td({ children }: { children?: React.ReactNode }) {
  return <td className="px-3 py-3 border-b border-rule-2 align-baseline">{children}</td>;
}

function fmtAgo(s?: string): string {
  if (!s) return "—";
  try {
    const d = new Date(s);
    const sec = Math.floor((Date.now() - d.getTime()) / 1000);
    if (sec < 60) return "just now";
    if (sec < 3600) return `${Math.floor(sec / 60)} min`;
    if (sec < 86400) return `${Math.floor(sec / 3600)} h`;
    if (sec < 604800) return `${Math.floor(sec / 86400)} d`;
    return d.toISOString().slice(0, 10);
  } catch { return s; }
}
