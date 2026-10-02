// A local OpenAI-compatible provider.
//
// The app's client library parses the chat-completions streaming wire:
// `data: {json}` frames followed by `data: [DONE]`. This server speaks that
// wire, records every request it receives for the spec to inspect, and
// refuses a request that does not carry the model, messages or key the app is
// configured to send - so a suite that passes proves the app actually called
// the provider, not that a reply was cached somewhere.

import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import {
  ERROR_MESSAGE,
  ERROR_PROMPT,
  MODEL,
  STREAM_CHUNKS,
  STREAM_PROMPT,
  STUB_API_KEY,
  messageText,
} from "./settings.ts";

export interface CapturedRequest {
  path: string;
  model: string;
  messages: Array<{ role: string; content: unknown }>;
  authorization: string | null;
}

export interface ProviderStub {
  /** Requests seen so far, in arrival order. */
  requests: CapturedRequest[];
  reset(): void;
  close(): Promise<void>;
}

function readBody(request: IncomingMessage): Promise<string> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    request.on("data", (chunk: Buffer) => chunks.push(chunk));
    request.on("error", reject);
    request.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
  });
}

function json(response: ServerResponse, status: number, body: unknown): void {
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

function frame(response: ServerResponse, body: unknown): void {
  response.write(`data: ${JSON.stringify(body)}\n\n`);
}

function delta(content: string | undefined, finishReason: string | null): unknown {
  return {
    id: "chatcmpl-stub",
    object: "chat.completion.chunk",
    created: 0,
    model: MODEL,
    choices: [
      {
        index: 0,
        delta: { role: "assistant", ...(content === undefined ? {} : { content }) },
        finish_reason: finishReason,
      },
    ],
  };
}

export function startProviderStub(port: number, host: string): Promise<ProviderStub> {
  const requests: CapturedRequest[] = [];
  const server: Server = createServer((request, response) => {
    const url = new URL(request.url ?? "/", `http://${request.headers.host ?? host}`);

    if (request.method === "GET" && url.pathname === "/__requests") {
      json(response, 200, requests);
      return;
    }
    if (request.method === "POST" && url.pathname === "/__reset") {
      requests.length = 0;
      json(response, 200, { ok: true });
      return;
    }
    if (!(request.method === "POST" && url.pathname === "/v1/chat/completions")) {
      json(response, 404, { error: { message: `stub: no route for ${request.method} ${url.pathname}` } });
      return;
    }

    void readBody(request)
      .then((raw) => {
        let body: { model?: unknown; messages?: unknown };
        try {
          body = JSON.parse(raw) as { model?: unknown; messages?: unknown };
        } catch {
          json(response, 400, { error: { message: "stub: request body was not JSON" } });
          return;
        }

        const messages = Array.isArray(body.messages)
          ? (body.messages as Array<{ role: string; content: unknown }>)
          : [];
        const captured: CapturedRequest = {
          path: url.pathname,
          model: typeof body.model === "string" ? body.model : String(body.model),
          messages,
          authorization:
            typeof request.headers.authorization === "string" ? request.headers.authorization : null,
        };
        requests.push(captured);

        const prompts = messages.filter((m) => m.role === "user").map((m) => messageText(m.content));
        if (
          captured.model !== MODEL ||
          !prompts.some((text) => text.includes(STREAM_PROMPT) || text.includes(ERROR_PROMPT))
        ) {
          json(response, 400, {
            error: { message: `stub: unexpected request for model ${captured.model}` },
          });
          return;
        }
        if (captured.authorization !== `Bearer ${STUB_API_KEY}`) {
          json(response, 401, { error: { message: "stub: missing or unexpected api key" } });
          return;
        }
        if (prompts.some((text) => text.includes(ERROR_PROMPT))) {
          json(response, 500, { error: { message: ERROR_MESSAGE } });
          return;
        }

        response.writeHead(200, {
          "content-type": "text/event-stream",
          "cache-control": "no-cache",
          connection: "keep-alive",
        });
        for (const part of STREAM_CHUNKS) frame(response, delta(part, null));
        frame(response, delta(undefined, "stop"));
        response.write("data: [DONE]\n\n");
        response.end();
      })
      .catch((error: unknown) => {
        json(response, 500, {
          error: { message: `stub: ${error instanceof Error ? error.message : String(error)}` },
        });
      });
  });

  return new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, host, () => {
      resolve({
        requests,
        reset: () => {
          requests.length = 0;
        },
        close: () => new Promise((done) => server.close(() => done())),
      });
    });
  });
}
