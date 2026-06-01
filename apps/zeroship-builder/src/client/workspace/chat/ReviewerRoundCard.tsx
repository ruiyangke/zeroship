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

import { Badge, Card, Cluster } from "@zeroship/ui";
import type { ReviewerRound } from "../../types/chat";
import "./ReviewerRoundCard.css";

export function ReviewerRoundCard({ round }: { round: ReviewerRound }) {
  const blockerCount = round.blockers.length;

  // Compact form: approved with no blockers. Renders inline next to the
  // diff cards above it so the user can see "yes, this passed review"
  // without scanning a full card.
  if (round.approved && blockerCount === 0) {
    return (
      <Cluster
        data-testid="reviewer-round-card"
        gap={2}
        style={{ marginBlockStart: "var(--zs-space-1)" }}
      >
        <span className="zs-reviewer-card__eyebrow">reviewer</span>
        <Badge intent="success" variant="soft" size="sm">
          approved
        </Badge>
      </Cluster>
    );
  }

  // Expanded form: there are blockers (or approved=false). Render the
  // headline plus a stacked list. The list maxes at 5 blockers visually
  // and notes overflow ("+2 more") rather than scrolling — Reviewer's
  // job is to be terse.
  const visible = round.blockers.slice(0, 5);
  const overflow = blockerCount - visible.length;

  return (
    <Card
      data-testid="reviewer-round-card"
      variant="outline"
      size="sm"
      style={{ marginBlockStart: "var(--zs-space-2)" }}
    >
      <Card.Header>
        <Card.Title className="zs-reviewer-card__eyebrow zs-reviewer-card__title">
          reviewer
        </Card.Title>
        <Card.Action>
          <Badge
            intent={round.approved ? "warning" : "danger"}
            variant="soft"
            size="sm"
          >
            {round.approved
              ? `${blockerCount} ${blockerCount === 1 ? "concern" : "concerns"}`
              : "blocked"}
          </Badge>
        </Card.Action>
      </Card.Header>
      <Card.Content>
        <ul className="zs-reviewer-card__list">
          {visible.map((b, i) => (
            <li key={i} className="zs-reviewer-card__row">
              <span
                className="zs-reviewer-card__severity"
                data-severity={b.severity}
              >
                {b.severity}
              </span>
              <span className="zs-reviewer-card__kind">{b.kind}</span>
              <span className="zs-reviewer-card__why">
                {b.why}
                {b.fix ? (
                  <>
                    <span className="zs-reviewer-card__arrow"> → </span>
                    <span className="zs-reviewer-card__fix">{b.fix}</span>
                  </>
                ) : null}
              </span>
            </li>
          ))}
          {overflow > 0 && (
            <li className="zs-reviewer-card__overflow">+{overflow} more</li>
          )}
        </ul>
      </Card.Content>
    </Card>
  );
}
