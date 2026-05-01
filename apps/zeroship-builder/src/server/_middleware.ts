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

/**
 * Build a middleware bound to a specific v6 stream writer. The middleware
 * is single-use per request — never share across turns.
 */
export async function dataPartMiddleware(
  writer: UIMessageStreamWriter,
): Promise<AgentMiddleware> {
  // Lazy import to keep non-chat code paths free of the langchain
  // dep tree (matches the translator's lazy-import policy).
  const { createMiddleware } = await import("langchain");

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

      // All other tools fall through; the translator's streamEvents
      // switch turns them into native v6 tool-call chunks.
      return handler(request);
    },
  });
}
