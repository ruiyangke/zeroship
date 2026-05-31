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
import { z } from "zod";

// Schema is shared with the wizard runtime — see `survey-wire.ts` for
// why and the cross-runtime contract.
import { surveyInputSchema } from "./survey-wire.js";
import { OPENAI_API_KEY } from "./env.js";
import { REVIEWER_PROMPT } from "./prompts.js";
import {
  normalizeReviewerGate,
  reviewerResponseSchema,
  type ReviewerResponse,
} from "./reviewer.js";

interface ToolExecuteResponse {
  output: string;
  exitCode: number | null;
  truncated?: boolean;
}

// The review tool only needs to snapshot the sandbox source — it reads
// nothing back as bytes and writes nothing. (The console is a pure
// creator app: no deploy artifact, no control plane.)
interface ReviewSandboxBackend {
  execute(command: string): Promise<ToolExecuteResponse>;
}

export interface CreateReviewToolOptions {
  backend: ReviewSandboxBackend;
  apiKey?: string;
}

const reviewInputSchema = z.object({
  changes: z.string().min(1).describe(
    "Concise description of the current sandbox changes to review.",
  ),
});

const SOURCE_SNAPSHOT_COMMAND = String.raw`set -eu
printf '## file tree\n'
find . \
  -path './node_modules' -prune -o \
  -path './dist' -prune -o \
  -path './.git' -prune -o \
  -path './.zeroship' -prune -o \
  -path './coverage' -prune -o \
  -type f \
  \( -name '*.ts' -o -name '*.tsx' -o -name '*.js' -o -name '*.jsx' -o -name '*.mjs' -o -name '*.cjs' -o -name '*.json' -o -name '*.html' -o -name '*.css' -o -name '*.md' -o -name '*.sql' -o -name '*.env' -o -name '.env*' \) \
  -size -65536c \
  -print | sort | head -80
printf '\n## file excerpts\n'
find . \
  -path './node_modules' -prune -o \
  -path './dist' -prune -o \
  -path './.git' -prune -o \
  -path './.zeroship' -prune -o \
  -path './coverage' -prune -o \
  -type f \
  \( -name '*.ts' -o -name '*.tsx' -o -name '*.js' -o -name '*.jsx' -o -name '*.mjs' -o -name '*.cjs' -o -name '*.json' -o -name '*.html' -o -name '*.css' -o -name '*.md' -o -name '*.sql' \) \
  -size -65536c \
  -print | sort | head -40 | while IFS= read -r file; do
    printf '\n--- %s ---\n' "$file"
    sed -n '1,220p' "$file" || true
  done`;

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

// Standalone REVIEW tool. The console is a PURE creator app: there is
// no deploy, no .zship build, no control plane. This tool snapshots the
// sandbox source, runs the Reviewer model over it, and returns the
// findings. It ships NOTHING — no build, no artifact download, no
// upload. See docs/superpowers/specs/2026-05-31-console-pure-creator-app-design.md §2.
export function createReviewTool(options: CreateReviewToolOptions) {
  const { backend, apiKey } = options;

  return tool(
    async ({ changes }) => {
      const reviewerApiKey = apiKey ?? OPENAI_API_KEY();
      if (!reviewerApiKey) {
        throw new Error(
          "OPENAI_API_KEY is not set. Configure it in the Builder app env.",
        );
      }

      const snapshot = await collectReviewSnapshot(backend);
      const review = await runReviewerGate({
        apiKey: reviewerApiKey,
        changes,
        snapshot,
      });

      // Return the findings only. `approved` reflects whether the
      // change clears the reviewer's hard-gate bar; `blockers` carries
      // every finding (the client renders them with severities). No
      // deploy side-effect is performed regardless of the verdict.
      return JSON.stringify({
        reviewer_approved: review.approved,
        blockers: review.blockers,
      });
    },
    {
      name: "review",
      description:
        "Review the current sandbox app for quality and safety. Snapshots " +
        "the sandbox source and invokes the Reviewer model, returning its " +
        "findings: { reviewer_approved, blockers: [{kind, severity, why, " +
        "fix?}] }. This is a QUALITY tool — it ships nothing (no build, no " +
        "deploy). Use it before handing the app back to the user, or when " +
        "the user asks for a review.",
      schema: reviewInputSchema,
    },
  );
}

async function collectReviewSnapshot(backend: ReviewSandboxBackend): Promise<string> {
  const result = await backend.execute(SOURCE_SNAPSHOT_COMMAND);
  const exitCode = result.exitCode ?? -1;
  const status = exitCode === 0 ? "ok" : `exit ${exitCode}`;
  return capText(
    `Snapshot command status: ${status}\n\n${result.output}`,
    45_000,
  );
}

async function runReviewerGate(args: {
  apiKey: string;
  changes: string;
  snapshot: string;
}): Promise<ReviewerResponse> {
  const { ChatOpenAI } = await import("@langchain/openai");
  const { HumanMessage, SystemMessage } = await import(
    "@langchain/core/messages"
  );

  const reviewerModel = new ChatOpenAI({
    model: "gpt-5.4-mini",
    temperature: 0.2,
    apiKey: args.apiKey,
  }).withStructuredOutput(reviewerResponseSchema, {
    name: "reviewer_gate",
    method: "functionCalling",
  });

  const review = await reviewerModel.invoke([
    new SystemMessage(REVIEWER_PROMPT),
    new HumanMessage(
      [
        "Review this change candidate. Return approved=false only for high or critical blockers; return low/medium findings as warnings.",
        "",
        "## Builder change summary",
        args.changes,
        "",
        "## Sandbox source snapshot",
        args.snapshot,
      ].join("\n"),
    ),
  ]);
  return normalizeReviewerGate(review);
}

function capText(value: string, max: number): string {
  if (value.length <= max) return value;
  return `${value.slice(0, max)}\n...[truncated ${value.length - max} chars]`;
}
