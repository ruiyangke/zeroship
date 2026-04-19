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
  model = new ChatOpenAI({ model: "gpt-4o-mini", temperature: 0 }).bindTools(tools);
  return model;
}
const toolMap = Object.fromEntries(tools.map(t => [t.name, t]));

// ── Non-streaming ReAct loop ───────────────────────────────────────────
//
// NOTE: .stream() on LangChain's model currently returns a stream whose
// async iterator yields zero items in the zeroship V8 runtime (to debug).
// Using .invoke() for now — tokens arrive all at once instead of
// incrementally, but the chat works end-to-end.

async function reactLoop(
  messages: any[],
  emit: (ev: any) => void,
): Promise<void> {
  const m = getModel();
  for (let i = 0; i < 5; i++) {
    const response: any = await m.invoke(messages);
    messages.push(response);

    if (response.content && typeof response.content === "string") {
      emit({ token: response.content });
    }

    if (!response.tool_calls || response.tool_calls.length === 0) return;

    for (const tc of response.tool_calls) {
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

export async function chat(message: string, history: ChatMsg[] = []): Promise<any> {
  const msgs: any[] = (Array.isArray(history) ? history : []).map((m: ChatMsg) =>
    m.role === "user" ? new HumanMessage(m.content) : new AIMessage(m.content)
  );
  msgs.push(new HumanMessage(message));

  // NOTE: the user-facing SSE stream is disabled for now — returning a
  // Response(ReadableStream) through the V8→HTTP path appears to buffer
  // or drop the body (separate runtime bug). Collect events and return
  // as JSON so the demo's chat() actually produces output.
  const events: any[] = [];
  try {
    await reactLoop(msgs, (ev) => events.push(ev));
  } catch (e: any) {
    events.push({ error: e.message });
  }

  // Pull the final assistant content out of collected token events.
  const reply = events
    .filter((e) => typeof e.token === "string")
    .map((e) => e.token)
    .join("");
  return { reply, events };
}

export function ping() { return "pong"; }
