# Architecture overview

zeroship is two systems that share infrastructure. Reading this gives you the mental model that the rest of `docs/architecture/` and the per-crate code assumes.

## The two systems

```
┌─────────────────────────── System 1: Creator Platform ────────────────────────────┐
│                                                                                    │
│   Creator Dashboard ─────► Control Plane ─────► PostgreSQL                         │
│   (web UI)                 (CRUD, deploy,        (control + per-app schemas)       │
│                             billing, env)                                          │
│                                  │                                                 │
│                                  ▼                                                 │
│                            Object Storage                                          │
│                            (.appbundle + assets)                                   │
└────────────────────────────────────────────────────────────────────────────────────┘
                                  │
                                  │ deploys flow down
                                  ▼
┌─────────────────────────── System 2: App Runtime ─────────────────────────────────┐
│                                                                                    │
│   End Users ─► Gateway ─► Worker (V8 isolate per app)                              │
│                  │             │                                                   │
│                  │             ├─► zeroship.db.*       ──► PostgreSQL              │
│                  │             ├─► zeroship.storage.*  ──► Object Storage          │
│                  │             ├─► zeroship.kv.*       ──► Redis                   │
│                  │             ├─► zeroship.auth.*     ──► (header from gateway)   │
│                  │             └─► zeroship.meter.*    ──► Control Plane (usage)   │
│                  │                                                                 │
│                  └────────────► Auth Service (cookie-based JWT)                    │
└────────────────────────────────────────────────────────────────────────────────────┘
```

The systems are physically separate (different binaries, different processes, different concerns) but logically connected: a deploy flows from creator → control plane → object storage; the runtime polls control every 5s for route changes.

## Concerns by component

| Component | Crate | Single-line responsibility |
| --- | --- | --- |
| Control plane | `crates/control` | App CRUD · deploy · env/secrets · billing · route registry · auth service |
| Gateway | `crates/gateway` | Manifest dispatch · JWT validation · rate limit · CHWBL routing · asset proxy |
| Worker | `crates/worker` | V8-per-thread · on-demand bundle loading · LRU isolate eviction · usage reporting |
| Runtime (lib) | `crates/runtime` | V8 + compio event loop · fetch · WebSocket · streams · WebCrypto · auth context |
| Bundle | `crates/bundle` | `.appbundle` binary format (read, write, lazy decompress) |
| Compiler | `crates/compiler` | SWC + esbuild → `.appbundle` |
| Plugins (DB/KV/Storage) | `crates/plugin-{db,kv,storage}` | `zeroship.{db,kv,storage}.*` native ops |
| Postgres driver | `crates/compio-postgres` | compio-native PG, replaces sqlx, eliminates tokio |
| Redis driver | `crates/compio-redis` | compio-native Redis, cluster-aware |
| Core | `crates/core` | Shared types · typed_id · VFS · `Manifest` schema · auth utilities |
| CLI | `crates/cli` | `zeroship build · serve · deploy · inspect` |

## Request paths

### End-user request to an app

```
1. End user → Gateway (e.g. POST /api/orders to myapp.zeroship.ai)
2. Gateway:  extract app_name from subdomain → lookup_by_name → CompiledRoute
             validate JWT cookie if present → inject ZeroShip-User header
             check rate limit + concurrency
             walk compiled manifest:
               - matched a static rule? → fetch bytes from control plane → respond
               - matched a worker rule? → forward via CHWBL hash ring → worker
               - matched a redirect/rewrite? → respond / re-walk
3. Worker:   route to the V8 isolate for this app_id (LRU-cached)
             call default.fetch(req, env, ctx)
             return response (streaming OK)
4. Gateway → end user
```

### Creator deploys an app

```
1. Creator → CLI: `zeroship deploy ./src --app=<id> --key=<master>`
2. CLI:     SWC + esbuild bundle → .appbundle on disk
3. CLI →    Control plane: POST /api/apps/<id>/deploy (16 MB body cap)
4. Control: write .appbundle to object storage
            UPDATE apps SET deploy_hash = <hash>
            (manifest_json column written here when the build emits a manifest)
5. Worker:  next 5s sync sees the new deploy_hash → reload bundle → swap V8 isolate
6. Gateway: next 5s sync sees the new manifest → re-compile → swap CompiledRoute
```

## Why these choices

| Decision | Why |
| --- | --- |
| **Zero tokio** | One scheduler. compio/io_uring is faster on Linux and avoids two competing executors fighting over the worker thread. Drivers are bespoke (`compio-postgres`, `compio-redis`). |
| **V8 per thread, one isolate per app** | Multi-tenancy without process-per-app overhead. LRU evicts cold apps; `enter_isolate`/`exit_isolate` lets many isolates live on one thread without leaking V8's TLS state. |
| **CHWBL routing** | Consistent hashing with bounded loads (xxh3, 150 vnodes per worker) — even distribution + minimal churn when workers are added/removed. |
| **Manifest dispatch** | Every app's routing logic is data, not code. AI generators emit a `Manifest`. The gateway walks it. See `docs/architecture/gateway-routing.md`. |
| **Native primitives + npm SDK** | Native surface is small and forever; npm packages evolve independently. ~90% of new features stay in JS (`@zeroship/email`, `@zeroship/payments`). |
| **typed_id** | UUIDv7 + base62 + entity prefix (`usr_…`, `app_…`). Sortable by creation time, URL-safe, self-describing. |

## What's NOT in this document

- The `.appbundle` byte layout → `docs/reference/appbundle-format.md`
- The `Manifest` JSON shape → `crates/core/src/types.rs` and `docs/architecture/gateway-routing.md`
- Auth flows (creator login, end-user OAuth, JWT cookie semantics) → `docs/reference/auth.md`
- Billing wiring (Stripe Connect, usage metering) → `docs/reference/billing-metering.md`
- Bench setup and what we measure → `docs/reference/zerobench.md`
