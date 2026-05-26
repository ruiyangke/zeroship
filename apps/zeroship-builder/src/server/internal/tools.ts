"use server";
// Server-side custom tools that aren't covered by deepagents' built-in
// fs/exec set. The ask_survey tool is the first —
// when the Builder needs structured clarification (single-/multi-choice
// or short-text answers, max 3 questions per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §8.2.7), it calls
// `ask_survey` and the run halts via `interrupt()`. The client renders
// a SurveyCard from the `data-survey` chunk emitted by `middleware.ts`,
// the user submits, and the resume protocol (`Command({resume})` server-
// side, `body.resume` client-side) feeds the answer back into the
// interrupted node — `interrupt(...)` returns the resume value, which
// becomes the tool's result and goes to the LLM as a ToolMessage.
//
// Why interrupt + tool, not interruptOn + accept/edit/respond? interrupt
// gives us a clean "resume returns the answer as the tool result" wire,
// where the answer's shape is whatever we put in the resume value. The
// HumanInTheLoopMiddleware (interruptOn) shape forces a fixed
// accept/edit/respond verb that doesn't match a survey response.
//
// Tool schema = the survey definition itself. The LLM constructs a
// Survey ({ preamble, questions, skip_label? }), the middleware emits
// it as `data-survey`, the SurveyCard renders, the answer comes back as
// `Record<question_id, value>` via Command({resume}). Keep this shape in
// sync with `apps/zeroship-builder/src/client/types/chat.ts` (the client
// renders the same survey schema; the resume value mirrors
// SurveyResponse.answers).

import { tool } from "@langchain/core/tools";
import { interrupt } from "@langchain/langgraph";

// Schema is shared with the wizard runtime — see `survey-wire.ts` for
// why and the cross-runtime contract.
import { surveyInputSchema } from "./survey-wire.js";

export const askSurveyTool = tool(
  // The LLM's args ARE the survey definition. interrupt() halts the
  // graph; on resume it returns whatever value the client sent in
  // Command({resume: ...}). We stringify because LangChain wraps the
  // return value in a ToolMessage whose `content` is a string.
  async (survey) => {
    const answers = interrupt({ kind: "ask_survey", survey });
    return JSON.stringify({ answers });
  },
  {
    name: "ask_survey",
    description:
      "Ask the user 1-3 short clarifying questions before building. " +
      "Use ONLY when missing information would force a guess that " +
      "could waste a build cycle (e.g., target platform, auth model, " +
      "data shape). Each question has an `id` (referenced in the " +
      "answer payload), a `prompt`, and a `kind` describing the " +
      "answer shape (single_choice / yes_no / short_text / long_text). " +
      "Returns: JSON with `answers: { <question_id>: <value> }` once " +
      "the user submits, or `answers: { skipped: true }` if they skip.",
    schema: surveyInputSchema,
  },
);
