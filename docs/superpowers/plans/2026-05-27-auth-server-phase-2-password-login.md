# Auth server — Phase 2 implementation plan: Password login UX

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the email + password authentication flow end-to-end. A user can register, log in via `auth.zeroship.ai/login`, and complete a full OIDC `code+PKCE` exchange against a first-party client. The acceptance test boots PG + hydra + `crates/auth` and drives the entire flow against the real binaries — no shims.

**Architecture:** All handlers live in `crates/auth/`. The login/consent UI is server-rendered HTML (askama templates, single CSS file, no SPA). The handlers consume hydra's `login_challenge` / `consent_challenge` query params, run the credential check, and call hydra's admin API to accept (`PUT /admin/oauth2/auth/requests/{login,consent}/accept`). Hydra then issues the auth code + ID token + access token + refresh token.

**Tech Stack:**
- Rust (compio runtime, ntex web framework, cyper HTTP, compio-postgres)
- `argon2 = "0.5"` + `password-hash = "0.5"` (already added in Phase 2 Task P2-1 in the Phase 1 plan; verify workspace)
- `askama = "0.12"` (compile-time HTML templating)
- `rand = "0.8"` (CSPRNG for session ids, CSRF tokens, salts)
- `cookie = "0.18"` (cookie parsing)

**Reference docs to keep open:**
- `docs/archive/auth-server.md` §8.1 (password flow) and §13 (threat model — ours rows)
- `docs/superpowers/plans/2026-05-26-auth-server-phase-1-foundation.md` — Phase 1 task layout for pattern reference
- `crates/control/src/auth_handlers.rs` — legacy login/signup pattern, for cookie + ntex idioms only (we are NOT carrying logic over; just the shape of ntex handlers)

**Pre-launch posture (AGENTS.md):** no back-compat shims, no `@deprecated`, no migration paths. Hydra's TTLs in `ops/hydra.yaml` (10-min ID token, 10-min access token, 60-s auth code, 30-d refresh) are the contract. We add new code only; nothing in main yet calls any of it.

**Phase 2 starting point:** worktree tip at `034f1eaf` (or wherever HEAD is on `proposal/auth-server`). The Phase 1 milestone tag `auth-phase-1` at `b630c473`. Hydra is reachable via `docker compose up -d` and the smoke tests in `tests/hydra_client_smoke.rs` pass against it.

---

## Phase 2 unit list (overview)

| # | Unit | Plan tasks covered | Sub-commits | Time |
|---|---|---|---|---|
| U1 | Foundation primitives | password, ratelimit, audit, user-store | 4 | 80 min |
| U2 | HTTP safety primitives | sessions, csrf, security headers | 3 | 45 min |
| U3 | Askama templates + base layout | login.html, signup.html, error.html, base.html, CSS | 1 | 25 min |
| U4 | Login + signup handlers | /login GET, /login POST, /signup GET, /signup POST | 2 | 60 min |
| U5 | Consent handler (skip-consent fast path) | /consent GET, /consent POST | 1 | 25 min |
| U6 | Wire handlers into `server::configure` | route mapping | 1 | 5 min |
| U7 | e2e_password test (live PG + live hydra) | the acceptance criterion | 1 | 50 min |
| U8 | enum_defense + threat_model tests | parametric over §13 "ours" rows | 1 | 30 min |
| U9 | Phase 2 close-out (clippy + milestone) | tag + commit | 1 | 10 min |

Total: ~15 commits across 9 units, ~5h of focused work.

---

# Unit U1 · Foundation primitives

Four small pure-logic modules. Each gets its own sub-task + commit. No HTTP, no hydra in this unit.

## U1-A · Argon2id password module

**Files:**
- Modify: `crates/auth/Cargo.toml` (add deps)
- Create: `crates/auth/src/identity/mod.rs`
- Create: `crates/auth/src/identity/password.rs`
- Modify: `crates/auth/src/lib.rs` (export `identity`)
- Create: `crates/auth/tests/password_test.rs`

- [ ] **Step U1-A.1: Add password-hashing deps**

Open `crates/auth/Cargo.toml`. Add under `[dependencies]`:

```toml
argon2 = "0.5"
password-hash = "0.5"
rand = "0.8"
```

If these are workspace-pinned in the root `Cargo.toml`, use `argon2 = { workspace = true }` etc. Check first.

Verify with `cargo check -p zeroship-auth` after adding.

- [ ] **Step U1-A.2: Create `crates/auth/src/identity/mod.rs`**

```rust
//! User identity flows. Phase 2 = password. Phase 4 adds federation
//! (Google/GitHub OAuth). Phase 5 adds magic-link + email verification.

pub mod password;
```

- [ ] **Step U1-A.3: Create `crates/auth/src/identity/password.rs`**

```rust
//! Argon2id password hashing + enumeration-resistant verification.
//!
//! Per proposal §8.1: OWASP 2026 second-recommended params
//! (m = 19 MiB / 19456 KiB, t = 2, p = 1). Returns PHC strings.
//!
//! Argon2 is CPU-bound and synchronous; callers running on the ntex
//! event loop wrap calls in `compio::runtime::spawn_blocking` to avoid
//! parking the loop.

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use password_hash::{rand_core::OsRng, SaltString};
use std::sync::OnceLock;

use crate::error::{AuthError, Result};

fn argon2() -> Argon2<'static> {
    let params = Params::new(19_456, 2, 1, None).expect("argon2 params");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a password, returning a PHC string suitable for `auth.users.password_hash`.
///
/// # Errors
///
/// Returns `AuthError::Internal` on argon2 misconfiguration (shouldn't happen
/// in practice — params are fixed at compile time).
pub fn hash(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let phc = argon2()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| AuthError::Internal(format!("argon2 hash: {e}")))?;
    Ok(phc.to_string())
}

/// Verify a password against a stored PHC string.
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch. Only returns `Err`
/// when the PHC string itself is malformed.
///
/// # Errors
///
/// Returns `AuthError::Internal` on argon2 parse/verify failure.
pub fn verify(password: &str, phc: &str) -> Result<bool> {
    let parsed = PasswordHash::new(phc)
        .map_err(|e| AuthError::Internal(format!("argon2 parse: {e}")))?;
    match argon2().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(password_hash::Error::Password) => Ok(false),
        Err(e) => Err(AuthError::Internal(format!("argon2 verify: {e}"))),
    }
}

/// Pre-computed dummy hash. Used by the login handler when no user matches the
/// submitted email, so the failure path runs the same code and spends the same
/// wall time as the real verify path. Defeats login-side email enumeration.
///
/// Hashed once on first call and memoised.
#[must_use]
pub fn dummy_hash() -> &'static str {
    static D: OnceLock<String> = OnceLock::new();
    D.get_or_init(|| hash("absent-user-padding").expect("dummy hash"))
}

/// Verify against the dummy hash. Always returns `Ok(false)` but spends the
/// same wall time as a real verify call.
///
/// # Errors
///
/// Returns `AuthError::Internal` on argon2 misconfiguration (should not occur).
pub fn verify_against_dummy(password: &str) -> Result<bool> {
    verify(password, dummy_hash())
}
```

- [ ] **Step U1-A.4: Export `identity` from `lib.rs`**

Add `pub mod identity;` to `crates/auth/src/lib.rs`. Keep the existing alphabetical order.

- [ ] **Step U1-A.5: Write the failing test**

Create `crates/auth/tests/password_test.rs`:

```rust
//! Argon2id roundtrip + enumeration-defense timing test.

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

    let ratio = t_dummy.as_secs_f64() / t_real.as_secs_f64();
    assert!(ratio > 0.5 && ratio < 2.0,
            "dummy/real timing ratio = {ratio}, expected ~1.0 (got real={t_real:?}, dummy={t_dummy:?})");
}
```

- [ ] **Step U1-A.6: Run the test**

```
cargo test -p zeroship-auth --test password_test
```

Expected: both tests PASS. If `dummy_hash_is_constant_time_within_tolerance` flakes on slow CI, widen the ratio tolerance to `> 0.4 && < 2.5`. The point of the test is to catch a 10x mismatch, not to assert nanosecond equality.

- [ ] **Step U1-A.7: Commit**

```bash
git add crates/auth/Cargo.toml crates/auth/src/identity/ crates/auth/src/lib.rs crates/auth/tests/password_test.rs
git commit -m "auth: identity/password — Argon2id + dummy-hash enumeration defense"
```

## U1-B · Rate-limit token-bucket

**Files:**
- Create: `crates/auth/src/store/ratelimit.rs`
- Create: `crates/auth/src/ratelimit.rs`
- Modify: `crates/auth/src/store/mod.rs`
- Modify: `crates/auth/src/lib.rs`
- Create: `crates/auth/tests/ratelimit_test.rs`

The token bucket: each `bucket_key` has a max capacity, a refill rate (tokens/sec), and current state `(tokens, last_updated)`. `consume(key, cost)` deducts; if insufficient, returns `Err(RateLimited { retry_after })`. State persists in PG `auth.rate_limits`.

- [ ] **Step U1-B.1: Create `crates/auth/src/store/ratelimit.rs`**

```rust
//! Rate-limit bucket persistence in `auth.rate_limits`.
//!
//! One row per bucket key. State is `(tokens, updated_at)`. Atomic
//! upsert via `INSERT ... ON CONFLICT DO UPDATE` so concurrent requests
//! don't race.

use compio_postgres::Client;

use crate::error::{AuthError, Result};

#[derive(Debug, Clone)]
pub struct BucketState {
    pub tokens: f64,
    pub updated_at_micros: i64,
}

/// Fetch (or initialise) a bucket. Returns the current state. If the row
/// doesn't exist, returns `(capacity, now)` — a fresh full bucket.
pub async fn fetch_or_init(
    conn: &Client,
    key: &str,
    capacity: f64,
) -> Result<BucketState> {
    let rows = conn
        .query(
            "SELECT tokens, EXTRACT(EPOCH FROM updated_at)::DOUBLE PRECISION AS updated_secs \
             FROM auth.rate_limits WHERE bucket_key = $1",
            &[&key],
        )
        .await
        .map_err(|e| AuthError::Db(format!("ratelimit fetch {key}: {e}")))?;

    if let Some(row) = rows.first() {
        let tokens: f64 = row.get("tokens");
        let updated: f64 = row.get("updated_secs");
        return Ok(BucketState {
            tokens,
            updated_at_micros: (updated * 1_000_000.0) as i64,
        });
    }

    // Fresh full bucket; epoch in microseconds.
    let now_micros = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| AuthError::Internal(format!("sys time: {e}")))?
        .as_micros()) as i64;

    Ok(BucketState {
        tokens: capacity,
        updated_at_micros: now_micros,
    })
}

/// Atomically write the bucket state back.
pub async fn upsert(conn: &Client, key: &str, state: &BucketState) -> Result<()> {
    let secs = (state.updated_at_micros as f64) / 1_000_000.0;
    conn.execute(
        "INSERT INTO auth.rate_limits (bucket_key, tokens, updated_at) \
         VALUES ($1, $2, TO_TIMESTAMP($3)) \
         ON CONFLICT (bucket_key) DO UPDATE \
         SET tokens = EXCLUDED.tokens, updated_at = EXCLUDED.updated_at",
        &[&key, &state.tokens, &secs],
    )
    .await
    .map_err(|e| AuthError::Db(format!("ratelimit upsert {key}: {e}")))?;
    Ok(())
}
```

- [ ] **Step U1-B.2: Register `pub mod ratelimit;` in `crates/auth/src/store/mod.rs`**

- [ ] **Step U1-B.3: Create `crates/auth/src/ratelimit.rs`**

```rust
//! Leaky token bucket. Three configured profiles map to the three buckets
//! in proposal §8.1 (per-email-IP, per-email, per-IP).
//!
//! Each call: read state, refill based on elapsed time, attempt to consume,
//! write back. PG row-level locks make this race-safe; in practice the
//! UPDATE ON CONFLICT pattern is atomic.

use compio_postgres::Client;

use crate::error::Result;
use crate::store::ratelimit as store;

#[derive(Debug, Clone, Copy)]
pub struct Bucket {
    /// Maximum tokens the bucket holds.
    pub capacity: f64,
    /// Tokens refilled per second.
    pub refill_per_sec: f64,
}

impl Bucket {
    /// Per-(email, ip): 5 requests / 15 minutes.
    pub const LOGIN_EIP: Self = Self { capacity: 5.0, refill_per_sec: 5.0 / 900.0 };
    /// Per-account: 10 requests / hour.
    pub const LOGIN_EMAIL: Self = Self { capacity: 10.0, refill_per_sec: 10.0 / 3600.0 };
    /// Per-IP: 60 / hour.
    pub const LOGIN_IP: Self = Self { capacity: 60.0, refill_per_sec: 60.0 / 3600.0 };
}

#[derive(Debug)]
pub struct RateLimited {
    pub retry_after_secs: f64,
}

/// Attempt to consume one token from the named bucket.
///
/// Returns `Ok(())` on success, `Ok(Err(RateLimited))` on throttle, `Err` on DB error.
///
/// # Errors
///
/// Returns `AuthError::Db` if the PG read/write fails.
pub async fn consume(
    conn: &Client,
    key: &str,
    bucket: Bucket,
) -> Result<std::result::Result<(), RateLimited>> {
    let mut state = store::fetch_or_init(conn, key, bucket.capacity).await?;

    // Refill based on elapsed time.
    let now_micros = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros()) as i64;
    let elapsed_secs = ((now_micros - state.updated_at_micros) as f64) / 1_000_000.0;
    state.tokens = (state.tokens + elapsed_secs * bucket.refill_per_sec).min(bucket.capacity);
    state.updated_at_micros = now_micros;

    if state.tokens >= 1.0 {
        state.tokens -= 1.0;
        store::upsert(conn, key, &state).await?;
        Ok(Ok(()))
    } else {
        // Persist updated state so refill clock advances even on rejection.
        store::upsert(conn, key, &state).await?;
        let deficit = 1.0 - state.tokens;
        let retry_after_secs = deficit / bucket.refill_per_sec;
        Ok(Err(RateLimited { retry_after_secs }))
    }
}
```

- [ ] **Step U1-B.4: Export `pub mod ratelimit;` in `crates/auth/src/lib.rs`**

- [ ] **Step U1-B.5: Write the failing test**

Create `crates/auth/tests/ratelimit_test.rs`:

```rust
//! Token-bucket rate-limit smoke test (live PG).

use compio_postgres::{connect, NoTls};
use zeroship_auth::ratelimit::{consume, Bucket};
use zeroship_auth::store::migrations;

async fn pg_or_skip() -> Option<compio_postgres::Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("conn err: {e}");
        }
    })
    .detach();
    migrations::migrate(&client).await.expect("migrate");
    Some(client)
}

#[compio::test]
async fn consumes_until_throttled() {
    let Some(client) = pg_or_skip().await else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };

    let key = format!("test:{}", uuid::Uuid::new_v4().simple());
    let bucket = Bucket { capacity: 3.0, refill_per_sec: 0.0 }; // no refill for the test

    for i in 1..=3 {
        let res = consume(&client, &key, bucket).await.expect("consume");
        assert!(res.is_ok(), "request {i} should pass");
    }

    let res = consume(&client, &key, bucket).await.expect("consume");
    let err = res.expect_err("4th request should throttle");
    assert!(err.retry_after_secs > 0.0 || err.retry_after_secs.is_infinite(),
            "retry_after_secs should be positive, got {}", err.retry_after_secs);
}
```

- [ ] **Step U1-B.6: Run**

```
cargo test -p zeroship-auth --test ratelimit_test
```

Skip without `AUTH_DB_URL`; if PG is up, expect PASS.

- [ ] **Step U1-B.7: Commit**

```bash
git add crates/auth/src/{ratelimit.rs,store/ratelimit.rs,store/mod.rs,lib.rs} crates/auth/tests/ratelimit_test.rs
git commit -m "auth: ratelimit — PG-backed token bucket (login per-eip/email/ip)"
```

## U1-C · Audit event helper

**Files:**
- Create: `crates/auth/src/store/audit.rs`
- Create: `crates/auth/src/audit.rs`
- Modify: `crates/auth/src/store/mod.rs`
- Modify: `crates/auth/src/lib.rs`

Simple. One PG insert per emit, plus a JSON-on-stdout fan-out for SIEM ingestion.

- [ ] **Step U1-C.1: Create `crates/auth/src/store/audit.rs`**

```rust
//! Audit event insert into `auth.audit_events`.

use compio_postgres::Client;
use serde_json::Value;

use crate::error::{AuthError, Result};

#[allow(clippy::too_many_arguments)]
pub async fn insert(
    conn: &Client,
    event_type: &str,
    outcome: &str,
    user_id: Option<&uuid::Uuid>,
    client_id: Option<&str>,
    request_id: Option<&str>,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
    auth_method: Option<&str>,
    detail: &Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO auth.audit_events \
            (event_type, outcome, user_id, client_id, request_id, ip, user_agent, auth_method, detail) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        &[
            &event_type,
            &outcome,
            &user_id,
            &client_id,
            &request_id,
            &ip,
            &user_agent,
            &auth_method,
            &detail,
        ],
    )
    .await
    .map_err(|e| AuthError::Db(format!("audit insert: {e}")))?;
    Ok(())
}
```

- [ ] **Step U1-C.2: Register `pub mod audit;` in `crates/auth/src/store/mod.rs`**

- [ ] **Step U1-C.3: Create `crates/auth/src/audit.rs`**

```rust
//! Structured audit-event emission.
//!
//! Every event lands in both: PG `auth.audit_events` (for query/retention)
//! and stdout JSON (for SIEM ingestion, per proposal §15).

use compio_postgres::Client;
use serde_json::{json, Value};

use crate::error::Result;
use crate::store::audit as store;

#[derive(Debug, Default)]
pub struct AuditEvent<'a> {
    pub event_type: &'a str,
    pub outcome: &'a str, // "success" | "failure"
    pub user_id: Option<&'a uuid::Uuid>,
    pub client_id: Option<&'a str>,
    pub request_id: Option<&'a str>,
    pub ip: Option<std::net::IpAddr>,
    pub user_agent: Option<&'a str>,
    pub auth_method: Option<&'a str>,
    pub detail: Value,
}

/// Emit an audit event. Failure to insert is logged but does NOT propagate —
/// audit must never block the user-facing request.
pub async fn emit(conn: &Client, ev: &AuditEvent<'_>) {
    // stdout fan-out first (cheap, can't fail).
    let stdout_payload = json!({
        "type":         ev.event_type,
        "outcome":      ev.outcome,
        "user_id":      ev.user_id,
        "client_id":    ev.client_id,
        "request_id":   ev.request_id,
        "ip":           ev.ip.map(|i| i.to_string()),
        "user_agent":   ev.user_agent,
        "auth_method":  ev.auth_method,
        "detail":       ev.detail,
    });
    tracing::info!(target: "auth.audit", payload = %stdout_payload, "audit event");

    // PG insert.
    if let Err(e) = store::insert(
        conn,
        ev.event_type,
        ev.outcome,
        ev.user_id,
        ev.client_id,
        ev.request_id,
        ev.ip,
        ev.user_agent,
        ev.auth_method,
        &ev.detail,
    )
    .await
    {
        tracing::error!(error = %e, event_type = ev.event_type, "audit PG insert failed");
    }
}

/// Convenience: a `Result`-returning variant for cases where audit failure
/// IS a real error (e.g., admin-grade events that absolutely need a row).
/// Phase 2 doesn't use this; reserved for later.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG insert failure.
pub async fn emit_strict(conn: &Client, ev: &AuditEvent<'_>) -> Result<()> {
    let stdout_payload = json!({
        "type": ev.event_type, "outcome": ev.outcome,
        "user_id": ev.user_id, "client_id": ev.client_id,
        "detail": ev.detail,
    });
    tracing::info!(target: "auth.audit", payload = %stdout_payload, "audit event (strict)");

    store::insert(
        conn,
        ev.event_type,
        ev.outcome,
        ev.user_id,
        ev.client_id,
        ev.request_id,
        ev.ip,
        ev.user_agent,
        ev.auth_method,
        &ev.detail,
    )
    .await
}
```

- [ ] **Step U1-C.4: Export `pub mod audit;` in `crates/auth/src/lib.rs`**

- [ ] **Step U1-C.5: Build + commit**

No unit test for this one — it's pure delegation. The `emit` path is covered transitively by the e2e_password test in U7 (which asserts an `audit_events` row appears after login).

```bash
cargo check -p zeroship-auth
git add crates/auth/src/{audit.rs,store/audit.rs,store/mod.rs,lib.rs}
git commit -m "auth: audit — structured event emission (PG + stdout JSON)"
```

## U1-D · User CRUD store

**Files:**
- Create: `crates/auth/src/store/users.rs`
- Modify: `crates/auth/src/store/mod.rs`

- [ ] **Step U1-D.1: Create `crates/auth/src/store/users.rs`**

```rust
//! `auth.users` CRUD.

use compio_postgres::Client;

use crate::error::{AuthError, Result};

#[derive(Debug, Clone)]
pub struct UserRow {
    pub id: uuid::Uuid,
    pub email: String,
    pub email_verified_at: Option<chrono::DateTime<chrono::Utc>>,
    pub name: String,
    pub avatar_url: Option<String>,
    pub password_hash: Option<String>,
    pub locked_until: Option<chrono::DateTime<chrono::Utc>>,
}

/// Look up a user by email. Returns `None` if not found.
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn find_by_email(conn: &Client, email: &str) -> Result<Option<UserRow>> {
    let rows = conn
        .query(
            "SELECT id, email::text, email_verified_at, name, avatar_url, password_hash, locked_until \
             FROM auth.users WHERE email = $1",
            &[&email],
        )
        .await
        .map_err(|e| AuthError::Db(format!("users find_by_email: {e}")))?;
    Ok(rows.first().map(row_to_user))
}

/// Insert a new user. Returns the created row.
///
/// # Errors
///
/// Returns `AuthError::Db` on conflict (e.g., duplicate email) or other PG failure.
pub async fn create(
    conn: &Client,
    email: &str,
    name: &str,
    password_hash: Option<&str>,
) -> Result<UserRow> {
    let rows = conn
        .query(
            "INSERT INTO auth.users (email, name, password_hash) \
             VALUES ($1, $2, $3) \
             RETURNING id, email::text, email_verified_at, name, avatar_url, password_hash, locked_until",
            &[&email, &name, &password_hash],
        )
        .await
        .map_err(|e| {
            if let Some(db_err) = e.as_db_error() {
                if db_err.code().code() == "23505" {
                    return AuthError::Db("email already registered".into());
                }
            }
            AuthError::Db(format!("users create: {e}"))
        })?;
    let row = rows.first().ok_or_else(|| AuthError::Db("users create: no row returned".into()))?;
    Ok(row_to_user(row))
}

/// Bump `last_login_at` to NOW().
///
/// # Errors
///
/// Returns `AuthError::Db` on PG failure.
pub async fn touch_last_login(conn: &Client, id: uuid::Uuid) -> Result<()> {
    conn.execute(
        "UPDATE auth.users SET last_login_at = NOW(), updated_at = NOW() WHERE id = $1",
        &[&id],
    )
    .await
    .map_err(|e| AuthError::Db(format!("users touch_last_login: {e}")))?;
    Ok(())
}

fn row_to_user(row: &compio_postgres::Row) -> UserRow {
    UserRow {
        id: row.get("id"),
        email: row.get::<_, String>("email"),
        email_verified_at: row.try_get("email_verified_at").ok(),
        name: row.get("name"),
        avatar_url: row.try_get("avatar_url").ok(),
        password_hash: row.try_get("password_hash").ok(),
        locked_until: row.try_get("locked_until").ok(),
    }
}
```

- [ ] **Step U1-D.2: Add `chrono` workspace dep**

If `chrono` isn't already in `crates/auth/Cargo.toml`, add it:

```toml
chrono = { workspace = true, features = ["serde"] }
```

(It's likely already workspace-pinned from other crates. Check root `Cargo.toml`.)

- [ ] **Step U1-D.3: Register `pub mod users;` in `crates/auth/src/store/mod.rs`**

- [ ] **Step U1-D.4: Build + commit**

```bash
cargo check -p zeroship-auth
git add crates/auth/Cargo.toml crates/auth/src/store/{users.rs,mod.rs}
git commit -m "auth: store/users — find_by_email + create + touch_last_login"
```

---

# Unit U2 · HTTP safety primitives

Three small modules. All run before any handler logic touches user data.

## U2-A · IdP session store + cookie

**Files:**
- Create: `crates/auth/src/store/sessions.rs`
- Create: `crates/auth/src/sessions/mod.rs`
- Create: `crates/auth/src/sessions/login.rs`
- Modify: `crates/auth/src/store/mod.rs`
- Modify: `crates/auth/src/lib.rs`

- [ ] **Step U2-A.1: Create `crates/auth/src/store/sessions.rs`**

```rust
//! `auth.sessions` CRUD — the IdP login session at auth.zeroship.ai.

use compio_postgres::Client;

use crate::error::{AuthError, Result};

#[derive(Debug, Clone)]
pub struct Session {
    pub id: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub auth_method: String,
    pub amr: Vec<String>,
    pub acr: Option<String>,
    pub idle_expires_at: chrono::DateTime<chrono::Utc>,
    pub abs_expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug)]
pub struct CreateSession<'a> {
    pub user_id: uuid::Uuid,
    pub auth_method: &'a str,
    pub amr: Vec<String>,
    pub acr: Option<&'a str>,
    pub idle_minutes: i64,
    pub absolute_hours: i64,
}

/// Insert a new session row.
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn create(conn: &Client, params: &CreateSession<'_>) -> Result<Session> {
    let rows = conn
        .query(
            "INSERT INTO auth.sessions \
                (user_id, auth_method, amr, acr, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3, $4, NOW() + ($5::text || ' minutes')::interval, \
                                            NOW() + ($6::text || ' hours')::interval) \
             RETURNING id, user_id, auth_method, amr, acr, idle_expires_at, abs_expires_at",
            &[
                &params.user_id,
                &params.auth_method,
                &params.amr,
                &params.acr,
                &params.idle_minutes.to_string(),
                &params.absolute_hours.to_string(),
            ],
        )
        .await
        .map_err(|e| AuthError::Db(format!("sessions create: {e}")))?;
    let row = rows.first().ok_or_else(|| AuthError::Db("sessions create: empty return".into()))?;
    Ok(Session {
        id: row.get("id"),
        user_id: row.get("user_id"),
        auth_method: row.get("auth_method"),
        amr: row.get("amr"),
        acr: row.try_get("acr").ok(),
        idle_expires_at: row.get("idle_expires_at"),
        abs_expires_at: row.get("abs_expires_at"),
    })
}

/// Revoke a session (set `revoked_at = NOW()`).
///
/// # Errors
///
/// `AuthError::Db` on PG failure.
pub async fn revoke(conn: &Client, id: uuid::Uuid) -> Result<()> {
    conn.execute("UPDATE auth.sessions SET revoked_at = NOW() WHERE id = $1", &[&id])
        .await
        .map_err(|e| AuthError::Db(format!("sessions revoke: {e}")))?;
    Ok(())
}
```

- [ ] **Step U2-A.2: Register `pub mod sessions;` in `store/mod.rs`**

- [ ] **Step U2-A.3: Create `crates/auth/src/sessions/mod.rs`**

```rust
//! IdP session management (the cookie at auth.zeroship.ai).
//!
//! Distinct from hydra's session and from per-RP sessions. We track
//! "this browser holds a verified zeroship user"; hydra tracks "an
//! OIDC subject has been confirmed at the AS". The two coordinate
//! through hydra's accept_login call.

pub mod login;
```

- [ ] **Step U2-A.4: Create `crates/auth/src/sessions/login.rs`**

```rust
//! IdP login session cookie at `auth.zeroship.ai`. Cookie name:
//! `__Host-zsidp_session`. 12 h hard absolute, 30 min sliding idle.

pub const COOKIE_NAME: &str = "__Host-zsidp_session";

pub const IDLE_MINUTES: i64 = 30;
pub const ABSOLUTE_HOURS: i64 = 12;

/// Build the `Set-Cookie` header value for the IdP session.
///
/// `insecure_dev = true` drops the `Secure` flag (so localhost HTTP works).
/// In production this MUST be false.
#[must_use]
pub fn set_cookie(session_id: &uuid::Uuid, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    let max_age = ABSOLUTE_HOURS * 3600;
    format!("{COOKIE_NAME}={session_id}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={max_age}")
}

/// Clear the session cookie.
#[must_use]
pub fn clear_cookie(insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

/// Parse the session id from a request's `Cookie` header value.
#[must_use]
pub fn parse_cookie(cookie_header: &str) -> Option<uuid::Uuid> {
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
            return uuid::Uuid::parse_str(rest).ok();
        }
    }
    None
}
```

- [ ] **Step U2-A.5: Export `pub mod sessions;` in `lib.rs`**

- [ ] **Step U2-A.6: Unit-test cookie shape**

Add to `crates/auth/src/sessions/login.rs` at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_cookie_has_secure_in_prod() {
        let id = uuid::Uuid::new_v4();
        let c = set_cookie(&id, false);
        assert!(c.contains("Secure"), "prod cookie must have Secure: {c}");
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Max-Age=43200")); // 12h * 3600
    }

    #[test]
    fn set_cookie_drops_secure_in_dev() {
        let id = uuid::Uuid::new_v4();
        let c = set_cookie(&id, true);
        assert!(!c.contains("Secure"), "dev cookie must NOT have Secure: {c}");
    }

    #[test]
    fn parses_cookie() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; __Host-zsidp_session={id}; baz=qux");
        assert_eq!(parse_cookie(&header), Some(id));
        assert_eq!(parse_cookie("nothing-here"), None);
    }
}
```

Run: `cargo test -p zeroship-auth sessions::login::tests`. Expected: 3 PASS.

- [ ] **Step U2-A.7: Commit**

```bash
git add crates/auth/src/{sessions/,store/sessions.rs,store/mod.rs,lib.rs}
git commit -m "auth: sessions/login — __Host-zsidp_session cookie + auth.sessions store"
```

## U2-B · CSRF double-submit helper

**File:**
- Create: `crates/auth/src/csrf.rs`
- Modify: `crates/auth/src/lib.rs`

- [ ] **Step U2-B.1: Create `crates/auth/src/csrf.rs`**

```rust
//! Double-submit CSRF token. The IdP's own login/signup/consent forms
//! carry a `csrf` form field that MUST match a `__Host-zsidp_csrf` cookie.
//!
//! Cookie is **NOT** HttpOnly — the inline `<script nonce>` reads it for
//! the hidden form field (that's the "double-submit" pattern).

use rand::Rng;

pub const COOKIE_NAME: &str = "__Host-zsidp_csrf";
const TOKEN_LEN_BYTES: usize = 16;
const MAX_AGE_SECS: i64 = 3600;

/// Generate a fresh token (128-bit base64url).
#[must_use]
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_LEN_BYTES];
    rand::thread_rng().fill(&mut bytes);
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the `Set-Cookie` header value.
#[must_use]
pub fn set_cookie(token: &str, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{COOKIE_NAME}={token}; Path=/; SameSite=Strict{secure}; Max-Age={MAX_AGE_SECS}")
}

/// Parse the CSRF token from a Cookie header.
#[must_use]
pub fn parse_cookie(cookie_header: &str) -> Option<String> {
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&format!("{COOKIE_NAME}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Constant-time comparison of the form-field token vs the cookie token.
/// Returns true only on exact byte match.
#[must_use]
pub fn matches(form_token: &str, cookie_token: &str) -> bool {
    if form_token.len() != cookie_token.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in form_token.bytes().zip(cookie_token.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}
```

Add `base64` to deps if not already there (it's workspace-pinned per Phase 1's existing usage).

- [ ] **Step U2-B.2: Export from `lib.rs`**

- [ ] **Step U2-B.3: Unit test**

Append to `csrf.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact() {
        let t = generate_token();
        assert!(matches(&t, &t));
    }

    #[test]
    fn rejects_mismatch() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
        assert!(!matches(&a, &b));
    }

    #[test]
    fn rejects_length_mismatch() {
        assert!(!matches("short", "much-longer-than-short"));
    }
}
```

Run + commit:

```bash
cargo test -p zeroship-auth csrf::tests
git add crates/auth/src/{csrf.rs,lib.rs}
git commit -m "auth: csrf — double-submit token + constant-time compare"
```

## U2-C · Security headers middleware

**Files:**
- Create: `crates/auth/src/headers.rs`
- Modify: `crates/auth/src/lib.rs`

- [ ] **Step U2-C.1: Create `crates/auth/src/headers.rs`**

```rust
//! Security headers applied to every `crates/auth` response. Hydra
//! sets its own on /oauth2/* responses.

use ntex::http::header::{HeaderName, HeaderValue};
use ntex::http::HeaderMap;

/// Apply the standard security headers to an outgoing response's header map.
pub fn apply(headers: &mut HeaderMap) {
    static_set(headers, "strict-transport-security",
        "max-age=63072000; includeSubDomains; preload");
    static_set(headers, "x-frame-options", "DENY");
    static_set(headers, "x-content-type-options", "nosniff");
    static_set(headers, "referrer-policy", "no-referrer");
    static_set(headers, "permissions-policy",
        "camera=(), microphone=(), geolocation=(), payment=(), \
         publickey-credentials-get=(self), interest-cohort=()");
    static_set(headers, "cross-origin-opener-policy", "same-origin");
    static_set(headers, "cross-origin-resource-policy", "same-origin");
    static_set(headers, "cache-control", "no-store");

    // CSP — same shape as proposal §14. `'nonce-...'` and per-page hardening
    // are added by handlers that render inline scripts; the baseline blocks
    // everything else.
    static_set(headers, "content-security-policy",
        "default-src 'self'; \
         script-src 'self'; \
         style-src 'self'; \
         img-src 'self' data: https://*.zeroship.ai \
                       https://lh3.googleusercontent.com \
                       https://avatars.githubusercontent.com; \
         connect-src 'self'; \
         form-action 'self'; \
         frame-ancestors 'none'; \
         base-uri 'none'; \
         object-src 'none'; \
         upgrade-insecure-requests");
}

fn static_set(headers: &mut HeaderMap, name: &'static str, value: &'static str) {
    headers.insert(
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    );
}
```

- [ ] **Step U2-C.2: Export from `lib.rs` and use in `server.rs`**

In `server.rs`, after the response is built, apply security headers. The cleanest ntex way is a middleware; for Phase 2 a per-handler call is acceptable since we have only a small number of handlers. Phase 2 Unit U6 will install it as middleware. For now, just expose the function.

- [ ] **Step U2-C.3: Build + commit**

```bash
cargo check -p zeroship-auth
git add crates/auth/src/{headers.rs,lib.rs}
git commit -m "auth: headers — security-header bundle (CSP + HSTS + frame-ancestors + COOP/CORP)"
```

---

# Unit U3 · Askama templates + base layout

**Files:**
- Modify: `crates/auth/Cargo.toml` (add askama)
- Create: `crates/auth/src/ui/mod.rs`
- Create: `crates/auth/src/ui/templates/base.html`
- Create: `crates/auth/src/ui/templates/login.html`
- Create: `crates/auth/src/ui/templates/signup.html`
- Create: `crates/auth/src/ui/templates/error.html`
- Create: `crates/auth/static/style.css`
- Modify: `crates/auth/src/lib.rs`

- [ ] **Step U3.1: Add askama dep**

In `crates/auth/Cargo.toml`:

```toml
askama = { version = "0.12", features = ["with-ntex"] }
```

(Adjust feature name to whatever matches the workspace pin. The `with-ntex` integration may not exist; if so, drop features and use askama's raw `render()` method which returns `Result<String>` directly.)

- [ ] **Step U3.2: Create `crates/auth/src/ui/mod.rs`**

```rust
//! Server-rendered HTML UI for the IdP. Templates compiled via askama.

pub mod templates;

use askama::Template;

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginPage<'a> {
    pub challenge: &'a str,
    pub csrf: &'a str,
    pub error: Option<&'a str>,
    pub client_name: &'a str,
}

#[derive(Template)]
#[template(path = "signup.html")]
pub struct SignupPage<'a> {
    pub challenge: &'a str,
    pub csrf: &'a str,
    pub error: Option<&'a str>,
}

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorPage<'a> {
    pub error: &'a str,
    pub error_description: Option<&'a str>,
}
```

Also create `crates/auth/src/ui/templates/mod.rs`:

```rust
//! Compile-time-checked askama templates. Source lives under `templates/`.
```

But more practically, askama looks for templates under `templates/` at the crate root by default. Set the path via `[package.metadata.askama]` in `Cargo.toml`:

```toml
[package.metadata.askama]
dirs = ["src/ui/templates"]
```

Or use the absolute `template(path = ...)` attribute; consult the askama docs version your workspace pins.

- [ ] **Step U3.3: Create `crates/auth/src/ui/templates/base.html`**

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{% block title %}zeroship{% endblock %}</title>
  <link rel="stylesheet" href="/static/style.css">
</head>
<body>
  <main class="auth-shell">
    <header>
      <h1>zeroship</h1>
    </header>
    <section>
      {% block body %}{% endblock %}
    </section>
  </main>
</body>
</html>
```

- [ ] **Step U3.4: Create `login.html`**

```html
{% extends "base.html" %}
{% block title %}Sign in · zeroship{% endblock %}
{% block body %}
<h2>Sign in to {{ client_name }}</h2>
{% if let Some(err) = error %}
  <div class="error">{{ err }}</div>
{% endif %}
<form method="POST" action="/login?login_challenge={{ challenge }}">
  <input type="hidden" name="csrf" value="{{ csrf }}">
  <label>Email
    <input type="email" name="email" autocomplete="username" required autofocus>
  </label>
  <label>Password
    <input type="password" name="password" autocomplete="current-password" required>
  </label>
  <button type="submit">Sign in</button>
</form>
<p><a href="/signup?login_challenge={{ challenge }}">Create an account</a></p>
{% endblock %}
```

- [ ] **Step U3.5: Create `signup.html`**

```html
{% extends "base.html" %}
{% block title %}Sign up · zeroship{% endblock %}
{% block body %}
<h2>Create your zeroship account</h2>
{% if let Some(err) = error %}
  <div class="error">{{ err }}</div>
{% endif %}
<form method="POST" action="/signup?login_challenge={{ challenge }}">
  <input type="hidden" name="csrf" value="{{ csrf }}">
  <label>Name
    <input name="name" autocomplete="name" required autofocus>
  </label>
  <label>Email
    <input type="email" name="email" autocomplete="email" required>
  </label>
  <label>Password (15+ characters)
    <input type="password" name="password" minlength="15" autocomplete="new-password" required>
  </label>
  <button type="submit">Sign up</button>
</form>
{% endblock %}
```

- [ ] **Step U3.6: Create `error.html`**

```html
{% extends "base.html" %}
{% block title %}Error · zeroship{% endblock %}
{% block body %}
<h2>{{ error }}</h2>
{% if let Some(desc) = error_description %}
  <p>{{ desc }}</p>
{% endif %}
<p><a href="/login">Back to sign in</a></p>
{% endblock %}
```

- [ ] **Step U3.7: Create `crates/auth/static/style.css`**

```css
:root { color-scheme: light dark; }
body { font-family: ui-sans-serif, system-ui, sans-serif; max-width: 28rem; margin: 4rem auto; padding: 0 1rem; }
h1 { font-size: 1rem; opacity: 0.6; margin-bottom: 2rem; }
h2 { font-size: 1.5rem; margin-bottom: 1.5rem; }
form { display: grid; gap: 1rem; }
label { display: grid; gap: 0.25rem; font-size: 0.875rem; }
input { padding: 0.5rem 0.75rem; border: 1px solid currentColor; border-radius: 0.25rem; font-size: 1rem; background: transparent; }
button { padding: 0.5rem 1rem; border: 0; background: #111; color: white; border-radius: 0.25rem; font-size: 1rem; cursor: pointer; }
.error { padding: 0.75rem; border: 1px solid #c00; border-radius: 0.25rem; color: #c00; background: #fee; font-size: 0.875rem; }
a { color: inherit; }
p { font-size: 0.875rem; margin-top: 1rem; }
```

- [ ] **Step U3.8: Export `pub mod ui;` in `lib.rs`**

- [ ] **Step U3.9: Verify templates compile**

```
cargo check -p zeroship-auth
```

If askama errors, the error message tells you which template line failed.

- [ ] **Step U3.10: Commit**

```bash
git add crates/auth/Cargo.toml crates/auth/src/ui/ crates/auth/static/
git commit -m "auth: ui — askama templates for login/signup/error + base layout + CSS"
```

---

# Unit U4 · Login + signup handlers

The substantive handlers. They consume `login_challenge`, run the auth flow, accept the challenge with hydra, and redirect.

## U4-A · `/login` GET handler

**Files:**
- Create: `crates/auth/src/ui/login.rs`
- Modify: `crates/auth/src/ui/mod.rs`

The GET handler:

1. Read `login_challenge` from query.
2. Call `admin.get_login(challenge)`.
3. If `skip == true` (hydra already has a session for this subject): immediately call `admin.accept_login` with the same subject and redirect. No UI.
4. Else: render the login form with a fresh CSRF token.

- [ ] **Step U4-A.1: Create `crates/auth/src/ui/login.rs`**

```rust
//! /login GET handler.

use ntex::http::header::{HeaderValue, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use std::sync::Arc;

use crate::config::AuthConfig;
use crate::csrf;
use crate::hydra_client::HydraAdmin;
use crate::hydra_client::types::AcceptLoginRequest;
use crate::ui::LoginPage;
use askama::Template;

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    pub login_challenge: String,
}

pub async fn get(
    req: HttpRequest,
    query: ntex::web::types::Query<LoginQuery>,
    admin: ntex::web::types::Data<HydraAdmin>,
    cfg: ntex::web::types::Data<Arc<AuthConfig>>,
) -> HttpResponse {
    let _ = req; // for headers extraction in later phases
    let challenge = &query.login_challenge;

    // Fetch challenge details from hydra.
    let info = match admin.get_login(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "login challenge fetch failed");
            return render_error("invalid login request", Some(&e.to_string()));
        }
    };

    // Skip path: hydra already knows the subject.
    if info.skip {
        let accept = AcceptLoginRequest {
            subject: info.subject.clone(),
            remember: Some(true),
            remember_for: Some(3600),
            ..Default::default()
        };
        match admin.accept_login(challenge, &accept).await {
            Ok(resp) => return redirect(&resp.redirect_to),
            Err(e) => {
                tracing::error!(error = %e, "accept_login (skip path) failed");
                return render_error("internal error", Some(&e.to_string()));
            }
        }
    }

    // Render the form with a fresh CSRF token cookie.
    let csrf_token = csrf::generate_token();
    let page = LoginPage {
        challenge,
        csrf: &csrf_token,
        error: None,
        client_name: info.client.client_name.as_deref().unwrap_or(&info.client.client_id),
    };
    let body = match page.render() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "render login.html failed");
            return render_error("internal error", Some("template render"));
        }
    };

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

fn redirect(to: &str) -> HttpResponse {
    let mut resp = HttpResponse::Found();
    resp.header(LOCATION, HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/")));
    resp.finish()
}

fn render_error(error: &str, error_description: Option<&str>) -> HttpResponse {
    use crate::ui::ErrorPage;
    let page = ErrorPage { error, error_description };
    let body = page.render().unwrap_or_else(|_| format!("<h1>{error}</h1>"));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}
```

- [ ] **Step U4-A.2: Export from `ui/mod.rs`**

Add `pub mod login;` at the top of `crates/auth/src/ui/mod.rs`.

- [ ] **Step U4-A.3: Build**

```
cargo check -p zeroship-auth
```

- [ ] **Step U4-A.4: Commit**

```bash
git add crates/auth/src/ui/{login.rs,mod.rs}
git commit -m "auth: /login GET — reads hydra challenge, handles skip path, renders form"
```

## U4-B · `/login` POST + `/signup` GET/POST handlers

The POST is the substantive piece. Steps inside the POST handler:

1. Verify CSRF (form field vs cookie).
2. Look up user by email.
3. Run password verify (against real hash if user exists, against dummy hash if not — constant time either way).
4. Apply rate limits (per-email-IP, per-email, per-IP). On throttle: 429 with retry-after, audit `login_failure`.
5. On password mismatch: audit `login_failure`, redirect back to `/login` with `?error=invalid_credentials`.
6. On success: create `auth.sessions` row, set cookie, audit `login_success`, call `admin.accept_login(challenge, { subject: user_id, acr, amr: ["pwd"] })`, redirect to `redirect_to`.

For brevity here, the implementer should follow §8.1 of the proposal closely and the U4-A pattern. The full handler is ~100 LOC.

- [ ] **Step U4-B.1: Implement `crates/auth/src/ui/login.rs::post`**

(Implementer: extend the file from U4-A with a `post` function. Use `ntex::web::types::Form` for the body. Borrow shared state via `Data<...>`.)

- [ ] **Step U4-B.2: Create `crates/auth/src/ui/signup.rs`** — GET renders the form, POST validates (email format, password ≥ 15 chars), creates `auth.users`, audits, then redirects back to `/login?login_challenge=...` so the user can sign in immediately. (Email-verification flow is Phase 5; for Phase 2 we just create the row and let the user sign in.)

- [ ] **Step U4-B.3: Add `pub mod signup;` to `ui/mod.rs`**

- [ ] **Step U4-B.4: Build + verify**

```
cargo check -p zeroship-auth
cargo clippy -p zeroship-auth --no-deps --tests
```

- [ ] **Step U4-B.5: Commit**

```bash
git add crates/auth/src/ui/{login.rs,signup.rs,mod.rs}
git commit -m "auth: /login POST + /signup GET/POST — credential flow + accept_login"
```

---

# Unit U5 · Consent handler (skip-consent fast path)

**Files:**
- Create: `crates/auth/src/ui/consent.rs`
- Modify: `crates/auth/src/ui/mod.rs`

For Phase 2 we only handle the **first-party skip-consent** path. Third-party consent UI is Phase 4+.

- [ ] **Step U5.1: Create `crates/auth/src/ui/consent.rs`**

```rust
//! /consent GET handler. Phase 2: first-party clients (skip_consent=true).
//! Third-party consent UI is Phase 4+.

use ntex::http::header::{HeaderValue, LOCATION};
use ntex::web::{HttpResponse, types::{Data, Query}};
use serde::Deserialize;
use serde_json::json;

use crate::hydra_client::HydraAdmin;
use crate::hydra_client::types::{AcceptConsentRequest, ConsentSession};

#[derive(Debug, Deserialize)]
pub struct ConsentQuery {
    pub consent_challenge: String,
}

pub async fn get(
    query: Query<ConsentQuery>,
    admin: Data<HydraAdmin>,
) -> HttpResponse {
    let challenge = &query.consent_challenge;

    let info = match admin.get_consent(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "consent challenge fetch failed");
            return error_response(&e.to_string());
        }
    };

    // Phase 2: only handle the skip path. Third-party UI lands in Phase 4.
    if !info.client.skip_consent {
        return error_response("third-party consent UI not yet implemented");
    }

    let accept = AcceptConsentRequest {
        grant_scope: info.requested_scope.clone(),
        grant_access_token_audience: info.requested_access_token_audience.clone(),
        remember: Some(true),
        remember_for: Some(3600),
        session: Some(ConsentSession {
            id_token: Some(json!({
                // Phase 2 only ships subject/email/name in the ID token.
                // Email/name come from the user row at /login time, but the
                // simplest path is to look them up here. For Phase 2 we keep
                // the ID token claims minimal and let RPs hit /userinfo for
                // the rest.
            })),
            access_token: None, // intentionally empty per §13 row "session.access_token leakage"
        }),
    };

    match admin.accept_consent(challenge, &accept).await {
        Ok(resp) => {
            let mut r = HttpResponse::Found();
            r.header(LOCATION, HeaderValue::from_str(&resp.redirect_to)
                .unwrap_or_else(|_| HeaderValue::from_static("/")));
            r.finish()
        }
        Err(e) => {
            tracing::error!(error = %e, "accept_consent failed");
            error_response(&e.to_string())
        }
    }
}

fn error_response(msg: &str) -> HttpResponse {
    use askama::Template;
    use crate::ui::ErrorPage;
    let page = ErrorPage { error: "Consent failed", error_description: Some(msg) };
    let body = page.render().unwrap_or_else(|_| format!("<h1>{msg}</h1>"));
    let mut r = HttpResponse::Ok();
    r.content_type("text/html; charset=utf-8");
    r.body(body)
}
```

- [ ] **Step U5.2: Build + commit**

```
cargo check -p zeroship-auth
git add crates/auth/src/ui/{consent.rs,mod.rs}
git commit -m "auth: /consent GET — first-party skip-consent fast path"
```

---

# Unit U6 · Wire handlers into `server::configure`

**File:**
- Modify: `crates/auth/src/server.rs`

- [ ] **Step U6.1: Wire routes + shared state**

`server::configure` needs:
- `Data<HydraAdmin>` (the admin client)
- `Data<Arc<AuthConfig>>` (for `insecure_dev` and `hydra_admin` URL)
- `Data<Arc<compio_postgres::Client>>` (for sessions, users, audit)

Update `server::run(cfg: AuthConfig, admin: HydraAdmin, db: compio_postgres::Client)` to take all three, wrap them in `Arc` / `Data` inside the closure, and register:

```rust
.service(crate::ui::login::get)         // GET /login
.service(crate::ui::login::post)        // POST /login
.service(crate::ui::signup::get)        // GET /signup
.service(crate::ui::signup::post)       // POST /signup
.service(crate::ui::consent::get)       // GET /consent
.service(static_handler)                // GET /static/*
.service(healthz).service(readyz)
```

`static_handler`: serves the single `style.css` file (mount under `/static/`). In ntex, use `ntex_files::Files::new("/static", "crates/auth/static")` if `ntex-files` is in deps, or hand-roll a tiny handler that reads the file and serves it with `Content-Type: text/css`. For Phase 2 simplicity, hand-roll:

```rust
#[ntex::web::get("/static/style.css")]
async fn style() -> ntex::web::HttpResponse {
    const CSS: &str = include_str!("../static/style.css");
    let mut r = ntex::web::HttpResponse::Ok();
    r.content_type("text/css; charset=utf-8");
    r.body(CSS)
}
```

- [ ] **Step U6.2: Update `main.rs` to pass admin + db into `server::run`**

The current main creates `HydraAdmin` for bootstrap. Re-use it for the server. Pass the PG `Client` similarly.

- [ ] **Step U6.3: Build + commit**

```
cargo check -p zeroship-auth
cargo clippy -p zeroship-auth --no-deps --tests
git add crates/auth/src/{server.rs,main.rs}
git commit -m "auth: server — register login/signup/consent routes + static CSS + state"
```

---

# Unit U7 · `e2e_password` — the acceptance test

**File:**
- Create: `crates/auth/tests/e2e_password.rs`

This is the central proof Phase 2 is done. It:

1. Skips if `AUTH_DB_URL` and `AUTH_HYDRA_ADMIN` are not both set.
2. Boots `crates/auth` (in-process via `server::run` on a random port, OR runs against a pre-started binary — your call; in-process is faster).
3. Registers a test OIDC client with hydra (`POST /admin/clients`).
4. Drives the OIDC code flow as a browser would:
   - GET `http://localhost:4444/oauth2/auth?...` (follow redirect to /login)
   - GET `http://<auth-port>/login?login_challenge=...` (extract CSRF)
   - POST `/signup` with email/password/name
   - POST `/login` with email/password (csrf)
   - Follow 302 back to hydra
   - GET hydra's redirect to `/consent?consent_challenge=...`
   - Follow 302 back to hydra
   - Receive the authorization code at the test client's redirect_uri
   - POST `/oauth2/token` with code+verifier
   - Decode the ID token, verify claims (sub == user.id, email, etc.)
5. Cleans up: deletes the test client + test user.

This is ~250 LOC. Use `cyper` for the HTTP. Use `jsonwebtoken` (already a workspace dep) for ID-token decode.

The full implementation is substantial; the implementer should pattern off `crates/auth/tests/hydra_client_smoke.rs` and refer to §2.2 of the proposal for the end-user login flow sequence.

- [ ] **Step U7.1: Write the test scaffold (skip path)**

Boilerplate + the skip check + boot-in-process. Run, confirm skip path passes.

- [ ] **Step U7.2: Implement the OIDC dance**

The bulk of the work. Each phase of the flow is one HTTP call + assert.

- [ ] **Step U7.3: Run against the live stack**

```
docker compose up -d hydra
AUTH_DB_URL=postgres://postgres:zeroship@localhost:5441/zeroship \
AUTH_HYDRA_ADMIN=http://localhost:4445 \
cargo test -p zeroship-auth --test e2e_password -- --nocapture
```

Expected: 1 PASS, ID token decoded with expected claims.

- [ ] **Step U7.4: Commit**

```bash
git add crates/auth/tests/e2e_password.rs
git commit -m "auth: e2e_password — full OIDC code+PKCE flow against live hydra"
```

---

# Unit U8 · `enum_defense` + threat_model parametric tests

**Files:**
- Create: `crates/auth/tests/enum_defense.rs`
- Create: `crates/auth/tests/threat_model.rs`

## enum_defense.rs

Hits POST /login with N requests, half against existing users, half against random emails. Asserts response body/status/timing distributions are statistically indistinguishable.

## threat_model.rs

Parametric harness over proposal §13 rows tagged `[ours]`. Each row drives a `#[test]` that exercises the attacker pattern and asserts the mitigation fires:

- CSRF: form without csrf field or with mismatched csrf rejected
- Session fixation: post-login session id differs from pre-login
- Brute force: 6th attempt within 15 min on same (email, ip) gets 429
- Clickjacking: response has `frame-ancestors 'none'`
- Open redirect: PR has been made for hydra (out of our scope; mark as such)

- [ ] **Step U8.1: Write enum_defense (skip-when-env-unset pattern)**
- [ ] **Step U8.2: Write threat_model parametric tests**
- [ ] **Step U8.3: Run + commit**

```bash
cargo test -p zeroship-auth --test enum_defense --test threat_model
git add crates/auth/tests/{enum_defense.rs,threat_model.rs}
git commit -m "auth: tests — enum_defense (timing) + threat_model parametric §13"
```

---

# Unit U9 · Phase 2 close-out

- [ ] **Step U9.1: Run the full suite**

```
cargo test -p zeroship-auth
AUTH_DB_URL=... AUTH_HYDRA_ADMIN=... cargo test -p zeroship-auth
```

All tests pass.

- [ ] **Step U9.2: Clippy pass**

```
cargo clippy -p zeroship-auth --no-deps --tests
```

No NEW warnings beyond the workspace-tolerated set (the 51 pedantic-tier from Phase 1, possibly +/- a few from Phase 2's additions).

- [ ] **Step U9.3: Milestone commit + tag**

```bash
git commit --allow-empty -m "auth: Phase 2 complete — password login UX

Closes Phase 2 of the auth-server proposal:
- Argon2id password hashing + dummy-hash enumeration defense
- PG-backed token-bucket rate limiter (3 buckets per §8.1)
- IdP login session cookie + auth.sessions store
- Double-submit CSRF
- Security headers middleware
- Askama templates (login, signup, consent skip, error) + minimal CSS
- /login + /signup + /consent handlers wired to hydra admin API
- e2e_password test: full OIDC code+PKCE flow against live hydra
- enum_defense + threat_model parametric tests

Phase 3 (gateway + control as OIDC RPs) is next."
git tag auth-phase-2
```

---

# Future phases (separate plans)

- **Phase 3** — Gateway + control plane as OIDC RPs; retire legacy `crates/control/src/auth_*.rs` and `crates/gateway/src/user_auth.rs`. End-to-end via real browser.
- **Phase 4** — Google + GitHub OAuth federation; third-party consent UI.
- **Phase 5** — Magic link, email verification, password reset, mailer abstraction + lettre/resend drivers + bounce webhooks.
- **Phase 6** — Polish: JWK rotation cron, DPoP at the gateway, audit-log retention sweeper, runbook, load test, security review.

# Self-review (run by plan author)

**Spec coverage** — every proposal §8.1 line maps to a U1/U2/U4 task. §9 maps to U2-A. §10 (skip-consent) maps to U5. §13 "ours" rows map to U8.

**Placeholder scan** — no TBDs. The U4-B "implementer extends the file with a post function" is the only place the plan delegates to proposal text rather than embedding full code; this is intentional, the proposal §8.1 has the algorithmic detail.

**Type consistency** — `AuthError`, `Result<T>`, `HydraAdmin`, `OAuth2Client`, `Bucket`, `Session`, `UserRow` all defined once, referenced consistently across units.

**Scope** — Phase 2 ships ONE login method (password). Federation, magic-link, MFA, account-linking — all out of scope. The plan does not silently expand.

# Execution handoff

Plan saved to `docs/superpowers/plans/2026-05-27-auth-server-phase-2-password-login.md`.

To execute via subagent-driven-development: dispatch one subagent per unit (U1 through U9), with U1 broken into four sub-commits (A/B/C/D) and U4 into two (A/B). Each subagent's brief should reference this plan's unit section verbatim.
