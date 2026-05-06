"use server";
// Shared survey wire — the parts of the survey contract that BOTH the
// wizard runtime (plain LangGraph) and the Builder runtime (deepagents)
// need. Per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §4.8.2b + §8.2.7: the wire format is runtime-agnostic.
// Anything that ties into deepagents (tool wrapping, middleware) lives
// elsewhere; anything that ties into LangGraph specifically (interrupt,
// StateGraph) lives in the runtime that uses it.
//
// What's here:
//   - surveyInputSchema: Zod schema the LLM produces — used as
//     tool-input schema by Builder's askSurveyTool, and as the
//     structured-output schema by the wizard's clarifier node.
//   - emitDataSurvey: writer.write call for the v6 `data-survey`
//     custom part. Same chunk shape on both runtimes so
//     <SurveyCard> renders identically.
//
// What's NOT here:
//   - interrupt() call — runtime-specific (tool body in Builder, node
//     body in the wizard).
//   - data-brief emit — wizard-only (Builder doesn't produce briefs;
//     it consumes them).
//   - middleware factory — Builder-only (deepagents-specific).

import type { UIMessageStreamWriter } from "ai";
import { z } from "zod";

// Mirror of `optionSchema` in client/types/chat.ts. Keep aligned with
// what <SurveyCard> renders.
const optionSchema = z.object({
  value: z.string(),
  label: z.string(),
  hint: z.string().optional(),
});

// Question kinds the SurveyCard supports today. The full visual taxonomy
// in the spec includes scale / image_upload / multi_choice; we only
// allow the LLM to author kinds we actually render. New kinds: extend
// here AND in client/types/chat.ts AND in SurveyCard.tsx (three-place
// change is intentional — the LLM should only ever produce questions
// the renderer can handle).
const questionKindSchema = z.discriminatedUnion("type", [
  z.object({
    type: z.literal("single_choice"),
    options: z.array(optionSchema).min(2).max(6),
  }),
  z.object({ type: z.literal("yes_no") }),
  z.object({
    type: z.literal("short_text"),
    placeholder: z.string().optional(),
    max_length: z.number().int().min(1).optional(),
  }),
  z.object({
    type: z.literal("long_text"),
    placeholder: z.string().optional(),
    max_length: z.number().int().min(1).optional(),
  }),
]);

const questionSchema = z.object({
  id: z.string().min(1),
  prompt: z.string().min(1),
  kind: questionKindSchema,
  required: z.boolean().optional(),
});

/**
 * The survey shape the LLM produces — same on both runtimes. Cap of
 * 1–3 questions per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §8.2.7. The renderer is also defensive
 * (truncates >3, collapses >6 single_choice options to a dropdown), so
 * malformed surveys still degrade gracefully.
 */
export const surveyInputSchema = z.object({
  preamble: z.string().optional(),
  questions: z.array(questionSchema).min(1).max(3),
  skip_label: z.string().optional(),
});

export type SurveyInput = z.infer<typeof surveyInputSchema>;

/**
 * Write a `data-survey` chunk to a v6 UI Message Stream writer.
 * Returns the token the client must echo back in `body.resume.token`.
 *
 * Token doubles as the chunk id (so v6 dedupes if the same survey is
 * re-emitted on resume — though both runtimes suppress that case
 * explicitly) and as the resume token. Currently opaque on the wire:
 * the server only validates `body.resume.value`, not the token.
 *
 * Caller is responsible for handling the GraphInterrupt / interrupt()
 * call afterwards. This helper just emits — it doesn't halt anything.
 */
export function emitDataSurvey(
  writer: UIMessageStreamWriter,
  survey: SurveyInput,
  token: string = crypto.randomUUID(),
): string {
  try {
    writer.write({
      type: "data-survey",
      id: token,
      data: { token, survey },
    } as Parameters<UIMessageStreamWriter["write"]>[0]);
  } catch {
    // Stream closed (client disconnect). Caller's interrupt() will
    // still halt; the disconnected client won't see anything either
    // way. Swallow so an aborted SSE doesn't fault the agent loop.
  }
  return token;
}
emitDataSurvey.config = { id: "_internal.emitDataSurvey" };
