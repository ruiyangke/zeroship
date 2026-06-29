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
2. ✅ A fresh DB is migrated + the platform stack (control + worker + gateway)
   comes up healthy on the current HEAD binaries.
3. ✅ The app is provisioned + deployed (`dev-provision`, dev-only) and the
   **gateway serves it**: `GET /apps/<name>/` returns the app's `index.html` + JS
   asset, and the **RPC server function executes** in the worker and returns data
   (`/__zeroship/v1/getMessages`). The whole in-monorepo chain passes **9/9**.

## Both halves are now proven end-to-end

### External build (`tests/external_chain.sh`) — gap #1 PROVEN closable
An app **outside the monorepo** installs `@zeroship/*` **from a registry** and
builds a deploy artifact, with zero workspace/file coupling. The script stands up
the `verdaccio` compose service (`config/verdaccio/`), publishes all SDKs
(`scripts/publish-sdks.sh`), scaffolds with `create-zeroship-app` into a temp
dir, `npm install`s (`@zeroship:registry=http://localhost:4873`) — verified to
resolve every `@zeroship/*` from Verdaccio with **no `workspace:`/`file:` links**
— and runs `npm run build` → `dist/app.zship`. So the SDK-distribution mechanism
works; the remaining *productization* step is hosting a real registry (npmjs or a
hosted Verdaccio) instead of the local one. (DB-backed scaffolds that *regenerate*
schema also need the `zeroship-migrate-js` toolchain on PATH; the committed
template ships pre-generated `generated/zeroship/` so a first build doesn't.)

### Deploy auth — unblocked for local/CI; real flow uses `zeroship login`
App CRUD + deploy require a **PAT**, minted only by the platform's auth stack via
`zeroship login` (OAuth device flow) — there is intentionally **no**
master-key/dev shortcut, and a PAT can't be forged for a local control. For
**local/CI** this is unblocked by `dev-provision` (`crates/control/src/bin/`):
a DEV-ONLY internal provisioning tool (gated on `ZEROSHIP_DEV_INSECURE=1`,
needs direct DB+blob access — not a network endpoint; it reuses the same
`zeroship_bundle::ingest` + `Registry::set_deploy_with_manifest` path
`bootstrap_console` uses, and does not touch the production `/api/apps` PAT path).
For the **real agent flow** against a deployed platform, the path is
`zeroship login` (one-time) → `zeroship deploy`.

## Status

- ✅ Starter scaffold + agent `CLAUDE.md` (`examples/starter/`), incl. the SEC-5
  RPC-auth default + public opt-in (`src/server/config.ts`).
- ✅ Real build → `.zship` (vite-plugin) — proven.
- ✅ In-monorepo chain: migrate → stack → deploy → serve + RPC — **9/9**
  (`tests/golden_path.sh`).
- ✅ External build: registry-installed SDKs, scaffolded app builds outside the
  monorepo — **PASS** (`tests/external_chain.sh`) — gap #1 mechanism proven.
- ✅ Local/CI deploy auth unblocked (`dev-provision`); real flow = `zeroship login`.
- ☐ Host a real SDK registry (npmjs / hosted Verdaccio) for production gap #1.
- ☐ Smooth one-step deploy UX (provision-app-if-needed; a `zeroship deploy` that
  creates the app on first push instead of requiring a pre-created `--app`).
- ☐ Agent integration: a control-plane MCP server / Claude Code skill so the
  agent deploys + manages apps directly as tools.
