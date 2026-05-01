// Typed RPC client for the builder's chat surface. Mirrors
// `examples/ai-chat/src/api.ts` exactly: declare an `App` type listing
// every procedure with its kind + input/output, then `client<App>`
// returns a typed proxy.
//
// `rpc.chat.streamUrl()` is the AI-SDK entry point — `@zeroship/rpc-client`
// exposes it explicitly so we can hand the URL to `useChat` without
// hardcoding `/_zs/v1/chat`. The transport-level body envelope
// (`{ json: <input> }`) is wrapped in `chatTransport` below.

import { client, type ProcedureType } from "@zeroship/rpc-client";
import { DefaultChatTransport } from "ai";
import type { UIMessage } from "ai";

// One procedure to start with — Plan 02 may add more (file CRUD, deploy,
// etc.) and they go here as additional fields on `App`.
type App = {
  chat: ProcedureType<"stream", { messages: UIMessage[] }, never>;
};

export const rpc = client<App>({ baseUrl: "" });

/**
 * AI SDK transport bound to a streaming RPC procedure. Wraps the
 * `useChat` request shape in zeroship's superjson envelope
 * (`{ json: ... }`) and points at the procedure's stream URL.
 *
 * Two send shapes flow through this transport:
 *   1. Normal turn — `sendMessage({ text })` from a composer:
 *      wire body = `{ json: { messages: UIMessage[], id } }`.
 *   2. Resume turn — `sendMessage(_, { body: { resume: {token,value} }})`
 *      after a SurveyCard submit (server tool halted via
 *      `interrupt()`): wire body = `{ json: { resume, id } }`. Messages
 *      are stripped because the server feeds `Command({resume})` into
 *      the existing thread instead of replaying history.
 *
 * Usage:
 *   const { messages, sendMessage } = useChat({
 *     transport: chatTransport(rpc.chat),
 *   });
 */
export function chatTransport<TIn>(handle: {
  streamUrl: (input?: TIn) => string | Promise<string>;
}) {
  return new DefaultChatTransport({
    // streamUrl() with no input returns a synchronous URL
    // (`/_zs/v1/<id>`). With `transformer: "superjson"` set on the
    // client it would return a Promise — fine for `api`, which
    // DefaultChatTransport accepts as either form.
    api: handle.streamUrl() as string,
    prepareSendMessagesRequest: ({ messages, id, body }) => {
      // Resume payload from a SurveyCard (or any future interrupt).
      // ChatRail attaches it via `sendMessage(_, { body: { resume } })`.
      // The server's chat.ts treats `body.json.resume` as the cue to
      // skip message replay and feed Command({resume}) into the same
      // thread.
      const resume = (body as { resume?: { token: string; value: unknown } } | undefined)?.resume;
      if (resume) {
        return { body: { json: { resume, id } } };
      }
      return { body: { json: { messages, id } } };
    },
  });
}
