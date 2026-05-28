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
  reviewerResponseSchema,
  type ReviewerResponse,
} from "./reviewer.js";
import { getControlClient } from "../control-client.js";

interface ToolExecuteResponse {
  output: string;
  exitCode: number | null;
  truncated?: boolean;
}

interface ToolFileDownloadResponse {
  path: string;
  content: Uint8Array | null;
  error: string | null;
}

interface DeploySandboxBackend {
  execute(command: string): Promise<ToolExecuteResponse>;
  downloadFiles(paths: string[]): Promise<ToolFileDownloadResponse[]>;
}

export interface CreateDeployToolOptions {
  backend: DeploySandboxBackend;
  appId?: string | null;
  apiKey?: string;
}

const deployInputSchema = z.object({
  changes: z.string().min(1).describe(
    "Concise description of the current sandbox changes the Builder wants to ship.",
  ),
});

const DEPLOY_ARTIFACT_PATH = "dist/app.zship";

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

const BUILD_ZSHIP_COMMAND = String.raw`set -eu
if [ ! -f package.json ]; then
  echo "package.json not found; zeroship deploys must be built by the app project"
  exit 2
fi

if [ -f pnpm-lock.yaml ] && command -v pnpm >/dev/null 2>&1; then
  pnpm build
elif [ -f bun.lockb ] && command -v bun >/dev/null 2>&1; then
  bun run build
elif [ -f yarn.lock ] && command -v yarn >/dev/null 2>&1; then
  yarn build
else
  npm run build
fi

test -s dist/app.zship`;

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

export function createDeployTool(options: CreateDeployToolOptions) {
  const { backend, appId, apiKey } = options;

  return tool(
    async ({ changes }) => {
      if (!appId) {
        return JSON.stringify({
          error: "deploy requires an appId-scoped Builder workspace",
        });
      }

      const reviewerApiKey = apiKey ?? OPENAI_API_KEY();
      if (!reviewerApiKey) {
        throw new Error(
          "OPENAI_API_KEY is not set. Configure it in the Builder app env.",
        );
      }

      const control = getControlClient();
      const snapshot = await collectReviewSnapshot(backend);
      const review = await runReviewerGate({
        apiKey: reviewerApiKey,
        changes,
        snapshot,
      });

      if (!review.approved) {
        return JSON.stringify({
          blocked: true,
          reviewer_approved: false,
          blockers: review.blockers,
        });
      }

      const build = await backend.execute(BUILD_ZSHIP_COMMAND);
      const buildExitCode = build.exitCode ?? -1;
      if (buildExitCode !== 0) {
        return JSON.stringify({
          blocked: true,
          reason: "build_failed",
          build: {
            exit_code: buildExitCode,
            output: capText(build.output, 12_000),
          },
        });
      }

      const artifact = await downloadArtifact(backend, DEPLOY_ARTIFACT_PATH);
      const appRecord = await control.getApp(appId).catch(() => null);
      const deploy = await control.deploy(appId, artifact);
      const name = appRecord?.name ?? appId;

      return JSON.stringify({
        url: `/apps/${encodeURIComponent(name)}/`,
        app_id: appId,
        app_name: appRecord?.name ?? null,
        deploy_hash: deploy.deploy_hash,
        blobs_uploaded: deploy.blobs_uploaded ?? null,
        blobs_deduped: deploy.blobs_deduped ?? null,
        reviewer_approved: true,
      });
    },
    {
      name: "deploy",
      description:
        "Deploy the current sandbox app to zeroship. This tool is the only " +
        "deploy path: it snapshots the sandbox change, invokes the Reviewer " +
        "model as a hard gate, blocks on approved=false, then runs the " +
        "sandbox build and uploads dist/app.zship to the real control-plane " +
        "/api/apps/{id}/deploy endpoint. Returns JSON with { url, " +
        "deploy_hash } on success or { blocked: true, blockers } when " +
        "Reviewer refuses the deploy.",
      schema: deployInputSchema,
    },
  );
}

async function collectReviewSnapshot(backend: DeploySandboxBackend): Promise<string> {
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

  return reviewerModel.invoke([
    new SystemMessage(REVIEWER_PROMPT),
    new HumanMessage(
      [
        "Review this deploy candidate. Return approved=false for any blocker.",
        "",
        "## Builder change summary",
        args.changes,
        "",
        "## Sandbox source snapshot",
        args.snapshot,
      ].join("\n"),
    ),
  ]);
}

async function downloadArtifact(
  backend: DeploySandboxBackend,
  path: string,
): Promise<Uint8Array> {
  const [artifact] = await backend.downloadFiles([path]);
  if (!artifact || artifact.error || !artifact.content) {
    throw new Error(
      `deploy artifact ${path} unavailable: ${artifact?.error ?? "empty"}`,
    );
  }
  return artifact.content;
}

function capText(value: string, max: number): string {
  if (value.length <= max) return value;
  return `${value.slice(0, max)}\n...[truncated ${value.length - max} chars]`;
}
