"use server";

/**
 * LangChain ReAct agent — runs in zeroship V8 runtime.
 * Tools: calculator, weather, datetime.
 */

import { ChatOpenAI } from "@langchain/openai";
import { createReactAgent } from "@langchain/langgraph/prebuilt";
import { tool } from "@langchain/core/tools";
import { z } from "zod";

// ── Tools ──────────────────────────────────────────────────────────────

const calculator = tool(
  async ({ expression }: { expression: string }) => {
    try {
      const result = new Function(`"use strict"; return (${expression})`)();
      return `${expression} = ${result}`;
    } catch (e: any) {
      return `Error: ${e.message}`;
    }
  },
  {
    name: "calculator",
    description: "Evaluate a math expression. Supports +, -, *, /, (), Math.sqrt, Math.PI, etc.",
    schema: z.object({ expression: z.string().describe("Math expression, e.g. '(2+3)*4'") }),
  }
);

const weather = tool(
  async ({ city }: { city: string }) => {
    const data: Record<string, string> = {
      "new york": "72°F, Partly cloudy",
      "london": "59°F, Overcast",
      "tokyo": "68°F, Clear",
      "paris": "64°F, Light rain",
      "san francisco": "61°F, Foggy",
    };
    return `Weather in ${city}: ${data[city.toLowerCase()] ?? "65°F, Unknown"}`;
  },
  {
    name: "weather",
    description: "Get current weather for a city.",
    schema: z.object({ city: z.string().describe("City name") }),
  }
);

const datetime = tool(
  async () => {
    const now = new Date();
    return `${now.toISOString()}, ${now.toLocaleDateString("en-US", { weekday: "long" })}`;
  },
  {
    name: "datetime",
    description: "Get current date and time.",
    schema: z.object({}),
  }
);

// ── Agent ──────────────────────────────────────────────────────────────

let agent: any = null;

function getAgent() {
  if (agent) return agent;
  const model = new ChatOpenAI({ model: "gpt-4.1-mini", temperature: 0 });
  agent = createReactAgent({ llm: model, tools: [calculator, weather, datetime] });
  return agent;
}

// ── RPC exports ────────────────────────────────────────────────────────

// Simple chat — just ChatOpenAI, no agent (for debugging)
export async function chatSimple(message: string) {
  try {
    const model = new ChatOpenAI({ model: "gpt-4.1-mini", temperature: 0 });
    const result = await model.invoke(message);
    return { role: "assistant", content: result.content };
  } catch (e: any) {
    return { error: e.message, stack: e.stack?.split("\n").slice(0, 8) };
  }
}

// Full agent chat
export async function chat(message: string, history: Array<{ role: string; content: string }> = []) {
  try {
    const a = getAgent();
    const messages = [...history.map(m => ({ role: m.role, content: m.content })), { role: "user", content: message }];
    const result = await a.invoke({ messages });
    const last = result.messages[result.messages.length - 1];
    return { role: "assistant", content: typeof last.content === "string" ? last.content : JSON.stringify(last.content) };
  } catch (e: any) {
    return { error: e.message, stack: e.stack?.split("\n").slice(0, 8) };
  }
}

export function ping() { return "pong"; }
