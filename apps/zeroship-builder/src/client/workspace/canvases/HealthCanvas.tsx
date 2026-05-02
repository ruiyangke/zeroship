// ─── HealthCanvas — SRE agent's home (spec §9.9) ────────────────
//
// Four stacked sections inside one scrolling canvas:
//   1. Status pulse — Live/Draft chip from `app.deploy_hash`, last
//      deploy relative time, region (placeholder).
//   2. Quality scorecard — seven dimensions per spec §11.1, sourced
//      from the Critic stub via `getQualityScores({appId})`.
//   3. Incidents — empty state for V1 (no incidents table yet, see
//      ISSUES.md ISS-17).
//   4. Performance — placeholder boxes (no metering yet, ISS-18).

import { useQuery } from "@tanstack/react-query";
import {
  getQualityScores,
  type AppRecord,
  type QualityDimension,
  type QualityGrade,
} from "../../api";

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
        <Incidents />
        <div className="h-12" />
        <Performance />
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
          Seven dimensions the Critic grades on every build. The current
          snapshot is a stub — wiring lands with ISS-16.
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

function Incidents() {
  return (
    <section data-testid="health-incidents-section">
      <header className="mb-4">
        <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
          Incidents
        </h2>
        <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
          When something breaks, the SRE agent files a record here with
          a timeline, root cause, and fix.
        </p>
      </header>
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
    </section>
  );
}

// ─── 2d · Performance ──────────────────────────────────────────

function Performance() {
  return (
    <section data-testid="health-performance-section">
      <header className="mb-4">
        <h2 className="font-serif italic font-medium text-[24px] m-0 mb-1">
          Performance
        </h2>
        <p className="font-serif text-[14px] text-ink-soft leading-[1.55]">
          Live latency, error rate, and request volume. Connect a deploy
          to see real numbers — metering wiring is on the way.
        </p>
      </header>
      <div className="grid gap-3 grid-cols-1 sm:grid-cols-3">
        <PerfTile label="p95 latency · 24h" testid="health-perf-latency" />
        <PerfTile label="error rate · 24h" testid="health-perf-errors" />
        <PerfTile label="requests · 24h" testid="health-perf-rps" />
      </div>
      <div className="mt-3 font-serif italic text-[12px] text-pencil">
        Metering pipeline tracked as ISS-18.
      </div>
    </section>
  );
}

function PerfTile({ label, testid }: { label: string; testid: string }) {
  return (
    <div
      data-testid={testid}
      className="border border-rule-2 bg-paper-2/40 px-4 py-5"
    >
      <div className="label-uc mb-2">{label}</div>
      <div className="font-serif italic font-medium text-[28px] text-pencil mb-1">
        —
      </div>
      <div className="font-serif italic text-[12px] text-pencil leading-[1.4]">
        Connect a deploy to see live performance.
      </div>
    </div>
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
