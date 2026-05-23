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
use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::error_envelope::{error_response, ErrorEnvelope};
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
/// Round-4 / IMPORTANT #2 + A5 (api-surface-2026-05-24-r1):
/// defense-in-depth empty-token guard. The post-Round-4 footgun was
/// that `AppState.admin_token` was a `pub` field — anyone could
/// build `AppState { admin_token: Some(Zeroizing::new(String::new())), .. }`
/// and the constant-time compare against an empty
/// `Authorization: Bearer ` presented bytes would PASS (silent
/// unauthenticated admin access). A5 closed the front door by
/// restricting the field to `pub(crate)` and routing all writes
/// through `AppState::with_admin_token`, which rejects empty
/// strings. The boot loader (`load_admin_token`) also rejects
/// empty tokens loudly, so production never reaches the branch
/// below. We KEEP this check as defense-in-depth for any future
/// in-crate setter that bypasses the builder — treat empty
/// `expected` as "no token configured" → 401.
pub(crate) fn admin_check(
    req: &HttpRequest,
    state: &AppState,
) -> Result<(), HttpResponse> {
    use zeroize::Zeroizing;
    let expected: &Zeroizing<String> = match state.admin_token.as_ref() {
        Some(t) => t,
        None => {
            return Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "admin_api_disabled",
                "admin api disabled (SANDBOX_ADMIN_TOKEN_PATH not configured)",
            ));
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
    error_response(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "authentication required",
    )
}

/// Render a §10.0-compliant error envelope for admin endpoints.
fn err(status: u16, code: &'static str, msg: impl Into<String>) -> HttpResponse {
    let s = msg.into();
    if status >= 500 {
        tracing::error!(status, code, error = %s, "sandbox/admin");
    }
    let sc = StatusCode::from_u16(status)
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    error_response(sc, code, s)
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
        return Err(err(503, "pg_disabled", "pg integration disabled"));
    };
    db.pool_app().await.map_err(|e| {
        err(503, "pg_pool_unavailable", format!("admin api: pool_app: {e}"))
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
            return err(400, "invalid_user_id", "invalid user_id filter");
        }
    }
    if let Some(ref h) = q.host_id {
        if zeroship_core::typed_id::parse_with_prefix(h, "hst").is_err() {
            return err(400, "invalid_host_id", "invalid host_id filter");
        }
    }
    if let Some(ref s) = q.status {
        if !is_known_status(s) {
            return err(400, "invalid_status", "invalid status filter");
        }
    }

    let client = match pool.get().await {
        Ok(c) => c,
        Err(e) => return err(503, "pg_pool_unavailable", format!("pool acquire: {e}")),
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
        Err(e) => return err(500, "pg_query_failed", format!("query: {e}")),
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
        Err(_) => return err(400, "invalid_sandbox_id", "invalid sandbox_id"),
    };
    let Some(db) = state.database.as_ref() else {
        return err(503, "pg_disabled", "pg integration disabled");
    };
    let row = match db.get_sandbox_row(uuid).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(404, "sandbox_not_found", "sandbox not found"),
        Err(e) => return err(500, "pg_query_failed", format!("get_sandbox_row: {e}")),
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
        return err(400, "invalid_user_id", "invalid user_id");
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
        Err(e) => return err(503, "pg_pool_unavailable", format!("pool acquire: {e}")),
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
        Err(e) => return err(500, "pg_query_failed", format!("query: {e}")),
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
        return err(400, "invalid_user_id", "invalid user_id");
    }
    let pool = match open_app_pool(&state).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let client = match pool.get().await {
        Ok(c) => c,
        Err(e) => return err(503, "pg_pool_unavailable", format!("pool acquire: {e}")),
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
        Err(e) => return err(500, "pg_query_failed", format!("query: {e}")),
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
        Err(e) => return err(503, "pg_pool_unavailable", format!("pool acquire: {e}")),
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
        Err(e) => return err(500, "pg_query_failed", format!("query: {e}")),
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
        return err(400, "invalid_user_id", "invalid user_id");
    }
    let pool = match open_app_pool(&state).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    let mut client = match pool.get().await {
        Ok(c) => c,
        Err(e) => return err(503, "pg_pool_unavailable", format!("pool acquire: {e}")),
    };
    let tx = match client.transaction().await {
        Ok(t) => t,
        Err(e) => return err(500, "pg_tx_begin_failed", format!("begin tx: {e}")),
    };
    if let Err(e) = tx
        .batch_execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
    {
        return err(500, "pg_tx_isolation_failed", format!("set tx isolation: {e}"));
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
        Err(e) => return err(500, "export_sandboxes_failed", format!("export sandboxes: {e}")),
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
        Err(e) => return err(500, "export_shares_failed", format!("export shares: {e}")),
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
        Err(e) => return err(500, "export_events_failed", format!("export events: {e}")),
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
        Err(e) => return err(500, "count_events_failed", format!("count events: {e}")),
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
        Err(e) => return err(500, "export_tombstones_failed", format!("export tombstones: {e}")),
    };
    if let Err(e) = tx.commit().await {
        return err(500, "pg_commit_failed", format!("commit: {e}"));
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
        return err(400, "invalid_user_id", "invalid user_id");
    }
    let Some(db) = state.database.as_ref() else {
        return err(503, "pg_disabled", "pg integration disabled");
    };

    // Use the gdpr-role pool. Single connection — the cascade is one TX.
    let gdpr_pool = match db.pool_gdpr().await {
        Ok(p) => p,
        Err(e) => return err(503, "pg_pool_gdpr_unavailable", format!("pool_gdpr: {e}")),
    };
    let mut client = match gdpr_pool.get().await {
        Ok(c) => c,
        Err(e) => return err(500, "pg_pool_gdpr_acquire_failed", format!("pool_gdpr acquire: {e}")),
    };
    let tx = match client.transaction().await {
        Ok(t) => t,
        Err(e) => return err(500, "pg_tx_begin_failed", format!("begin tx: {e}")),
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
        Err(e) => return err(500, "gdpr_collect_ids_failed", format!("collect ids: {e}")),
    };

    let events_deleted: i64 = match tx
        .execute(
            "DELETE FROM sandbox.events WHERE user_id = $1::TEXT",
            &[&user_id],
        )
        .await
    {
        Ok(n) => n as i64,
        Err(e) => return err(500, "gdpr_delete_events_failed", format!("delete events: {e}")),
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
        Err(e) => return err(500, "gdpr_delete_shares_failed", format!("delete shares: {e}")),
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
        Err(e) => return err(500, "gdpr_tombstone_failed", format!("tombstone: {e}")),
    };
    let sandboxes_deleted: i64 = match tx
        .execute(
            "DELETE FROM sandbox.sandboxes WHERE user_id = $1::TEXT",
            &[&user_id],
        )
        .await
    {
        Ok(n) => n as i64,
        Err(e) => return err(500, "gdpr_delete_sandboxes_failed", format!("delete sandboxes: {e}")),
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
        return err(500, "audit_insert_failed", format!("audit insert: {e}"));
    }

    if let Err(e) = tx.commit().await {
        return err(500, "pg_commit_failed", format!("commit: {e}"));
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
//   - Phase A: wired `state.snapshot_store / ch_remote /
//     restore_backend` into AppState; left a 503 `wiring_partial`
//     short-circuit because `SourceVmOps` was not yet exposed from
//     `NomadCHBackend`.
//   - Phase B (this commit): `Backend::lookup_source_vm_ops` returns
//     a resolved `SourceVmOpsHandle { api_socket, vm_index, alloc_dir }`
//     so the snapshot handler can run end to end. The wake handler
//     forwards to `restore_handler::restore_sandbox`. Cold-boot stays
//     a 501 stub until the cold-boot orchestrator ships.
// ────────────────────────────────────────────────────────────────────

use crate::backend::nomad_ch::SourceVmOpsHandle;
use crate::snapshot_handler::{
    self, snap_stage_dir, SnapshotHandlerError, SourceVmOps,
};
use crate::restore_handler::{self, RestoreHandlerError};
use uuid::Uuid;

fn feature_disabled() -> HttpResponse {
    // 501 Not Implemented — matches § 10.0's `feature_disabled` envelope.
    error_response(
        StatusCode::NOT_IMPLEMENTED,
        "feature_disabled",
        "snapshot/restore feature is not enabled (SANDBOX_SNAPSHOT_ENABLED=false)",
    )
}

/// Wire envelope for snapshot/wake errors. Maps the typed handler
/// errors to the response shapes documented in proposal § 10.0.
fn map_snapshot_error(e: SnapshotHandlerError) -> HttpResponse {
    match e {
        SnapshotHandlerError::FeatureDisabled => feature_disabled(),
        SnapshotHandlerError::StateMismatch { current } => ErrorEnvelope::new(
            StatusCode::CONFLICT,
            "state_mismatch",
            format!("sandbox is in state {current:?}; snapshot requires \"running\""),
        )
        .with_extra(serde_json::json!({
            "current": current,
            "expected": "running",
        }))
        .into_response(),
        SnapshotHandlerError::NotFound(id) => ErrorEnvelope::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "sandbox not found",
        )
        .with_extra(serde_json::json!({"sandbox_id": id}))
        .into_response(),
        SnapshotHandlerError::ChRemote(s) => err(500, "ch_remote_failed", format!("ch_remote: {s}")),
        SnapshotHandlerError::Store(s) => err(500, "snapshot_store_failed", format!("snapshot_store: {s}")),
        SnapshotHandlerError::Database(d) => err(500, "database_failed", format!("database: {d}")),
        SnapshotHandlerError::Internal(s) => err(500, "internal_error", format!("internal: {s}")),
    }
}

fn map_restore_error(e: RestoreHandlerError) -> HttpResponse {
    match e {
        RestoreHandlerError::FeatureDisabled => feature_disabled(),
        RestoreHandlerError::StateMismatch { current } => ErrorEnvelope::new(
            StatusCode::CONFLICT,
            "state_mismatch",
            format!("sandbox is in state {current:?}; wake requires \"snapshotted\""),
        )
        .with_extra(serde_json::json!({
            "current": current,
            "expected": "snapshotted",
        }))
        .into_response(),
        RestoreHandlerError::NotFound(id) => ErrorEnvelope::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "sandbox not found",
        )
        .with_extra(serde_json::json!({"sandbox_id": id}))
        .into_response(),
        RestoreHandlerError::VmIndexUnavailable { requested } => ErrorEnvelope::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "vm_index_unavailable",
            "no vm_index available to host the restored sandbox",
        )
        .with_extra(serde_json::json!({"requested": requested}))
        .into_response(),
        RestoreHandlerError::SnapshotCorrupt => {
            err(500, "snapshot_corrupt", "snapshot_corrupt: row marked snapshotted_suspect")
        }
        RestoreHandlerError::Store(s) => err(500, "snapshot_store_failed", format!("snapshot_store: {s}")),
        RestoreHandlerError::Backend(s) => err(500, "restore_backend_failed", format!("restore_backend: {s}")),
        RestoreHandlerError::ConfigRewrite(s) => err(500, "config_rewrite_failed", format!("config_rewrite: {s}")),
        RestoreHandlerError::Database(d) => err(500, "database_failed", format!("database: {d}")),
        RestoreHandlerError::Internal(s) => err(500, "internal_error", format!("internal: {s}")),
    }
}

/// Adapter: wrap the resolved `SourceVmOpsHandle` into a
/// `SourceVmOps` trait impl that the snapshot handler consumes.
///
/// The trait's `locate_*` methods are sync and ignore the `sandbox_id`
/// arg because we resolve the handle ahead of time (the lookup is
/// async and HTTP-bound to Nomad).
///
/// `teardown_source` is a no-op here — the admin handler tears down
/// the source explicitly after `snapshot_sandbox` returns success
/// (see `snapshot_sandbox` below). Folding the teardown into the
/// trait would force a sync→async bridge inside the snapshot handler;
/// keeping it post-handler keeps both layers boring.
struct ResolvedSourceVmOps {
    handle: SourceVmOpsHandle,
}

impl SourceVmOps for ResolvedSourceVmOps {
    fn locate_api_socket(&self, _sandbox_id: Uuid) -> Option<std::path::PathBuf> {
        Some(self.handle.api_socket.clone())
    }

    fn locate_vm_index(&self, _sandbox_id: Uuid) -> Option<i16> {
        i16::try_from(self.handle.vm_index).ok()
    }

    fn teardown_source(&self, _sandbox_id: Uuid) -> Result<(), String> {
        // No-op: the admin handler runs the async backend teardown
        // after `snapshot_sandbox` returns, so the snapshot handler
        // doesn't need a sync→async bridge. The handler logs a
        // warning if this returns Err, which we never do.
        Ok(())
    }
}

pub async fn snapshot_sandbox(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    if !state.config.snapshot_enabled {
        return feature_disabled();
    }
    let raw = path.into_inner();
    let sandbox_id = match zeroship_core::typed_id::parse_with_prefix(&raw, "sbx") {
        Ok(u) => u,
        Err(_) => return err(400, "invalid_sandbox_id", "invalid sandbox_id"),
    };

    // The store + ch + db handles are present iff snapshot_enabled.
    let (Some(store), Some(ch), Some(db)) = (
        state.snapshot_store.as_ref(),
        state.ch_remote.as_ref(),
        state.database.as_ref(),
    ) else {
        return err(503, "snapshot_wiring_unavailable", "snapshot wiring not initialized (database/store/ch_remote None)");
    };

    // Resolve the source VM identity BEFORE the destructive CAS so a
    // missing alloc / unreachable Nomad surfaces as 503 with the row
    // still `running`. (snapshot_handler also re-checks this inside
    // its own resolve step but that one is sync — the async lookup
    // here gives operators a clearer error path.)
    let handle = match state.backend.lookup_source_vm_ops(sandbox_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(
                sandbox_id = %sandbox_id,
                error = %e,
                "admin/snapshot: lookup_source_vm_ops failed; refusing snapshot"
            );
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "source_vm_unavailable",
                e,
            );
        }
    };

    let stage_dir = snap_stage_dir(&state.config.snapshot_l1_root, sandbox_id);
    let vm_ops = ResolvedSourceVmOps { handle };
    let outcome = snapshot_handler::snapshot_sandbox(
        db.as_ref(),
        store.as_ref(),
        ch.as_ref(),
        &vm_ops,
        sandbox_id,
        stage_dir,
        state.config.snapshot_enabled,
    )
    .await;
    match outcome {
        Ok(o) => {
            // Source teardown — best-effort, post-snapshot. The pg row
            // already reads `snapshotted` so a teardown failure here
            // leaves a runtime-orphan that the next-boot orphan-prune
            // sweeps. We log loudly + return 200 so the operator sees
            // the snapshot succeeded.
            if let Err(e) = state
                .backend
                .teardown_source_for_snapshot(sandbox_id)
                .await
            {
                tracing::warn!(
                    sandbox_id = %sandbox_id,
                    error = %e,
                    "admin/snapshot: source teardown failed (non-fatal; orphan-prune will reclaim)"
                );
            }
            HttpResponse::Ok().json(&serde_json::json!({
                "sandbox_id": format!(
                    "sbx_{}",
                    zeroship_core::typed_id::uuid_to_base62(&o.sandbox_id)
                ),
                "generation": o.generation,
                "vm_index": o.vm_index,
                "snapshot": {
                    "artifact_path": o.metadata.artifact_path,
                    "sha256_hex": hex::encode(o.metadata.sha256),
                    "ch_version": o.metadata.ch_version,
                    "bytes": o.metadata.bytes,
                },
            }))
        }
        Err(e) => {
            tracing::warn!(
                sandbox_id = %sandbox_id,
                error = %e,
                "admin/snapshot: handler failed"
            );
            map_snapshot_error(e)
        }
    }
}

pub async fn wake_sandbox(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    if !state.config.snapshot_enabled {
        return feature_disabled();
    }
    let raw = path.into_inner();
    let sandbox_id = match zeroship_core::typed_id::parse_with_prefix(&raw, "sbx") {
        Ok(u) => u,
        Err(_) => return err(400, "invalid_sandbox_id", "invalid sandbox_id"),
    };
    let (Some(store), Some(rb), Some(db)) = (
        state.snapshot_store.as_ref(),
        state.restore_backend.as_ref(),
        state.database.as_ref(),
    ) else {
        return err(
            503,
            "wake_wiring_unavailable",
            "wake wiring not initialized (database/store/restore_backend None)",
        );
    };
    let outcome = restore_handler::restore_sandbox(
        db.as_ref(),
        store.as_ref(),
        rb.as_ref(),
        sandbox_id,
        state.config.snapshot_enabled,
    )
    .await;
    match outcome {
        Ok(o) => HttpResponse::Ok().json(&serde_json::json!({
            "sandbox_id": format!(
                "sbx_{}",
                zeroship_core::typed_id::uuid_to_base62(&o.sandbox_id)
            ),
            "vm_index": o.vm_index,
            "generation": o.generation,
        })),
        Err(e) => {
            tracing::warn!(
                sandbox_id = %sandbox_id,
                error = %e,
                "admin/wake: handler failed"
            );
            map_restore_error(e)
        }
    }
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

