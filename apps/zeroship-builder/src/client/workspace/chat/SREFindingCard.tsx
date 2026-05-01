// SRE finding card. Builder dispatches SRE via task("sre", …) when the
// user asks reliability questions ("why is the app slow?"). Middleware
// extracts the structured response (see _middleware.ts →
// normaliseSREFinding) and emits `data-sre-finding` which ChatMessages
// dispatches here.
//
// Visual contract:
//  - Severity drives the left-edge tone bar:
//      info     → ink   (just FYI)
//      warning  → amber (degraded but live)
//      error    → blood (broken for some users)
//      critical → blood w/ extra emphasis (broken for all / data loss)
//  - Diagnosis = headline / first thing the user reads.
//  - Recommendation = the actionable bit.
//  - related_logs = optional, collapsed by default to keep the card
//    compact when SRE doesn't ground in logs.

import { useState } from "react";
import type { SREFinding } from "../../types/chat";

const SEVERITY_TONE: Record<
  string,
  { label: string; pill: string; bar: string }
> = {
  info: {
    label: "info",
    pill: "text-pencil bg-paper-2 border-rule-2",
    bar: "bg-rule",
  },
  warning: {
    label: "warning",
    pill: "text-amber bg-paper-2 border-amber/40",
    bar: "bg-amber",
  },
  error: {
    label: "error",
    pill: "text-blood bg-paper-2 border-blood/40",
    bar: "bg-blood",
  },
  critical: {
    label: "critical",
    pill: "text-paper bg-blood border-blood font-medium",
    bar: "bg-blood",
  },
};

export function SREFindingCard({ finding }: { finding: SREFinding }) {
  const [logsOpen, setLogsOpen] = useState(false);
  const tone = SEVERITY_TONE[finding.severity] ?? SEVERITY_TONE.info;
  const logs = finding.related_logs ?? [];

  return (
    <div
      data-testid="sre-finding-card"
      className="mt-2 bg-paper border border-rule-2 rounded overflow-hidden flex"
    >
      <div className={"w-1 shrink-0 " + tone.bar} aria-hidden />
      <div className="flex-1 min-w-0">
        <div className="px-3 py-1.5 border-b border-rule-2 flex items-center justify-between">
          <span className="font-sans text-[10px] uppercase tracking-wider text-pencil">
            sre · finding
          </span>
          <span
            className={
              "font-sans text-[9px] uppercase tracking-wider px-1.5 py-0.5 border rounded " +
              tone.pill
            }
          >
            {tone.label}
          </span>
        </div>

        <div className="px-3 py-2.5 space-y-1.5">
          <div className="font-sans text-[13px] text-ink leading-snug">
            {finding.diagnosis}
          </div>
          {finding.recommendation && (
            <div className="font-serif italic text-[12px] text-ink-soft leading-snug">
              <span className="not-italic font-sans uppercase tracking-wider text-[9px] text-pencil mr-1.5">
                fix
              </span>
              {finding.recommendation}
            </div>
          )}
        </div>

        {logs.length > 0 && (
          <div className="border-t border-rule-2">
            <button
              type="button"
              data-testid="sre-logs-toggle"
              onClick={() => setLogsOpen((v) => !v)}
              className="w-full px-3 py-1.5 font-sans text-[10px] uppercase tracking-wider text-pencil hover:text-ink text-left flex items-center justify-between"
            >
              <span>related logs ({logs.length})</span>
              <span aria-hidden>{logsOpen ? "−" : "+"}</span>
            </button>
            {logsOpen && (
              <div className="px-3 pb-2.5 border-t border-rule-2 pt-2 space-y-1.5">
                {logs.map((l, i) => (
                  <div key={i}>
                    <div className="font-sans text-[10px] text-pencil">
                      {l.source}
                    </div>
                    <pre className="font-mono text-[11px] text-ink-soft whitespace-pre-wrap leading-snug">
                      {l.excerpt}
                    </pre>
                  </div>
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
