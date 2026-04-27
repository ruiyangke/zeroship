# zeroship editor — AI-builder web UI design

**Date:** 2026-04-27
**Status:** design approved, ready for implementation plan
**Scope:** new repo extracted from `web/dashboard/` + `agent/`. Builds a chat-primary, preview-primary AI editor that runs on top of zeroship and uses Docker sandboxes for live dev environments.

---

## TL;DR

A Bolt-style AI app editor for zeroship creators. Chat on the left, live-HMR preview on the right; the code editor is hidden by default for "advanced users." Runs as a zeroship app (dogfood). The agent is `deepagents` (LangGraph + Anthropic) running inside the editor's backend; agent tools call a new Rust platform service (`zeroship-sandbox`) that manages Docker containers, one per active editor session, each holding the creator's project as a real git repo. Preview is a live `vite dev` server inside the sandbox container, proxied to the browser through `*.preview.zeroship.ai`. "Publish" runs the production-grade build + deploy through the existing control plane pipeline.

**Net new infrastructure:**
- New repo `zeroship/editor` (extracted from `web/dashboard/` + `agent/` via `git filter-repo`)
- New Rust binary `zeroship-sandbox` (Docker manager + HTTP/WS proxy)
- New gateway routing rule for `*.preview.zeroship.ai` → sandbox proxy
- Wildcard DNS + Let's Encrypt cert for `*.preview.zeroship.ai`
- Node-compat work in the V8 runtime (driven by what `deepagents` needs)

**Net code change (rough):**
- `+3-5K LOC` editor app (frontend + Hono backend)
- `+1.5-2K LOC` `zeroship-sandbox` Rust service
- `+200-500 LOC` gateway routing changes
- `+~1-2K LOC` runtime Node-compat shims (discovered iteratively)
- `~0 LOC` existing platform — control / worker / current gateway barely change

---

## Why

**Phase 2 of the platform roadmap** (creator dashboard) and **Phase 3** (AI app builder) are the differentiator. The current `web/dashboard/` is a control-plane CRUD UI. The current `agent/` is a separate Hono service that streams an SSE chat. Neither alone is "the editor." Merged + given a real dev sandbox, they become the product creators come for.

**Why dogfood (editor runs on zeroship):**
- Strongest possible product proof: "we built our own editor on it."
- Forces us to find and fix every Node-compat / runtime gap that real apps hit.
- Aligns the editor's roadmap with the platform's roadmap — every primitive the editor needs is one creators get for free.

**Why a real Docker sandbox (vs in-browser WebContainers, vs isomorphic-git in V8):**
- The agent (`deepagents`) lives on the backend; in-browser sandboxes need awkward round-trips for every file op.
- Real `npm install`, real `vite dev`, real `git`, real shell — no compat ceiling.
- One container per session is the right granularity: warm during the session, free during idle, scales horizontally.
- Standard ops surface (Docker is debuggable, observable, well-understood) — we own no ports of git or shell into V8.

**Why `vite dev` proxy for preview (vs deploy-then-iframe):**
- Sub-100ms HMR vs 2-5s redeploy — dramatically better iteration feel.
- Identical to local dev (creator can clone the repo and run `npm run dev` and get the same thing).
- Reuses `sdks/vite-plugin/` we just shipped.
- Production deploy still goes through the bundled-deploy pipeline → covers the "what does shipped code look like" question.

---

## Architecture

```
┌───────────────────────────────────────────────────────────────────────┐
│ zeroship platform (existing)                                          │
│                                                                       │
│   control-plane  ─┐    gateway  ─┐    worker pool  ─┐                 │
│                   │              │                  │                 │
│                   │              │                  │                 │
│                   │              ▼                  │                 │
│                   │       ┌──────────────┐          │                 │
│                   │       │ Sandbox-proxy│          │                 │
│                   │       │ rule:        │          │                 │
│                   │       │ *.preview.   │          │                 │
│                   │       │ zeroship.ai  │          │                 │
│                   │       └──────┬───────┘          │                 │
│                   │              │                  │                 │
└───────────────────┼──────────────┼──────────────────┼─────────────────┘
                    │              │                  │
                    │              │                  │ deploy bundle
                    │              ▼                  ▼
                    │     ┌──────────────────────────────────┐
                    │     │ zeroship-sandbox (NEW BINARY)    │
                    │     │                                  │
                    │     │  HTTP API:                       │
                    │     │   POST   /sessions               │
                    │     │   POST   /sessions/:id/exec      │
                    │     │   GET    /sessions/:id/files/*   │
                    │     │   PUT    /sessions/:id/files/*   │
                    │     │   DELETE /sessions/:id           │
                    │     │                                  │
                    │     │  HTTP/WS proxy for preview:      │
                    │     │   slug → container_ip:5173       │
                    │     │                                  │
                    │     │  Docker host pool                │
                    │     │  - one container per session     │
                    │     │  - bind-mount /workspace         │
                    │     │  - vite dev runs continuously    │
                    │     │                                  │
                    │     │  Persistent storage:             │
                    │     │  - local SSD (hot)               │
                    │     │  - object-store backup (cold)    │
                    │     └──────────────────────────────────┘
                    │              ▲
                    │              │ HTTP (agent tool calls)
                    ▼              │
        ┌───────────────────────────────────────┐
        │  EDITOR APP                           │
        │  (zeroship app, multi-tenant)         │
        │                                       │
        │  Frontend (React + Vite)              │
        │   - chat (SSE)                        │
        │   - preview iframe                    │
        │   - CodeMirror 6 (toggle)             │
        │   - file tree (toggle)                │
        │                                       │
        │  Backend (Hono inside V8)             │
        │   - /chat   (SSE → deepagents)        │
        │   - /api/projects/*  (CRUD)           │
        │   - /api/auth/*      (@zeroship/auth) │
        │   - control-plane proxy (publish)     │
        │                                       │
        │  Persistence (@zeroship/db):          │
        │   - projects                          │
        │   - conversations                     │
        │   - sandbox sessions                  │
        └───────────────────────────────────────┘
                    ▲
                    │ HTTPS
                    │
                  Browser
```

### Components

| Component | Type | Responsibility | New? |
|---|---|---|---|
| Editor app | zeroship app (V8) | UI + agent + project metadata + control-plane proxy | NEW (extracts dashboard + agent) |
| `zeroship-sandbox` | Rust binary | Docker session lifecycle + HTTP proxy for preview | NEW |
| Sandbox container | docker | hosts one project's git repo + vite dev + agent's tool target | NEW |
| Gateway routing rule | gateway change | route `*.preview.zeroship.ai` → sandbox-proxy | NEW (~50 LOC) |
| control-plane | existing | unchanged — receives bundled deploys via existing API | unchanged |
| worker | existing | unchanged — runs editor app + creator's published apps | unchanged |
| `@zeroship/auth` | existing SDK | creator login + session for editor | unchanged |
| `sdks/vite-plugin/` | existing | runs in sandbox container as the dev server | unchanged |
| `@zeroship/db` | existing SDK | editor metadata (projects, conversations, sessions) | unchanged |

---

## Repo extraction (PR 0)

```bash
# 1. Clone with full history
git clone <appbase> editor-extract
cd editor-extract

# 2. Filter to the two paths we want
git filter-repo \
  --path web/dashboard \
  --path agent

# 3. Move them up to the repo root with sensible names
git filter-repo \
  --path-rename web/dashboard:apps/editor-web \
  --path-rename agent:apps/editor-agent

# 4. Push to new repo
git remote add origin git@github.com:zeroship/editor.git
git push -u origin main
```

**New repo top-level layout:**

```
editor/
├── apps/
│   ├── editor-web/      (was web/dashboard/, React + Vite + CM6 + Hono dev proxy)
│   └── editor-agent/    (was agent/, deepagents + LangGraph + Anthropic)
├── services/
│   └── sandbox/         (NEW: zeroship-sandbox Rust binary)
├── packages/
│   └── shared-types/    (TS types shared between editor-web and editor-agent)
├── docker/
│   └── sandbox-base/    (Dockerfile for the sandbox container image)
├── flake.nix            (build env)
├── docker-compose.yml   (local dev: editor + sandbox + control-plane stub)
├── package.json         (workspaces root, pnpm)
└── README.md
```

**During PR 0:** the existing `appbase` repo's `web/dashboard/` and `agent/` directories stay in place (deletion is a separate cleanup PR after the editor is functional in its new home — avoids breaking anything that references them during transition).

**Note on agent/web merge:** PR 2 merges `editor-web` and `editor-agent` into a single Hono app (one process serves React static + agent SSE + control-plane proxy). PR 0 just extracts; merging is its own step.

---

## Editor app

### Frontend stack

- **React 18** (existing) + **Vite** (existing)
- **CodeMirror 6** for the editor pane (replaces existing `@monaco-editor/react`)
- **Tailwind v4** + **shadcn/ui** (existing)
- **TanStack Query** (existing, for server state)
- **TanStack Router** (replaces react-router; better data loading semantics)
- **`@xyflow/react`** for any future visual builder (deferred)

### Layout

Default view (≥80% of users):

```
┌──────────────────────────┬───────────────────────────────────────┐
│                          │                                       │
│         CHAT             │           PREVIEW (iframe)            │
│                          │                                       │
│  - message thread        │   src = "https://{slug}               │
│  - tool-call cards       │           .preview.zeroship.ai/"      │
│    (collapsible)         │                                       │
│  - "thinking..." pulse   │   - sandbox attribute set             │
│  - input box at bottom   │   - HMR reloads automatically         │
│                          │                                       │
│                          │   Tabs above: Preview | Logs          │
└──────────────────────────┴───────────────────────────────────────┘
                Width: 380px              Width: rest
```

Power-user view (toggle "Show code"):

```
┌──────────┬───────────────────────────┬─────────────────────────┐
│ chat     │ FILE TREE │ CODEMIRROR 6  │      PREVIEW            │
│ (narrow) │ src/      │               │                         │
│          │  server.ts│               │                         │
│          │  main.tsx │               │                         │
│          │ public/   │               │                         │
└──────────┴───────────────────────────┴─────────────────────────┘
```

### Backend (Hono inside V8)

Single Hono app exposed via the editor app's `fetch` handler (per the new programming model). Routes:

| Route | Purpose |
|---|---|
| `GET /` | Static React shell |
| `GET /assets/*` | Static React assets |
| `POST /api/chat` | SSE-stream agent response (calls deepagents internally) |
| `GET /api/projects` | List creator's projects |
| `POST /api/projects` | Create project (provisions sandbox, seeds template, opens session) |
| `GET /api/projects/:id` | Project metadata + active session info |
| `POST /api/projects/:id/publish` | Trigger production deploy |
| `DELETE /api/projects/:id` | Delete project + tear down sandbox |
| `GET /api/projects/:id/conversation` | Load chat history |
| `POST /api/projects/:id/files/*` | (proxy to sandbox-service for file ops triggered by editor pane, not agent) |

The agent's tool calls hit the sandbox service directly via Hono's internal fetch — they don't go through these public routes.

---

## Sandbox service (`zeroship-sandbox`)

New Rust binary in `services/sandbox/`. compio-native, like the rest of the platform. Responsibilities:

1. **Session lifecycle** — provision/teardown Docker containers per editor session.
2. **File ops** — read/write/list files in the bind-mounted project directory.
3. **Exec** — run shell commands inside the container, stream output back over HTTP.
4. **Preview proxy** — forward HTTP + WS traffic from `*.preview.zeroship.ai` to the right container's vite dev port.
5. **Persistence** — manage bind-mount directory + periodic backup to object storage.

### Container spec

Base image (`docker/sandbox-base/Dockerfile`):

```dockerfile
FROM node:22-alpine

RUN apk add --no-cache git curl

# Pre-install zeroship CLI
RUN npm install -g zeroship-cli

# Pre-seed the project template
COPY templates/vite-react-tailwind /opt/templates/default/
RUN cd /opt/templates/default && npm install

# The sandbox proxy connects here
EXPOSE 5173

WORKDIR /workspace

# vite dev runs as the entrypoint when /workspace has a project;
# until then, sleep so the container stays up
ENTRYPOINT ["/usr/local/bin/sandbox-entrypoint.sh"]
```

`sandbox-entrypoint.sh`:

```bash
#!/bin/sh
# If /workspace is empty (new project), copy the template
if [ ! -f /workspace/package.json ]; then
  cp -r /opt/templates/default/. /workspace/
  cd /workspace
  git init -q
  git add -A
  git commit -q -m "initial commit (zeroship template)"
fi

cd /workspace
exec npm run dev -- --host 0.0.0.0 --port 5173
```

Runtime container args (set by `zeroship-sandbox` when spawning):

```
docker run -d --rm
  --name {session-id}
  --network zeroship-sandbox-net
  --memory 1024m
  --cpus 2.0
  --pids-limit 256
  --read-only
  --tmpfs /tmp:size=128m
  --volume /var/zeroship/projects/{creator}/{project}:/workspace
  --label zeroship.session={session-id}
  --label zeroship.creator={creator-id}
  --label zeroship.project={project-id}
  zeroship/sandbox-base:{version}
```

**Why these flags:**
- `--read-only` — root FS is immutable; only `/workspace` (bind-mount) and `/tmp` (tmpfs) are writable.
- `--memory 1024m --cpus 2.0` — generous default, enough for vite dev + node + small npm installs.
- `--pids-limit 256` — prevents fork-bombs.
- `--network zeroship-sandbox-net` — isolated bridge network; container can't reach platform internals; outbound internet is allowed (for npm, fetch, etc.) via the bridge's NAT.
- `--label` — for `docker ps`-based observability.

### Sandbox HTTP API

```
POST /sessions
  Body: { project_id, creator_id }
  Response: { session_id, container_ip, preview_slug }
  → Spawns container, mounts /workspace from the per-project dir,
    runs npm install if package.json changed since last session,
    starts vite dev, returns when port :5173 is responsive.

POST /sessions/:id/exec
  Body: { cmd: string, cwd?: string, timeout_ms?: number }
  Response: SSE stream of { type: "stdout"|"stderr"|"exit", data: string|number }
  → docker exec into the container; streams output.

GET /sessions/:id/files/*path
  Response: file content (text or base64 for binary)
  → Reads from the bind-mount directly (no docker exec needed).

PUT /sessions/:id/files/*path
  Body: file content
  → Writes to the bind-mount directly.
  → Vite dev's file watcher picks up the change → HMR fires.

DELETE /sessions/:id/files/*path
  → Removes file from bind-mount.

GET /sessions/:id/file-tree
  Response: { entries: [{path, kind: "file"|"dir", size}] }
  → Walks the bind-mount.

POST /sessions/:id/git/commit
  Body: { message: string, paths?: string[] }
  → docker exec git add + git commit.

GET /sessions/:id/git/log
  Response: { commits: [{ sha, message, timestamp }] }

POST /sessions/:id/git/checkout
  Body: { sha: string }

DELETE /sessions/:id
  → Stops + removes container.
  → Snapshots project dir to object storage.
  → 5-min idle grace before this fires automatically (driven by editor app's keepalive).
```

### Preview proxy

`zeroship-sandbox` exposes a second HTTP server (different port from the API) that handles preview traffic. The platform gateway forwards `*.preview.zeroship.ai` to this port.

```
Browser ──► gateway ──► sandbox-proxy
                           │
                           │ Host: abc123.preview.zeroship.ai
                           │ → look up: abc123 → container_ip
                           │
                           ▼
                       container_ip:5173 (vite dev)
```

- Standard reverse proxy: copy headers (with Host rewrite), pass body, stream response back.
- WebSocket upgrade supported (vite HMR uses WS): on `Connection: upgrade`, hijack the TCP stream and bidirectionally pipe.
- If session is idle (container killed), proxy returns a "session expired, click to wake" page that calls back to the editor to re-provision.

### Lifecycle & idle policy

```
[creator opens project in editor]
   ↓
   editor frontend opens WS to editor backend (`/api/projects/:id/session`)
   ↓
   editor backend POSTs /sessions to sandbox service (idempotent: re-attaches if container still alive)
   ↓
   container starts (or wakes if existing) → vite dev ready → preview iframe loads
   ↓
[creator works on project]
   ↓
   editor frontend pings WS every 30s (keepalive)
   ↓
[creator closes tab / loses connection]
   ↓
   WS close → editor backend marks session "idle"
   ↓
   5 min later → editor backend POSTs DELETE /sessions/:id → container stops, project snapshotted
   ↓
[8h later, regardless of state] → hard timeout, container force-killed
```

### Persistence

```
/var/zeroship/projects/{creator-id}/{project-id}/
   ├── workspace/           (bind-mounted into containers)
   │    ├── .git/
   │    ├── package.json
   │    ├── src/
   │    └── ...
   └── .meta/
        ├── last-snapshot   (timestamp)
        └── version         (incremented per snapshot)
```

- **Hot path (active session):** files live on local SSD of the sandbox host — fast.
- **Cold path (session idle):** on session teardown, tar.gz the project dir → upload to `s3://zeroship-projects/{creator-id}/{project-id}.tar.gz`.
- **On wake / cross-host:** if a creator's next session lands on a different sandbox host (load-based scheduling), the new host downloads the tarball and unpacks before starting the container.
- **Backup cadence:** every 10 min while session active (incremental — only changed files via `rsync`-style delta), plus on teardown.

### Why a separate binary, not part of control or gateway

- Different runtime concerns: control plane is request/response over postgres; sandbox is long-lived containers + bind mounts + WS proxy.
- Different scaling axis: control plane scales with API traffic; sandbox scales with active editor sessions.
- Different host requirements: sandbox hosts need Docker daemon + persistent disks; control hosts don't.
- Different blast radius: a bug in sandbox shouldn't take down the control plane.

---

## Project model

### File layout (Vite-conventional)

The pre-seeded template:

```
{project root}/
├── package.json            (vite + react + tailwind + zeroship-vite-plugin)
├── vite.config.ts          (uses sdks/vite-plugin)
├── tsconfig.json
├── tailwind.config.ts
├── index.html
├── src/
│   ├── main.tsx            (React entry)
│   ├── App.tsx             (default landing page)
│   ├── server.ts           (zeroship backend handler — `export default { fetch }`)
│   └── styles.css
├── public/
│   └── favicon.svg
├── .gitignore
└── README.md
```

The agent treats this as a normal Vite project. The only zeroship-specific bit is `src/server.ts` and the vite-plugin in `vite.config.ts` that wires backend and frontend together for dev mode.

### Per-project git

- Initialized at template-seed time (`git init` in the entrypoint script).
- Agent commits after every successful change set: `git add -A && git commit -m "agent: <summary>"`.
- Creator can roll back via `git log` UI in the editor (deferred to PR 4 polish).
- `git push` to GitHub: deferred, future feature.

### Storage durability summary

| State | Where | Backup |
|---|---|---|
| Active session, workspace files | sandbox host SSD (bind mount) | tar.gz to S3 every 10 min + on teardown |
| Idle (no container) | S3 only | versioned (S3 object versioning) |
| Cross-host migration | S3 → fresh host SSD | always pulls latest version |

---

## Agent

### Implementation

- `deepagents` (LangGraph + `@langchain/anthropic`) — kept as-is from `agent/`.
- Runs **inside the editor's V8 isolate** as part of the Hono backend. Imported via `import { createDeepAgent } from "deepagents"`.
- This requires the runtime to be Node-compatible enough — see "Node compat" section.
- Conversations stored in `@zeroship/db` (`conversations` table per-project, append-only message rows).

### Tools (replace existing `agent/src/tools/`)

```ts
// File operations (target the active sandbox session)
read_file({ path }): string
write_file({ path, content }): void
list_files({ dir? }): { path, kind, size }[]
search_files({ query, glob? }): { path, line, match }[]
delete_file({ path }): void

// Shell
run_command({ cmd, timeout_ms? }): { stdout, stderr, exit_code }

// Git
git_log({ limit? }): Commit[]
git_diff({ from, to? }): string
git_checkout({ sha }): void

// Deploy (production)
publish({ message? }): { url, version }

// Logs from the live preview container
get_logs({ tail? }): string[]

// Platform discovery (control plane)
list_apps(): App[]
get_app({ app_id }): AppDetails
```

Each tool is a thin wrapper around the sandbox service HTTP API (or, for `publish`, the control plane). LangGraph tool implementations are ~5 lines each.

### Conversation persistence

```sql
CREATE TABLE conversations (
  id UUID PRIMARY KEY,
  project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE messages (
  id UUID PRIMARY KEY,
  conversation_id UUID NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  role TEXT NOT NULL,            -- 'user' | 'assistant' | 'tool'
  content JSONB NOT NULL,        -- text + tool_use blocks
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX ON messages (conversation_id, created_at);
```

- One conversation per project (extend to multiple later if needed).
- Messages stored individually so the editor can paginate / lazy-load on large histories.
- Agent reads the last N messages (default 50) into context on each turn; older messages summarized via a separate "summarize" call (deferred).

---

## Build & deploy pipeline

### Preview (default, every save)

**Nothing happens.** The vite dev server in the sandbox container picks up the file change via the file watcher and pushes HMR via WebSocket. Iframe updates in <100ms.

### Production publish ("Publish" button)

```
1. Agent (or creator) triggers publish:
     POST /api/projects/:id/publish
2. Editor backend POSTs to sandbox service:
     POST /sessions/:id/exec
       cmd: "npm run build && zeroship bundle --out /tmp/app.appbundle"
3. Sandbox runs:
     - vite build → dist/
     - zeroship bundle picks up dist/ + src/server.ts
     - emits .appbundle (zstd-compressed)
4. Editor backend reads /tmp/app.appbundle from sandbox
     GET /sessions/:id/files/tmp/app.appbundle
5. Editor backend POSTs bundle to control plane:
     POST /api/apps/{production-app-id}/bundle
     (uses creator's session token; control plane authorizes via existing per-app rules)
6. Control plane stores bundle + bumps env_version
7. Workers pick up new bundle on next request
8. Editor surfaces "Published! Live at https://{app-slug}.zeroship.ai"
```

### Two-app-per-project

Each project has TWO `apps` rows in the control DB:

```sql
ALTER TABLE projects ADD COLUMN production_app_id UUID REFERENCES apps(id);
-- preview app exists only as a Docker container (no apps row needed
-- because preview is a live container, not a deployed bundle)
```

The preview side is *not* a control-plane app. The production side is a normal app, deployed and served exactly like any other zeroship app — the only thing special about it is that the editor knows which `app_id` corresponds to which `project_id`.

---

## Auth & multi-tenancy

### Creator authentication

- Editor uses `@zeroship/auth` exactly like any other zeroship app.
- Gateway validates the auth cookie and injects `ZeroShip-User` header.
- Editor backend reads the header → looks up creator → scopes all DB queries to `creator_id`.

### Project ownership

```sql
CREATE TABLE projects (
  id UUID PRIMARY KEY,
  creator_id UUID NOT NULL REFERENCES users(id),
  name TEXT NOT NULL,
  preview_slug TEXT NOT NULL UNIQUE,        -- random, e.g. 'sky-river-72'
  production_app_id UUID REFERENCES apps(id),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX ON projects (creator_id);
```

- All `/api/projects/*` routes filter by `creator_id` from the auth header. 403 on mismatch.
- Sandbox service authorizes via a service-token in `Authorization: Bearer ...` header. The editor backend holds this token from its env (`SANDBOX_SERVICE_TOKEN` populated by control plane at deploy).

### Preview access

Default: **logged-in creator only**. The sandbox proxy validates the auth cookie before forwarding traffic to the container. If `creator_id` from the cookie ≠ the project's `creator_id`, return 403.

Future: "Share preview link" feature — creator can mint a signed URL that anyone can use (HMAC-signed, expiry, optional password). Adds ~100 LOC, defer to v2.

### Rate limits

Per-creator, enforced by the editor backend:

| Resource | Default limit |
|---|---|
| Active sandbox sessions | 1 (one project at a time) |
| Sandbox CPU-time | 60 min/day on free tier |
| Agent messages | 100/day on free tier |
| Project count | 5 on free tier |
| Storage per project | 100 MB |

Paid tier raises all of these significantly. Enforced via `@zeroship/db` counters checked at session-creation + per-request.

---

## DNS & TLS

- Add wildcard A record: `*.preview.zeroship.ai → gateway public IP`.
- Acquire wildcard cert via Let's Encrypt DNS-01 challenge:
  - covers `*.preview.zeroship.ai`
  - existing cert covers `*.zeroship.ai` (or extend to include both)
- Gateway already handles SNI + TLS termination; one new routing rule:

```rust
// crates/gateway/src/router.rs (sketch)
if let Some(host) = req.headers().get("host") {
    if host.ends_with(".preview.zeroship.ai") {
        let slug = host.strip_suffix(".preview.zeroship.ai").unwrap();
        return forward_to_sandbox_proxy(slug, req).await;
    }
}
// existing routing follows
```

---

## Node compat strategy

**Approach:** discover blockers by running, fix per blocker. The set of Node APIs `deepagents` + `langchain` actually use is finite; we hit each one and add a shim.

**Likely blockers (based on dependency tree):**

| API | Strategy |
|---|---|
| `process.env.X` | already supported via `env.get()`; alias |
| `process.cwd()` | return `/`; warn-once if called |
| `process.versions` | populate with our V8 version + a synthetic `node` version |
| `Buffer` | shim with Uint8Array adapter (most uses are encoding-related) |
| `node:fs` (read-only) | shim against `@zeroship/storage` for paths under `/workspace`; deny otherwise |
| `node:path` | pure JS, port to runtime built-ins |
| `node:crypto` | most uses can target WebCrypto; for legacy, ship a shim |
| `node:stream` | shim Readable/Writable on top of Web Streams |
| `EventEmitter` | tiny pure-JS impl |
| `node:util` | `promisify`, `inspect` — small shims |
| `node:url` | use WHATWG URL (already there) |
| `setImmediate` | alias to `queueMicrotask` |
| `dynamic require()` | error with a clear "use import()" message |

**Process:**
1. Boot `deepagents` in the runtime → capture all `ReferenceError` / `TypeError`.
2. Add the missing API to `crates/runtime/src/node_compat/`.
3. Re-test until it boots.
4. Run a real chat → catch any runtime errors in the agent loop.
5. Iterate.

**Spec:** `docs/specs/node-compat.md` is the existing baseline. Update it with each new shim added.

**Tracking:** create a checklist `docs/superpowers/plans/node-compat-blockers.md` that the implementation plan ticks off as each is fixed.

---

## Security

### Sandbox isolation

| Layer | Mitigation |
|---|---|
| Container escape | `--read-only`, `--cap-drop=ALL --cap-add=...minimal`, default seccomp profile, `--pids-limit`, no docker socket mount |
| Network | dedicated bridge network (`zeroship-sandbox-net`) with iptables drop for platform-internal CIDRs; outbound internet allowed via NAT |
| Resource exhaustion | `--memory`, `--cpus`, `--pids-limit`, `--blkio-weight` |
| Persistent storage | per-creator + per-project bind mount; cross-creator access blocked at filesystem level |
| Cross-session data leak | container removed (`--rm`) on stop; bind mount path includes creator_id; new sessions get fresh container |

### Secret handling

- Editor backend's `SANDBOX_SERVICE_TOKEN` and `ANTHROPIC_API_KEY` come from `env` (set by control plane at deploy, encrypted at rest via existing `EnvStore`).
- Anthropic key never enters the sandbox container — agent runs in editor backend, talks to Anthropic over `fetch`.
- Sandbox service token validates with constant-time compare.

### Audit log

Reuse existing `crates/control/src/audit.rs` infrastructure. New action types:
- `CreateProject`, `DeleteProject`
- `StartSession`, `EndSession`
- `Publish` (with bundle hash)

---

## Observability

New Prometheus metrics in `zeroship-sandbox`:

```
zeroship_sandbox_sessions_active                   gauge
zeroship_sandbox_sessions_total{outcome}           counter (started, killed_idle, killed_hard, error)
zeroship_sandbox_session_duration_seconds          histogram
zeroship_sandbox_container_spawn_seconds           histogram
zeroship_sandbox_exec_total{exit}                  counter
zeroship_sandbox_proxy_requests_total{status}      counter
zeroship_sandbox_proxy_ws_active                   gauge
zeroship_sandbox_storage_bytes{creator,project}    gauge
zeroship_sandbox_backup_bytes_total                counter
```

Editor backend metrics (added to existing platform metrics):
```
zeroship_editor_chat_requests_total{outcome}       counter
zeroship_editor_chat_duration_seconds              histogram
zeroship_editor_tool_calls_total{tool}             counter
zeroship_editor_tool_duration_seconds{tool}        histogram
zeroship_editor_anthropic_tokens_total{kind}       counter (in/out)
zeroship_editor_publish_total{outcome}             counter
```

Logs: structured JSON to stderr (existing convention).

Cost tracking: per-creator daily aggregate stored in `@zeroship/db`, surfaced in the existing creator dashboard's billing view.

---

## Phased rollout

| Phase | PR | Scope | Est |
|---|---|---|---|
| **0** | repo extract | `git filter-repo` of `web/dashboard` + `agent` into `zeroship/editor`. CI green. | 1d |
| **1** | sandbox service v1 | `zeroship-sandbox` Rust binary: HTTP API for sessions, exec, files. Docker integration. Bind-mount workspace. NO preview proxy yet (use deploy-then-iframe stub). | 1-2w |
| **2** | node-compat sweep | Run `deepagents` in the V8 runtime; add shims until it boots; pin a known-good langchain version; ship `crates/runtime/src/node_compat/`. | 1-2w (parallel to 1) |
| **3** | editor MVP | Merge `editor-web` + `editor-agent` into one Hono app. Chat (SSE) → deepagents → sandbox tools. Preview = iframe pointing to deployed app via deploy-on-save (placeholder until proxy lands). React layout: chat + preview only (no editor pane). | 2w |
| **4** | preview proxy | `zeroship-sandbox` HTTP/WS proxy. Gateway routing for `*.preview.zeroship.ai`. Wildcard DNS + TLS. Switch preview from deploy-stub to live proxy. | 1w |
| **5** | editor pane + git | CodeMirror 6 + file tree + git log/diff/checkout UI, hidden behind "Show code" toggle. Read/write files via sandbox API. | 1w |
| **6** | publish flow | "Publish" button → vite build + zeroship bundle in sandbox → upload to control plane → production app deploys. Project schema: `production_app_id`. | 1w |
| **7** | polish + multi-tenant | Auth via `@zeroship/auth`. Rate limits. Audit log. Observability dashboards. Backup automation. | 1-2w |

**Total: 7-10 weeks.**

Each PR ships a working slice — the editor is usable end-to-end after PR 4 (preview proxy lands), with editor-pane and publish following.

---

## Open questions / future work

| Topic | Status | Notes |
|---|---|---|
| Multi-conversation per project | deferred | One conversation/project is fine for v1. |
| Conversation summarization | deferred | Send last N messages until budget is a real concern. |
| Share-preview-link feature | deferred | HMAC-signed URL, optional password. ~100 LOC. |
| GitHub push integration | deferred | Project IS a real git repo; pushing is a sandbox `git remote add` + `git push`. |
| Per-project git → libgit2 plugin | deferred | Move off `git` shell-out to a native plugin if perf demands. |
| Firecracker microVM upgrade | deferred | Replace Docker with Firecracker if isolation/density/boot-time becomes an issue. |
| In-browser sandbox (WebContainers) | rejected | Backend agent + WebContainers = round-trip per file op; bad fit. |
| Multi-cursor live collab | deferred | CodeMirror 6 + Yjs is well-trodden; add when team accounts ship. |
| Visual builder ("no-code") | deferred | `@xyflow/react` + drag-drop component palette. Phase 4 of the platform roadmap. |
| Agent sub-agents (planner / reviewer) | deferred | deepagents supports it; turn on once single-agent loop is stable. |
| Custom domains for production | deferred | Cross-cuts with the platform's domains roadmap. |
| Session resumption across reloads | partial | Sandbox container survives 5 min after WS close → reload within 5 min re-attaches. Beyond that, fresh container with same workspace. |
| Edit conflict between agent + creator | deferred | If creator and agent edit same file simultaneously: last-write-wins for v1; surface a "files changed externally" toast in CodeMirror. |

---

## Decisions made (locked in)

1. **Repo:** new `zeroship/editor`, extracted via `git filter-repo`.
2. **Editor unification:** `web/dashboard` + `agent` merge into one Hono app served by a single zeroship app.
3. **Dogfood:** editor runs as a zeroship app on the platform.
4. **Agent stack:** `deepagents` (LangGraph + `@langchain/anthropic`); commit to expanding zeroship's Node compatibility.
5. **Project model:** Vite-conventional full-stack (frontend + backend + assets) per project.
6. **Storage:** per-project git repo on sandbox host SSD, tar.gz backup to object storage.
7. **Sandbox runtime:** Docker, one container per active session, warm for the session.
8. **Sandbox host topology:** dedicated `zeroship-sandbox` binary on dedicated hosts.
9. **Preview model:** live container with `vite dev` + `sdks/vite-plugin/`, proxied via `*.preview.zeroship.ai`.
10. **Production model:** "Publish" runs `vite build` + `zeroship bundle` in sandbox → bundled deploy through existing control plane.
11. **Editor UX:** chat + preview by default; CodeMirror 6 editor + file tree behind a "Show code" toggle.
12. **Editor pane library:** CodeMirror 6 (replaces existing Monaco).
13. **Bootstrap:** sandbox container ships pre-seeded Vite + React + Tailwind starter.
14. **Idle policy:** session ties to editor WebSocket; 5-min grace after close; 8h hard ceiling.
15. **Conversation storage:** `@zeroship/db` per-project tables.
