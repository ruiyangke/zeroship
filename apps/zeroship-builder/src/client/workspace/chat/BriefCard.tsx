// Wizard-only terminal card. The wizard's `decide` node (plain
// LangGraph, see apps/zeroship-builder/src/server/_wizard.ts) emits
// a `data-brief` chunk after surveys are exhausted. This card renders
// it: the LLM's refined summary, the answer trail, and a Begin button
// that hands off to project creation + Builder.
//
// Begin is a callback prop because brief-rendered surfaces differ:
//   - WizardPage hits createApp + sessionStorage stash + navigate to
//     /p/<id>/preview (per spec §8.2.4)
//   - Future preview/embed contexts may just close
//
// Per spec §8.2.7: "After submit: the survey card collapses to a
// one-line summary…". Same intent here for the terminal brief — once
// the user clicks Begin, the card stays as a record but the button
// disables. The parent page is responsible for navigating away (so
// even a long mutation is visible).

import { Button } from "../../components/Button";
import type { Brief } from "../../types/chat";

export interface BriefCardProps {
  brief: Brief;
  onBegin: () => void;
  busy?: boolean;
  /** Set true once Begin has been clicked at least once. Disables the
   *  button + dims the card so the user understands the action is
   *  in flight or has already fired. */
  committed?: boolean;
}

export function BriefCard({ brief, onBegin, busy, committed }: BriefCardProps) {
  const disabled = busy || committed;
  return (
    <div
      data-testid="brief-card"
      className={`mt-3 bg-paper border border-rule rounded-md p-4 ${committed ? "opacity-70" : ""}`}
    >
      <div className="font-sans text-[10px] uppercase tracking-wider text-pencil mb-2">
        Brief ready
      </div>
      <p className="font-serif text-[15px] leading-[1.55] text-ink mb-3">
        {brief.summary}
      </p>

      {brief.answers.length > 0 && (
        <div className="border-t border-rule-2 pt-3 mb-3">
          <div className="font-sans text-[10px] uppercase tracking-wider text-pencil mb-2">
            What you told me
          </div>
          <ul className="m-0 p-0 list-none font-serif text-[13px] leading-[1.65] text-ink-soft">
            {brief.answers.map((a, i) => (
              <li
                key={i}
                className="before:content-['·'] before:text-tomato before:font-bold before:mr-2"
              >
                <span className="text-ink">{a.question}</span>{" "}
                <em className="italic">{stringifyAnswer(a.answer)}</em>
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="flex justify-between items-center">
        <span className="font-sans text-[11px] text-pencil italic">
          {committed ? "Beginning…" : "I'll create the project and start coding."}
        </span>
        <Button
          type="button"
          variant="primary"
          size="sm"
          onClick={onBegin}
          disabled={disabled}
          data-testid="brief-begin"
        >
          {committed ? "Beginning…" : "Begin →"}
        </Button>
      </div>
    </div>
  );
}

function stringifyAnswer(value: unknown): string {
  if (value == null) return "(skipped)";
  if (typeof value === "string") return value;
  if (typeof value === "boolean") return value ? "yes" : "no";
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}
