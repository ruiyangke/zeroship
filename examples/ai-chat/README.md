# ai-chat

AI SDK v5 chat demo. The client uses
[`useChat`](https://ai-sdk.dev/docs/ai-sdk-ui/chatbot) from
`@ai-sdk/react`; the server is a single zeroship RPC procedure that
calls OpenAI via `@ai-sdk/openai` and streams the response.

## Run it

Set `OPENAI_API_KEY` in `.env` (gitignored). Then either:

**Dev (recommended for iteration):**

```bash
pnpm install
pnpm dev
```

The vite-plugin's dev-server sources `.env`, builds the client with HMR,
and spawns the zeroship runtime against `dev-bootstrap.js`.

**Production-like (built bundle, no HMR):**

```bash
pnpm build
OPENAI_API_KEY=$(grep OPENAI_API_KEY .env | cut -d= -f2) \
  zeroship serve dist/server/index.js --port 3000
```

Open http://localhost:3000/ to chat.

## How the wire fits together

```
useChat (browser)
  │ POST /__zeroship/v1/chat
  │ body: { json: { messages: UIMessage[] } }
  ▼
zeroship kernel
  │ slices wireId, parses envelope
  │ calls default.rpc("chat", { messages }, ctx)
  ▼
chat() in src/server.ts
  │ streamText({ model: openai("gpt-4o-mini"), messages })
  │ result.toUIMessageStreamResponse()  ← v5 SSE wire
  ▼
zeroship kernel inspect_response forwards SSE bytes verbatim
  ▼
useChat parses SSE → updates messages[] → React re-renders
```

Two adapters bridge zeroship and the AI SDK:

1. **`prepareSendMessagesRequest`** (`Chat.tsx`) wraps the AI SDK's
   default `{ messages }` body in zeroship's `{ json: ... }` envelope.
2. **The procedure returns a `Response` directly** so the kernel
   forwards SSE bytes byte-for-byte. Async-iter returns get
   re-encoded as the older line-prefixed v3 protocol that v5 `useChat`
   doesn't parse.

## Behind the scenes

The runtime gained two pieces of WHATWG plumbing to make the AI SDK
work without modification:

- **`TextEncoderStream` / `TextDecoderStream`** — WHATWG transform
  streams that wrap `TextEncoder` / `TextDecoder`. The AI SDK's
  `toUIMessageStreamResponse()` ends with
  `pipeThrough(new TextEncoderStream())`. Implemented as ~50 LOC
  of JS in `crates/runtime/src/embed/text-streams.js`, layered on
  top of native `TransformStream` (see
  `docs/proposals/streams-native.md`).

- **Stream-body forwarder** — when a handler returns
  `new Response(stream)`, the kernel's `inspect_response` calls
  `__zsBeginStreamForward(response)` (in `embed/fetch.js`) which
  locks the body via `getReader()` and pumps each chunk into a
  Rust-side StreamState identified by `response._streamId`. Works
  against any class implementing the spec ReadableStream surface;
  no reach into private fields.

- **`process.env` preservation in SSR builds** —
  `target: "webworker"` makes Rolldown statically rewrite
  `process.env` to `{}`. The vite-plugin now adds
  `define: { "process.env": "process.env" }` to keep the references
  intact so the OpenAI provider reads `OPENAI_API_KEY` at runtime
  from the env the runtime injects.
