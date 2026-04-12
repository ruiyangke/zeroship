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

## SDK Architecture

Two layers: native primitives (Rust kernel) and npm packages (JS ecosystem).

### Native primitives (`appbase.*` global)

Registered by Rust on every V8 isolate. The "syscalls" of the platform — stable, secure, low-level:

```
appbase.db.*       — structured database operations (no raw SQL)
appbase.auth.*     — password hashing, JWT sign/verify
appbase.storage.*  — object storage put/get/delete
appbase.kv.*       — key-value get/set/delete
appbase.meter.*    — billing counter increment
```

Creators don't call these directly. SDK packages wrap them.

### SDK packages (`@appbase/*` npm scope)

High-level APIs published as standard npm packages. Creators install and import them:

```javascript
import { model, t } from "@appbase/db";
import { auth } from "@appbase/auth";
import { storage } from "@appbase/storage";
import { kv } from "@appbase/kv";
```

SDK packages call `appbase.*` primitives internally. They handle validation, query building, error mapping, and TypeScript types. They evolve independently of the Rust runtime.

### Extensibility

Features that compose existing primitives are pure JS packages — no Rust needed:

```
Needs Rust (new primitive):        Pure JS (npm package):
  New storage backend               @appbase/email (calls fetch)
  New database engine                @appbase/payments (calls fetch to Stripe)
  Low-level crypto ops               @appbase/ai (calls fetch to OpenAI/Claude)
                                     @appbase/permissions (uses db + auth)
                                     @appbase/analytics (uses db)
                                     Third-party: @cooldev/appbase-redis
```

~90% of new features are pure JS. The native layer is the stable kernel.

## What's next

### Phase 2: Creator platform (the product)

- **@appbase/db** — per-app database, model builder, document API, auto-migration
- **@appbase/auth** — per-app user accounts, signUp/signIn/verify, JWT sessions
- **@appbase/storage** — per-app file storage, put/get/delete
- **Stripe Connect** — creators connect Stripe, end users subscribe, revenue splits
- **Creator dashboard** — web UI for apps, pricing, revenue, analytics
- **Custom domains** — {app-name}.appbase.dev + creator's own domain

### Phase 3: AI app builder (the differentiator)

- Creator describes the app in natural language
- AI generates: app code, database schema, auth flow, pricing page
- One-click deploy to the platform
- Iterative: "add a feature that..." → AI updates the app

### Phase 4: Ecosystem

- **@appbase/kv** — sessions, cache, feature flags
- **@appbase/email** — transactional email (via Resend/Sendgrid)
- **@appbase/payments** — Stripe wrapper for subscriptions
- **@appbase/ai** — LLM inference wrapper
- **Marketplace** — discover and install published apps

## Specs

- `docs/specs/api-design-guidelines.md` — 10 principles for AI-friendly APIs
- `docs/specs/db.md` — @appbase/db: model builder, document API, aggregation
- `docs/specs/billing-metering.md` — Meter trait, 25+ metrics, pricing, spending limits
- `docs/specs/appbundle-format.md` — binary bundle format
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
