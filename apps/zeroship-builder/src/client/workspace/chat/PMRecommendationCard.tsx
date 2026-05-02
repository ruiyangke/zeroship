// PM recommendation card. Builder dispatches PM via task("pm", …) when
// the user asks "what should I build next?". Middleware extracts the
// structured response (see _middleware.ts → normalisePMRecommendation)
// and emits `data-pm-recommendation` which ChatMessages dispatches here.
//
// Visual contract:
//  - Headline = the primary recommendation's title + urgency pill.
//  - Body    = the `why` for the primary.
//  - Disclosure expandable with up to 2 alternatives.

import { useState } from "react";
import type { PMRecommendation, PMRecommendationItem } from "../../types/chat";

const URGENCY_TONE: Record<string, string> = {
  low: "text-pencil bg-paper-2 border-rule-2",
  medium: "text-amber bg-paper-2 border-rule-2",
  // "high" lands on ivy not blood — high-urgency for PM means
  // "unblocks the next milestone", which is positive momentum, not
  // failure. Blood is reserved for SRE/Reviewer destructive cases.
  high: "text-ivy bg-ivy-3 border-ivy/30",
};

export function PMRecommendationCard({
  recommendation,
}: {
  recommendation: PMRecommendation;
}) {
  const [open, setOpen] = useState(false);
  const alts = recommendation.alternatives;
  const primary = recommendation.recommendation;

  return (
    <div
      data-testid="pm-recommendation-card"
      className="mt-2 bg-paper border border-rule-2 rounded"
    >
      <div className="px-3 py-1.5 border-b border-rule-2 flex items-center justify-between">
        <span className="font-sans text-[10px] uppercase tracking-wider text-pencil">
          pm · what to build next
        </span>
        <UrgencyPill urgency={primary.urgency} />
      </div>

      <div className="px-3 py-2.5">
        <div className="font-sans text-[13px] text-ink leading-snug">
          {primary.title}
        </div>
        <div className="font-serif italic text-[12px] text-ink-soft mt-1 leading-snug">
          {primary.why}
        </div>
        {primary.issueId && (
          <div className="mt-1 font-mono text-[10px] text-pencil">
            #{primary.issueId}
          </div>
        )}
      </div>

      {alts.length > 0 && (
        <div className="border-t border-rule-2">
          <button
            type="button"
            data-testid="pm-alternatives-toggle"
            onClick={() => setOpen((v) => !v)}
            className="w-full px-3 py-1.5 font-sans text-[10px] uppercase tracking-wider text-pencil hover:text-ink text-left flex items-center justify-between"
          >
            <span>see alternatives ({alts.length})</span>
            <span aria-hidden>{open ? "−" : "+"}</span>
          </button>
          {open && (
            <ul className="px-3 pb-2.5 space-y-2 border-t border-rule-2 pt-2">
              {alts.map((a, i) => (
                <AltRow key={i} alt={a} />
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}

function AltRow({ alt }: { alt: PMRecommendationItem }) {
  return (
    <li>
      <div className="flex items-start justify-between gap-2">
        <div className="font-sans text-[12px] text-ink leading-snug min-w-0">
          {alt.title}
        </div>
        <UrgencyPill urgency={alt.urgency} />
      </div>
      <div className="font-serif italic text-[11px] text-ink-soft leading-snug">
        {alt.why}
      </div>
      {alt.issueId && (
        <div className="font-mono text-[10px] text-pencil">#{alt.issueId}</div>
      )}
    </li>
  );
}

function UrgencyPill({ urgency }: { urgency: string }) {
  const tone = URGENCY_TONE[urgency] ?? URGENCY_TONE.medium;
  return (
    <span
      className={
        "shrink-0 font-sans text-[9px] uppercase tracking-wider px-1.5 py-0.5 border rounded " +
        tone
      }
    >
      {urgency}
    </span>
  );
}
