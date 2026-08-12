"use server";
// This directive opts the module into zeroship RPC discovery. The Vite plugin
// scans named exports from `"use server"` files, then publishes only exports
// wrapped with `query`, `mutation`, `stream`, or `subscription` from
// `@zeroship/rpc/server`. Plain helpers, constants, and unwrapped exports stay
// private to the server bundle.
//
// Each wrapper takes the handler plus a config object with an explicit stable
// `id`. Input and output schemas are Zod schemas from `@zeroship/server`; the
// runtime parses inputs at the wire boundary before the handler runs.

import { mutation, query } from "@zeroship/rpc/server";
import { z } from "@zeroship/server";

export type Message = {
  id: number;
  text: string;
  createdAt: number;
};

const MessageSchema = z.object({
  id: z.number(),
  text: z.string(),
  createdAt: z.number(),
});

let nextId = 3;
const messages: Message[] = [
  { id: 1, text: "Build locally with an AI coding agent.", createdAt: Date.now() - 60_000 },
  { id: 2, text: "Run pnpm build to produce dist/app.zship.", createdAt: Date.now() - 30_000 },
];

export const getMessages = query(
  async () => {
    // Server-side `console` output is a real platform affordance, and this is
    // the only line in the starter that exercises it. In `pnpm dev` it lands in
    // the terminal running the dev runtime. Deployed, the worker captures it
    // per request into a per-app ring buffer and the control plane serves it
    // back at `GET /api/apps/<id>/logs` -- which is how a creator sees what
    // their app printed once it is no longer running on their machine.
    console.log(`[starter] getMessages -> ${messages.length} message(s)`);
    return messages;
  },
  {
    id: "getMessages",
    output: z.array(MessageSchema),
  },
);

// A procedure that always throws. It exists so the platform's error path can be
// exercised on a REAL deployed app: golden_path step 13 drives it and then asks
// `GET /api/apps/<id>/logs` whether a creator can see anything about a request
// their own code failed. The throw is inside the handler on purpose -- an input
// rejection is refused before the handler runs, so it cannot tell "the error
// rail does not deliver" from "no JS ever executed".
export const boom = query(
  async () => {
    throw new Error("[starter] boom: deliberate handler failure");
  },
  {
    id: "boom",
    output: z.object({ never: z.string() }),
  },
);

export const addMessage = mutation(
  async (input: { text: string }) => {
    const message: Message = {
      id: nextId++,
      text: input.text,
      createdAt: Date.now(),
    };
    messages.push(message);
    return message;
  },
  {
    id: "addMessage",
    input: z.object({ text: z.string().min(1).max(280) }),
    output: MessageSchema,
  },
);
