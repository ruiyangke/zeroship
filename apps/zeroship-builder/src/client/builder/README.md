# AI Builder (chat-primary)

A Bolt-style AI app builder for the zeroship platform. The creator
describes what they want; the agent generates a complete `export
default { fetch }` module, deploys it, and the iframe shows the live
result.

**Routes** (in `src/App.tsx`):

| Path | Component | Purpose |
|---|---|---|
| `/builder` | `Builder` (bootstrap mode) | First message provisions a new app via `create_app` |
| `/builder/:appId` | `Builder` (project mode) | Iterate on an existing app |

These routes own the full viewport — no Layout chrome — because chat-primary needs every pixel.

## Layout

```
┌────────── TopBar ─────────────────────────────────────────────┐
├── Chat (400px) ─┬── CodePanel (toggle) ─┬── Preview (rest) ───┤
│                 │ CodeMirror 6, hidden  │ <iframe>             │
│ Msgs + composer │ behind "Show code"    │ /apps/{name}/        │
└─────────────────┴───────────────────────┴──────────────────────┘
```

## Files

- `pages/Builder.tsx` — top-level page; routes between bootstrap & project modes.
- `builder/types.ts` — wire types for SSE events + chat state.
- `builder/agentClient.ts` — POST to the agent's `/chat`, parse SSE.
- `builder/storage.ts` — localStorage persistence (per-`appId`, capped at 100 msgs).
- `builder/useBuilderChat.ts` — chat hook: history, in-flight turn, tool accumulation, deploy callback.
- `builder/components/`
    - `Chat.tsx` — messages + composer, auto-scroll, cancel.
    - `Message.tsx` — single message bubble (user or assistant), inline code/fences.
    - `ToolCall.tsx` — collapsible tool-use card.
    - `Preview.tsx` — iframe with reload + status strip.
    - `CodePanel.tsx` — CodeMirror 6 view of `server.js` (read-only by default; pencil to enable edit + deploy).
    - `TopBar.tsx` — header with status pill, "show code" toggle, clear-chat.

## How to run locally

You need three things running:

```bash
# 1. Platform (postgres + control + worker + gateway)
docker compose up -d

# 2. Agent (Bun, separate terminal)
cd agent
ANTHROPIC_API_KEY=sk-ant-...  bun run src/server.ts

# 3. Dashboard (Vite, separate terminal)
cd web/dashboard
npm run dev
# → http://localhost:5173/builder
```

Vite proxies `/api` → control (`:9090`), `/apps` → gateway (`:8000`),
so the iframe loads the deployed app same-origin and there are no CORS
issues. The agent runs on `:4444` and accepts cross-origin calls
(it has `cors()` middleware).

Override defaults via env (set before `npm run dev`):

```bash
VITE_PROXY_CONTROL=http://localhost:9090
VITE_PROXY_GATEWAY=http://localhost:8000
VITE_AGENT_URL=http://localhost:4444
```

## Agent contract

The dashboard sends `POST /chat` to the agent with:

```ts
{
  messages: [{ role: "user" | "assistant", content: string }],
  thread_id: string,                     // appId or "draft-..."
  context?: { app_id?: string; app_name?: string },
}
```

The agent responds with SSE frames (`data: {...}\n\n`):

```ts
{ type: "text",       content: string }
{ type: "tool_start", name: string, input: any }
{ type: "tool_end",   name: string, output: string, error?: boolean }
{ type: "done" }
{ type: "error",      content: string }
```

The chat hook listens for `tool_end` on `create_app` (navigates to the
new appId) and `deploy_app` (reloads the preview iframe).

## Generated app shape

The agent is prompted to produce single-module fetch handlers like:

```js
export default {
  async fetch(request, env, ctx) {
    const url = new URL(request.url);

    if (url.pathname === "/") {
      return new Response(`<!doctype html>...`, {
        headers: { "content-type": "text/html; charset=utf-8" },
      });
    }

    if (url.pathname.startsWith("/api/")) {
      return Response.json({ ok: true });
    }

    return new Response("Not Found", { status: 404 });
  },
};
```

Frontend interactivity is plain JS in a `<script>` tag — or React via
ESM CDN. No build step inside the runtime.

## Conversation persistence

Per-app chat history lives in `localStorage` under
`zeroship_builder_chat_{appId}`, trimmed to the last 100 messages.
Upgrade path: move to `@zeroship/db` once the editor has its own
backend tables.

## Known limitations (deferred for the docker-sandbox track)

- **No live HMR** — preview is the deployed app; reloads after deploy
  (typically 1-3s).
- **No multi-file** — the agent generates one server.js file.
- **No git** — undo via "clear chat" only; per-project git is the
  upgrade path documented in `docs/superpowers/specs/2026-04-27-zeroship-editor-design.md`.
- **No real auth scoping yet** — admin master key is the auth model.
- **`@monaco-editor/react` still installed** — the existing
  `AppDetail` page uses it; the new builder uses CodeMirror 6. Remove
  Monaco once `AppDetail` is migrated.
