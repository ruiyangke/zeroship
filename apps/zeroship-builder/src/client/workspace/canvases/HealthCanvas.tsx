// ─── HealthCanvas — SRE agent's home (spec §9.9) ────────────────
//
// Four stacked sections inside one scrolling canvas:
//   1. Status pulse — Live/Draft chip from `app.deploy_hash`, last
//      deploy relative time, region (placeholder).
//   2. Quality scorecard — seven dimensions per spec §11.1, sourced
//      from the Critic stub via `getQualityScores({appId})`.
//   3. Incidents — empty state for V1 (no incidents table yet, see
//      ISSUES.md ISS-17).
//   4. Performance — defensive log-line parsing for request rate /
//      error rate / p95 latency. Polls `getAppLogs(appId)` every 5s
//      and keeps a 24-tick rolling history per metric. Hand-rolled
//      SVG sparkline (no chart lib). Structured metering still
//      pending (ISS-18 partial).

import { useEffect, useMemo, useRef, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import {
  getAppLogs,
  getQualityScores,
  sreMonitor,
  type AppRecord,
  type QualityDimension,
  type QualityGrade,
  type SREFindingItem,
} from "../../api";
import { GhostButton } from "../../components/GhostButton";

export interface HealthCanvasProps {
  appId: string;
  app?: AppRecord;
}

export function HealthCanvas({ appId, app }: HealthCanvasProps) {
  return (
    <div data-testid="health-canvas" className="h-full overflow-auto bg-paper">
      <div className="max-w-[860px] mx-auto px-4 sm:px-8 lg:px-12 py-6 sm:py-10">
        <StatusPulse app={app} />
        <div className="h-12" />
        <QualityScorecard appId={appId} />
        <div className="h-12" />
        <Incidents appId={appId} />
        <div className="h-12" />
        <Performance appId={appId} />
      </div>
    </div>
  );
}

// ─── 2a · Status pulse ──────────────────────────────────────────

function StatusPulse({ app }: { app?: AppRecord }) {
  const live = !!app?.deploy_hash;
  return (
    <section data-testid="health-status-section">
      <header className="mb-4">
        <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
          Status
        </h2>
        <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
          Is your project up? When did it last ship? Where does it run?
        </p>
      </header>
      <div
        className={
          "border px-6 py-5 " +
          (live ? "border-ivy/40 bg-ivy/5" : "border-rule-2 bg-paper-2/40")
        }
      >
        <div className="flex items-center gap-3">
          <span
            aria-hidden="true"
            className={
              "size-3 rounded-full inline-block " +
              (live ? "bg-ivy pulse-dot" : "bg-pencil")
            }
          />
          <span
            data-testid="health-status-label"
            className={
              "font-serif italic font-medium text-[22px] " +
              (live ? "text-ivy" : "text-ink-soft")
            }
          >
            {live ? "Live" : "Draft"}
          </span>
          <span className="font-sans text-[10px] uppercase tracking-[0.18em] text-ink-soft border border-rule rounded-full px-2 py-0.5 ml-auto">
            us-east-1
          </span>
        </div>
        <div className="mt-3 font-serif text-[14px] text-ink-soft">
          {live ? (
            <>
              Last deploy{" "}
              <span className="text-ink">
                {app ? relativeTime(app.updated_at) : "—"}
              </span>
              .
            </>
          ) : (
            <em>Nothing has shipped yet — once you deploy, this turns green.</em>
          )}
        </div>
      </div>
    </section>
  );
}

// ─── 2b · Quality scorecard ────────────────────────────────────

function QualityScorecard({ appId }: { appId: string }) {
  const { data, isLoading } = useQuery({
    queryKey: ["health-quality", appId],
    queryFn: () => getQualityScores({ appId }),
    retry: false,
  });

  return (
    <section data-testid="health-quality-section">
      <header className="mb-4">
        <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
          Quality
        </h2>
        <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
          Seven dimensions the Critic grades on every build. Updated
          live as Builder works — refresh after a chat turn to see the
          latest scorecard.
        </p>
      </header>
      {isLoading && (
        <div className="font-serif italic text-pencil py-2">loading…</div>
      )}
      {data && (
        <div className="grid gap-3" data-testid="health-quality-grid">
          <div
            className="grid items-center gap-4 px-4 py-3 border border-ink bg-paper-2"
            style={{ gridTemplateColumns: "120px 1fr" }}
          >
            <div className="label-uc">Overall</div>
            <div className="flex items-baseline gap-3">
              <GradeBadge grade={data.overall} />
              <span className="font-serif italic text-[13.5px] text-ink-soft">
                Composite of the seven dimensions below.
              </span>
            </div>
          </div>
          {data.dimensions.map((d) => (
            <DimensionRow key={d.key} dim={d} />
          ))}
          <div
            data-testid="health-quality-last-run"
            className="font-serif italic text-[12px] text-pencil text-right pt-1"
          >
            {data.last_run_at
              ? `Last graded ${relativeTime(data.last_run_at)}.`
              : "Not graded yet — a Critic round will set this."}
          </div>
        </div>
      )}
    </section>
  );
}

function DimensionRow({ dim }: { dim: QualityDimension }) {
  return (
    <div
      data-testid={`health-quality-row:${dim.key}`}
      className="grid items-center gap-4 px-4 py-3 border border-rule-2 bg-paper"
      style={{ gridTemplateColumns: "120px 1fr" }}
    >
      <div className="font-serif italic font-medium text-[15px] text-ink">
        {dim.label}
      </div>
      <div className="flex items-center gap-3">
        <GradeBadge grade={dim.grade} />
        <span className="font-serif text-[13.5px] text-ink-soft truncate">
          {dim.rationale}
        </span>
      </div>
    </div>
  );
}

function GradeBadge({ grade }: { grade: QualityGrade }) {
  const tone = gradeTone(grade);
  return (
    <span
      data-testid="health-quality-grade"
      className={
        "inline-flex items-center justify-center font-serif italic font-medium text-[18px] w-10 h-8 border " +
        tone
      }
    >
      {grade}
    </span>
  );
}

function gradeTone(grade: QualityGrade): string {
  if (grade.startsWith("A")) return "border-ivy/50 text-ivy bg-ivy/5";
  if (grade.startsWith("B")) return "border-ink/30 text-ink bg-paper-2";
  if (grade.startsWith("C")) return "border-tomato/40 text-tomato bg-tomato/5";
  return "border-tomato text-tomato bg-tomato/10";
}

// ─── 2c · Incidents ────────────────────────────────────────────

function Incidents({ appId }: { appId: string }) {
  // Findings live in component state — same shape as PlanCanvas's
  // digest panel. No persistence needed for the on-demand path; if a
  // page refresh wipes the result, the user can hit Scan again.
  const [findings, setFindings] = useState<SREFindingItem[] | null>(null);
  const scan = useMutation({
    mutationFn: () => sreMonitor({ appId }),
    onSuccess: (r) => setFindings(r.findings),
  });
  return (
    <section data-testid="health-incidents-section">
      <header className="flex items-baseline justify-between mb-4">
        <div>
          <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
            Incidents
          </h2>
          <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
            When something breaks, the SRE agent files a record here with
            a timeline, root cause, and fix.
          </p>
        </div>
        <GhostButton
          onClick={() => scan.mutate()}
          disabled={scan.isPending}
          data-testid="health-scan-issues"
        >
          {scan.isPending ? "scanning…" : findings ? "Scan again" : "Scan for issues"}
        </GhostButton>
      </header>
      {scan.error && (
        <div
          data-testid="health-scan-error"
          className="border border-tomato/40 bg-tomato/5 px-4 py-3 mb-3 font-serif italic text-[13.5px] text-tomato"
        >
          {(scan.error as Error).message || "Scan failed."}
        </div>
      )}
      {findings === null && !scan.isPending && !scan.error && (
        <div
          data-testid="health-incidents-empty"
          className="border border-rule-2 bg-paper-2/40 py-10 text-center"
        >
          <div className="font-serif italic text-[15px] text-ink-soft">
            All quiet — no incidents on record.
          </div>
          <div className="mt-1 font-serif italic text-[12px] text-pencil">
            Backing table tracked as ISS-17.
          </div>
        </div>
      )}
      {findings && findings.length === 0 && (
        <div
          data-testid="health-incidents-clear"
          className="border border-ivy/40 bg-ivy/5 py-8 text-center"
        >
          <div className="font-serif italic text-[15px] text-ivy">
            All quiet — the SRE agent didn't see anything concerning.
          </div>
        </div>
      )}
      {findings && findings.length > 0 && (
        <div className="grid gap-3" data-testid="health-incidents-list">
          {findings.map((f, i) => (
            <FindingCard key={i} finding={f} />
          ))}
        </div>
      )}
    </section>
  );
}

function FindingCard({ finding }: { finding: SREFindingItem }) {
  const tone =
    finding.severity === "critical"
      ? "border-tomato bg-tomato/10 text-tomato"
      : finding.severity === "error"
        ? "border-tomato/60 bg-tomato/5 text-tomato"
        : finding.severity === "warning"
          ? "border-ink/40 bg-paper-2 text-ink"
          : "border-rule-2 bg-paper-2/40 text-ink-soft";
  return (
    <div
      data-testid="health-finding-card"
      className={"border px-4 py-3 " + tone}
    >
      <div className="flex items-baseline justify-between mb-1.5">
        <span className="font-serif italic font-medium text-[16px]">
          {finding.diagnosis}
        </span>
        <span className="font-sans text-[10px] uppercase tracking-[0.18em] border border-current rounded-full px-2 py-0.5">
          {finding.severity}
        </span>
      </div>
      <div className="font-serif text-[13.5px] text-ink leading-[1.55]">
        {finding.recommendation}
      </div>
    </div>
  );
}

// ─── 2d · Performance ──────────────────────────────────────────
//
// Real perf signals derived from log lines (best-effort; the log
// format isn't strict). We poll `getAppLogs(appId)` every PERF_POLL_MS
// and on each tick:
//   - count HTTP method occurrences → "requests in window" (per-min
//     extrapolated from total lines)
//   - count error keywords / total lines → error rate %
//   - extract `<n>ms` numbers → p50 / p95
// The latest values feed three KPI tiles; a 24-tick rolling history
// per metric powers a hand-rolled SVG sparkline. When no logs (or no
// matching lines), tiles render the original "connect a deploy"
// placeholder per spec §26 voice.

const PERF_POLL_MS = 5_000;
const PERF_HISTORY = 24;
const HTTP_METHOD_RE = /\b(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)\b/i;
const ERROR_RE = /\b(error|exception|failed|panic|fatal)\b/i;
// Match "12ms", "12 ms", "1234ms" — but bound the digit run so we
// don't mistake a 9-digit timestamp for latency. Capture the number.
const LATENCY_RE = /\b(\d{1,5})\s?ms\b/gi;

interface PerfSnapshot {
  rpm: number;       // requests-per-minute (approx, from window)
  errorRate: number; // 0..100
  p50: number;       // ms (NaN if no samples)
  p95: number;       // ms (NaN if no samples)
  hasData: boolean;  // any signal at all this tick
}

function parsePerf(lines: string[]): PerfSnapshot {
  if (!lines || lines.length === 0) {
    return { rpm: 0, errorRate: 0, p50: NaN, p95: NaN, hasData: false };
  }
  let requests = 0;
  let errors = 0;
  const latencies: number[] = [];
  for (const line of lines) {
    if (HTTP_METHOD_RE.test(line)) requests += 1;
    if (ERROR_RE.test(line)) errors += 1;
    // Reset regex global state per line.
    LATENCY_RE.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = LATENCY_RE.exec(line)) !== null) {
      const n = Number(m[1]);
      // 0ms is a valid sample; cap at 60s to discard nonsense.
      if (Number.isFinite(n) && n >= 0 && n <= 60_000) latencies.push(n);
    }
  }
  // The control plane caps log slices, so "rpm" is really "requests
  // observed in the most recent slice". We label it accordingly in
  // the tile rather than over-claim a per-minute rate.
  const errorRate = lines.length > 0 ? (errors / lines.length) * 100 : 0;
  let p50 = NaN;
  let p95 = NaN;
  if (latencies.length > 0) {
    latencies.sort((a, b) => a - b);
    p50 = latencies[Math.floor(latencies.length * 0.5)];
    p95 = latencies[Math.floor(latencies.length * 0.95)] ?? latencies[latencies.length - 1];
  }
  const hasData = requests > 0 || errors > 0 || latencies.length > 0;
  return { rpm: requests, errorRate, p50, p95, hasData };
}

function Performance({ appId }: { appId: string }) {
  const { data: lines } = useQuery({
    queryKey: ["health-perf-logs", appId],
    // Tolerate 404 (no logs yet) and other transient failures — perf
    // tiles should degrade to placeholder, not blow up.
    queryFn: () => getAppLogs(appId).catch(() => [] as string[]),
    refetchInterval: PERF_POLL_MS,
    retry: false,
  });

  const snapshot = useMemo(() => parsePerf(lines ?? []), [lines]);

  // 24-tick rolling history per metric for the sparklines. We push on
  // every refetch (not every render) — guard with a ref to the last
  // log-array reference so re-renders from sibling state don't add
  // duplicate points.
  const lastSeenRef = useRef<string[] | null>(null);
  const [history, setHistory] = useState<{
    rpm: number[];
    errorRate: number[];
    p95: number[];
  }>({ rpm: [], errorRate: [], p95: [] });

  useEffect(() => {
    if (!lines) return;
    if (lines === lastSeenRef.current) return;
    lastSeenRef.current = lines;
    setHistory((h) => ({
      rpm: [...h.rpm, snapshot.rpm].slice(-PERF_HISTORY),
      errorRate: [...h.errorRate, snapshot.errorRate].slice(-PERF_HISTORY),
      // Render p95 series; substitute 0 for NaN so the polyline stays
      // valid. The tile's own value formatter handles NaN separately.
      p95: [...h.p95, Number.isFinite(snapshot.p95) ? snapshot.p95 : 0].slice(
        -PERF_HISTORY,
      ),
    }));
  }, [lines, snapshot.rpm, snapshot.errorRate, snapshot.p95]);

  return (
    <section data-testid="health-performance-section">
      <header className="mb-4">
        <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
          Performance
        </h2>
        <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
          Live latency, error rate, and request volume — derived from the
          log stream. Numbers refresh every 5s.
        </p>
      </header>
      <div className="grid gap-3 grid-cols-1 sm:grid-cols-3">
        <PerfTile
          label="p95 latency"
          testid="health-perf-latency"
          value={
            Number.isFinite(snapshot.p95) ? `${Math.round(snapshot.p95)}ms` : null
          }
          series={history.p95}
          empty={!snapshot.hasData}
        />
        <PerfTile
          label="error rate"
          testid="health-perf-errors"
          value={snapshot.hasData ? `${snapshot.errorRate.toFixed(1)}%` : null}
          series={history.errorRate}
          empty={!snapshot.hasData}
        />
        <PerfTile
          label="requests · window"
          testid="health-perf-rps"
          value={snapshot.hasData ? String(snapshot.rpm) : null}
          series={history.rpm}
          empty={!snapshot.hasData}
        />
      </div>
      <div className="mt-3 font-serif italic text-[12px] text-pencil">
        Best-effort signals from log text. Structured metering tracked as ISS-18.
      </div>
    </section>
  );
}

function PerfTile({
  label,
  testid,
  value,
  series,
  empty,
}: {
  label: string;
  testid: string;
  value: string | null;
  series: number[];
  empty: boolean;
}) {
  return (
    <div
      data-testid={testid}
      className={
        "border px-4 py-5 " +
        (empty ? "border-rule-2 bg-paper-2/40" : "border-ink/30 bg-paper")
      }
    >
      <div className="label-uc mb-2">{label}</div>
      <div
        className={
          "font-serif italic font-medium text-[28px] mb-1 " +
          (empty ? "text-pencil" : "text-ink")
        }
      >
        {value ?? "—"}
      </div>
      {empty ? (
        <div className="font-serif italic text-[12px] text-pencil leading-[1.4]">
          Connect a deploy to see live performance.
        </div>
      ) : (
        <Sparkline values={series} testid={`${testid}-spark`} />
      )}
    </div>
  );
}

// Hand-rolled SVG sparkline — no chart lib. Maps `values` linearly
// onto a 100×30 viewBox, polyline only. Single point renders as a
// flat line. Empty input renders nothing (caller already handled
// empty state).
function Sparkline({ values, testid }: { values: number[]; testid?: string }) {
  if (values.length === 0) return null;
  const min = Math.min(...values);
  const max = Math.max(...values);
  const span = max - min || 1;
  const stepX = values.length > 1 ? 100 / (values.length - 1) : 0;
  const points = values
    .map((v, i) => {
      const x = i * stepX;
      // Invert y so larger values sit higher.
      const y = 28 - ((v - min) / span) * 26;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg
      data-testid={testid}
      viewBox="0 0 100 30"
      preserveAspectRatio="none"
      className="w-full h-8 text-ink/60"
    >
      <polyline
        points={points}
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}

// ─── helpers ───────────────────────────────────────────────────

function relativeTime(iso: string): string {
  const t = new Date(iso).getTime();
  if (!Number.isFinite(t)) return "—";
  const diff = Date.now() - t;
  if (diff < 60_000) return "just now";
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}
