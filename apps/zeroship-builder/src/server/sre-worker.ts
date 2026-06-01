"use server";
// SRE monitor worker — the SCHEDULED-mode half of the SRE agent (spec
// §4.8.3.2). Mirrors the dual-shape pattern: SRE has a chat SubAgent
// (`internal/sre.ts`, dispatched via `task("sre", …)` from Builder) plus a
// background worker that scans logs + perf for concerning signals on
// a cron.
//
// Current surface: ONE RPC procedure (`sreMonitor`) that an external
// cron (or a future control-plane scheduler) hits per app.
// Synchronous from the caller's POV — gathers the recent log slice,
// fires the SubAgent's model with a monitor-shaped prompt once,
// parses the structured response, returns it. No streaming and no
// chat-thread side-effect at this layer; the cron is responsible for
// pushing significant findings back to the project chat.
//
// Still deferred:
//   - The cron itself (no scheduler in `crates/control` yet).
//   - Posting findings into the chat thread as data-sre-finding
//     parts.
//   - Real perf data. For now perf context is a placeholder string so
//     the prompt structure is in place when the real metrics pipeline
//     lands.
//
// Naming (no underscore prefix): files re-exported by `server.ts` get
// their exports registered as public RPC procedures. This file IS meant
// to be public; the underscore opt-out is reserved for internal helpers
// (`internal/sre.ts`, translator helpers, etc.).
//
// Wire convention (single-input object):
//   POST /__zeroship/v1/sre.monitor
//     body: { json: { appId: string } }
//     response: { findings: SREFindingItem[] }

import { action } from "@zeroship/rpc/server";
import { z } from "zod";

import { SRE_PROMPT } from "./internal/prompts";
import { getLogs } from "./projects";

// ─── finding shape ───────────────────────────────────────────────
//
// The chat-mode SRE SubAgent (`internal/sre.ts`) returns a SINGLE diagnosis
// per turn (one card per `task("sre", …)` call). The monitor pass is
// different: it scans a window and may surface 0..N independent
// findings. Each finding mirrors the SubAgent's response shape (so
// the client renderer can reuse the same card component) but the
// envelope is an array.
//
// Mirror of `client/types/chat.ts → sreFindingSchema`. Inlined so this
// worker doesn't depend on the client-side schema module.
const sreFindingItemSchema = z.object({
  diagnosis: z.string(),
  severity: z.enum(["info", "warning", "error", "critical"]),
  recommendation: z.string(),
  related_logs: z
    .array(
      z.object({
        source: z.string(),
        excerpt: z.string(),
      }),
    )
    .max(5)
    .optional(),
});

const sreMonitorResponseSchema = z.object({
  /**
   * 0..5 findings, severity-ranked (critical first). Empty array means
   * "all quiet" — nothing concerning in the window. The cron can use
   * `findings.length === 0` as a "skip the chat post" signal.
   */
  findings: z.array(sreFindingItemSchema).max(5),
});

export type SREFindingItem = z.infer<typeof sreFindingItemSchema>;
export type SREMonitorResult = z.infer<typeof sreMonitorResponseSchema>;

export interface SREMonitorInput {
  appId: string;
}

const sreMonitorInputSchema = z.object({
  appId: z.string().min(1).max(256),
}).strict();

/**
 * Run an SRE monitor pass over the given app's recent logs + perf.
 *
 * Flow:
 *   1. Pull the last log slice via `getLogs` (tails the sandbox's
 *      `.zeroship/dev.log`). Real "last hour" windowing isn't available
 *      in a preview sandbox — we take what `getLogs` returns and tell
 *      the model that's the available window.
 *   2. Synthesize a perf-context line. Until the real metrics pipeline
 *      lands, this is a placeholder so the prompt is structurally
 *      complete.
 *   3. Invoke the same model the chat-mode SRE subagent uses
 *      (`gpt-5.4-mini`) with `SRE_PROMPT` plus structured output
 *      binding. The prompt steers the model toward
 *      multi-finding output (vs the single-card chat shape).
 *   4. Return the parsed JSON.
 *
 * No retries (V1) — the cron will hit again. Throws on missing
 * OPENAI_API_KEY so the caller sees a real error rather than a silent
 * empty findings array.
 */
export const sreMonitor = action(async (
  input: SREMonitorInput,
): Promise<SREMonitorResult> => {
  const apiKey = process.env.OPENAI_API_KEY;
  if (!apiKey) {
    throw new Error(
      "OPENAI_API_KEY is not set. Configure it in apps/zeroship-builder/.env.",
    );
  }

  const { ChatOpenAI } = await import("@langchain/openai");
  const { HumanMessage, SystemMessage } = await import(
    "@langchain/core/messages"
  );

  // Best-effort log fetch — a sandbox that never started the preview
  // (no `.zeroship/dev.log`) degrades to "no logs available" rather
  // than throwing. The model is told what's missing so it returns an
  // empty findings array (or an info-severity "insufficient data"
  // finding) instead of guessing.
  const logs = await Promise.resolve()
    .then(() => getLogs(input.appId))
    .catch(() => null);

  const contextText = renderHealthContext({
    appId: input.appId,
    logs,
    // Placeholder until the performance metrics pipeline is wired. The
    // shape stays the same (a labelled block) so swapping in real
    // p50/p95/error-rate numbers is a one-line change later.
    perfNote: "Performance metrics: <pipeline not wired yet>",
  });

  // functionCalling vs jsonSchema strict — same rationale as
  // pm-worker.ts: `related_logs` is `.optional()` and strict mode
  // would reject without `.nullable()`.
  const model = new ChatOpenAI({
    model: "gpt-5.4-mini",
    temperature: 0.2,
    apiKey,
  }).withStructuredOutput(sreMonitorResponseSchema, {
    name: "sre_monitor",
    method: "functionCalling",
  });

  const result = await model.invoke([
    new SystemMessage(SRE_PROMPT),
    new HumanMessage(buildMonitorPrompt(contextText)),
  ]);

  return result;
}, { id: "sre.monitor", input: sreMonitorInputSchema, maxInputBytes: 4_096 });

// --- helpers --------------------------------------------------------

function renderHealthContext(args: {
  appId: string;
  logs: string[] | null;
  perfNote: string;
}): string {
  const { appId, logs, perfNote } = args;

  const lines: string[] = [];
  lines.push(`Project id: ${appId}`);
  lines.push("");
  lines.push("## Recent logs");
  if (!logs) {
    lines.push("<unavailable — control plane not reachable>");
  } else if (logs.length === 0) {
    lines.push("<no logs in window>");
  } else {
    // Cap at 200 lines to keep the prompt bounded. SRE looks for
    // patterns, not history; a longer slice doesn't help and can crowd
    // the model's attention budget.
    const slice = logs.slice(-200);
    for (const line of slice) {
      lines.push(line);
    }
  }

  lines.push("");
  lines.push(`## Perf snapshot`);
  lines.push(perfNote);

  return lines.join("\n");
}

function buildMonitorPrompt(contextText: string): string {
  return `You are running in MONITOR mode. Builder is not on the line — this is a scheduled pass that scans the most recent log slice + perf data for concerning signals.

Read the project health snapshot below. Produce a "findings" array, ranked critical-first:

- 0 findings → all quiet. Return { findings: [] }.
- 1..N findings → one entry per independent issue. Each entry has the same shape as the chat-mode SRE card: { diagnosis, severity, recommendation, related_logs? }.
- DO NOT invent issues to look productive. An empty findings array is the right answer when nothing concerning is in the window.
- DO NOT collapse two genuinely distinct problems into one finding. They get different cards in the cron's chat post.
- Severity ramp matches the chat SubAgent: info / warning / error / critical.

If the evidence is insufficient (no logs available, perf pipeline not wired), return AT MOST ONE finding with severity "info" describing what would be needed to monitor this app properly. NEVER guess from nothing.

--- HEALTH SNAPSHOT ---
${contextText}
--- END HEALTH SNAPSHOT ---`;
}
