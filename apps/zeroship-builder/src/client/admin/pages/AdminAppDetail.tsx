// ─── Admin App Detail — three-pane forensic view ───────────────

import { useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { getApp, getAppLogs } from "../../api";
import { AdminShell } from "../AdminShell";
import { Kpi, StatusPill } from "../../components/Kpi";
import { LedgerRow, type LedgerLevel } from "../../components/LedgerRow";
import { GhostButton } from "../../components/GhostButton";

export function AdminAppDetail() {
  const { appId } = useParams<{ appId: string }>();
  const { data: app } = useQuery({
    queryKey: ["admin", "app", appId],
    queryFn: () => getApp(appId!),
    enabled: !!appId,
  });
  const { data: logs } = useQuery({
    queryKey: ["admin", "app-logs", appId],
    queryFn: () => getAppLogs(appId!),
    enabled: !!appId,
  });

  return (
    <AdminShell
      pageLabel="app detail"
      crumb={[{ label: "apps", to: "/admin/apps" }, { label: app?.name ?? "…" }]}
    >
      <div className="grid gap-0" style={{ gridTemplateColumns: "200px 1fr 240px" }}>
        <aside className="border-r border-rule pr-4 py-2">
          <div className="label-uc mb-3">App pages</div>
          {[
            ["Overview", true],
            ["Deploys", false],
            ["Logs", false],
            ["Audit", false],
            ["Env", false],
            ["Billing", false],
          ].map(([label, active]: any) => (
            <a
              key={label}
              href="#"
              className={
                "block px-2.5 py-1.5 font-serif " +
                (active
                  ? "text-ink bg-paper-2 border-l-2 border-tomato pl-3 -ml-1 font-medium"
                  : "italic text-ink-soft hover:text-ink")
              }
              style={{ textDecoration: "none" }}
            >
              {label}
            </a>
          ))}
        </aside>

        <div className="px-7 py-2">
          <h1 className="font-serif font-medium text-[36px] -tracking-[0.015em] mb-1">
            {app?.name ?? "…"}
          </h1>
          <p className="font-serif italic text-[14.5px] text-ink-soft mb-7">
            App detail · admin forensic view
          </p>

          <div className="grid gap-4 mb-7" style={{ gridTemplateColumns: "repeat(4, 1fr)" }}>
            <Kpi label="Status" value={<span className="text-tomato">{app?.deploy_hash ? "live" : "draft"}</span>} />
            <Kpi label="Plan" value={app?.plan_id ?? "—"} />
            <Kpi label="Deploys" value={app?.deploy_hash ? "≥1" : "0"} />
            <Kpi label="Updated" value={app ? fmtAgo(app.updated_at) : "—"} />
          </div>

          <h3 className="font-serif italic font-medium text-[22px] mb-2">Recent log events</h3>
          <hr className="hairline" />
          <div className="mt-3">
            {(logs ?? []).slice(0, 20).map((line, i) => (
              <LedgerRow
                key={i}
                num={i + 1}
                timestamp="—"
                level={detectLevel(line)}
                message={line}
              />
            ))}
            {(!logs || logs.length === 0) && (
              <div className="font-serif italic text-pencil py-3">No log events yet.</div>
            )}
          </div>
        </div>

        <aside className="border-l border-rule px-4 py-2">
          <div className="label-uc mb-1.5">Identifiers</div>
          <dl className="m-0 space-y-3">
            <Row label="App ID" value={app?.id} />
            <Row label="Bundle hash" value={app?.deploy_hash ?? "—"} />
            <Row label="Created" value={fmtTs(app?.created_at)} />
            <Row label="Updated" value={fmtTs(app?.updated_at)} />
          </dl>
          <div className="mt-7 flex flex-col gap-2 items-stretch">
            <GhostButton>Force redeploy</GhostButton>
            <GhostButton danger>Suspend</GhostButton>
          </div>
        </aside>
      </div>
    </AdminShell>
  );
}

function Row({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div>
      <dt className="font-sans text-[9.5px] uppercase tracking-[0.16em] text-pencil mb-0.5">{label}</dt>
      <dd className="m-0 font-mono text-[11px] text-ink break-all">{value ?? "—"}</dd>
    </div>
  );
}
function fmtTs(s?: string): string {
  if (!s) return "—";
  try { return new Date(s).toISOString().slice(0, 19).replace("T", " "); } catch { return s; }
}
function detectLevel(raw: string): LedgerLevel {
  const lower = raw.toLowerCase();
  if (/\b(error|err|fail|failed|exception|panic)\b/.test(lower)) return "error";
  if (/\b(warn|warning|deprecated)\b/.test(lower)) return "warn";
  return "info";
}
function fmtAgo(s?: string): string {
  if (!s) return "—";
  try {
    const sec = Math.floor((Date.now() - new Date(s).getTime()) / 1000);
    if (sec < 60) return "just now";
    if (sec < 3600) return `${Math.floor(sec / 60)} min`;
    return `${Math.floor(sec / 3600)} h`;
  } catch { return s; }
}
