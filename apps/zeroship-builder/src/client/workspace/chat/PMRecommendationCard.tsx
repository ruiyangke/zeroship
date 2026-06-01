// PM recommendation card. Builder dispatches PM via task("pm", …) when
// the user asks "what should I build next?". Middleware extracts the
// structured response (see internal/middleware.ts -> normalisePMRecommendation)
// and emits `data-pm-recommendation` which ChatMessages dispatches here.
//
// Visual contract:
//  - Headline = the primary recommendation's title + urgency badge.
//  - Body    = the `why` for the primary.
//  - Disclosure expandable with up to 2 alternatives.
//
// Crystal: built on @zeroship/ui Card + Collapsible + Badge over the
// Stack / Cluster layout primitives. Bespoke type ramp + the issue-id
// mono chip live in the co-located PMRecommendationCard.css over --zs-*
// tokens.

import { Badge, Card, Cluster, Collapsible, Stack } from "@zeroship/ui";
import type { BadgeIntent } from "@zeroship/ui";
import type { PMRecommendation, PMRecommendationItem } from "../../types/chat";
import "./PMRecommendationCard.css";

// Urgency → Badge intent.
//   low    → neutral (a quiet, non-semantic chip)
//   medium → warning (caution / amber)
//   high   → success — high-urgency for PM means "unblocks the next
//            milestone", which is positive momentum, not failure. The
//            danger intent stays reserved for SRE/Reviewer destructive
//            cases.
const URGENCY_INTENT: Record<string, BadgeIntent> = {
  low: "neutral",
  medium: "warning",
  high: "success",
};

export function PMRecommendationCard({
  recommendation,
}: {
  recommendation: PMRecommendation;
}) {
  const alts = recommendation.alternatives;
  const primary = recommendation.recommendation;

  return (
    <Card
      data-testid="pm-recommendation-card"
      variant="outline"
      className="pm-rec"
    >
      <Card.Header className="pm-rec__header">
        <Cluster justify="between" align="center">
          <span className="pm-rec__eyebrow">pm · what to build next</span>
          <UrgencyBadge urgency={primary.urgency} />
        </Cluster>
      </Card.Header>

      <Card.Content className="pm-rec__body">
        <Stack gap="half">
          <div className="pm-rec__title">{primary.title}</div>
          <div className="pm-rec__why">{primary.why}</div>
          {primary.issueId && (
            <div className="pm-rec__issue">#{primary.issueId}</div>
          )}
        </Stack>
      </Card.Content>

      {alts.length > 0 && (
        <Collapsible className="pm-rec__disclosure">
          <Collapsible.Trigger
            data-testid="pm-alternatives-toggle"
            className="pm-rec__toggle"
            asChild
          >
            <button type="button">
              <span>see alternatives ({alts.length})</span>
              <span aria-hidden className="pm-rec__toggle-glyph" />
            </button>
          </Collapsible.Trigger>
          <Collapsible.Panel className="pm-rec__panel">
            <ul className="pm-rec__alts">
              {alts.map((a, i) => (
                <AltRow key={i} alt={a} />
              ))}
            </ul>
          </Collapsible.Panel>
        </Collapsible>
      )}
    </Card>
  );
}

function AltRow({ alt }: { alt: PMRecommendationItem }) {
  return (
    <li className="pm-rec__alt">
      <Cluster justify="between" align="start" gap={2}>
        <div className="pm-rec__alt-title">{alt.title}</div>
        <UrgencyBadge urgency={alt.urgency} />
      </Cluster>
      <div className="pm-rec__alt-why">{alt.why}</div>
      {alt.issueId && <div className="pm-rec__issue">#{alt.issueId}</div>}
    </li>
  );
}

function UrgencyBadge({ urgency }: { urgency: string }) {
  const intent = URGENCY_INTENT[urgency] ?? URGENCY_INTENT.medium;
  return (
    <Badge intent={intent} variant="soft" size="sm" className="pm-rec__urgency">
      {urgency}
    </Badge>
  );
}
