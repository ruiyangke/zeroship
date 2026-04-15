"use server";

/**
 * LangGraph ReAct agent with tools — runs in zeroship V8 runtime.
 *
 * Tools:
 *   - calculator: evaluate math expressions
 *   - weather: get current weather for a city
 *   - datetime: get current date/time info
 */

import { ChatOpenAI } from "@langchain/openai";
import { createReactAgent } from "@langchain/langgraph/prebuilt";
import { tool } from "@langchain/core/tools";
import { z } from "zod";

// ── Tools ──────────────────────────────────────────────────────────────

const calculator = tool(
  async ({ expression }: { expression: string }) => {
    try {
      // Safe eval for math: only allows numbers, operators, parens, Math.*
      const sanitized = expression.replace(/[^0-9+\-*/().,%\s]|(?:Math\.(?:sqrt|pow|abs|floor|ceil|round|min|max|PI|E|log|sin|cos|tan))/g, "");
      if (sanitized !== expression) {
        return `Error: expression contains invalid characters. Only numbers, +, -, *, /, (), and Math functions are allowed.`;
      }
      const result = new Function(`"use strict"; return (${expression})`)();
      return `${expression} = ${result}`;
    } catch (e: any) {
      return `Error evaluating "${expression}": ${e.message}`;
    }
  },
  {
    name: "calculator",
    description: "Evaluate a mathematical expression. Supports +, -, *, /, (), and Math functions (Math.sqrt, Math.pow, Math.PI, etc.)",
    schema: z.object({
      expression: z.string().describe("The math expression to evaluate, e.g. '(2 + 3) * 4' or 'Math.sqrt(144)'"),
    }),
  }
);

const weather = tool(
  async ({ city }: { city: string }) => {
    // Mock weather data — in production, call a real weather API
    const conditions: Record<string, { temp: number; condition: string; humidity: number }> = {
      "new york": { temp: 72, condition: "Partly cloudy", humidity: 55 },
      "london": { temp: 59, condition: "Overcast", humidity: 78 },
      "tokyo": { temp: 68, condition: "Clear", humidity: 45 },
      "paris": { temp: 64, condition: "Light rain", humidity: 82 },
      "sydney": { temp: 75, condition: "Sunny", humidity: 40 },
      "san francisco": { temp: 61, condition: "Foggy", humidity: 88 },
    };
    const data = conditions[city.toLowerCase()] ?? { temp: 65, condition: "Unknown", humidity: 50 };
    return `Weather in ${city}: ${data.temp}°F, ${data.condition}, humidity ${data.humidity}%`;
  },
  {
    name: "weather",
    description: "Get current weather for a city. Returns temperature, condition, and humidity.",
    schema: z.object({
      city: z.string().describe("The city name, e.g. 'New York', 'Tokyo'"),
    }),
  }
);

const datetime = tool(
  async () => {
    const now = new Date();
    return `Current date/time: ${now.toISOString()}. Day: ${now.toLocaleDateString("en-US", { weekday: "long" })}. Unix timestamp: ${Math.floor(now.getTime() / 1000)}`;
  },
  {
    name: "datetime",
    description: "Get the current date, time, day of week, and unix timestamp.",
    schema: z.object({}),
  }
);

// ── Agent ──────────────────────────────────────────────────────────────

let agent: any = null;

function getAgent() {
  if (agent) return agent;

  const model = new ChatOpenAI({
    model: "gpt-4.1-mini",
    temperature: 0,
    streaming: true,
  });

  agent = createReactAgent({
    llm: model,
    tools: [calculator, weather, datetime],
  });

  return agent;
}

// ── Exports ────────────────────────────────────────────────────────────

export interface ChatMessage {
  role: "user" | "assistant" | "tool";
  content: string;
  toolName?: string;
}

/**
 * Send a message to the agent (non-streaming).
 */
export async function chat(message: string, history: ChatMessage[] = []): Promise<ChatMessage> {
  const a = getAgent();

  const messages = [
    ...history.map((m) => ({
      role: m.role as string,
      content: m.content,
    })),
    { role: "user", content: message },
  ];

  const result = await a.invoke({ messages });
  const lastMessage = result.messages[result.messages.length - 1];

  return {
    role: "assistant",
    content: typeof lastMessage.content === "string" ? lastMessage.content : JSON.stringify(lastMessage.content),
  };
}

/**
 * Stream a response from the agent, token by token.
 * Returns an SSE-compatible Response with streaming body.
 */
export async function chatStream(message: string, history: ChatMessage[] = []): Promise<any> {
  const a = getAgent();

  const messages = [
    ...history.map((m) => ({
      role: m.role as string,
      content: m.content,
    })),
    { role: "user", content: message },
  ];

  const stream = await a.stream({ messages }, { streamMode: "messages" });

  const encoder = new TextEncoder();
  const readable = new ReadableStream({
    async start(controller) {
      try {
        for await (const [message, _metadata] of stream) {
          if (message.content && typeof message.content === "string") {
            const data = JSON.stringify({ token: message.content, type: message._getType() });
            controller.enqueue(encoder.encode(`data: ${data}\n\n`));
          }
        }
        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
        controller.close();
      } catch (e: any) {
        controller.enqueue(encoder.encode(`data: ${JSON.stringify({ error: e.message })}\n\n`));
        controller.close();
      }
    },
  });

  return new Response(readable, {
    headers: {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache",
    },
  });
}
