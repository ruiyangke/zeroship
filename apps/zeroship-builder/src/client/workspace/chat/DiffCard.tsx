import { Card } from "@zeroship/ui";
import type { Diff } from "../../types/chat";
import "./DiffCard.css";

export function DiffCard({ diff }: { diff: Diff }) {
  return (
    <Card variant="outline" className="diff-card" data-testid="diff-card">
      <div className="diff-card__bar">
        <span className="diff-card__path">{diff.path}</span>
        <span className="diff-card__tag">diff</span>
      </div>
      <div className="diff-card__body">
        {/* Simple line diff for now; richer diff tooling can land later. */}
        {simpleDiff(diff.before, diff.after).map((line, i) => (
          <div key={i} className="diff-card__line" data-kind={line.kind}>
            {line.kind === "add" ? "+ " : line.kind === "remove" ? "- " : "  "}
            {line.text}
          </div>
        ))}
      </div>
    </Card>
  );
}

interface DiffLine { kind: "add" | "remove" | "context"; text: string; }

function simpleDiff(before: string, after: string): DiffLine[] {
  const a = before.split("\n");
  const b = after.split("\n");
  const out: DiffLine[] = [];
  let i = 0, j = 0;
  while (i < a.length || j < b.length) {
    if (i < a.length && j < b.length && a[i] === b[j]) {
      out.push({ kind: "context", text: a[i] });
      i++; j++;
    } else if (j < b.length && (i >= a.length || a[i] !== b[j])) {
      out.push({ kind: "add", text: b[j] });
      j++;
    } else {
      out.push({ kind: "remove", text: a[i] });
      i++;
    }
  }
  return out;
}
