# PR 4 — Secrets / Vars CRUD + env Hydration

> **For agentic workers:** Use superpowers:subagent-driven-development or execute inline. Steps use `- [ ]` checkboxes.

**Goal:** Creators declare `[secrets]` + `[vars]` in `zeroship.toml`; the control plane stores them (secrets encrypted at rest via AES-256-GCM), exposes CRUD endpoints, and hydrates worker `EnvSnapshot` at deploy + request time. Unblocks `env.STRIPE_KEY` and every other creator-configured binding.

**Architecture:**
- New tables `app_vars` (plaintext) and `app_secrets` (nonce + ciphertext) keyed by `(app_id, key_name)`.
- Master encryption key from a CLI flag / env var on control plane startup (already have one — `--master-key`).
- Secrets use AES-256-GCM with a random 12-byte nonce per value; ciphertext includes the 16-byte auth tag.
- Worker fetches a merged `env` map when loading a bundle (alongside the bundle itself). Stored in-worker, used for every request through that bundle.
- CLI: `zeroship secret set KEY=value --app=<id>`, `secret list --app`, `secret rm KEY --app`, and `var …` symmetric commands.

**Tech stack:** Rust (ring or aes-gcm crate), Postgres via compio-postgres, CLI via existing `zeroship` binary. Worker's `env` delivery reuses the bundle-fetch channel.

**Spec-level decisions:**
- Secrets are write-only from the API surface — `GET /apps/:id/secrets` returns names + last-updated timestamps, never values. Rotating a secret = PUT + new ciphertext.
- Vars are read-write via plain API.
- `[bindings]` from the spec is deferred — v1 only needs `[secrets]` + `[vars]`. Platform bindings (`DB`, `KV`, `STORAGE`, `METER`) are still auto-injected.
- Encryption primitive: `aes-gcm` crate (well-audited, pure Rust). Nonce generated via `getrandom`.
- No secret versioning v1 — in-place rotation is fine for day-one.
- No per-env overrides (dev vs prod) v1 — each app has exactly one env.

---

## File Structure

**New files:**
- `crates/control/src/env_store.rs` — `EnvStore` type with CRUD + encryption (~200 LOC).
- `crates/control/src/env_handlers.rs` — HTTP handlers for `/apps/:id/{secrets,vars}` (~150 LOC).
- `crates/core/src/crypto.rs` — thin wrapper around `aes-gcm` for encrypt/decrypt (~50 LOC).
- `crates/cli/src/secrets.rs` — CLI subcommands (~100 LOC).

**Modified files:**
- `crates/control/src/registry.rs` — add migrations for `app_vars` + `app_secrets` tables, `AppState` passthrough.
- `crates/control/src/main.rs` — wire new handlers + env store, require master-key not empty when any secrets exist.
- `crates/worker/src/handler.rs` — switch `EnvSnapshot::empty()` to `EnvSnapshot::new(merged_env_for_app)`.
- `crates/worker/src/sync.rs` — fetch env map alongside bundle metadata; cache per-app.
- `crates/control/src/internal.rs` — add `GET /internal/apps/:id/env` endpoint (worker-authenticated) that returns the decrypted merged env as JSON.
- `crates/core/Cargo.toml` — add `aes-gcm` + `getrandom` deps.
- `crates/control/Cargo.toml` — depend on `env_store` module (no external deps beyond zeroship-core's crypto).
- `crates/cli/src/main.rs` — route `secret` + `var` subcommands.

**Out of scope:**
- Per-environment overrides (one env per app)
- Secret versioning / rotation history
- `[bindings]` declarative config (platform bindings stay hardcoded)
- Vault / KMS integration (a later enhancement; same API surface)

---

## Task S1: Crypto wrapper in `zeroship-core`

**Files:** Modify `crates/core/Cargo.toml`, create `crates/core/src/crypto.rs`.

- [ ] **Step 1: Add deps**

Add to `crates/core/Cargo.toml` under `[dependencies]`:

```toml
aes-gcm = "0.10"
rand = "0.8"
```

- [ ] **Step 2: Write the module**

Create `crates/core/src/crypto.rs`:

```rust
//! AES-256-GCM wrapper for secrets at rest.
//!
//! The stored format is a single blob: `nonce(12) || ciphertext || tag(16)`.
//! The tag is appended automatically by aes-gcm. We carry the nonce inline
//! so the caller only stores one `Vec<u8>` per secret.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};

const NONCE_LEN: usize = 12;

#[derive(Debug)]
pub enum CryptoError {
    BadKey,
    TooShort,
    Decrypt,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadKey   => write!(f, "master key must be 32 bytes after base64 decode"),
            Self::TooShort => write!(f, "ciphertext too short to contain a nonce"),
            Self::Decrypt  => write!(f, "decryption failed (wrong key, tampered, or corrupt)"),
        }
    }
}

impl std::error::Error for CryptoError {}

/// Derive a 32-byte AES key from a master-key string. We SHA-256 the
/// string so callers can pass any length. For production, supply a
/// pre-generated 32-byte key (base64-encoded) and verify length.
pub fn derive_key(master: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"zeroship-secret-key-v1");
    h.update(master.as_bytes());
    let out = h.finalize();
    let mut k = [0u8; 32];
    k.copy_from_slice(&out);
    k
}

pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let ct = cipher.encrypt(&nonce, plaintext).map_err(|_| CryptoError::Decrypt)?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(nonce.as_slice());
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn decrypt(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < NONCE_LEN { return Err(CryptoError::TooShort); }
    let (nonce_bytes, ct) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ct).map_err(|_| CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let key = derive_key("platform-key");
        let ct = encrypt(&key, b"sk_live_hunter2").unwrap();
        assert_eq!(decrypt(&key, &ct).unwrap(), b"sk_live_hunter2");
    }

    #[test]
    fn wrong_key_fails() {
        let k1 = derive_key("key1");
        let k2 = derive_key("key2");
        let ct = encrypt(&k1, b"secret").unwrap();
        assert!(matches!(decrypt(&k2, &ct), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = derive_key("k");
        let mut ct = encrypt(&key, b"hello").unwrap();
        *ct.last_mut().unwrap() ^= 0x01;
        assert!(matches!(decrypt(&key, &ct), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn nonce_uniqueness() {
        // Two encryptions of the same plaintext must produce different
        // ciphertexts (because nonces are random).
        let key = derive_key("k");
        let a = encrypt(&key, b"x").unwrap();
        let b = encrypt(&key, b"x").unwrap();
        assert_ne!(a, b);
    }
}
```

Export from `crates/core/src/lib.rs`: `pub mod crypto;`

- [ ] **Step 3: Verify**

`cargo test -p zeroship-core crypto 2>&1 | tail -5` — 4 tests pass.

- [ ] **Step 4: Commit**

```bash
git add crates/core/Cargo.toml crates/core/src/crypto.rs crates/core/src/lib.rs
git commit -m "core: AES-256-GCM wrapper for secrets at rest"
```

---

## Task S2: `EnvStore` with migrations + CRUD

**Files:** Create `crates/control/src/env_store.rs`, modify `crates/control/src/registry.rs` (migrations), `crates/control/src/lib.rs` / `main.rs` (module).

- [ ] **Step 1: Add migrations**

In `registry.rs::Registry::new`, after the `usage_history` index block, add:

```rust
conn.execute(
    "CREATE TABLE IF NOT EXISTS app_vars (
        app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
        key_name TEXT NOT NULL,
        value TEXT NOT NULL,
        updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        PRIMARY KEY (app_id, key_name)
    )",
    &[],
).await.map_err(|e| format!("migration: {e}"))?;

conn.execute(
    "CREATE TABLE IF NOT EXISTS app_secrets (
        app_id UUID NOT NULL REFERENCES apps(id) ON DELETE CASCADE,
        key_name TEXT NOT NULL,
        ciphertext BYTEA NOT NULL,
        updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        PRIMARY KEY (app_id, key_name)
    )",
    &[],
).await.map_err(|e| format!("migration: {e}"))?;
```

- [ ] **Step 2: Write `EnvStore`**

Create `crates/control/src/env_store.rs`:

```rust
//! Per-app secrets + vars store. Secrets are encrypted at rest with
//! AES-256-GCM using a key derived from the control-plane master key.

use uuid::Uuid;
use zeroship_core::crypto::{self, CryptoError};

use crate::registry::Registry;

#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    #[error("{0}")] Db(String),
    #[error("crypto: {0}")] Crypto(#[from] CryptoError),
    #[error("key must match /^[A-Z][A-Z0-9_]{{0,63}}$/, got '{0}'")] BadKey(String),
    #[error("not found: {0}")] NotFound(String),
}

pub struct EnvStore {
    registry: Registry,
    key: [u8; 32],
}

fn valid_key(k: &str) -> bool {
    if k.is_empty() || k.len() > 64 { return false; }
    let first = k.as_bytes()[0];
    if !first.is_ascii_uppercase() { return false; }
    k.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

impl EnvStore {
    pub fn new(registry: Registry, master_key: &str) -> Self {
        Self { registry, key: crypto::derive_key(master_key) }
    }

    pub async fn list_vars(&self, app_id: Uuid) -> Result<Vec<(String, String)>, EnvError> {
        let conn = self.registry.conn().await.map_err(|e| EnvError::Db(format!("{e:?}")))?;
        let rows = conn.query(
            "SELECT key_name, value FROM app_vars WHERE app_id = $1 ORDER BY key_name",
            &[&app_id],
        ).await.map_err(|e| EnvError::Db(e.to_string()))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows.iter() {
            let k: String = r.get("key_name").ok_or_else(|| EnvError::Db("missing key_name".into()))?;
            let v: String = r.get("value").ok_or_else(|| EnvError::Db("missing value".into()))?;
            out.push((k, v));
        }
        Ok(out)
    }

    pub async fn set_var(&self, app_id: Uuid, key: &str, value: &str) -> Result<(), EnvError> {
        if !valid_key(key) { return Err(EnvError::BadKey(key.into())); }
        let conn = self.registry.conn().await.map_err(|e| EnvError::Db(format!("{e:?}")))?;
        conn.execute(
            "INSERT INTO app_vars(app_id, key_name, value) VALUES($1, $2, $3)
             ON CONFLICT (app_id, key_name) DO UPDATE
                SET value = EXCLUDED.value, updated_at = NOW()",
            &[&app_id, &key, &value],
        ).await.map_err(|e| EnvError::Db(e.to_string()))?;
        Ok(())
    }

    pub async fn delete_var(&self, app_id: Uuid, key: &str) -> Result<bool, EnvError> {
        let conn = self.registry.conn().await.map_err(|e| EnvError::Db(format!("{e:?}")))?;
        let n = conn.execute(
            "DELETE FROM app_vars WHERE app_id = $1 AND key_name = $2",
            &[&app_id, &key],
        ).await.map_err(|e| EnvError::Db(e.to_string()))?;
        Ok(n > 0)
    }

    pub async fn list_secret_names(&self, app_id: Uuid) -> Result<Vec<String>, EnvError> {
        let conn = self.registry.conn().await.map_err(|e| EnvError::Db(format!("{e:?}")))?;
        let rows = conn.query(
            "SELECT key_name FROM app_secrets WHERE app_id = $1 ORDER BY key_name",
            &[&app_id],
        ).await.map_err(|e| EnvError::Db(e.to_string()))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows.iter() {
            out.push(r.get::<String>("key_name").ok_or_else(|| EnvError::Db("missing key_name".into()))?);
        }
        Ok(out)
    }

    pub async fn set_secret(&self, app_id: Uuid, key: &str, value: &str) -> Result<(), EnvError> {
        if !valid_key(key) { return Err(EnvError::BadKey(key.into())); }
        let ct = crypto::encrypt(&self.key, value.as_bytes())?;
        let conn = self.registry.conn().await.map_err(|e| EnvError::Db(format!("{e:?}")))?;
        conn.execute(
            "INSERT INTO app_secrets(app_id, key_name, ciphertext) VALUES($1, $2, $3)
             ON CONFLICT (app_id, key_name) DO UPDATE
                SET ciphertext = EXCLUDED.ciphertext, updated_at = NOW()",
            &[&app_id, &key, &ct],
        ).await.map_err(|e| EnvError::Db(e.to_string()))?;
        Ok(())
    }

    pub async fn delete_secret(&self, app_id: Uuid, key: &str) -> Result<bool, EnvError> {
        let conn = self.registry.conn().await.map_err(|e| EnvError::Db(format!("{e:?}")))?;
        let n = conn.execute(
            "DELETE FROM app_secrets WHERE app_id = $1 AND key_name = $2",
            &[&app_id, &key],
        ).await.map_err(|e| EnvError::Db(e.to_string()))?;
        Ok(n > 0)
    }

    /// Merged env for worker consumption. Decrypts every secret. Never expose
    /// this over the public API — only `/internal/apps/:id/env` to the worker.
    pub async fn merged_env(&self, app_id: Uuid) -> Result<serde_json::Map<String, serde_json::Value>, EnvError> {
        let mut map = serde_json::Map::new();
        for (k, v) in self.list_vars(app_id).await? {
            map.insert(k, serde_json::Value::String(v));
        }
        let conn = self.registry.conn().await.map_err(|e| EnvError::Db(format!("{e:?}")))?;
        let rows = conn.query(
            "SELECT key_name, ciphertext FROM app_secrets WHERE app_id = $1",
            &[&app_id],
        ).await.map_err(|e| EnvError::Db(e.to_string()))?;
        for r in rows.iter() {
            let k: String = r.get("key_name").ok_or_else(|| EnvError::Db("missing key_name".into()))?;
            let ct: Vec<u8> = r.get("ciphertext").ok_or_else(|| EnvError::Db("missing ciphertext".into()))?;
            let plain = crypto::decrypt(&self.key, &ct)?;
            let s = String::from_utf8_lossy(&plain).into_owned();
            map.insert(k, serde_json::Value::String(s));
        }
        Ok(map)
    }
}
```

- [ ] **Step 3: Expose `Registry::conn` as `pub(crate)`**

`registry.rs::Registry::conn` is currently private. Change to `pub(crate)`.

- [ ] **Step 4: Wire into `main.rs` / `AppState`**

In `crates/control/src/main.rs`, extend `AppState` with `env_store: EnvStore` and populate on startup. Also expose `pub mod env_store;` from `crates/control/src/lib.rs` (if one exists — otherwise via `main.rs`).

- [ ] **Step 5: `cargo check` and commit**

```bash
cargo check -p zeroship-control
git add crates/control/src/env_store.rs crates/control/src/registry.rs crates/control/src/main.rs
git commit -m "control: EnvStore — per-app vars + encrypted secrets CRUD"
```

---

## Task S3: HTTP handlers

**Files:** Create `crates/control/src/env_handlers.rs`, modify `crates/control/src/main.rs` routes, `crates/control/src/internal.rs`.

- [ ] **Step 1: Public handlers**

Create `crates/control/src/env_handlers.rs`:

```rust
//! Admin API for per-app vars + secret names. Mutations require master key
//! (same auth as the rest of the admin API). Secrets are write-only over
//! this surface — GET returns names, never values.

use std::sync::Arc;
use ntex::web::{self, types::{Json, Path, State}};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;
use crate::env_store::EnvError;

fn env_err_response(e: EnvError) -> web::HttpResponse {
    use EnvError::*;
    let (status, msg) = match &e {
        NotFound(_) => (404, e.to_string()),
        BadKey(_)   => (400, e.to_string()),
        _           => (500, e.to_string()),
    };
    web::HttpResponse::build(ntex::http::StatusCode::from_u16(status).unwrap())
        .json(&serde_json::json!({"error": msg}))
}

#[derive(Deserialize)] pub struct SetKv { pub key: String, pub value: String }

pub async fn list_vars(
    req: web::HttpRequest, path: Path<String>, state: State<Arc<AppState>>
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(id) = Uuid::parse_str(&path) else {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({"error":"bad app_id"}));
    };
    match state.env_store.list_vars(id).await {
        Ok(rows) => web::HttpResponse::Ok().json(&serde_json::json!({
            "vars": rows.into_iter().map(|(k,v)| serde_json::json!({"key":k,"value":v})).collect::<Vec<_>>()
        })),
        Err(e) => env_err_response(e),
    }
}

pub async fn set_var(
    req: web::HttpRequest, path: Path<String>, body: Json<SetKv>, state: State<Arc<AppState>>
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(id) = Uuid::parse_str(&path) else {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({"error":"bad app_id"}));
    };
    match state.env_store.set_var(id, &body.key, &body.value).await {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => env_err_response(e),
    }
}

pub async fn delete_var(
    req: web::HttpRequest, path: Path<(String, String)>, state: State<Arc<AppState>>
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let (id_s, key) = path.into_inner();
    let Ok(id) = Uuid::parse_str(&id_s) else {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({"error":"bad app_id"}));
    };
    match state.env_store.delete_var(id, &key).await {
        Ok(true)  => web::HttpResponse::NoContent().finish(),
        Ok(false) => web::HttpResponse::NotFound().json(&serde_json::json!({"error":"not found"})),
        Err(e)    => env_err_response(e),
    }
}

pub async fn list_secrets(
    req: web::HttpRequest, path: Path<String>, state: State<Arc<AppState>>
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(id) = Uuid::parse_str(&path) else {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({"error":"bad app_id"}));
    };
    match state.env_store.list_secret_names(id).await {
        Ok(names) => web::HttpResponse::Ok().json(&serde_json::json!({"secrets": names})),
        Err(e) => env_err_response(e),
    }
}

pub async fn set_secret(
    req: web::HttpRequest, path: Path<String>, body: Json<SetKv>, state: State<Arc<AppState>>
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(id) = Uuid::parse_str(&path) else {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({"error":"bad app_id"}));
    };
    match state.env_store.set_secret(id, &body.key, &body.value).await {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(e) => env_err_response(e),
    }
}

pub async fn delete_secret(
    req: web::HttpRequest, path: Path<(String, String)>, state: State<Arc<AppState>>
) -> web::HttpResponse {
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let (id_s, key) = path.into_inner();
    let Ok(id) = Uuid::parse_str(&id_s) else {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({"error":"bad app_id"}));
    };
    match state.env_store.delete_secret(id, &key).await {
        Ok(true)  => web::HttpResponse::NoContent().finish(),
        Ok(false) => web::HttpResponse::NotFound().json(&serde_json::json!({"error":"not found"})),
        Err(e)    => env_err_response(e),
    }
}
```

Change `api::check_admin_auth` visibility to `pub(crate)`.

- [ ] **Step 2: Worker-facing internal endpoint**

In `crates/control/src/internal.rs`, add:

```rust
pub async fn get_app_env(
    req: web::HttpRequest, path: Path<String>, state: State<Arc<AppState>>
) -> web::HttpResponse {
    // Control-key auth (workers authenticate with the same master key here).
    if let Some(r) = crate::api::check_admin_auth(&req, &state) { return r; }
    let Ok(id) = Uuid::parse_str(&path) else {
        return web::HttpResponse::BadRequest().json(&serde_json::json!({"error":"bad app_id"}));
    };
    match state.env_store.merged_env(id).await {
        Ok(map) => web::HttpResponse::Ok().json(&serde_json::Value::Object(map)),
        Err(e)  => web::HttpResponse::InternalServerError().json(&serde_json::json!({"error": e.to_string()})),
    }
}
```

- [ ] **Step 3: Register routes in `main.rs`**

```rust
.service(
    web::scope("/apps/{id}")
        .route("/vars",   web::get().to(env_handlers::list_vars))
        .route("/vars",   web::post().to(env_handlers::set_var))
        .route("/vars/{key}", web::delete().to(env_handlers::delete_var))
        .route("/secrets", web::get().to(env_handlers::list_secrets))
        .route("/secrets", web::post().to(env_handlers::set_secret))
        .route("/secrets/{key}", web::delete().to(env_handlers::delete_secret))
)
.route("/internal/apps/{id}/env", web::get().to(internal::get_app_env))
```

Place alongside the existing `/apps` routes.

- [ ] **Step 4: Build + smoke test**

```bash
cargo check -p zeroship-control
# Manual smoke:
#   curl -X POST -H 'Authorization: Bearer $KEY' -d '{"key":"FOO","value":"bar"}' \
#     $CONTROL/apps/<uuid>/vars
#   curl -H 'Authorization: Bearer $KEY' $CONTROL/apps/<uuid>/vars
```

- [ ] **Step 5: Commit**

```bash
git add crates/control/src/env_handlers.rs crates/control/src/main.rs crates/control/src/internal.rs crates/control/src/api.rs
git commit -m "control: CRUD endpoints for app vars + secrets"
```

---

## Task S4: Worker env hydration

**Files:** Modify `crates/worker/src/sync.rs`, `crates/worker/src/handler.rs`, `crates/worker/src/cache.rs`.

- [ ] **Step 1: Fetch env on bundle load**

In `sync.rs`, when the worker pulls a bundle for an app, also `GET /internal/apps/:id/env` and cache the resulting JSON map alongside the bundle. Add a field `env_json: String` to the cached entry in `cache.rs`.

- [ ] **Step 2: Inject into handler**

In `handler.rs`, replace `EnvSnapshot::empty()` with:

```rust
let env = match cache::get_env(&app_id) {
    Some(json) => EnvSnapshot::new(serde_json::from_str(&json).unwrap_or_default()),
    None => EnvSnapshot::empty(),
};
```

- [ ] **Step 3: Invalidation**

On bundle version change, refetch env too. For day-one, env TTL is the same as bundle TTL (refetch on version bump). Add an explicit `POST /internal/apps/:id/env/invalidate` endpoint returning 204 that workers listen to via the existing sync-poll mechanism (or piggyback on version bump — simpler).

- [ ] **Step 4: Verify via E2E**

Start control+worker, `POST /apps/:id/secrets {key:"FOO",value:"bar"}`, deploy a bundle that returns `JSON.stringify(env)`, hit the worker, confirm body contains `"FOO":"bar"`.

- [ ] **Step 5: Commit**

```bash
git add crates/worker/
git commit -m "worker: hydrate EnvSnapshot from control plane per app"
```

---

## Task S5: CLI commands

**Files:** Create `crates/cli/src/secrets.rs`, modify `crates/cli/src/main.rs`.

- [ ] **Step 1: Subcommand structure**

Add to `main.rs` the top-level `secret` + `var` subcommands. Shape mirrors existing `deploy`:

```
zeroship secret set   FOO=bar --app=<uuid> [--control=url] [--key=master]
zeroship secret list                --app=<uuid> ...
zeroship secret rm    FOO           --app=<uuid> ...
zeroship var    set   FEATURE_X=on  --app=<uuid> ...
zeroship var    list  --app=<uuid>
zeroship var    rm    FEATURE_X     --app=<uuid>
```

- [ ] **Step 2: Implement the client calls**

Thin wrappers over `reqwest` (or whatever the existing CLI uses — grep `deploy.rs` for the pattern). Each subcommand is 10-15 LOC.

- [ ] **Step 3: Test manually**

Round-trip: `zeroship secret set STRIPE_KEY=sk_test_x --app=<id>`; then `list` must show `STRIPE_KEY` (never the value); then `rm` returns OK.

- [ ] **Step 4: Commit**

```bash
git add crates/cli/
git commit -m "cli: zeroship secret / var subcommands"
```

---

## Task S6: Integration test

**Files:** Create `tests/e2e_secrets.sh` (new).

- [ ] **Step 1: Shell test**

Follow the style of `tests/e2e_platform.sh`. Sequence:

1. Boot control+worker with docker compose.
2. Create app.
3. POST a secret + a var.
4. Deploy a bundle that returns `JSON.stringify({FOO: env.FOO, SECRET: env.STRIPE_KEY})`.
5. GET through the gateway; assert both values visible.
6. Delete the secret; redeploy; assert absent.
7. `list_secrets` never returns the value.

- [ ] **Step 2: Commit**

```bash
git add tests/e2e_secrets.sh
git commit -m "test: E2E for secrets + vars end-to-end"
```

---

## Out-of-scope

- Stripe Connect integration (separate plan C).
- Secret versioning history.
- Per-env overrides.
- Vault/KMS integration.
- `[bindings]` section of zeroship.toml.
