# zeroship

A platform where anyone can create, launch, and monetize software — without writing code.

Creators describe what they want. AI builds it. The platform handles everything: hosting, database, auth, payments, scaling. Creators earn money. The platform takes 15%.

Think Shopify for AI-generated apps.

## Vision

Non-technical creators should be able to go from idea to profitable app in minutes:

1. Creator: "I want a recipe sharing app with user accounts and a paid tier"
2. AI generates the full app (code, database schema, auth flow, payment integration)
3. Platform deploys and runs it (compute, database, auth, billing — all handled)
4. Creator sets pricing (free tier + $9/mo pro)
5. End users sign up, subscribe, use the app
6. Creator gets paid (Stripe → 85% to creator, 15% platform fee)

The creator never touches code, infrastructure, or ops. They focus on their product and audience.

## Architecture

Two systems, shared infrastructure, zero tokio, everything on compio/io_uring.

### System 1: Creator Platform

For creators — build, deploy, manage, monetize apps.

```
Creator Dashboard (web UI)
  → Control Plane     App CRUD, deploy, billing, analytics, route registry
  → Builder Service   AI app generation, templates, preview (future)
```

### System 2: App Runtime

For end users — use the apps creators built.

```
End Users → Gateway          JWT validation, rate limiting, CHWBL routing, static assets
           → Auth Service    Login/signup, OAuth, consent, sessions
           → Workers (V8)    App code, zeroship.db/auth/kv/storage primitives
```

### Shared Infrastructure

```
PostgreSQL          One database, separate schemas (control, auth, per-app)
Object Storage      Bundles, assets, user uploads
DNS/Domains         console.zeroship.ai, auth.zeroship.ai, {app}.zeroship.ai
```

### How they connect

```
Creator Platform                          App Runtime
────────────────                          ───────────
Control Plane ───deploy bundle──────────→ Object Storage
              ───update routes──────────→ Gateway (syncs every 5s)
              ───register model─────────→ PostgreSQL (per-app schema)

Auth Service ←──401 redirect────────────← Gateway (end-user not logged in)
             ───JWT cookie──────────────→ Gateway (validates on every request)
             ───user profile────────────→ Workers (via ZeroShip-User header)
```

### Key technical decisions

- **Runtime**: V8 + compio/io_uring (394K req/s raw, 354K through full pipeline)
- **HTTP framework**: ntex + compio (matches raw httparse at scale)
- **Database**: zeroship-pg (compio-native PostgreSQL driver, no tokio)
- **Routing**: CHWBL (Consistent Hashing with Bounded Loads, XXH3, 150 vnodes)
- **Gateway → worker**: HTTP/1.1 keep-alive connection pool + Unix domain socket support
- **Bundle format**: .appbundle (APPB magic, zstd compression, SHA-256 integrity, lazy decompression)
- **Worker dispatch**: V8 per thread (Option B), on-demand bundle loading, LRU eviction
- **Sync**: HTTP pull every 5s (worker polls control for version changes)

## Crates

```
crates/
├── core/           Shared types, typed_id (UUIDv7 + base62), VFS, auth utilities
├── pg/             PostgreSQL driver (compio-native, zero tokio)
├── bundle/         .appbundle binary format (read, write, lazy decompress)
├── runtime/        V8 engine + compio event loop + fetch + WebSocket + crypto + auth context
├── runtime-macros/ #[zeroship_op] proc macro
├── compiler/       SWC + esbuild → .appbundle
│
│ System 1: Creator Platform
├── control/        Control plane (app CRUD, deploy, billing, route registry)
│
│ System 2: App Runtime
├── auth/           Auth service (users, login, OAuth, consent, JWT) [to be extracted]
├── gateway/        Gateway (JWT validation, rate limiting, CHWBL routing, proxy)
├── worker/         Worker (V8 per thread, on-demand bundle loading, LRU)
│
│ Tools
└── cli/            CLI: build, serve, deploy, inspect
```

## What's built

- V8 runtime: fetch, WebSocket (RFC 6455), WebCrypto, streams, .appbundle
- Platform: control + auth + gateway + worker (4 components)
- PostgreSQL driver (compio-native, replaces sqlx, eliminates tokio)
- CHWBL routing with XXH3, connection pool, UDS support
- On-demand V8 loading, LRU eviction, V8 isolate enter/exit for multi-app
- Auth service: user registration, login, bcrypt, JWT (HS256), OAuth (pluggable), consent
- Gateway JWT middleware: cookie validation, user injection, 401 redirect
- Typed IDs: UUIDv7 + base62 encoding + entity prefix (usr_, app_, ses_)
- Docker Compose deployment (tested at 50 workers)
- CLI: zeroship build, serve, deploy, inspect
- E2E tests (20/20 passing), benchmarks (354K req/s pipeline)
- Zero tokio in the entire stack

## SDK Architecture

Two layers: native primitives (Rust kernel) and npm packages (JS ecosystem).

### Native primitives (`zeroship.*` global)

Registered by Rust on every V8 isolate. The "syscalls" of the platform — stable, secure, low-level:

```
zeroship.db.*       — structured database operations (no raw SQL)
zeroship.auth.*     — getUser/requireUser (reads gateway-injected user context)
zeroship.storage.*  — object storage put/get/delete
zeroship.kv.*       — key-value get/set/delete
zeroship.meter.*    — billing counter increment
```

Creators don't call these directly. SDK packages wrap them.

### SDK packages (`@zeroship/*` npm scope)

High-level APIs published as standard npm packages. Creators install and import them:

```javascript
import { createDb, t, schema } from "@zeroship/db";
import { auth } from "@zeroship/auth";
import { storage } from "@zeroship/storage";
import { kv } from "@zeroship/kv";
```

SDK packages call `zeroship.*` primitives internally. They handle validation, query building, error mapping, and TypeScript types. They evolve independently of the Rust runtime.

### Extensibility

Features that compose existing primitives are pure JS packages — no Rust needed:

```
Needs Rust (new primitive):        Pure JS (npm package):
  New storage backend               @zeroship/email (calls fetch)
  New database engine                @zeroship/payments (calls fetch to Stripe)
  Low-level crypto ops               @zeroship/ai (calls fetch to OpenAI/Claude)
                                     @zeroship/permissions (uses db + auth)
                                     @zeroship/analytics (uses db)
                                     Third-party: @cooldev/zeroship-redis
```

~90% of new features are pure JS. The native layer is the stable kernel.

## What's next

### Phase 2: Creator platform (the product) — IN PROGRESS

- **@zeroship/db** — DONE: createDb, schema builder, typed CRUD, naming strategy, soft delete, transactions, aggregation
- **@zeroship/auth** — DONE: platform-managed auth, getUser/requireUser, gateway JWT, OAuth (pluggable)
- **@zeroship/storage** — per-app file storage, put/get/delete
- **@zeroship/kv** — key-value store, sessions, cache, feature flags
- **Stripe Connect** — creators connect Stripe, end users subscribe, revenue splits
- **Creator dashboard** — web UI for apps, pricing, revenue, analytics
- **Custom domains** — {app-name}.zeroship.ai + creator's own domain

### Phase 3: AI app builder (the differentiator)

- Creator describes the app in natural language
- AI generates: app code, database schema, auth flow, pricing page
- One-click deploy to the platform
- Iterative: "add a feature that..." → AI updates the app

### Phase 4: Ecosystem

- **@zeroship/email** — transactional email (via Resend/Sendgrid)
- **@zeroship/payments** — Stripe wrapper for subscriptions
- **@zeroship/ai** — LLM inference wrapper
- **Marketplace** — discover and install published apps

## Specs

- `docs/specs/api-design-guidelines.md` — 10 principles for AI-friendly APIs
- `docs/specs/db.md` — @zeroship/db: createDb, schema builder, CRUD, aggregation, naming strategy
- `docs/specs/auth.md` — @zeroship/auth: platform-managed auth, gateway JWT, OAuth, consent
- `docs/specs/billing-metering.md` — Meter trait, 25+ metrics, pricing, spending limits
- `docs/specs/appbundle-format.md` — binary bundle format
- `docs/specs/websocket-design.md` — WebSocketPair, RFC 6455

## Development

```bash
# Build
cargo build --release

# Run single-tenant (dev)
zeroship serve myapp.js --port 3000

# Run platform (production)
zeroship-control --port 9090 --db postgres://... --bundles ./bundles --auth-secret <secret>
zeroship-worker --port 8080 --workers 16 --control http://localhost:9090
zeroship-gate --port 80 --control http://localhost:9090 --workers http://localhost:8080 --auth-secret <secret>

# Deploy an app
zeroship deploy ./src --app=<uuid> --control=http://localhost:9090 --key=<master-key>

# Docker Compose (multi-node)
docker compose up -d --scale worker=10

# Tests
./tests/e2e_platform.sh       # 20 E2E tests
./tests/bench_platform.sh     # performance benchmarks
cargo test -p zeroship-pg -- --test-threads=1  # 26 Postgres driver tests
cargo test -p zeroship-core    # 22 core tests
cargo test -p zeroship-bundle  # 10 bundle tests
```

## Revenue model

```
Creator's app earns $100/mo from subscribers:
  Stripe fees:    -$3.20
  Platform (15%): -$15.00
  Creator keeps:  $81.80

Infrastructure cost per app: ~$0.12/mo (98% gross margin)
```

Platform only earns when creators earn. Aligned incentives.
