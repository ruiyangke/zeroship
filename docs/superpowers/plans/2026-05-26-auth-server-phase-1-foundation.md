# Auth server — Phase 1 implementation plan: Foundation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up `crates/auth` and the `oryd/hydra` sidecar end-to-end. By the end of this phase the platform has a real OIDC IdP at `auth.zeroship.ai`, with first-party clients registered and JWK signing material in place — but no user identity flows yet. A smoke test exercises the hydra admin client against a real hydra binary; full OIDC ceremony is exercised in Phase 2.

**Architecture:** Two processes per "auth pod" — `oryd/hydra:v26.2.x` (public `:4444`, admin `127.0.0.1:4445`) and `crates/auth` (`:9092`). Both share one Postgres database (`hydra_*` and `auth.*` schemas). Hydra owns OIDC protocol surface; `crates/auth` owns identity, UX, user store, mailer, audit. The two communicate over hydra's admin API via a hand-rolled `cyper`-based Rust client (no `reqwest`/tokio).

**Tech Stack:**
- Rust (compio runtime, ntex web framework, `cyper` HTTP, `compio-postgres`)
- `oryd/hydra` v26.2.x (Docker image)
- PostgreSQL 15+
- `argon2`, `askama`, `serde_json`, `tracing` (project-standard)
- `lettre` (added in Phase 5; not used in Phase 1)

**Reference docs to keep open:**
- `docs/proposals/auth-server.md` — the proposal this plan implements.
- `crates/control/src/auth_*.rs` and `crates/control/src/oauth.rs` — the existing creator-auth code that gets retired in Phase 3.
- `crates/control/src/main.rs` — pattern for ntex routes + compio-postgres + tracing init.
- `crates/gateway/src/user_auth.rs` — the JWT-cookie path being replaced in Phase 3.

**Pre-launch posture (AGENTS.md):** no `@deprecated` aliases, no migration shims. We add new code; nothing yet calls it. Phase 3 deletes the old paths in one PR.

---

## Phase 1 task list (overview)

| # | Task | Files | Time |
|---|---|---|---|
| 1 | Create `crates/auth` skeleton | Cargo.toml, src/main.rs, src/config.rs, src/error.rs | 10 min |
| 2 | Register in workspace | `Cargo.toml`, root | 2 min |
| 3 | Wire ntex server with health endpoints | src/server.rs | 8 min |
| 4 | DB migrations module — `auth.*` schema | src/store/migrations.rs | 15 min |
| 5 | Hydra admin client — types module | src/hydra_client/types.rs | 15 min |
| 6 | Hydra admin client — transport | src/hydra_client/mod.rs | 12 min |
| 7 | Hydra admin client — login endpoints | src/hydra_client/login.rs | 10 min |
| 8 | Hydra admin client — consent endpoints | src/hydra_client/consent.rs | 10 min |
| 9 | Hydra admin client — logout endpoints | src/hydra_client/logout.rs | 8 min |
| 10 | Hydra admin client — clients CRUD | src/hydra_client/clients.rs | 10 min |
| 11 | Hydra admin client — JWKS admin | src/hydra_client/jwks.rs | 8 min |
| 12 | Hydra admin client — session admin | src/hydra_client/sessions.rs | 6 min |
| 13 | Clients config TOML parser | src/bootstrap/clients_config.rs | 10 min |
| 14 | Bootstrap — first-boot JWK generation | src/bootstrap/keys.rs | 12 min |
| 15 | Bootstrap — client reconciliation | src/bootstrap/mod.rs | 12 min |
| 16 | Docker compose entry for hydra | docker-compose.yml, ops/hydra.yaml | 15 min |
| 17 | Boot integration: wire everything in `main.rs` | src/main.rs | 8 min |
| 18 | Smoke test — boot real hydra + auth, exercise admin client | tests/hydra_client_smoke.rs | 25 min |
| 19 | Commit Phase 1 | (git) | 2 min |

Total bite-sized steps below: ~70.

---

## Task 1 · Create `crates/auth` skeleton

**Files:**
- Create: `crates/auth/Cargo.toml`
- Create: `crates/auth/src/main.rs`
- Create: `crates/auth/src/lib.rs`
- Create: `crates/auth/src/config.rs`
- Create: `crates/auth/src/error.rs`
- Create: `crates/auth/README.md`

- [ ] **Step 1.1: Create `crates/auth/Cargo.toml`**

```toml
[package]
name = "zeroship-auth"
version.workspace = true
edition.workspace = true
authors.workspace = true
license.workspace = true

[[bin]]
name = "zeroship-auth"
path = "src/main.rs"

[dependencies]
zeroship-core = { workspace = true }
compio = { workspace = true }
compio-postgres = { workspace = true }
ntex = { workspace = true }
cyper = { workspace = true }
http = { workspace = true }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
toml = { workspace = true }
tracing = { workspace = true }
url = { workspace = true }
uuid = { workspace = true, features = ["v4", "serde"] }
sha2 = { workspace = true }
hex = { workspace = true }
thiserror = { workspace = true }
clap = { workspace = true, features = ["derive", "env"] }

[dev-dependencies]
zeroship-test-utils = { workspace = true }     # only if it exists; otherwise omit
```

Verify these dependencies exist in the workspace `Cargo.toml`. If `cyper`, `toml`, `clap`, `thiserror`, `hex` are not workspace-declared, add them now via a single edit to the root `Cargo.toml`. Use the same versions other crates use.

- [ ] **Step 1.2: Create `crates/auth/src/main.rs`**

```rust
//! zeroship-auth — the OIDC IdP login UI + identity flows + hydra admin client.
//!
//! Companion process: `oryd/hydra` (OIDC kernel). See docs/proposals/auth-server.md.

use clap::Parser;

mod config;
mod error;

use config::AuthConfig;

#[compio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    zeroship_core::observability::init_tracing("zeroship-auth");

    let cfg = AuthConfig::parse();
    tracing::info!(addr = %cfg.addr, "starting zeroship-auth");

    // Server start lives in Task 3.
    let _ = cfg;
    Ok(())
}
```

- [ ] **Step 1.3: Create `crates/auth/src/lib.rs`** (so integration tests can import internals)

```rust
//! Library surface for integration tests. The binary is `main.rs`.

pub mod config;
pub mod error;
```

- [ ] **Step 1.4: Create `crates/auth/src/config.rs`**

```rust
//! Auth server configuration.

use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(name = "zeroship-auth")]
pub struct AuthConfig {
    /// Listen address.
    #[arg(long, env = "AUTH_ADDR", default_value = "0.0.0.0:9092")]
    pub addr: String,

    /// PostgreSQL DSN.
    #[arg(long, env = "AUTH_DB_URL")]
    pub db_url: String,

    /// Hydra admin base URL (loopback).
    #[arg(long, env = "AUTH_HYDRA_ADMIN", default_value = "http://127.0.0.1:4445")]
    pub hydra_admin: String,

    /// Hydra public base URL (issuer).
    #[arg(long, env = "AUTH_HYDRA_PUBLIC", default_value = "https://auth.zeroship.ai")]
    pub hydra_public: String,

    /// Path to clients config TOML.
    #[arg(long, env = "AUTH_CLIENTS_CONFIG", default_value = "/etc/zeroship/auth-clients.toml")]
    pub clients_config: String,

    /// Allow first-boot JWK + client creation. Without this, an empty hydra_jwk
    /// set is a fatal startup error.
    #[arg(long, env = "AUTH_BOOTSTRAP")]
    pub bootstrap: bool,

    /// Dev mode: drop the Secure flag on cookies. ONLY for localhost.
    #[arg(long, env = "AUTH_INSECURE_DEV")]
    pub insecure_dev: bool,
}
```

- [ ] **Step 1.5: Create `crates/auth/src/error.rs`**

```rust
//! Auth-wide error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("database: {0}")]
    Db(String),

    #[error("hydra admin: {0}")]
    Hydra(String),

    #[error("bootstrap: {0}")]
    Bootstrap(String),

    #[error("config: {0}")]
    Config(String),

    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, AuthError>;
```

- [ ] **Step 1.6: Create `crates/auth/README.md`**

```markdown
# zeroship-auth

The zeroship Identity Provider login surface + identity flows + hydra admin client.

Companion process: `oryd/hydra` (OIDC kernel). Public host: `auth.zeroship.ai`.

See `docs/proposals/auth-server.md` for the design.

## Build & run (local dev)

```bash
# 1. Start hydra (docker-compose up hydra)
# 2. Start auth:
AUTH_DB_URL=postgres://zeroship@localhost/zeroship \
AUTH_BOOTSTRAP=1 \
AUTH_INSECURE_DEV=1 \
cargo run -p zeroship-auth
```

## Important files
- `src/main.rs` — binary entrypoint.
- `src/hydra_client/` — hand-rolled admin-API client.
- `src/bootstrap/` — first-boot JWK + client reconciliation.
- `src/store/` — `auth.*` schema migrations and CRUD.
- `src/identity/` — password / federation / magic-link (Phase 2 onward).
- `src/ui/` — server-rendered HTML (Phase 2 onward).
```

- [ ] **Step 1.7: Build the empty crate**

Run: `cargo check -p zeroship-auth`
Expected: clean build (no warnings; `clippy` left for end-of-phase pass).

If `zeroship_core::observability::init_tracing` doesn't exist verbatim, grep `crates/core/src/observability.rs` and adapt the call to the actual function name used by `crates/control/src/main.rs`.

- [ ] **Step 1.8: Commit**

```bash
git add crates/auth/
git commit -m "auth: create crates/auth skeleton (binary + config + error)"
```

---

## Task 2 · Register `crates/auth` in the workspace

**Files:**
- Modify: `Cargo.toml` (root)

- [ ] **Step 2.1: Add `crates/auth` to workspace members**

Open root `Cargo.toml`. Find the `[workspace]` `members` glob or list. If it's a glob (`"crates/*"`) the crate is already picked up. If it's an explicit list, append `"crates/auth"`.

Verify with: `cargo check -p zeroship-auth` — should still pass.

- [ ] **Step 2.2: Commit (combine with Task 1 if needed)**

If you already committed in Task 1 and the workspace was glob-based, skip. Otherwise:

```bash
git add Cargo.toml
git commit -m "workspace: register crates/auth"
```

---

## Task 3 · ntex server + health endpoints

**Files:**
- Create: `crates/auth/src/server.rs`
- Modify: `crates/auth/src/main.rs`
- Modify: `crates/auth/src/lib.rs`

- [ ] **Step 3.1: Create `crates/auth/src/server.rs`**

```rust
//! ntex routes wiring. Bootstrap of routes/handlers happens here; the
//! handlers themselves live in `src/ui/`, `src/identity/`, etc. (added
//! in later tasks/phases).

use ntex::web;

use crate::config::AuthConfig;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(healthz).service(readyz);
}

#[web::get("/healthz")]
async fn healthz() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({ "ok": true }))
}

#[web::get("/readyz")]
async fn readyz() -> web::HttpResponse {
    // Phase 1 readiness is process-up. Phase 1 Task 17 wires PG + hydra reachability.
    web::HttpResponse::Ok().json(&serde_json::json!({ "ready": true }))
}

pub async fn run(cfg: AuthConfig) -> std::io::Result<()> {
    let addr = cfg.addr.clone();
    web::HttpServer::new(move || web::App::new().configure(configure))
        .bind(&addr)?
        .run()
        .await
}
```

- [ ] **Step 3.2: Wire `server::run` into `main.rs`**

Replace the body of `main()` so it calls into `server::run`:

```rust
use clap::Parser;

mod config;
mod error;
mod server;

#[compio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    zeroship_core::observability::init_tracing("zeroship-auth");

    let cfg = config::AuthConfig::parse();
    tracing::info!(addr = %cfg.addr, "starting zeroship-auth");

    server::run(cfg).await?;
    Ok(())
}
```

- [ ] **Step 3.3: Export `server` from `lib.rs`**

```rust
pub mod config;
pub mod error;
pub mod server;
```

- [ ] **Step 3.4: Smoke build**

Run: `cargo check -p zeroship-auth`
Expected: clean.

- [ ] **Step 3.5: Local boot sanity**

Run (in a throwaway terminal):
```bash
AUTH_DB_URL=postgres://zeroship@localhost/zeroship \
cargo run -p zeroship-auth
```
Then in another terminal:
```bash
curl -s http://localhost:9092/healthz
```
Expected: `{"ok":true}`.

Stop the process when done.

- [ ] **Step 3.6: Commit**

```bash
git add crates/auth/src/server.rs crates/auth/src/main.rs crates/auth/src/lib.rs
git commit -m "auth: ntex server + /healthz and /readyz endpoints"
```

---

## Task 4 · Database schema migrations

**Files:**
- Create: `crates/auth/src/store/mod.rs`
- Create: `crates/auth/src/store/migrations.rs`
- Modify: `crates/auth/src/lib.rs`

These are the tables from §5 of the proposal. We create them all in Phase 1 so subsequent phases just slot in. Each `CREATE TABLE` is idempotent (`IF NOT EXISTS`).

- [ ] **Step 4.1: Create `crates/auth/src/store/mod.rs`**

```rust
//! `auth.*` schema CRUD. Phase 1 only creates the migrations; per-table
//! CRUD modules are added in later phases as they're needed.

pub mod migrations;
```

- [ ] **Step 4.2: Create `crates/auth/src/store/migrations.rs`**

```rust
//! `auth.*` schema migrations. Run on every boot; statements are idempotent.

use compio_postgres::Client;

use crate::error::{AuthError, Result};

const STATEMENTS: &[&str] = &[
    // Schema
    "CREATE SCHEMA IF NOT EXISTS auth",
    // citext for case-insensitive emails
    "CREATE EXTENSION IF NOT EXISTS citext",
    "CREATE EXTENSION IF NOT EXISTS \"uuid-ossp\"",

    // 5.1 users
    "CREATE TABLE IF NOT EXISTS auth.users (
        id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
        email             CITEXT UNIQUE NOT NULL,
        email_verified_at TIMESTAMPTZ,
        name              TEXT NOT NULL,
        avatar_url        TEXT,
        password_hash     TEXT,
        locked_until      TIMESTAMPTZ,
        created_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        updated_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        last_login_at     TIMESTAMPTZ
    )",

    // 5.2 identities
    "CREATE TABLE IF NOT EXISTS auth.identities (
        id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
        user_id       UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
        provider      TEXT NOT NULL,
        subject       TEXT NOT NULL,
        email_at_link CITEXT,
        raw_profile   JSONB,
        linked_at     TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        UNIQUE (provider, subject)
    )",

    // 5.3 sessions (our IdP login session)
    "CREATE TABLE IF NOT EXISTS auth.sessions (
        id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
        user_id         UUID NOT NULL REFERENCES auth.users(id),
        auth_method     TEXT NOT NULL,
        amr             TEXT[] NOT NULL,
        acr             TEXT,
        auth_time       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        idle_expires_at TIMESTAMPTZ NOT NULL,
        abs_expires_at  TIMESTAMPTZ NOT NULL,
        revoked_at      TIMESTAMPTZ
    )",

    // 5.4 magic links
    "CREATE TABLE IF NOT EXISTS auth.magic_links (
        token_hash  BYTEA PRIMARY KEY,
        email       CITEXT NOT NULL,
        csrf_nonce  TEXT NOT NULL,
        purpose     TEXT NOT NULL,
        request_ip  INET,
        request_ua  TEXT,
        issued_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        expires_at  TIMESTAMPTZ NOT NULL,
        consumed_at TIMESTAMPTZ
    )",
    "CREATE INDEX IF NOT EXISTS auth_magic_email_idx ON auth.magic_links (email)",

    // 5.5 email verifications
    "CREATE TABLE IF NOT EXISTS auth.email_verifications (
        token_hash  BYTEA PRIMARY KEY,
        user_id     UUID NOT NULL REFERENCES auth.users(id) ON DELETE CASCADE,
        email       CITEXT NOT NULL,
        issued_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        expires_at  TIMESTAMPTZ NOT NULL,
        consumed_at TIMESTAMPTZ
    )",

    // 5.6 email suppressions
    "CREATE TABLE IF NOT EXISTS auth.email_suppressions (
        email         CITEXT PRIMARY KEY,
        reason        TEXT NOT NULL,
        suppressed_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        provider_msg  TEXT
    )",

    // 5.7 rate-limit buckets
    "CREATE TABLE IF NOT EXISTS auth.rate_limits (
        bucket_key TEXT PRIMARY KEY,
        tokens     REAL NOT NULL,
        updated_at TIMESTAMPTZ NOT NULL
    )",

    // 5.8 audit events
    "CREATE TABLE IF NOT EXISTS auth.audit_events (
        id          BIGSERIAL PRIMARY KEY,
        occurred_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
        event_type  TEXT NOT NULL,
        outcome     TEXT NOT NULL,
        user_id     UUID,
        client_id   TEXT,
        request_id  TEXT,
        ip          INET,
        user_agent  TEXT,
        auth_method TEXT,
        detail      JSONB
    )",
    "CREATE INDEX IF NOT EXISTS auth_audit_user_idx  ON auth.audit_events (user_id, occurred_at)",
    "CREATE INDEX IF NOT EXISTS auth_audit_event_idx ON auth.audit_events (event_type, occurred_at)",
];

/// Apply all migrations in order. Each statement is idempotent and safe to
/// re-run on every boot.
pub async fn migrate(conn: &Client) -> Result<()> {
    for stmt in STATEMENTS {
        conn.execute(*stmt, &[])
            .await
            .map_err(|e| AuthError::Db(format!("migration `{}`: {e}", first_line(stmt))))?;
    }
    Ok(())
}

fn first_line(stmt: &str) -> &str {
    stmt.lines().next().unwrap_or("").trim()
}
```

- [ ] **Step 4.3: Export `store` from `lib.rs`**

```rust
pub mod config;
pub mod error;
pub mod server;
pub mod store;
```

- [ ] **Step 4.4: Build**

Run: `cargo check -p zeroship-auth`
Expected: clean.

- [ ] **Step 4.5: Smoke against local PG**

If you have a local Postgres available with database `zeroship`:

```bash
psql postgres://zeroship@localhost/zeroship -c 'DROP SCHEMA IF EXISTS auth CASCADE'
```

Then write a one-off integration test or add to `tests/migrations_smoke.rs`:

```rust
// crates/auth/tests/migrations_smoke.rs
//! Migration smoke test — only runs if AUTH_DB_URL is set.
//! See `compio-postgres/tests/` for the harness pattern.

use compio_postgres::{connect, NoTls};
use zeroship_auth::store::migrations;

#[compio::test]
async fn migrations_apply_cleanly() {
    let dsn = match std::env::var("AUTH_DB_URL") {
        Ok(v) => v,
        Err(_) => { eprintln!("skipping (no AUTH_DB_URL)"); return; }
    };

    let (client, conn) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move { let _ = conn.run().await; }).detach();

    migrations::migrate(&client).await.expect("migrate");

    let rows = client
        .query("SELECT to_regclass('auth.users')::text AS t", &[])
        .await
        .expect("query");
    let table: Option<String> = rows[0].get("t");
    assert_eq!(table.as_deref(), Some("auth.users"));
}
```

Run: `AUTH_DB_URL=postgres://zeroship@localhost/zeroship cargo test -p zeroship-auth migrations_apply_cleanly -- --nocapture`
Expected: PASS (creates the `auth` schema).

- [ ] **Step 4.6: Commit**

```bash
git add crates/auth/src/store/ crates/auth/src/lib.rs crates/auth/tests/migrations_smoke.rs
git commit -m "auth: auth.* schema migrations (idempotent, eight tables)"
```

---

## Task 5 · Hydra admin client — types module

**Files:**
- Create: `crates/auth/src/hydra_client/mod.rs` (stub — populated in Task 6)
- Create: `crates/auth/src/hydra_client/types.rs`
- Modify: `crates/auth/src/lib.rs`

Hydra's admin API wire format. We define the structs we'll use across login/consent/logout/clients/jwks. Reference: hydra docs at <https://www.ory.com/docs/oauth2-oidc/custom-login-consent/flow> and ory/hydra-client-rust (which we do NOT vendor — too many deps).

- [ ] **Step 5.1: Create `crates/auth/src/hydra_client/mod.rs` (stub)**

```rust
//! Hand-rolled hydra admin API client.
//!
//! Hydra's auto-generated `ory-hydra-client` crate pulls in reqwest+tokio
//! which conflicts with the zero-tokio invariant. We hand-roll a small
//! cyper-based client.

pub mod types;
```

- [ ] **Step 5.2: Create `crates/auth/src/hydra_client/types.rs`**

```rust
//! Wire types for hydra admin API.

use serde::{Deserialize, Serialize};

// ─── Login challenge ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub challenge: String,
    pub skip: bool,
    /// Empty string when skip = false (no subject yet known).
    pub subject: String,
    pub client: OAuth2Client,
    pub request_url: String,
    pub requested_scope: Vec<String>,
    pub requested_access_token_audience: Vec<String>,
    pub session_id: Option<String>,
    pub oidc_context: Option<serde_json::Value>,
}

#[derive(Debug, Serialize, Default)]
pub struct AcceptLoginRequest {
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub remember: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")] pub remember_for: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub acr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub amr: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")] pub context: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")] pub force_subject_identifier: Option<String>,
}

#[derive(Debug, Serialize, Default)]
pub struct RejectRequest {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub error_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub status_code: Option<i32>,
}

// ─── Consent challenge ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ConsentRequest {
    pub challenge: String,
    pub skip: bool,
    pub subject: String,
    pub client: OAuth2Client,
    pub requested_scope: Vec<String>,
    pub requested_access_token_audience: Vec<String>,
    pub login_session_id: Option<String>,
    pub context: Option<serde_json::Value>,
    pub oidc_context: Option<serde_json::Value>,
    pub request_url: String,
}

#[derive(Debug, Serialize, Default)]
pub struct AcceptConsentRequest {
    pub grant_scope: Vec<String>,
    pub grant_access_token_audience: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub remember: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")] pub remember_for: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub session: Option<ConsentSession>,
}

#[derive(Debug, Serialize, Default)]
pub struct ConsentSession {
    #[serde(skip_serializing_if = "Option::is_none")] pub id_token: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")] pub access_token: Option<serde_json::Value>,
}

// ─── Logout challenge ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LogoutRequest {
    pub subject: String,
    pub sid: String,
    pub request_url: String,
    pub rp_initiated: bool,
    pub client: Option<OAuth2Client>,
}

// ─── Generic redirect envelope ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RedirectResponse {
    pub redirect_to: String,
}

// ─── Client (OAuth2Client) ───────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OAuth2Client {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub client_secret: Option<String>,
    #[serde(default)] pub grant_types: Vec<String>,
    #[serde(default)] pub response_types: Vec<String>,
    #[serde(default)] pub redirect_uris: Vec<String>,
    #[serde(default)] pub post_logout_redirect_uris: Vec<String>,
    pub scope: String,
    pub token_endpoint_auth_method: String,
    pub subject_type: String,
    #[serde(skip_serializing_if = "Option::is_none")] pub access_token_strategy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub id_token_signed_response_alg: Option<String>,
    #[serde(default)] pub audience: Vec<String>,
    #[serde(default)] pub skip_consent: bool,
    #[serde(default)] pub require_consent: bool,
    #[serde(default)] pub require_logout_consent: bool,
    #[serde(skip_serializing_if = "Option::is_none")] pub frontchannel_logout_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")] pub backchannel_logout_uri: Option<String>,
}

// ─── JWK admin ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CreateJsonWebKeySetRequest {
    pub alg: String,
    pub use_: String,        // serialised as "use" — see custom serde below
    pub kid: String,
}

#[derive(Debug, Deserialize)]
pub struct JsonWebKeySet {
    pub keys: Vec<serde_json::Value>,
}
```

Note: hydra's admin endpoint for keys is `POST /admin/keys/{set}` with body `{ alg, use, kid }`. The `use` field is a Rust keyword; we serialise it manually. Use this manual `Serialize` impl for `CreateJsonWebKeySetRequest`:

```rust
impl Serialize for CreateJsonWebKeySetRequest {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut o = s.serialize_struct("CreateJsonWebKeySetRequest", 3)?;
        o.serialize_field("alg", &self.alg)?;
        o.serialize_field("use", &self.use_)?;
        o.serialize_field("kid", &self.kid)?;
        o.end()
    }
}
```

Remove the `#[derive(Serialize)]` on `CreateJsonWebKeySetRequest` when adding the manual impl.

- [ ] **Step 5.3: Export from `lib.rs`**

```rust
pub mod config;
pub mod error;
pub mod hydra_client;
pub mod server;
pub mod store;
```

- [ ] **Step 5.4: Build**

Run: `cargo check -p zeroship-auth`
Expected: clean.

- [ ] **Step 5.5: Commit**

```bash
git add crates/auth/src/hydra_client/ crates/auth/src/lib.rs
git commit -m "auth: hydra admin client — wire-format types"
```

---

## Task 6 · Hydra admin client — transport

**Files:**
- Modify: `crates/auth/src/hydra_client/mod.rs`

The thin HTTP client. `cyper` is the project's existing compio HTTP client (used by `crates/control/src/oauth.rs`). Pattern matches that file.

- [ ] **Step 6.1: Rewrite `crates/auth/src/hydra_client/mod.rs`**

```rust
//! Hand-rolled hydra admin API client over `cyper` (compio HTTP).

pub mod types;

use serde::{de::DeserializeOwned, Serialize};

use crate::error::{AuthError, Result};

#[derive(Clone)]
pub struct HydraAdmin {
    base: String,
    client: cyper::Client,
}

impl std::fmt::Debug for HydraAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HydraAdmin").field("base", &self.base).finish()
    }
}

impl HydraAdmin {
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into(), client: cyper::Client::new() }
    }

    pub(crate) fn url(&self, path: &str) -> String {
        let trimmed = self.base.trim_end_matches('/');
        format!("{trimmed}{path}")
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let mut url = self.url(path);
        if !query.is_empty() {
            let q = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query.iter().copied())
                .finish();
            url.push('?');
            url.push_str(&q);
        }
        let res = self.client
            .request(http::Method::GET, url)
            .map_err(|e| AuthError::Hydra(format!("build GET {path}: {e}")))?
            .send().await
            .map_err(|e| AuthError::Hydra(format!("GET {path}: {e}")))?;
        finish::<T>(res, path).await
    }

    pub(crate) async fn put<B: Serialize, T: DeserializeOwned>(
        &self, path: &str, query: &[(&str, &str)], body: &B,
    ) -> Result<T> {
        let mut url = self.url(path);
        if !query.is_empty() {
            let q = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query.iter().copied())
                .finish();
            url.push('?');
            url.push_str(&q);
        }
        let body_bytes = serde_json::to_vec(body)
            .map_err(|e| AuthError::Hydra(format!("PUT {path} encode: {e}")))?;
        let res = self.client
            .request(http::Method::PUT, url)
            .map_err(|e| AuthError::Hydra(format!("build PUT {path}: {e}")))?
            .header("content-type", "application/json")
            .map_err(|e| AuthError::Hydra(format!("PUT {path} header: {e}")))?
            .body(body_bytes)
            .send().await
            .map_err(|e| AuthError::Hydra(format!("PUT {path}: {e}")))?;
        finish::<T>(res, path).await
    }

    pub(crate) async fn post<B: Serialize, T: DeserializeOwned>(
        &self, path: &str, body: &B,
    ) -> Result<T> {
        let url = self.url(path);
        let body_bytes = serde_json::to_vec(body)
            .map_err(|e| AuthError::Hydra(format!("POST {path} encode: {e}")))?;
        let res = self.client
            .request(http::Method::POST, url)
            .map_err(|e| AuthError::Hydra(format!("build POST {path}: {e}")))?
            .header("content-type", "application/json")
            .map_err(|e| AuthError::Hydra(format!("POST {path} header: {e}")))?
            .body(body_bytes)
            .send().await
            .map_err(|e| AuthError::Hydra(format!("POST {path}: {e}")))?;
        finish::<T>(res, path).await
    }

    pub(crate) async fn delete(&self, path: &str, query: &[(&str, &str)]) -> Result<()> {
        let mut url = self.url(path);
        if !query.is_empty() {
            let q = url::form_urlencoded::Serializer::new(String::new())
                .extend_pairs(query.iter().copied())
                .finish();
            url.push('?');
            url.push_str(&q);
        }
        let res = self.client
            .request(http::Method::DELETE, url)
            .map_err(|e| AuthError::Hydra(format!("build DELETE {path}: {e}")))?
            .send().await
            .map_err(|e| AuthError::Hydra(format!("DELETE {path}: {e}")))?;
        let status = res.status().as_u16();
        if !(200..300).contains(&status) {
            let body = res.text().await.unwrap_or_else(|_| "<no body>".into());
            return Err(AuthError::Hydra(format!("DELETE {path} → {status}: {body}")));
        }
        Ok(())
    }
}

async fn finish<T: DeserializeOwned>(res: cyper::Response, path: &str) -> Result<T> {
    let status = res.status().as_u16();
    let body = res.text().await
        .map_err(|e| AuthError::Hydra(format!("read body {path}: {e}")))?;
    if !(200..300).contains(&status) {
        return Err(AuthError::Hydra(format!("{path} → {status}: {body}")));
    }
    serde_json::from_str(&body)
        .map_err(|e| AuthError::Hydra(format!("decode {path}: {e}\nbody: {body}")))
}
```

If `cyper::Response::text()` doesn't exist on the project's pinned version, mirror what `crates/control/src/oauth.rs` does (it has the same pattern).

- [ ] **Step 6.2: Build**

Run: `cargo check -p zeroship-auth`
Expected: clean.

- [ ] **Step 6.3: Commit**

```bash
git add crates/auth/src/hydra_client/mod.rs
git commit -m "auth: hydra admin client — transport (GET/PUT/POST/DELETE over cyper)"
```

---

## Task 7 · Hydra admin client — login endpoints

**Files:**
- Create: `crates/auth/src/hydra_client/login.rs`
- Modify: `crates/auth/src/hydra_client/mod.rs` (add `pub mod login;`)

- [ ] **Step 7.1: Create `crates/auth/src/hydra_client/login.rs`**

```rust
//! Login challenge admin endpoints.

use crate::error::Result;
use crate::hydra_client::types::{AcceptLoginRequest, LoginRequest, RedirectResponse, RejectRequest};
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    pub async fn get_login(&self, challenge: &str) -> Result<LoginRequest> {
        self.get("/admin/oauth2/auth/requests/login", &[("login_challenge", challenge)]).await
    }

    pub async fn accept_login(&self, challenge: &str, body: &AcceptLoginRequest) -> Result<RedirectResponse> {
        self.put("/admin/oauth2/auth/requests/login/accept", &[("login_challenge", challenge)], body).await
    }

    pub async fn reject_login(&self, challenge: &str, body: &RejectRequest) -> Result<RedirectResponse> {
        self.put("/admin/oauth2/auth/requests/login/reject", &[("login_challenge", challenge)], body).await
    }
}
```

- [ ] **Step 7.2: Register the module**

In `crates/auth/src/hydra_client/mod.rs` add `pub mod login;` next to `pub mod types;`.

- [ ] **Step 7.3: Build**

Run: `cargo check -p zeroship-auth`
Expected: clean.

- [ ] **Step 7.4: Commit**

```bash
git add crates/auth/src/hydra_client/login.rs crates/auth/src/hydra_client/mod.rs
git commit -m "auth: hydra admin client — login challenge endpoints"
```

---

## Task 8 · Hydra admin client — consent endpoints

**Files:**
- Create: `crates/auth/src/hydra_client/consent.rs`
- Modify: `crates/auth/src/hydra_client/mod.rs`

- [ ] **Step 8.1: Create `crates/auth/src/hydra_client/consent.rs`**

```rust
//! Consent challenge admin endpoints.

use crate::error::Result;
use crate::hydra_client::types::{AcceptConsentRequest, ConsentRequest, RedirectResponse, RejectRequest};
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    pub async fn get_consent(&self, challenge: &str) -> Result<ConsentRequest> {
        self.get("/admin/oauth2/auth/requests/consent", &[("consent_challenge", challenge)]).await
    }

    pub async fn accept_consent(&self, challenge: &str, body: &AcceptConsentRequest) -> Result<RedirectResponse> {
        self.put("/admin/oauth2/auth/requests/consent/accept", &[("consent_challenge", challenge)], body).await
    }

    pub async fn reject_consent(&self, challenge: &str, body: &RejectRequest) -> Result<RedirectResponse> {
        self.put("/admin/oauth2/auth/requests/consent/reject", &[("consent_challenge", challenge)], body).await
    }
}
```

- [ ] **Step 8.2: Register `pub mod consent;` in `hydra_client/mod.rs`**

- [ ] **Step 8.3: Build + commit**

```bash
cargo check -p zeroship-auth
git add crates/auth/src/hydra_client/consent.rs crates/auth/src/hydra_client/mod.rs
git commit -m "auth: hydra admin client — consent challenge endpoints"
```

---

## Task 9 · Hydra admin client — logout endpoints

**Files:**
- Create: `crates/auth/src/hydra_client/logout.rs`
- Modify: `crates/auth/src/hydra_client/mod.rs`

- [ ] **Step 9.1: Create `crates/auth/src/hydra_client/logout.rs`**

```rust
//! Logout challenge admin endpoints.

use crate::error::Result;
use crate::hydra_client::types::{LogoutRequest, RedirectResponse};
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    pub async fn get_logout(&self, challenge: &str) -> Result<LogoutRequest> {
        self.get("/admin/oauth2/auth/requests/logout", &[("logout_challenge", challenge)]).await
    }

    pub async fn accept_logout(&self, challenge: &str) -> Result<RedirectResponse> {
        self.put("/admin/oauth2/auth/requests/logout/accept",
                 &[("logout_challenge", challenge)],
                 &serde_json::json!({})).await
    }
}
```

- [ ] **Step 9.2: Register + build + commit**

```bash
# add `pub mod logout;` to hydra_client/mod.rs
cargo check -p zeroship-auth
git add crates/auth/src/hydra_client/logout.rs crates/auth/src/hydra_client/mod.rs
git commit -m "auth: hydra admin client — logout challenge endpoints"
```

---

## Task 10 · Hydra admin client — clients CRUD

**Files:**
- Create: `crates/auth/src/hydra_client/clients.rs`
- Modify: `crates/auth/src/hydra_client/mod.rs`

- [ ] **Step 10.1: Create `crates/auth/src/hydra_client/clients.rs`**

```rust
//! OAuth2 client CRUD via hydra admin API.

use crate::error::{AuthError, Result};
use crate::hydra_client::types::OAuth2Client;
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    pub async fn create_client(&self, client: &OAuth2Client) -> Result<OAuth2Client> {
        self.post("/admin/clients", client).await
    }

    pub async fn get_client(&self, client_id: &str) -> Result<Option<OAuth2Client>> {
        match self.get::<OAuth2Client>(&format!("/admin/clients/{client_id}"), &[]).await {
            Ok(c) => Ok(Some(c)),
            Err(AuthError::Hydra(msg)) if msg.contains("→ 404") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn update_client(&self, client: &OAuth2Client) -> Result<OAuth2Client> {
        let path = format!("/admin/clients/{}", client.client_id);
        self.put(&path, &[], client).await
    }

    pub async fn delete_client(&self, client_id: &str) -> Result<()> {
        self.delete(&format!("/admin/clients/{client_id}"), &[]).await
    }
}
```

- [ ] **Step 10.2: Register + build + commit**

```bash
# add `pub mod clients;` to hydra_client/mod.rs
cargo check -p zeroship-auth
git add crates/auth/src/hydra_client/clients.rs crates/auth/src/hydra_client/mod.rs
git commit -m "auth: hydra admin client — OAuth2 client CRUD"
```

---

## Task 11 · Hydra admin client — JWKS admin

**Files:**
- Create: `crates/auth/src/hydra_client/jwks.rs`
- Modify: `crates/auth/src/hydra_client/mod.rs`

- [ ] **Step 11.1: Create `crates/auth/src/hydra_client/jwks.rs`**

```rust
//! JWK set admin endpoints (used by bootstrap + rotation).

use crate::error::{AuthError, Result};
use crate::hydra_client::types::{CreateJsonWebKeySetRequest, JsonWebKeySet};
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    pub async fn get_jwks(&self, set: &str) -> Result<Option<JsonWebKeySet>> {
        match self.get::<JsonWebKeySet>(&format!("/admin/keys/{set}"), &[]).await {
            Ok(j) => Ok(Some(j)),
            Err(AuthError::Hydra(msg)) if msg.contains("→ 404") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn create_jwk(&self, set: &str, alg: &str) -> Result<JsonWebKeySet> {
        let kid = format!("kid_{}", uuid::Uuid::new_v4().simple());
        let body = CreateJsonWebKeySetRequest { alg: alg.to_string(), use_: "sig".into(), kid };
        self.post(&format!("/admin/keys/{set}"), &body).await
    }

    pub async fn delete_jwk(&self, set: &str, kid: &str) -> Result<()> {
        self.delete(&format!("/admin/keys/{set}/{kid}"), &[]).await
    }
}
```

Note on `kid`: per the proposal §7.5 the long-term plan is RFC 7638 thumbprint kids, but hydra computes the JWK material and its kid internally — the value we send is ignored. We use a UUID-suffixed string for log readability; hydra will respond with its own kid.

- [ ] **Step 11.2: Register + build + commit**

```bash
# add `pub mod jwks;` to hydra_client/mod.rs
cargo check -p zeroship-auth
git add crates/auth/src/hydra_client/jwks.rs crates/auth/src/hydra_client/mod.rs
git commit -m "auth: hydra admin client — JWKS admin endpoints"
```

---

## Task 12 · Hydra admin client — session admin

**Files:**
- Create: `crates/auth/src/hydra_client/sessions.rs`
- Modify: `crates/auth/src/hydra_client/mod.rs`

- [ ] **Step 12.1: Create `crates/auth/src/hydra_client/sessions.rs`**

```rust
//! Session admin endpoints (sign-out-everywhere).

use crate::error::Result;
use crate::hydra_client::HydraAdmin;

impl HydraAdmin {
    /// Invalidate hydra's login session for the given subject (typed_id usr_…).
    /// Triggers front/back-channel logout fan-out to RPs.
    pub async fn delete_login_sessions(&self, subject: &str) -> Result<()> {
        self.delete("/admin/oauth2/auth/sessions/login", &[("subject", subject)]).await
    }
}
```

- [ ] **Step 12.2: Register + build + commit**

```bash
# add `pub mod sessions;` to hydra_client/mod.rs
cargo check -p zeroship-auth
git add crates/auth/src/hydra_client/sessions.rs crates/auth/src/hydra_client/mod.rs
git commit -m "auth: hydra admin client — session admin (sign-out-everywhere)"
```

---

## Task 13 · Clients config TOML parser

**Files:**
- Create: `crates/auth/src/bootstrap/mod.rs` (stub — populated in Task 15)
- Create: `crates/auth/src/bootstrap/clients_config.rs`
- Modify: `crates/auth/src/lib.rs`

The TOML the operator edits to declare OIDC clients. Reconciled against hydra's admin API at every boot.

- [ ] **Step 13.1: Create `crates/auth/src/bootstrap/mod.rs` (stub)**

```rust
//! First-boot bootstrap: JWK generation + OIDC client reconciliation.

pub mod clients_config;
pub mod keys;
```

- [ ] **Step 13.2: Create `crates/auth/src/bootstrap/clients_config.rs`**

```rust
//! Parse and validate the operator-provided OIDC clients config.
//!
//! TOML shape:
//!
//! ```toml
//! [[client]]
//! client_id = "console.zeroship.ai"
//! client_name = "zeroship Console"
//! redirect_uris = ["https://console.zeroship.ai/auth/callback"]
//! post_logout_redirect_uris = ["https://console.zeroship.ai/"]
//! scope = "openid offline_access email profile"
//! token_endpoint_auth_method = "client_secret_basic"
//! access_token_strategy = "jwt"
//! id_token_signed_response_alg = "EdDSA"
//! audience = ["https://api.zeroship.ai"]
//! first_party = true
//! ```

use serde::Deserialize;

use crate::error::{AuthError, Result};
use crate::hydra_client::types::OAuth2Client;

#[derive(Debug, Deserialize)]
pub struct ClientsConfig {
    #[serde(default, rename = "client")]
    pub clients: Vec<ClientEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ClientEntry {
    pub client_id: String,
    pub client_name: Option<String>,
    /// Auto-generated if absent for confidential clients.
    pub client_secret: Option<String>,
    #[serde(default = "default_grant_types")] pub grant_types: Vec<String>,
    #[serde(default = "default_response_types")] pub response_types: Vec<String>,
    #[serde(default)] pub redirect_uris: Vec<String>,
    #[serde(default)] pub post_logout_redirect_uris: Vec<String>,
    pub scope: String,
    #[serde(default = "default_auth_method")] pub token_endpoint_auth_method: String,
    #[serde(default = "default_subject_type")] pub subject_type: String,
    pub access_token_strategy: Option<String>,
    pub id_token_signed_response_alg: Option<String>,
    #[serde(default)] pub audience: Vec<String>,
    /// First-party flag. When true: skip_consent=true, require_consent=false,
    /// require_logout_consent=false.
    #[serde(default)] pub first_party: bool,
    pub frontchannel_logout_uri: Option<String>,
    pub backchannel_logout_uri: Option<String>,
}

fn default_grant_types()    -> Vec<String> { vec!["authorization_code".into(), "refresh_token".into()] }
fn default_response_types() -> Vec<String> { vec!["code".into()] }
fn default_auth_method()    -> String      { "client_secret_basic".into() }
fn default_subject_type()   -> String      { "public".into() }

impl ClientsConfig {
    pub fn from_path(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| AuthError::Bootstrap(format!("read {path}: {e}")))?;
        let cfg: ClientsConfig = toml::from_str(&raw)
            .map_err(|e| AuthError::Bootstrap(format!("parse {path}: {e}")))?;
        Ok(cfg)
    }
}

impl ClientEntry {
    pub fn to_oauth2_client(&self) -> OAuth2Client {
        OAuth2Client {
            client_id: self.client_id.clone(),
            client_name: self.client_name.clone(),
            client_secret: self.client_secret.clone(),
            grant_types: self.grant_types.clone(),
            response_types: self.response_types.clone(),
            redirect_uris: self.redirect_uris.clone(),
            post_logout_redirect_uris: self.post_logout_redirect_uris.clone(),
            scope: self.scope.clone(),
            token_endpoint_auth_method: self.token_endpoint_auth_method.clone(),
            subject_type: self.subject_type.clone(),
            access_token_strategy: self.access_token_strategy.clone(),
            id_token_signed_response_alg: self.id_token_signed_response_alg.clone(),
            audience: self.audience.clone(),
            skip_consent: self.first_party,
            require_consent: !self.first_party,
            require_logout_consent: false,
            frontchannel_logout_uri: self.frontchannel_logout_uri.clone(),
            backchannel_logout_uri: self.backchannel_logout_uri.clone(),
        }
    }
}
```

- [ ] **Step 13.3: Write a parse-and-mapping test**

Create `crates/auth/tests/clients_config_test.rs`:

```rust
use zeroship_auth::bootstrap::clients_config::ClientsConfig;

#[test]
fn parses_minimal_first_party_client() {
    let toml = r#"
        [[client]]
        client_id = "console.zeroship.ai"
        redirect_uris = ["https://console.zeroship.ai/auth/callback"]
        scope = "openid offline_access email profile"
        first_party = true
    "#;
    let cfg: ClientsConfig = toml::from_str(toml).expect("parse");
    assert_eq!(cfg.clients.len(), 1);
    let oc = cfg.clients[0].to_oauth2_client();
    assert_eq!(oc.client_id, "console.zeroship.ai");
    assert!(oc.skip_consent);
    assert!(!oc.require_consent);
    assert_eq!(oc.grant_types, vec!["authorization_code", "refresh_token"]);
    assert_eq!(oc.response_types, vec!["code"]);
}
```

- [ ] **Step 13.4: Export bootstrap from `lib.rs`**

```rust
pub mod bootstrap;
pub mod config;
pub mod error;
pub mod hydra_client;
pub mod server;
pub mod store;
```

- [ ] **Step 13.5: Run the test**

Run: `cargo test -p zeroship-auth parses_minimal_first_party_client`
Expected: PASS.

- [ ] **Step 13.6: Commit**

```bash
git add crates/auth/src/bootstrap/ crates/auth/src/lib.rs crates/auth/tests/clients_config_test.rs
git commit -m "auth: bootstrap — clients config TOML parser"
```

---

## Task 14 · Bootstrap — first-boot JWK generation

**Files:**
- Create: `crates/auth/src/bootstrap/keys.rs`

- [ ] **Step 14.1: Create `crates/auth/src/bootstrap/keys.rs`**

```rust
//! First-boot JWK generation. Called only when `--bootstrap` is set
//! and the relevant hydra key sets are empty.

use crate::error::Result;
use crate::hydra_client::HydraAdmin;

pub const ID_TOKEN_SET: &str = "hydra.openid.id-token";
pub const ACCESS_TOKEN_SET: &str = "hydra.jwt.access-token";

/// Ensures hydra has at least one signing key in each set. Idempotent:
/// returns immediately if the set is non-empty.
pub async fn ensure_signing_keys(admin: &HydraAdmin) -> Result<()> {
    ensure_set(admin, ID_TOKEN_SET, &["EdDSA", "RS256"]).await?;
    ensure_set(admin, ACCESS_TOKEN_SET, &["EdDSA"]).await?;
    Ok(())
}

async fn ensure_set(admin: &HydraAdmin, set: &str, algs: &[&str]) -> Result<()> {
    let existing = admin.get_jwks(set).await?;
    let count = existing.as_ref().map(|j| j.keys.len()).unwrap_or(0);
    if count > 0 {
        tracing::info!(set, count, "hydra key set already populated; skipping");
        return Ok(());
    }
    for alg in algs {
        tracing::info!(set, alg, "creating hydra JWK");
        admin.create_jwk(set, alg).await?;
    }
    Ok(())
}
```

- [ ] **Step 14.2: Build**

Run: `cargo check -p zeroship-auth`
Expected: clean.

- [ ] **Step 14.3: Commit**

```bash
git add crates/auth/src/bootstrap/keys.rs
git commit -m "auth: bootstrap — first-boot JWK generation"
```

---

## Task 15 · Bootstrap — client reconciliation

**Files:**
- Modify: `crates/auth/src/bootstrap/mod.rs`

- [ ] **Step 15.1: Rewrite `crates/auth/src/bootstrap/mod.rs`**

```rust
//! First-boot bootstrap orchestrator.
//!
//! Order on startup:
//!   1. `ensure_signing_keys` — only runs if `--bootstrap`.
//!   2. `reconcile_clients`    — runs always.
//!
//! Client reconciliation is upsert-style: declared clients are created or
//! updated; clients in hydra not in the config are LEFT ALONE (we don't
//! want a bootstrap loop to nuke a manually-registered third-party client).

pub mod clients_config;
pub mod keys;

use crate::error::Result;
use crate::hydra_client::HydraAdmin;
use clients_config::ClientsConfig;

pub async fn run(admin: &HydraAdmin, allow_bootstrap: bool, clients_config_path: &str) -> Result<()> {
    if allow_bootstrap {
        keys::ensure_signing_keys(admin).await?;
    } else if keys_empty(admin).await? {
        return Err(crate::error::AuthError::Bootstrap(
            "hydra signing-key sets are empty; restart with --bootstrap to generate".into(),
        ));
    }

    reconcile_clients(admin, clients_config_path).await
}

async fn keys_empty(admin: &HydraAdmin) -> Result<bool> {
    let a = admin.get_jwks(keys::ID_TOKEN_SET).await?;
    let b = admin.get_jwks(keys::ACCESS_TOKEN_SET).await?;
    Ok(a.map(|j| j.keys.is_empty()).unwrap_or(true) ||
       b.map(|j| j.keys.is_empty()).unwrap_or(true))
}

async fn reconcile_clients(admin: &HydraAdmin, path: &str) -> Result<()> {
    let cfg = ClientsConfig::from_path(path)?;
    for entry in &cfg.clients {
        let desired = entry.to_oauth2_client();
        match admin.get_client(&entry.client_id).await? {
            None => {
                tracing::info!(client_id = %entry.client_id, "registering OIDC client");
                admin.create_client(&desired).await?;
            }
            Some(_existing) => {
                tracing::info!(client_id = %entry.client_id, "updating OIDC client");
                admin.update_client(&desired).await?;
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 15.2: Build**

Run: `cargo check -p zeroship-auth`
Expected: clean.

- [ ] **Step 15.3: Commit**

```bash
git add crates/auth/src/bootstrap/mod.rs
git commit -m "auth: bootstrap — orchestrator (keys + client reconciliation)"
```

---

## Task 16 · Docker compose entry for hydra + `ops/hydra.yaml`

**Files:**
- Modify: `docker-compose.yml` (root)
- Create: `ops/hydra.yaml`
- Create: `ops/auth-clients.example.toml`

- [ ] **Step 16.1: Inspect existing compose file**

Run: `cat docker-compose.yml | head -100`

Identify the existing services (likely `postgres`, `control`, `gateway`, `worker`). The hydra service slots in alongside them.

- [ ] **Step 16.2: Add the `hydra` service**

Insert under `services:`:

```yaml
  hydra:
    image: oryd/hydra:v2.4.0  # bump as hydra releases v26.2.x
    depends_on:
      postgres:
        condition: service_healthy
    environment:
      DSN: postgres://zeroship:zeroship@postgres:5432/zeroship?sslmode=disable
      SECRETS_SYSTEM: ${SECRETS_SYSTEM:-dev-secret-please-change-this-please}
      SECRETS_COOKIE: ${SECRETS_COOKIE:-dev-secret-please-change-this-please}
    volumes:
      - ./ops/hydra.yaml:/etc/config/hydra/hydra.yaml:ro
    command: serve all --dev --config /etc/config/hydra/hydra.yaml
    ports:
      - "4444:4444"           # public
      # admin port 4445 NOT exposed — only reachable via the auth service network
    networks:
      - default

  hydra-migrate:
    image: oryd/hydra:v2.4.0
    depends_on:
      postgres:
        condition: service_healthy
    environment:
      DSN: postgres://zeroship:zeroship@postgres:5432/zeroship?sslmode=disable
    command: migrate sql up -e --yes
    restart: on-failure
```

Note: at writing time Docker Hub tag for v26.2.x calendar versioning is `v2.x` until ory's tag-naming catches up. Confirm by `docker pull oryd/hydra` and inspecting available tags. The image's CLI flag `--dev` disables HTTPS — fine for compose; production uses Helm or Nomad with TLS-terminating ingress.

- [ ] **Step 16.3: Create `ops/hydra.yaml`**

```yaml
dsn: postgres://zeroship:zeroship@postgres:5432/zeroship?sslmode=disable

serve:
  public:
    port: 4444
    host: 0.0.0.0
  admin:
    port: 4445
    host: 0.0.0.0           # in production: 127.0.0.1; in compose we share network
  cookies:
    same_site_mode: Lax
    domain: auth.zeroship.ai

urls:
  self:
    issuer: https://auth.zeroship.ai/
    public: https://auth.zeroship.ai/
  login:   https://auth.zeroship.ai/login
  consent: https://auth.zeroship.ai/consent
  logout:  https://auth.zeroship.ai/logout
  error:   https://auth.zeroship.ai/error

strategies:
  access_token: opaque

oauth2:
  pkce:
    enforced_for_public_clients: true
    enforced: true
  grant:
    refresh_token:
      rotation_grace_period: 30s
      rotation_grace_reuse_count: 3

ttl:
  access_token: 1h
  refresh_token: 720h
  id_token: 1h
  auth_code: 60s
  login_consent_request: 1h

oidc:
  subject_identifiers:
    supported_types: [public]
  dynamic_client_registration:
    enabled: false

log:
  level: info
  format: json

tracing:
  provider: otel
  providers:
    otlp:
      server_url: http://otel-collector:4318
```

- [ ] **Step 16.4: Create `ops/auth-clients.example.toml`**

```toml
# Declarative OIDC client config. The auth service reconciles this against
# hydra's admin API at every boot (upsert; never deletes).

[[client]]
client_id = "console.zeroship.ai"
client_name = "zeroship Console"
client_secret = "dev-secret-rotate-me"
redirect_uris = ["https://console.zeroship.ai/auth/callback", "http://localhost:5173/auth/callback"]
post_logout_redirect_uris = ["https://console.zeroship.ai/", "http://localhost:5173/"]
scope = "openid offline_access email profile"
access_token_strategy = "jwt"
id_token_signed_response_alg = "EdDSA"
audience = ["https://api.zeroship.ai"]
first_party = true

[[client]]
client_id = "gateway"
client_name = "zeroship Gateway (hosted apps)"
client_secret = "dev-secret-rotate-me-too"
# redirect_uris grow per-deployed-app — control plane appends on each deploy
redirect_uris = []
scope = "openid offline_access email profile"
access_token_strategy = "jwt"
id_token_signed_response_alg = "EdDSA"
audience = ["https://api.zeroship.ai"]
first_party = true
```

- [ ] **Step 16.5: Smoke — bring hydra up**

```bash
docker compose up -d postgres
docker compose run --rm hydra-migrate          # one-shot migration
docker compose up -d hydra
sleep 3
curl -s http://localhost:4444/.well-known/openid-configuration | head -40
```

Expected: discovery JSON with `issuer: https://auth.zeroship.ai/`, `code_challenge_methods_supported: [S256]`.

- [ ] **Step 16.6: Commit**

```bash
git add docker-compose.yml ops/hydra.yaml ops/auth-clients.example.toml
git commit -m "ops: add hydra v2.4 service, config, and example clients TOML"
```

---

## Task 17 · Wire everything in `main.rs`

**Files:**
- Modify: `crates/auth/src/main.rs`

- [ ] **Step 17.1: Rewrite `main.rs` to run migrations + bootstrap on startup**

```rust
//! zeroship-auth — the OIDC IdP login UI + identity flows + hydra admin client.

use clap::Parser;
use compio_postgres::{connect, NoTls};

mod bootstrap;
mod config;
mod error;
mod hydra_client;
mod server;
mod store;

use crate::config::AuthConfig;
use crate::hydra_client::HydraAdmin;

#[compio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    zeroship_core::observability::init_tracing("zeroship-auth");

    let cfg = AuthConfig::parse();
    tracing::info!(addr = %cfg.addr, "starting zeroship-auth");

    // 1. Open PG.
    let (client, conn) = connect(&cfg.db_url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = conn.run().await {
            tracing::error!(error = %e, "auth/pg connection error");
        }
    }).detach();

    // 2. Run migrations.
    store::migrations::migrate(&client).await?;
    tracing::info!("auth.* migrations applied");

    // 3. Bootstrap: keys + client reconciliation.
    let admin = HydraAdmin::new(&cfg.hydra_admin);
    bootstrap::run(&admin, cfg.bootstrap, &cfg.clients_config).await?;
    tracing::info!("bootstrap complete");

    // 4. Serve.
    server::run(cfg).await?;
    Ok(())
}
```

- [ ] **Step 17.2: Build**

Run: `cargo check -p zeroship-auth`
Expected: clean.

- [ ] **Step 17.3: Commit**

```bash
git add crates/auth/src/main.rs
git commit -m "auth: wire PG + migrations + bootstrap in main"
```

---

## Task 18 · Smoke test — real hydra round-trip

**Files:**
- Create: `crates/auth/tests/hydra_client_smoke.rs`

A test that boots against a real hydra (the compose stack). Skips when `AUTH_HYDRA_ADMIN` is unset, so CI without docker doesn't break.

- [ ] **Step 18.1: Write the failing test**

```rust
//! Smoke test: every hydra admin endpoint we call must round-trip
//! cleanly against a live hydra. Runs only when AUTH_HYDRA_ADMIN is set;
//! intended to be invoked after `docker compose up -d hydra`.

use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::hydra_client::types::OAuth2Client;

fn admin() -> Option<HydraAdmin> {
    let base = std::env::var("AUTH_HYDRA_ADMIN").ok()?;
    Some(HydraAdmin::new(base))
}

#[compio::test]
async fn jwks_create_and_list() {
    let Some(admin) = admin() else { eprintln!("skip"); return };

    // Trying both — if either is empty hydra responds 404, otherwise non-empty.
    let _ = admin.get_jwks("hydra.openid.id-token").await.expect("get_jwks");
    let _ = admin.get_jwks("hydra.jwt.access-token").await.expect("get_jwks");
}

#[compio::test]
async fn client_crud_roundtrip() {
    let Some(admin) = admin() else { eprintln!("skip"); return };

    let cid = format!("test-client-{}", uuid::Uuid::new_v4().simple());

    let mut desired = OAuth2Client {
        client_id: cid.clone(),
        client_name: Some("smoke test".into()),
        client_secret: Some("smoke-secret".into()),
        grant_types: vec!["authorization_code".into(), "refresh_token".into()],
        response_types: vec!["code".into()],
        redirect_uris: vec!["https://example.test/cb".into()],
        post_logout_redirect_uris: vec![],
        scope: "openid offline_access".into(),
        token_endpoint_auth_method: "client_secret_basic".into(),
        subject_type: "public".into(),
        access_token_strategy: Some("jwt".into()),
        id_token_signed_response_alg: Some("EdDSA".into()),
        audience: vec![],
        skip_consent: true,
        require_consent: false,
        require_logout_consent: false,
        frontchannel_logout_uri: None,
        backchannel_logout_uri: None,
    };

    // Create
    let created = admin.create_client(&desired).await.expect("create_client");
    assert_eq!(created.client_id, cid);

    // Read
    let fetched = admin.get_client(&cid).await.expect("get_client");
    assert!(fetched.is_some(), "client must exist after creation");

    // Update
    desired.client_name = Some("smoke test (updated)".into());
    let updated = admin.update_client(&desired).await.expect("update_client");
    assert_eq!(updated.client_name.as_deref(), Some("smoke test (updated)"));

    // Delete
    admin.delete_client(&cid).await.expect("delete_client");

    // Read-back: gone
    let after = admin.get_client(&cid).await.expect("get_client (after delete)");
    assert!(after.is_none(), "client must be gone after delete");
}

#[compio::test]
async fn login_challenge_returns_404_for_unknown() {
    let Some(admin) = admin() else { eprintln!("skip"); return };

    // Random challenge that doesn't exist; expect a hydra error containing → 404 or → 410.
    let err = admin.get_login(&format!("nonexistent-{}", uuid::Uuid::new_v4().simple())).await;
    let msg = err.expect_err("hydra should error").to_string();
    assert!(msg.contains("→ 404") || msg.contains("→ 410"),
            "unexpected error shape: {msg}");
}
```

- [ ] **Step 18.2: Run with hydra down — expect skip**

Run: `cargo test -p zeroship-auth --test hydra_client_smoke -- --nocapture`
Expected: each test prints "skip" and passes (no `AUTH_HYDRA_ADMIN` set).

- [ ] **Step 18.3: Bring up hydra and re-run**

```bash
docker compose up -d hydra
sleep 3
# First call uses --bootstrap to populate JWKs:
AUTH_DB_URL=postgres://zeroship:zeroship@localhost/zeroship \
AUTH_HYDRA_ADMIN=http://localhost:4445 \
AUTH_CLIENTS_CONFIG=ops/auth-clients.example.toml \
AUTH_BOOTSTRAP=1 \
cargo run -p zeroship-auth &
AUTH_PID=$!
sleep 3
kill $AUTH_PID

# Now hydra has keys + clients. Run the smoke test:
AUTH_HYDRA_ADMIN=http://localhost:4445 \
cargo test -p zeroship-auth --test hydra_client_smoke -- --nocapture
```

Expected: 3 tests PASS.

If `jwks_create_and_list` fails because keys are still empty: re-run the auth server boot above; `--bootstrap` should populate.

If `client_crud_roundtrip` fails: inspect hydra's response (`docker compose logs hydra | tail -50`) — most likely a 422 because a required field is missing in `OAuth2Client`.

- [ ] **Step 18.4: Commit**

```bash
git add crates/auth/tests/hydra_client_smoke.rs
git commit -m "auth: smoke test — hydra admin client round-trips against live hydra"
```

---

## Task 19 · Phase 1 close-out commit and sanity

- [ ] **Step 19.1: Run the full crate test suite**

Run: `cargo test -p zeroship-auth`
Expected: every test PASS or `skip` (the smoke tests skip without hydra; the migrations smoke skips without PG).

- [ ] **Step 19.2: Build all crates to ensure no workspace breakage**

Run: `cargo build --workspace`
Expected: clean.

- [ ] **Step 19.3: Lint pass**

Run: `cargo clippy -p zeroship-auth -- -D warnings`
Expected: no warnings. Fix any inline.

- [ ] **Step 19.4: Commit a Phase 1 milestone marker**

```bash
git commit --allow-empty -m "auth: Phase 1 complete — hydra sidecar + admin client + bootstrap"
```

- [ ] **Step 19.5: Tag the milestone (optional, no push)**

```bash
git tag auth-phase-1
```

---

# Phase 2 — Password login UX

**Goal:** Implement `/login`, `/signup`, and the password-credential flow. By the end of this phase a user can complete an OIDC `code+PKCE` flow end-to-end against the new IdP using email+password.

(Full bite-sized tasks below.)

## Phase 2 task list (overview)

| # | Task | Files | Time |
|---|---|---|---|
| 1 | Argon2id password module + dummy-hash | src/identity/password.rs | 20 min |
| 2 | Rate-limit token-bucket | src/ratelimit.rs, src/store/ratelimit.rs | 20 min |
| 3 | Audit event helper | src/audit.rs, src/store/audit.rs | 12 min |
| 4 | IdP login session store + cookie | src/sessions/login.rs, src/store/sessions.rs | 25 min |
| 5 | CSRF double-submit helper | src/csrf.rs | 12 min |
| 6 | Security-headers middleware | src/headers.rs | 8 min |
| 7 | Askama base template + login HTML | src/ui/templates/* | 25 min |
| 8 | `/login` GET handler (reads challenge, renders) | src/ui/login.rs | 20 min |
| 9 | `/login` POST handler (verifies + accept_login) | src/ui/login.rs | 30 min |
| 10 | `/signup` GET/POST + email verification stub | src/ui/signup.rs | 25 min |
| 11 | `/consent` skip-consent fast path | src/ui/consent.rs | 20 min |
| 12 | User CRUD in store | src/store/users.rs | 18 min |
| 13 | Wire all handlers into `server::configure` | src/server.rs | 8 min |
| 14 | e2e_password test | tests/e2e_password.rs | 45 min |
| 15 | enum_defense test | tests/enum_defense.rs | 25 min |
| 16 | threat_model tests (csrf, fixation, ratelimit) | tests/threat_model.rs | 30 min |
| 17 | Phase 2 close-out | (clippy, milestone) | 10 min |

## Phase 2 task details (bite-sized)

### Task P2-1 · Argon2id password module

**Files:** Create `crates/auth/src/identity/mod.rs`, `crates/auth/src/identity/password.rs`. Modify `src/lib.rs`.

- [ ] **Step P2-1.1: Add Argon2id deps**

Open `crates/auth/Cargo.toml` and add to `[dependencies]`:

```toml
argon2 = "0.5"
password-hash = "0.5"
rand = "0.8"
```

If these are workspace-pinned, use `argon2 = { workspace = true }`.

- [ ] **Step P2-1.2: Create `crates/auth/src/identity/mod.rs`**

```rust
//! User identity flows. Phase 2 = password; later phases add federation,
//! magic-link, verification, reset.

pub mod password;
```

- [ ] **Step P2-1.3: Create `crates/auth/src/identity/password.rs`**

```rust
//! Argon2id password hashing + enumeration-resistant verification.
//!
//! See proposal §8.1: OWASP 2026 params (m=19 MiB, t=2, p=1).
//! Argon2 is CPU-bound and synchronous; callers wrap in
//! `compio::runtime::spawn_blocking`.

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use password_hash::{rand_core::OsRng, SaltString};
use std::sync::OnceLock;

use crate::error::{AuthError, Result};

/// OWASP 2026 second-recommended profile (m = 19 MiB, t = 2, p = 1).
fn argon2() -> Argon2<'static> {
    let params = Params::new(19_456, 2, 1, None).expect("argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a password. Returns a PHC string ($argon2id$v=19$m=19456,t=2,p=1$...).
pub fn hash(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let phc = argon2()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::Internal(format!("argon2 hash: {e}")))?;
    Ok(phc.to_string())
}

/// Verify a password. Returns Ok(true) on match, Ok(false) on mismatch.
/// Returns Err only on malformed PHC strings.
pub fn verify(password: &str, phc: &str) -> Result<bool> {
    let parsed = PasswordHash::new(phc)
        .map_err(|e| AuthError::Internal(format!("argon2 parse: {e}")))?;
    match argon2().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(password_hash::Error::Password) => Ok(false),
        Err(e) => Err(AuthError::Internal(format!("argon2 verify: {e}"))),
    }
}

/// Pre-computed dummy hash used when no user matches the submitted email.
/// Keeps wall time and code path constant regardless of user existence
/// (account-enumeration defense — see proposal §8.1).
///
/// Hashed once on first call and memoised.
pub fn dummy_hash() -> &'static str {
    static D: OnceLock<String> = OnceLock::new();
    D.get_or_init(|| hash("absent-user-padding").expect("dummy hash"))
}

/// Verify against the dummy hash. Always returns `Ok(false)` but spends
/// the same wall time as a real verify.
pub fn verify_against_dummy(password: &str) -> Result<bool> {
    verify(password, dummy_hash())
}
```

- [ ] **Step P2-1.4: Add identity to `lib.rs`**

```rust
pub mod bootstrap;
pub mod config;
pub mod error;
pub mod hydra_client;
pub mod identity;
pub mod server;
pub mod store;
```

- [ ] **Step P2-1.5: Write the roundtrip + enumeration-defense test**

Create `crates/auth/tests/password_test.rs`:

```rust
use std::time::Instant;
use zeroship_auth::identity::password;

#[test]
fn hash_verify_roundtrip() {
    let phc = password::hash("correct-horse-battery-staple").expect("hash");
    assert!(password::verify("correct-horse-battery-staple", &phc).expect("verify"));
    assert!(!password::verify("wrong-password", &phc).expect("verify"));
}

#[test]
fn dummy_hash_is_constant_time_within_tolerance() {
    // Warm the dummy hash so first-call init isn't counted.
    let _ = password::dummy_hash();

    // Generate a real hash to compare against.
    let real_phc = password::hash("real-password-here").expect("hash");

    let t_real = {
        let t0 = Instant::now();
        let _ = password::verify("wrong-password", &real_phc).expect("verify");
        t0.elapsed()
    };
    let t_dummy = {
        let t0 = Instant::now();
        let _ = password::verify_against_dummy("any-password").expect("verify");
        t0.elapsed()
    };

    // Tolerance: 30% — argon2 is CPU-bound; this checks that we're in the
    // same ballpark, not exactly equal.
    let ratio = t_dummy.as_secs_f64() / t_real.as_secs_f64();
    assert!(ratio > 0.7 && ratio < 1.3,
            "dummy/real timing ratio = {ratio}, expected ~1.0");
}
```

- [ ] **Step P2-1.6: Run the test**

Run: `cargo test -p zeroship-auth password_`
Expected: both PASS.

- [ ] **Step P2-1.7: Commit**

```bash
git add crates/auth/Cargo.toml crates/auth/src/identity/ crates/auth/src/lib.rs crates/auth/tests/password_test.rs
git commit -m "auth: identity/password — Argon2id + dummy-hash enumeration defense"
```

### Task P2-2 · Rate-limit token-bucket

**Files:** Create `crates/auth/src/ratelimit.rs`, `crates/auth/src/store/ratelimit.rs`. Modify `src/store/mod.rs`.

(Full bite-sized steps continue in the same shape as Task P2-1.)

The bucket math is a leaky token bucket: each bucket has a refill rate (tokens/sec) and a capacity. `consume(key, cost)` returns Ok if there are enough tokens; deducts; returns Err with `retry_after` otherwise. Buckets persist as PG rows with a row-level lock.

For brevity, the rest of Phase 2 follows the same TDD pattern per task. Each task: failing test → run → impl → run → commit. Reference the proposal sections cited in the overview table above.

### Tasks P2-3 through P2-17

Each follows the bite-sized template:

- **Step N.1** Write the failing test.
- **Step N.2** Run the test; expect FAIL.
- **Step N.3** Write the minimal implementation per the snippet in the proposal section.
- **Step N.4** Run the test; expect PASS.
- **Step N.5** Commit.

The detailed code snippets for these tasks are not duplicated here; they trace 1:1 to the proposal section listed in the task table. When you reach a task, open the proposal at that section and use it as the source of truth.

If during execution any snippet from the proposal turns out to be ambiguous (e.g. a struct field name that doesn't match cyper's actual API), pause and ask the pilot rather than guessing. The plan's job is to scope each task; the proposal's job is to specify behavior.

**End of Phase 2 milestone:** a user can register, log in via the IdP at `auth.zeroship.ai/login`, complete a code+PKCE flow with a registered first-party client, and the gateway-or-control RP would (in Phase 3) exchange the code and proxy the user to the worker.

---

# Phase 3 — Gateway and control plane as OIDC RPs (separate plan)

Triggers when Phase 2 lands. Outline only here:

| # | Task |
|---|---|
| 1 | New module `crates/gateway/src/oidc_rp.rs` — code exchange, JWKS cache, per-app session set |
| 2 | Gateway proxy rules: forward `auth.zeroship.ai/oauth2/*` and `/.well-known/*` to hydra-public |
| 3 | Gateway 401-redirect to `auth.zeroship.ai/oauth2/auth` with PKCE |
| 4 | Gateway `/__zs/auth/callback` handler |
| 5 | Per-app session cookie `__Host-zs_app_session` |
| 6 | Worker payload (`ZeroShip-User`) unchanged — verify HMAC tests still pass |
| 7 | New module `crates/control/src/oidc_rp.rs` — same shape for the dashboard |
| 8 | Delete `crates/control/src/{auth_service,auth_handlers,oauth}.rs` |
| 9 | Delete `crates/gateway/src/user_auth.rs` JWT-cookie path |
| 10 | Migrate `auth_users` → `auth.users` (one-shot in store::migrations) |
| 11 | Drop legacy tables (`auth_users`, `auth_app_consents`, `auth_sessions`) |
| 12 | Update `docs/reference/auth.md` to point at the new architecture |
| 13 | e2e_full_path test: real browser → gateway → hydra → crates/auth → back → gateway → worker |

# Phase 4 — Federation (separate plan)

| # | Task |
|---|---|
| 1 | `identity/oauth/mod.rs` — shared PKCE+state machinery for FEDERATION (distinct from our IdP) |
| 2 | `identity/oauth/google.rs` — Google OIDC; reuse existing `crates/control/src/oauth.rs` patterns |
| 3 | `identity/oauth/github.rs` — `/user` + `/user/emails`; verified-primary picker |
| 4 | `/oauth/google/start` + `/oauth/google/callback` handlers |
| 5 | `/oauth/github/start` + `/oauth/github/callback` handlers |
| 6 | `store/identities.rs` — find-or-link by `(provider, subject)` |
| 7 | Account-linking decision tree (verified-email gates) |
| 8 | `/me/link` + `/me/unlink` handlers |
| 9 | e2e_google + e2e_github tests with mocked provider |

# Phase 5 — Magic-link + email flows (separate plan)

| # | Task |
|---|---|
| 1 | `mailer/mod.rs` trait |
| 2 | `mailer/stdout.rs` (dev) |
| 3 | `mailer/smtp.rs` (lettre) |
| 4 | `mailer/resend.rs` (cyper) |
| 5 | Email templates via askama (verify-email, magic-link, password-reset, suspicious-activity) |
| 6 | `identity/magic_link.rs` — issue + redeem |
| 7 | Cross-device CSRF binding (6-digit code interstitial) |
| 8 | `/magic/verify` handler |
| 9 | `identity/verification.rs` — signup-time email verification (24h) |
| 10 | `/verify` handler |
| 11 | `/forgot` + `/reset` handlers (1 h reset token) |
| 12 | `identity/breach.rs` — HIBP k-anonymity |
| 13 | `mailer/bounce.rs` + `/webhooks/{postmark,ses-sns}` handlers |
| 14 | `store/suppressions.rs` |
| 15 | e2e_magic test (same-device + cross-device) |

# Phase 6 — Polish & hardening (separate plan)

| # | Task |
|---|---|
| 1 | DPoP gateway-side opt (one of §19 open questions) |
| 2 | `/me` page + linked-identities UI |
| 3 | `acr`/`amr` propagation for future MFA |
| 4 | JWK rotation cron (90 d cycle) |
| 5 | Audit-log retention sweeper |
| 6 | Observability: hydra OTLP correlation IDs join with our audit log via `request_id` |
| 7 | Load test: 200 logins/sec sustained on a 4-core box |
| 8 | Security review by external reviewer or codex |
| 9 | `docs/runbooks/auth-deploy.md` |

---

# Self-review

(Run by the plan author before handing off.)

**1. Spec coverage.** Cross-check every proposal section with at least one task or future-phase entry:

- §0 framing → Phase 1 (skeleton)
- §1 decisions → all phases
- §2 architecture → Phase 1 (hydra + crates/auth) + Phase 3 (gateway)
- §3 spec surface → owned by hydra config in Phase 1; gaps documented (DPoP in Phase 6)
- §4 crate layout → Phase 1 tasks 1, 4, 5–12, 13–15; Phase 2 tasks 1–13
- §5 data model → Phase 1 Task 4
- §6 endpoint surface → Phase 1 Tasks 3, 16, 17; Phase 2 Tasks 8–11; Phase 3 (gateway proxy rules); Phase 5 (magic, webhooks)
- §7 tokens + keys → Phase 1 Task 14 (bootstrap); Phase 6 (rotation cron)
- §8 identity flows → Phase 2 (password); Phase 4 (federation); Phase 5 (magic, verify, reset)
- §9 sessions → Phase 2 Task 4
- §10 client registry → Phase 1 Tasks 10, 13, 15
- §11 migration → Phase 3 (atomic in single PR)
- §12 mailer → Phase 5 Tasks 1–4
- §13 threat model → Phase 2 Task 16
- §14 cookies/headers → Phase 2 Tasks 5, 6
- §15 audit → Phase 2 Task 3
- §16 ops → Phase 1 Task 16; Phase 6 (rotation cron, runbook)
- §17 testing → all phases; e2e per phase
- §18 out of scope → Phase 6 (DPoP) flags continuing gap
- §19 open questions → answered as we go; tracked in proposal §19

**2. Placeholder scan.** No "TBD" / "TODO" / "fill in later" in Phase 1 tasks (Phase 2 includes explicit "open the proposal at this section" pointers, which is acceptable per the proposal-driven model). No vague "add error handling" — every error path is named.

**3. Type consistency.** `AuthError` consistently from `crate::error::AuthError`. `HydraAdmin` consistently as the admin client. `OAuth2Client` is the wire-format struct in `hydra_client/types.rs`. `AuthConfig` is the CLI/env config. `ClientsConfig` parses the TOML.

---

# Execution handoff

Plan saved to `docs/superpowers/plans/2026-05-26-auth-server-phase-1-foundation.md` (this file).

Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review the diff and test output between tasks, fast iteration with two-stage review.

**2. Inline Execution** — Execute tasks in this session using `superpowers:executing-plans`, batched with mid-phase checkpoints for review.

Which approach do you want?
