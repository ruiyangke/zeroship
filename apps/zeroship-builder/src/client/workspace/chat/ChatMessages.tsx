// Chat messages renderer for the workspace ChatRail.
//
// The assistant renderer dispatches several families of v6 message parts:
//   - text                       → MessageAssistant text (concatenated)
//   - tool-<name> / dynamic-tool → <Receipt> (one per toolCallId)
//   - data-diff                  → <DiffCard>
//   - data-survey                → <SurveyCard> (interrupt resume flow)
//   - data-critic-round          → <CriticRoundCard>
//   - data-reviewer-round        → <ReviewerRoundCard>
//   - data-pm-recommendation     → <PMRecommendationCard>
//   - data-sre-finding           → <SREFindingCard>
//   - data-brief                 → <BriefCard> (wizard-only)
//
// The translator emits chunks of these types on the wire (see
// apps/zeroship-builder/src/server/chat.ts and _middleware.ts).
// `useChat` from @ai-sdk/react reassembles them into UIMessage.parts[]
// with stable discriminants — we just type-narrow and render.
//
// Tool-call lifecycle states (per node_modules/ai/dist/index.d.ts:1694)
// arrive as a single part whose `state` cycles through input-streaming
// → input-available → output-available (or terminal output-error /
// output-denied). The Receipt component reads that lifecycle to render
// running / done / error badges.

import { useEffect, useRef, type ReactNode } from "react";
import type { UIMessage } from "ai";
import { MessageUser } from "./MessageUser";
import { MessageAssistant } from "./MessageAssistant";
import { Receipt } from "./Receipt";
import { DiffCard } from "./DiffCard";
import { SurveyCard } from "./SurveyCard";
import { BriefCard } from "./BriefCard";
import { CriticRoundCard } from "./CriticRoundCard";
import { ReviewerRoundCard } from "./ReviewerRoundCard";
import { PMRecommendationCard } from "./PMRecommendationCard";
import { SREFindingCard } from "./SREFindingCard";
import type {
  Brief,
  CriticRound,
  Diff,
  PMRecommendation,
  ReviewerRound,
  SREFinding,
  Survey,
  SurveyResponse,
} from "../../types/chat";

export interface ChatMessagesProps {
  messages: UIMessage[];
  busy: boolean;
  // Called when the user submits a SurveyCard rendered from a
  // `data-survey` part. ChatRail wires this to the resume protocol —
  // see api.ts `chatTransport` and server-side `chat.ts`.
  onSubmitSurvey?: (token: string, response: SurveyResponse) => void;
  // Set of survey tokens that have already been answered (or skipped).
  // SurveyCard collapses to "Answered." once its token is in this set,
  // and submit handlers no-op. Keyed by token (the `data-survey` chunk
  // id), so multiple surveys in the same conversation each manage
  // their own state.
  answeredSurveys?: ReadonlySet<string>;
  // Wizard-only: called when the user clicks Begin on a BriefCard
  // rendered from a `data-brief` part. WizardPage wires this to
  // project creation + navigation. Builder surfaces never receive a
  // data-brief chunk (it's a wizard-runtime-only output) so omitting
  // this prop in ChatRail is fine.
  onBeginBrief?: (brief: Brief) => void;
  /** True once a Begin has been clicked. Renders all BriefCards in
   *  committed state so a user who scrolls back doesn't think the
   *  action is still pending. */
  briefCommitted?: boolean;
  /** Loading flag for the createApp mutation triggered by Begin. */
  briefBusy?: boolean;
  /** Hover-action: regenerate an assistant turn. ChatRail wires this
   *  to useChat.regenerate(). Hidden on streaming turns. */
  onRegenerate?: () => void;
  /** Hover-action: edit a prior user message and replay from there.
   *  Receives the message id + the new text. */
  onEditUser?: (messageId: string, newText: string) => void;
}

export function ChatMessages({
  messages,
  busy,
  onSubmitSurvey,
  answeredSurveys,
  onBeginBrief,
  briefCommitted,
  briefBusy,
  onRegenerate,
  onEditUser,
}: ChatMessagesProps) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  useEffect(() => {
    if (!stickToBottom.current) return;
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messages]);

  function onScroll() {
    const el = scrollRef.current;
    if (!el) return;
    const dist = el.scrollHeight - el.clientHeight - el.scrollTop;
    stickToBottom.current = dist < 60;
  }

  return (
    <div
      ref={scrollRef}
      onScroll={onScroll}
      data-testid="chat-messages"
      className="flex-1 overflow-y-auto px-5 py-4 flex flex-col gap-5 min-h-0"
    >
      {messages.length === 0 && !busy && (
        <div
          data-testid="chat-empty"
          className="flex flex-col gap-2 text-ink-soft"
        >
          <div className="font-display italic text-[18px] text-ink leading-tight">
            What shall we make?
          </div>
          <div className="font-serif text-[13.5px] italic text-pencil leading-snug">
            Describe an app, paste a screenshot, or sketch a feature.
            Type{" "}
            <span className="font-mono not-italic text-[12px]">@</span>{" "}
            to mention a file, an issue, or a recent error.
          </div>
        </div>
      )}

      {messages.map((m, idx) => {
        const text = (m.parts as Array<{ type: string; text?: string }>)
          .filter((p) => p.type === "text")
          .map((p) => p.text ?? "")
          .join("");

        if (m.role === "user") {
          return (
            <MessageUser
              key={m.id}
              text={text}
              onEdit={
                onEditUser ? (newText) => onEditUser(m.id, newText) : undefined
              }
            />
          );
        }

        const isLast = idx === messages.length - 1;
        const renderedParts = renderAssistantParts(m, {
          onSubmitSurvey,
          answeredSurveys,
          onBeginBrief,
          briefCommitted,
          briefBusy,
        });
        return (
          <MessageAssistant
            key={m.id}
            text={text}
            streaming={isLast && busy}
            parts={renderedParts}
            // Only the latest assistant turn gets a regenerate button.
            // Regenerating a mid-history turn would require truncating
            // forward and replaying — the AI SDK's regenerate() always
            // operates on the tail, so only expose it where it matches
            // user intent.
            onRegenerate={
              onRegenerate && isLast && !busy ? onRegenerate : undefined
            }
          />
        );
      })}
    </div>
  );
}

// Render non-text assistant message parts (tool invocations, custom
// data parts). Returns null if there's nothing to render so
// MessageAssistant can avoid the wrapping <div>.
function renderAssistantParts(
  m: UIMessage,
  ctx: {
    onSubmitSurvey?: (token: string, response: SurveyResponse) => void;
    answeredSurveys?: ReadonlySet<string>;
    onBeginBrief?: (brief: Brief) => void;
    briefCommitted?: boolean;
    briefBusy?: boolean;
  },
): ReactNode {
  const out: ReactNode[] = [];

  for (const part of m.parts as Array<Record<string, unknown>>) {
    const type = typeof part.type === "string" ? part.type : "";

    // Tool invocation parts: `tool-<name>` (static tools registered
    // upfront) or `dynamic-tool` (everything else — including
    // deepagents' built-in fs/exec tools, which aren't declared in
    // `tools[]` at agent-construction time).
    if (type.startsWith("tool-") || type === "dynamic-tool") {
      const toolName =
        type === "dynamic-tool"
          ? String(part.toolName ?? "tool")
          : type.slice("tool-".length);
      // Suppress write_file / edit_file receipts — those flow through
      // <DiffCard> as data-diff parts and we don't want both rendering.
      if (toolName === "write_file" || toolName === "edit_file") continue;

      const toolCallId = String(part.toolCallId ?? `${m.id}-${out.length}`);
      const state = String(part.state ?? "");
      const status: "running" | "done" | "error" =
        state === "output-available"
          ? "done"
          : state === "output-error" || state === "output-denied"
          ? "error"
          : "running";
      out.push(
        <Receipt
          key={toolCallId}
          toolName={toolName}
          status={status}
          inputJson={part.input}
          outputJson={
            state === "output-available"
              ? part.output
              : state === "output-error"
              ? part.errorText
              : undefined
          }
        />,
      );
      continue;
    }

    // Custom data parts. The wire type is `data-<NAME>` with a `data`
    // payload (per node_modules/ai/dist/index.d.ts:2055-2062).
    if (type === "data-diff") {
      const diff = (part as { data?: Diff }).data;
      if (diff && typeof diff.path === "string") {
        out.push(<DiffCard key={`diff-${out.length}`} diff={diff} />);
      }
      continue;
    }

    if (type === "data-survey") {
      // Server emits `{ id: token, data: { token, survey } }`. The id
      // and data.token are the same value — we use it both as the
      // React key (stable across re-renders) and as the resume token
      // submitted back to the server.
      const surveyData = (part as { data?: { token?: unknown; survey?: unknown } }).data;
      const token =
        typeof surveyData?.token === "string"
          ? surveyData.token
          : String(part.id ?? "");
      const survey = surveyData?.survey as Survey | undefined;
      if (token && survey && Array.isArray(survey.questions)) {
        const answered = ctx.answeredSurveys?.has(token) ?? false;
        out.push(
          <SurveyCard
            key={`survey-${token}`}
            survey={survey}
            surveyId={token}
            // When already-answered, suppress callbacks so the parent
            // doesn't re-fire a stale resume on re-render. The card
            // itself collapses internally on submit, but the parent
            // also tracks answered tokens (see ChatRail) so a
            // remounted card stays collapsed.
            onSubmit={(response) => {
              if (answered) return;
              ctx.onSubmitSurvey?.(token, response);
            }}
            onSkip={() => {
              if (answered) return;
              ctx.onSubmitSurvey?.(token, {
                survey_id: token,
                answers: {},
                skipped: true,
              });
            }}
          />,
        );
      }
      continue;
    }

    if (type === "data-critic-round") {
      // Builder dispatches Critic via task("critic", …) after a write
      // batch. Middleware extracts the structured response and emits
      // this chunk; we render a small badge per round.
      const round = (part as { data?: CriticRound }).data;
      if (round && typeof round.round === "number") {
        out.push(
          <CriticRoundCard key={`critic-${out.length}`} round={round} />,
        );
      }
      continue;
    }

    if (type === "data-reviewer-round") {
      // Pre-deploy hard-gate result. Builder calls task("reviewer", …)
      // before any deploy; middleware emits this chunk. The card
      // renders a compact badge when approved with no blockers, or an
      // expanded list when there are blockers / approved=false.
      const round = (part as { data?: ReviewerRound }).data;
      if (round && typeof round.approved === "boolean") {
        out.push(
          <ReviewerRoundCard key={`reviewer-${out.length}`} round={round} />,
        );
      }
      continue;
    }

    if (type === "data-pm-recommendation") {
      // PM SubAgent's strategic recommendation. Builder calls
      // task("pm", …) when the user asks "what should I build next?".
      const rec = (part as { data?: PMRecommendation }).data;
      if (rec && rec.recommendation && typeof rec.recommendation.title === "string") {
        out.push(
          <PMRecommendationCard
            key={`pm-${out.length}`}
            recommendation={rec}
          />,
        );
      }
      continue;
    }

    if (type === "data-sre-finding") {
      // SRE SubAgent's diagnosis. Builder calls task("sre", …) when
      // the user asks reliability questions.
      const finding = (part as { data?: SREFinding }).data;
      if (finding && typeof finding.diagnosis === "string") {
        out.push(
          <SREFindingCard key={`sre-${out.length}`} finding={finding} />,
        );
      }
      continue;
    }

    if (type === "data-brief") {
      // Wizard-only terminal chunk. Defensive: only render if onBeginBrief
      // is wired — Builder surfaces (which never see data-brief) shouldn't
      // accidentally render an action button with no handler. Brief shape
      // mirrors the server's WizardBrief.
      const brief = (part as { data?: Brief }).data;
      if (brief && typeof brief.summary === "string" && ctx.onBeginBrief) {
        out.push(
          <BriefCard
            key={`brief-${out.length}`}
            brief={brief}
            onBegin={() => ctx.onBeginBrief!(brief)}
            busy={ctx.briefBusy}
            committed={ctx.briefCommitted}
          />,
        );
      }
      continue;
    }

  }

  return out.length > 0 ? <>{out}</> : null;
}
