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

import { createUIMessageStream, type UIMessage } from "ai";

export interface BuilderTurnInput {
  messages: UIMessage[];
}

export async function buildTranslatedStream(input: BuilderTurnInput) {
  // Lazy imports — keep non-chat server functions free of the deepagents
  // dep tree (per design §4.8.5: server bundle weight mitigation).
  const { createDeepAgent } = await import("deepagents");
  const { ChatOpenAI } = await import("@langchain/openai");
  const { HumanMessage, AIMessage } = await import("@langchain/core/messages");

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
  const agent = createDeepAgent({
    model,
    tools: [],
    systemPrompt: BUILDER_SYSTEM,
  });

  const langchainMessages = input.messages.map((m) => {
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

  return createUIMessageStream({
    async execute({ writer }) {
      const textId = crypto.randomUUID();
      let textStarted = false;

      const events = agent.streamEvents(
        { messages: langchainMessages },
        { version: "v2" as const },
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
