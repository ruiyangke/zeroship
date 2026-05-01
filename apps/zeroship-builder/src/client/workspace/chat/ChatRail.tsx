// Chat rail — desk-side notebook for the builder workspace.
//
// Built on `@ai-sdk/react`'s `useChat` hook against the typed RPC
// procedure `rpc.chat`. The transport (URL + body envelope) is bound
// once in `client/api.ts` via `chatTransport(rpc.chat)`, so this
// component never touches the wire.
//
// Plan 01.5: text-only mock. The visual chrome (header, ChatComposer,
// error band) is unchanged from Plan 01. ChatMessages renders the v6
// `UIMessage[]` shape (parts: [{ type: "text", text }]).
//
// Plan 02 will dispatch custom `data-*` parts (survey, diff,
// critic-round) into the assistant message renderer.

import { useState } from "react";
import { useChat } from "@ai-sdk/react";
import { rpc, chatTransport } from "../../api";
import type { SurveyResponse } from "../../types/chat";
import { ChatComposer } from "./ChatComposer";
import { ChatMessages } from "./ChatMessages";

export interface ChatRailProps {
  appName?: string;
}

export function ChatRail({ appName }: ChatRailProps) {
  const [input, setInput] = useState("");
  // Tokens of surveys the user has already answered or skipped this
  // session. Prevents the SurveyCard from re-firing on re-render after
  // resume, and lets the renderer collapse the card if a previously-
  // answered survey scrolls back into view. Scoped to the ChatRail
  // instance — refresh = clean slate, matching useChat's per-mount
  // message state.
  const [answeredSurveys, setAnsweredSurveys] = useState<Set<string>>(() => new Set());

  const { messages, sendMessage, status, error, stop } = useChat({
    transport: chatTransport(rpc.chat),
    onError: (err) => console.error("[chat]", err),
  });

  const busy = status === "submitted" || status === "streaming";

  function handleSubmit(text: string, _attachments: File[]) {
    if (!text.trim() || busy) return;
    sendMessage({ text });
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
    // `ask_survey` (see _tools.ts): `{ skipped, answers }`. Dropping
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
    <div data-testid="chat-rail" className="flex flex-col h-full bg-paper-2">
      <div className="px-5 pt-4 pb-2 border-b border-rule flex items-baseline justify-between">
        <h3 className="font-display italic font-medium text-base">Notes &amp; thoughts</h3>
        <span className="font-sans text-[10px] uppercase tracking-wider text-pencil">
          {messages.length} {messages.length === 1 ? "turn" : "turns"}
        </span>
      </div>

      <ChatMessages
        messages={messages}
        busy={busy}
        onSubmitSurvey={submitSurvey}
        answeredSurveys={answeredSurveys}
      />

      {error && (
        <div className="px-5 py-2 border-t border-blood/30 bg-blood/5 font-sans text-[12px] text-blood">
          {error.message}
        </div>
      )}

      <ChatComposer
        value={input}
        onChange={setInput}
        onSubmit={handleSubmit}
        onStop={() => stop()}
        busy={busy}
        placeholder={appName ? `Tell ${appName} what to make.` : "Describe what to make."}
      />
    </div>
  );
}
