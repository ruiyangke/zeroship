# zeroship-builder

The zeroship AI builder is a fullstack zeroship app that runs on the
zeroship platform itself. It is the creator-facing surface for starting a
project, clarifying the brief, building in a sandbox, inspecting the result,
and iterating through the chat rail.

The app is mid-rebuild. Keep changes contract-first: preserve the RPC ids,
stream part names, sandbox ownership rules, and manifest resource policy
before reshaping UI.

## Current Shape

```
Browser
  -> gateway
  -> worker V8
  -> builder fetch/RPC handlers
       -> @zeroship/auth BFF session via currentUser()
       -> @zeroship/kv for project/agent read models
       -> zeroship-sandbox controller for files, exec, preview proxy
       -> OpenAI/LangGraph/deepagents for wizard + builder agents
```

The builder is a pure creator app. It does not import `@zeroship/control`,
does not hold a control-plane credential, and does not deploy apps from the
console. Projects are local builder records backed by KV; generated app bytes
live in the per-project sandbox.

## Server Modules

| Module | Purpose |
| --- | --- |
| `src/server/config.ts` | Manifest resource policy. `rpc:projects`, `rpc:sandbox`, `rpc:agents`, and `rpc:chat` are user-gated; `rpc:wizard` is anonymous/public; worker stubs are admin-gated. |
| `src/server/projects.ts` | Project registry in KV plus `.env` and log helpers over sandbox files. |
| `src/server/sandbox.ts` | Canvas-facing file and live-preview RPCs. |
| `src/server/preview-proxy.ts` | Same-origin `/api/preview/:appId/:port/*` proxy to the sandbox controller. |
| `src/server/chat.ts` | Builder stream. Translates deepagents/LangGraph events into AI SDK UI message stream parts. |
| `src/server/wizard.ts` | Pre-coding clarification stream. Emits `data-survey` and terminal `data-brief` parts. |
| `src/server/agents.ts` | Issue and quality read models used by the workspace and subagent cards. |
| `src/server/pm-worker.ts` / `sre-worker.ts` | Scheduled-worker stubs for PM/SRE digests; scheduler wiring is still future work. |

Server-only helper code lives in `src/server/internal/`. Do not import those
modules from client code.

## Client Surfaces

| Surface | Path |
| --- | --- |
| Public marketing/auth/legal pages | `/`, `/pricing`, `/skills`, `/templates`, `/about`, `/changelog`, `/login`, `/signup`, `/legal/*` |
| Signed-in project gallery | `/home` |
| Anonymous pre-coding wizard | `/new` |
| Workspace | `/p/:appId/*` |

The active chat surface is `src/client/workspace/chat/*` and uses
`@ai-sdk/react` with explicit `chatTransport()` / `wizardTransport()`
wrappers in `src/client/api.ts`.

## Key Contracts

- Project RPC ids are `projects.*` and inherit auth/rate-limit from
  `rpc:projects`.
- Missing projects fail; they must not synthesize placeholder records.
- The wizard stream emits `data-survey` and `data-brief`.
- The builder stream emits text, tool receipts, `data-survey`,
  `data-critic-round`, `data-reviewer-round`, `data-pm-recommendation`,
  and `data-sre-finding`.
- Sandbox ownership is `(user_id, project_id)`, where the user id comes from
  `currentUser()` in production and the dev synthetic id only in local dev.
- The console has no deploy tool. The quality tool is `review` and ships
  nothing.

## Configuration

| Var | What | Default in dev |
| --- | --- | --- |
| `SANDBOX_URL` | sandbox controller base URL | `http://localhost:9091` |
| `SANDBOX_TOKEN` | sandbox bearer token | `test` |
| `OPENAI_API_KEY` | model key for chat, wizard, reviewer, PM, SRE | required for agent flows |
| `ZEROSHIP_SDK_REGISTRY` | optional private registry line injected into sandbox `.npmrc` | unset |

## Commands

```bash
pnpm --filter zeroship-builder test
pnpm --filter zeroship-builder build
pnpm --filter zeroship-builder test:e2e
```

`pnpm --filter zeroship-builder build` emits `dist/` and `dist/app.zship`.
After build, inspect the resource policy with:

```bash
tar -xOf apps/zeroship-builder/dist/app.zship manifest.json | jq '.resources'
```

## Rebuild Direction

Prefer vertical slices over a blind rewrite:

1. Stabilize manifest/RPC/security contracts.
2. Delete dead compatibility paths and stale comments.
3. Keep the server stream contracts while rebuilding the client shell.
4. Keep splitting heavy workspace/canvas bundles; route and workspace shell
   chunks are split, but the files editor still carries large CodeMirror/vendor
   chunks.
5. Promote E2E journeys that represent actual product flows, and remove
   mock-era tests that assert deleted behavior.
