// Pre-deploy hard-gate card. Builder dispatches Reviewer via
// task("reviewer", …) before any deploy; the middleware extracts the
// structured response (see internal/middleware.ts -> normaliseReviewerRound)
// and emits a `data-reviewer-round` chunk that ChatMessages dispatches
// here.
//
// Visual contract:
//  - approved=true with no blockers → small green badge "review passed".
//  - approved=true with low/medium blockers → amber badge with count.
//  - approved=false → expanded card listing each blocker (kind +
//    severity + why + fix). Blockers are the actionable bit; lay them
//    out so the user can see them without expanding.

import type { ReviewerRound } from "../../types/chat";

const SEVERITY_TONE: Record<string, string> = {
  low: "text-pencil",
  medium: "text-amber",
  high: "text-blood",
  critical: "text-blood font-medium",
};

export function ReviewerRoundCard({ round }: { round: ReviewerRound }) {
  const blockerCount = round.blockers.length;

  // Compact form: approved with no blockers. Renders inline next to the
  // diff cards above it so the user can see "yes, this passed review"
  // without scanning a full card.
  if (round.approved && blockerCount === 0) {
    return (
      <div
        data-testid="reviewer-round-card"
        className="mt-1.5 inline-flex items-center gap-2 px-2 py-1 bg-paper-2 border border-rule-2 rounded"
      >
        <span className="font-sans text-[10px] uppercase tracking-wider text-pencil">
          reviewer
        </span>
        <span className="font-sans text-[10px] text-ivy">approved</span>
      </div>
    );
  }

  // Expanded form: there are blockers (or approved=false). Render the
  // headline plus a stacked list. The list maxes at 5 blockers visually
  // and notes overflow ("+2 more") rather than scrolling — Reviewer's
  // job is to be terse.
  const visible = round.blockers.slice(0, 5);
  const overflow = blockerCount - visible.length;

  return (
    <div
      data-testid="reviewer-round-card"
      className="mt-2 bg-paper border border-rule-2 rounded"
    >
      <div className="px-3 py-1.5 border-b border-rule-2 flex items-center justify-between">
        <span className="font-sans text-[10px] uppercase tracking-wider text-pencil">
          reviewer
        </span>
        <span
          className={
            "font-sans text-[10px] " +
            (round.approved ? "text-amber" : "text-blood")
          }
        >
          {round.approved
            ? `${blockerCount} ${blockerCount === 1 ? "concern" : "concerns"}`
            : "blocked"}
        </span>
      </div>
      <ul className="px-3 py-2 space-y-1.5 font-mono text-[11px] leading-snug">
        {visible.map((b, i) => (
          <li key={i} className="flex items-start gap-2">
            <span
              className={
                "shrink-0 font-sans uppercase tracking-wider text-[9px] mt-0.5 " +
                (SEVERITY_TONE[b.severity] ?? "text-pencil")
              }
            >
              {b.severity}
            </span>
            <span className="shrink-0 text-ink-soft">{b.kind}</span>
            <span className="text-ink min-w-0">
              {b.why}
              {b.fix ? (
                <>
                  <span className="text-pencil"> → </span>
                  <span className="text-ink-soft">{b.fix}</span>
                </>
              ) : null}
            </span>
          </li>
        ))}
        {overflow > 0 && (
          <li className="text-pencil font-sans text-[10px]">
            +{overflow} more
          </li>
        )}
      </ul>
    </div>
  );
}
