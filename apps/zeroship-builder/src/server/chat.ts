"use server";
// Builder chat — routed through deepagents (server-side agent runtime,
// `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §4.8 Foundation Decision #8) and translated to AI SDK v6 UI
// Message Stream Protocol on the wire. The kernel forwards the Response's
// SSE bytes verbatim to the v6 `useChat` client.
//
// Wire (per AI SDK v6 spec, identical to examples/ai-chat):
//   POST /__zeroship/v1/chat
//     fresh body:   { json: { messages: UIMessage[], id: string } }
//     resume body:  { json: { resume: { token, value }, id: string } }
//     response:     text/event-stream                  ← UI Message Stream
//                with header `x-vercel-ai-ui-message-stream: v1`
//                and SSE frames carrying { type, ... } JSON objects.
//
// --- Stream translator: deepagents/LangGraph events → AI SDK v6 ---------
//
// This is the seam committed to in `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §4.8.4b — the agent runtime
// upstream (deepagents → LangGraph → LangChain models) is converted to
// AI SDK v6 wire format on the way out so that the existing `useChat`
// v6 client keeps working unchanged.
//
// The current translator handles text events only (single text-start →
// text-delta* → text-end per chat-model run). Tool input/output events
// and custom data parts (Survey, Diff, CriticRound) are layered on
// around it without changing the client contract.
//
// Investigation note: AI SDK v6 ships no built-in LangChain adapter
// (verified by absence of any `LangChain*` export in
// `node_modules/ai/dist/index.d.ts` at the time of writing), so this
// translator is hand-rolled. If a future v6 release adds an official
// adapter, prefer it over this module.
//
// Resume protocol (post-`interruptOn` halt). Wire shapes:
//
//   Normal turn body:
//     { json: { messages: UIMessage[], id: string } }
//
//   Resume turn body (after a tool with `interruptOn` halted the run):
//     { json: { resume: { token: string, value: unknown }, id: string } }
//
// On a resume request the server skips message conversion and feeds
// `new Command({ resume: <value> })` into the same Pregel graph using
// the same thread_id. The interrupted run picks up where it left off
// (the deepagents middleware emits new chunks; the translator forwards
// them as additional UI message parts on the same assistant turn).
// `token` is opaque on the wire. The `ask_survey` interrupt handler
// mints it so the client can echo it back, but the server only needs
// `id` (= thread_id) to find the right thread to resume.
//
// The chat handler runs through the RPC fast path, which does NOT
// construct a Request, so we can't read
// `request.signal`. Instead we mint our own AbortController and abort
// it when the response body's ReadableStream is cancelled (which the
// V8 runtime does when the SSE consumer disconnects). The signal is
// threaded into `streamEvents`, where LangChain forwards it to the
// underlying OpenAI HTTP call.
//
// We wrap the body stream so we observe `cancel()`. If a future
// runtime change exposes `request.signal` on the RPC fast path, this
// can be simplified to just plumb that signal through — no body
// wrapping needed.

import { createUIMessageStream, createUIMessageStreamResponse, type UIMessage } from "ai";
import { streamResponse } from "@zeroship/rpc/server";
import { z } from "zod";
import { critic } from "./internal/critic";
import { reviewer } from "./internal/reviewer";
import { pm } from "./internal/pm";
import { sre } from "./internal/sre";
import { BUILDER_SYSTEM } from "./internal/prompts";
import { askSurveyTool, createReviewTool } from "./internal/tools";

export interface BuilderTurnInput {
  messages?: UIMessage[];
  /**
   * Conversation/session id from `useChat`. Becomes the LangGraph
   * `thread_id` so the checkpointer scopes middleware-managed state
   * (TodoListMiddleware todos, FilesystemMiddleware fs,
   * SummarizationMiddleware history) to this conversation. Without it
   * every turn gets a fresh thread and Builder forgets its work.
   *
   * Also doubles as the thread id the resume protocol targets — the
   * server uses `id` to find which interrupted run to resume.
   */
  id?: string;
  /**
   * Resume payload — present iff the client is answering an
   * `interruptOn`-emitted prompt (e.g. SurveyCard submit). When
   * present, `messages` is ignored: the server passes
   * `new Command({ resume: value })` into the same thread instead of
   * replaying the message history.
   */
  resume?: { token: string; value?: unknown };
  /**
   * Project id this chat surface is scoped to. Distinct from `id`
   * (the chat thread): one project may host multiple threads but the
   * scorecard / issues / health belong to the project. The middleware
   * uses this to persist Critic-graded scorecards into the right KV
   * slot. Optional — missing → side-effect skipped.
   */
  appId?: string;
}

const builderTurnInputSchema = z.object({
  messages: z.array(z.custom<UIMessage>((value) => value !== null && typeof value === "object")).optional(),
  id: z.string().min(1).max(256).optional(),
  resume: z.object({
    token: z.string().min(1).max(512),
    value: z.unknown(),
  }).strict().optional(),
  appId: z.string().min(1).max(256).optional(),
}).strict().refine((input) => input.resume || (input.messages?.length ?? 0) > 0, {
  message: "chat input requires messages or resume",
});

/**
 * Mode passed to `buildTranslatedStream`:
 *  - "fresh"  → input has `messages`; we replay history into a new
 *               (or continued) run on `thread_id`.
 *  - "resume" → input has `resume.value`; we issue `Command({resume})`
 *               into the existing thread, which restarts the
 *               interrupted node from where it halted.
 */
export type BuilderTurnMode = "fresh" | "resume";

// Process-local in-memory checkpointer. deepagents' middleware
// state (todos, virtual fs, summarised history) lives outside the
// LangChain `messages` array, so without a checkpointer it's
// discarded between turns and Builder loses context. Module-level
// singleton — one instance per worker, persists for the worker's
// lifetime.
//
// DEV-ONLY. The MemorySaver maps thread_id → state in-memory; data
// vanishes on worker restart and isn't shared across workers.
// Production needs a Postgres-backed BaseCheckpointSaver writing to
// the control plane DB once durable checkpoint storage is wired.
//
// Lazy-loaded at first chat call so non-chat server functions don't
// pay the @langchain/langgraph dep cost on cold isolates.
let _checkpointer: import("@langchain/langgraph-checkpoint").BaseCheckpointSaver | null = null;
async function getCheckpointer(): Promise<
  import("@langchain/langgraph-checkpoint").BaseCheckpointSaver
> {
  if (_checkpointer) return _checkpointer;
  const { MemorySaver } = await import("@langchain/langgraph");
  _checkpointer = new MemorySaver();
  return _checkpointer;
}

// Default thread id used when `useChat` doesn't supply one. Should be
// rare (the React hook generates one per Chat instance), but falling
// back to a single shared thread is better than minting a new one
// per call (which would drop state every turn).
const DEFAULT_THREAD_ID = "builder-default";

// An optional AbortSignal lets the chat handler tear down the LLM
// HTTP call when the SSE consumer disconnects. The signal threads
// through `agent.streamEvents({ signal })` — LangChain respects it
// natively (RunnableConfig.signal). Without this plumbing the OpenAI
// request kept running after `useChat`'s Stop button, billing tokens
// the user never saw.
async function buildTranslatedStream(
  input: BuilderTurnInput,
  signal?: AbortSignal,
) {
  // Lazy imports — keep non-chat server functions free of the deepagents
  // dep tree (per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §4.8.5: server bundle weight mitigation).
  const { createDeepAgent } = await import("deepagents");
  const { ChatOpenAI } = await import("@langchain/openai");
  const { HumanMessage, AIMessage, ToolMessage } = await import(
    "@langchain/core/messages"
  );
  const { Command, isGraphInterrupt } = await import("@langchain/langgraph");

  const apiKey = process.env.OPENAI_API_KEY;
  if (!apiKey) {
    throw new Error(
      "OPENAI_API_KEY is not set. Configure it in apps/zeroship-builder/.env.",
    );
  }

  // Keep the same model family across Builder and the subagents for now.
  // The model is parameterised here so a later provider swap
  // (`@langchain/anthropic`, etc.) does not require translator changes.
  const model = new ChatOpenAI({
    model: "gpt-5.4-mini",
    temperature: 0.2,
    streaming: true,
    apiKey,
  });

  const threadId = input.id ?? DEFAULT_THREAD_ID;

  // Acquire a sandbox for this Builder thread BEFORE constructing the
  // agent so the backend instance can be wired into
  // `createDeepAgent`. The lookup is idempotent at the wire (the
  // controller dedups on (user_id, project_id)) and cached
  // process-locally.
  //
  // Note: we don't acquire the sandbox in "resume" mode either —
  // resuming an interrupted run on the same thread should also use
  // the same sandbox (the in-flight tool that interrupted may need
  // sandbox access on the resumed half).
  const { ZeroshipSandboxBackend, getOrCreateSandboxFor } = await import(
    "./internal/sandbox-backend.js"
  );
  const sandbox = await getOrCreateSandboxFor(threadId, {
    projectSourceId: input.appId ?? threadId,
  });
  const backend = new ZeroshipSandboxBackend({
    id: sandbox.id,
    userId: sandbox.userId,
  });

  // Pass a process-local MemorySaver as the checkpointer so middleware
  // state survives across turns scoped by `thread_id`.
  const checkpointer = await getCheckpointer();

  const mode: BuilderTurnMode = input.resume ? "resume" : "fresh";

  // Build the streamEvents input depending on mode. In "fresh" mode we
  // feed the converted message history; in "resume" mode we feed a
  // Command(resume=...) so the Pregel runtime continues the
  // interrupted node with the user's answer. Both modes hit the same
  // agent and same thread_id, so the checkpointer ties them together.
  let streamInput: { messages: unknown[] } | InstanceType<typeof Command>;
  if (mode === "resume") {
    // Defensive: if this branch is hit with no live thread,
    // `streamEvents` will throw inside
    // streamEvents. We catch it inside the SSE writer so the client
    // sees a structured error rather than a connection drop.
    streamInput = new Command({ resume: input.resume!.value });
  } else {
    const langchainMessages = convertUIMessagesToLangChain(
      input.messages ?? [],
      { HumanMessage, AIMessage, ToolMessage },
    );
    streamInput = { messages: langchainMessages };
  }

  // Import the data-part emitter middleware factory. The middleware
  // itself needs the per-request v6 writer, so we
  // instantiate it inside `execute({writer})` below.
  const { dataPartMiddleware } = await import("./internal/middleware.js");

  return createUIMessageStream({
    async execute({ writer }) {
      // deepagents activates its built-in fs/exec tools (`ls`,
      // `read_file`, `write_file`, `edit_file`, `grep`, `glob`,
      // `execute`) automatically when `backend:` is configured — they're
      // rewritten on top of the backend's protocol methods.
      //
      // Custom tools registered here add to that built-in set:
      //   - askSurveyTool: halts the run via interrupt() and emits a
      //     data-survey chunk through the middleware below; resumes
      //     when the client sends `body.resume` (see chat.ts).
      //
      // Middleware: the data-part emitter sits in `wrapToolCall` and
      // turns `write_file` / `edit_file` / `ask_survey` invocations
      // into v6 custom data chunks (`data-diff`, `data-survey`). The
      // streamEvents loop below SKIPS native tool chunks for those
      // same tools so the wire shows one card per call, not two.
      const dataPartMw = await dataPartMiddleware(writer, {
        isResume: mode === "resume",
        appId: input.appId,
      });
      // Override each SubAgent's `model: "openai:gpt-5.4-mini"` (string)
      // with the live ChatOpenAI instance. deepagents resolves a string
      // model via `langchain/chat_models/universal#initChatModel`, which
      // does `await import("@langchain/openai")` with a runtime-variable
      // package name. Vite can't statically detect that import, so the
      // module gets externalized and the V8 dev-bootstrap throws
      // "Cannot import external module" the first time a subagent is
      // dispatched. Passing an already-constructed BaseChatModel instance
      // skips initChatModel entirely.
      const subagents = [critic, reviewer, pm, sre].map((sa) => ({
        ...sa,
        model,
      }));
      const reviewTool = createReviewTool({
        backend,
        apiKey,
      });
      const agent = createDeepAgent({
        model,
        tools: [askSurveyTool, reviewTool],
        backend,
        systemPrompt: BUILDER_SYSTEM,
        checkpointer,
        middleware: [dataPartMw] as const,
        subagents,
      });

      const textId = crypto.randomUUID();
      let textStarted = false;

      // Tools whose visualisation goes via custom data parts instead
      // of the native v6 tool-call chunks. Keep in sync with the
      // wrapToolCall hooks in `internal/middleware.ts`.
      //   write_file / edit_file → data-diff
      //   ask_survey             → data-survey (also: tool halts via
      //                            interrupt(), so on_tool_end may not
      //                            even fire on the interrupted run)
      //   task                   → routed by subagent_type in the
      //                            middleware:
      //                              "critic"   → data-critic-round
      //                              "reviewer" → data-reviewer-round
      //                              "pm"       → data-pm-recommendation
      //                              "sre"      → data-sre-finding
      //                            All four types are suppressed from
      //                            native tool-call chunks here so the
      //                            wire shows one card per dispatch,
      //                            not a duplicate receipt + card.
      const CUSTOM_DATA_TOOLS = new Set([
        "write_file",
        "edit_file",
        "ask_survey",
        "task",
      ]);

      // streamEvents accepts InputType | Command — both modes use the
      // same v2 protocol, signal plumbing, and thread_id.
      const events = agent.streamEvents(
        streamInput as any,
        {
          version: "v2" as const,
          signal,
          configurable: { thread_id: threadId },
        },
      );

      // GraphInterrupt is thrown when an interrupt() inside a tool halts
      // the run (e.g., ask_survey). The middleware has already emitted
      // the data-survey chunk by the time the interrupt propagates, so
      // we just need to swallow the error and let the stream close
      // cleanly. Anything else is a real error and re-thrown.
      try {
       for await (const event of events) {
        switch (event.event) {
          case "on_chat_model_start":
            // Defer text-start until first non-empty delta — some providers
            // emit empty start frames (e.g., for tool-call planning) and
            // we don't want to open a text part that has nothing in it.
            break;
          case "on_chat_model_stream": {
            const delta = extractTextDelta(event);
            if (delta) {
              if (!textStarted) {
                writer.write({ type: "text-start", id: textId });
                textStarted = true;
              }
              writer.write({ type: "text-delta", id: textId, delta });
            }
            break;
          }
          case "on_chat_model_end":
            if (textStarted) {
              writer.write({ type: "text-end", id: textId });
              textStarted = false;
            }
            break;

          // Native tool-call chunks. AI SDK v6 names these
          // `tool-input-available` and `tool-output-available` (see
          // node_modules/ai/dist/index.d.ts:2093-2122). The client's
          // ChatMessages dispatcher pairs them by toolCallId into a
          // <Receipt>.
          case "on_tool_start": {
            const toolName = String(event.name ?? "");
            if (!toolName || CUSTOM_DATA_TOOLS.has(toolName)) break;
            const toolCallId = String(event.run_id ?? crypto.randomUUID());
            // LangChain wraps the resolved input as
            // `event.data.input = { input: <stringified-args-or-raw> }`
            // — see the canonical event shape in
            // node_modules/@langchain/core/dist/tracers/event_stream.cjs.
            // Unwrap to the actual args object (best-effort JSON parse
            // when it's a stringified object) so the v6 chunk's
            // `input` is the structured args, not an envelope.
            const inputRaw = (event as { data?: { input?: unknown } }).data?.input;
            const input = unwrapToolInput(inputRaw);
            try {
              writer.write({
                type: "tool-input-available",
                toolCallId,
                toolName,
                input,
              });
            } catch {
              // stream closed — drop
            }
            break;
          }

          case "on_tool_end": {
            const toolName = String(event.name ?? "");
            if (!toolName || CUSTOM_DATA_TOOLS.has(toolName)) break;
            const toolCallId = String(event.run_id ?? crypto.randomUUID());
            const rawOutput =
              (event as { data?: { output?: unknown } }).data?.output ?? null;
            // LangChain hands us the full ToolMessage object as `output`
            // (with `kwargs.content` carrying the actual tool result).
            // The v6 chunk's `output` is meant to be the tool's result
            // value, not a serialized message envelope, so unwrap.
            const output = unwrapToolOutput(rawOutput);
            try {
              writer.write({
                type: "tool-output-available",
                toolCallId,
                output,
              });
            } catch {
              // stream closed — drop
            }
            break;
          }
        }
       }
      } catch (err) {
        if (!isGraphInterrupt(err)) throw err;
        // ask_survey (or any other interrupt-using tool) halted the run.
        // The data-survey chunk has already been written via middleware
        // — we just close the text part if one was open and let the SSE
        // stream end. Client renders the SurveyCard; on submit it sends
        // the resume body which re-enters this function in resume mode.
      }

      // Belt-and-braces: if a model run ended without an explicit end event
      // (shouldn't happen with v2, but keeps the wire valid). Also fires on
      // GraphInterrupt where the model run was mid-flight when the tool
      // halted — the text part is still open from the model's prelude.
      if (textStarted) {
        writer.write({ type: "text-end", id: textId });
      }
    },
  });
}

// --- helpers --------------------------------------------------------------

// Convert AI SDK v6 UIMessage[] → LangChain BaseMessage[]. The converter
// must round-trip not only text but also the model's
// tool-call history, otherwise the next turn's LLM doesn't see "what
// tools did I just call and what did they return", and the agent
// will repeat or get confused.
//
// AI SDK v6 part discriminants we handle:
//   - { type: "text", text }                             → text content
//   - { type: "tool-<name>", toolCallId, state, input,
//       output? }                                        → AIMessage.tool_calls + ToolMessage
//   - { type: "dynamic-tool", toolName, toolCallId,
//       state, input, output? }                          → same shape, name from `toolName`
//   - { type: "data-*", ... }                            → UI-only; stripped here
//   - { type: "reasoning", ... }                         → not surfaced to model for now
//
// Tool-call lifecycle states (from `UIToolInvocation` in
// node_modules/ai/dist/index.d.ts:1694):
//   "input-streaming" | "input-available" → call exists but no result yet
//   "output-available"                    → result ready (emits ToolMessage)
//   "output-error" / "output-denied"      → terminal but with error/denied
//
// For each assistant message, tool-call parts in any input-/output-
// state become entries in the AIMessage's `tool_calls` array. Then,
// for parts in `output-available` (or terminal error/denied) state,
// we append a follow-up ToolMessage carrying the result. This matches
// LangChain's expected shape: AIMessage with tool_calls → ToolMessage(s)
// keyed by `tool_call_id`.
//
// Not every tool emits these parts yet, so some of this branch is still
// exercised mainly by type coverage. `write_file` and `ask_survey` are
// the first concrete producers.
function convertUIMessagesToLangChain(
  messages: UIMessage[],
  ctors: {
    HumanMessage: typeof import("@langchain/core/messages").HumanMessage;
    AIMessage: typeof import("@langchain/core/messages").AIMessage;
    ToolMessage: typeof import("@langchain/core/messages").ToolMessage;
  },
): import("@langchain/core/messages").BaseMessage[] {
  const { HumanMessage, AIMessage, ToolMessage } = ctors;
  const out: import("@langchain/core/messages").BaseMessage[] = [];

  for (const m of messages) {
    const parts = (m.parts ?? []) as any[];

    // --- user messages: collapse text parts only (no tool-call shape) ---
    if (m.role === "user") {
      const text = parts
        .filter((p) => p?.type === "text")
        .map((p) => p.text ?? "")
        .join("");
      out.push(new HumanMessage(text));
      continue;
    }

    if (m.role === "assistant") {
      // Aggregate all text into a single content string and collect
      // any tool-call parts. v6 emits one part per tool invocation;
      // the same toolCallId may appear in multiple lifecycle states
      // across the part stream, but inside the persisted UIMessage
      // we expect one terminal-state part per call.
      const text = parts
        .filter((p) => p?.type === "text")
        .map((p) => p.text ?? "")
        .join("");

      const toolCalls: {
        id: string;
        name: string;
        args: Record<string, any>;
        type?: "tool_call";
      }[] = [];
      const toolResults: {
        tool_call_id: string;
        content: string;
        status: "success" | "error";
      }[] = [];

      for (const p of parts) {
        if (!p || typeof p.type !== "string") continue;
        // Match `tool-<name>` (static tools) or `dynamic-tool`.
        let toolName: string | null = null;
        if (p.type === "dynamic-tool") {
          toolName = typeof p.toolName === "string" ? p.toolName : null;
        } else if (p.type.startsWith("tool-")) {
          toolName = p.type.slice("tool-".length);
        }
        if (!toolName || typeof p.toolCallId !== "string") continue;

        // Always record the call (so the model sees what it asked
        // for) — `args` can be the partial input on streaming states.
        // Coerce `input` into a Record (LangChain's ToolCall.args is
        // typed `Record<string, any>`); a non-object input is wrapped
        // under `{ value }` rather than dropped.
        const inputAsArgs: Record<string, any> =
          p.input && typeof p.input === "object" && !Array.isArray(p.input)
            ? (p.input as Record<string, any>)
            : { value: p.input };
        toolCalls.push({
          id: p.toolCallId,
          name: toolName,
          args: inputAsArgs,
          type: "tool_call",
        });

        // Only emit a ToolMessage when there's a terminal result.
        if (p.state === "output-available") {
          toolResults.push({
            tool_call_id: p.toolCallId,
            content: stringifyToolResult(p.output),
            status: "success",
          });
        } else if (p.state === "output-error") {
          toolResults.push({
            tool_call_id: p.toolCallId,
            content: typeof p.errorText === "string" ? p.errorText : "error",
            status: "error",
          });
        }
        // input-streaming / input-available / approval-* / output-denied:
        // no ToolMessage. The AIMessage carries the call; the model
        // can see it was made but never resolved.
      }

      // data-* parts are intentionally dropped — they're UI-only
      // (DiffCard, IssueCard, CriticRoundCard etc., per `docs/superpowers/specs/2026-04-30-zeroship-builder-design.md` §4.8.3.3
      // and would only confuse the LLM if echoed back.

      out.push(
        new AIMessage({
          content: text,
          tool_calls: toolCalls.length > 0 ? toolCalls : undefined,
        }),
      );

      for (const r of toolResults) {
        out.push(
          new ToolMessage({
            content: r.content,
            tool_call_id: r.tool_call_id,
            status: r.status,
          }),
        );
      }
      continue;
    }

    // Other roles (system, tool) — skip. System prompt is on the
    // agent; loose ToolMessages without a parent AIMessage's tool_call
    // would be malformed history.
  }

  return out;
}

function stringifyToolResult(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

// Unwrap a LangChain on_tool_start input envelope into the structured
// args. LangChain emits `data.input = { input: <args-or-string> }`.
// `<args-or-string>` is usually the raw structured args object, but for
// some tool wrappers it's a JSON-stringified object. Best-effort parse.
function unwrapToolInput(value: unknown): unknown {
  if (!value || typeof value !== "object") return value;
  const v = value as Record<string, unknown>;
  if (Object.keys(v).length === 1 && "input" in v) {
    const inner = v.input;
    if (typeof inner === "string") {
      try {
        return JSON.parse(inner);
      } catch {
        return inner;
      }
    }
    return inner;
  }
  return value;
}

// Unwrap a LangChain ToolMessage (or ToolMessageChunk) coming through
// the `on_tool_end` event so the v6 `tool-output-available` chunk
// carries the tool's actual return value, not a serialized message
// envelope. LangChain serializes ToolMessages as
//   { lc, type: "constructor", id, kwargs: { content, status, ... } }
// — we want `kwargs.content` (and fall back to the raw value when the
// shape isn't a ToolMessage, e.g., raw strings or Command objects).
function unwrapToolOutput(value: unknown): unknown {
  if (!value || typeof value !== "object") return value;
  const v = value as Record<string, unknown>;
  if (v.type === "constructor" && v.kwargs && typeof v.kwargs === "object") {
    const kw = v.kwargs as Record<string, unknown>;
    if ("content" in kw) return kw.content;
  }
  // Some tools return a ToolMessage instance directly (not a serialized
  // form). Those have a `.content` property.
  if ("content" in v && (typeof v.content === "string" || Array.isArray(v.content))) {
    return v.content;
  }
  return value;
}

function extractTextDelta(event: {
  data?: { chunk?: { content?: unknown } };
}): string {
  // LangGraph emits a ChatGenerationChunk whose `.content` is either a
  // string (most providers) or an array of content parts
  // (`{ type: "text"; text: string } | { type: "tool_use"; ... }`). The
  // OpenAI provider in our setup uses the string form, but we handle both
  // so this translator survives a model swap (for example to Anthropic).
  const content = event?.data?.chunk?.content;
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .filter((c: any) => c?.type === "text")
      .map((c: any) => c.text ?? "")
      .join("");
  }
  return "";
}

// --- RPC handler --------------------------------------------------------

// Marked as `stream` for the RPC capability frame: the handler returns an
// AI-SDK SSE Response, but it is still a long-lived streaming endpoint and
// must be allowed to call external APIs and sandbox-controller fetch paths.
export const chat = streamResponse(
  async (input: BuilderTurnInput): Promise<Response> => {
    const ac = new AbortController();
    const stream = await buildTranslatedStream(input, ac.signal);

    // Wrap the SSE Response's body to abort the in-flight LLM call when
    // the client disconnects. The default `createUIMessageStreamResponse`
    // body is a ReadableStream; we passthrough chunks but intercept
    // `cancel()` to fire the AbortController.
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
        // Client disconnected mid-stream — abort the OpenAI request.
        ac.abort(reason);
      },
    });

    return new Response(wrapped, {
      status: baseResponse.status,
      statusText: baseResponse.statusText,
      headers: baseResponse.headers,
    });
  },
  {
    id: "chat",
    lazy: true,
    input: builderTurnInputSchema,
    maxInputBytes: 262_144,
  },
);
