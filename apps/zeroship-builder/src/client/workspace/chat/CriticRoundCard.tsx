// Critic round indicator — a compact chat-stream card noting which
// critic pass this is (round N of M) and whether the critic approved
// or flagged concerns.
//
// Crystal: a small outline DS Card holding a horizontal DescriptionList
// (Round → N/M, Status → Badge). The status Badge is the colour signal
// — success when approved, warning when there are open concerns. The
// public interface (the `round` prop) and the `critic-round-card`
// testid are preserved exactly; only presentation changed.

import { Badge, Card, DescriptionList } from "@zeroship/ui";
import type { CriticRound } from "../../types/chat";
import "./CriticRoundCard.css";

export function CriticRoundCard({ round }: { round: CriticRound }) {
  const concerns = round.issues.length;

  return (
    <Card
      variant="outline"
      size="sm"
      data-testid="critic-round-card"
      className="zs-critic-round"
    >
      <DescriptionList orientation="horizontal" className="zs-critic-round__list">
        <DescriptionList.Item>
          <DescriptionList.Term>Critic round</DescriptionList.Term>
          <DescriptionList.Detail>
            {round.round}/{round.total}
          </DescriptionList.Detail>
        </DescriptionList.Item>
        <DescriptionList.Item>
          <DescriptionList.Term>Status</DescriptionList.Term>
          <DescriptionList.Detail>
            <Badge
              size="sm"
              variant="soft"
              intent={round.approved ? "success" : "warning"}
            >
              {round.approved
                ? "approved"
                : `${concerns} ${concerns === 1 ? "concern" : "concerns"}`}
            </Badge>
          </DescriptionList.Detail>
        </DescriptionList.Item>
      </DescriptionList>
    </Card>
  );
}
