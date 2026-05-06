import { useState } from "react";
import { Button } from "../../components/Button";
import type { Survey, SurveyResponse } from "../../types/chat";

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
      <div data-testid="survey-card-collapsed" className="mt-2 px-3 py-1.5 border border-rule-2 bg-paper-2 rounded text-[12px] text-ink-soft">
        Answered.
      </div>
    );
  }

  const allRequiredAnswered = questions
    .filter((q) => q.required)
    .every((q) => answers[q.id] !== undefined);

  return (
    <div data-testid="survey-card" className="mt-2 bg-paper border border-rule rounded p-3">
      {survey.preamble && (
        <p className="font-serif text-[13.5px] text-ink mb-3">{survey.preamble}</p>
      )}
      <div className="space-y-3">
        {questions.map((q) => (
          <div key={q.id}>
            <div className="font-sans text-[12px] font-medium text-ink mb-1.5">{q.prompt}</div>
            {q.kind.type === "single_choice" && (
              <div className="flex flex-wrap gap-2">
                {q.kind.options.slice(0, 6).map((opt) => (
                  <Button
                    key={opt.value}
                    type="button"
                    size="sm"
                    variant={answers[q.id] === opt.value ? "primary" : "secondary"}
                    onClick={() => pick(q.id, opt.value)}
                  >
                    {opt.label}
                  </Button>
                ))}
              </div>
            )}
            {q.kind.type === "yes_no" && (
              <div className="flex gap-2">
                <Button
                  type="button"
                  size="sm"
                  variant={answers[q.id] === true ? "primary" : "secondary"}
                  onClick={() => pick(q.id, true)}
                >
                  Yes
                </Button>
                <Button
                  type="button"
                  size="sm"
                  variant={answers[q.id] === false ? "primary" : "secondary"}
                  onClick={() => pick(q.id, false)}
                >
                  No
                </Button>
              </div>
            )}
            {q.kind.type === "short_text" && (
              <input
                type="text"
                placeholder={q.kind.placeholder}
                maxLength={q.kind.max_length}
                value={(answers[q.id] as string | undefined) ?? ""}
                onChange={(e) => pick(q.id, e.target.value)}
                className="w-full px-2 py-1.5 border border-rule bg-paper-2 rounded font-sans text-[13px] focus:outline-none focus:border-ink"
              />
            )}
            {q.kind.type === "long_text" && (
              <textarea
                rows={2}
                placeholder={q.kind.placeholder}
                maxLength={q.kind.max_length}
                value={(answers[q.id] as string | undefined) ?? ""}
                onChange={(e) => pick(q.id, e.target.value)}
                className="w-full px-2 py-1.5 border border-rule bg-paper-2 rounded font-sans text-[13px] focus:outline-none focus:border-ink resize-none"
              />
            )}
            {/* multi_choice / scale / image_upload are not rendered yet */}
          </div>
        ))}
      </div>
      <div className="flex justify-between items-center mt-3">
        {onSkip ? (
          <button
            type="button"
            onClick={skip}
            className="font-sans text-[11px] text-pencil hover:text-ink cursor-pointer"
          >
            {survey.skip_label ?? "skip — just build"}
          </button>
        ) : <span />}
        <Button type="button" size="sm" variant="primary" onClick={submit} disabled={!allRequiredAnswered}>
          Send →
        </Button>
      </div>
    </div>
  );
}
