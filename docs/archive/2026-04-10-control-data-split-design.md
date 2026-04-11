# appbase-control / appbase-gate / appbase-worker

## Goal

Split the monolith `appbase platform` into three components:
- **appbase-control**: stateful control plane (Postgres, VFS, admin API, internal API)
- **appbase-gate**: smart gateway (auth, rate limit, quota, route → worker)
- **appbase-worker**: stateless compute (V8 isolates only, no auth, no DB)

```
Users → gate (auth, enforce, route) → worker (V8 compute)
Admin → control (API, DB, VFS)
gate ↔ control (routing table sync)
worker ↔ control (bundle sync, usage reporting)
```

## Key Decisions (validated by benchmarks and stress tests)

| Decision | Choice | Evidence |
|---|---|---|
| HTTP framework | **ntex + compio** | Matches raw httparse at scale (191K vs 196K req/s at 16 cores) |
| V8 dispatch | **Option B: V8 on HTTP thread** | 38% faster than flume v8pool (191K vs 139K req/s) |
| Database | **appbase-pg (compio-native)** | 23 tests passing, no tokio |
| Bundle storage | **VFS: tenant-isolated by app_id** | deploy_hash for change detection |
| Sync | **HTTP pull (5s) + WebSocket hints** | Self-healing, validated by CF/Lambda/K8s |
| App identity | **UUID + human name** | deploy_hash (SHA-256) |

## Architecture

```
                         ┌──────────────────────────────┐
                         │      appbase-control          │
                         │     (ntex + compio, 1 inst)   │
                         │                               │
   Admin/CLI ───────────→│  /api/apps/*                  │
                         │  /internal/versions           │
                         │  /internal/bundles/*          │
                         │  /internal/routes    (NEW)    │
                         │  /internal/usage              │
                         │  /internal/events (WS)        │
                         │                               │
                         │  Postgres (appbase-pg)        │
                         │  VFS (LocalFs or S3)          │
                         └──────────┬───────────────────┘
                                    │
                         ┌──────────┴───────────────────┐
                         │                              │
                         ▼                              ▼
          ┌──────────────────────┐        ┌──────────────────────┐
          │    appbase-gate      │        │   appbase-worker ×N  │
          │   (ntex + compio)    │        │   (ntex + compio)    │
          │                      │        │                      │
 Users ──→│ 1. Resolve app_id   │        │  V8 per thread       │
          │ 2. Auth (API key)   │  HTTP  │  No auth             │
          │ 3. Rate limit       │───────→│  No rate limit       │
          │ 4. Concurrency      │        │  No DB               │
          │ 5. Quota check      │        │  Pure compute        │
          │ 6. Proxy → worker   │        │                      │
          └──────────────────────┘        └──────────────────────┘
```

### Component Responsibilities

| Responsibility | control | gate | worker |
|---|---|---|---|
| Postgres | Yes | No | No |
| VFS (bundles) | Yes | No | No |
| Admin API | Yes | No | No |
| App routing | No | **Yes** | No |
| API key auth | No | **Yes** | No |
| Rate limiting | No | **Yes** (global) | No |
| Concurrency guard | No | **Yes** | No |
| Quota check | No | **Yes** | No |
| HTTP proxy | No | **Yes** | No |
| V8 execution | No | No | **Yes** |
| Bundle sync | No | No | **Yes** |
| Usage recording | No | No | **Yes** |

## Gateway (appbase-gate)

### Sync with control plane

Gateway polls control plane for a routing table — app metadata needed for auth and enforcement:

```
Gate → GET /internal/routes
       Auth: Bearer <control-key>
→ {
    "a3f8c2e1-...": {
      "name": "myapp",
      "plan_id": "pro",
      "api_key_hash": "sha256...",
      "deploy_hash": "abc123..."
    },
    ...
  }
```

Polled every 5s. Cached in-memory as `HashMap<UUID, RouteEntry>`. Also indexed by name for path/host lookup.

### Request flow

```
User: POST /apps/myapp/rpc
  │
  ├─ 1. Parse HTTP (ntex)
  ├─ 2. Extract app name from path → lookup route entry
  │     404 if app not found
  ├─ 3. Auth: validate API key from X-Api-Key header
  │     401 if invalid/missing
  ├─ 4. Rate limit: token bucket per app (lock-free CAS)
  │     429 if exceeded
  ├─ 5. Concurrency: CAS counter per app
  │     429 if exceeded
  ├─ 6. Quota: check plan limits
  │     429 if exceeded
  ├─ 7. Proxy to worker:
  │     POST http://worker:8080/dispatch/{app_id}
  │     Headers: X-App-Id, X-Plan-Id, X-Request-Id
  │     Body: original request body
  ├─ 8. Forward worker response to user
  │     Add headers: X-Cpu-Time, X-Wall-Time, RateLimit-*
  └─ 9. Release concurrency guard (RAII drop)
```

### Worker selection

For v1: round-robin or random across configured worker addresses. No sticky sessions.

```
--workers=http://w1:8080,http://w2:8080,http://w3:8080
```

Future: health-check aware, least-connections, sticky by app_id.

## Worker (appbase-worker)

### Internal dispatch API

Worker exposes a single internal endpoint. Gateway is the only caller.

```
POST /dispatch/{app_id}
  Headers:
    X-App-Id: uuid
    X-Plan-Id: pro
    X-Request-Id: uuid
  Body: raw request body (JSON-RPC or HTTP)
  
  → V8 dispatch_rpc → response
```

No auth on this endpoint — worker trusts gateway. Worker should only be reachable from gateway (private network / firewall).

### Thread model (Option B, unchanged)

```
ntex worker thread 1:              ntex worker thread 2:
  compio event loop                  compio event loop
  ├─ apps: HashMap<UUID, Runtime>    ├─ apps: HashMap<UUID, Runtime>
  │   ├─ app-A → V8 Runtime         │   ├─ app-A → V8 Runtime
  │   ├─ app-B → V8 Runtime         │   ├─ app-C → V8 Runtime
  │   └─ LRU eviction               │   └─ LRU eviction
  └─ pump task (timers, fetch)       └─ pump task (timers, fetch)
```

### Bundle sync (unchanged)

```
Worker → GET /internal/versions → compare hashes → pull changed bundles
Worker → WS /internal/events → instant deploy hints
```

### Usage reporting (unchanged)

```
Worker → POST /internal/usage (every 10s)
{ "worker_id": "w1", "counters": { "app-uuid": { "requests": 142, "cpu_us": 50000 } } }
```

### Capacity (validated)

| Metric | Value |
|---|---|
| Memory per idle isolate | ~1 MB |
| 1,000 isolates RSS | ~1 GB |
| Isolate creation time | ~1.5 ms each |
| Max isolates (16 GB server) | ~10,000 |

## Control Plane (appbase-control)

### Admin API (public)

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

### Internal API (for gate + worker)

```
GET  /internal/versions          → { app_id → deploy_hash }          (worker)
GET  /internal/bundles/{app_id}  → raw .appbundle bytes              (worker)
GET  /internal/routes            → { app_id → { name, plan, key } }  (gate)
POST /internal/usage             ← usage counters from workers       (worker)
WS   /internal/events            ← deploy/delete hints               (gate + worker)
```

All /internal/* authenticated via shared `control-key`.

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

## VFS — Bundle Storage

```rust
pub trait BundleStore: Send + Sync {
    async fn put(&self, app_id: &str, data: &[u8]) -> Result<()>;
    async fn get(&self, app_id: &str) -> Result<Vec<u8>>;
    async fn delete(&self, app_id: &str) -> Result<()>;
    async fn exists(&self, app_id: &str) -> Result<bool>;
}
```

**LocalFs**: `./bundles/{app_id}/bundle.appbundle`
**S3**: `s3://bucket/{app_id}/bundle.appbundle`

Tenant-isolated by app_id path.

## Deploy Flow

```
 1. CLI:     appbase deploy myapp/ --control=https://control.example.com
 2. CLI:     Compile JS → .appbundle
 3. CLI:     POST /api/apps/myapp/deploy (multipart)
 4. Control: hash = sha256(appbundle)
 5. Control: vfs.put(app_id, bytes)
 6. Control: UPDATE apps SET deploy_hash = $1 WHERE name = $2
 7. Control: Push WS event { type: "deploy", app_id, hash }
 8. Gate:    Receives hint → updates routing table (deploy_hash changed)
 9. Worker:  Receives hint (or polls within 5s)
10. Worker:  GET /internal/bundles/{app_id}
11. Worker:  Verify sha256 == hash
12. Worker:  Load into thread-local V8 Runtime
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
│   ├── types.rs      AppRecord, RouteEntry, VersionMap, UsageReport
│   ├── vfs.rs        BundleStore trait + LocalFs + S3
│   └── auth.rs       Control key validation
├── control/          NEW — appbase-control binary
│   ├── main.rs       Entry point (ntex + compio)
│   ├── api.rs        Public admin API routes
│   ├── internal.rs   Internal API for gate + worker
│   ├── registry.rs   App CRUD (uses appbase-pg)
│   └── metering.rs   Usage aggregation from workers
├── gateway/          NEW — appbase-gate binary
│   ├── main.rs       Entry point (ntex + compio)
│   ├── router.rs     Route resolution (path/host → app_id)
│   ├── auth.rs       API key validation (hash compare)
│   ├── enforce.rs    Rate limit + concurrency + quota (lock-free)
│   ├── proxy.rs      HTTP proxy to workers (round-robin)
│   └── sync.rs       Poll control plane for routing table
├── worker/           NEW — appbase-worker binary
│   ├── main.rs       Entry point (ntex + compio)
│   ├── sync.rs       Poll control plane for bundles + WS hints
│   ├── handler.rs    /dispatch/{app_id} → V8 dispatch (per thread)
│   └── cache.rs      Thread-local app cache + LRU eviction
├── platform/         DEPRECATED — replaced by control + gate + worker
└── cli/
    ├── appbase serve      Single-tenant, raw httparse (unchanged)
    ├── appbase control    Start control plane
    ├── appbase gate       Start gateway
    ├── appbase worker     Start data plane worker
    ├── appbase deploy     Deploy to control plane
    └── appbase build      Compile only (unchanged)
```

### Dependencies

```
control → common, pg, ntex, serde, uuid
gateway → common, ntex, cyper (HTTP proxy client), serde
worker  → common, runtime, ntex, serde
common  → serde (no runtime dependency)
```

No circular dependencies. Gateway never imports runtime or pg. Worker never imports pg. Control never imports runtime.

### CLI Flags

**appbase control:**
```
--port=3000              HTTP listen port
--db=postgres://...      Postgres connection URL
--bundles=./bundles      VFS path (local) or s3://bucket
--master-key=<secret>    Admin master key
--control-key=<secret>   Shared secret for gate + worker auth
```

**appbase gate:**
```
--port=80                Public listen port
--control=http://...     Control plane URL
--control-key=<secret>   Auth for internal API
--workers=http://w1:8080,http://w2:8080   Worker addresses
--poll-interval=5        Route table sync interval (seconds)
```

**appbase worker:**
```
--port=8080              Internal listen port (gate → worker)
--workers=16             ntex worker threads (default: num_cpus)
--control=http://...     Control plane URL
--control-key=<secret>   Auth for internal API
--max-isolates=200       Max V8 isolates per worker thread (LRU)
--poll-interval=5        Bundle sync interval (seconds)
```

## Migration from platform crate

1. Extract shared types (AppRecord, RegistryError, etc.) → `crates/common/`
2. Move enforcement (rate limit, concurrency, quota) → `crates/gateway/enforce.rs`
3. Move AppRegistry + metering → `crates/control/`
4. Rewrite V8 dispatch as Option B → `crates/worker/`
5. Build gateway as new component → `crates/gateway/`
6. Remove flume, sqlx, tokio, axum from workspace
7. Deprecate `crates/platform/`

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

- /internal/* requires control-key (shared secret, Bearer token)
- Gateway validates user API keys (hash comparison, not plaintext)
- Worker only reachable from gateway (private network / firewall)
- Bundle storage tenant-isolated by app_id path
- deploy_hash for integrity, not access control
- S3 buckets private; access via pre-signed URL or control plane proxy
- Admin API requires per-app API key or master key

## Scope Exclusions (v1)

- Custom domains (Host header routing) — v2
- Geographic routing / multi-region — v2
- Canary deploys / traffic splitting — v2
- Auto-scaling workers — external orchestrator (K8s/Nomad)
- Bundle garbage collection — manual for v1
- Encrypted bundles at rest — v2
- HTTP/2 (ntex supports it, enable with TLS)
- V8 snapshots (faster cold start) — separate effort
- Sticky sessions in gateway — v2
- Worker health checks in gateway — v2
