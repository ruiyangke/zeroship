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

export const getMessages = query(async () => messages, {
  id: "getMessages",
  output: z.array(MessageSchema),
});

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
