/**
 * AI agent server for zeroship — Bun + Hono.
 *
 * POST /chat   — streaming chat with the agent (SSE)
 * GET  /health — health check
 *
 * Each /chat request creates a fresh agent instance with the
 * workspace context baked into the system prompt. This keeps
 * tool calls focused on the current project (the agent reaches
 * for `deploy_app` against the right `app_id` instead of
 * hallucinating new app names).
 */
import { Hono } from "hono";
import { cors } from "hono/cors";
import { streamSSE } from "hono/streaming";
import { createZeroshipAgent, type AgentContext } from "./agent.js";
import { HumanMessage, AIMessage } from "@langchain/core/messages";

const app = new Hono();

app.use("/*", cors());

app.get("/health", (c) => c.json({ status: "ok" }));

interface ChatBody {
  messages?: { role: string; content: string }[];
  thread_id?: string;
  context?: AgentContext;
  model?: string;
}

app.post("/chat", async (c) => {
  let body: ChatBody;
  try {
    body = await c.req.json();
  } catch {
    return c.json({ error: "invalid JSON body" }, 400);
  }

  const messages = body.messages ?? [];
  const threadId = body.thread_id ?? "default";
  const context = body.context;

  let agent;
  try {
    agent = createZeroshipAgent({ model: body.model, context });
  } catch (err: any) {
    // Most common cause: ANTHROPIC_API_KEY missing. Surface as a
    // clean SSE error so the dashboard can render it instead of
    // showing a 500 with no message.
    return streamSSE(c, async (stream) => {
      await stream.writeSSE({
        data: JSON.stringify({
          type: "error",
          content: `agent init failed: ${err?.message ?? String(err)}`,
        }),
      });
      await stream.writeSSE({ data: JSON.stringify({ type: "done" }) });
    });
  }

  const lcMessages = messages.map((m) => {
    if (m.role === "user") return new HumanMessage(m.content);
    return new AIMessage(m.content);
  });

  return streamSSE(c, async (stream) => {
    try {
      const eventStream = agent.streamEvents(
        { messages: lcMessages },
        { configurable: { thread_id: threadId }, version: "v2" },
      );

      for await (const event of eventStream) {
        // Streaming text from the model
        if (event.event === "on_chat_model_stream" && event.data?.chunk?.content) {
          const content = event.data.chunk.content;
          if (typeof content === "string" && content.length > 0) {
            await stream.writeSSE({
              data: JSON.stringify({ type: "text", content }),
            });
          } else if (Array.isArray(content)) {
            for (const block of content) {
              if (block.type === "text" && block.text) {
                await stream.writeSSE({
                  data: JSON.stringify({ type: "text", content: block.text }),
                });
              }
            }
          }
        }

        // Tool lifecycle
        if (event.event === "on_tool_start") {
          await stream.writeSSE({
            data: JSON.stringify({
              type: "tool_start",
              name: event.name,
              input: event.data?.input,
            }),
          });
        }

        if (event.event === "on_tool_end") {
          // Best-effort error detection: tools that follow our convention
          // return a JSON string with `ok: false` on failure.
          const out = event.data?.output;
          let isError = false;
          if (typeof out === "string") {
            try {
              const parsed = JSON.parse(out);
              if (parsed && parsed.ok === false) isError = true;
            } catch { /* not JSON, treat as success */ }
          }

          await stream.writeSSE({
            data: JSON.stringify({
              type: "tool_end",
              name: event.name,
              output: typeof out === "string" ? out : JSON.stringify(out),
              error: isError,
            }),
          });
        }
      }

      await stream.writeSSE({ data: JSON.stringify({ type: "done" }) });
    } catch (err: any) {
      await stream.writeSSE({
        data: JSON.stringify({
          type: "error",
          content: err?.message ?? String(err),
        }),
      });
    }
  });
});

const port = parseInt(process.env.AGENT_PORT ?? "4444");

if (!process.env.ANTHROPIC_API_KEY) {
  console.warn("[agent] ANTHROPIC_API_KEY not set — calls will fail");
}
if (!process.env.ZEROSHIP_MASTER_KEY) {
  console.warn("[agent] ZEROSHIP_MASTER_KEY not set — using 'dev-master-key'");
}

console.log(`[agent] http://localhost:${port}`);

export default {
  port,
  fetch: app.fetch,
};
