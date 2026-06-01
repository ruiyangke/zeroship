import { useState } from "react";
import { Button, Card, Field, Input, Radio, Stack } from "@zeroship/ui";
import type { Survey, SurveyResponse } from "../../types/chat";
import "./SurveyCard.css";

export interface SurveyCardProps {
  survey: Survey;
  onSubmit: (response: SurveyResponse) => void;
  onSkip?: () => void;
  surveyId?: string;
}

export function SurveyCard({ survey, onSubmit, onSkip, surveyId = "anon" }: SurveyCardProps) {
  const [answers, setAnswers] = useState<Record<string, unknown>>({});
  const [submitted, setSubmitted] = useState(false);

  // Defensive: truncate to 3 questions per spec.
  const questions = survey.questions.slice(0, 3);

  function pick(qId: string, value: unknown) {
    setAnswers((a) => ({ ...a, [qId]: value }));
  }

  function submit() {
    const merged: Record<string, unknown> = {};
    for (const q of questions) {
      merged[q.id] = answers[q.id] ?? q.default;
    }
    setSubmitted(true);
    onSubmit({ survey_id: surveyId, answers: merged, skipped: false });
  }

  function skip() {
    setSubmitted(true);
    onSkip?.();
  }

  if (submitted) {
    return (
      <Card
        data-testid="survey-card-collapsed"
        variant="outline"
        size="sm"
        className="survey-card survey-card--collapsed"
      >
        Answered.
      </Card>
    );
  }

  const allRequiredAnswered = questions
    .filter((q) => q.required)
    .every((q) => answers[q.id] !== undefined);

  return (
    <Card
      data-testid="survey-card"
      variant="outline"
      size="sm"
      className="survey-card"
    >
      {survey.preamble && <p className="survey-card__preamble">{survey.preamble}</p>}
      <Stack gap={3}>
        {questions.map((q) => {
          if (q.kind.type === "short_text" || q.kind.type === "long_text") {
            // Text inputs carry their prompt as a linked Field.Label so the
            // control gets a programmatic name.
            return (
              <Field key={q.id} className="survey-card__field">
                <Field.Label className="survey-card__prompt">{q.prompt}</Field.Label>
                {q.kind.type === "short_text" ? (
                  <Input
                    size="sm"
                    placeholder={q.kind.placeholder}
                    maxLength={q.kind.max_length}
                    value={(answers[q.id] as string | undefined) ?? ""}
                    onChange={(e) => pick(q.id, e.target.value)}
                  />
                ) : (
                  <textarea
                    className="survey-card__textarea"
                    rows={2}
                    placeholder={q.kind.placeholder}
                    maxLength={q.kind.max_length}
                    value={(answers[q.id] as string | undefined) ?? ""}
                    onChange={(e) => pick(q.id, e.target.value)}
                  />
                )}
              </Field>
            );
          }

          // Choice questions: the prompt names the radio group; the chips
          // are the mutually-exclusive options.
          return (
            <div key={q.id}>
              <div className="survey-card__prompt">{q.prompt}</div>
              {q.kind.type === "single_choice" && (
                <Radio.Group
                  orientation="horizontal"
                  size="sm"
                  aria-label={q.prompt}
                  value={(answers[q.id] as string | undefined) ?? undefined}
                  onValueChange={(value) => pick(q.id, value)}
                >
                  {q.kind.options.slice(0, 6).map((opt) => (
                    <Radio key={opt.value} value={opt.value} label={opt.label} />
                  ))}
                </Radio.Group>
              )}
              {q.kind.type === "yes_no" && (
                <Radio.Group<boolean>
                  orientation="horizontal"
                  size="sm"
                  aria-label={q.prompt}
                  value={answers[q.id] === true ? true : answers[q.id] === false ? false : undefined}
                  onValueChange={(value) => pick(q.id, value)}
                >
                  <Radio<boolean> value={true} label="Yes" />
                  <Radio<boolean> value={false} label="No" />
                </Radio.Group>
              )}
              {/* multi_choice / scale / image_upload are not rendered yet */}
            </div>
          );
        })}
      </Stack>
      <div className="survey-card__actions">
        {onSkip ? (
          <Button type="button" variant="plain" size="small" onClick={skip}>
            {survey.skip_label ?? "skip — just build"}
          </Button>
        ) : (
          <span />
        )}
        <Button
          type="button"
          variant="filled"
          size="small"
          onClick={submit}
          disabled={!allRequiredAnswered}
        >
          Send →
        </Button>
      </div>
    </Card>
  );
}
