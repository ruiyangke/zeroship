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
// Async generator — each `yield` produces an event the client receives
// directly. The zeroship runtime wraps the generator into an SSE
// Response automatically; the vite-plugin client stub exposes this
// as an AsyncIterable<{token?: string; tool?: string; ...}>.

interface ChatMsg { role: string; content: string }

export async function* chat(message: string, history: ChatMsg[] = []) {
  const msgs: any[] = (Array.isArray(history) ? history : []).map((m: ChatMsg) =>
    m.role === "user" ? new HumanMessage(m.content) : new AIMessage(m.content)
  );
  msgs.push(new HumanMessage(message));

  const m = getModel();
  for (let step = 0; step < 5; step++) {
    const stream: any = await m.stream(msgs);

    let accumulated: any = null;
    for await (const chunk of stream) {
      accumulated = accumulated == null ? chunk : accumulated.concat(chunk);
      const delta = typeof chunk.content === "string" ? chunk.content : "";
      if (delta) yield { token: delta };
    }
    if (accumulated == null) return;
    msgs.push(accumulated);

    const toolCalls = accumulated.tool_calls ?? [];
    if (toolCalls.length === 0) return;

    for (const tc of toolCalls) {
      yield { tool: tc.name, args: tc.args };
      const fn = toolMap[tc.name];
      if (!fn) {
        const msg = `Tool not found: ${tc.name}`;
        msgs.push(new ToolMessage({ tool_call_id: tc.id, content: msg }));
        yield { tool: tc.name, result: msg };
        continue;
      }
      try {
        const result = await fn.invoke(tc.args);
        msgs.push(new ToolMessage({ tool_call_id: tc.id, content: result }));
        yield { tool: tc.name, result };
      } catch (e: any) {
        const msg = `Error: ${e.message}`;
        msgs.push(new ToolMessage({ tool_call_id: tc.id, content: msg }));
        yield { tool: tc.name, result: msg };
      }
    }
  }
}

export function ping() { return "pong"; }
