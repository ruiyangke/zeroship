import type { Diff } from "../../types/chat";

export function DiffCard({ diff }: { diff: Diff }) {
  return (
    <div data-testid="diff-card" className="mt-2 bg-paper border border-rule-2 rounded">
      <div className="px-3 py-1.5 border-b border-rule-2 font-mono text-[11px] text-ink-soft flex items-center justify-between">
        <span>{diff.path}</span>
        <span className="text-pencil text-[10px]">diff</span>
      </div>
      <div className="p-3 font-mono text-[11px] leading-snug whitespace-pre overflow-x-auto max-h-48">
        {/* Simple line diff for now; richer diff tooling can land later. */}
        {simpleDiff(diff.before, diff.after).map((line, i) => (
          <div key={i} className={
            line.kind === "add"    ? "bg-ivy/10 text-ivy" :
            line.kind === "remove" ? "bg-blood/10 text-blood" :
            "text-ink-soft"
          }>
            {line.kind === "add" ? "+ " : line.kind === "remove" ? "- " : "  "}
            {line.text}
          </div>
        ))}
      </div>
    </div>
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
