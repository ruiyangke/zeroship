/**
 * AI agent server for zeroship — Bun + Hono.
 *
 * POST /chat — streaming chat with the AI agent
 * GET  /health — health check
 */
import { Hono } from "hono";
import { cors } from "hono/cors";
import { streamSSE } from "hono/streaming";
import { createAppbaseAgent } from "./agent.js";
import { HumanMessage, AIMessage } from "@langchain/core/messages";

const app = new Hono();

app.use("/*", cors());

app.get("/health", (c) => c.json({ status: "ok" }));

/**
 * POST /chat
 * Body: { messages: [{ role, content }], thread_id?: string }
 * Response: SSE stream of agent responses
 */
app.post("/chat", async (c) => {
  const body = await c.req.json();
  const messages = body.messages ?? [];
  const threadId = body.thread_id ?? "default";

  const agent = createAppbaseAgent();

  // Convert to LangChain message format
  const lcMessages = messages.map((m: { role: string; content: string }) => {
    if (m.role === "user") return new HumanMessage(m.content);
    return new AIMessage(m.content);
  });

  return streamSSE(c, async (stream) => {
    try {
      const eventStream = agent.streamEvents(
        { messages: lcMessages },
        {
          configurable: { thread_id: threadId },
          version: "v2",
        }
      );

      for await (const event of eventStream) {
        // Text deltas
        if (
          event.event === "on_chat_model_stream" &&
          event.data?.chunk?.content
        ) {
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

        // Tool calls
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
          await stream.writeSSE({
            data: JSON.stringify({
              type: "tool_end",
              name: event.name,
              output: event.data?.output,
            }),
          });
        }
      }

      await stream.writeSSE({
        data: JSON.stringify({ type: "done" }),
      });
    } catch (err: any) {
      await stream.writeSSE({
        data: JSON.stringify({ type: "error", content: err.message }),
      });
    }
  });
});

const port = parseInt(process.env.AGENT_PORT ?? "4444");

export default {
  port,
  fetch: app.fetch,
};

if (!process.env.ZEROSHIP_MASTER_KEY) {
  console.warn("\u26a0\ufe0f  ZEROSHIP_MASTER_KEY not set \u2014 using dev-master-key");
}

console.log(`[agent] http://localhost:${port}`);
