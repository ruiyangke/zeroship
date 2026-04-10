# appbase-control / appbase-worker: Control Plane & Data Plane

## Goal

Split the monolith `appbase platform` into:
- **appbase-control**: stateful control plane (Postgres, VFS, admin API, internal API for workers)
- **appbase-worker**: stateless data plane (V8 isolates, user traffic, no DB)

Workers sync via HTTP pull + WebSocket hints. Bundle storage abstracted via VFS (local filesystem or S3).

## Key Decisions (validated by benchmarks and stress tests)

| Decision | Choice | Evidence |
|---|---|---|
| HTTP framework | **ntex + compio** | Matches raw httparse at scale (191K vs 196K req/s at 16 cores), gives routing/middleware/HTTP2/WS for free |
| V8 dispatch model | **Option B: V8 on HTTP thread** | 38% faster than flume v8pool at 16 cores (191K vs 139K req/s), matches workerd model |
| Database | **appbase-pg (compio-native)** | 23 integration tests passing, no tokio dependency |
| Bundle storage | **VFS: tenant-isolated by app_id, integrity via SHA-256** | deploy_hash for change detection only, not as storage key |
| Sync mechanism | **HTTP pull (5s) + WebSocket hints** | Self-healing, validated by Cloudflare/Lambda/K8s patterns |
| App identity | **UUID app_id + human-readable name** | deploy_hash (SHA-256) for change detection |
| Naming | **appbase-control / appbase-worker** | Descriptive, K8s-style |

## Architecture

```
                         ┌──────────────────────────────┐
                         │      appbase-control          │
                         │     (ntex + compio, 1 inst)   │
                         │                               │
   Admin/CLI ───────────→│  Public:  /api/apps/*         │
                         │  Internal:/internal/versions  │
                         │          /internal/bundles/*  │
                         │          /internal/usage      │
                         │          /internal/events (WS)│
                         │                               │
                         │  Postgres (appbase-pg)        │
                         │  VFS (LocalFs or S3)          │
                         └──────────┬───────────────────┘
                                    │
                    ┌───────────────┼───────────────┐
                    │               │               │
                    ▼               ▼               ▼
          ┌──────────────┐ ┌──────────────┐ ┌──────────────┐
          │appbase-worker│ │appbase-worker│ │appbase-worker│
          │(ntex+compio) │ │(ntex+compio) │ │(ntex+compio) │
          │              │ │              │ │              │
 Users ──→│ V8 per thread│ │ V8 per thread│ │ V8 per thread│
          │ No flume     │ │ No flume     │ │ No flume     │
          │ No Postgres  │ │ No Postgres  │ │ No Postgres  │
          └──────────────┘ └──────────────┘ └──────────────┘
```

## Worker Thread Model (Option B)

Each ntex worker thread owns a thread-local `HashMap<UUID, Runtime>`. Only one isolate is active at a time per thread (V8 requires LIFO enter/exit order — validated by stress test).

```
ntex worker thread 1:              ntex worker thread 2:
  compio event loop                  compio event loop
  ├─ HTTP accept + parse             ├─ HTTP accept + parse
  ├─ apps: HashMap<UUID, Runtime>    ├─ apps: HashMap<UUID, Runtime>
  │   ├─ app-A → V8 Runtime         │   ├─ app-A → V8 Runtime
  │   ├─ app-B → V8 Runtime         │   ├─ app-C → V8 Runtime
  │   └─ LRU eviction               │   └─ LRU eviction
  └─ pump task (timers, fetch)       └─ pump task (timers, fetch)

  Request flow (same thread, no channel):
    HTTP parse → lookup app → dispatch_rpc → respond
```

### Capacity (validated)

| Metric | Value |
|---|---|
| Memory per idle isolate | ~1 MB |
| 1,000 isolates RSS | ~1 GB |
| Isolate creation time | ~1.5 ms each |
| Dispatch latency | ~2.3 ms each (cold, first call) |
| Max isolates (16 GB server) | ~10,000 (with 6 GB headroom) |

Tested: 1,000 isolates created and dispatched successfully in a single process.

### V8 constraint

V8 `OwnedIsolate` instances must be entered/exited in LIFO (stack) order. In Option B, only one isolate is active per thread at any time — `dispatch_rpc` enters V8, executes, exits, then the next request can use a different isolate. No concurrent V8 entry on the same thread.

## Database Schema (Postgres)

```sql
CREATE TABLE apps (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name         TEXT NOT NULL UNIQUE,
    plan_id      TEXT NOT NULL DEFAULT 'free',
    deploy_hash  TEXT,
    api_key      TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE usage (
    app_id       UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    resource     TEXT NOT NULL,
    value        BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (app_id, resource)
);

CREATE TABLE usage_history (
    app_id       UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
    period       TEXT NOT NULL,
    counters     JSONB NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
CREATE INDEX idx_usage_history_app ON usage_history(app_id, period);
```

- `id`: UUID, auto-generated
- `name`: human-readable, unique (e.g. "myapp")
- `deploy_hash`: SHA-256 hex of the live .appbundle, NULL if never deployed
- Code lives in VFS, not Postgres

## VFS — Bundle Storage

```rust
pub trait BundleStore: Send + Sync {
    async fn put(&self, app_id: &str, data: &[u8]) -> Result<()>;
    async fn get(&self, app_id: &str) -> Result<Vec<u8>>;
    async fn delete(&self, app_id: &str) -> Result<()>;
    async fn exists(&self, app_id: &str) -> Result<bool>;
}
```

**LocalFs**: `./bundles/{app_id}/bundle.appbundle` — single-server, dev, testing.

**S3**: `s3://bucket/{app_id}/bundle.appbundle` — multi-server production.

Storage is tenant-isolated by app_id path. deploy_hash is for change detection and integrity verification only.

## Sync Protocol

### Version polling (every 5s)

```
Worker → GET /internal/versions
         Auth: Bearer <worker-key>
→ { "app-uuid-1": "sha256hex", "app-uuid-2": "sha256hex" }

Worker compares with local cache:
  - Hash matches → skip
  - Hash differs → GET /internal/bundles/{app_id} → reload isolate
  - New app → pull + load
  - Missing app → evict isolate
```

### WebSocket hints (instant deploys)

```
Worker → WS /internal/events?key=<worker-key>
Server pushes: { "type": "deploy", "app_id": "uuid", "hash": "sha256" }
               { "type": "delete", "app_id": "uuid" }
```

Push is optimization. Pull is source of truth. If WS disconnects, 5s poll catches up.

### Usage reporting (every 10s)

```
Worker → POST /internal/usage
         Auth: Bearer <worker-key>
{ "worker_id": "w1", "counters": { "app-uuid": { "requests": 142, "cpu_us": 50000 } } }
```

### Authentication

Shared secret via `--control-key=<secret>` or `APPBASE_CONTROL_KEY` env var.
All /internal/* requests require `Authorization: Bearer <key>`.

## Admin API (appbase-control)

```
POST   /api/apps                 Create app { name, plan_id }
GET    /api/apps                 List apps
GET    /api/apps/:id             Get app details
DELETE /api/apps/:id             Delete app + bundle
POST   /api/apps/:id/deploy     Deploy .appbundle (multipart upload)
PUT    /api/apps/:id/plan        Set plan { plan_id }
GET    /api/apps/:id/usage       Get usage counters
POST   /api/apps/:id/rollback   Rollback to previous deploy_hash
```

Authenticated via per-app API key or master key.

## Request Flow (appbase-worker)

```
User request → ntex worker thread:
  │
  ├─ Parse HTTP
  ├─ Extract app_id from path: /apps/{app_id}/rpc
  │
  ├─ App in thread-local HashMap?
  │   ├─ Yes → rate limit (atomic) → dispatch_rpc → respond
  │   └─ No  → GET /internal/bundles/{app_id}
  │            → verify sha256 == expected hash
  │            → create Runtime → dispatch → respond
  │
  ├─ Record usage (atomic in-memory)
  └─ Background: flush to control plane every 10s
```

## Deploy Flow

```
1. CLI:     appbase deploy myapp/ --control=https://control.example.com
2. CLI:     Compile JS → .appbundle
3. CLI:     POST /api/apps/myapp/deploy (multipart)
4. Control: hash = sha256(appbundle)
5. Control: vfs.put(app_id, bytes)
6. Control: UPDATE apps SET deploy_hash = $1 WHERE name = $2
7. Control: Push WS event { type: "deploy", app_id, hash }
8. Worker:  Receives hint (or polls within 5s)
9. Worker:  GET /internal/bundles/{app_id}
10. Worker: Verify sha256 == hash
11. Worker: Load into thread-local Runtime
```

## Crate Structure

```
crates/
├── runtime/          V8 engine (unchanged)
├── runtime-macros/   Proc macros (unchanged)
├── pg/               PostgreSQL driver (compio-native, built)
├── compiler/         SWC + esbuild (unchanged)
├── common/           NEW — shared types + VFS
│   ├── lib.rs
│   ├── types.rs      AppRecord, VersionMap, UsageReport
│   ├── vfs.rs        BundleStore trait + LocalFs + S3
│   └── auth.rs       Worker key validation
├── control/          NEW — appbase-control binary
│   ├── main.rs       Entry point (ntex + compio)
│   ├── api.rs        Public admin API routes
│   ├── internal.rs   Internal API for workers
│   ├── registry.rs   App CRUD (uses appbase-pg)
│   └── metering.rs   Usage aggregation from workers
├── worker/           NEW — appbase-worker binary
│   ├── main.rs       Entry point (ntex + compio)
│   ├── sync.rs       Poll loop + WebSocket hint channel
│   ├── handler.rs    Request dispatch (V8 per thread)
│   └── cache.rs      Thread-local app cache + LRU eviction
├── platform/         DEPRECATED — replaced by control + worker
└── cli/
    ├── appbase serve      Single-tenant, raw httparse (unchanged)
    ├── appbase control    Start control plane
    ├── appbase worker     Start data plane worker
    ├── appbase deploy     Deploy to control plane
    └── appbase build      Compile only (unchanged)
```

### Dependencies

```
control → common, pg, ntex, serde, uuid
worker  → common, runtime, ntex, serde
common  → serde (no runtime dependency)
```

No circular dependencies. Worker never imports pg. Control never imports runtime.

### CLI flags

**appbase control:**
```
--port=3000              HTTP listen port
--db=postgres://...      Postgres connection URL
--bundles=./bundles      VFS path (local) or s3://bucket (S3)
--master-key=<secret>    Admin master key
--control-key=<secret>   Shared secret for worker auth
```

**appbase worker:**
```
--port=8080              HTTP listen port
--workers=16             ntex worker threads (default: num_cpus)
--control=http://...     Control plane URL
--control-key=<secret>   Shared secret for control plane auth
--max-isolates=200       Max V8 isolates per worker thread (LRU eviction)
--poll-interval=5        Sync poll interval in seconds
```

## Migration from platform crate

1. Extract shared types → `crates/common/`
2. Move AppRegistry + metering → `crates/control/`
3. Move V8Pool + enforcement → `crates/worker/` (rewrite to Option B)
4. Remove flume dependency from worker
5. Remove sqlx, tokio, axum from workspace
6. Deprecate `crates/platform/`

The enforcement code (rate limit, concurrency, quota) moves to worker unchanged — it's already lock-free atomics with no runtime dependency.

## Benchmark Results

### ntex + compio vs raw httparse (with V8, 16 cores)

| Endpoint | raw httparse | ntex + compio | Diff |
|---|---|---|---|
| RPC ping | 196,207 | 203,621 | +4% |
| HTTP handler | 189,524 | 202,178 | +7% |
| health | 206,235 | 198,484 | -4% |

### Option B vs flume v8pool (ntex + compio, 16 cores)

| Endpoint | Option B (direct) | flume v8pool | Diff |
|---|---|---|---|
| RPC ping | 191,186 | 138,574 | **+38%** |

### Isolate stress test (single process)

| Count | RSS | Creation time |
|---|---|---|
| 100 | 116 MB | 164 ms |
| 500 | 521 MB | 783 ms |
| 1,000 | 1,028 MB | 1.55 s |

## Security

- /internal/* requires worker key (shared secret, Bearer token)
- Bundle storage tenant-isolated by app_id path
- deploy_hash is for integrity, not access control
- S3 buckets private; access via pre-signed URL or control plane proxy
- Admin API requires per-app API key or master key
- V8 isolates are process-isolated per worker (no shared memory between apps)

## Scope Exclusions (v1)

- Custom domains (Host header routing) — v2
- Geographic routing / multi-region — v2
- Canary deploys / traffic splitting — v2
- Auto-scaling workers — external orchestrator (K8s/Nomad)
- Bundle garbage collection — manual for v1
- Encrypted bundles at rest — v2
- HTTP/2 (ntex supports it, enable with TLS)
- V8 snapshots (faster cold start) — separate effort
