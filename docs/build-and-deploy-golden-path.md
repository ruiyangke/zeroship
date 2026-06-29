# Build-local → deploy: the golden path

**Strategy (2026-06-29).** The primary creator flow is: **build a zeroship app
locally with an AI coding agent (Claude Code / Codex), then `zeroship deploy`
the built `.zship` to the platform.** The platform's differentiated value is the
*infrastructure* — the zero-tokio compio/io_uring V8 runtime, the
`env.{db,kv,storage,auth}` primitives, the `.zship` deploy contract,
gateway/CHWBL routing, and billing/metering. We do **not** rebuild a hosted
in-browser AI builder (Claude Code / Codex already do that better); the
sandbox / hosted-build environment is **deferred** (extracted to the standalone
`zeroship-sandbox` project, ready to return for a no-terminal v2).

## The chain

```
agent scaffolds (examples/starter)            ← CLAUDE.md teaches the contract
  → pnpm build  (@zeroship/vite-plugin)        → dist/app.zship
  → zeroship deploy dist/app.zship --app=<id> --token=<PAT>
  → control plane ingests → BlobStore + route registry
  → gateway pulls routes (5s) → serves the app (static + RPC + env.* primitives)
```

- **Scaffold:** `examples/starter/` — a minimal, agent-facing zeroship app
  (fetch/static SPA + `getMessages`/`addMessage` RPCs via `@zeroship/rpc/server`,
  validated with zod via `@zeroship/server`) with a `CLAUDE.md` that teaches an
  AI agent the deploy contract, the `env.*` primitives, and the build+deploy
  steps. Copy it, point Claude Code/Codex at it, iterate.
- **Build:** `pnpm build` (= `vite build`); the `@zeroship/vite-plugin` discovers
  `"use server"` RPC functions, bundles the server module, and writes
  `dist/app.zship`.
- **Deploy:** `zeroship deploy ./dist/app.zship --app=<id> --control=<url> --token=<PAT>`.

## What is validated today (`tests/golden_path.sh`)

Run `nix develop --command bash tests/golden_path.sh` (needs the release binaries
+ a Postgres on :5440). It proves the **load-bearing half** end-to-end with a
REAL vite-built `.zship` (not a hand-packed fixture):

1. ✅ `examples/starter` builds to `dist/app.zship` via the real vite-plugin pipeline.
2. ✅ The platform stack (control + worker + gateway) comes up healthy on the
   current HEAD binaries.
3. ⏭️ Create-app + deploy + serve runs only when `ZEROSHIP_TOKEN=<pat>` is set
   (see the auth gap below) — point it at a full platform with auth and re-run.

## The two productization gaps (the roadmap to "easy create + deploy")

These are the concrete things to build to make the golden path frictionless for
external creators and AI agents:

### 1. SDK distribution — external creators can't `npm install @zeroship/*` yet
The `@zeroship/*` SDKs (`@zeroship/rpc`, `@zeroship/server`, `@zeroship/vite-plugin`,
`@zeroship/db`, …) are **workspace-only** (`workspace:*`); they resolve only
inside this monorepo. A creator building on their own machine needs them
installable. Options (existing track:
`docs/decisions/2026-05-26-sdk-distribution-private-registry.md`):
publish to npm, or serve a private registry (Verdaccio). Until then the starter
builds only inside the monorepo.

### 2. Frictionless deploy auth — no dev/CI/agent token path
App CRUD + deploy require a **Personal Access Token (PAT)**, minted only by the
platform's auth stack via `zeroship login` (OAuth device flow). There is
intentionally **no** master-key/dev shortcut for user-level app CRUD, and
`--dev-insecure` only relaxes admin/control/webhook secrets (not user auth), so
a PAT cannot be forged for a local control. For "an AI agent / CI deploys" to be
smooth, we need either:
- a headless-friendly `zeroship login` against the **dev auth tier**
  (`docs/reference/auth-dev-tier.md`) that auto-mints a PAT in dev, and/or
- a scoped, deploy-only **CI/agent token** path.

This is the single biggest blocker for end-to-end `golden_path.sh` (the create +
deploy + serve steps). Closing it lets the script prove the *whole* chain.

## Status

- ✅ Starter scaffold + agent `CLAUDE.md` (`examples/starter/`).
- ✅ Real build → `.zship` (vite-plugin) — proven.
- ✅ Stack bringup (control/worker/gateway) — proven on HEAD.
- ☐ Frictionless deploy auth (gap #2) — next.
- ☐ SDK distribution for external creators (gap #1).
- ☐ Smooth one-step deploy UX (provision-app-if-needed; a `zeroship deploy` that
  creates the app on first push instead of requiring a pre-created `--app`).
- ☐ Agent integration: a control-plane MCP server / Claude Code skill so the
  agent deploys + manages apps directly as tools.
