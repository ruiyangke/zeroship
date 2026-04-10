# Control Plane / Data Plane Split

## Goal

Split the monolith `appbase platform` into a stateful control plane (owns Postgres, manages apps/billing) and stateless data plane workers (run V8 isolates, handle user traffic). Workers sync via HTTP pull + WebSocket hints. Bundle storage abstracted via VFS (local filesystem or S3).

## Architecture

```
                         ┌──────────────────────────────┐
                         │        Control Plane          │
                         │     (1 instance, stateful)    │
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
          │  (stateless) │ │  (stateless) │ │  (stateless) │
          │              │ │              │ │              │
 Users ──→│ compio HTTP  │ │ compio HTTP  │ │ compio HTTP  │
          │ V8Pool       │ │ V8Pool       │ │ V8Pool       │
          │ enforcement  │ │ enforcement  │ │ enforcement  │
          │              │ │              │ │              │
          │ No Postgres  │ │ No Postgres  │ │ No Postgres  │
          └──────────────┘ └──────────────┘ └──────────────┘
```

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

**S3**: `s3://bucket/{app_id}/bundle.appbundle` — multi-server production. Data plane can pull directly via pre-signed URL or proxied through control plane.

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

On hint, data plane fetches the bundle immediately instead of waiting for the next poll cycle. If the WebSocket disconnects, the 5s poll is the fallback. The push is a performance optimization, not the source of truth.

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

Control plane aggregates deltas into the `usage` table.

### Authentication

Data plane authenticates to control plane with a shared secret:
- CLI flag: `--control-key=<secret>`
- Env var fallback: `APPBASE_CONTROL_KEY`
- Sent as `Authorization: Bearer <key>` on all /internal/* requests

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

Admin API authenticated via API key (per-app) or master key (global admin).

## Data Plane Request Flow

```
User HTTP request arrives:
  │
  ├─ Parse HTTP (compio httparse)
  ├─ Extract app_id from path: /apps/{app_id}/rpc
  │
  ├─ App in local V8Pool?
  │   ├─ Yes → rate limit (atomic) → dispatch to V8 → respond
  │   └─ No  → GET /internal/bundles/{app_id}
  │            → verify sha256(bundle) == expected hash
  │            → load into V8Pool → dispatch → respond
  │
  ├─ Record usage (atomic in-memory counters)
  └─ Background: flush counters to control plane every 10s
```

App routing for v1: path prefix `/apps/{app_id}/rpc`. Future: Host header (`myapp.appbase.dev`).

## Crate Structure

```
crates/
├── runtime/          V8 engine (unchanged)
├── runtime-macros/   Proc macros (unchanged)
├── pg/               PostgreSQL driver (unchanged)
├── compiler/         SWC + esbuild (unchanged)
├── common/           Shared types + VFS
│   ├── types.rs      AppRecord, VersionMap, UsageReport
│   ├── vfs.rs        BundleStore trait + LocalFs + S3
│   └── auth.rs       Worker key validation
├── control/          Control plane server
│   ├── main.rs       Entry point (compio HTTP)
│   ├── api.rs        Public admin API
│   ├── internal.rs   Internal API for workers
│   ├── registry.rs   App CRUD (uses appbase-pg)
│   └── metering.rs   Usage aggregation
├── worker/           Data plane server
│   ├── main.rs       Entry point (compio HTTP + V8Pool)
│   ├── sync.rs       Poll loop + WebSocket hints
│   ├── handler.rs    Request dispatch + enforcement
│   └── cache.rs      Local bundle cache
└── cli/              CLI commands
    ├── appbase serve      Single-tenant (unchanged)
    ├── appbase control    Start control plane
    ├── appbase worker     Start data plane worker
    ├── appbase deploy     Deploy to control plane
    └── appbase build      Compile only (unchanged)
```

Dependency graph:
```
control → common, pg
worker  → common, runtime
common  → (no runtime deps)
```

No circular dependencies. Worker never imports pg. Control never imports runtime.

## Deploy Flow

```
1. CLI:   appbase deploy myapp/ --control=https://control.example.com
2. CLI:   Compile JS → .appbundle (SWC + esbuild + zstd)
3. CLI:   POST /api/apps/myapp/deploy (multipart: .appbundle)
4. Control: hash = sha256(appbundle)
5. Control: bundle_store.put(app_id, bytes)
6. Control: UPDATE apps SET deploy_hash = $1, updated_at = NOW() WHERE name = $2
7. Control: Push WS event { type: "deploy", app_id, hash }
8. Worker:  Receives hint (or polls within 5s)
9. Worker:  GET /internal/bundles/{app_id}
10. Worker: Verify sha256(bytes) == hash
11. Worker: Load into V8Pool, start serving new version
```

## Rollback

```
1. Admin:  POST /api/apps/myapp/rollback { hash: "previous_hash" }
2. Control: Verify bundle exists in VFS
3. Control: UPDATE apps SET deploy_hash = $1, updated_at = NOW()
4. Control: Push WS event
5. Worker:  Pulls the old bundle (still in VFS), reloads
```

Old bundles are kept in VFS until explicitly garbage-collected.

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
