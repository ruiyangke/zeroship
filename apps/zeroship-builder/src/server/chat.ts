"use server";
import {
  type AIStreamChunk,
  delay,
  streamResponse,
} from "./_shared/stream";

export interface ChatTurnInput {
  /** Plain text prompt. */
  text: string;
  /** Image attachments — V1 supports image input only; later expands. */
  images?: Array<{ name: string; mediaType: string; bytes: Uint8Array }>;
}

/**
 * Mock chat — produces a streamed sequence that exercises every
 * data-part shape the client handles.
 *
 * Plan 02 replaces the body with the deepagents → translator pipeline.
 * The wire format (AIStreamChunk) stays the same.
 */
export async function postChat(input: ChatTurnInput): Promise<Response> {
  return streamResponse(generate(input));
}

async function* generate(input: ChatTurnInput): AsyncIterable<AIStreamChunk> {
  // 1. Initial preamble text streaming
  const preamble = "Got it — let me think about that.\n\n";
  for (const ch of preamble) {
    yield { type: "text-delta", delta: ch };
    await delay(15);
  }

  // 2. A survey data part (asks one clarifying question)
  yield {
    type: "data-part",
    partName: "survey",
    payload: {
      preamble: "A quick thing first:",
      questions: [
        {
          id: "vibe",
          prompt: "Vibe?",
          kind: {
            type: "single_choice",
            options: [
              { value: "cozy",     label: "cozy / warm" },
              { value: "minimal",  label: "minimal" },
              { value: "playful",  label: "playful" },
            ],
          },
          default: "minimal",
        },
      ],
      skip_label: "skip — just build",
    },
  };

  // For Plan 01 the mock proceeds whether or not the user answers
  // (we don't yet wait on a real response from the client).
  await delay(800);

  // 3. Resume text
  const resume = "\nOK — I'll start by writing a small file.\n\n";
  for (const ch of resume) {
    yield { type: "text-delta", delta: ch };
    await delay(12);
  }

  // 4. A tool call (write_file)
  const toolCallId = "tool_" + Math.random().toString(36).slice(2, 10);
  yield {
    type: "tool-call",
    toolCallId,
    toolName: "write_file",
    args: { path: "src/index.tsx", contents_preview: "<… mock contents …>" },
  };
  await delay(600);

  yield {
    type: "tool-result",
    toolCallId,
    result: { ok: true, bytes_written: 312 },
  };

  // 5. A diff card
  yield {
    type: "data-part",
    partName: "diff",
    payload: {
      path: "src/index.tsx",
      before: "",
      after:
        "import { render } from 'react-dom';\n" +
        "render(<h1>Hello</h1>, document.body);\n",
    },
  };

  // 6. A critic round
  yield {
    type: "data-part",
    partName: "critic-round",
    payload: { round: 1, total: 3, approved: true, issues: [] },
  };

  // 7. Final text
  const trailer = "\nDone. (This is the Plan 01 mock — Plan 02 wires the real Builder agent.)\n";
  for (const ch of trailer) {
    yield { type: "text-delta", delta: ch };
    await delay(10);
  }
}
