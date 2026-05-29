"use server";
// PM digest worker — the SCHEDULED-mode half of the PM agent (spec
// §4.8.3.2). Mirrors the dual shape called out in the spec: PM has a
// chat SubAgent (`internal/pm.ts`, dispatched via `task("pm", …)` from
// Builder) plus a background worker that produces periodic digests
// for the project chat thread.
//
// V1 surface: this file exposes ONE RPC procedure (`pmDigest`) that
// an external cron (or, eventually, a control-plane scheduler) hits
// per app at whatever cadence makes sense. The proc is synchronous
// from the caller's POV — it gathers project state, fires the
// SubAgent's model with the digest-shaped prompt once, parses the
// structured response, and returns it. There is no streaming and no
// chat-thread side-effect at this layer; whoever owns the cron is
// responsible for posting the digest back into the thread (that
// avoids a second wire trip and keeps this proc stateless).
//
// Still deferred:
//   - The cron itself. There's no scheduler in `crates/control` yet
//     that polls projects and POSTs to this endpoint. Until that
//     lands, callers are external (curl from a cron, manual hits
//     from the dashboard, e2e tests). The proc just needs to be
//     callable.
//   - Posting the digest into the chat thread. Same wire as Builder's
//     custom data parts (data-pm-recommendation) — needs the chat
//     thread runtime to accept "out-of-band assistant turn" writes.
//
// Why this is not in `internal/`: files re-exported by `server.ts` are
// treated as public RPC modules. The underscore prefix is the project-
// side opt-out convention (see `internal/pm.ts` etc.). This file IS meant to be
// a public RPC endpoint, so it lives without the prefix and gets
// re-exported from `server.ts` deliberately.
//
// Wire convention (single-input object, per the rest of `server/`):
//   POST /_zs/v1/pm.digest
//     body: { json: { appId: string } }
//     response: { summary: string, recommendations: PMRecommendationItem[] }

import { action } from "@zeroship/rpc/server";
import { z } from "zod";

import { PM_PROMPT } from "./internal/prompts";
import { listIssues, getQualityScores } from "./agents";
// `getApp` lives in apps.ts (proxied to the control plane). Keep the
// wire optional — the digest still works without deploy info, so a
// catch() below lets a control-plane outage degrade gracefully.
import { getApp as getAppRecord } from "./apps";

// ─── recommendation shape ────────────────────────────────────────
//
// Reuse the same recommendation-item schema the chat-mode PM SubAgent
// emits (`internal/pm.ts` -> `recommendationSchema`). The digest just produces
// a list of them rather than a single primary + alternatives — the
// caller (cron, dashboard) is in a better position to pick how many
// to surface.
//
// Inlined (rather than imported from `internal/pm.ts`) so this file doesn't
// depend on the SubAgent's internal exports — the SubAgent's
// `pmResponseSchema` is shaped for the chat card, not the digest.
const recommendationItemSchema = z.object({
  /** If present, references an existing Issue id (see agents.ts). */
  issueId: z.string().optional(),
  title: z.string(),
  why: z.string(),
  urgency: z.enum(["low", "medium", "high"]),
});

const pmDigestResponseSchema = z.object({
  /** 1-3 sentence narrative — "what shipped this week, what's next". */
  summary: z.string(),
  /**
   * 1-2 ranked next moves. Order matters: index 0 is the primary
   * recommendation. Capped at 3 so the digest stays readable in a
   * chat post.
   */
  recommendations: z.array(recommendationItemSchema).min(1).max(3),
});

export type PMDigestRecommendationItem = z.infer<typeof recommendationItemSchema>;
export type PMDigest = z.infer<typeof pmDigestResponseSchema>;

export interface PMDigestInput {
  appId: string;
}

const pmDigestInputSchema = z.object({
  appId: z.string().min(1).max(256),
}).strict();

/**
 * Run a PM digest pass over the given app's current state.
 *
 * Flow:
 *   1. Snapshot the project: open issues, quality scorecard, last
 *      deploy hash (best-effort; failures degrade to "no data").
 *   2. Render that snapshot into a digest-shaped user prompt.
 *   3. Invoke the same model the chat-mode PM subagent uses
 *      (`gpt-5.4-mini`) with the `PM_PROMPT` system prompt and
 *      structured-output binding.
 *   4. Return the parsed JSON.
 *
 * No retries on model failure (V1) — the cron will hit again on the
 * next tick. Throws on missing OPENAI_API_KEY so the calling cron
 * sees a real error rather than a silent empty digest.
 */
export const pmDigest = action(async (input: PMDigestInput): Promise<PMDigest> => {
  const apiKey = process.env.OPENAI_API_KEY;
  if (!apiKey) {
    throw new Error(
      "OPENAI_API_KEY is not set. Configure it in apps/zeroship-builder/.env.",
    );
  }

  // Lazy import the langchain stack so non-worker procs don't pay
  // the dep-tree cost on a cold isolate (mirrors translator.ts).
  const { ChatOpenAI } = await import("@langchain/openai");
  const { HumanMessage, SystemMessage } = await import(
    "@langchain/core/messages"
  );

  // Gather context. Each lookup is best-effort — if a stub throws
  // (e.g., control plane unreachable for getApp), we substitute a
  // placeholder line and keep going. The model is told what's missing
  // so its recommendation set reflects the gaps.
  const [issuesResult, scores, appRecord] = await Promise.all([
    Promise.resolve().then(() => listIssues({ appId: input.appId })).catch(() => null),
    Promise.resolve().then(() => getQualityScores({ appId: input.appId })).catch(() => null),
    Promise.resolve().then(() => getAppRecord(input.appId)).catch(() => null),
  ]);

  const contextText = renderProjectContext({
    appId: input.appId,
    issuesResult,
    scores,
    appRecord,
  });

  // Bind structured output. functionCalling mode (vs default
  // jsonSchema strict) — same rationale as `_wizard.ts` at line 170:
  // strict mode rejects `.optional()` without `.nullable()`, and our
  // `recommendationItemSchema.issueId` is `.optional()`.
  const model = new ChatOpenAI({
    model: "gpt-5.4-mini",
    temperature: 0.3,
    apiKey,
  }).withStructuredOutput(pmDigestResponseSchema, {
    name: "pm_digest",
    method: "functionCalling",
  });

  const result = await model.invoke([
    new SystemMessage(PM_PROMPT),
    new HumanMessage(buildDigestPrompt(contextText)),
  ]);

  return result;
}, { id: "pm.digest", input: pmDigestInputSchema, maxInputBytes: 4_096 });

// --- helpers --------------------------------------------------------

function renderProjectContext(args: {
  appId: string;
  issuesResult: { issues: Array<{ id: string; title: string; status: string; source: string; assignee: string | null; updated_at: string }> } | null;
  scores: { overall: string; last_run_at: string | null; dimensions: Array<{ key: string; label: string; grade: string; rationale: string }> } | null;
  appRecord: { id: string; name: string; deploy_hash: string | null; updated_at: string } | null;
}): string {
  const { appId, issuesResult, scores, appRecord } = args;

  const lines: string[] = [];
  lines.push(`Project id: ${appId}`);
  if (appRecord) {
    lines.push(`Project name: ${appRecord.name}`);
    lines.push(
      `Last deploy: ${appRecord.deploy_hash ? appRecord.deploy_hash.slice(0, 12) : "<never deployed>"} (record updated ${appRecord.updated_at})`,
    );
  } else {
    lines.push("Project record: <unavailable — control plane not reachable>");
  }

  lines.push("");
  lines.push("## Issues (open + recent)");
  if (!issuesResult) {
    lines.push("<unavailable>");
  } else if (issuesResult.issues.length === 0) {
    lines.push("<no issues filed yet>");
  } else {
    // Cap at 25 to keep the prompt bounded — PM doesn't need every
    // historical issue to make a "next 1-2" call.
    for (const i of issuesResult.issues.slice(0, 25)) {
      const assignee = i.assignee ?? "unassigned";
      lines.push(
        `- [${i.status}] (${i.source} → ${assignee}) ${i.id}: ${i.title} (updated ${i.updated_at})`,
      );
    }
  }

  lines.push("");
  lines.push("## Quality scorecard");
  if (!scores) {
    lines.push("<unavailable>");
  } else {
    lines.push(`Overall: ${scores.overall} (last run: ${scores.last_run_at ?? "never"})`);
    for (const d of scores.dimensions) {
      lines.push(`- ${d.label}: ${d.grade} — ${d.rationale}`);
    }
  }

  return lines.join("\n");
}

function buildDigestPrompt(contextText: string): string {
  return `You are running in DIGEST mode. Builder is not on the line — this is a periodic background pass that produces a short summary of project momentum plus 1-2 ranked next moves.

Read the project state below. Produce:

1. A 1-3 sentence "summary" capturing what shipped recently and where the project is. Tone: upbeat but honest. If nothing has shipped, say so plainly.

2. 1-3 "recommendations" ranked best-first. Same shape as the chat-mode card: each has { issueId?, title, why, urgency }. Use issueId only when the recommendation maps to an existing open issue listed below. NEVER recommend an item already in "done" status.

If the project state is too thin to recommend ("no issues, no deploys, no scorecard"), recommend the smallest concrete next step (e.g., "Pick the auth model: passwordless email vs. OAuth"). NEVER return an empty recommendations array.

--- PROJECT STATE ---
${contextText}
--- END PROJECT STATE ---`;
}
