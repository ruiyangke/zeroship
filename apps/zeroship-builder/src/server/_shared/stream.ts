"use server";
// AI SDK stream protocol helpers.
// AI SDK uses Server-Sent Events with each chunk being one of these types.
// Reference: https://sdk.vercel.ai/docs/ai-sdk-ui/stream-protocol

export type AIStreamChunk =
  | { type: "text-delta"; delta: string }
  | { type: "tool-call"; toolCallId: string; toolName: string; args: unknown }
  | { type: "tool-result"; toolCallId: string; result: unknown }
  | { type: "data-part"; partName: string; payload: unknown }
  | { type: "error"; message: string }
  | { type: "finish"; usage?: { inputTokens?: number; outputTokens?: number } };

/** Encode a single chunk as the AI SDK SSE wire format. */
export function encodeChunk(chunk: AIStreamChunk): string {
  // The wire format the @ai-sdk/react useChat hook understands is one of
  // several "stream protocols". We use the data-stream protocol for typed
  // chunks. Each line is `<type-prefix>:<payload-json>\n` for the legacy
  // format, OR newline-delimited JSON for the newer protocol used in AI SDK 4.
  // For Plan 01 we ship the newline-delimited JSON form which @ai-sdk/react
  // accepts when the response Content-Type is "text/plain" with proper headers.
  return JSON.stringify(chunk) + "\n";
}

/** Build a Response that streams from an async iterable of chunks. */
export function streamResponse(
  source: AsyncIterable<AIStreamChunk>,
): Response {
  const encoder = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    async start(controller) {
      try {
        for await (const chunk of source) {
          controller.enqueue(encoder.encode(encodeChunk(chunk)));
        }
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        controller.enqueue(encoder.encode(encodeChunk({ type: "error", message })));
      } finally {
        controller.enqueue(encoder.encode(encodeChunk({ type: "finish" })));
        controller.close();
      }
    },
  });
  return new Response(stream, {
    headers: {
      "Content-Type": "text/plain; charset=utf-8",
      "Cache-Control": "no-cache, no-transform",
      "X-Accel-Buffering": "no",
    },
  });
}

/** Convenience: a delay that yields control. Used by the mock to feel realistic. */
export function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
