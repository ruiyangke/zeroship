# appbase

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

Three components, zero tokio, everything on compio/io_uring:

```
appbase-control    Stateful control plane (Postgres, VFS, admin API)
appbase-gate       Smart gateway (auth, rate limit, CHWBL routing, proxy)
appbase-worker     Stateless compute (V8 isolates, on-demand loading, LRU)
```

```
Users → gate (auth, enforce, route) → worker (V8 compute)
Admin/Creator → control (API, DB, VFS)
```

### Key technical decisions

- **Runtime**: V8 + compio/io_uring (394K req/s raw, 354K through full pipeline)
- **HTTP framework**: ntex + compio (matches raw httparse at scale)
- **Database**: appbase-pg (compio-native PostgreSQL driver, no tokio)
- **Routing**: CHWBL (Consistent Hashing with Bounded Loads, XXH3, 150 vnodes)
- **Gateway → worker**: HTTP/1.1 keep-alive connection pool + Unix domain socket support
- **Bundle format**: .appbundle (APPB magic, zstd compression, SHA-256 integrity, lazy decompression)
- **Worker dispatch**: V8 per thread (Option B), on-demand bundle loading, LRU eviction
- **Sync**: HTTP pull every 5s (worker polls control for version changes)

## Crates

```
crates/
├── bundle/         .appbundle binary format (read, write, lazy decompress)
├── core/           Shared types (AppRecord, RouteEntry, UsageReport), VFS, auth
├── pg/             PostgreSQL driver (compio-native, 26 integration tests)
├── runtime/        V8 engine + compio event loop + fetch + WebSocket + crypto
├── runtime-macros/ #[appbase_op] proc macro
├── compiler/       SWC + esbuild → .appbundle
├── control/        Control plane binary (ntex + compio + Postgres)
├── gateway/        Gateway binary (ntex + compio, CHWBL, connection pool)
├── worker/         Worker binary (ntex + compio, V8 per thread)
└── cli/            CLI: build, serve, deploy, inspect
```

## What's built

- V8 runtime: fetch, WebSocket (RFC 6455), WebCrypto, streams, .appbundle
- 3-component platform: control + gateway + worker
- PostgreSQL driver (compio-native, replaces sqlx, eliminates tokio)
- CHWBL routing with XXH3, connection pool, UDS support
- On-demand V8 loading, LRU eviction, V8 isolate enter/exit for multi-app
- Docker Compose deployment (tested at 50 workers)
- CLI: appbase build, serve, deploy, inspect
- E2E tests (20/20 passing), benchmarks (354K req/s pipeline)
- Zero tokio in the entire stack

## What's next

### Phase 2: Creator platform (the product)

Per-app services that let creators ship apps without managing infrastructure:

- **Per-app auth** — app_users table, signUp/signIn/verify exposed to V8
- **Per-app database** — Postgres schema isolation, db.query()/db.execute() in V8
- **Per-app storage** — S3-compatible object storage, storage.put()/get() in V8
- **Stripe Connect** — creators connect their Stripe, end users subscribe, revenue splits automatically
- **Creator dashboard** — web UI for managing apps, pricing, revenue, analytics
- **Custom domains** — {app-name}.appbase.dev + creator's own domain

### Phase 3: AI app builder (the differentiator)

- Creator describes the app in natural language
- AI generates: V8 app code, database schema, auth flow, pricing page
- One-click deploy to the platform
- Iterative: "add a feature that..." → AI updates the app

### Phase 4: Ecosystem

- **KV Store** — sessions, cache, feature flags (fast, per-app)
- **Message Queue** — async jobs, webhooks, event processing
- **Cron** — scheduled tasks
- **Marketplace** — discover and install published apps

## Specs

- `docs/specs/appbundle-format.md` — binary bundle format
- `docs/specs/billing-metering.md` — Meter trait, 25+ metrics, pricing, spending limits
- `docs/specs/websocket-design.md` — WebSocketPair, RFC 6455

## Development

```bash
# Build
cargo build --release

# Run single-tenant (dev)
appbase serve myapp.js --port 3000

# Run platform (production)
appbase-control --port 9090 --db postgres://... --bundles ./bundles
appbase-worker --port 8080 --workers 16 --control http://localhost:9090
appbase-gate --port 80 --control http://localhost:9090 --workers http://localhost:8080

# Deploy an app
appbase deploy ./src --app=<uuid> --control=http://localhost:9090 --key=<master-key>

# Docker Compose (multi-node)
docker compose up -d --scale worker=10

# Tests
./tests/e2e_platform.sh       # 20 E2E tests
./tests/bench_platform.sh     # performance benchmarks
cargo test -p appbase-pg -- --test-threads=1  # 26 Postgres driver tests
cargo test -p appbase-core    # 22 core tests
cargo test -p appbase-bundle  # 10 bundle tests
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
