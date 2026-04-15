"use server";

/**
 * LangChain AI chatbot — runs in zeroship V8 runtime.
 * Uses ChatOpenAI with tool calling (manual ReAct loop).
 */

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
  model = new ChatOpenAI({ model: "gpt-4.1-mini", temperature: 0 }).bindTools(tools);
  return model;
}

const toolMap = Object.fromEntries(tools.map(t => [t.name, t]));

// ── ReAct loop (manual — avoids LangGraph chunking issues) ─────────────

async function reactLoop(messages: any[]): Promise<string> {
  const m = getModel();
  // Max 5 iterations to prevent infinite loops
  for (let i = 0; i < 5; i++) {
    const response = await m.invoke(messages);
    messages.push(response);

    // If no tool calls, return the content
    if (!response.tool_calls || response.tool_calls.length === 0) {
      return typeof response.content === "string" ? response.content : JSON.stringify(response.content);
    }

    // Execute tool calls
    for (const tc of response.tool_calls) {
      const fn = toolMap[tc.name];
      if (!fn) {
        messages.push(new ToolMessage({ tool_call_id: tc.id, content: `Tool not found: ${tc.name}` }));
        continue;
      }
      try {
        const result = await fn.invoke(tc.args);
        messages.push(new ToolMessage({ tool_call_id: tc.id, content: result }));
      } catch (e: any) {
        messages.push(new ToolMessage({ tool_call_id: tc.id, content: `Error: ${e.message}` }));
      }
    }
  }
  return "Max iterations reached.";
}

// ── RPC exports ────────────────────────────────────────────────────────

interface ChatMsg { role: string; content: string }

export async function chat(message: string, history: ChatMsg[] = {}): Promise<ChatMsg> {
  try {
    const messages: any[] = history.map((m: ChatMsg) =>
      m.role === "user" ? new HumanMessage(m.content) : new AIMessage(m.content)
    );
    messages.push(new HumanMessage(message));

    const content = await reactLoop(messages);
    return { role: "assistant", content };
  } catch (e: any) {
    return { role: "assistant", content: `Error: ${e.message}` };
  }
}

export function ping() { return "pong"; }
