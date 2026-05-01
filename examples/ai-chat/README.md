# ai-chat

AI SDK v5 chat demo. The client uses
[`useChat`](https://ai-sdk.dev/docs/ai-sdk-ui/chatbot) from
`@ai-sdk/react`; the server is a single zeroship RPC procedure that
emits the AI SDK UI Message Stream Protocol (SSE).

## Run it

```bash
pnpm install
pnpm build
zeroship serve dist/server/index.js --port 3000
# open http://localhost:3000/ to chat
```

`pnpm dev` runs the Vite dev server with HMR if you want a faster
iteration loop.

## How the wire fits together

```
useChat (browser)
  │ POST /_zs/v1/chat
  │ body: { json: { messages: UIMessage[] } }
  ▼
zeroship kernel
  │ slices wireId, parses envelope
  │ calls default.rpc("chat", { messages }, ctx)
  ▼
chat() in src/server.ts
  │ returns Response(SSE-stream, {
  │   "x-vercel-ai-ui-message-stream": "v1",
  │   "content-type": "text/event-stream"
  │ })
  ▼
useChat parses SSE → updates messages[] → React re-renders
```

Two things to notice:

1. **`prepareSendMessagesRequest`** in `Chat.tsx` wraps the AI SDK's
   default `{ messages }` body in the zeroship `{ json: ... }`
   envelope. That's the only client-side adapter needed.
2. **The procedure returns a `Response` directly**, not an async
   iterator. The kernel forwards the SSE stream byte-for-byte to the
   client. (Async-iter returns get re-encoded as the older line-prefixed
   AI SDK v3 protocol, which `useChat` v5 doesn't parse.)

## Plugging in a real model

The default `chat()` is a deterministic mock so the demo works with no
API keys. Swap in OpenAI / Anthropic / etc. by replacing the body with
the AI SDK's `streamText()` helper — see the commented `// REAL MODEL`
block at the bottom of `src/server.ts`.
