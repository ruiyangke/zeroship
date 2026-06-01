// Wizard-only terminal card. The wizard's `decide` node (plain
// LangGraph, see apps/zeroship-builder/src/server/wizard.ts) emits
// a `data-brief` chunk after surveys are exhausted. This card renders
// it: the LLM's refined summary, the answer trail, and a Begin button
// that hands off to project creation + Builder.
//
// Begin is a callback prop because brief-rendered surfaces differ:
//   - WizardPage hits createApp + sessionStorage stash + navigate to
//     /p/<id>/preview (per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §8.2.4)
//   - Future preview/embed contexts may just close
//
// Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §8.2.7: "After submit: the survey card collapses to a
// one-line summary…". Same intent here for the terminal brief — once
// the user clicks Begin, the card stays as a record but the button
// disables. The parent page is responsible for navigating away (so
// even a long mutation is visible).
//
// Rebuilt on @zeroship/ui crystal: a Card surface, a Tag eyebrow, a
// DescriptionList for the answer trail (question → answer pairs), and a
// Button footer. Bespoke type/colour reads --zs-* tokens in the
// co-located BriefCard.css.

import { Button, Card, DescriptionList, Stack, Tag } from "@zeroship/ui";
import type { Brief } from "../../types/chat";
import "./BriefCard.css";

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
    <Card
      variant="outline"
      size="sm"
      data-testid="brief-card"
      data-committed={committed ? "" : undefined}
      className="zs-brief-card"
    >
      <Card.Content>
        <Stack gap={3}>
          <Stack gap={2}>
            <Tag size="sm" className="zs-brief-card__eyebrow">
              Brief ready
            </Tag>
            <p className="zs-brief-card__summary">{brief.summary}</p>
          </Stack>

          {brief.answers.length > 0 && (
            <Stack gap={2} className="zs-brief-card__answers">
              <Tag size="sm" className="zs-brief-card__eyebrow">
                What you told me
              </Tag>
              <DescriptionList
                orientation="vertical"
                className="zs-brief-card__trail"
              >
                {brief.answers.map((a, i) => (
                  <DescriptionList.Item
                    key={i}
                    className="zs-brief-card__pair"
                  >
                    <DescriptionList.Term className="zs-brief-card__question">
                      {a.question}
                    </DescriptionList.Term>
                    <DescriptionList.Detail className="zs-brief-card__answer">
                      <em>{stringifyAnswer(a.answer)}</em>
                    </DescriptionList.Detail>
                  </DescriptionList.Item>
                ))}
              </DescriptionList>
            </Stack>
          )}
        </Stack>
      </Card.Content>

      <Card.Footer align="between" divider="top">
        <span className="zs-brief-card__note">
          {committed ? "Beginning…" : "I'll create the project and start coding."}
        </span>
        <Button
          type="button"
          variant="filled"
          size="small"
          onClick={onBegin}
          disabled={disabled}
          data-testid="brief-begin"
        >
          {committed ? "Beginning…" : "Begin →"}
        </Button>
      </Card.Footer>
    </Card>
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
