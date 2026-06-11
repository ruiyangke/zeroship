# zeroship

A platform where anyone can create, launch, and monetize software — without writing code. Creators describe what they want in natural language. AI builds it. The platform handles everything: hosting, database, auth, payments, scaling. The platform takes 15%; the creator keeps the rest. Think Shopify for AI-generated apps.

This file is the AI-agent landing page. Read the **task router** below first.

---

## Development status — pre-launch, no back-compat

**Zeroship has never been published.** No production users, no production tenants, no creator apps in the wild. Every API, every wire format, every schema is fair game to break.

This is a deliberate stance — not a limitation. Pre-launch is the moment to get shapes right. Building back-compat infrastructure for users that don't exist is wasted work.

**Implications for any change you propose:**

- **No `@deprecated` aliases.** Rename the symbol. Delete the old name. One PR.
- **No migration shims, no detect-and-warn paths, no "legacy mode" fallbacks.** If the new shape is right, the old one disappears in the same PR.
- **No back-compat shims for wire format / SDK contract / V8 RPC.** Break the shape and update every caller in the same change.
- **No "scan the creator codebase" CLI tooling.** There are no creator codebases.
- **No "ALTER existing tables for new system fields" backfills.** There are no existing tables in production. The detect-and-warn arms that exist today (plugin-db PR 5 Path C, the unreached migration scanners) are dead code waiting for the post-launch simplification pass.
- **Wire-format versioning is for code-evolution discipline, not user-compat.** P5 encrypted ciphertext flags (`0x01` → `0x02`), masking sentinel formats, etc. exist so dev/test databases can re-decrypt across runtime versions — not so deployed apps can be left alone.

**When this changes** (post-launch): this section gets a new "Migration discipline" subsection and `feedback_no_backward_compat.md` gets retired. Until then, treat back-compat constraints as user-requested guardrails only, never defaults.

---

## Where to start, by task

| If you're working on… | Start here |
| --- | --- |
| **Routing / dispatch / manifest** | `docs/architecture/gateway-routing.md` · `crates/gateway/src/router/dispatch.rs` · `crates/bundle/src/{manifest,rule}.rs` (`Manifest`, `Rule`, `Match`, `Action`) |
| **V8 runtime** (fetch, streams, WebSocket, modules) | `docs/architecture/runtime.md` · `crates/runtime/` |
| **Adding a native primitive** (`env.*`) | `docs/reference/plugin-system.md` · `crates/runtime-macros/` · `crates/plugin-{db,kv,storage}/` |
| **Control plane** (app CRUD, deploy, env, route registry) | `docs/architecture/control-plane.md` · `crates/control/src/api.rs` · `crates/control/src/registry.rs` |
| **Control-plane TypeScript client** (`@zeroship/control`) | `docs/reference/control.md` · `sdks/control/` · `crates/control/src/{api,env_handlers,token_handlers}.rs` |
| **Deploy artifact** (.zship + manifest + blob storage) | `docs/reference/zship.md` · `docs/architecture/blob-store.md` · `crates/bundle/` (manifest types, BlobStore, pack/unpack) |
| **Auth** (OIDC IdP + login UI + RPs) | `docs/reference/auth.md` · `crates/auth/` · `crates/gateway/src/oidc_rp.rs` |
| **The DB SDK** (`@zeroship/db`) | `docs/reference/db.md` · `crates/plugin-db/` |
| **The KV SDK** (`@zeroship/kv`) | `docs/reference/kv.md` · `sdks/kv/` · `crates/plugin-kv/` |
| **The RPC SDK / server functions** (`@zeroship/rpc`) | `docs/reference/rpc.md` · `sdks/rpc/` · `sdks/vite-plugin/src/{transform,rpc-registry,manifest}.ts` · `sdks/bootstrap/src/dispatcher.ts` |
| **zeroship deploy contract** (`default = { schema?, fetch?, rpc? }`, dispatcher, raw-JS deploys) | `docs/reference/zeroship-standard.md` · `sdks/bootstrap/src/{dispatcher,runtime-entry}.ts` · `crates/runtime/src/core/init.rs` |
| **Framework-internal coordination** (`installSchema`, `__zsDispatch`, dev-entry) | `sdks/bootstrap/` · `sdks/bootstrap/README.md` |
| **Billing / metering / Stripe Connect** | `docs/reference/billing-metering.md` · `crates/control/src/{stripe_handlers,stripe_store,metering}.rs` |
| **WebSocket** (RFC 6455 implementation) | `docs/reference/websocket-design.md` · `crates/runtime/src/` (search `WebSocket`) |
| **Vite plugin / build pipeline** (synthetic entry is a thin normaliser; runtime owns dispatch) | `docs/reference/vite-plugin.md` · `docs/reference/vite-environment-api.md` · `sdks/vite-plugin/src/rpc-registry.ts` |
| **Node.js compat** (npm packages in V8) | `docs/reference/node-compat.md` · `crates/runtime/src/core/init.rs` |
| **Benchmarks** | `crates/runtime/benches/` · `docs/reference/zerobench.md` · `docs/archive/benchmarks/` |
| **Local dev setup** | `docs/runbooks/local-dev.md` |
| **Multi-node / Docker Compose** | `docs/runbooks/docker-compose.md` |
| **Nomad + Cloud Hypervisor sandbox backend** | `docs/runbooks/sandbox-nomad-ch.md` · `crates/sandbox/src/backend/nomad_ch.rs` · `nomad-driver-ch/` |
| **Why we made decision X** | `docs/decisions/` (date-prefixed ADRs, immutable once landed) |
| **Pre-ship proposals** | `docs/proposals/` (active, may not have shipped) |
| **AI-builder competitive landscape** | `docs/research/ai-builder-features.md` |

---

## System map

Two systems, shared infrastructure, **zero tokio** — everything on compio/io_uring.

### System 1 — Creator Platform (creators build, deploy, monetize)

```
Creator Dashboard (web UI)
  → Control Plane     App CRUD, deploy, billing, route registry
  → Builder Service   AI app generation, templates, preview
```

### System 2 — App Runtime (end users hit the apps creators ship)

```
End Users → Gateway          JWT, rate-limit, manifest dispatch, asset proxy, CHWBL routing
           → Auth Service    Login/signup, OAuth, consent, sessions
           → Workers (V8)    App code · env.{db,auth,kv,storage,meter} primitives
```

### Shared

```
PostgreSQL          One database, separate schemas (control, auth, per-app)
Object Storage      Bundles, assets, user uploads (LocalFs in dev; S3/R2 in prod)
DNS                 console.zeroship.ai · auth.zeroship.ai · {app}.zeroship.ai
```

### How they connect

```
Creator Platform                          App Runtime
────────────────                          ───────────
Control Plane ───deploy bundle──────────→ Object Storage
              ───update routes──────────→ Gateway (HTTP pull every 5s)
              ───register model─────────→ PostgreSQL (per-app schema)

Auth Service ←──401 redirect────────────← Gateway (end-user not logged in)
             ───JWT cookie──────────────→ Gateway (validates per request)
             ───user profile────────────→ Workers (via ZeroShip-User header)
```

For the long form with sequence diagrams, see `docs/architecture/distributed.md`.

---

## Crate index

```
crates/
├── core/             Inter-service wire types (RouteEntry, AppRecord, UsageReport, ControlEvent), typed_id, auth utils, observability
├── bundle/           .zship deploy artifact: Manifest types, BlobStore, BundleStore, tar.zst pack/unpack
├── compio-postgres/  PostgreSQL driver (compio-native, replaces sqlx)
├── compio-redis/     Redis driver (cluster-aware, compio-native)
├── runtime/          V8 + compio event loop + fetch + WebSocket + crypto + auth context
├── runtime-macros/   #[v8_class] proc macro (V8 ObjectTemplate-backed classes)
├── plugin-db/        env.db.* native ops
├── plugin-kv/        env.kv.* native ops
├── plugin-storage/   env.storage.* native ops
│
│ System 1 — Creator Platform
├── control/          Control plane (app CRUD, deploy, billing, env, route registry)
│
│ System 2 — App Runtime
├── gateway/          Manifest dispatch, JWT, rate-limit, CHWBL routing, asset proxy
├── worker/           V8-per-thread, on-demand bundle loading, LRU eviction
│
│ Tools
└── cli/              CLI: build, serve, deploy, inspect
```

Per-crate READMEs (where present) carry the responsibility statement and list of important files.

---

## Key invariants

These don't change. If you're about to violate one, stop and ask.

- **Zero tokio in the stack.** Everything is compio/io_uring. Drivers are bespoke (`compio-postgres`, `compio-redis`).
- **V8 per thread, one isolate per app.** Worker uses LRU eviction; isolates `enter`/`exit` to allow many apps per thread (`crates/worker/src/cache.rs`).
- **typed_id everywhere.** UUIDv7 + base62 + entity prefix (`usr_…`, `app_…`, `ses_…`). Defined in `crates/core/src/typed_id.rs`.
- **Wire formats are explicit contracts.** `Manifest`, `RouteEntry`, `AppRecord`, `.zship` archive layout, and RPC envelopes must be changed deliberately. Pre-launch can break them, but every producer, consumer, fixture, and reference doc changes in the same patch; no hidden compatibility shim.
- **Native primitives are the kernel.** Anything user code can do via `fetch` or composition belongs in an npm package (`@zeroship/*`), not in Rust. The native surface is small and stable on purpose.
- **The gateway is dumb.** It does manifest dispatch, JWT, rate-limit, CHWBL routing — and forwards. All app logic runs in the worker.

---

## SDK layers

Two layers: native primitives (Rust kernel) and npm packages (JS ecosystem).

### Native primitives (`env.*` namespaces)

Registered native primitives appear as namespaces on the `env` object
(the 2nd arg to `fetch(req, env, ctx)` and the `env` named export of the
`zeroship` module). These are the platform "syscalls": small, stable,
low-level operations that SDK packages wrap.

Creator-facing namespaces registered today:

```
env.db.*       structured database operations (no raw SQL)
env.storage.*  object storage put/get/delete
env.kv.*       key-value get/set/delete
env.auth.*     getUser/requireUser — per-request identity (AuthPlugin, registered
               on the worker + CLI `zeroship serve` vectors). Fed in prod by the
               gateway's `ZeroShip-User` header; in dev by the dev-auth provider
               (see `docs/reference/auth-dev-tier.md`).
```

Planned or platform-internal namespaces must be documented as such until the
runtime actually registers them:

```
env.meter.*    billing counter increment
env.assets.*   runtime-emitted static asset CRUD (manifest runtime_assets)
```

Creators don't call these directly. SDK packages wrap them.

### SDK packages (`@zeroship/*` npm scope)

```javascript
import { t, schema } from "@zeroship/db";
import { env } from "zeroship";
import { auth } from "@zeroship/auth";
import { storage } from "@zeroship/storage";
import { kv } from "@zeroship/kv";
import { query, mutation } from "@zeroship/rpc/server";

// Declare your schema once via the `export default { schema }` convention;
// the platform installs typed Collection wrappers on `env.db` at app boot.
// Handlers then write `env.db.users.find(...)` directly.
```

SDK packages call the `env.*` native primitives internally. Validation, query building, error mapping, TypeScript types all live in JS. They evolve independently of the Rust runtime.

### Framework-internal: `@zeroship/bootstrap`

`@zeroship/bootstrap` is the coordination package the runtime crate and Vite plugin both consume. It owns:

- `installSchema(schema, env.db)` — orchestrator behind `export default { schema }`
- `__zsDispatch` — the embedded RPC dispatcher (input parse / capability / stream framing)
- `normalizeUserModule` — namespace → `{ schema, fetch, rpc }` shape
- `createFetchHandler` — WinterCG fetch wrapper routing `/__zeroship/v1/<id>` through the dispatcher
- `runtime-entry.ts` — TLA orchestrator the runtime crate `include_str!`s
- `dev-entry.ts` — dev-mode equivalent the Vite plugin's dev-bootstrap delegates to

**User code MUST NOT import `@zeroship/bootstrap`.** It carries no back-compat guarantee; the runtime crate and Vite plugin are the only stable consumers. See `sdks/bootstrap/README.md`.

### When to add a native primitive vs. an npm package

| Needs Rust (new primitive) | Pure JS (npm package) |
| --- | --- |
| New storage backend | `@zeroship/email` (calls fetch) |
| New database engine | `@zeroship/payments` (Stripe wrapper) |
| Low-level crypto ops | `@zeroship/ai` (OpenAI/Claude wrapper) |
| New protocol (e.g., gRPC) | `@zeroship/permissions` (uses db + auth) |

Default to npm package. Native primitives are forever.

---

## Reference docs

Stable contracts, live in `docs/reference/`:

- `api-design-guidelines.md` — 10 principles for AI-friendly APIs
- `zeroship-standard.md` — the deploy contract: `default = { schema?, fetch?, rpc? }`, dispatch, raw-JS deploys
- `control.md` — `@zeroship/control`: framework-neutral client for control-plane app, auth, deploy, and env endpoints
- `db.md` — `@zeroship/db`: `default.schema` discovery, CRUD, aggregation, naming strategy
- `kv.md` — `@zeroship/kv`: ephemeral key-value surface, TTL, atomic counters, `setIfAbsent`, paginated `list`
- `rpc.md` — `@zeroship/rpc`: server wrappers, generated and manual clients, transport, transformers, retries
- `auth.md` — platform-managed auth, gateway JWT, OAuth, consent
- `auth-dev-tier.md` — the self-contained `pnpm dev` auth provider (the peer of `env.db`→SQLite / `env.kv`→redb): contract parity, the dev impl, the dev-only-by-construction guarantee
- `billing-metering.md` — Meter trait, 25+ metrics, pricing, spending limits
- `zship.md` — `.zship` deploy artifact format (tar.zst with content-addressed blobs)
- `websocket-design.md` — WebSocketPair, RFC 6455
- `plugin-system.md` — how to add an `env.*` namespace
- `node-compat.md` — Node.js module resolution in V8
- `vite-plugin.md` — `@zeroship/vite-plugin`: node-compat shims, server-procedure discovery, the synthetic server entry, dev runtime + `.zship` build
- `vite-environment-api.md` — Vite dev server inside the V8 runtime
- `runtime-limits.md` — per-app `AppRuntimeLimits` vs runtime-side `RuntimeLimits` (CPU, wall timeout, heap), plus idle-GC knobs
- `sqlite-divergences.md` — intentional Postgres↔SQLite differences in vector/full-text/spatial search, transaction isolation, locking, and text ordering
- `zerobench.md` — the HTTP/SSE/WS benchmark tool

---

## Development

```bash
# Build (workspace) — build the SDKs FIRST. The runtime crate
# include_str!s `sdks/bootstrap/dist/{runtime-entry,dispatcher}.js`,
# so `pnpm build` must run before `cargo build -p zeroship-runtime`.
# Root `pnpm build` respects the dependency graph (bootstrap → db);
# cargo then sees the freshly emitted dist files.
pnpm build
cargo build --release

# Run single-tenant (dev)
zeroship serve myapp.js --port 3000

# Run platform (multi-node)
zeroship-control --port 9090 --db postgres://... --blob-store ./bundles --control-key <k>
zeroship-worker --port 8080 --worker-threads 16 --control http://localhost:9090 --control-key <k> --blob-store ./bundles
zeroship-gate    --port 80   --control http://localhost:9090 --control-key <k> --workers http://localhost:8080 --blob-store ./bundles

# Deploy (a pre-built .zship artifact; auth via `zeroship login`, --token=<PAT>, or ZEROSHIP_TOKEN)
zeroship deploy ./dist/app.zship --app=<uuid> --control=http://localhost:9090 --token=<PAT>

# Docker Compose
docker compose up -d --scale worker=10

# Tests (per crate)
cargo test -p zeroship-core
cargo test -p zeroship-gateway
cargo test -p zeroship-runtime --lib
cargo test -p compio-postgres -- --test-threads=1   # needs DB

# Web Platform Tests (WPT) — fetched on demand by setup-wpt.sh, NOT
# tracked in git. The script shallow-clones a pinned commit into
# crates/runtime/tests/wpt/ (gitignored). The `crates/runtime/tests/
# wpt_*.rs` runners `include_str!` upstream files verbatim (test
# files stay pristine — any shims/skips/sentinels live in the Rust
# runner code).
# After cloning the repo:
./crates/runtime/tests/setup-wpt.sh                 # ~930 MB working tree at depth=1
# Bump the pin via WPT_COMMIT env var; default is the last-known-good
# commit baked into setup-wpt.sh. Re-run after pulling if the pin moves.

# E2E + benchmarks
./tests/e2e_platform.sh
./tests/bench_platform.sh
```

Detailed setup: `docs/runbooks/local-dev.md`. Multi-node: `docs/runbooks/docker-compose.md`.

---

## @zeroship/ui — Storybook MCP

When working on @zeroship/ui components, an MCP server is available at
`http://127.0.0.1:6006/mcp` while `pnpm --filter @zeroship/ui storybook`
is running. It exposes the standard Storybook MCP tools:

- `list-all-documentation` — every documented component
- `get-documentation` — props + examples for a specific component
- `get-story-documentation` — single-story detail
- Test Runner integration — execute the `play()` + a11y suite

Before referencing any @zeroship/ui prop in generated code, call
`get-documentation` to verify it exists. Do not assume props by
naming convention.

Tests for the UI library run via Storybook Test Runner:

```bash
pnpm --filter @zeroship/ui test-storybook          # against a running storybook
pnpm --filter @zeroship/ui test-storybook:ci       # boots http-server + runs runner
pnpm --filter @zeroship/ui test-storybook:coverage # + Istanbul report at sdks/ui/coverage/
```

Conventions for writing `play()` interactions and using
@storybook/test live in `sdks/ui/.storybook/CONVENTIONS.md`.

---

## Revenue model

```
Creator's app earns $100/mo from subscribers:
  Stripe fees:    -$3.20
  Platform (15%): -$15.00
  Creator keeps:  $81.80

Infrastructure cost per app: ~$0.12/mo (98% gross margin)
```

The platform only earns when creators earn. Aligned incentives.
