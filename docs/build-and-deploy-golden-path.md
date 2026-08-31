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
                                               + generated/zeroship/migrations.ir.json
  → zeroship deploy                            ← path, app and control from the file
  → control plane ingests → BlobStore + route registry
  → zeroship migrate                           ← env.db apps ONLY, and REQUIRED
  -> /v1 edge route -> zeroship-migrate-server applies -> per-app schema + role
  → gateway pulls routes (5s) → serves the app (static + RPC + env.* primitives)
```

The scaffolds ship a committed `zeroship.jsonc` at the project root. It names the
deploy target (`app`, `control`) and the two paths that cross the tool boundary
(`build.output`, which the packer writes and `zeroship deploy` uploads;
`migrations.out`, which the build writes and `zeroship migrate` posts from), so
the typed commands above carry no target flags. The flags still exist and still
win over the file, and every command prints what it resolved and from where
before it acts. See
[`docs/reference/project-config.md`](reference/project-config.md) for the file,
its precedence, and named `environments`.

**Deploy is not the last step for an app that uses `env.db`.** The `.zship`
carries the app's code and the folded runtime schema *descriptor*; it does not
carry the migration documents, and the deploy endpoint refuses to apply them
(`crates/zeroship-control/src/api.rs`, `migration_approval_removed`). Applying them is
what creates the per-app schema and the `app_<id>_role` the runtime does
`SET LOCAL ROLE` to on every database call, and `zeroship-migrate-server`'s apply path
is that role's only producer. Deploy without migrating and the app serves its
static assets, dispatches its RPCs, and fails the first `env.db` call with
`role "app_..._role" does not exist` — which reaches the end user as
`{"message":"internal error"}`.

`zeroship migrate` reuses the configured control URL, but control is not in the
request path. The edge routes the complete `/v1/*` namespace directly to
`zeroship-migrate-server`, which verifies the creator bearer,
requires `AppsDeploy` plus app ownership, and intersects the creator draft with
the operator ceiling. The raw migration-service port remains loopback-only at
the host boundary.

The namespace-wide matcher is deliberate. The current
`/v1/apps/{app_id}/migrations/apply` route works through the shared control URL,
and a later database-id re-key does not require another edge rollout. Control
declares no `/v1` routes; `tests/deploy_scripts_gate.sh` enforces both that
collision boundary and the exact Caddy handler.

- **Scaffold:** `examples/starter/` — a minimal, agent-facing zeroship app
  (fetch/static SPA + `getMessages`/`addMessage` RPCs via `@zeroship/rpc/server`,
  validated with zod via `@zeroship/server`) with a `CLAUDE.md` that teaches an
  AI agent the deploy contract, the `env.*` primitives, and the build+deploy
  steps. Copy it, point Claude Code/Codex at it, iterate.
- **Workflow starter:** `examples/workflows-order/` — a raw workflow app with
  `step.run`, `step.sleep`, `step.waitForSignal`, a child workflow, and
  compensators. It is NOT on the deploy path, and the Build/Deploy bullets below
  do not apply to it: its `build` is `tsc -p tsconfig.json`, it declares no
  `@zeroship/vite-plugin` and has no `vite.config.*`, and it emits a generated
  entry under `dist` rather than a `.zship`.
  `examples/workflows-order/README.md` documents the required build before
  serving that generated entry, so this is a SINGLE-TENANT serve example, not a
  copyable counterpart to the starter.
  Copying it and following the two bullets below produces `tsc` output and then a
  deploy pointed at a `dist/app.zship` that was never written; `zeroship deploy`
  accepts only a `.zship` and answers by telling you to run `vite build` with a
  plugin this example does not have. Verified 2026-08-11 by reading its
  package.json, its README, and the deploy arg handling in crates/zeroship-cli/src/main.rs.
- **Build:** `pnpm build` (= `vite build`); the `@zeroship/vite-plugin` discovers
  `"use server"` RPC functions, bundles the server module, and writes
  `dist/app.zship`. Applies to `examples/starter/` and the
  `create-zeroship-app` template.
- **Deploy:** `zeroship deploy`. The archive path comes from `build.output`, the
  target from `app` and `control`, all three read from the project's
  `zeroship.jsonc`; the command prints each resolved value and its source on
  stderr before it uploads. Authentication is separate and never comes from that
  file: `zeroship login`, or `--token=<PAT>` / `ZEROSHIP_TOKEN`. Overrides:
  `zeroship deploy ./other.zship --app=<id> --control=<url>`, and `--env=<name>`
  to select a named environment.
- **Migrate** (apps that use `env.db`): `zeroship migrate`. It posts
  `<migrations.out>/migrations.ir.json` - by default
  `generated/zeroship/migrations.ir.json`, which the build writes beside the
  other two gen-types artifacts - to the `app` and `control` the file names. A
  positional path overrides it. An environment marked `"protected": true`
  requires `--yes`. Idempotent: re-running with nothing new to apply reports
  `Applied 0 migration op(s)`.

## What is validated today (`tests/golden_path.sh`)

Run `nix develop --command bash tests/golden_path.sh` (needs the release binaries
+ a Postgres on :5440). It proves the **load-bearing half** end-to-end with a
REAL vite-built `.zship` (not a hand-packed fixture).

It takes one argument, `--provision=deploy` (the default) or
`--provision=dev-provision`, and it decides whether step 3 ships the artifact
with the real `zeroship deploy` CLI or writes the registry row directly. The
bypass arm announces itself with the repo's skip marker, so a run that did not
exercise the deploy command cannot read like one that did. It does **not** read
`ZEROSHIP_TOKEN`: the deploy arm mints its own creator bearer against the
harness's platform OP and passes it on `--token=`.

1. ✅ `examples/starter` builds to `dist/app.zship` via the real vite-plugin pipeline.
2. ✅ A fresh DB is migrated + the platform stack (control + worker + gateway)
   comes up healthy on the current HEAD binaries.
3. ✅ The app is created through `POST /api/apps` and deployed with
   `zeroship deploy --app=... --token=...` against the running control plane -
   the CLI a creator runs - and the **gateway serves it**: `GET /apps/<name>/`
   returns the app's `index.html` + JS asset, and the **RPC server function
   executes** in the worker (`/__zeroship/v1/getMessages`). The step checks that
   the `deploy_hash` the CLI printed is the one control wrote to
   `zeroship.apps` and `zeroship.app_deploys`, and that the same deploy with a
   bad bearer is refused. Under `--provision=dev-provision` none of that runs:
   the registry row and blob store are written directly and the step says so.

   **The chain as a whole does NOT pass, deliberately.** Measured 2026-08-21 on a
   four-service run at HEAD: **126 passed, 15 failed, exit 1** over nineteen steps,
   141 outcomes, with `--provision=deploy`. The fifteen are the by-design reds the
   harness names on every run and classifies against a declared set: step 10's six
   scaffold comparisons (#260), step 11's two collation reds (#255), step 13's
   three log-visibility reds (#332/#333) and step 12's four app-delete reds (#331).
   The same tree with `--provision=dev-provision` scores **122 passed, 15 failed**;
   the four are step 3's deploy assertions, and the reds are identical.
   The "9/9" this line carried was the count when only the first three steps
   existed, and it survived every step added since - stating "passes" about a
   script that exits non-zero. The 2026-08-11 figure that replaced it (67/8, twelve
   steps) had rotted the same way by 2026-08-21.

   WHAT STEP 3 DOES NOT SHOW. `getMessages` is `query(async () => messages)` over
   a module-level array literal seeded in `examples/starter/src/server.ts`, and
   `addMessage` pushes to that same array. The RPC returns a value the handler
   closed over, not a value read from anywhere, so this leg proves dispatch and
   serialization and says NOTHING about the data plane: THIS STEP's outcomes are
   unchanged with `env.db` broken or absent. The starter's only `env.*` call is
   `env.auth.getUser`. Scoped to the step on purpose - the claim used to cover the
   whole chain, which stopped being true once steps 9, 9b and 11 started driving
   `env.db` through `examples/db-todos` (22 of the 75 outcomes).
   For a leg that does exercise `env.db`, the corpus has
   eight examples that reach it (heaviest: `hr-system` 117 call sites,
   `db-todos` 35, `db-e2e` 20). **`db-todos` IS wired into this chain** - steps 9,
   9b and 11 - and the other seven are not. "None of them wired" was true when
   written and was left standing after step 9 landed.

   Two consequences worth knowing before copying the starter: the array is
   per-isolate state, so with one isolate per (app, live deploy) plus LRU
   eviction an `addMessage` result can vanish and reappear depending on which
   isolate serves the next request; and it is a placeholder, not the house
   persistence pattern, despite `examples/starter/CLAUDE.md` being what an AI
   coding agent reads first.

## Both halves are now proven end-to-end

### External build (`tests/external_chain.sh`) — BROKEN since 2026-07-14

> **This section describes a run that no longer happens.** It was accurate when
> written on 2026-06-29. On 2026-07-14 (`4e0f49a16`) `@zeroship/vite-plugin`
> took a dependency on `zero-migrate` + `zeroship-migrate-node`, which are published
> to no registry, so `npm install` in a scaffolded app now dies with `E404`
> before the build step this section claims to have proven. Measured 2026-08-10;
> see task #265 and `tests/golden_path.sh:2786`. Nothing in CI runs this
> script, which is why the regression sat for weeks.

The claim below, as originally written: an app **outside the monorepo** installs `@zeroship/*` **from a registry** and
builds a deploy artifact, with zero workspace/file coupling. The script stands up
the `verdaccio` compose service (`deploy/verdaccio/`), publishes all SDKs
(`deploy/scripts/publish-sdks.sh`), scaffolds with `create-zeroship-app` into a temp
dir, `npm install`s (`@zeroship:registry=http://localhost:4873`) — verified to
resolve every `@zeroship/*` from Verdaccio with **no `workspace:`/`file:` links**
— and runs `npm run build` → `dist/app.zship`. So the SDK-distribution mechanism
works; the remaining *productization* step is hosting a real registry (npmjs or a
hosted Verdaccio) instead of the local one. (DB-backed scaffolds regenerate
schema artifacts in-process through `@zeroship/vite-plugin`; no authoring CLI is
required on PATH. The committed template also ships pre-generated
`generated/zeroship/`.)

### Deploy auth — unblocked for local/CI; real flow uses `zeroship login`
App CRUD + deploy require a **PAT**, minted only by the platform's auth stack via
`zeroship login` (OAuth device flow) — there is intentionally **no**
master-key/dev shortcut, and a PAT can't be forged for a local control. For
**local/CI** this is unblocked by `dev-provision` (`crates/zeroship-control/src/bin/`):
a local/CI internal provisioning tool that needs direct DB+blob access and is
not a network endpoint. It reuses the same
`zeroship_bundle::ingest` + `Registry::set_deploy_with_manifest` path the deploy
handler uses, and does not touch the production `/api/apps` PAT path).
For the **real agent flow** against a deployed platform, the path is
`zeroship login` (one-time) → `zeroship deploy`.

## Status

- ✅ Starter scaffold + agent `CLAUDE.md` (`examples/starter/`), incl. the SEC-5
  RPC-auth default + public opt-in (`src/server/config.ts`).
- ✅ Durable workflow starter (`examples/workflows-order/`) covering steps,
  sleeps, signals, child calls, and compensation.
- ✅ Real build → `.zship` (vite-plugin) — proven.
- ⚠️ In-monorepo chain: migrate → stack → deploy → serve + RPC + dev-vs-deployed.
  **67 passed, 8 failed, exit 1** (`tests/golden_path.sh`, measured 2026-08-11).
  The eight are RED AT HEAD BY DESIGN and the harness names them on every run:
  step 10's six scaffold comparisons (#260) and step 11's two collation reds
  (#255). Everything the chain covers works; the two open decisions are what keep
  it non-zero, so the CI `golden-path` job is red until they land. This row said
  `✅ ... 9/9` long after both were true.
- ❌ External build: registry-installed SDKs, scaffolded app builds outside the
  monorepo — **FAILS at `npm install`** (`tests/external_chain.sh`). Published
  `@zeroship/vite-plugin` requires `zero-migrate@0.1.0` +
  `zeroship-migrate-node@0.1.0`; neither is on npmjs at any version. This was PASS
  when recorded on 2026-06-29 and regressed on 2026-07-14. Task #265.
- ✅ Local/CI deploy auth unblocked (`dev-provision`); real flow = `zeroship login`.
- ☐ Host a real SDK registry (npmjs / hosted Verdaccio) for production gap #1.
- ✅ Smooth one-step deploy UX. `zeroship deploy` with no arguments deploys the
  artifact the build wrote to the app the project names.
  Read this row carefully, because two of its three parts were already true when
  it was written as open: **provision-app-if-needed already existed** -
  `resolve_or_create_app` looks a non-uuid `--app` up by name and creates it on a
  miss, with `--no-create` to turn that off; and the TRANSPORT half was done -
  Caddy now routes `control.<domain>` to control, while
  `deploy/scripts/deploy-app.sh` retains an SSH fallback to the loopback
  services. What was actually missing, and is what landed: the created id was
  reported to stderr and then thrown away, so the next command needed the flag
  again, and the archive path plus the target had to be retyped on every
  invocation. `zeroship.jsonc` now records all three, and an auto-create splices
  the new id back into the file's root `app` member (or prints the exact line to
  paste when the member is absent). A deploy under `--env=<name>` deliberately
  prints rather than writes: a splice can only touch a top-level member, and
  staging's id at the root is where every un-flagged command would then read
  production's.
- ☐ Agent integration: a control-plane MCP server / Claude Code skill so the
  agent deploys + manages apps directly as tools.
