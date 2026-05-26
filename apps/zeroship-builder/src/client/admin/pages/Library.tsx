// ─── Admin Library — overview ───────────────────────────────────

import { useQuery } from "@tanstack/react-query";
import { listApps } from "../../api";
import { AdminShell } from "../AdminShell";
import { Kpi } from "../../components/Kpi";
import { LedgerRow } from "../../components/LedgerRow";

export function AdminLibrary() {
  const { data: apps } = useQuery({
    queryKey: ["admin", "apps"],
    queryFn: () => listApps(),
  });
  const live = (apps ?? []).filter((a) => !!a.deploy_hash).length;

  return (
    <AdminShell pageLabel="overview">
      <header className="mb-8 reveal">
        <h1 className="font-serif font-medium text-[44px] leading-[0.98] -tracking-[0.02em]">
          The <em className="italic text-tomato">library</em>.
        </h1>
        <p className="font-serif italic text-[16px] text-ink-soft mt-1">
          Where every project is catalogued.
        </p>
      </header>

      <div className="grid gap-5 mb-10" style={{ gridTemplateColumns: "repeat(4, 1fr)" }}>
        <Kpi label="Apps"          value={fmtNum(apps?.length)} delta={`${live} live`} />
        <Kpi label="Users"         value={"—"} delta="placeholder" flat />
        <Kpi label="MRR"           value={"—"} delta="placeholder" flat />
        <Kpi label="Platform fee"  value={"—"} delta="today" flat />
      </div>

      <div className="grid gap-12" style={{ gridTemplateColumns: "2fr 1fr" }}>
        <section>
          <h3 className="font-serif italic font-medium text-[22px] mb-2">Recent activity</h3>
          <hr className="hairline" />
          <div className="mt-3">
            {(apps ?? []).slice(0, 8).map((a, i) => (
              <LedgerRow
                key={a.id}
                num={(apps?.length ?? 0) - i}
                timestamp={fmtTs(a.updated_at)}
                level={a.deploy_hash ? "deploy" : "draft" as any}
                message={
                  <>
                    <em className="italic">{a.name}</em>
                    {" "}{a.deploy_hash ? "shipped" : "saved as draft"}
                  </>
                }
              />
            ))}
            {(!apps || apps.length === 0) && (
              <div className="font-serif italic text-pencil py-3">No activity yet.</div>
            )}
          </div>
        </section>
        <section>
          <h3 className="font-serif italic font-medium text-[22px] mb-2">System pulse</h3>
          <hr className="hairline" />
          <ul className="m-0 p-0 list-none mt-3 font-serif text-[14.5px] leading-[1.9]">
            <PulseRow label="workers (4)" status="up" />
            <PulseRow label="control plane" status="up" />
            <PulseRow label="gateway" status="up" />
            <PulseRow label="postgres pool" status="up" />
            <PulseRow label="stripe webhooks" status="up" />
            <PulseRow label="sandbox docker" status="deg" />
          </ul>
        </section>
      </div>
    </AdminShell>
  );
}

function PulseRow({ label, status }: { label: string; status: "up" | "deg" | "down" }) {
  const color = status === "up" ? "var(--color-tomato)" : status === "deg" ? "var(--color-tomato-2)" : "var(--color-tomato)";
  const label2 = status === "up" ? "● up" : status === "deg" ? "● degraded" : "● down";
  return (
    <li className="flex items-baseline justify-between border-b border-rule-2 py-1.5">
      <span className="italic">{label}</span>
      <span className="font-sans text-[10px] uppercase tracking-[0.16em]" style={{ color }}>{label2}</span>
    </li>
  );
}

function fmtNum(n: number | undefined): string {
  if (n == null) return "—";
  return new Intl.NumberFormat("en-US").format(n);
}

function fmtTs(ts?: string): string {
  if (!ts) return "—";
  try { return new Date(ts).toISOString().slice(11, 19); } catch { return ts; }
}
