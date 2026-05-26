# zeroship-builder

The zeroship AI builder — a fullstack zeroship app that **runs on the
zeroship platform itself.** Replaces the standalone `web/dashboard/`
+ `agent/` services with a single deployable.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│  Browser  ──HTTPS──►  gateway  ──►  worker (V8)          │
│                                       │                  │
│                                       ▼                  │
│                              this app's fetch handler    │
│                              (dist/server/index.js)      │
│                                       │                  │
│                              ┌────────┼────────┐         │
│                              ▼        ▼        ▼         │
│                          control   sandbox   OpenAI API  │
│                          plane     service                │
└──────────────────────────────────────────────────────────┘
```

The `@zeroship/vite-plugin` builds two outputs:

- **Client bundle** (`dist/assets/`) — React + Tailwind + shadcn/ui
  + CodeMirror 6, mounts at `/`.
- **Server bundle** (`dist/server/index.js`) — every
  `"use server"` module compiled into RPC stubs the runtime
  registers as a fetch handler.

The client calls server functions as plain async functions —
imports get rewritten by the plugin into chunked RPCs over the wire.

## What runs in V8 (no Node)

| Module | Purpose |
|---|---|
| `src/server/auth.ts` | Proxies to control plane's `/auth/*` with cookie passthrough. Handles register / login / logout / userinfo. |
| `src/server/apps.ts` | Proxies to control plane's `/api/apps/*` for app CRUD, deploys, env vars, secrets, logs, plan switching. |
| `src/server/sandbox.ts` | Proxies to `zeroship-sandbox` for file ops + shell exec inside the per-project Docker container. Holds the sandbox token (browser never sees it). |
| `src/server/chat.ts` | Builder agent — deepagents + LangGraph in V8 (~691 LOC). Translates the LangChain event stream into AI SDK v6 UI Message Stream Protocol on the wire. Marked `lazy: true` so the heavy graph only loads on first call. |
| `src/server/wizard.ts` | Pre-coding clarification flow — plain LangGraph (no deepagents, no sandbox) in V8 (~495 LOC). Loops surveys against the LLM until it emits a terminal `data-brief` chunk. Also `lazy: true`. |

## Project layout

```
apps/zeroship-builder/
├── package.json
├── vite.config.ts          (react + tailwind + zeroship plugins)
├── tsconfig.json
├── index.html
└── src/
    ├── server.ts           (entry — re-exports every server fn)
    ├── server/
    │   ├── auth.ts
    │   ├── apps.ts
    │   ├── agents.ts        (issues + quality + data/media canvas stubs)
    │   ├── sandbox.ts
    │   ├── chat.ts          (Builder — deepagents + LangGraph, lazy)
    │   ├── wizard.ts        (clarification — plain LangGraph, lazy)
    │   ├── pm-worker.ts     (PM digest RPC)
    │   ├── sre-worker.ts    (SRE monitor RPC)
    │   └── internal/
    │       ├── env.ts
    │       ├── request-context.ts
    │       ├── critic.ts       (Critic SubAgent)
    │       ├── reviewer.ts     (Reviewer SubAgent)
    │       ├── pm.ts           (PM SubAgent)
    │       ├── sre.ts          (SRE SubAgent)
    │       ├── tools.ts        (tool defs)
    │       ├── middleware.ts   (deepagents middleware)
    │       ├── persist.ts      (checkpointer)
    │       ├── prompts.ts      (system prompts)
    │       ├── survey-wire.ts  (data-survey wire)
    │       ├── agent-writes.ts (agent-write fan-out)
    │       └── sandbox-backend.ts (sandbox HTTP client)
    └── client/
        ├── main.tsx
        ├── App.tsx
        ├── index.css       (Tailwind v4 + shadcn theme tokens)
        ├── api/            (re-exports of server functions)
        ├── auth/           (AuthProvider, useAuth)
        ├── components/ui/  (shadcn primitives — button, card, …)
        ├── lib/utils.ts    (cn helper)
        ├── pages/          (Home, Login, Signup, Account)
        ├── workspace/      (ProjectWorkspace + tabs)
        └── builder/        (Chat + tool-call rendering)
```

The files in `src/server/internal/` are server-only helpers (SubAgents,
prompts, middleware, wire shapes) - NOT client-facing RPC procedure
modules. They're imported by server modules and never reach the browser.
`chat.ts` and `wizard.ts` carry `lazy: true` in their config so the
heavy LangGraph / deepagents code only loads on the first call to
`/_zs/v1/chat` or `/_zs/v1/wizard` — boot stays cheap.

## Configuration (env / secrets)

| Var | What | Default in dev |
|---|---|---|
| `CONTROL_URL` | control plane base URL | `http://localhost:9090` |
| `CONTROL_KEY` | control plane Bearer token | `dev-master-key` |
| `SANDBOX_URL` | sandbox service URL | `http://localhost:9091` |
| `SANDBOX_TOKEN` | sandbox bearer token | `test` |
| `OPENAI_API_KEY` | OpenAI API key | (required for chat) |

In production, set via `zeroship secret set` per app.

## Build + deploy

```bash
# install deps (uses local file:../../sdks/vite-plugin)
npm install

# dev — vite serves client + the plugin runs server fns in-process
npm run dev

# production — emits dist/ + dist/server/index.js
npm run build

# deploy to zeroship — wraps the build into a .appbundle
zeroship deploy --app=zeroship-builder
```

After deploy, the app is reachable at `/apps/zeroship-builder/` on
the gateway. Set the env vars + `OPENAI_API_KEY` secret first.

## What this proves

1. **The platform can host its own creator surface.** The dashboard
   and the agent are now *zeroship apps*, deployed via the same
   pipeline a creator uses for their app. Dogfood: complete.
2. **No Node-compat compromises.** The chat/agent loop is pure JS
   (vanilla OpenAI tool-use); LangGraph/deepagents stay on the host
   for the standalone dashboard if anyone wants them, but the
   first-class builder is V8-clean.
3. **Server functions are real.** ~13 KB of compiled server code
   handles auth, apps CRUD, sandbox file ops, and a full agent
   loop. The `"use server"` transform is doing what it advertises.

## Differences vs. `web/dashboard/`

| | dashboard | builder |
|---|---|---|
| runtime | Node + Vite dev / static SPA | zeroship V8 fetch handler |
| auth surface | calls `/auth/*` directly | server functions proxy `/auth/*` |
| agent | separate Bun service (deepagents/LangGraph) | inline server fn (vanilla OpenAI loop) |
| sandbox | calls `/agent/*` proxy | server functions proxy sandbox HTTP |
| deploy | static files served by anything | `zeroship deploy` |

The dashboard remains as a fallback for environments where the
zeroship platform isn't available; the builder is what creators get.
