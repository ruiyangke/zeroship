# Control Plane / Data Plane Split

## Goal

Split the monolith `appbase platform` into a stateful control plane (owns Postgres, manages apps/billing) and stateless data plane workers (run V8 isolates, handle user traffic). Workers sync via HTTP pull + WebSocket hints. Bundle storage abstracted via VFS (local filesystem or S3).

## Key Decisions (from benchmarks)

| Decision | Choice | Evidence |
|---|---|---|
| HTTP framework | **ntex + compio** | Matches raw httparse at scale (191K vs 196K req/s), gives routing/middleware/HTTP2/WS for free |
| V8 dispatch model | **Option B: V8 on HTTP thread** | 28% faster than flume v8pool at 16 cores (191K vs 139K), matches workerd |
| Database | **appbase-pg (compio-native)** | 23 integration tests passing, no tokio dependency |
| Bundle storage | **VFS: content-addressed by app_id, integrity via SHA-256** | Tenant-isolated paths, hash for change detection only |
| Sync mechanism | **HTTP pull (5s) + WebSocket hints** | Self-healing, no message loss, validated by Cloudflare/Lambda/K8s patterns |
| App identity | **UUID app_id + human-readable name** | deploy_hash (SHA-256) for change detection |

## Architecture

```
                         ┌──────────────────────────────┐
                         │        Control Plane          │
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
          │ Data Plane 1 │ │ Data Plane 2 │ │ Data Plane N │
          │(ntex+compio) │ │(ntex+compio) │ │(ntex+compio) │
          │              │ │              │ │              │
 Users ──→│ Option B:    │ │ Option B:    │ │ Option B:    │
          │ V8 per thread│ │ V8 per thread│ │ V8 per thread│
          │ No flume     │ │ No flume     │ │ No flume     │
          │ No Postgres  │ │ No Postgres  │ │ No Postgres  │
          └──────────────┘ └──────────────┘ └──────────────┘
```

### Data Plane Worker Thread Model (Option B)

```
ntex worker thread 1:              ntex worker thread 2:
  compio event loop                  compio event loop
  ├─ HTTP accept + parse             ├─ HTTP accept + parse
  ├─ apps: HashMap<UUID, Runtime>    ├─ apps: HashMap<UUID, Runtime>
  │   ├─ "foo" → V8 Runtime         │   ├─ "foo" → V8 Runtime
  │   ├─ "bar" → V8 Runtime         │   ├─ "baz" → V8 Runtime
  │   └─ LRU eviction               │   └─ LRU eviction
  └─ pump task (timers, fetch)       └─ pump task (timers, fetch)
```

No flume channels, no cross-thread dispatch. Each thread independently loads apps from control plane. Same app may exist on multiple threads — kernel distributes connections.

## Database Schema

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
#[async_trait]
pub trait BundleStore: Send + Sync {
    async fn put(&self, app_id: &str, data: &[u8]) -> Result<()>;
    async fn get(&self, app_id: &str) -> Result<Vec<u8>>;
    async fn delete(&self, app_id: &str) -> Result<()>;
    async fn exists(&self, app_id: &str) -> Result<bool>;
}
```

Two implementations:

**LocalFs**: `./bundles/{app_id}/bundle.appbundle` — single-server, dev, testing.

**S3**: `s3://bucket/{app_id}/bundle.appbundle` — multi-server production.

Storage is tenant-isolated by app_id path. The deploy_hash is used only for change detection and integrity verification, not as a storage key.

## Sync Protocol

### Version polling (primary, every 5s)

```
Data plane → GET /internal/versions
             Auth: Bearer <worker-key>

Response: {
    "a3f8c2e1-7b4d-...": "abc123def456...",   // app_id → deploy_hash
    "b4c5d6e7-8a9b-...": "789xyz012345...",
}

Data plane compares with local cache:
  - Hash matches → skip (no change)
  - Hash differs → GET /internal/bundles/{app_id} → reload V8 isolate
  - App in remote, not local → new app → pull + load
  - App in local, not remote → deleted → evict isolate
```

### WebSocket hints (optional, instant deploys)

```
Data plane → WS /internal/events?key=<worker-key>

Server pushes:
  { "type": "deploy",      "app_id": "uuid", "hash": "sha256" }
  { "type": "delete",      "app_id": "uuid" }
  { "type": "plan_change", "app_id": "uuid", "plan": "pro" }
```

Push is a performance optimization. Pull is the source of truth.

### Usage reporting (every 10s)

```
Data plane → POST /internal/usage
             Auth: Bearer <worker-key>
Body: {
    "worker_id": "worker-1",
    "counters": {
        "app_id_uuid": { "requests": 142, "cpu_us": 50000, "egress": 28000 }
    }
}
```

### Authentication

Shared secret: `--control-key=<secret>` or `APPBASE_CONTROL_KEY` env var.
Sent as `Authorization: Bearer <key>` on all /internal/* requests.

## Public Admin API

```
POST   /api/apps                 Create app { name, plan_id }
GET    /api/apps                 List apps
GET    /api/apps/:id             Get app details
DELETE /api/apps/:id             Delete app + bundle
POST   /api/apps/:id/deploy     Deploy { multipart .appbundle }
PUT    /api/apps/:id/plan        Set plan { plan_id }
GET    /api/apps/:id/usage       Get usage counters
POST   /api/apps/:id/rollback   Set deploy_hash to previous value
```

## Data Plane Request Flow

```
User HTTP request arrives at ntex worker thread:
  │
  ├─ Parse HTTP (ntex, compio io_uring)
  ├─ Extract app_id from path: /apps/{app_id}/rpc
  │
  ├─ App in thread-local HashMap?
  │   ├─ Yes → rate limit (atomic) → dispatch_rpc on same thread → respond
  │   └─ No  → GET /internal/bundles/{app_id} from control plane
  │            → verify sha256(bundle) == expected hash
  │            → create V8 Runtime on this thread → dispatch → respond
  │
  ├─ Record usage (atomic in-memory counters)
  └─ Background: flush counters to control plane every 10s
```

## Deploy Flow

```
1. CLI:     appbase deploy myapp/ --control=https://control.example.com
2. CLI:     Compile JS → .appbundle (SWC + esbuild + zstd)
3. CLI:     POST /api/apps/myapp/deploy (multipart: .appbundle)
4. Control: hash = sha256(appbundle)
5. Control: bundle_store.put(app_id, bytes)
6. Control: UPDATE apps SET deploy_hash = $1, updated_at = NOW() WHERE name = $2
7. Control: Push WS event { type: "deploy", app_id, hash }
8. Worker:  Receives hint (or polls within 5s)
9. Worker:  GET /internal/bundles/{app_id}
10. Worker: Verify sha256(bytes) == hash
11. Worker: Load into thread-local V8 Runtime, start serving
```

## Crate Structure

```
crates/
├── runtime/          V8 engine (unchanged)
├── runtime-macros/   Proc macros (unchanged)
├── pg/               PostgreSQL driver (compio-native)
├── compiler/         SWC + esbuild (unchanged)
├── common/           Shared types + VFS
│   ├── types.rs      AppRecord, VersionMap, UsageReport
│   ├── vfs.rs        BundleStore trait + LocalFs + S3
│   └── auth.rs       Worker key validation
├── control/          Control plane server (ntex + compio)
│   ├── main.rs       Entry point
│   ├── api.rs        Public admin API
│   ├── internal.rs   Internal API for workers
│   ├── registry.rs   App CRUD (uses appbase-pg)
│   └── metering.rs   Usage aggregation
├── worker/           Data plane server (ntex + compio + Option B)
│   ├── main.rs       Entry point
│   ├── sync.rs       Poll loop + WebSocket hints
│   ├── handler.rs    Request dispatch (V8 per thread, no flume)
│   └── cache.rs      Thread-local app cache + LRU
└── cli/
    ├── appbase serve      Single-tenant, raw httparse (unchanged)
    ├── appbase control    Start control plane
    ├── appbase worker     Start data plane worker
    └── appbase deploy     Deploy to control plane
```

Dependency graph:
```
control → common, pg
worker  → common, runtime
common  → (no runtime deps)
```

No circular dependencies. Worker never imports pg. Control never imports runtime.

## Benchmark Results

### ntex + compio vs raw httparse (with V8, 16 cores)

| Endpoint | raw httparse | ntex + compio | Diff |
|---|---|---|---|
| RPC ping | 196,207 | 203,621 | +4% |
| HTTP handler | 189,524 | 202,178 | +7% |
| health | 206,235 | 198,484 | -4% |

### Option B vs flume v8pool (ntex + compio, 16 cores)

| Endpoint | Option B (direct) | flume (1 V8 thread) | Diff |
|---|---|---|---|
| RPC ping | 191,186 | 138,574 | **+38%** |

## Security

- Internal API requires worker key (shared secret)
- Bundle storage is tenant-isolated by app_id path
- deploy_hash is for change detection + integrity, not access control
- S3 buckets are private; access via pre-signed URLs or control plane proxy
- Admin API requires per-app API key or master key

## Scope Exclusions (v1)

- Custom domains (Host header routing)
- Geographic routing / multi-region
- Canary deploys / traffic splitting
- Auto-scaling data plane workers
- Bundle garbage collection
- Encrypted bundles at rest
- HTTP/2 (ntex supports it, enable when needed with TLS)
