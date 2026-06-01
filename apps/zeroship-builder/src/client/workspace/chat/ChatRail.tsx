// Chat rail — desk-side notebook for the builder workspace.
//
// Built on `@ai-sdk/react`'s `useChat` hook against the typed RPC
// procedure `rpc.chat`. The transport (URL + body envelope) is bound
// once in `client/api.ts` via `chatTransport(rpc.chat)`, so this
// component never touches the wire.
//
// This started as a text-only mock. The visual chrome (header,
// ChatComposer, error band) is still unchanged. ChatMessages renders the v6
// `UIMessage[]` shape (parts: [{ type: "text", text }]).
//
// The renderer also handles custom `data-*` parts (survey, diff,
// critic-round) into the assistant message renderer.

import { useEffect, useRef, useState } from "react";
import { useChat } from "@ai-sdk/react";
import { Button, Stack } from "@zeroship/ui";
import { rpc, chatTransport } from "../../api";
import type { Brief, SurveyResponse } from "../../types/chat";
import { ChatComposer } from "./ChatComposer";
import { ChatMessages } from "./ChatMessages";
import "./ChatRail.css";

export interface ChatRailProps {
  appName?: string;
  /** App id from the URL — feeds the composer's `@`-mention dropdown
   *  (file picker / issue picker / recent log error). When omitted
   *  (dev shell / no project), the dropdown silently degrades to
   *  "no suggestions". */
  appId?: string;
  /** Wizard hand-off: the brief stashed by WizardWorkspace.handleBegin
   *  and consumed once by WorkspaceShell. ChatRail synthesises a first
   *  user message from this on mount so Builder has full clarification
   *  context. Run-once: the parent only passes this on the very first
   *  render after navigation. */
  seedBrief?: Brief;
}

/**
 * Render an answer value for human-readable transport in the seeded
 * first turn. Mirrors `wizard.ts:wstringifyAnswer` and
 * `BriefCard.tsx:stringifyAnswer` — kept inline here rather than DRYed
 * into types/chat.ts because the helper is rendering policy (Builder-
 * facing), not part of the wire shape.
 */
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

function buildSeedMessage(brief: Brief): string {
  const lines: string[] = [];
  lines.push("Brief from the wizard:");
  lines.push("");
  lines.push(`Original idea: ${brief.idea}`);
  lines.push("");
  lines.push(`Refined summary: ${brief.summary}`);
  if (brief.answers.length > 0) {
    lines.push("");
    lines.push("Survey answers:");
    for (const a of brief.answers) {
      lines.push(`- ${a.question}: ${stringifyAnswer(a.answer)}`);
    }
  }
  lines.push("");
  lines.push("Please start coding it.");
  return lines.join("\n");
}

export function ChatRail({ appName, appId, seedBrief }: ChatRailProps) {
  const [input, setInput] = useState("");
  // Tokens of surveys the user has already answered or skipped this
  // session. Prevents the SurveyCard from re-firing on re-render after
  // resume, and lets the renderer collapse the card if a previously-
  // answered survey scrolls back into view. Scoped to the ChatRail
  // instance — refresh = clean slate, matching useChat's per-mount
  // message state.
  const [answeredSurveys, setAnsweredSurveys] = useState<Set<string>>(() => new Set());

  const { messages, sendMessage, setMessages, regenerate, status, error, stop } =
    useChat({
      // appId is threaded through the transport's body so the server-
      // side data-part middleware can persist Critic-graded quality
      // scorecards into the right project's KV slot.
      transport: chatTransport(rpc.chat, { appId }),
      onError: (err) => console.error("[chat]", err),
    });

  const busy = status === "submitted" || status === "streaming";

  // Wizard handoff: when WorkspaceShell consumed a pending brief and
  // passed it as `seedBrief`, synthesise a first user message so
  // Builder kicks off with full context. Empty-deps useEffect runs
  // once on mount; the `seeded` ref guards against React 18 strict
  // mode double-invocation in dev (and any hot-reload re-mount). The
  // explicit `messages.length === 0 && !busy` check is belt-and-
  // braces — by the time the rail mounts in production those are
  // both true. We don't include `seedBrief` in deps because the
  // parent only passes it on the very first render after navigation
  // (sessionStorage one-shot consume in WorkspaceShell).
  const seeded = useRef(false);
  useEffect(() => {
    if (!seedBrief || seeded.current) return;
    if (messages.length !== 0 || busy) return;
    seeded.current = true;
    sendMessage({ text: buildSeedMessage(seedBrief) });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function handleSubmit(text: string, _attachments: File[]) {
    if (!text.trim() || busy) return;
    sendMessage({ text });
  }

  /**
   * Truncate the conversation at (and including) the edited user
   * message, then re-fire the new text as a fresh send. The AI SDK's
   * `regenerate()` rewinds to the previous user turn — useful for the
   * "↻ regenerate" affordance — but doesn't accept a substitute prompt.
   * For "✎ edit prior" we want to *replace* the user message, so we
   * mutate `messages` directly via setMessages, then sendMessage. The
   * server treats the submitted history as the new ground truth.
   */
  function handleEditUser(messageId: string, newText: string) {
    if (busy) return;
    const idx = messages.findIndex((m) => m.id === messageId);
    if (idx < 0) return;
    setMessages(messages.slice(0, idx));
    sendMessage({ text: newText });
  }

  /** ↻ regenerate on the latest assistant turn. The SDK helper drops
   *  the trailing assistant message and re-runs the previous user
   *  turn — exactly the `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §10.6 semantics. */
  function handleRegenerate() {
    if (busy) return;
    void regenerate();
  }

  /** Retry button shown on error. The SDK keeps the failed user turn
   *  in `messages` and clears `error` once a new request succeeds.
   *  We just call regenerate(): same intent, no manual replay. */
  function handleRetry() {
    if (busy) return;
    void regenerate();
  }

  function submitSurvey(token: string, response: SurveyResponse) {
    if (busy || answeredSurveys.has(token)) return;
    setAnsweredSurveys((s) => {
      const next = new Set(s);
      next.add(token);
      return next;
    });
    // The resume value the server feeds into the interrupted tool's
    // `interrupt(...)` return. Match the shape Builder expects from
    // `ask_survey` (see internal/tools.ts): `{ skipped, answers }`. Dropping
    // the `survey_id` because the tool already knows it (it's the one
    // that interrupted).
    const value = response.skipped
      ? { skipped: true }
      : { skipped: false, answers: response.answers };
    sendMessage(
      // No new user-visible message — pass undefined to skip composing
      // a turn-starting message. The transport (api.ts) detects
      // `body.resume` and rewrites the wire body to the resume shape;
      // server skips message replay and feeds Command({resume:value}).
      undefined,
      { body: { resume: { token, value } } },
    );
  }

  return (
    <Stack data-testid="chat-rail" className="zb-chat-rail">
      <Stack
        className="zb-chat-rail__header"
        direction="row"
        align="center"
        justify="between"
        gap={3}
      >
        <h3 className="zb-chat-rail__title">Notes &amp; thoughts</h3>
        <span className="zb-chat-rail__count">
          {messages.length} {messages.length === 1 ? "turn" : "turns"}
        </span>
      </Stack>

      <ChatMessages
        messages={messages}
        busy={busy}
        onSubmitSurvey={submitSurvey}
        answeredSurveys={answeredSurveys}
        onRegenerate={handleRegenerate}
        onEditUser={handleEditUser}
      />

      {error && (
        <Stack
          data-testid="chat-error"
          className="zb-chat-rail__error"
          direction="row"
          align="center"
          justify="between"
          gap={2}
        >
          <span className="zb-chat-rail__error-msg" title={error.message}>
            {error.message}
          </span>
          <Button
            type="button"
            variant="plain"
            intent="destructive"
            size="small"
            data-testid="chat-retry"
            onClick={handleRetry}
            className="zb-chat-rail__retry"
          >
            ↻ retry
          </Button>
        </Stack>
      )}

      <ChatComposer
        value={input}
        onChange={setInput}
        onSubmit={handleSubmit}
        onStop={() => stop()}
        busy={busy}
        appId={appId}
        placeholder={appName ? `Tell ${appName} what to make.` : "Describe what to make."}
      />
    </Stack>
  );
}
