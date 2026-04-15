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

async function reactLoopStream(
  messages: any[],
  onToken: (token: string) => void,
  onToolCall: (name: string, args: any) => void,
  onToolResult: (name: string, result: string) => void,
): Promise<void> {
  const m = getModel();
  for (let i = 0; i < 5; i++) {
    const stream = await m.stream(messages);
    let fullResponse: any = null;

    for await (const chunk of stream) {
      if (!fullResponse) fullResponse = chunk;
      else fullResponse = fullResponse.concat(chunk);
      if (chunk.content && typeof chunk.content === "string") {
        onToken(chunk.content);
      }
    }

    if (!fullResponse) return;
    messages.push(fullResponse);

    if (!fullResponse.tool_calls || fullResponse.tool_calls.length === 0) return;

    for (const tc of fullResponse.tool_calls) {
      onToolCall(tc.name, tc.args);
      const fn = toolMap[tc.name];
      if (!fn) {
        messages.push(new ToolMessage({ tool_call_id: tc.id, content: `Tool not found: ${tc.name}` }));
        onToolResult(tc.name, `Tool not found: ${tc.name}`);
        continue;
      }
      try {
        const result = await fn.invoke(tc.args);
        messages.push(new ToolMessage({ tool_call_id: tc.id, content: result }));
        onToolResult(tc.name, result);
      } catch (e: any) {
        messages.push(new ToolMessage({ tool_call_id: tc.id, content: `Error: ${e.message}` }));
        onToolResult(tc.name, `Error: ${e.message}`);
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

  // Collect all ReAct events first, then stream them as SSE.
  // True token-by-token streaming from OpenAI requires fixing nested
  // async ReadableStream support in the single-threaded event loop.
  const events: string[] = [];
  const encoder = new TextEncoder();

  try {
    const m = getModel();
    for (let i = 0; i < 5; i++) {
      const response = await m.invoke(msgs);
      msgs.push(response);

      if (response.content && typeof response.content === "string") {
        events.push(JSON.stringify({ token: response.content }));
      }

      if (!response.tool_calls || response.tool_calls.length === 0) break;

      for (const tc of response.tool_calls) {
        events.push(JSON.stringify({ tool: tc.name, args: tc.args }));
        const fn = toolMap[tc.name];
        const result = fn ? await fn.invoke(tc.args) : `Tool not found: ${tc.name}`;
        events.push(JSON.stringify({ tool: tc.name, result }));
        msgs.push(new ToolMessage({ tool_call_id: tc.id, content: result }));
      }
    }
  } catch (e: any) {
    events.push(JSON.stringify({ error: e.message }));
  }

  const stream = new ReadableStream({
    start(controller: any) {
      for (const evt of events) {
        controller.enqueue(encoder.encode(`data: ${evt}\n\n`));
      }
      controller.enqueue(encoder.encode("data: [DONE]\n\n"));
      controller.close();
    },
  });

  return new Response(stream, {
    headers: { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" },
  });
}

export function ping() { return "pong"; }
