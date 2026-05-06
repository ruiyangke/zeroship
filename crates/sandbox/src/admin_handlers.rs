//! Phase-3 admin/operator API for the sandbox controller.
//!
//! See `docs/proposals/sandbox-pg-state.md` § 13 / § 13.5 / § 13.6 /
//! § 13.7 / § 15 Phase 3 for the design. This module is the
//! implementation of the operator-facing query surface + GDPR
//! data-export and data-delete pipelines.
//!
//! ## Auth shape (Phase 3)
//!
//! Phase 3 ships a deliberately-simple bearer-from-file admin auth
//! that mirrors the existing sandbox auth (`SANDBOX_TOKEN`) but uses
//! a separate token + env var so a leaked controller token does NOT
//! grant operator access:
//!
//!   - `SANDBOX_ADMIN_TOKEN_PATH` — file containing the admin bearer.
//!     Mode 0o400 enforced on Unix (mirrors `persist::AeadKey`).
//!   - When the path is unset OR the file is empty, every `/admin/*`
//!     endpoint 503s with `{"error":"admin api disabled"}`. There is
//!     no "default-allow" path; operators opt in explicitly.
//!   - When set, requests must carry `Authorization: Bearer <token>`
//!     where token equals the file contents (constant-time compare).
//!
//! The full design (§ 13.8) calls for short-lived JWT + per-endpoint
//! scopes + 2FA step-up + per-admin rate limit + anomaly detection.
//! Phase 3's bearer-from-file is intentionally narrow — the JWT
//! shape lands as Phase 5 / production hardening; doing it now
//! couples the sandbox crate to the platform JWT verifier (which
//! lives in `crates/control/`) before that contract is finalized.
//!
//! Migration to JWT will:
//!   1. Replace `admin_check` with a JWT verifier that pulls
//!      `admin_id` + `scopes` + `step_up` claims from the bearer.
//!   2. Replace `audit_admin_action`'s hard-coded "operator" actor
//!      with the JWT's `admin_id` claim.
//!   3. Add per-endpoint scope checks at the start of each handler.
//!   4. Add `WWW-Authenticate: Step-Up max_age=300` on 403 from
//!      destructive endpoints when `step_up` is missing/stale.
//!
//! Nothing in this module's wire format blocks the JWT shape —
//! every handler still takes `&HttpRequest` so the auth path can
//! evolve from "match bearer" to "verify JWT + check scope" without
//! touching the SQL or response shapes.
//!
//! ## Endpoints
//!
//! All endpoints are mounted under `/admin/*` and gated on the
//! admin bearer:
//!
//!   - `GET /admin/sandboxes`          list all sandboxes (cross-tenant)
//!   - `GET /admin/sandboxes/{id}`     single sandbox detail (pg row +
//!                                     in-memory state if held + agent
//!                                     `/version` if reachable)
//!   - `GET /admin/users/{user_id}/sandboxes`  per-user sandboxes
//!   - `GET /admin/users/{user_id}/shares`     per-user share metadata
//!   - `GET /admin/users/{user_id}/export`     GDPR data-export
//!   - `DELETE /admin/users/{user_id}`         GDPR cascade delete
//!   - `GET /admin/hosts`              controller fleet status
//!
//! Read endpoints emit no audit events (would explode the log under
//! operator-dashboard polling); the destructive `DELETE` emits a
//! `gdpr.delete_user` event.

use std::sync::Arc;

use compio_postgres::Pool;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::AppState;

type State = web::types::State<Arc<AppState>>;

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 1000;
const EVENT_EXPORT_CAP: i64 = 10_000;

// ────────────────────────────────────────────────────────────────────
// Auth — boot-time bearer cache (Round-3 / Phase-3 CRITICAL #3).
// § 13.8 expansion deferred to Phase 5 production hardening.
// ────────────────────────────────────────────────────────────────────

/// Constant-time bearer compare via SHA-256 digests.
///
/// Round-3 / Phase-3 CRITICAL #1: the original shape returned in O(1) ns
/// on a length mismatch (early `if presented.len() != expected.len()`),
/// while the matching path ran `ct_eq` + JSON allocation taking ~µs —
/// an attacker could bisect the token length from response-time
/// distributions. Round-3's first patch padded both sides to `max(len)`
/// and compared via `subtle::ConstantTimeEq`, but `vec![0u8; max_len]`
/// allocates an attacker-sized buffer on the unauthenticated path:
/// (a) allocator timing depends on `presented.len()` (capped ~8 KiB by
/// ntex via the `Authorization` header), so it isn't actually
/// constant-time at the allocator level; (b) it's a fresh DoS
/// amplifier on the auth path that the boot-cache fix was meant to
/// remove.
///
/// Round-4 fix: hash both inputs with SHA-256 and `ct_eq` the 32-byte
/// digests. SHA-256 is constant-time on a fixed-size finalize buffer;
/// the only length-dependent work is the streaming `update`, whose cost
/// scales with `presented.len()` (capped ~8 KiB) but does NOT branch on
/// `presented` vs. `expected` and is identical for the matching and
/// mismatching paths. After hashing, every code path executes the same
/// 32-byte ct_eq with no length-dependent control flow or allocation.
///
/// Note: the implementation contains no length-dependent control flow
/// or heap allocation past the fixed-size hasher state. This is a
/// stronger property than calling `ct_eq` on the raw bytes, which
/// would still leak length via the early `if a.len() != b.len()` that
/// `subtle` returns inside its `Choice` for unequal-length slices.
fn constant_time_bearer_eq(presented: &[u8], expected: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let p_digest = Sha256::digest(presented);
    let e_digest = Sha256::digest(expected);
    p_digest.ct_eq(&e_digest).into()
}

/// Validates the admin bearer. Returns:
///   - `Ok(())` when the request bears the configured admin token.
///   - `Err(503)` when admin API is disabled (no `SANDBOX_ADMIN_TOKEN_PATH`).
///   - `Err(401)` when the bearer is missing or wrong.
///
/// Round-3 / Phase-3 CRITICAL #3: reads from the boot-cached
/// `state.admin_token` instead of stat()+read()'ing the file per
/// request. Bounded amplification at 10k req/s; no slow-FS DoS;
/// no fail-open on chmod-error (boot-time read fails loud).
///
/// Round-4 / IMPORTANT #2: defense-in-depth empty-token guard.
/// `AppState.admin_token` is a `pub` field; if anything constructs
/// `AppState { admin_token: Some(Zeroizing::new(String::new())), .. }`
/// the constant-time compare against an empty `Authorization: Bearer `
/// presented bytes would PASS — silent unauthenticated admin access.
/// The boot loader (`load_admin_token`) already rejects empty tokens
/// loudly, so production code never reaches this branch, but tests
/// construct `AppState` directly and the type system advertises the
/// footgun. Treat empty-`expected` as "no token configured" → 401.
pub(crate) fn admin_check(
    req: &HttpRequest,
    state: &AppState,
) -> Result<(), HttpResponse> {
    use zeroize::Zeroizing;
    let expected: &Zeroizing<String> = match state.admin_token.as_ref() {
        Some(t) => t,
        None => {
            return Err(HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": "admin api disabled"})));
        }
    };
    let expected_bytes: &[u8] = expected.as_bytes();
    // Defense-in-depth: empty configured token must never authenticate
    // any request. Reject BEFORE the constant-time compare; we'd
    // otherwise need the comparator itself to special-case empty
    // input, and folding the check into admin_check keeps the
    // comparator's invariant simple.
    if expected_bytes.is_empty() {
        return Err(unauthorized());
    }
    let header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let presented = header
        .strip_prefix("Bearer ")
        .unwrap_or("")
        .as_bytes();
    if constant_time_bearer_eq(presented, expected_bytes) {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized()
        .json(&serde_json::json!({"error": "unauthorized"}))
}

fn err(status: u16, msg: impl Into<String>) -> HttpResponse {
    let s = msg.into();
    if status >= 500 {
        tracing::error!(status, error = %s, "sandbox/admin");
    }
    let mut resp = match status {
        400 => HttpResponse::BadRequest(),
        404 => HttpResponse::NotFound(),
        500 => HttpResponse::InternalServerError(),
        503 => HttpResponse::ServiceUnavailable(),
        _ => HttpResponse::InternalServerError(),
    };
    resp.json(&serde_json::json!({"error": s}))
}

// ────────────────────────────────────────────────────────────────────
// Common query helpers.
// ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct ListSandboxesQuery {
    pub user_id: Option<String>,
    pub host_id: Option<String>,
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Single row of `GET /admin/sandboxes` (cross-tenant). Fields
/// mirror `sandbox.sandboxes` plus a synthetic `in_memory: bool`
/// flag indicating whether THIS controller still holds the in-memory
/// registry entry. The flag is `false` for rows owned by peers + for
/// rows whose handler thread hasn't yet rehydrated post-takeover.
#[derive(Debug, Serialize)]
pub struct AdminSandboxRow {
    pub sandbox_id: String,
    pub user_id: String,
    pub project_id: String,
    pub backend: String,
    pub vm_index: Option<i32>,
    pub agent_url: Option<String>,
    pub host_id: String,
    pub generation: i64,
    pub status: String,
    pub key_fp: String,
    pub created_at_secs: i64,
    pub started_at_secs: Option<i64>,
    pub stopped_at_secs: Option<i64>,
    pub last_used_at_secs: i64,
    pub in_memory: bool,
}

async fn open_app_pool(state: &AppState) -> Result<Pool, HttpResponse> {
    let Some(db) = state.database.as_ref() else {
        return Err(err(503, "pg integration disabled"));
    };
    db.pool_app().await.map_err(|e| {
        err(503, format!("admin api: pool_app: {e}"))
    })
}

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn clamp_offset(offset: Option<i64>) -> i64 {
    offset.unwrap_or(0).max(0)
}

// ────────────────────────────────────────────────────────────────────
// GET /admin/sandboxes
// ────────────────────────────────────────────────────────────────────

pub async fn list_all_sandboxes(
    req: HttpRequest,
    state: State,
    query: web::types::Query<ListSandboxesQuery>,
) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    let pool = match open_app_pool(&state).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let q = query.into_inner();
    let limit = clamp_limit(q.limit);
    let offset = clamp_offset(q.offset);
    // Bound any user-supplied filter strings to the typed-id shape so
    // a malformed filter results in 400 rather than a 500 from pg.
    if let Some(ref u) = q.user_id {
        if zeroship_core::typed_id::parse_with_prefix(u, "usr").is_err() {
            return err(400, "invalid user_id filter");
        }
    }
    if let Some(ref h) = q.host_id {
        if zeroship_core::typed_id::parse_with_prefix(h, "hst").is_err() {
            return err(400, "invalid host_id filter");
        }
    }
    if let Some(ref s) = q.status {
        if !is_known_status(s) {
            return err(400, "invalid status filter");
        }
    }

    let client = match pool.get().await {
        Ok(c) => c,
        Err(e) => return err(503, format!("pool acquire: {e}")),
    };

    // Single SQL with three optional WHERE predicates; pg's planner
    // handles the NULL-or-equals form efficiently against the partial
    // indexes (`idx_sandboxes_user_id`, `idx_sandboxes_host_id_status`).
    let rows = client
        .query(
            "SELECT sandbox_id, user_id, project_id, backend, vm_index, \
                    agent_url, host_id, generation, status, key_fp, \
                    EXTRACT(EPOCH FROM created_at)::BIGINT AS created_at_secs, \
                    EXTRACT(EPOCH FROM started_at)::BIGINT AS started_at_secs, \
                    EXTRACT(EPOCH FROM stopped_at)::BIGINT AS stopped_at_secs, \
                    EXTRACT(EPOCH FROM last_used_at)::BIGINT AS last_used_at_secs \
               FROM sandbox.sandboxes \
              WHERE deleted_at IS NULL \
                AND ($1::TEXT IS NULL OR user_id = $1::TEXT) \
                AND ($2::TEXT IS NULL OR host_id = $2::TEXT) \
                AND ($3::TEXT IS NULL OR status = $3::TEXT) \
              ORDER BY created_at DESC \
              LIMIT $4::BIGINT OFFSET $5::BIGINT",
            &[&q.user_id, &q.host_id, &q.status, &limit, &offset],
        )
        .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return err(500, format!("query: {e}")),
    };

    let mut out: Vec<AdminSandboxRow> = Vec::with_capacity(rows.len());
    for r in rows {
        let sandbox_id: String = r.get("sandbox_id");
        let in_memory = is_held_in_memory(&state, &sandbox_id);
        let started_at_opt: Option<i64> = r.try_get("started_at_secs").ok();
        let stopped_at_opt: Option<i64> = r.try_get("stopped_at_secs").ok();
        out.push(AdminSandboxRow {
            sandbox_id: sandbox_id.clone(),
            user_id: r.get("user_id"),
            project_id: r.get("project_id"),
            backend: r.get("backend"),
            vm_index: r.try_get("vm_index").ok(),
            agent_url: r.try_get("agent_url").ok(),
            host_id: r.get("host_id"),
            generation: r.get::<_, i64>("generation"),
            status: r.get("status"),
            key_fp: r.get("key_fp"),
            created_at_secs: r.get::<_, i64>("created_at_secs"),
            started_at_secs: started_at_opt,
            stopped_at_secs: stopped_at_opt,
            last_used_at_secs: r.get::<_, i64>("last_used_at_secs"),
            in_memory,
        });
    }
    HttpResponse::Ok().json(&serde_json::json!({
        "sandboxes": out,
        "limit": limit,
        "offset": offset,
        "count": out.len(),
    }))
}

fn is_known_status(s: &str) -> bool {
    matches!(
        s,
        "starting"
            | "running"
            | "stopping"
            | "stopped"
            | "lost"
            | "recreating"
            | "orphan"
            | "unreachable"
    )
}

fn is_held_in_memory(state: &AppState, sandbox_id_typed: &str) -> bool {
    // The registry keys on Uuid; resolve typed-id → Uuid before lookup.
    if let Ok(uuid) = zeroship_core::typed_id::parse_with_prefix(sandbox_id_typed, "sbx") {
        return state.sandboxes.get(&uuid).is_some();
    }
    false
}

// ────────────────────────────────────────────────────────────────────
// GET /admin/sandboxes/{id}
// ────────────────────────────────────────────────────────────────────

pub async fn get_sandbox_detail(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    let raw = path.into_inner();
    let uuid = match zeroship_core::typed_id::parse_with_prefix(&raw, "sbx") {
        Ok(u) => u,
        Err(_) => return err(400, "invalid sandbox_id"),
    };
    let Some(db) = state.database.as_ref() else {
        return err(503, "pg integration disabled");
    };
    let row = match db.get_sandbox_row(uuid).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(404, "sandbox not found"),
        Err(e) => return err(500, format!("get_sandbox_row: {e}")),
    };

    let in_memory_info = state.sandboxes.get(&uuid);
    HttpResponse::Ok().json(&serde_json::json!({
        "row": serialize_sandbox_row(&row, in_memory_info.is_some()),
        "in_memory": in_memory_info,
        // /version probe is best-effort — we'd issue an HTTP call to
        // row.agent_url. Phase 3 surfaces None here; Phase 5 wires
        // the probe + signs the request with the persisted key.
        "agent_version": serde_json::Value::Null,
    }))
}

fn serialize_sandbox_row(row: &crate::db::SandboxRow, in_memory: bool) -> serde_json::Value {
    serde_json::json!({
        "sandbox_id": row.sandbox_id,
        "user_id": row.user_id,
        "project_id": row.project_id,
        "backend": row.backend,
        "vm_index": row.vm_index,
        "agent_url": row.agent_url,
        "host_id": row.host_id,
        "generation": row.generation,
        "status": row.status.as_str(),
        "key_fp": row.key_fp,
        "created_at_secs": row.created_at_secs,
        "started_at_secs": row.started_at_secs,
        "stopped_at_secs": row.stopped_at_secs,
        "last_used_at_secs": row.last_used_at_secs,
        "in_memory": in_memory,
    })
}

// ────────────────────────────────────────────────────────────────────
// GET /admin/users/{user_id}/sandboxes
// ────────────────────────────────────────────────────────────────────

pub async fn list_user_sandboxes(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
    query: web::types::Query<ListSandboxesQuery>,
) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    let user_id = path.into_inner();
    if zeroship_core::typed_id::parse_with_prefix(&user_id, "usr").is_err() {
        return err(400, "invalid user_id");
    }
    let mut q = query.into_inner();
    q.user_id = Some(user_id);
    list_all_sandboxes_inner(state, q).await
}

/// Shared body for both `/admin/sandboxes` and
/// `/admin/users/{user_id}/sandboxes`. Factored so the per-user
/// shortcut applies the same WHERE-chain + serialization without
/// re-implementing the SQL.
async fn list_all_sandboxes_inner(
    state: State,
    q: ListSandboxesQuery,
) -> HttpResponse {
    let pool = match open_app_pool(&state).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let limit = clamp_limit(q.limit);
    let offset = clamp_offset(q.offset);
    let client = match pool.get().await {
        Ok(c) => c,
        Err(e) => return err(503, format!("pool acquire: {e}")),
    };
    let rows = client
        .query(
            "SELECT sandbox_id, user_id, project_id, backend, vm_index, \
                    agent_url, host_id, generation, status, key_fp, \
                    EXTRACT(EPOCH FROM created_at)::BIGINT AS created_at_secs, \
                    EXTRACT(EPOCH FROM started_at)::BIGINT AS started_at_secs, \
                    EXTRACT(EPOCH FROM stopped_at)::BIGINT AS stopped_at_secs, \
                    EXTRACT(EPOCH FROM last_used_at)::BIGINT AS last_used_at_secs \
               FROM sandbox.sandboxes \
              WHERE deleted_at IS NULL \
                AND ($1::TEXT IS NULL OR user_id = $1::TEXT) \
                AND ($2::TEXT IS NULL OR host_id = $2::TEXT) \
                AND ($3::TEXT IS NULL OR status = $3::TEXT) \
              ORDER BY created_at DESC \
              LIMIT $4::BIGINT OFFSET $5::BIGINT",
            &[&q.user_id, &q.host_id, &q.status, &limit, &offset],
        )
        .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return err(500, format!("query: {e}")),
    };
    let mut out: Vec<AdminSandboxRow> = Vec::with_capacity(rows.len());
    for r in rows {
        let sandbox_id: String = r.get("sandbox_id");
        let in_memory = is_held_in_memory(&state, &sandbox_id);
        let started_at_opt: Option<i64> = r.try_get("started_at_secs").ok();
        let stopped_at_opt: Option<i64> = r.try_get("stopped_at_secs").ok();
        out.push(AdminSandboxRow {
            sandbox_id: sandbox_id.clone(),
            user_id: r.get("user_id"),
            project_id: r.get("project_id"),
            backend: r.get("backend"),
            vm_index: r.try_get("vm_index").ok(),
            agent_url: r.try_get("agent_url").ok(),
            host_id: r.get("host_id"),
            generation: r.get::<_, i64>("generation"),
            status: r.get("status"),
            key_fp: r.get("key_fp"),
            created_at_secs: r.get::<_, i64>("created_at_secs"),
            started_at_secs: started_at_opt,
            stopped_at_secs: stopped_at_opt,
            last_used_at_secs: r.get::<_, i64>("last_used_at_secs"),
            in_memory,
        });
    }
    HttpResponse::Ok().json(&serde_json::json!({
        "sandboxes": out,
        "limit": limit,
        "offset": offset,
        "count": out.len(),
    }))
}

// ────────────────────────────────────────────────────────────────────
// GET /admin/users/{user_id}/shares
// ────────────────────────────────────────────────────────────────────

pub async fn list_user_shares(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    let user_id = path.into_inner();
    if zeroship_core::typed_id::parse_with_prefix(&user_id, "usr").is_err() {
        return err(400, "invalid user_id");
    }
    let pool = match open_app_pool(&state).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let client = match pool.get().await {
        Ok(c) => c,
        Err(e) => return err(503, format!("pool acquire: {e}")),
    };
    let rows = client
        .query(
            "SELECT sh.token_id, sh.sandbox_id, sh.port, sh.scope, \
                    sh.secret_version, \
                    EXTRACT(EPOCH FROM sh.issued_at)::BIGINT AS issued_at_secs, \
                    EXTRACT(EPOCH FROM sh.expires_at)::BIGINT AS expires_at_secs, \
                    EXTRACT(EPOCH FROM sh.revoked_at)::BIGINT AS revoked_at_secs, \
                    sh.use_count, \
                    EXTRACT(EPOCH FROM sh.last_used_at)::BIGINT AS last_used_at_secs, \
                    sh.iss \
               FROM sandbox.shares sh \
               JOIN sandbox.sandboxes s ON sh.sandbox_id = s.sandbox_id \
              WHERE s.user_id = $1::TEXT AND sh.deleted_at IS NULL \
              ORDER BY sh.issued_at DESC \
              LIMIT 1000",
            &[&user_id],
        )
        .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return err(500, format!("query: {e}")),
    };
    let out: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            let port: i16 = r.get("port");
            serde_json::json!({
                "token_id": r.get::<_, String>("token_id"),
                "sandbox_id": r.get::<_, String>("sandbox_id"),
                "port": port,
                "scope": r.get::<_, String>("scope"),
                "secret_version": r.get::<_, i32>("secret_version"),
                "issued_at_secs": r.get::<_, i64>("issued_at_secs"),
                "expires_at_secs": r.get::<_, i64>("expires_at_secs"),
                "revoked_at_secs": r.try_get::<_, i64>("revoked_at_secs").ok(),
                "use_count": r.get::<_, i64>("use_count"),
                "last_used_at_secs": r.try_get::<_, i64>("last_used_at_secs").ok(),
                "iss": r.try_get::<_, String>("iss").ok(),
            })
        })
        .collect();
    HttpResponse::Ok().json(&serde_json::json!({
        "shares": out,
        "count": out.len(),
    }))
}

// ────────────────────────────────────────────────────────────────────
// GET /admin/hosts
// ────────────────────────────────────────────────────────────────────

pub async fn list_hosts(
    req: HttpRequest,
    state: State,
) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    let pool = match open_app_pool(&state).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let client = match pool.get().await {
        Ok(c) => c,
        Err(e) => return err(503, format!("pool acquire: {e}")),
    };
    let rows = client
        .query(
            "SELECT host_id, hostname, region, backend, status, \
                    EXTRACT(EPOCH FROM started_at)::BIGINT AS started_at_secs, \
                    EXTRACT(EPOCH FROM last_heartbeat)::BIGINT AS last_heartbeat_secs, \
                    EXTRACT(EPOCH FROM (now() - last_heartbeat))::DOUBLE PRECISION AS heartbeat_lag_secs, \
                    EXTRACT(EPOCH FROM drain_started_at)::BIGINT AS drain_started_at_secs, \
                    version \
               FROM sandbox.hosts \
              ORDER BY status, last_heartbeat DESC",
            &[],
        )
        .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => return err(500, format!("query: {e}")),
    };
    let out: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "host_id": r.get::<_, String>("host_id"),
                "hostname": r.get::<_, String>("hostname"),
                "region": r.get::<_, String>("region"),
                "backend": r.get::<_, String>("backend"),
                "status": r.get::<_, String>("status"),
                "started_at_secs": r.get::<_, i64>("started_at_secs"),
                "last_heartbeat_secs": r.get::<_, i64>("last_heartbeat_secs"),
                "heartbeat_lag_secs": r.get::<_, f64>("heartbeat_lag_secs"),
                "drain_started_at_secs": r.try_get::<_, i64>("drain_started_at_secs").ok(),
                "version": r.get::<_, String>("version"),
            })
        })
        .collect();
    HttpResponse::Ok().json(&serde_json::json!({
        "hosts": out,
        "count": out.len(),
    }))
}

// ────────────────────────────────────────────────────────────────────
// GET /admin/users/{user_id}/export   GDPR data-export
// ────────────────────────────────────────────────────────────────────
//
// One JSON document with everything the platform stores about this
// user, derived from a single REPEATABLE READ transaction so the
// shape is consistent (the operator sees a snapshot, not a moving
// target). Caps `events` at 10k rows; the response includes a
// `events_truncated: true` flag when the cap kicked in. Streaming
// NDJSON for huge users is Phase 5.

pub async fn export_user(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    let user_id = path.into_inner();
    if zeroship_core::typed_id::parse_with_prefix(&user_id, "usr").is_err() {
        return err(400, "invalid user_id");
    }
    let pool = match open_app_pool(&state).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut client = match pool.get().await {
        Ok(c) => c,
        Err(e) => return err(503, format!("pool acquire: {e}")),
    };
    let tx = match client.transaction().await {
        Ok(t) => t,
        Err(e) => return err(500, format!("begin tx: {e}")),
    };
    if let Err(e) = tx
        .batch_execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
    {
        return err(500, format!("set tx isolation: {e}"));
    }

    let sandboxes_json = match tx
        .query_one(
            "SELECT COALESCE(json_agg(s ORDER BY s.created_at), '[]'::json)::TEXT \
               FROM sandbox.sandboxes s WHERE s.user_id = $1::TEXT",
            &[&user_id],
        )
        .await
    {
        Ok(r) => r.get::<_, String>(0),
        Err(e) => return err(500, format!("export sandboxes: {e}")),
    };
    let shares_json = match tx
        .query_one(
            "SELECT COALESCE(json_agg(sh ORDER BY sh.issued_at), '[]'::json)::TEXT \
               FROM sandbox.shares sh \
               JOIN sandbox.sandboxes s ON sh.sandbox_id = s.sandbox_id \
              WHERE s.user_id = $1::TEXT",
            &[&user_id],
        )
        .await
    {
        Ok(r) => r.get::<_, String>(0),
        Err(e) => return err(500, format!("export shares: {e}")),
    };
    // Cap events at 10k. Order by ts so the truncation is a tail-cut
    // (operator gets the most recent 10k).
    //
    // Round-4 / IMPORTANT #3: filter out `kind = 'gdpr.delete_user'`
    // from the user-facing export. These rows are operator-side
    // records — they live in `sandbox.events` because the gdpr-role
    // INSERT grant runs through that table, but they are NOT user
    // data. They document who/when erased the user (GDPR Art. 30
    // Records of Processing Activities) and surfacing them on a
    // post-erasure export request would let the user re-discover
    // their own erasure record. The carve-out is operator-records
    // under the RoPA exemption — see runbook:
    // `docs/runbooks/sandbox-nomad-ch.md` § Phase-3 admin API. The
    // events_total / events_truncated counters mirror the same
    // filter so callers reading those numbers see the user-facing
    // row count, not the operator-record count.
    let events_json = match tx
        .query_one(
            "WITH capped AS ( \
               SELECT * FROM sandbox.events \
                 WHERE user_id = $1::TEXT \
                   AND kind <> 'gdpr.delete_user' \
                 ORDER BY ts DESC \
                 LIMIT $2::BIGINT \
             ) \
             SELECT COALESCE(json_agg(c ORDER BY c.ts), '[]'::json)::TEXT \
               FROM capped c",
            &[&user_id, &EVENT_EXPORT_CAP],
        )
        .await
    {
        Ok(r) => r.get::<_, String>(0),
        Err(e) => return err(500, format!("export events: {e}")),
    };
    let events_count: i64 = match tx
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.events \
              WHERE user_id = $1::TEXT \
                AND kind <> 'gdpr.delete_user'",
            &[&user_id],
        )
        .await
    {
        Ok(r) => r.get(0),
        Err(e) => return err(500, format!("count events: {e}")),
    };
    let deleted_json = match tx
        .query_one(
            "SELECT COALESCE(json_agg(ds ORDER BY ds.deleted_at), '[]'::json)::TEXT \
               FROM sandbox.deleted_sandboxes ds WHERE ds.user_id = $1::TEXT",
            &[&user_id],
        )
        .await
    {
        Ok(r) => r.get::<_, String>(0),
        Err(e) => return err(500, format!("export tombstones: {e}")),
    };
    if let Err(e) = tx.commit().await {
        return err(500, format!("commit: {e}"));
    }

    let exported_at_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Round-trip the per-table strings through serde_json so the
    // outer document is well-formed even if json_agg returns NULL
    // (we COALESCE to '[]'::json so this is defensive).
    let sandboxes_v: serde_json::Value =
        serde_json::from_str(&sandboxes_json).unwrap_or(serde_json::json!([]));
    let shares_v: serde_json::Value =
        serde_json::from_str(&shares_json).unwrap_or(serde_json::json!([]));
    let events_v: serde_json::Value =
        serde_json::from_str(&events_json).unwrap_or(serde_json::json!([]));
    let deleted_v: serde_json::Value =
        serde_json::from_str(&deleted_json).unwrap_or(serde_json::json!([]));

    HttpResponse::Ok().json(&serde_json::json!({
        "user_id": user_id,
        "exported_at_secs": exported_at_secs,
        "sandboxes": sandboxes_v,
        "shares": shares_v,
        "events": events_v,
        "events_truncated": events_count > EVENT_EXPORT_CAP,
        "events_total": events_count,
        "events_returned": events_count.min(EVENT_EXPORT_CAP),
        "deleted_sandboxes": deleted_v,
    }))
}

// ────────────────────────────────────────────────────────────────────
// DELETE /admin/users/{user_id}   GDPR cascade delete
// ────────────────────────────────────────────────────────────────────
//
// One transaction on the `sandbox_gdpr` connection. After the TX
// commits we walk the sealed-records dir and unlink any file whose
// derived UUID matches one of the deleted sandboxes.
//
// Important: the live runtime is unaffected. If the user has running
// sandboxes, the controller's in-memory state still holds them; the
// next stop+create cycle will fail (no pg row → 404). Operator
// workflow is "stop all sandboxes first, THEN call DELETE".

#[derive(Debug, Serialize)]
pub struct GdprDeleteResponse {
    pub deleted_user_id: String,
    pub sandboxes_tombstoned: i64,
    pub shares_deleted: i64,
    pub events_deleted: i64,
    pub sealed_files_unlinked: u64,
}

pub async fn delete_user(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    let user_id = path.into_inner();
    if zeroship_core::typed_id::parse_with_prefix(&user_id, "usr").is_err() {
        return err(400, "invalid user_id");
    }
    let Some(db) = state.database.as_ref() else {
        return err(503, "pg integration disabled");
    };

    // Use the gdpr-role pool. Single connection — the cascade is one TX.
    let gdpr_pool = match db.pool_gdpr().await {
        Ok(p) => p,
        Err(e) => return err(503, format!("pool_gdpr: {e}")),
    };
    let mut client = match gdpr_pool.get().await {
        Ok(c) => c,
        Err(e) => return err(500, format!("pool_gdpr acquire: {e}")),
    };
    let tx = match client.transaction().await {
        Ok(t) => t,
        Err(e) => return err(500, format!("begin tx: {e}")),
    };

    // Cross-user-leak guard: the `WHERE user_id = $1` predicate is on
    // every statement. The audit row is written inside the TX so the
    // gdpr-delete is atomically visible OR atomically rolled back —
    // we never end with audit-but-no-delete or vice versa.
    let sandbox_ids: Vec<String> = match tx
        .query(
            "SELECT sandbox_id FROM sandbox.sandboxes WHERE user_id = $1::TEXT",
            &[&user_id],
        )
        .await
    {
        Ok(rows) => rows.into_iter().map(|r| r.get::<_, String>(0)).collect(),
        Err(e) => return err(500, format!("collect ids: {e}")),
    };

    let events_deleted: i64 = match tx
        .execute(
            "DELETE FROM sandbox.events WHERE user_id = $1::TEXT",
            &[&user_id],
        )
        .await
    {
        Ok(n) => n as i64,
        Err(e) => return err(500, format!("delete events: {e}")),
    };
    let shares_deleted: i64 = match tx
        .execute(
            "DELETE FROM sandbox.shares \
              WHERE sandbox_id IN ( \
                  SELECT sandbox_id FROM sandbox.sandboxes WHERE user_id = $1::TEXT \
              )",
            &[&user_id],
        )
        .await
    {
        Ok(n) => n as i64,
        Err(e) => return err(500, format!("delete shares: {e}")),
    };
    let tombstoned: i64 = match tx
        .execute(
            "INSERT INTO sandbox.deleted_sandboxes (sandbox_id, user_id, deleted_at) \
             SELECT sandbox_id, user_id, now() \
               FROM sandbox.sandboxes \
              WHERE user_id = $1::TEXT \
             ON CONFLICT (sandbox_id) DO NOTHING",
            &[&user_id],
        )
        .await
    {
        Ok(n) => n as i64,
        Err(e) => return err(500, format!("tombstone: {e}")),
    };
    let sandboxes_deleted: i64 = match tx
        .execute(
            "DELETE FROM sandbox.sandboxes WHERE user_id = $1::TEXT",
            &[&user_id],
        )
        .await
    {
        Ok(n) => n as i64,
        Err(e) => return err(500, format!("delete sandboxes: {e}")),
    };
    // Audit row inside the same TX. The sandbox_gdpr role has INSERT
    // grant on events for exactly this audit row (§ 13.2).
    //
    // Round-4 / IMPORTANT #6: `admin_id` is hard-coded `"operator"`
    // pending the Phase-5 per-operator JWT claim. The trade-off is
    // documented in `docs/decisions/2026-05-05-sandbox-admin-shared-bearer.md`.
    let admin_id = "operator"; // Phase 5 replaces with JWT claim.
    let audit_event_id = zeroship_core::typed_id::generate("evt");
    let audit_data = serde_json::json!({
        "admin_id": admin_id,
        "rows_pg": sandboxes_deleted,
        "events_deleted": events_deleted,
        "shares_deleted": shares_deleted,
    })
    .to_string();
    // Round-4 / IMPORTANT #4: post-migration 0005 the `sandbox_id`
    // column on `sandbox.events` is NULLable. The GDPR audit row
    // isn't tied to any specific sandbox; we write `sandbox_id = NULL`
    // rather than synthesizing a never-existed `sbx_…` (which used
    // to pollute `idx_events_sandbox_ts` with unmatchable keys when
    // the user had zero sandboxes). The pre-existing CHECK
    // constraint accepts NULL by default.
    //
    // We bind `Option<String>` for `sandbox_id`: `None` for the
    // audit row, `Some(first_id)` would also work but adds nothing —
    // the row is operator-side metadata, not sandbox-scoped.
    let audit_sandbox_id: Option<String> = None;
    if let Err(e) = tx
        .execute(
            "INSERT INTO sandbox.events (event_id, sandbox_id, user_id, kind, ts, data) \
             VALUES ($1::TEXT, $2::TEXT, $3::TEXT, 'gdpr.delete_user', now(), \
                     CAST($4::TEXT AS JSONB))",
            &[&audit_event_id, &audit_sandbox_id, &user_id, &audit_data],
        )
        .await
    {
        return err(500, format!("audit insert: {e}"));
    }

    if let Err(e) = tx.commit().await {
        return err(500, format!("commit: {e}"));
    }

    // Sealed-record cleanup outside the TX. Best-effort + idempotent —
    // we walk the persist dir, decode each filename's prefix, and
    // unlink the ones that match a deleted sandbox_id. Failures are
    // logged but not reflected in the response status.
    let sealed_files_unlinked = unlink_sealed_for_user(&state, &sandbox_ids).await;

    HttpResponse::Ok().json(&GdprDeleteResponse {
        deleted_user_id: user_id,
        sandboxes_tombstoned: tombstoned,
        shares_deleted,
        events_deleted,
        sealed_files_unlinked,
    })
}

/// Walk the persist dir + unlink sealed-record files whose derived
/// filename matches one of `sandbox_ids`. The sealed filename is
/// `sha256(sandbox_id_bytes_canonical)[..32].sealed` per
/// `persist::seal_filename_for`; we re-derive each id's expected
/// filename and try to unlink it.
async fn unlink_sealed_for_user(state: &AppState, sandbox_ids: &[String]) -> u64 {
    let Some(persist) = state.persist.as_ref() else {
        return 0;
    };
    let mut unlinked: u64 = 0;
    for sid in sandbox_ids {
        let Ok(uuid) = zeroship_core::typed_id::parse_with_prefix(sid, "sbx") else {
            continue;
        };
        match persist.delete(uuid).await {
            Ok(()) => {
                unlinked += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Idempotent — file already gone (different host
                // sealed it, or a previous gdpr-delete run cleaned up).
            }
            Err(e) => {
                tracing::warn!(
                    sandbox_id = %sid,
                    error = %e,
                    "sandbox/admin: sealed-record unlink failed during gdpr delete"
                );
            }
        }
    }
    unlinked
}

// ────────────────────────────────────────────────────────────────────
// Snapshot/restore admin handlers.
//
// Lifecycle:
//   - PR 2c: 501 `feature_disabled` stubs
//   - PR 3a-h: db + store + handler + sweep modules landed (unwired)
//   - Phase A (this commit): wires `state.snapshot_store /
//     ch_remote / restore_backend` into the snapshot/wake handlers
//     when `SANDBOX_SNAPSHOT_ENABLED=true`. Cold-boot stays a 501
//     stub until the cold-boot orchestration ships (separate
//     follow-up — it's a different state machine).
//
// SourceVmOps wiring: the snapshot path needs an in-memory locator
// for `(api_socket, vm_index)` keyed by sandbox_id, plus a teardown
// hook. Plumbing those out of NomadCHBackend's private state map
// requires public accessors that haven't landed yet — a follow-up
// PR adds them. For Phase A, when wiring is enabled but SourceVmOps
// is unavailable, the handler returns 503 `wiring_partial` rather
// than 501, so operators can distinguish "feature off" from "feature
// on but SourceVmOps not yet wired." A9 smoke test verifies the
// 501 envelope is gone; full happy-path snapshot returns when the
// SourceVmOps PR lands.
// ────────────────────────────────────────────────────────────────────

fn feature_disabled() -> HttpResponse {
    // 501 Not Implemented — matches § 10.0's `feature_disabled` envelope.
    let mut resp = HttpResponse::NotImplemented();
    resp.json(&serde_json::json!({
        "error": "feature_disabled",
        "message": "snapshot/restore feature is not enabled (SANDBOX_SNAPSHOT_ENABLED=false)"
    }))
}

fn wiring_partial(detail: &str) -> HttpResponse {
    // 503 Service Unavailable — feature flag is on but the
    // controller hasn't finished plumbing SourceVmOps into the
    // backend. Operators see a distinct error code so this isn't
    // confused with `feature_disabled` (off) or 5xx infra errors.
    let mut resp = HttpResponse::ServiceUnavailable();
    resp.json(&serde_json::json!({
        "error": "wiring_partial",
        "message": format!(
            "snapshot/restore feature is enabled but the controller wiring is incomplete: {detail}"
        )
    }))
}

pub async fn snapshot_sandbox(req: HttpRequest, state: State) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    if !state.config.snapshot_enabled {
        return feature_disabled();
    }
    // The store + ch + restore backend are populated together when
    // snapshot_enabled = true. SourceVmOps is the missing piece —
    // see module doc above.
    if state.snapshot_store.is_some() {
        return wiring_partial(
            "SourceVmOps locator (api_socket + vm_index) not yet wired into NomadCHBackend",
        );
    }
    feature_disabled()
}

pub async fn wake_sandbox(req: HttpRequest, state: State) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    if !state.config.snapshot_enabled {
        return feature_disabled();
    }
    if state.restore_backend.is_some() {
        return wiring_partial(
            "wake handler not yet wired through restore_handler::restore_sandbox",
        );
    }
    feature_disabled()
}

pub async fn cold_boot_sandbox(req: HttpRequest, state: State) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    // Cold-boot is a separate state machine (no source VM, no
    // memory snapshot — it's a fresh boot from the rootfs). Stays
    // 501 until that orchestrator ships.
    feature_disabled()
}

// ────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_limit_bounds() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(50)), 50);
        assert_eq!(clamp_limit(Some(0)), 1, "0 clamps to 1");
        assert_eq!(clamp_limit(Some(-5)), 1, "negative clamps to 1");
        assert_eq!(clamp_limit(Some(99_999)), MAX_LIMIT, "huge clamps to MAX");
    }

    #[test]
    fn clamp_offset_bounds() {
        assert_eq!(clamp_offset(None), 0);
        assert_eq!(clamp_offset(Some(0)), 0);
        assert_eq!(clamp_offset(Some(50)), 50);
        assert_eq!(clamp_offset(Some(-1)), 0, "negative clamps to 0");
    }

    #[test]
    fn known_status_set() {
        for s in [
            "starting",
            "running",
            "stopping",
            "stopped",
            "lost",
            "recreating",
            "orphan",
            "unreachable",
        ] {
            assert!(is_known_status(s), "{s} must be accepted");
        }
        for s in ["", "garbage", "RUNNING", "starting ", "deleted"] {
            assert!(!is_known_status(s), "{s} must be rejected");
        }
    }

    // Phase-3 admin_check coverage lives in two places:
    //   - tests/sandbox_admin_e2e.rs — real ntex HTTP stack; full
    //     auth path including header parsing + 503/401 responses.
    //   - the constant_time_bearer_eq tests below — pure-helper
    //     correctness for the SHA-256-digest compare (Round-4 #1).
    // Boot-loader unit tests live inline in lib.rs's
    // `boot_loader_tests` module (Round-4 / MINOR #4).
    #[test]
    fn unauthorized_response_shape() {
        let resp = unauthorized();
        assert_eq!(resp.status().as_u16(), 401);
    }

    /// Round-4 fix: the SHA-256-digest compare returns the right
    /// boolean for matched / mismatched / unequal-length inputs.
    /// Timing-side-channel resistance is asserted by inspection of
    /// the function (no length-dependent control flow or allocation
    /// past the fixed-size hasher state); a reliable timing test in
    /// CI is notoriously flaky, so we pin behavioral correctness
    /// here.
    #[test]
    fn constant_time_bearer_eq_correctness() {
        // Equal-length match.
        assert!(constant_time_bearer_eq(b"right-token-12345", b"right-token-12345"));
        // Equal-length mismatch (last byte differs).
        assert!(!constant_time_bearer_eq(b"right-token-12345", b"right-token-12346"));
        // Equal-length mismatch (first byte differs).
        assert!(!constant_time_bearer_eq(b"aight-token-12345", b"bight-token-12345"));
        // Unequal length: presented shorter than expected.
        assert!(!constant_time_bearer_eq(b"short", b"right-token-12345"));
        // Unequal length: presented longer than expected.
        assert!(!constant_time_bearer_eq(b"right-token-12345-extra", b"right-token-12345"));
        // Empty presented.
        assert!(!constant_time_bearer_eq(b"", b"right-token-12345"));
        // Empty expected (degenerate at THIS layer; admin_check guards
        // the empty case with an early 401 — Round-4 / IMPORTANT #2.
        // We assert here that the comparator itself doesn't panic on
        // empty inputs.)
        assert!(!constant_time_bearer_eq(b"presented", b""));
        // Both empty (degenerate; admin_check rejects empty `expected`
        // before reaching this comparator. SHA-256(empty) ==
        // SHA-256(empty), so the comparator returns true here — this
        // is precisely WHY admin_check needs the early-empty reject).
        assert!(constant_time_bearer_eq(b"", b""));
    }

    /// Smoke test that the comparator handles wildly different
    /// lengths without allocating attacker-sized buffers (Round-4
    /// IMPORTANT #1: pre-fix the comparator did
    /// `vec![0u8; max(presented.len(), expected.len())]` which gave
    /// an attacker a fresh DoS amplifier — `presented` is bounded
    /// only by ntex's ~8 KiB header cap). The post-fix shape hashes
    /// both sides into 32 bytes regardless of input size.
    #[test]
    fn constant_time_bearer_eq_handles_wildly_different_lengths() {
        let presented = b"x";
        let expected = vec![0u8; 64];
        // Should return false without panicking, regardless of the
        // length disparity. No early-return on length difference.
        assert!(!constant_time_bearer_eq(presented, &expected));
        // Reverse: huge presented, tiny expected.
        let presented = vec![0u8; 64];
        let expected = b"y";
        assert!(!constant_time_bearer_eq(&presented, expected));
    }
}

