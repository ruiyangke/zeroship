# Platform Split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split `crates/platform/` into 4 new crates: `common`, `control`, `gateway`, `worker` — with ntex + compio HTTP, appbase-pg database, and V8 per thread (Option B).

**Architecture:** Three binaries (appbase-control, appbase-gate, appbase-worker) communicate via HTTP pull + WebSocket hints. Gateway handles auth/enforcement, worker runs V8, control owns DB + VFS. Shared types in common crate.

**Tech Stack:** ntex 3.7 (compio feature), appbase-pg, appbase-runtime, compio 0.18, serde, uuid

---

## Phase 1: Common Crate

### Task 1: Scaffold common crate with shared types

**Files:**
- Create: `crates/common/Cargo.toml`
- Create: `crates/common/src/lib.rs`
- Create: `crates/common/src/types.rs`
- Modify: `Cargo.toml` (workspace root — add `"crates/common"` to members)

- [ ] **Step 1: Create `crates/common/Cargo.toml`**

```toml
[package]
name = "appbase-common"
version = "0.1.0"
edition = "2021"
description = "Shared types for appbase control/gate/worker"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
uuid = { version = "1", features = ["v4", "serde"] }
sha2 = "0.10"
thiserror = "2"

[lints]
workspace = true
```

- [ ] **Step 2: Create `crates/common/src/types.rs`**

```rust
//! Shared types for control plane, gateway, and worker.

use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// App metadata (no code — code lives in VFS).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRecord {
    pub id: Uuid,
    pub name: String,
    pub plan_id: String,
    pub deploy_hash: Option<String>,
    pub api_key: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Route entry for gateway — everything needed for auth + enforcement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub name: String,
    pub plan_id: String,
    pub api_key_hash: String,
    pub deploy_hash: Option<String>,
}

/// Version map: app_id → deploy_hash (used by worker sync).
pub type VersionMap = HashMap<Uuid, Option<String>>;

/// Route map: app_id → route entry (used by gateway sync).
pub type RouteMap = HashMap<Uuid, RouteEntry>;

/// Usage counters from a single worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageReport {
    pub worker_id: String,
    pub counters: HashMap<Uuid, AppUsage>,
}

/// Per-app usage counters.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppUsage {
    pub requests: u64,
    pub cpu_us: u64,
    pub wall_us: u64,
    pub egress_bytes: u64,
    pub ingress_bytes: u64,
}

/// Events pushed via WebSocket from control plane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlEvent {
    #[serde(rename = "deploy")]
    Deploy { app_id: Uuid, hash: String },
    #[serde(rename = "delete")]
    Delete { app_id: Uuid },
    #[serde(rename = "plan_change")]
    PlanChange { app_id: Uuid, plan_id: String },
}

/// Errors shared across crates.
#[derive(Debug, thiserror::Error)]
pub enum CommonError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("internal: {0}")]
    Internal(String),
}
```

- [ ] **Step 3: Create `crates/common/src/lib.rs`**

```rust
//! appbase-common — shared types and abstractions for the appbase platform.
//!
//! Used by: appbase-control, appbase-gate, appbase-worker.

pub mod types;
pub mod vfs;
pub mod auth;

pub use types::*;
```

- [ ] **Step 4: Create stub modules**

`crates/common/src/vfs.rs`:
```rust
//! Virtual filesystem for .appbundle storage.
```

`crates/common/src/auth.rs`:
```rust
//! Control key validation for internal API auth.
```

- [ ] **Step 5: Add to workspace**

Add `"crates/common"` to `members` in root `Cargo.toml`.

- [ ] **Step 6: Verify**

Run: `cargo check -p appbase-common`

- [ ] **Step 7: Commit**

```bash
git add crates/common/ Cargo.toml
git commit -m "feat(common): scaffold shared types — AppRecord, RouteEntry, VersionMap, UsageReport"
```

---

### Task 2: VFS — BundleStore trait + LocalFs implementation

**Files:**
- Modify: `crates/common/src/vfs.rs`

- [ ] **Step 1: Implement BundleStore trait and LocalFs**

```rust
//! Virtual filesystem for .appbundle storage.
//!
//! BundleStore trait with two implementations:
//! - LocalFs: local filesystem (dev, single-server)
//! - S3: object storage (production, multi-server) — v2

use std::path::{Path, PathBuf};

/// Errors from bundle storage operations.
#[derive(Debug, thiserror::Error)]
pub enum VfsError {
    #[error("bundle not found: {0}")]
    NotFound(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(String),
}

pub type VfsResult<T> = Result<T, VfsError>;

/// Virtual filesystem for .appbundle storage.
/// Trait is sync — LocalFs uses blocking I/O (fast enough for bundles).
pub trait BundleStore: Send + Sync {
    /// Store a bundle for an app. Overwrites if exists.
    fn put(&self, app_id: &str, data: &[u8]) -> VfsResult<()>;

    /// Retrieve a bundle for an app.
    fn get(&self, app_id: &str) -> VfsResult<Vec<u8>>;

    /// Delete a bundle for an app.
    fn delete(&self, app_id: &str) -> VfsResult<()>;

    /// Check if a bundle exists for an app.
    fn exists(&self, app_id: &str) -> VfsResult<bool>;
}

/// Local filesystem bundle store.
///
/// Layout: `{base_dir}/{app_id}/bundle.appbundle`
pub struct LocalFs {
    base_dir: PathBuf,
}

impl LocalFs {
    /// Create a new LocalFs store. Creates base_dir if it doesn't exist.
    pub fn new(base_dir: impl AsRef<Path>) -> VfsResult<Self> {
        let base_dir = base_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&base_dir)?;
        Ok(Self { base_dir })
    }

    fn bundle_path(&self, app_id: &str) -> PathBuf {
        self.base_dir.join(app_id).join("bundle.appbundle")
    }
}

impl BundleStore for LocalFs {
    fn put(&self, app_id: &str, data: &[u8]) -> VfsResult<()> {
        let path = self.bundle_path(app_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, data)?;
        Ok(())
    }

    fn get(&self, app_id: &str) -> VfsResult<Vec<u8>> {
        let path = self.bundle_path(app_id);
        if !path.exists() {
            return Err(VfsError::NotFound(app_id.to_string()));
        }
        Ok(std::fs::read(&path)?)
    }

    fn delete(&self, app_id: &str) -> VfsResult<()> {
        let dir = self.base_dir.join(app_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }

    fn exists(&self, app_id: &str) -> VfsResult<bool> {
        Ok(self.bundle_path(app_id).exists())
    }
}

impl std::fmt::Debug for LocalFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalFs")
            .field("base_dir", &self.base_dir)
            .finish()
    }
}
```

- [ ] **Step 2: Verify**

Run: `cargo check -p appbase-common`

- [ ] **Step 3: Commit**

```bash
git add crates/common/src/vfs.rs
git commit -m "feat(common): BundleStore trait + LocalFs implementation"
```

---

### Task 3: Auth — control key validation

**Files:**
- Modify: `crates/common/src/auth.rs`

- [ ] **Step 1: Implement control key validation**

```rust
//! Control key validation for internal API auth.
//!
//! Gate and worker authenticate to control plane using a shared secret.
//! Sent as `Authorization: Bearer <key>` on all /internal/* requests.

use sha2::{Sha256, Digest};

/// Validate a Bearer token against the expected control key.
pub fn validate_control_key(provided: &str, expected: &str) -> bool {
    // Constant-time comparison to prevent timing attacks
    if provided.len() != expected.len() {
        return false;
    }
    provided
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Hash an API key for storage (don't store plaintext in routing table).
pub fn hash_api_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Validate a provided API key against a stored hash.
pub fn validate_api_key(provided: &str, stored_hash: &str) -> bool {
    let provided_hash = hash_api_key(provided);
    validate_control_key(&provided_hash, stored_hash)
}

/// Extract Bearer token from an Authorization header value.
pub fn extract_bearer(header: &str) -> Option<&str> {
    header.strip_prefix("Bearer ")
}
```

- [ ] **Step 2: Add `hex` dependency to Cargo.toml**

Add `hex = "0.4"` under `[dependencies]` in `crates/common/Cargo.toml`.

- [ ] **Step 3: Verify**

Run: `cargo check -p appbase-common`

- [ ] **Step 4: Commit**

```bash
git add crates/common/
git commit -m "feat(common): auth — control key + API key validation (constant-time)"
```

---

### Task 4: Unit tests for common crate

**Files:**
- Create: `crates/common/tests/types_test.rs`
- Create: `crates/common/tests/vfs_test.rs`
- Create: `crates/common/tests/auth_test.rs`

- [ ] **Step 1: Write VFS tests**

```rust
//! Tests for BundleStore + LocalFs

use appbase_common::vfs::{BundleStore, LocalFs, VfsError};
use std::path::PathBuf;

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("appbase-vfs-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn put_and_get() {
    let dir = temp_dir();
    let vfs = LocalFs::new(&dir).unwrap();
    vfs.put("app1", b"hello bundle").unwrap();
    let data = vfs.get("app1").unwrap();
    assert_eq!(data, b"hello bundle");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn get_nonexistent() {
    let dir = temp_dir();
    let vfs = LocalFs::new(&dir).unwrap();
    let err = vfs.get("ghost").unwrap_err();
    assert!(matches!(err, VfsError::NotFound(_)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exists() {
    let dir = temp_dir();
    let vfs = LocalFs::new(&dir).unwrap();
    assert!(!vfs.exists("app1").unwrap());
    vfs.put("app1", b"data").unwrap();
    assert!(vfs.exists("app1").unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn delete() {
    let dir = temp_dir();
    let vfs = LocalFs::new(&dir).unwrap();
    vfs.put("app1", b"data").unwrap();
    vfs.delete("app1").unwrap();
    assert!(!vfs.exists("app1").unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn overwrite() {
    let dir = temp_dir();
    let vfs = LocalFs::new(&dir).unwrap();
    vfs.put("app1", b"v1").unwrap();
    vfs.put("app1", b"v2").unwrap();
    assert_eq!(vfs.get("app1").unwrap(), b"v2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multiple_apps_isolated() {
    let dir = temp_dir();
    let vfs = LocalFs::new(&dir).unwrap();
    vfs.put("a", b"data-a").unwrap();
    vfs.put("b", b"data-b").unwrap();
    assert_eq!(vfs.get("a").unwrap(), b"data-a");
    assert_eq!(vfs.get("b").unwrap(), b"data-b");
    vfs.delete("a").unwrap();
    assert!(!vfs.exists("a").unwrap());
    assert!(vfs.exists("b").unwrap());
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 2: Write auth tests**

```rust
//! Tests for auth module

use appbase_common::auth::{
    validate_control_key, hash_api_key, validate_api_key, extract_bearer,
};

#[test]
fn control_key_valid() {
    assert!(validate_control_key("secret123", "secret123"));
}

#[test]
fn control_key_invalid() {
    assert!(!validate_control_key("secret123", "wrong"));
}

#[test]
fn control_key_empty() {
    assert!(validate_control_key("", ""));
    assert!(!validate_control_key("a", ""));
    assert!(!validate_control_key("", "a"));
}

#[test]
fn api_key_roundtrip() {
    let key = "my-api-key-12345";
    let hash = hash_api_key(key);
    assert!(validate_api_key(key, &hash));
    assert!(!validate_api_key("wrong-key", &hash));
}

#[test]
fn extract_bearer_token() {
    assert_eq!(extract_bearer("Bearer abc123"), Some("abc123"));
    assert_eq!(extract_bearer("Bearer "), Some(""));
    assert_eq!(extract_bearer("Basic abc"), None);
    assert_eq!(extract_bearer(""), None);
}
```

- [ ] **Step 3: Write types serialization tests**

```rust
//! Tests for shared types serialization

use appbase_common::types::*;
use uuid::Uuid;

#[test]
fn control_event_deploy_json() {
    let event = ControlEvent::Deploy {
        app_id: Uuid::nil(),
        hash: "abc123".to_string(),
    };
    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("\"type\":\"deploy\""));
    assert!(json.contains("\"hash\":\"abc123\""));

    let parsed: ControlEvent = serde_json::from_str(&json).unwrap();
    match parsed {
        ControlEvent::Deploy { hash, .. } => assert_eq!(hash, "abc123"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn usage_report_roundtrip() {
    let report = UsageReport {
        worker_id: "w1".to_string(),
        counters: {
            let mut m = std::collections::HashMap::new();
            m.insert(Uuid::nil(), AppUsage {
                requests: 100,
                cpu_us: 5000,
                ..Default::default()
            });
            m
        },
    };
    let json = serde_json::to_string(&report).unwrap();
    let parsed: UsageReport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.worker_id, "w1");
    assert_eq!(parsed.counters[&Uuid::nil()].requests, 100);
}

#[test]
fn route_entry_roundtrip() {
    let entry = RouteEntry {
        name: "myapp".to_string(),
        plan_id: "pro".to_string(),
        api_key_hash: "abc".to_string(),
        deploy_hash: Some("def".to_string()),
    };
    let json = serde_json::to_string(&entry).unwrap();
    let parsed: RouteEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.name, "myapp");
    assert_eq!(parsed.deploy_hash, Some("def".to_string()));
}
```

- [ ] **Step 4: Run all tests**

Run: `cargo test -p appbase-common`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/common/tests/
git commit -m "test(common): unit tests for VFS, auth, types serialization"
```

---

## Phase 2: Control Plane (`appbase-control`)

### Task 5: Scaffold control crate

**Files:**
- Create: `crates/control/Cargo.toml`
- Create: `crates/control/src/main.rs`
- Create: `crates/control/src/registry.rs` (stub)
- Create: `crates/control/src/api.rs` (stub)
- Create: `crates/control/src/internal.rs` (stub)
- Create: `crates/control/src/metering.rs` (stub)
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Create `crates/control/Cargo.toml`**

```toml
[package]
name = "appbase-control"
version = "0.1.0"
edition = "2021"
description = "appbase control plane — admin API, internal API, Postgres, VFS"

[[bin]]
name = "appbase-control"
path = "src/main.rs"

[dependencies]
appbase-common = { path = "../common" }
appbase-pg = { path = "../pg" }
ntex = { version = "3", features = ["compio"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
uuid = { version = "1", features = ["v4", "serde"] }
sha2 = "0.10"
hex = "0.4"
mimalloc = "0.1"

[lints]
workspace = true
```

- [ ] **Step 2: Create `crates/control/src/main.rs`**

```rust
//! appbase-control — control plane server.

mod api;
mod internal;
mod metering;
mod registry;

use std::sync::Arc;
use ntex::web;
use appbase_common::vfs::LocalFs;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub struct AppState {
    pub registry: registry::Registry,
    pub vfs: Arc<dyn appbase_common::vfs::BundleStore>,
    pub control_key: String,
}

#[ntex::main]
async fn main() -> std::io::Result<()> {
    let port: u16 = std::env::args()
        .find(|a| a.starts_with("--port="))
        .and_then(|a| a.strip_prefix("--port=").unwrap().parse().ok())
        .unwrap_or(3000);

    let db_url = std::env::args()
        .find(|a| a.starts_with("--db="))
        .map(|a| a.strip_prefix("--db=").unwrap().to_string())
        .unwrap_or_else(|| "postgres://postgres:test@localhost:5432/appbase".to_string());

    let bundles_dir = std::env::args()
        .find(|a| a.starts_with("--bundles="))
        .map(|a| a.strip_prefix("--bundles=").unwrap().to_string())
        .unwrap_or_else(|| "./bundles".to_string());

    let control_key = std::env::args()
        .find(|a| a.starts_with("--control-key="))
        .map(|a| a.strip_prefix("--control-key=").unwrap().to_string())
        .or_else(|| std::env::var("APPBASE_CONTROL_KEY").ok())
        .unwrap_or_else(|| "dev-key".to_string());

    let master_key = std::env::args()
        .find(|a| a.starts_with("--master-key="))
        .map(|a| a.strip_prefix("--master-key=").unwrap().to_string())
        .or_else(|| std::env::var("APPBASE_MASTER_KEY").ok())
        .unwrap_or_else(|| "dev-master".to_string());

    eprintln!("[appbase-control] connecting to {db_url}");
    let registry = registry::Registry::new(&db_url).await
        .expect("Failed to connect to database");

    let vfs = Arc::new(LocalFs::new(&bundles_dir).expect("Failed to create VFS"));

    let state = Arc::new(AppState {
        registry,
        vfs,
        control_key,
    });

    eprintln!("[appbase-control] http://0.0.0.0:{port}");

    web::server(async move || {
        let state = state.clone();
        web::App::new()
            .state(state)
            // Admin API
            .service(web::resource("/api/apps").route(web::post().to(api::create_app)))
            .service(web::resource("/api/apps").route(web::get().to(api::list_apps)))
            .service(web::resource("/api/apps/{id}").route(web::get().to(api::get_app)))
            .service(web::resource("/api/apps/{id}").route(web::delete().to(api::delete_app)))
            .service(web::resource("/api/apps/{id}/deploy").route(web::post().to(api::deploy)))
            .service(web::resource("/api/apps/{id}/plan").route(web::put().to(api::set_plan)))
            .service(web::resource("/api/apps/{id}/usage").route(web::get().to(api::get_usage)))
            // Internal API
            .service(web::resource("/internal/versions").route(web::get().to(internal::versions)))
            .service(web::resource("/internal/bundles/{app_id}").route(web::get().to(internal::get_bundle)))
            .service(web::resource("/internal/routes").route(web::get().to(internal::routes)))
            .service(web::resource("/internal/usage").route(web::post().to(internal::report_usage)))
            // Health
            .service(web::resource("/health").route(web::get().to(|| async {
                web::HttpResponse::Ok().body(r#"{"status":"ok"}"#)
            })))
    })
    .bind(format!("0.0.0.0:{port}"))?
    .run()
    .await
}
```

- [ ] **Step 3: Create stub modules**

`crates/control/src/registry.rs`:
```rust
//! App registry — CRUD operations backed by appbase-pg.

pub struct Registry;

impl Registry {
    pub async fn new(_db_url: &str) -> Result<Self, String> {
        Ok(Self)
    }
}
```

`crates/control/src/api.rs`:
```rust
//! Public admin API handlers.

use ntex::web;

pub async fn create_app() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
pub async fn list_apps() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
pub async fn get_app() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
pub async fn delete_app() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
pub async fn deploy() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
pub async fn set_plan() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
pub async fn get_usage() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
```

`crates/control/src/internal.rs`:
```rust
//! Internal API handlers for gate + worker.

use ntex::web;

pub async fn versions() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
pub async fn get_bundle() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
pub async fn routes() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
pub async fn report_usage() -> web::HttpResponse { web::HttpResponse::Ok().finish() }
```

`crates/control/src/metering.rs`:
```rust
//! Usage aggregation from workers.
```

- [ ] **Step 4: Add to workspace, verify**

Add `"crates/control"` to workspace members.
Run: `cargo check -p appbase-control`

- [ ] **Step 5: Commit**

```bash
git add crates/control/ Cargo.toml
git commit -m "feat(control): scaffold appbase-control with ntex + stub routes"
```

---

### Task 6: Registry — App CRUD with appbase-pg

**Files:**
- Modify: `crates/control/src/registry.rs`

Implement all app CRUD operations using appbase-pg. Port from the existing `crates/platform/src/control/sqlx_registry.rs` — same SQL queries, different driver.

- [ ] **Step 1: Implement Registry**

```rust
//! App registry — CRUD operations backed by appbase-pg.

use appbase_common::types::{AppRecord, RouteEntry, RouteMap, VersionMap};
use appbase_common::auth::hash_api_key;
use appbase_pg::{Pool, Error as PgError};
use uuid::Uuid;
use std::collections::HashMap;

#[derive(Debug)]
pub enum RegistryError {
    NotFound(String),
    AlreadyExists(String),
    Database(String),
    InvalidInput(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(s) => write!(f, "not found: {s}"),
            Self::AlreadyExists(s) => write!(f, "already exists: {s}"),
            Self::Database(s) => write!(f, "database: {s}"),
            Self::InvalidInput(s) => write!(f, "invalid input: {s}"),
        }
    }
}

impl From<PgError> for RegistryError {
    fn from(e: PgError) -> Self {
        let msg = e.to_string();
        if msg.contains("duplicate key") || msg.contains("unique") {
            Self::AlreadyExists(msg)
        } else {
            Self::Database(msg)
        }
    }
}

pub struct Registry {
    pool: Pool,
}

impl Registry {
    pub async fn new(db_url: &str) -> Result<Self, String> {
        let pool = Pool::connect(db_url, 4).await.map_err(|e| e.to_string())?;

        // Run migrations
        pool.execute(
            "CREATE TABLE IF NOT EXISTS apps (
                id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                name         TEXT NOT NULL UNIQUE,
                plan_id      TEXT NOT NULL DEFAULT 'free',
                deploy_hash  TEXT,
                api_key      TEXT NOT NULL,
                created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )", &[]
        ).await.map_err(|e| e.to_string())?;

        pool.execute(
            "CREATE TABLE IF NOT EXISTS usage (
                app_id       UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
                resource     TEXT NOT NULL,
                value        BIGINT NOT NULL DEFAULT 0,
                PRIMARY KEY (app_id, resource)
            )", &[]
        ).await.map_err(|e| e.to_string())?;

        pool.execute(
            "CREATE TABLE IF NOT EXISTS usage_history (
                app_id       UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
                period       TEXT NOT NULL,
                counters     JSONB NOT NULL,
                created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )", &[]
        ).await.map_err(|e| e.to_string())?;

        pool.execute(
            "CREATE INDEX IF NOT EXISTS idx_usage_history_app ON usage_history(app_id, period)",
            &[]
        ).await.map_err(|e| e.to_string())?;

        Ok(Self { pool })
    }

    pub async fn create_app(&self, name: &str, plan_id: &str) -> Result<AppRecord, RegistryError> {
        if name.is_empty() || name.len() > 64
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(RegistryError::InvalidInput(
                "name must be 1-64 alphanumeric/hyphen/underscore".into(),
            ));
        }
        let api_key = Uuid::new_v4().to_string();
        self.pool.execute(
            "INSERT INTO apps (name, plan_id, api_key) VALUES ($1, $2, $3)",
            &[&name, &plan_id, &api_key],
        ).await?;

        let rows = self.pool.query(
            "SELECT id, name, plan_id, deploy_hash, api_key, created_at::text, updated_at::text FROM apps WHERE name = $1",
            &[&name],
        ).await?;
        if rows.is_empty() {
            return Err(RegistryError::Database("insert succeeded but select failed".into()));
        }
        Ok(row_to_app_record(&rows[0]))
    }

    pub async fn get_app(&self, id: &Uuid) -> Result<Option<AppRecord>, RegistryError> {
        let rows = self.pool.query(
            "SELECT id, name, plan_id, deploy_hash, api_key, created_at::text, updated_at::text FROM apps WHERE id = $1",
            &[id],
        ).await?;
        Ok(rows.first().map(row_to_app_record))
    }

    pub async fn get_app_by_name(&self, name: &str) -> Result<Option<AppRecord>, RegistryError> {
        let rows = self.pool.query(
            "SELECT id, name, plan_id, deploy_hash, api_key, created_at::text, updated_at::text FROM apps WHERE name = $1",
            &[&name],
        ).await?;
        Ok(rows.first().map(row_to_app_record))
    }

    pub async fn list_apps(&self) -> Result<Vec<AppRecord>, RegistryError> {
        let rows = self.pool.query(
            "SELECT id, name, plan_id, deploy_hash, api_key, created_at::text, updated_at::text FROM apps ORDER BY name",
            &[],
        ).await?;
        Ok(rows.iter().map(row_to_app_record).collect())
    }

    pub async fn delete_app(&self, id: &Uuid) -> Result<bool, RegistryError> {
        let affected = self.pool.execute(
            "DELETE FROM apps WHERE id = $1", &[id],
        ).await?;
        Ok(affected > 0)
    }

    pub async fn set_deploy_hash(&self, id: &Uuid, hash: &str) -> Result<bool, RegistryError> {
        let affected = self.pool.execute(
            "UPDATE apps SET deploy_hash = $1, updated_at = NOW() WHERE id = $2",
            &[&hash, id],
        ).await?;
        Ok(affected > 0)
    }

    pub async fn set_plan(&self, id: &Uuid, plan_id: &str) -> Result<bool, RegistryError> {
        let affected = self.pool.execute(
            "UPDATE apps SET plan_id = $1, updated_at = NOW() WHERE id = $2",
            &[&plan_id, id],
        ).await?;
        Ok(affected > 0)
    }

    /// Get version map for worker sync: { app_id → deploy_hash }
    pub async fn get_versions(&self) -> Result<VersionMap, RegistryError> {
        let rows = self.pool.query(
            "SELECT id, deploy_hash FROM apps", &[],
        ).await?;
        let mut map = HashMap::new();
        for row in &rows {
            let id: Uuid = row.get("id");
            let hash: Option<String> = row.get("deploy_hash");
            map.insert(id, hash);
        }
        Ok(map)
    }

    /// Get routing table for gateway sync: { app_id → route entry }
    pub async fn get_routes(&self) -> Result<RouteMap, RegistryError> {
        let rows = self.pool.query(
            "SELECT id, name, plan_id, api_key, deploy_hash FROM apps", &[],
        ).await?;
        let mut map = HashMap::new();
        for row in &rows {
            let id: Uuid = row.get("id");
            let api_key: String = row.get("api_key");
            map.insert(id, RouteEntry {
                name: row.get("name"),
                plan_id: row.get("plan_id"),
                api_key_hash: hash_api_key(&api_key),
                deploy_hash: row.get("deploy_hash"),
            });
        }
        Ok(map)
    }

    /// Record usage counters from a worker.
    pub async fn record_usage(&self, app_id: &Uuid, resource: &str, delta: i64) -> Result<(), RegistryError> {
        self.pool.execute(
            "INSERT INTO usage (app_id, resource, value) VALUES ($1, $2, $3)
             ON CONFLICT (app_id, resource) DO UPDATE SET value = usage.value + $3",
            &[app_id, &resource, &delta],
        ).await?;
        Ok(())
    }

    /// Get usage counters for an app.
    pub async fn get_usage(&self, app_id: &Uuid) -> Result<HashMap<String, i64>, RegistryError> {
        let rows = self.pool.query(
            "SELECT resource, value FROM usage WHERE app_id = $1", &[app_id],
        ).await?;
        let mut map = HashMap::new();
        for row in &rows {
            let resource: String = row.get("resource");
            let value: i64 = row.get("value");
            map.insert(resource, value);
        }
        Ok(map)
    }
}

fn row_to_app_record(row: &appbase_pg::Row) -> AppRecord {
    AppRecord {
        id: row.get("id"),
        name: row.get("name"),
        plan_id: row.get("plan_id"),
        deploy_hash: row.get("deploy_hash"),
        api_key: row.get("api_key"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}
```

- [ ] **Step 2: Verify**

Run: `cargo check -p appbase-control`

- [ ] **Step 3: Commit**

```bash
git add crates/control/src/registry.rs
git commit -m "feat(control): registry — app CRUD + versions/routes/usage via appbase-pg"
```

---

### Task 7: Admin API handlers

**Files:**
- Modify: `crates/control/src/api.rs`

Implement all `/api/apps/*` routes using the Registry.

- [ ] **Step 1: Implement admin API**

```rust
//! Public admin API handlers.

use std::sync::Arc;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use sha2::{Sha256, Digest};

use crate::AppState;

#[derive(Deserialize)]
pub struct CreateAppRequest {
    pub name: String,
    #[serde(default = "default_plan")]
    pub plan_id: String,
}

fn default_plan() -> String { "free".to_string() }

#[derive(Deserialize)]
pub struct SetPlanRequest {
    pub plan_id: String,
}

pub async fn create_app(
    state: web::types::State<Arc<AppState>>,
    body: web::types::Json<CreateAppRequest>,
) -> HttpResponse {
    match state.registry.create_app(&body.name, &body.plan_id).await {
        Ok(record) => HttpResponse::Created().json(&record),
        Err(e) => error_response(e),
    }
}

pub async fn list_apps(state: web::types::State<Arc<AppState>>) -> HttpResponse {
    match state.registry.list_apps().await {
        Ok(apps) => HttpResponse::Ok().json(&apps),
        Err(e) => error_response(e),
    }
}

pub async fn get_app(
    state: web::types::State<Arc<AppState>>,
    path: web::types::Path<String>,
) -> HttpResponse {
    let id = match path.parse::<uuid::Uuid>() {
        Ok(id) => id,
        Err(_) => return HttpResponse::BadRequest().json(&serde_json::json!({"error": "invalid UUID"})),
    };
    match state.registry.get_app(&id).await {
        Ok(Some(app)) => HttpResponse::Ok().json(&app),
        Ok(None) => HttpResponse::NotFound().json(&serde_json::json!({"error": "not found"})),
        Err(e) => error_response(e),
    }
}

pub async fn delete_app(
    state: web::types::State<Arc<AppState>>,
    path: web::types::Path<String>,
) -> HttpResponse {
    let id = match path.parse::<uuid::Uuid>() {
        Ok(id) => id,
        Err(_) => return HttpResponse::BadRequest().json(&serde_json::json!({"error": "invalid UUID"})),
    };
    // Also delete from VFS
    let _ = state.vfs.delete(&id.to_string());
    match state.registry.delete_app(&id).await {
        Ok(true) => HttpResponse::Ok().json(&serde_json::json!({"deleted": true})),
        Ok(false) => HttpResponse::NotFound().json(&serde_json::json!({"error": "not found"})),
        Err(e) => error_response(e),
    }
}

pub async fn deploy(
    state: web::types::State<Arc<AppState>>,
    path: web::types::Path<String>,
    body: web::types::Bytes,
) -> HttpResponse {
    let id = match path.parse::<uuid::Uuid>() {
        Ok(id) => id,
        Err(_) => return HttpResponse::BadRequest().json(&serde_json::json!({"error": "invalid UUID"})),
    };

    // Verify app exists
    match state.registry.get_app(&id).await {
        Ok(None) => return HttpResponse::NotFound().json(&serde_json::json!({"error": "not found"})),
        Err(e) => return error_response(e),
        Ok(Some(_)) => {}
    }

    // Compute hash
    let mut hasher = Sha256::new();
    hasher.update(&body);
    let hash = hex::encode(hasher.finalize());

    // Store in VFS
    if let Err(e) = state.vfs.put(&id.to_string(), &body) {
        return HttpResponse::InternalServerError().json(&serde_json::json!({"error": e.to_string()}));
    }

    // Update deploy_hash in DB
    match state.registry.set_deploy_hash(&id, &hash).await {
        Ok(_) => HttpResponse::Ok().json(&serde_json::json!({"deploy_hash": hash})),
        Err(e) => error_response(e),
    }
}

pub async fn set_plan(
    state: web::types::State<Arc<AppState>>,
    path: web::types::Path<String>,
    body: web::types::Json<SetPlanRequest>,
) -> HttpResponse {
    let id = match path.parse::<uuid::Uuid>() {
        Ok(id) => id,
        Err(_) => return HttpResponse::BadRequest().json(&serde_json::json!({"error": "invalid UUID"})),
    };
    match state.registry.set_plan(&id, &body.plan_id).await {
        Ok(true) => HttpResponse::Ok().json(&serde_json::json!({"ok": true})),
        Ok(false) => HttpResponse::NotFound().json(&serde_json::json!({"error": "not found"})),
        Err(e) => error_response(e),
    }
}

pub async fn get_usage(
    state: web::types::State<Arc<AppState>>,
    path: web::types::Path<String>,
) -> HttpResponse {
    let id = match path.parse::<uuid::Uuid>() {
        Ok(id) => id,
        Err(_) => return HttpResponse::BadRequest().json(&serde_json::json!({"error": "invalid UUID"})),
    };
    match state.registry.get_usage(&id).await {
        Ok(usage) => HttpResponse::Ok().json(&usage),
        Err(e) => error_response(e),
    }
}

fn error_response(e: crate::registry::RegistryError) -> HttpResponse {
    use crate::registry::RegistryError::*;
    match e {
        NotFound(msg) => HttpResponse::NotFound().json(&serde_json::json!({"error": msg})),
        AlreadyExists(msg) => HttpResponse::Conflict().json(&serde_json::json!({"error": msg})),
        InvalidInput(msg) => HttpResponse::BadRequest().json(&serde_json::json!({"error": msg})),
        Database(msg) => HttpResponse::InternalServerError().json(&serde_json::json!({"error": msg})),
    }
}
```

- [ ] **Step 2: Verify**

Run: `cargo check -p appbase-control`

- [ ] **Step 3: Commit**

```bash
git add crates/control/src/api.rs
git commit -m "feat(control): admin API — create, list, get, delete, deploy, set_plan, usage"
```

---

### Task 8: Internal API handlers

**Files:**
- Modify: `crates/control/src/internal.rs`

Implement `/internal/*` routes with control-key auth.

- [ ] **Step 1: Implement internal API**

```rust
//! Internal API handlers for gate + worker.

use std::sync::Arc;
use ntex::web::{self, HttpRequest, HttpResponse};
use appbase_common::auth::{extract_bearer, validate_control_key};
use appbase_common::types::UsageReport;

use crate::AppState;

/// Check control-key auth. Returns error response if invalid.
fn check_auth(req: &HttpRequest, state: &AppState) -> Option<HttpResponse> {
    let auth = req.headers().get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(extract_bearer);

    match auth {
        Some(key) if validate_control_key(key, &state.control_key) => None,
        _ => Some(HttpResponse::Unauthorized().json(
            &serde_json::json!({"error": "invalid or missing control key"})
        )),
    }
}

/// GET /internal/versions → { app_id → deploy_hash }
pub async fn versions(
    req: HttpRequest,
    state: web::types::State<Arc<AppState>>,
) -> HttpResponse {
    if let Some(err) = check_auth(&req, &state) { return err; }

    match state.registry.get_versions().await {
        Ok(versions) => HttpResponse::Ok().json(&versions),
        Err(e) => HttpResponse::InternalServerError().json(
            &serde_json::json!({"error": e.to_string()})
        ),
    }
}

/// GET /internal/bundles/{app_id} → raw .appbundle bytes
pub async fn get_bundle(
    req: HttpRequest,
    state: web::types::State<Arc<AppState>>,
    path: web::types::Path<String>,
) -> HttpResponse {
    if let Some(err) = check_auth(&req, &state) { return err; }

    let app_id = path.into_inner();
    match state.vfs.get(&app_id) {
        Ok(data) => HttpResponse::Ok()
            .content_type("application/octet-stream")
            .body(data),
        Err(appbase_common::vfs::VfsError::NotFound(_)) => {
            HttpResponse::NotFound().json(&serde_json::json!({"error": "bundle not found"}))
        }
        Err(e) => HttpResponse::InternalServerError().json(
            &serde_json::json!({"error": e.to_string()})
        ),
    }
}

/// GET /internal/routes → { app_id → RouteEntry }
pub async fn routes(
    req: HttpRequest,
    state: web::types::State<Arc<AppState>>,
) -> HttpResponse {
    if let Some(err) = check_auth(&req, &state) { return err; }

    match state.registry.get_routes().await {
        Ok(routes) => HttpResponse::Ok().json(&routes),
        Err(e) => HttpResponse::InternalServerError().json(
            &serde_json::json!({"error": e.to_string()})
        ),
    }
}

/// POST /internal/usage ← usage counters from workers
pub async fn report_usage(
    req: HttpRequest,
    state: web::types::State<Arc<AppState>>,
    body: web::types::Json<UsageReport>,
) -> HttpResponse {
    if let Some(err) = check_auth(&req, &state) { return err; }

    for (app_id, usage) in &body.counters {
        let _ = state.registry.record_usage(app_id, "requests", usage.requests as i64).await;
        let _ = state.registry.record_usage(app_id, "cpu_us", usage.cpu_us as i64).await;
        let _ = state.registry.record_usage(app_id, "wall_us", usage.wall_us as i64).await;
        let _ = state.registry.record_usage(app_id, "egress_bytes", usage.egress_bytes as i64).await;
        let _ = state.registry.record_usage(app_id, "ingress_bytes", usage.ingress_bytes as i64).await;
    }

    HttpResponse::Ok().json(&serde_json::json!({"ok": true}))
}
```

- [ ] **Step 2: Verify**

Run: `cargo check -p appbase-control`

- [ ] **Step 3: Commit**

```bash
git add crates/control/src/internal.rs
git commit -m "feat(control): internal API — versions, bundles, routes, usage (with auth)"
```

---

## Phase 3: Worker (`appbase-worker`)

### Task 9: Scaffold worker crate

**Files:**
- Create: `crates/worker/Cargo.toml`
- Create: `crates/worker/src/main.rs`
- Create: `crates/worker/src/handler.rs`
- Create: `crates/worker/src/sync.rs`
- Create: `crates/worker/src/cache.rs`
- Modify: `Cargo.toml` (workspace root)

Implement the full worker binary: ntex HTTP server with `/dispatch/{app_id}` endpoint, V8 per thread (Option B), bundle sync loop from control plane, thread-local app cache with LRU eviction.

The worker receives requests from the gateway via `POST /dispatch/{app_id}` with the raw body. It looks up the V8 Runtime for that app in its thread-local cache, calls `dispatch_rpc`, and returns the result.

Bundle sync runs as a background task on each worker thread, polling `GET /internal/versions` every 5s, pulling changed bundles via `GET /internal/bundles/{app_id}`.

- [ ] **Step 1: Create all files, add to workspace, verify compilation**

- [ ] **Step 2: Implement handler.rs — `/dispatch/{app_id}` → V8 dispatch per thread**

- [ ] **Step 3: Implement cache.rs — thread-local `HashMap<Uuid, Runtime>` + LRU eviction**

- [ ] **Step 4: Implement sync.rs — poll control plane for versions, pull bundles**

- [ ] **Step 5: Wire everything together in main.rs**

- [ ] **Step 6: Test manually: start control + worker, deploy an app, dispatch a request**

- [ ] **Step 7: Commit**

---

## Phase 4: Gateway (`appbase-gate`)

### Task 10: Scaffold gateway crate

**Files:**
- Create: `crates/gateway/Cargo.toml`
- Create: `crates/gateway/src/main.rs`
- Create: `crates/gateway/src/router.rs`
- Create: `crates/gateway/src/auth.rs`
- Create: `crates/gateway/src/enforce.rs`
- Create: `crates/gateway/src/proxy.rs`
- Create: `crates/gateway/src/sync.rs`
- Modify: `Cargo.toml` (workspace root)

Implement the full gateway binary: ntex HTTP server, route resolution from path/host, API key auth, rate limit + concurrency + quota enforcement (ported from existing platform enforcement code), HTTP proxy to workers (round-robin), routing table sync from control plane.

- [ ] **Step 1: Create all files, add to workspace, verify compilation**

- [ ] **Step 2: Implement sync.rs — poll control plane for routing table**

- [ ] **Step 3: Implement router.rs — path/host → app_id lookup**

- [ ] **Step 4: Implement auth.rs — API key validation against route entry**

- [ ] **Step 5: Implement enforce.rs — rate limit + concurrency + quota (port from platform)**

- [ ] **Step 6: Implement proxy.rs — HTTP proxy to workers (round-robin)**

- [ ] **Step 7: Wire everything together in main.rs**

- [ ] **Step 8: Test manually: start control + worker + gate, send request through gate**

- [ ] **Step 9: Commit**

---

## Phase 5: CLI + Integration

### Task 11: CLI commands

**Files:**
- Modify: `crates/cli/src/main.rs`

Add `appbase control`, `appbase gate`, `appbase worker`, `appbase deploy` subcommands. Each spawns the respective binary with parsed arguments.

- [ ] **Step 1: Add subcommands to CLI**

- [ ] **Step 2: Implement `appbase deploy` — compile + upload to control plane**

- [ ] **Step 3: Commit**

### Task 12: End-to-end integration test

**Files:**
- Create: `tests/e2e_platform.sh`

Script that starts all 3 components, creates an app, deploys code, sends a request through the full pipeline (gate → worker → V8 → response), verifies the result.

- [ ] **Step 1: Write the test script**

- [ ] **Step 2: Run and verify**

- [ ] **Step 3: Commit**

---

## Self-Review

**Spec coverage:**
- [x] Common types (AppRecord, RouteEntry, VersionMap, UsageReport, ControlEvent) → Task 1
- [x] VFS (BundleStore trait + LocalFs) → Task 2
- [x] Auth (control key, API key, Bearer extraction) → Task 3
- [x] Control plane scaffold → Task 5
- [x] Registry (app CRUD + versions + routes + usage) → Task 6
- [x] Admin API (all 8 routes) → Task 7
- [x] Internal API (versions, bundles, routes, usage) → Task 8
- [x] Worker (V8 per thread, dispatch, sync, cache) → Task 9
- [x] Gateway (auth, enforce, proxy, sync) → Task 10
- [x] CLI commands → Task 11
- [x] E2E test → Task 12
- [x] Database schema (CREATE TABLE in registry migrations) → Task 6

**Note:** Phases 3-5 (Tasks 9-12) are outlined at high level. Each will be detailed into bite-sized steps when we reach that phase — the implementation of Phases 1-2 may surface design changes that affect later phases.
