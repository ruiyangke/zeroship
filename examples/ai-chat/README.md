# ai-chat

AI SDK v5 chat demo. The client uses
[`useChat`](https://ai-sdk.dev/docs/ai-sdk-ui/chatbot) from
`@ai-sdk/react`; the server is a single zeroship RPC procedure that
calls OpenAI via `@ai-sdk/openai` and streams the response.

## Run it

The app reads its provider from its own environment: `OPENAI_API_KEY` (the
key) and, for an OpenAI-compatible endpoint, `OPENAI_BASE_URL` (the base
URL). In a deployment those are an app secret and an app variable:

```bash
zeroship secret set OPENAI_API_KEY=sk-... --app=<app-id>
zeroship var set OPENAI_BASE_URL=https://api.openai.com/v1 --app=<app-id>
```

Locally the same values come from `.env` (gitignored) with the `ZS_VAR_`
prefix the dev server strips into `env`:

```dotenv
ZS_VAR_OPENAI_API_KEY=sk-...
ZS_VAR_OPENAI_BASE_URL=https://api.openai.com/v1
```

Then either:

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
ZS_VAR_OPENAI_API_KEY=$(grep ZS_VAR_OPENAI_API_KEY .env | cut -d= -f2) \
  zeroship serve dist/server/index.js --port 3000
```

Open http://localhost:3000/ to chat.

## Tests

`pnpm test:e2e` runs hermetically. The Playwright web-server fixture starts a
local OpenAI-compatible stub and points the app at it with
`ZS_VAR_OPENAI_BASE_URL` / `ZS_VAR_OPENAI_API_KEY` - the same configuration
surface a creator uses - so no real key and no network to OpenAI are needed.
The stub streams several deltas in the chat-completions SSE wire and records
every request; the suite asserts the app sent the expected model, messages and
key, and that a provider error surfaces in the chat UI.

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
  │ streamText({ model: openai.chat("gpt-5-nano"), messages })
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

- **`env` for provider configuration** - the app builds its provider from
  `env.OPENAI_API_KEY` and `env.OPENAI_BASE_URL`. Those are app values the
  runtime seeds before the worker evaluates creator code, so a deployment and
  a local dev server supply the provider the same way; the app never depends
  on a shell environment variable.
