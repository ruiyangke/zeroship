# zeroship

A platform where anyone can create, launch, and monetize software — without writing code. Creators describe what they want in natural language. AI builds it. The platform handles everything: hosting, database, auth, payments, scaling. The platform takes 15%; the creator keeps the rest. Think Shopify for AI-generated apps.

This file is the AI-agent landing page. Read the **task router** below first.

---

## Where to start, by task

| If you're working on… | Start here |
| --- | --- |
| **Routing / dispatch / manifest** | `docs/architecture/gateway-routing.md` · `crates/gateway/src/dispatch.rs` · `crates/core/src/types.rs` (`Manifest`, `Rule`, `Match`, `Action`) |
| **V8 runtime** (fetch, streams, WebSocket, modules) | `docs/architecture/runtime.md` · `crates/runtime/` |
| **Adding a native primitive** (`zeroship.*`) | `docs/reference/plugin-system.md` · `crates/runtime-macros/` · `crates/plugin-{db,kv,storage}/` |
| **Control plane** (app CRUD, deploy, env, route registry) | `docs/architecture/control-plane.md` · `crates/control/src/api.rs` · `crates/control/src/registry.rs` |
| **Deploy artifact** (.zsdeploy + manifest + blob storage) | `docs/reference/zsdeploy.md` · `docs/architecture/blob-store.md` · `crates/control/src/deploy.rs` · `crates/core/src/blob.rs` |
| **Auth** (creator + end-user, OAuth, JWT) | `docs/reference/auth.md` · `crates/control/src/auth_*.rs` · `crates/gateway/src/auth.rs` |
| **The DB SDK** (`@zeroship/db`) | `docs/reference/db.md` · `docs/reference/mongoose-compat.md` · `crates/plugin-db/` |
| **Billing / metering / Stripe Connect** | `docs/reference/billing-metering.md` · `crates/control/src/{stripe_handlers,stripe_store,metering}.rs` |
| **WebSocket** (RFC 6455 implementation) | `docs/reference/websocket-design.md` · `crates/runtime/src/` (search `WebSocket`) |
| **Vite plugin / build pipeline** | `docs/reference/vite-environment-api.md` · `sdks/vite-plugin/` |
| **Node.js compat** (npm packages in V8) | `docs/reference/node-compat.md` · `crates/runtime/src/init.rs` |
| **Benchmarks** | `crates/runtime/benches/` · `docs/reference/zerobench.md` · `docs/benchmarks/` |
| **Local dev setup** | `docs/runbooks/local-dev.md` |
| **Multi-node / Docker Compose** | `docs/runbooks/docker-compose.md` |
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
           → Workers (V8)    App code · zeroship.{db,auth,kv,storage,meter} primitives
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
├── core/             Shared types, typed_id (UUIDv7 + base62), VFS, auth utils, Manifest
├── compio-postgres/  PostgreSQL driver (compio-native, replaces sqlx)
├── compio-redis/     Redis driver (cluster-aware, compio-native)
├── bundle/           .appbundle binary format (read, write, lazy decompress)
├── runtime/          V8 + compio event loop + fetch + WebSocket + crypto + auth context
├── runtime-macros/   #[zeroship_op] proc macro
├── compiler/         SWC + esbuild → .appbundle
├── plugin-db/        zeroship.db.* native ops
├── plugin-kv/        zeroship.kv.* native ops
├── plugin-storage/   zeroship.storage.* native ops
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
- **Wire formats are immutable contracts.** `Manifest`, `RouteEntry`, `AppRecord`, `.appbundle` headers — back-compat is required at the wire level even when internal types change.
- **Native primitives are the kernel.** Anything user code can do via `fetch` or composition belongs in an npm package (`@zeroship/*`), not in Rust. The native surface is small and stable on purpose.
- **The gateway is dumb.** It does manifest dispatch, JWT, rate-limit, CHWBL routing — and forwards. All app logic runs in the worker.

---

## SDK layers

Two layers: native primitives (Rust kernel) and npm packages (JS ecosystem).

### Native primitives (`zeroship.*` global)

Registered by Rust on every V8 isolate. The "syscalls" of the platform — small, stable, low-level:

```
zeroship.db.*       structured database operations (no raw SQL)
zeroship.auth.*     getUser/requireUser (reads gateway-injected user context)
zeroship.storage.*  object storage put/get/delete
zeroship.kv.*       key-value get/set/delete
zeroship.meter.*    billing counter increment
zeroship.assets.*   runtime-emitted static asset CRUD (manifest runtime_assets)
```

Creators don't call these directly. SDK packages wrap them.

### SDK packages (`@zeroship/*` npm scope)

```javascript
import { createDb, t, schema } from "@zeroship/db";
import { auth } from "@zeroship/auth";
import { storage } from "@zeroship/storage";
import { kv } from "@zeroship/kv";
```

SDK packages call `zeroship.*` primitives internally. Validation, query building, error mapping, TypeScript types all live in JS. They evolve independently of the Rust runtime.

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
- `db.md` — `@zeroship/db`: createDb, schema, CRUD, aggregation, naming strategy
- `auth.md` — platform-managed auth, gateway JWT, OAuth, consent
- `billing-metering.md` — Meter trait, 25+ metrics, pricing, spending limits
- `zsdeploy.md` — `.zsdeploy` deploy artifact format (replaces the deleted `.appbundle`)
- `websocket-design.md` — WebSocketPair, RFC 6455
- `plugin-system.md` — how to add a `zeroship.*` namespace
- `node-compat.md` — Node.js module resolution in V8
- `vite-environment-api.md` — Vite dev server inside the V8 runtime
- `mongoose-compat.md` — what `@zeroship/db` matches from Mongoose
- `zerobench.md` — the HTTP/SSE/WS benchmark tool

---

## Development

```bash
# Build (workspace)
cargo build --release

# Run single-tenant (dev)
zeroship serve myapp.js --port 3000

# Run platform (multi-node)
zeroship-control --port 9090 --db postgres://... --bundles ./bundles --auth-secret <s>
zeroship-worker --port 8080 --workers 16 --control http://localhost:9090
zeroship-gate    --port 80   --control http://localhost:9090 --workers http://localhost:8080 --auth-secret <s>

# Deploy
zeroship deploy ./src --app=<uuid> --control=http://localhost:9090 --key=<master>

# Docker Compose
docker compose up -d --scale worker=10

# Tests (per crate)
cargo test -p zeroship-core
cargo test -p zeroship-gateway
cargo test -p zeroship-bundle
cargo test -p compio-postgres -- --test-threads=1   # needs DB

# E2E + benchmarks
./tests/e2e_platform.sh
./tests/bench_platform.sh
```

Detailed setup: `docs/runbooks/local-dev.md`. Multi-node: `docs/runbooks/docker-compose.md`.

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
