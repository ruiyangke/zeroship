# Control Plane Design

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a persistent control plane for multi-tenant app management — app registry, deploy API, per-app plans, hot reload.

**Architecture:** New `crates/control/` crate defines `AppRegistry` trait (the boundary between control and data planes). Day 1 implementation: `SqlxRegistry` backed by SQLite or Postgres via SQLx `AnyPool`. Data plane (`crates/server/`) depends on `Arc<dyn AppRegistry>`. Edge nodes poll for version changes every 5s.

**Tech Stack:** SQLx (async, multi-DB), axum (API endpoints), uuid (API keys)

---

## AppRegistry Trait

```rust
#[async_trait]
pub trait AppRegistry: Send + Sync {
    async fn get_app(&self, app_id: &str) -> Result<Option<AppData>, RegistryError>;
    async fn get_version(&self, app_id: &str) -> Result<Option<i64>, RegistryError>;
    async fn get_plan(&self, app_id: &str) -> Result<String, RegistryError>;
    async fn create_app(&self, app_id: &str, plan_id: &str) -> Result<AppRecord, RegistryError>;
    async fn deploy(&self, app_id: &str, server_js: &str, client_html: Option<&[u8]>) -> Result<i64, RegistryError>;
    async fn delete_app(&self, app_id: &str) -> Result<bool, RegistryError>;
    async fn list_apps(&self) -> Result<Vec<AppRecord>, RegistryError>;
    async fn set_plan(&self, app_id: &str, plan_id: &str) -> Result<bool, RegistryError>;
    async fn validate_key(&self, app_id: &str, key: &str) -> Result<bool, RegistryError>;
}
```

## Schema

```sql
CREATE TABLE IF NOT EXISTS apps (
    id          TEXT PRIMARY KEY,
    plan_id     TEXT NOT NULL DEFAULT 'free',
    server_js   TEXT NOT NULL DEFAULT '',
    client_html BLOB,
    version     INTEGER NOT NULL DEFAULT 0,
    api_key     TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
```

## API Endpoints

```
POST   /api/apps                  — create app (master key)
DELETE /api/apps/:id              — delete app (master key)
POST   /api/apps/:id/deploy      — deploy JS (per-app key)
GET    /api/apps/:id              — get app info (master key)
PUT    /api/apps/:id/plan         — set plan (master key)
GET    /api/apps                  — list apps (master key)
```

Auth: `Authorization: Bearer <key>`. Master key from config. Per-app key generated at creation.

## Request Routing

```
1. Host: my-app.platform.dev → app_id = "my-app"
2. X-App-Id: my-app          → app_id = "my-app"
3. neither                   → app_id = default_app
```

## Hot Reload

- Data plane caches (app_id, version, bundle) in memory
- Background task polls get_version() every 5s for cached apps
- DB version > cached → evict isolate, next request loads fresh
- Same-node deploy evicts immediately

## Crate Structure

```
crates/control/
  Cargo.toml
  src/
    lib.rs              — AppRegistry trait, types, RegistryError
    sqlx_registry.rs    — SqlxRegistry (SQLite + Postgres via AnyPool)
```
