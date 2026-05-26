"use server";
// Data-part emitter middleware.
//
// deepagents/LangChain runs every tool call through `wrapToolCall`
// hooks installed via `createMiddleware`. We sit in that seam and emit
// AI SDK v6 custom data parts (`data-diff` for now; survey/critic-round
// will follow as those tools land) on top of whatever the tool itself
// returns.
//
// Why a closure factory and not a singleton? `createUIMessageStream`
// hands the v6 writer to its `execute` callback at request time — the
// writer is per-request, so the middleware that captures it must also
// be per-request. The translator builds one `dataPartMiddleware(writer)`
// per chat turn, inside the `execute({writer})` closure, and feeds the
// result into `createDeepAgent`.
//
// Tool taxonomy:
//   write_file, edit_file → emit `data-diff` (custom data part) here.
//                           The translator's streamEvents loop SKIPS
//                           emitting native tool-call chunks for these
//                           same names so the wire shows one card, not
//                           two.
//   ask_survey            → emit `data-survey` (custom data part). The
//                           tool body calls `interrupt()` from
//                           langgraph, which halts the run. The
//                           translator catches the resulting
//                           GraphInterrupt and closes the SSE cleanly;
//                           client renders the SurveyCard from the
//                           data-survey chunk and submits via the
//                           resume protocol (body.resume).
//   task (subagent)       → emit per-subagent_type custom data parts
//                           after the SubAgent returns. Builder is
//                           told (via BUILDER_SYSTEM) when to call
//                           which subagent; we route on
//                           `args.subagent_type` to the matching
//                           wire shape:
//                             critic   → data-critic-round
//                             reviewer → data-reviewer-round
//                             pm       → data-pm-recommendation
//                             sre      → data-sre-finding
//                           Each extracts the structured response from
//                           the Command result the SubAgent returns
//                           (it's JSON-stringified on the ToolMessage
//                           because each SubAgent has `responseFormat`
//                           set). Unknown subagent types pass through
//                           silently — the LLM still gets the raw
//                           ToolMessage so the conversation isn't
//                           broken, just no card.
//   ls, read_file, grep,
//   glob, execute, …      → middleware passes through; native
//                           tool-input-available / tool-output-available
//                           chunks are emitted by the translator from
//                           on_tool_start / on_tool_end events.
//
// "before" content for diffs currently ships as `before=""`. To fetch
// the on-disk file we'd need backend access here; deepagents doesn't
// currently expose the backend instance through the tool-call request.
// If the backend is later injected through the context schema, this can
// show real before/after diffs. For now the DiffCard renders an
// "all added" diff for `write_file`, which matches the user experience
// of seeing a freshly-written file.

import type { AgentMiddleware } from "langchain";
import type { UIMessageStreamWriter } from "ai";
// `waitUntil` from the zeroship runtime extends the request's lifetime
// past the SSE close so a fire-and-forget side-effect (writing the
// quality scorecard to KV) finishes before the worker prunes the task.
// Outside the V8 runtime (SDK unit tests) this is a no-op shim — see
// `sdks/zeroship-stub/index.js`.
import { waitUntil } from "zeroship";

// emit helper is shared with the wizard runtime (per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §4.8.2b /
// §8.2.7). The wizard calls it directly from its node body; Builder
// goes through this middleware. Same chunk shape on the wire.
import { emitDataSurvey, type SurveyInput } from "./_survey_wire.js";
// Quality-scoreboard updater. Lives in `_agent_writes.ts`
// (underscore-prefixed) so it stays out of the public RPC surface —
// only the server middleware writes here.
import { setQualityFromCritic } from "./_agent_writes.js";

/**
 * Build a middleware bound to a specific v6 stream writer. The middleware
 * is single-use per request — never share across turns.
 *
 * `isResume` toggles data-survey emission off on resumed runs. When
 * langgraph resumes from an interrupt, it REPLAYS the interrupted node
 * from scratch — so the ask_survey tool body re-enters and our
 * `wrapToolCall` hook runs again. Emitting the data-survey a second
 * time would re-render the SurveyCard on the client even though the
 * user already answered. We suppress emission in resume mode; the
 * client SurveyCard for that token is already collapsed (see
 * ChatRail's answeredSurveys set), so even a stray emit would be
 * filtered visually — but the wire stays clean if we don't emit at
 * all. Other custom data parts (data-diff for write_file/edit_file)
 * are idempotent at the visual layer (each write is a fresh diff with
 * a fresh id) so they're safe to re-emit on resume.
 *
 * Per-turn critic-round counter: each Builder turn instantiates one
 * `dataPartMiddleware(writer)` call, so the closure-captured counter
 * is naturally turn-scoped. We increment on every `task("critic", …)`
 * dispatch and emit `{round, total}` where `total` is the rolling
 * count (the true total isn't known until the turn ends). Client
 * renders whatever shows up.
 */
export async function dataPartMiddleware(
  writer: UIMessageStreamWriter,
  options: { isResume?: boolean; appId?: string } = {},
): Promise<AgentMiddleware> {
  const { isResume = false, appId } = options;
  // Lazy import to keep non-chat code paths free of the langchain
  // dep tree (matches the translator's lazy-import policy).
  const { createMiddleware } = await import("langchain");

  // Per-turn critic round counter. Closure-scoped so it's reset
  // automatically on the next chat turn (the translator constructs
  // a fresh middleware per `execute({writer})`).
  let criticRound = 0;

  return createMiddleware({
    name: "ZeroshipDataPartEmitter",
    wrapToolCall: async (request, handler) => {
      const name = request.toolCall.name;
      const args = (request.toolCall.args ?? {}) as Record<string, unknown>;

      if (name === "write_file") {
        // Path arg is `file_path` (deepagents' built-in tool — see
        // node_modules/deepagents/dist/index.d.ts:2818-2829). Be
        // defensive: also accept `path` in case a custom override is
        // wired later.
        const path =
          (typeof args.file_path === "string" && args.file_path) ||
          (typeof args.path === "string" && args.path) ||
          "";
        const content = typeof args.content === "string" ? args.content : "";
        // Run the tool first so a failure is visible to the LLM via
        // the normal ToolMessage path. Only emit the diff card on
        // success — there's nothing visual to show for a failed write.
        const result = await handler(request);
        try {
          writer.write({
            type: "data-diff",
            id: crypto.randomUUID(),
            data: { path, before: "", after: content },
          } as Parameters<UIMessageStreamWriter["write"]>[0]);
        } catch {
          // Stream may be closed (client disconnect) — swallow; the
          // tool call result still propagates to the LLM via the
          // returned ToolMessage.
        }
        return result;
      }

      if (name === "edit_file") {
        // edit_file args: { file_path, old_string, new_string, replace_all }
        const path =
          (typeof args.file_path === "string" && args.file_path) ||
          (typeof args.path === "string" && args.path) ||
          "";
        const before = typeof args.old_string === "string" ? args.old_string : "";
        const after = typeof args.new_string === "string" ? args.new_string : "";
        const result = await handler(request);
        try {
          writer.write({
            type: "data-diff",
            id: crypto.randomUUID(),
            data: { path, before, after },
          } as Parameters<UIMessageStreamWriter["write"]>[0]);
        } catch {
          // see write_file branch
        }
        return result;
      }

      if (name === "task") {
        // deepagents' built-in subagent dispatcher. Args shape is
        //   { description: string, subagent_type: string }
        // — see node_modules/deepagents/dist/index.js (createTaskTool).
        // We pass-through to the handler then route on subagent_type
        // to the matching custom data part.
        //
        // Result shape (from returnCommandWithStateUpdate, same file):
        //   Command({ update: { messages: [ToolMessage({
        //     content: <JSON-stringified structuredResponse> | <text>
        //   })] } })
        // — when the subagent has a `responseFormat` (all four do),
        // the ToolMessage.content is the JSON-stringified Zod-validated
        // response. Otherwise it's the last message's text content.
        const result = await handler(request);
        const subagentType =
          typeof args.subagent_type === "string" ? args.subagent_type : "";
        const parsed = extractSubagentJson(result);

        if (subagentType === "critic") {
          // Always increment the round counter so the sequence is
          // contiguous even if a Critic call returned malformed JSON.
          const n = ++criticRound;
          const round = parsed
            ? normaliseCriticRound(parsed, n)
            : {
                round: n,
                total: n,
                approved: true,
                issues: [] as { dimension: string; severity: string; note: string }[],
              };
          if (round) {
            try {
              writer.write({
                type: "data-critic-round",
                id: crypto.randomUUID(),
                data: round,
              } as Parameters<UIMessageStreamWriter["write"]>[0]);
            } catch {
              // Stream closed — drop. The Command still propagates back
              // to the LLM as a ToolMessage so Builder can react.
            }
            // Persist the Critic-graded scorecard into the per-app KV
            // slot so HealthCanvas reflects the live grades on its
            // next refetch. Fire-and-forget via
            // waitUntil() so the SSE stream isn't held open by the KV
            // round-trip — the user-visible part is the chat receipt
            // already emitted above.
            if (appId) {
              try {
                waitUntil(
                  setQualityFromCritic(appId, round.issues).catch((e) => {
                    console.warn(
                      `[zeroship:_middleware] setQualityFromCritic threw: ${
                        e instanceof Error ? e.message : String(e)
                      }`,
                    );
                  }),
                );
              } catch {
                // waitUntil unavailable (test stub throws on missing
                // request scope) — drop the side-effect; the scorecard
                // just stays at its previous value.
              }
            }
          }
        } else if (subagentType === "reviewer") {
          const data = parsed ? normaliseReviewerRound(parsed) : null;
          if (data) {
            try {
              writer.write({
                type: "data-reviewer-round",
                id: crypto.randomUUID(),
                data,
              } as Parameters<UIMessageStreamWriter["write"]>[0]);
            } catch {
              // Stream closed — drop.
            }
          }
        } else if (subagentType === "pm") {
          const data = parsed ? normalisePMRecommendation(parsed) : null;
          if (data) {
            try {
              writer.write({
                type: "data-pm-recommendation",
                id: crypto.randomUUID(),
                data,
              } as Parameters<UIMessageStreamWriter["write"]>[0]);
            } catch {
              // Stream closed — drop.
            }
          }
        } else if (subagentType === "sre") {
          const data = parsed ? normaliseSREFinding(parsed) : null;
          if (data) {
            try {
              writer.write({
                type: "data-sre-finding",
                id: crypto.randomUUID(),
                data,
              } as Parameters<UIMessageStreamWriter["write"]>[0]);
            } catch {
              // Stream closed — drop.
            }
          }
        }
        // Unknown subagent_type → no card. The Command still propagates
        // back to the LLM as a ToolMessage so the conversation isn't
        // broken; we just skip the visual receipt.
        return result;
      }

      if (name === "ask_survey") {
        // Args ARE the survey definition (validated upstream by the
        // tool's surveyInputSchema). Emit the data-survey chunk
        // BEFORE calling handler — the handler runs the tool body
        // which calls interrupt() and throws GraphInterrupt; the
        // pre-emit guarantees the client sees the survey before the
        // SSE stream closes.
        //
        // Skip emission in resume mode — see the comment on isResume
        // above. interrupt() inside the tool will return the resume
        // value instead of throwing, so handler() resolves normally
        // and the LLM gets the answer as a ToolMessage.
        if (!isResume) {
          emitDataSurvey(writer, args as SurveyInput);
        }
        return handler(request);
      }

      // All other tools fall through; the translator's streamEvents
      // switch turns them into native v6 tool-call chunks.
      return handler(request);
    },
  });
}

// Extract the JSON-decoded subagent response from the Command the
// `task` tool returns. Each SubAgent in the fleet has a `responseFormat`
// set, so deepagents wraps the structured result in a ToolMessage whose
// `.content` is a JSON-stringified Zod-validated object. We probe the
// Command-with-state-update shape AND the bare ToolMessage shape so a
// future deepagents change doesn't silently break us.
//
// Returns null on any decode failure. Per-agent normalisers below
// decide what "best-effort" means when the shape doesn't match — for
// Critic that's a synthesised "approved" round, for the others it's
// "no card".
function extractSubagentJson(result: unknown): Record<string, unknown> | null {
  let content: unknown = null;
  if (result && typeof result === "object") {
    const r = result as Record<string, unknown>;
    // Command shape: r.update.messages[last].content
    const update = r.update as Record<string, unknown> | undefined;
    if (update && Array.isArray(update.messages) && update.messages.length > 0) {
      const lastMsg = update.messages[update.messages.length - 1] as
        | { content?: unknown; kwargs?: { content?: unknown } }
        | undefined;
      content = lastMsg?.content ?? lastMsg?.kwargs?.content ?? null;
    }
    // Fallback: bare ToolMessage (r.content) or serialised
    // ({lc, type:"constructor", kwargs:{content}}).
    if (content == null) {
      content = r.content ?? null;
      if (content == null && r.kwargs && typeof r.kwargs === "object") {
        content = (r.kwargs as Record<string, unknown>).content ?? null;
      }
    }
  }

  if (typeof content !== "string" || content.length === 0) return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(content);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) {
    return null;
  }
  return parsed as Record<string, unknown>;
}

// Critic's wire shape: { round, total, approved, issues: [{dimension,
// severity, note}] }. The round counter is provided by the caller (the
// per-turn closure variable in `dataPartMiddleware`) so we don't need
// to track it here. Folds Critic's `issue` + `suggested_fix` into a
// single `note` string for the badge.
function normaliseCriticRound(
  p: Record<string, unknown>,
  round: number,
): {
  round: number;
  total: number;
  approved: boolean;
  issues: { dimension: string; severity: string; note: string }[];
} {
  const approved = typeof p.approved === "boolean" ? p.approved : false;
  const rawIssues = Array.isArray(p.issues) ? p.issues : [];
  const issues = rawIssues
    .map((iss): { dimension: string; severity: string; note: string } | null => {
      if (!iss || typeof iss !== "object") return null;
      const i = iss as Record<string, unknown>;
      const dimension = typeof i.dimension === "string" ? i.dimension : "";
      const severity = typeof i.severity === "string" ? i.severity : "low";
      const issueText = typeof i.issue === "string" ? i.issue : "";
      const fixText =
        typeof i.suggested_fix === "string" ? i.suggested_fix : "";
      const note = fixText ? `${issueText} → ${fixText}` : issueText;
      if (!dimension && !note) return null;
      return { dimension, severity, note };
    })
    .filter(
      (x): x is { dimension: string; severity: string; note: string } =>
        x != null,
    );
  return { round, total: round, approved, issues };
}

// Reviewer's wire shape: { approved, blockers: [{kind, severity, why,
// fix?}] }. Maps 1:1 onto the SubAgent's responseFormat (apart from
// dropping unknown blockers).
function normaliseReviewerRound(
  p: Record<string, unknown>,
): {
  approved: boolean;
  blockers: { kind: string; severity: string; why: string; fix?: string }[];
} | null {
  const approved = typeof p.approved === "boolean" ? p.approved : false;
  const rawBlockers = Array.isArray(p.blockers) ? p.blockers : [];
  const blockers = rawBlockers
    .map(
      (
        b,
      ): { kind: string; severity: string; why: string; fix?: string } | null => {
        if (!b || typeof b !== "object") return null;
        const o = b as Record<string, unknown>;
        const kind = typeof o.kind === "string" ? o.kind : "";
        const severity = typeof o.severity === "string" ? o.severity : "low";
        const why = typeof o.why === "string" ? o.why : "";
        const fix = typeof o.fix === "string" ? o.fix : undefined;
        if (!kind && !why) return null;
        return fix ? { kind, severity, why, fix } : { kind, severity, why };
      },
    )
    .filter(
      (x): x is { kind: string; severity: string; why: string; fix?: string } =>
        x != null,
    );
  // approved=false with no blockers is still a valid render — the card
  // just shows the headline. approved=true with blockers is also OK
  // (Reviewer flagged low-severity stuff but didn't block).
  return { approved, blockers };
}

// PM's wire shape: { recommendation, alternatives }. Each recommendation
// is { issueId?, title, why, urgency }.
function normalisePMRecommendation(
  p: Record<string, unknown>,
): {
  recommendation: { issueId?: string; title: string; why: string; urgency: string };
  alternatives: { issueId?: string; title: string; why: string; urgency: string }[];
} | null {
  const rec = normaliseRecommendation(p.recommendation);
  if (!rec) return null;
  const rawAlts = Array.isArray(p.alternatives) ? p.alternatives : [];
  const alternatives = rawAlts
    .map(normaliseRecommendation)
    .filter(
      (
        x,
      ): x is { issueId?: string; title: string; why: string; urgency: string } =>
        x != null,
    )
    .slice(0, 2);
  return { recommendation: rec, alternatives };
}

function normaliseRecommendation(
  v: unknown,
):
  | { issueId?: string; title: string; why: string; urgency: string }
  | null {
  if (!v || typeof v !== "object") return null;
  const r = v as Record<string, unknown>;
  const title = typeof r.title === "string" ? r.title : "";
  const why = typeof r.why === "string" ? r.why : "";
  const urgency = typeof r.urgency === "string" ? r.urgency : "medium";
  const issueId = typeof r.issueId === "string" ? r.issueId : undefined;
  if (!title && !why) return null;
  return issueId ? { issueId, title, why, urgency } : { title, why, urgency };
}

// SRE's wire shape: { diagnosis, severity, recommendation, related_logs? }.
function normaliseSREFinding(
  p: Record<string, unknown>,
): {
  diagnosis: string;
  severity: string;
  recommendation: string;
  related_logs?: { source: string; excerpt: string }[];
} | null {
  const diagnosis = typeof p.diagnosis === "string" ? p.diagnosis : "";
  const severity = typeof p.severity === "string" ? p.severity : "info";
  const recommendation =
    typeof p.recommendation === "string" ? p.recommendation : "";
  if (!diagnosis && !recommendation) return null;
  let related_logs: { source: string; excerpt: string }[] | undefined;
  if (Array.isArray(p.related_logs)) {
    related_logs = p.related_logs
      .map((l): { source: string; excerpt: string } | null => {
        if (!l || typeof l !== "object") return null;
        const o = l as Record<string, unknown>;
        const source = typeof o.source === "string" ? o.source : "";
        const excerpt = typeof o.excerpt === "string" ? o.excerpt : "";
        if (!source && !excerpt) return null;
        return { source, excerpt };
      })
      .filter(
        (x): x is { source: string; excerpt: string } => x != null,
      )
      .slice(0, 5);
    if (related_logs.length === 0) related_logs = undefined;
  }
  return related_logs
    ? { diagnosis, severity, recommendation, related_logs }
    : { diagnosis, severity, recommendation };
}
