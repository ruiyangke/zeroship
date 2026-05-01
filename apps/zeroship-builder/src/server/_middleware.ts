"use server";
// Phase B.2: data-part emitter middleware.
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
// Tool taxonomy (per spec §4.8.9 G4):
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
//   task (critic)         → emit `data-critic-round` after the SubAgent
//                           returns. Builder is told (via BUILDER_SYSTEM)
//                           to call task("critic", …) after every
//                           meaningful write batch; we extract Critic's
//                           structured response from the Command result
//                           and surface it as a small badge per round.
//                           Other subagent_types pass through silently
//                           — extend the branch if more subagents need
//                           dedicated UI.
//   ls, read_file, grep,
//   glob, execute, …      → middleware passes through; native
//                           tool-input-available / tool-output-available
//                           chunks are emitted by the translator from
//                           on_tool_start / on_tool_end events.
//
// "before" content for diffs: Phase B.2 ships before="" (empty). To
// fetch the on-disk file we'd need backend access here; deepagents
// doesn't currently expose the backend instance through the tool-call
// request. Phase B.3 may inject backend via context schema, at which
// point this can show real before/after diffs. For now the DiffCard
// renders an "all added" diff for write_file (matches the user
// experience of seeing a freshly-written file).

import type { AgentMiddleware } from "langchain";
import type { UIMessageStreamWriter } from "ai";

// emit helper is shared with the wizard runtime (per spec §4.8.2b /
// §8.2.7). The wizard calls it directly from its node body; Builder
// goes through this middleware. Same chunk shape on the wire.
import { emitDataSurvey, type SurveyInput } from "./_survey_wire.js";

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
  options: { isResume?: boolean } = {},
): Promise<AgentMiddleware> {
  const { isResume = false } = options;
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
        // We pass-through to the handler then, IFF the dispatched
        // subagent is "critic", parse the result and emit a
        // data-critic-round chunk.
        //
        // Result shape (from returnCommandWithStateUpdate, same file):
        //   Command({ update: { messages: [ToolMessage({
        //     content: <JSON-stringified structuredResponse> | <text>
        //   })] } })
        // — when the subagent has a `responseFormat` (Critic does), the
        // ToolMessage.content is the JSON-stringified Zod-validated
        // response. Otherwise it's the last message's text content.
        const result = await handler(request);
        const subagentType =
          typeof args.subagent_type === "string" ? args.subagent_type : "";
        if (subagentType === "critic") {
          const round = extractCriticRound(result, ++criticRound);
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
          }
        }
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

// Pull a critic-round payload out of the Command returned by the `task`
// tool when it dispatches the Critic SubAgent. The Command carries an
// update with a single ToolMessage whose content is the JSON-stringified
// structured response (because Critic has `responseFormat` set). We pull
// `{ approved, issues }`, normalise into the wire shape that
// CriticRoundCard expects ({ round, total, approved, issues:[{dimension,
// severity, note}] }), and stamp the round number from the per-turn
// counter.
//
// Defensive: returns null on any shape mismatch. The caller no-ops on
// null so a malformed critic response still lets the turn finish — the
// LLM gets the raw ToolMessage either way and can decide what to do.
function extractCriticRound(
  result: unknown,
  round: number,
): {
  round: number;
  total: number;
  approved: boolean;
  issues: { dimension: string; severity: string; note: string }[];
} | null {
  // The handler returns either a ToolMessage or a Command. When Critic
  // has a `responseFormat`, deepagents wraps in a Command via
  // `returnCommandWithStateUpdate`. We probe both shapes.
  let content: unknown = null;
  if (result && typeof result === "object") {
    const r = result as Record<string, unknown>;
    // Command shape: r.update.messages[0].content
    const update = r.update as Record<string, unknown> | undefined;
    if (update && Array.isArray(update.messages) && update.messages.length > 0) {
      const lastMsg = update.messages[update.messages.length - 1] as
        | { content?: unknown; kwargs?: { content?: unknown } }
        | undefined;
      content = lastMsg?.content ?? lastMsg?.kwargs?.content ?? null;
    }
    // Fallback: ToolMessage shape (r.content) or serialised
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
    // Critic returned plain text (no responseFormat hit) — emit a
    // best-effort "approved" round so the user still sees a checkmark.
    return { round, total: round, approved: true, issues: [] };
  }
  if (!parsed || typeof parsed !== "object") return null;
  const p = parsed as Record<string, unknown>;
  const approved = typeof p.approved === "boolean" ? p.approved : false;
  const rawIssues = Array.isArray(p.issues) ? p.issues : [];
  const issues = rawIssues
    .map((iss): { dimension: string; severity: string; note: string } | null => {
      if (!iss || typeof iss !== "object") return null;
      const i = iss as Record<string, unknown>;
      const dimension = typeof i.dimension === "string" ? i.dimension : "";
      const severity = typeof i.severity === "string" ? i.severity : "low";
      // Critic returns `issue` (description) and `suggested_fix` — fold
      // both into a single `note` string so the badge renders something
      // a human can act on.
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
