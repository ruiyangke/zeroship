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

Three components, zero tokio, everything on compio/io_uring:

```
zeroship-control    Stateful control plane (Postgres, VFS, admin API)
zeroship-gate       Smart gateway (auth, rate limit, CHWBL routing, proxy)
zeroship-worker     Stateless compute (V8 isolates, on-demand loading, LRU)
```

```
Users → gate (auth, enforce, route) → worker (V8 compute)
Admin/Creator → control (API, DB, VFS)
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
├── bundle/         .appbundle binary format (read, write, lazy decompress)
├── core/           Shared types (AppRecord, RouteEntry, UsageReport), VFS, auth
├── pg/             PostgreSQL driver (compio-native, 26 integration tests)
├── runtime/        V8 engine + compio event loop + fetch + WebSocket + crypto
├── runtime-macros/ #[zeroship_op] proc macro
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
- CLI: zeroship build, serve, deploy, inspect
- E2E tests (20/20 passing), benchmarks (354K req/s pipeline)
- Zero tokio in the entire stack

## SDK Architecture

Two layers: native primitives (Rust kernel) and npm packages (JS ecosystem).

### Native primitives (`zeroship.*` global)

Registered by Rust on every V8 isolate. The "syscalls" of the platform — stable, secure, low-level:

```
zeroship.db.*       — structured database operations (no raw SQL)
zeroship.auth.*     — password hashing, JWT sign/verify
zeroship.storage.*  — object storage put/get/delete
zeroship.kv.*       — key-value get/set/delete
zeroship.meter.*    — billing counter increment
```

Creators don't call these directly. SDK packages wrap them.

### SDK packages (`@zeroship/*` npm scope)

High-level APIs published as standard npm packages. Creators install and import them:

```javascript
import { model, t } from "@zeroship/db";
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

### Phase 2: Creator platform (the product)

- **@zeroship/db** — per-app database, model builder, document API, auto-migration
- **@zeroship/auth** — per-app user accounts, signUp/signIn/verify, JWT sessions
- **@zeroship/storage** — per-app file storage, put/get/delete
- **Stripe Connect** — creators connect Stripe, end users subscribe, revenue splits
- **Creator dashboard** — web UI for apps, pricing, revenue, analytics
- **Custom domains** — {app-name}.zeroship.dev + creator's own domain

### Phase 3: AI app builder (the differentiator)

- Creator describes the app in natural language
- AI generates: app code, database schema, auth flow, pricing page
- One-click deploy to the platform
- Iterative: "add a feature that..." → AI updates the app

### Phase 4: Ecosystem

- **@zeroship/kv** — sessions, cache, feature flags
- **@zeroship/email** — transactional email (via Resend/Sendgrid)
- **@zeroship/payments** — Stripe wrapper for subscriptions
- **@zeroship/ai** — LLM inference wrapper
- **Marketplace** — discover and install published apps

## Specs

- `docs/specs/api-design-guidelines.md` — 10 principles for AI-friendly APIs
- `docs/specs/db.md` — @zeroship/db: model builder, document API, aggregation
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
zeroship-control --port 9090 --db postgres://... --bundles ./bundles
zeroship-worker --port 8080 --workers 16 --control http://localhost:9090
zeroship-gate --port 80 --control http://localhost:9090 --workers http://localhost:8080

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
