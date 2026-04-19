"use server";

import { ChatOpenAI } from "@langchain/openai";
import { HumanMessage, AIMessage, ToolMessage } from "@langchain/core/messages";
import { tool } from "@langchain/core/tools";
import { z } from "zod";

// ── Tools ──────────────────────────────────────────────────────────────

const tools = [
  tool(
    async ({ expression }: { expression: string }) => {
      try {
        const result = new Function(`"use strict"; return (${expression})`)();
        return `${expression} = ${result}`;
      } catch (e: any) { return `Error: ${e.message}`; }
    },
    {
      name: "calculator",
      description: "Evaluate a math expression. Supports +, -, *, /, (), Math.sqrt, Math.PI.",
      schema: z.object({ expression: z.string().describe("Math expression") }),
    }
  ),
  tool(
    async ({ city }: { city: string }) => {
      const data: Record<string, string> = {
        "new york": "72°F, Partly cloudy, humidity 55%",
        "london": "59°F, Overcast, humidity 78%",
        "tokyo": "68°F, Clear, humidity 45%",
        "paris": "64°F, Light rain, humidity 82%",
        "san francisco": "61°F, Foggy, humidity 88%",
      };
      return `Weather in ${city}: ${data[city.toLowerCase()] ?? "65°F, conditions unknown"}`;
    },
    {
      name: "weather",
      description: "Get current weather for a city.",
      schema: z.object({ city: z.string().describe("City name") }),
    }
  ),
  tool(
    async () => {
      const now = new Date();
      return `Current time: ${now.toISOString()}, ${now.toLocaleDateString("en-US", { weekday: "long" })}`;
    },
    {
      name: "datetime",
      description: "Get current date and time.",
      schema: z.object({}),
    }
  ),
];

// ── Model ──────────────────────────────────────────────────────────────

let model: any = null;
function getModel() {
  if (model) return model;
  model = new ChatOpenAI({ model: "gpt-5.4-mini", temperature: 0 }).bindTools(tools);
  return model;
}
const toolMap = Object.fromEntries(tools.map(t => [t.name, t]));

// ── Streaming ReAct loop ───────────────────────────────────────────────
//
// Each iteration either:
//   (a) streams tokens from the model until it finishes, or
//   (b) collects tool_calls, executes them, appends ToolMessage results,
//       and loops. Tool events surface as `{tool, args}` / `{tool, result}`
//       SSE frames so the UI can render them inline.
//
// LangChain's `.stream()` returns an AsyncIterable<AIMessageChunk>. Chunks
// carry either a content delta (text token) or a tool_call fragment. We
// concatenate the fragments so that after the iterator completes we have
// the same AIMessage we would have gotten from `.invoke()`, plus the
// token events already emitted.
async function reactLoop(
  messages: any[],
  emit: (ev: any) => void,
): Promise<void> {
  const m = getModel();
  for (let step = 0; step < 5; step++) {
    const stream: any = await m.stream(messages);

    let accumulated: any = null;
    for await (const chunk of stream) {
      accumulated = accumulated == null ? chunk : accumulated.concat(chunk);
      const delta = typeof chunk.content === "string" ? chunk.content : "";
      if (delta) emit({ token: delta });
    }
    if (accumulated == null) return;
    messages.push(accumulated);

    const toolCalls = accumulated.tool_calls ?? [];
    if (toolCalls.length === 0) return;

    for (const tc of toolCalls) {
      emit({ tool: tc.name, args: tc.args });
      const fn = toolMap[tc.name];
      if (!fn) {
        const msg = `Tool not found: ${tc.name}`;
        messages.push(new ToolMessage({ tool_call_id: tc.id, content: msg }));
        emit({ tool: tc.name, result: msg });
        continue;
      }
      try {
        const result = await fn.invoke(tc.args);
        messages.push(new ToolMessage({ tool_call_id: tc.id, content: result }));
        emit({ tool: tc.name, result });
      } catch (e: any) {
        const msg = `Error: ${e.message}`;
        messages.push(new ToolMessage({ tool_call_id: tc.id, content: msg }));
        emit({ tool: tc.name, result: msg });
      }
    }
  }
}

// ── RPC exports ────────────────────────────────────────────────────────

interface ChatMsg { role: string; content: string }

export async function chat(message: string, history: ChatMsg[] = []): Promise<Response> {
  const msgs: any[] = (Array.isArray(history) ? history : []).map((m: ChatMsg) =>
    m.role === "user" ? new HumanMessage(m.content) : new AIMessage(m.content)
  );
  msgs.push(new HumanMessage(message));

  const encoder = new TextEncoder();
  const body = new ReadableStream({
    async start(controller) {
      const emit = (ev: unknown) =>
        controller.enqueue(encoder.encode(`data: ${JSON.stringify(ev)}\n\n`));
      try {
        await reactLoop(msgs, emit);
      } catch (e: any) {
        emit({ error: e?.message ?? String(e) });
      } finally {
        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
        controller.close();
      }
    },
  });

  return new Response(body, {
    headers: {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache, no-transform",
      "X-Accel-Buffering": "no",
    },
  });
}

export function ping() { return "pong"; }
