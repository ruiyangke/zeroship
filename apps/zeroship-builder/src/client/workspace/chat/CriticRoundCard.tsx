import type { CriticRound } from "../../types/chat";

export function CriticRoundCard({ round }: { round: CriticRound }) {
  return (
    <div
      data-testid="critic-round-card"
      className="mt-1.5 inline-flex items-center gap-2 px-2 py-1 bg-paper-2 border border-rule-2 rounded"
    >
      <span className="font-sans text-[10px] uppercase tracking-wider text-pencil">
        critic round {round.round}/{round.total}
      </span>
      <span className={
        "font-sans text-[10px] " +
        (round.approved ? "text-ivy" : "text-amber")
      }>
        {round.approved ? "approved" : `${round.issues.length} concerns`}
      </span>
    </div>
  );
}
