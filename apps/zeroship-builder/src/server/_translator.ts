"use server";
// Stream translator: deepagents/LangGraph events → AI SDK v6 UI Message
// Stream chunks. This is the seam committed to in design §4.8.4b — the
// agent runtime upstream (deepagents → LangGraph → LangChain models) is
// converted to AI SDK v6 wire format on the way out so that the existing
// `useChat` v6 client keeps working unchanged.
//
// Phase A handles text events only (single text-start → text-delta* →
// text-end per chat-model run). Phase B will add tool input/output events
// and custom data parts (Survey, Diff, CriticRound) without touching the
// client.
//
// Investigation note: AI SDK v6 ships no built-in LangChain adapter
// (verified by absence of any `LangChain*` export in
// `node_modules/ai/dist/index.d.ts` at the time of writing), so this
// translator is hand-rolled. If a future v6 release adds an official
// adapter, prefer it over this module.
//
// G3 — Resume protocol (post-`interruptOn` halt). Wire shapes:
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
// `token` is opaque on the wire — Phase B.1 will mint it inside the
// `ask_survey` interrupt handler so the client can echo it back, but
// the server only needs `id` (= thread_id) to find the right thread to
// resume.

import { createUIMessageStream, type UIMessage } from "ai";

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
  resume?: { token: string; value: unknown };
}

/**
 * Mode passed to `buildTranslatedStream`:
 *  - "fresh"  → input has `messages`; we replay history into a new
 *               (or continued) run on `thread_id`.
 *  - "resume" → input has `resume.value`; we issue `Command({resume})`
 *               into the existing thread, which restarts the
 *               interrupted node from where it halted.
 */
export type BuilderTurnMode = "fresh" | "resume";

// G1: process-local in-memory checkpointer. deepagents' middleware
// state (todos, virtual fs, summarised history) lives outside the
// LangChain `messages` array, so without a checkpointer it's
// discarded between turns and Builder loses context. Module-level
// singleton — one instance per worker, persists for the worker's
// lifetime.
//
// DEV-ONLY. The MemorySaver maps thread_id → state in-memory; data
// vanishes on worker restart and isn't shared across workers.
// Production needs a Postgres-backed BaseCheckpointSaver writing to
// the control plane DB (deferred to Plan 03+ — see spec §4.8.9 G1).
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

// G2: An optional AbortSignal lets the chat handler tear down the LLM
// HTTP call when the SSE consumer disconnects. The signal threads
// through `agent.streamEvents({ signal })` — LangChain respects it
// natively (RunnableConfig.signal). Without this plumbing the OpenAI
// request kept running after `useChat`'s Stop button, billing tokens
// the user never saw.
export async function buildTranslatedStream(
  input: BuilderTurnInput,
  signal?: AbortSignal,
) {
  // Lazy imports — keep non-chat server functions free of the deepagents
  // dep tree (per design §4.8.5: server bundle weight mitigation).
  const { createDeepAgent } = await import("deepagents");
  const { ChatOpenAI } = await import("@langchain/openai");
  const { HumanMessage, AIMessage } = await import("@langchain/core/messages");
  const { Command } = await import("@langchain/langgraph");

  const apiKey = process.env.OPENAI_API_KEY;
  if (!apiKey) {
    throw new Error(
      "OPENAI_API_KEY is not set. Configure it in apps/zeroship-builder/.env.",
    );
  }

  // gpt-5-nano matches the canonical v6 wire example at examples/ai-chat —
  // proven to stream cleanly through the runtime. The model is parameterized
  // here so Phase B can swap in Anthropic via @langchain/anthropic without
  // changing the translator.
  const model = new ChatOpenAI({
    model: "gpt-5.4-mini",
    temperature: 0.2,
    streaming: true,
    apiKey,
  });

  // Phase A: empty tools array. Phase B will inject write_file / propose_diff
  // / ask_survey tools and the translator's switch below will gain
  // `on_tool_*` handling that emits tool-input-* / tool-output-* chunks.
  //
  // G1: pass a process-local MemorySaver as the checkpointer so
  // middleware state survives across turns scoped by thread_id.
  const checkpointer = await getCheckpointer();
  const agent = createDeepAgent({
    model,
    tools: [],
    systemPrompt: BUILDER_SYSTEM,
    checkpointer,
  });

  const threadId = input.id ?? DEFAULT_THREAD_ID;
  const mode: BuilderTurnMode = input.resume ? "resume" : "fresh";

  // Build the streamEvents input depending on mode. In "fresh" mode we
  // feed the converted message history; in "resume" mode we feed a
  // Command(resume=...) so the Pregel runtime continues the
  // interrupted node with the user's answer. Both modes hit the same
  // agent and same thread_id, so the checkpointer ties them together.
  let streamInput: { messages: unknown[] } | InstanceType<typeof Command>;
  if (mode === "resume") {
    // Defensive: Phase B.0 has nothing emitting interruptOn yet, so
    // hitting this branch with no live thread will throw inside
    // streamEvents. We catch it inside the SSE writer so the client
    // sees a structured error rather than a connection drop.
    streamInput = new Command({ resume: input.resume!.value });
  } else {
    const langchainMessages = (input.messages ?? []).map((m) => {
      const text = (m.parts ?? [])
        .filter((p: any) => p?.type === "text")
        .map((p: any) => p.text ?? "")
        .join("");
      if (m.role === "user") return new HumanMessage(text);
      if (m.role === "assistant") return new AIMessage(text);
      // Skip unsupported roles (system, tool) for Phase A — the system prompt
      // is set on the agent itself, and tool messages aren't in scope yet.
      return new HumanMessage(text);
    });
    streamInput = { messages: langchainMessages };
  }

  return createUIMessageStream({
    async execute({ writer }) {
      const textId = crypto.randomUUID();
      let textStarted = false;

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
          // TODO Phase B: case "on_tool_start" → tool-input-available chunk
          // TODO Phase B: case "on_tool_end"   → tool-output-available chunk
          // TODO Phase B: agent custom events  → data-survey/data-diff/etc.
        }
      }

      // Belt-and-braces: if a model run ended without an explicit end event
      // (shouldn't happen with v2, but keeps the wire valid).
      if (textStarted) {
        writer.write({ type: "text-end", id: textId });
      }
    },
  });
}

// --- helpers --------------------------------------------------------------

function extractTextDelta(event: {
  data?: { chunk?: { content?: unknown } };
}): string {
  // LangGraph emits a ChatGenerationChunk whose `.content` is either a
  // string (most providers) or an array of content parts
  // (`{ type: "text"; text: string } | { type: "tool_use"; ... }`). The
  // OpenAI provider in our setup uses the string form, but we handle both
  // so this translator survives a model swap (e.g., to Anthropic in Phase B).
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

const BUILDER_SYSTEM = `You are Builder, the zeroship platform's coding agent.

You help creators build full-stack apps that run on the zeroship runtime.
The platform handles hosting, database, auth, payments, and scaling — your
job is to write the application code.

Style:
- Direct and concise. No preamble, no filler.
- Ask 1-2 clarifying questions only when intent is genuinely ambiguous.
- When you don't know something, say so plainly.

Phase A capability: text replies only. Tools (file edits, deploys,
clarifying surveys, diff proposals) come online in Phase B.`;
