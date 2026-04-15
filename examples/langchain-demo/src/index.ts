"use server";

/**
 * LangChain Demo — verifies LangChain runs on the zeroship runtime.
 *
 * Tests:
 * 1. Basic LLM call (ChatOpenAI / ChatAnthropic)
 * 2. Prompt template + chain
 * 3. Structured output (JSON)
 * 4. Streaming (token-by-token)
 * 5. Tool calling
 * 6. Multi-step chain (RAG-lite)
 */

import { ChatOpenAI } from "@langchain/openai";
import { ChatAnthropic } from "@langchain/anthropic";
import { ChatPromptTemplate } from "@langchain/core/prompts";
import { StringOutputParser, JsonOutputParser } from "@langchain/core/output_parsers";
import { RunnableSequence } from "@langchain/core/runnables";
import { tool } from "@langchain/core/tools";
import { z } from "zod";

// ---------------------------------------------------------------------------
// LLM setup — uses environment variables for API keys
// ---------------------------------------------------------------------------

function getModel(provider: "openai" | "anthropic" = "openai") {
  if (provider === "anthropic") {
    return new ChatAnthropic({
      model: "claude-sonnet-4-20250514",
      temperature: 0,
      // API key from process.env.ANTHROPIC_API_KEY (needs polyfill on zeroship)
    });
  }
  return new ChatOpenAI({
    model: "gpt-4o-mini",
    temperature: 0,
    // API key from process.env.OPENAI_API_KEY
  });
}

// ---------------------------------------------------------------------------
// Test 1: Basic LLM call
// ---------------------------------------------------------------------------

export async function basicCall() {
  const model = getModel();
  const response = await model.invoke("What is 2 + 2? Reply with just the number.");
  return { result: response.content };
}

// ---------------------------------------------------------------------------
// Test 2: Prompt template + chain
// ---------------------------------------------------------------------------

export async function promptChain() {
  const model = getModel();
  const prompt = ChatPromptTemplate.fromMessages([
    ["system", "You are a helpful assistant that translates {input_language} to {output_language}."],
    ["human", "{text}"],
  ]);

  const chain = prompt.pipe(model).pipe(new StringOutputParser());

  const result = await chain.invoke({
    input_language: "English",
    output_language: "French",
    text: "Hello, how are you?",
  });

  return { translation: result };
}

// ---------------------------------------------------------------------------
// Test 3: Structured output (JSON)
// ---------------------------------------------------------------------------

export async function structuredOutput() {
  const model = getModel();
  const prompt = ChatPromptTemplate.fromMessages([
    ["system", "Extract the name and age from the text. Respond with JSON: {{\"name\": \"...\", \"age\": number}}"],
    ["human", "{text}"],
  ]);

  const chain = prompt.pipe(model).pipe(new JsonOutputParser());

  const result = await chain.invoke({
    text: "My name is Alice and I am 30 years old.",
  });

  return { parsed: result };
}

// ---------------------------------------------------------------------------
// Test 4: Streaming (token-by-token)
// ---------------------------------------------------------------------------

export async function streamingCall() {
  const model = getModel();
  const prompt = ChatPromptTemplate.fromMessages([
    ["human", "Write a haiku about programming."],
  ]);

  const chain = prompt.pipe(model).pipe(new StringOutputParser());

  // .stream() returns an async iterable of chunks
  const stream = await chain.stream({});

  const chunks: string[] = [];
  for await (const chunk of stream) {
    chunks.push(chunk);
  }

  return {
    fullText: chunks.join(""),
    chunkCount: chunks.length,
    streamingWorks: chunks.length > 1, // should be many small chunks
  };
}

// ---------------------------------------------------------------------------
// Test 5: Tool calling
// ---------------------------------------------------------------------------

const weatherTool = tool(
  async ({ city }: { city: string }) => {
    // Mock weather API — in a real app, this would call a weather service
    const temps: Record<string, number> = {
      "New York": 72,
      "London": 59,
      "Tokyo": 68,
    };
    const temp = temps[city] ?? 65;
    return `The temperature in ${city} is ${temp}°F.`;
  },
  {
    name: "get_weather",
    description: "Get the current weather in a city",
    schema: z.object({
      city: z.string().describe("The city name"),
    }),
  }
);

export async function toolCalling() {
  const model = getModel().bindTools([weatherTool]);

  const response = await model.invoke("What's the weather in Tokyo?");

  // Check if the model requested a tool call
  if (response.tool_calls && response.tool_calls.length > 0) {
    const toolCall = response.tool_calls[0];
    const toolResult = await weatherTool.invoke(toolCall.args as { city: string });
    return {
      toolUsed: toolCall.name,
      toolArgs: toolCall.args,
      toolResult,
    };
  }

  return { result: response.content, toolUsed: null };
}

// ---------------------------------------------------------------------------
// Test 6: Multi-step chain
// ---------------------------------------------------------------------------

export async function multiStepChain() {
  const model = getModel();

  // Step 1: Generate a topic
  const topicChain = ChatPromptTemplate.fromMessages([
    ["human", "Pick a random programming concept. Reply with just the concept name."],
  ]).pipe(model).pipe(new StringOutputParser());

  // Step 2: Explain it
  const explainChain = ChatPromptTemplate.fromMessages([
    ["human", "Explain {topic} in one sentence, for a beginner."],
  ]).pipe(model).pipe(new StringOutputParser());

  // Step 3: Generate a quiz question
  const quizChain = ChatPromptTemplate.fromMessages([
    ["human", "Create a multiple choice question about {topic}. Format:\nQ: ...\nA) ...\nB) ...\nC) ...\nD) ...\nAnswer: ..."],
  ]).pipe(model).pipe(new StringOutputParser());

  // Run the pipeline
  const topic = await topicChain.invoke({});
  const explanation = await explainChain.invoke({ topic });
  const quiz = await quizChain.invoke({ topic });

  return { topic, explanation, quiz };
}

// ---------------------------------------------------------------------------
// Run all tests
// ---------------------------------------------------------------------------

export async function runAllTests() {
  const results: Record<string, { success: boolean; data?: unknown; error?: string }> = {};

  for (const [name, fn] of Object.entries({
    basicCall,
    promptChain,
    structuredOutput,
    streamingCall,
    toolCalling,
    multiStepChain,
  })) {
    try {
      const data = await fn();
      results[name] = { success: true, data };
    } catch (e) {
      results[name] = { success: false, error: e instanceof Error ? e.message : String(e) };
    }
  }

  return results;
}
