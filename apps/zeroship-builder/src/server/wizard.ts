"use server";
// Wizard RPC procedure — `/_zs/v1/wizard`. The pre-coding clarification
// flow per spec §4.8.2b + §8.2.7. Plain-LangGraph backend (NOT
// deepagents) — see `_wizard.ts` for why.
//
// Wire (mirrors chat.ts so the client transport is reusable):
//   POST /_zs/v1/wizard
//     fresh body:   { json: { idea: string, id: string } }
//     resume body:  { json: { resume: { token, value }, id: string } }
//     response:     text/event-stream  (UI Message Stream)
//                   chunks: data-survey* + data-brief (terminal)
//
// The wizard's stream contains ONLY data-* chunks — no text deltas, no
// tool-call chunks. Everything user-visible is rendered from
// data-survey (SurveyCard) and the eventual data-brief (a Begin button
// / brief preview, future client work).

import { createUIMessageStreamResponse, type UIMessage } from "ai";

export async function wizard(
  input: {
    idea?: string;
    id?: string;
    resume?: { token: string; value: unknown };
    // Accepted but ignored — the chatTransport ships `messages` even
    // for fresh wizard turns. The wizard doesn't replay history; the
    // checkpointer keyed by `id` carries state across turns.
    messages?: UIMessage[];
  },
): Promise<Response> {
  const { buildWizardStream } = await import("./_wizard.js");

  // Same AbortController-on-stream-cancel pattern as chat.ts. The
  // kernel RPC fast path doesn't expose request.signal, so we mint
  // our own and abort when the response body is cancelled.
  const ac = new AbortController();
  const stream = await buildWizardStream(input, ac.signal);

  const baseResponse = createUIMessageStreamResponse({ stream });
  if (!baseResponse.body) return baseResponse;

  const wrapped = new ReadableStream({
    async start(controller) {
      const reader = baseResponse.body!.getReader();
      try {
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          controller.enqueue(value);
        }
        controller.close();
      } catch (err) {
        controller.error(err);
      }
    },
    cancel(reason) {
      ac.abort(reason);
    },
  });

  return new Response(wrapped, {
    status: baseResponse.status,
    statusText: baseResponse.statusText,
    headers: baseResponse.headers,
  });
}

wizard.config = { id: "wizard", kind: "mutation" };
