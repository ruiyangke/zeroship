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
///
/// `msg` is the user-visible message; pass ONLY values that are safe
/// to ship over the wire (typed-ids, fixed prose, structural status
/// names). For raw driver-error strings — `compio_postgres::Error`
/// renders host:port + schema + SQL fragments, `ch-remote` errors
/// render binary paths — funnel through [`err_safe`] instead, which
/// logs the raw error via `tracing::error!` and returns a sanitized
/// envelope.
fn err(status: u16, code: &'static str, msg: impl Into<String>) -> HttpResponse {
    let s = msg.into();
    if status >= 500 {
        tracing::error!(status, code, error = %s, "sandbox/admin");
    }
    let sc = StatusCode::from_u16(status)
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    error_response(sc, code, s)
}

/// Sanitizing variant of [`err`]. Logs the raw error to operator
/// observability (journald via `tracing::error!`) but renders a
/// FIXED `public_msg` into the wire-visible `message` field.
///
/// Per security review r4 (S4): A4's uniform envelope migration
/// funneled 30+ admin sites' `format!("query: {e}")` into the
/// `message` field. `compio_postgres::Error` carries host:port,
/// schema names, and sometimes SQL fragments / row values; the
/// admin endpoint IS admin-token-gated, but admin-token holders
/// shouldn't see internal infrastructure details either (defense
/// in depth — the threat model is the leak surface, not the
/// authorization gate). The `code` field is the stable contract;
/// the `message` should be safe prose.
///
/// Operators recover the raw error from journald keyed by the
/// `tracing::error!` line below. The `code` field on the wire is
/// the stable client contract — clients still branch on it.
pub(crate) fn err_safe(
    status: u16,
    code: &'static str,
    public_msg: &'static str,
    raw: impl std::fmt::Display,
) -> HttpResponse {
    if status >= 500 {
        tracing::error!(
            status,
            code,
            error = %raw,
            "sandbox/admin: sanitized error (raw not on wire)"
        );
    }
    let sc = StatusCode::from_u16(status)
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    error_response(sc, code, public_msg)
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
        err_safe(503, "pg_pool_unavailable", "database unavailable", e)
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
        Err(e) => return err_safe(503, "pg_pool_unavailable", "database unavailable", e),
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
        Err(e) => return err_safe(500, "pg_query_failed", "database error", e),
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
        Err(e) => return err_safe(500, "pg_query_failed", "database error", e),
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
        Err(e) => return err_safe(503, "pg_pool_unavailable", "database unavailable", e),
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
        Err(e) => return err_safe(500, "pg_query_failed", "database error", e),
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
        Err(e) => return err_safe(503, "pg_pool_unavailable", "database unavailable", e),
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
        Err(e) => return err_safe(500, "pg_query_failed", "database error", e),
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
        Err(e) => return err_safe(503, "pg_pool_unavailable", "database unavailable", e),
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
        Err(e) => return err_safe(500, "pg_query_failed", "database error", e),
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
        Err(e) => return err_safe(503, "pg_pool_unavailable", "database unavailable", e),
    };
    let tx = match client.transaction().await {
        Ok(t) => t,
        Err(e) => return err_safe(500, "pg_tx_begin_failed", "database error", e),
    };
    if let Err(e) = tx
        .batch_execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
    {
        return err_safe(500, "pg_tx_isolation_failed", "database error", e);
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
        Err(e) => return err_safe(500, "export_sandboxes_failed", "database error", e),
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
        Err(e) => return err_safe(500, "export_shares_failed", "database error", e),
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
        Err(e) => return err_safe(500, "export_events_failed", "database error", e),
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
        Err(e) => return err_safe(500, "count_events_failed", "database error", e),
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
        Err(e) => return err_safe(500, "export_tombstones_failed", "database error", e),
    };
    if let Err(e) = tx.commit().await {
        return err_safe(500, "pg_commit_failed", "database error", e);
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
        Err(e) => return err_safe(503, "pg_pool_gdpr_unavailable", "database unavailable", e),
    };
    let mut client = match gdpr_pool.get().await {
        Ok(c) => c,
        Err(e) => return err_safe(500, "pg_pool_gdpr_acquire_failed", "database unavailable", e),
    };
    let tx = match client.transaction().await {
        Ok(t) => t,
        Err(e) => return err_safe(500, "pg_tx_begin_failed", "database error", e),
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
        Err(e) => return err_safe(500, "gdpr_collect_ids_failed", "database error", e),
    };

    let events_deleted: i64 = match tx
        .execute(
            "DELETE FROM sandbox.events WHERE user_id = $1::TEXT",
            &[&user_id],
        )
        .await
    {
        Ok(n) => n as i64,
        Err(e) => return err_safe(500, "gdpr_delete_events_failed", "database error", e),
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
        Err(e) => return err_safe(500, "gdpr_delete_shares_failed", "database error", e),
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
        Err(e) => return err_safe(500, "gdpr_tombstone_failed", "database error", e),
    };
    let sandboxes_deleted: i64 = match tx
        .execute(
            "DELETE FROM sandbox.sandboxes WHERE user_id = $1::TEXT",
            &[&user_id],
        )
        .await
    {
        Ok(n) => n as i64,
        Err(e) => return err_safe(500, "gdpr_delete_sandboxes_failed", "database error", e),
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
        return err_safe(500, "audit_insert_failed", "database error", e);
    }

    if let Err(e) = tx.commit().await {
        return err_safe(500, "pg_commit_failed", "database error", e);
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
        SnapshotHandlerError::ChRemote(s) => {
            err_safe(500, "ch_remote_failed", "hypervisor error", s)
        }
        SnapshotHandlerError::Store(s) => {
            err_safe(500, "snapshot_store_failed", "snapshot store error", s)
        }
        SnapshotHandlerError::Database(d) => {
            err_safe(500, "database_failed", "database error", d)
        }
        SnapshotHandlerError::Internal(s) => {
            err_safe(500, "internal_error", "internal error", s)
        }
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
        RestoreHandlerError::Store(s) => {
            err_safe(500, "snapshot_store_failed", "snapshot store error", s)
        }
        RestoreHandlerError::Backend(s) => {
            err_safe(500, "restore_backend_failed", "restore backend error", s)
        }
        RestoreHandlerError::ConfigRewrite(s) => {
            err_safe(500, "config_rewrite_failed", "config rewrite error", s)
        }
        RestoreHandlerError::Database(d) => {
            err_safe(500, "database_failed", "database error", d)
        }
        RestoreHandlerError::Internal(s) => {
            err_safe(500, "internal_error", "internal error", s)
        }
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
            // Per security review r4 (S4): the raw error here can
            // carry Nomad addresses / internal alloc ids; log it but
            // return a fixed public message. The `code` field is the
            // stable wire contract.
            tracing::warn!(
                sandbox_id = %sandbox_id,
                error = %e,
                "admin/snapshot: lookup_source_vm_ops failed; refusing snapshot"
            );
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "source_vm_unavailable",
                "source VM unavailable",
            );
        }
    };

    let stage_dir = snap_stage_dir(&state.config.snapshot_l1_root, sandbox_id);
    let vm_ops = ResolvedSourceVmOps { handle };
    let outcome = snapshot_handler::snapshot_sandbox(
        db.as_ref(),
        std::sync::Arc::clone(store),
        std::sync::Arc::clone(ch),
        &vm_ops,
        sandbox_id,
        stage_dir,
        state.config.snapshot_enabled,
    )
    .await;
    match outcome {
        Ok(o) => {
            // Source teardown — best-effort, post-snapshot, DETACHED.
            //
            // R6-P1 fix (perf-r6): the pg row already reads `snapshotted`
            // (the CAS inside `snapshot_handler::snapshot_sandbox` has
            // committed by this point), so the snapshot itself is durable
            // regardless of what the teardown does. The teardown's cost
            // is dominated by Nomad alloc-terminal wait (up to 30s
            // `wait_for_job_gone` + up to ~20s host_fence — see
            // `backend::nomad_ch::stop_inner` steps 3–4), which kept
            // snapshot p50 around 50s when awaited inline. Detaching it
            // returns 200 immediately and lets the teardown run in
            // background.
            //
            // Race safety: the vm_index_allocator is shared
            // (`Arc<Mutex<VmIndexAllocator>>`, B19 wiring), so a wake
            // racing the teardown will see `reserve(vm_index)` reject
            // with "vm_index N already reserved" until the teardown's
            // `release()` fires after host_fence clears. This is the
            // same pre-existing race the sweep-path teardown already
            // exposes (`sweep.rs::idle-eviction`); detaching does not
            // create new shared state. A teardown failure here leaves a
            // runtime-orphan that the next-boot orphan-prune sweeps.
            //
            // Errors surface as `tracing::error!` (not warn) — the
            // operator has no other signal that the background teardown
            // failed, so it must be loud in the logs.
            //
            // **C-6 fix** (T-8b-smoke-r7 cluster review, 2026-05-25):
            // do NOT detach via `compio::runtime::spawn(...).detach()`.
            // That puts the teardown future on the SAME ntex-worker
            // compio runtime that subsequent wake requests land on
            // (1-worker fleet → guaranteed collision; N-worker fleets
            // pin requests per TCP connection, so a wake reusing the
            // same client connection co-locates with the teardown too).
            // `stop_inner`'s first await is `http_signed_async("/shutdown")`
            // whose underlying ureq call burns up to 60 s on connection-
            // timeout against a half-dead agent. While that future was
            // mid-`/shutdown`, the runtime starved C-4's
            // `reserve_vm_index_with_retry` 2 s sleep — the wake handler
            // emitted `phase=pre_reserve_vm_index` and then nothing for
            // the full 60 s client deadline, dropping the wake future.
            //
            // Mirror C-3's pattern (`snapshot_store_gcs.rs::Tiered::put`):
            // spawn a dedicated OS thread with its own short-lived compio
            // runtime via `compio::runtime::Runtime::new().block_on(...)`.
            // Decoupling from the ntex worker's runtime is the only way
            // to guarantee no cross-task starvation; `spawn_blocking` on
            // the worker runtime is insufficient because the teardown
            // future itself (between blocking calls) runs on the worker.
            //
            // R14-A1 (C-7-LT-PR1 commit 1, 3d8acc23): the open-coded
            // OS-thread + private compio runtime pattern is now extracted
            // into `crate::detach::detach_isolated`. This call site is the
            // canonical C-6 fix; the helper preserves identical semantics
            // (fire-and-forget, log-and-drop on thread/runtime spawn
            // failure, kernel-truncated thread name).
            //
            // Byte-slice tail naming preserved for log/grep correlation:
            // Linux's `pr_set_name` truncates thread names at 15 bytes
            // (TASK_COMM_LEN-1), so only `snap-teardown-` fits in
            // `ps`/`top -H`; the tail is preserved at the Rust thread-name
            // level for `tracing` / `std::thread::current().name()`.
            // Byte-slice is ASCII-safe: `uuid_to_base62` emits base62
            // characters (0-9, a-z, A-Z) which are all single-byte UTF-8,
            // so `s.len() - 8` lands on a char boundary.
            let state_for_teardown = Arc::clone(&state);
            let sandbox_id_base62 = zeroship_core::typed_id::uuid_to_base62(&sandbox_id);
            let tail = sandbox_id_base62
                .get(sandbox_id_base62.len().saturating_sub(8)..)
                .unwrap_or(&sandbox_id_base62);
            crate::detach::detach_isolated(
                format!("snap-teardown-{tail}"),
                move || async move {
                    if let Err(e) = state_for_teardown
                        .backend
                        .teardown_source_for_snapshot(sandbox_id)
                        .await
                    {
                        tracing::error!(
                            sandbox_id = %sandbox_id,
                            error = %e,
                            "admin/snapshot: detached teardown_source_for_snapshot failed (non-fatal; orphan-prune will reclaim)"
                        );
                    }
                },
            );
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

/// Query-string for `POST /admin/sandboxes/{id}/wake`. The `?sync=1`
/// override forces the legacy synchronous path even when the
/// controller's default `WakeResponseMode` is `Async`. Deprecated
/// per C-7-LT proposal § 4; tracked by the
/// `sandbox_wake_sync_uses_total` counter (R16-API1 #5).
#[derive(Debug, Deserialize, Default)]
pub struct WakeQuery {
    /// Operator override: when `1`, force the legacy 200-OK
    /// synchronous response. Any other value (or absence) defers to
    /// the controller's `SANDBOX_WAKE_RESPONSE_MODE` env-resolved
    /// default.
    pub sync: Option<u8>,
}

/// `POST /admin/sandboxes/{id}/wake` — C-7-LT-PR2 dual-mode entry.
///
/// Two response shapes:
///
/// - Sync (legacy; default until C-7-LT phase 4): 200 OK with the
///   full wake outcome `{sandbox_id, vm_index, generation}`. Subject
///   to the ntex client-deadline ceiling that motivated the
///   redesign. Selected when `WakeResponseMode::Sync` (the env
///   default) OR `?sync=1` override.
/// - Async (C-7-LT contract): 202 Accepted with
///   `{wake_id, poll_url, state}`. The state machine runs on a
///   private compio runtime in [`crate::wake_machine::WakeMachine`];
///   the client polls `GET /admin/sandboxes/{id}/wake/{wake_id}` for
///   the terminal outcome. Idempotent: a duplicate POST while a wake
///   is in flight returns the existing `wake_id` with `replay: true`.
pub async fn wake_sandbox(
    req: HttpRequest,
    state: State,
    path: web::types::Path<String>,
    query: web::types::Query<WakeQuery>,
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

    let q = query.into_inner();
    let force_sync = matches!(q.sync, Some(1));
    let mode = state.wake_response_mode;
    let take_sync_path = force_sync || matches!(mode, crate::config::WakeResponseMode::Sync);

    if take_sync_path {
        // Deprecation telemetry: bump on EVERY sync use so Phase 5's
        // "zero sync uses for one minor" gate has data. The warn log
        // dedups in the operator's eyes by client_ua + sandbox_id;
        // the counter is the source-of-truth for the gate.
        crate::metrics::inc_wake_sync_deprecated();
        let client_ua = req
            .headers()
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        tracing::warn!(
            sandbox_id = %sandbox_id,
            force_sync,
            mode = mode.as_str(),
            client_ua,
            "admin/wake: sync mode used (deprecated; migrate to async polling per C-7-LT)"
        );
        return wake_sandbox_sync_inner(&state, sandbox_id).await;
    }

    wake_sandbox_async_inner(&state, &raw, sandbox_id).await
}

/// Legacy synchronous wake path. Drives `restore_handler::restore_sandbox`
/// inline and emits the existing flat `{sandbox_id, vm_index, generation}`
/// 200-OK body or the §10.0-shaped error envelope from `map_restore_error`.
///
/// Preserved verbatim (modulo the function-boundary extraction) through
/// C-7-LT phase 4; phase 5 deletes it along with `do_restore_inner`.
async fn wake_sandbox_sync_inner(state: &AppState, sandbox_id: uuid::Uuid) -> HttpResponse {
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
        std::sync::Arc::clone(store),
        std::sync::Arc::clone(rb),
        state.persist.as_deref(),
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

/// Async (202 Accepted + polling) wake path per C-7-LT proposal § 2.
///
/// Idempotency / status matrix (R16-API1 #3):
///
/// | precondition                                       | response                                                 |
/// |----------------------------------------------------|----------------------------------------------------------|
/// | no in-flight wake for sandbox                      | 202 with newly minted `wake_id`                          |
/// | in-flight wake exists                              | 202 with existing `wake_id` + `replay: true`             |
/// | terminal wake within `T_KEEP`                      | (handled by `GET /wake/{wake_id}`, not this POST)        |
/// | terminal wake evicted (>T_KEEP)                    | 202 with newly minted `wake_id` (caller retried POST)    |
///
/// Note: a fresh POST after a terminal wake (still in pg) is a
/// distinct semantic from a duplicate in-flight POST — we treat
/// "in-flight only" as the replay case. A terminal-state replay is
/// surfaced through the GET endpoint, not the POST.
async fn wake_sandbox_async_inner(
    state: &AppState,
    sandbox_id_typed: &str,
    sandbox_id: uuid::Uuid,
) -> HttpResponse {
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

    // Idempotency fast-path: short-circuit duplicate in-flight wakes
    // BEFORE the pre-flight + INSERT. The
    // `find_pending_wake_for_sandbox` query rides the partial index
    // (`wake_jobs_state_idx WHERE state NOT IN ('ok', 'failed')`) so
    // this is a cheap lookup even at fleet scale. This precheck is
    // **only** an optimisation — the GATE-C2 fix is at the INSERT
    // site, where the migration-0011 UNIQUE INDEX
    // `wake_jobs_sandbox_pending_uniq` enforces at-most-one
    // non-terminal row per sandbox atomically. Two concurrent POSTs
    // that both miss this precheck still race deterministically:
    // one's INSERT lands, the other's `ON CONFLICT … DO NOTHING`
    // returns 0 rows affected and the handler surfaces the winner's
    // wake_id via [`InsertWakeJobOutcome::Replay`].
    let typed_sandbox_id = sandbox_id_typed.to_string();
    match db.find_pending_wake_for_sandbox(&typed_sandbox_id).await {
        Ok(Some(existing)) => {
            return HttpResponse::Accepted().json(&serde_json::json!({
                "wake_id": existing.wake_id,
                "sandbox_id": typed_sandbox_id,
                "poll_url": format!(
                    "/admin/sandboxes/{typed_sandbox_id}/wake/{wake_id}",
                    wake_id = existing.wake_id,
                ),
                "state": existing.state.as_str(),
                "replay": true,
            }));
        }
        Ok(None) => {}
        Err(e) => {
            return err_safe(
                500,
                "database_failed",
                "wake idempotency lookup failed",
                e,
            );
        }
    }

    // Pre-flight: refuse if the sandbox row isn't a valid wake
    // candidate. Mirrors `restore_sandbox`'s first two checks
    // (NotFound + StateMismatch) so the client gets the same 404/409
    // it would have gotten on the sync path; we only spawn the
    // machine when the wake actually has work to do.
    let row = match db.get_sandbox_row(sandbox_id).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return ErrorEnvelope::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "sandbox not found",
            )
            .with_extra(serde_json::json!({"sandbox_id": typed_sandbox_id}))
            .into_response();
        }
        Err(e) => {
            return err_safe(
                500,
                "database_failed",
                "sandbox row lookup failed",
                e,
            );
        }
    };
    if !matches!(
        row.status,
        crate::db::SandboxStatus::Snapshotted
            | crate::db::SandboxStatus::SnapshottedSuspect
    ) {
        return ErrorEnvelope::new(
            StatusCode::CONFLICT,
            "state_mismatch",
            format!(
                "sandbox is in state {:?}; wake requires \"snapshotted\"",
                row.status
            ),
        )
        .with_extra(serde_json::json!({
            "current": row.status.as_str(),
            "expected": "snapshotted",
        }))
        .into_response();
    }

    // Mint wake_id (typed-id with wak_ prefix per R16-API2) and
    // attempt the INSERT. The DB layer enforces GATE-C2 via the
    // migration-0011 partial UNIQUE INDEX
    // `wake_jobs_sandbox_pending_uniq` + ON CONFLICT DO NOTHING. The
    // outcome tells us whether THIS caller's row landed (spawn the
    // machine) or whether a concurrent POST won the race (return the
    // winner's wake_id with `replay: true` — DO NOT spawn a second
    // machine).
    let wake_id = zeroship_core::typed_id::new_wake_id();
    let lessee = db.host_id().to_string();
    let new_row = crate::db::WakeJobRow {
        wake_id: wake_id.clone(),
        sandbox_id: typed_sandbox_id.clone(),
        state: crate::db::WakeJobState::Pending,
        error_code: None,
        error_message: None,
        started_at_secs: 0, // server-side default
        updated_at_secs: 0,
        ready_at_secs: None,
        agent_url: None,
        lessee: lessee.clone(),
        lessee_updated_at_secs: 0,
    };
    let outcome = match db.insert_wake_job(&new_row).await {
        Ok(o) => o,
        Err(e) => {
            return err_safe(
                500,
                "database_failed",
                "wake job insert failed",
                e,
            );
        }
    };
    // GATE-C2: race-loser branch. A concurrent POST won the
    // partial-UNIQUE-INDEX conflict and its WakeMachine is already
    // driving the wake. Return the winner's wake_id with
    // `replay: true` and DO NOT spawn a duplicate machine — if we
    // did, the loser machine's `rollback_with` would call
    // `teardown_restore` and release the winner's vm_index
    // (R10-C1-shape race; the fingerprint R17-C2 was filed against).
    if let crate::db::InsertWakeJobOutcome::Replay(existing) = outcome {
        return HttpResponse::Accepted().json(&serde_json::json!({
            "wake_id": existing.wake_id,
            "sandbox_id": typed_sandbox_id,
            "poll_url": format!(
                "/admin/sandboxes/{typed_sandbox_id}/wake/{wake_id}",
                wake_id = existing.wake_id,
            ),
            "state": existing.state.as_str(),
            "replay": true,
        }));
    }

    // Spawn the state machine on a dedicated OS thread with a
    // private compio runtime. The wake_id parameter is moved into
    // the closure; ntex can't reach it after the 202 is sent.
    let machine = crate::wake_machine::WakeMachine {
        database: Arc::clone(db),
        backend: Arc::clone(rb),
        snapshot_store: Arc::clone(store),
        persist: state.persist.clone(),
        sandbox_id,
        wake_id: wake_id.clone(),
        lessee,
    };
    // Linux truncates thread names to 15 chars; prefix with "wake-"
    // and a short suffix of the wake_id so dashboards / `ps -L` show
    // a stable, debug-friendly tag without overflowing TASK_COMM_LEN.
    let short_tail: String = wake_id
        .as_bytes()
        .iter()
        .rev()
        .take(8)
        .rev()
        .map(|b| *b as char)
        .collect();
    let thread_name = format!("wake-{short_tail}");
    crate::detach::detach_isolated(thread_name, move || machine.drive());

    HttpResponse::Accepted().json(&serde_json::json!({
        "wake_id": wake_id,
        "sandbox_id": typed_sandbox_id,
        "poll_url": format!(
            "/admin/sandboxes/{typed_sandbox_id}/wake/{wake_id}"
        ),
        "state": "pending",
        "replay": false,
    }))
}

/// `GET /admin/sandboxes/{id}/wake/{wake_id}` — C-7-LT-PR2 polling
/// endpoint per proposal § 2.
///
/// Response matrix:
///
/// | row state                  | code | body                                                           |
/// |----------------------------|------|----------------------------------------------------------------|
/// | not found / GC'd           | 404  | §10.0 envelope `{error: "wake_not_found", message}`            |
/// | sandbox_id mismatch        | 404  | §10.0 envelope `{error: "wake_not_found", message}`            |
/// | terminal `ok`              | 200  | flat `{state: "ok", ready_at, agent_url}`                       |
/// | terminal `failed`          | 200  | §10.0 + wake extras `{error, message, state: "failed", …}`      |
/// | intermediate (any other)   | 202  | flat `{state, started_at}`                                      |
///
/// Per R16-API1 #1: the failed-state body uses the `error`/`message`
/// keys (NOT `error_code`/`error_message`) so it's an §10.0 envelope
/// plus a `state: "failed"` extra. Success bodies stay flat per the
/// existing convention (success bodies in the crate's HTTP surface
/// are flat; only error bodies wear the envelope).
pub async fn poll_wake(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, String)>,
) -> HttpResponse {
    if let Err(r) = admin_check(&req, &state) {
        return r;
    }
    if !state.config.snapshot_enabled {
        return feature_disabled();
    }
    let (sandbox_raw, wake_raw) = path.into_inner();
    let _sandbox_id = match zeroship_core::typed_id::parse_with_prefix(&sandbox_raw, "sbx") {
        Ok(u) => u,
        Err(_) => return err(400, "invalid_sandbox_id", "invalid sandbox_id"),
    };
    let wake_uuid = match zeroship_core::typed_id::parse_with_prefix(&wake_raw, "wak") {
        Ok(u) => u,
        Err(_) => return err(400, "invalid_wake_id", "invalid wake_id"),
    };
    // The typed_id parse rejects malformed shapes / wrong prefixes
    // but lets through any valid `wak_<base62>` — even one we never
    // minted. The pg lookup below distinguishes "valid shape, never
    // existed" from "valid, evicted by GC sweep" by always 404'ing
    // both: see § 2.cleanup of the proposal.
    let _ = wake_uuid;

    let Some(db) = state.database.as_ref() else {
        return err(
            503,
            "wake_wiring_unavailable",
            "wake wiring not initialized (database None)",
        );
    };

    let row = match db.get_wake_job(&wake_raw).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return ErrorEnvelope::new(
                StatusCode::NOT_FOUND,
                "wake_not_found",
                "wake_id not found (may have been evicted by the GC sweep)",
            )
            .with_extra(serde_json::json!({"wake_id": wake_raw}))
            .into_response();
        }
        Err(e) => {
            return err_safe(
                500,
                "database_failed",
                "wake job lookup failed",
                e,
            );
        }
    };

    // Path mismatch: a wake_id valid for a different sandbox MUST
    // 404 (not 200) so a client can't probe other tenants' wake
    // outcomes via guessed wake_ids. Constant-time comparison is
    // overkill (wake_ids are 22-char base62, the attacker can't
    // narrow the search), but the path is admin-bearer-gated so
    // narrowness isn't a primary defense — symmetric 404 is enough.
    if row.sandbox_id != sandbox_raw {
        return ErrorEnvelope::new(
            StatusCode::NOT_FOUND,
            "wake_not_found",
            "wake_id does not belong to the requested sandbox",
        )
        .with_extra(serde_json::json!({"wake_id": wake_raw}))
        .into_response();
    }

    render_wake_poll_response(&row)
}

/// Render the wake-job row into the polling-response shape per
/// R16-API1. Extracted from [`poll_wake`] so the wire-format tests
/// can pin §10.0 parity without a Postgres-backed AppState.
///
/// - intermediate states → 202 with flat `{state, wake_id, sandbox_id,
///   started_at, updated_at}`
/// - terminal `ok` → 200 with flat `{state: "ok", wake_id, sandbox_id,
///   ready_at, agent_url}`
/// - terminal `failed` → 200 with §10.0 envelope
///   `{error: <wire_code>, message, state: "failed", wake_id,
///   sandbox_id, updated_at}` per R16-API1 #1: field names match the
///   envelope at `error_envelope.rs:88-110`, NOT the proposal's
///   pre-review `error_code`/`error_message`.
pub(crate) fn render_wake_poll_response(row: &crate::db::WakeJobRow) -> HttpResponse {
    if !row.state.is_terminal() {
        return HttpResponse::Accepted().json(&serde_json::json!({
            "state": row.state.as_str(),
            "wake_id": row.wake_id,
            "sandbox_id": row.sandbox_id,
            "started_at": row.started_at_secs,
            "updated_at": row.updated_at_secs,
        }));
    }

    match row.state {
        crate::db::WakeJobState::Ok => HttpResponse::Ok().json(&serde_json::json!({
            "state": "ok",
            "wake_id": row.wake_id,
            "sandbox_id": row.sandbox_id,
            "ready_at": row.ready_at_secs,
            "agent_url": row.agent_url,
        })),
        crate::db::WakeJobState::Failed => {
            // §10.0 envelope on the body, plus the wake extras.
            let code = row
                .error_code
                .unwrap_or(crate::db::WakeErrorCode::Internal)
                .wire_code();
            let message = row
                .error_message
                .clone()
                .unwrap_or_else(|| "wake failed (no message recorded)".to_string());
            ErrorEnvelope::new(StatusCode::OK, code, message)
                .with_extra(serde_json::json!({
                    "state": "failed",
                    "wake_id": row.wake_id,
                    "sandbox_id": row.sandbox_id,
                    "updated_at": row.updated_at_secs,
                }))
                .into_response()
        }
        // Unreachable: `is_terminal()` only matches Ok / Failed; the
        // match-all branch is defense-in-depth in case the discriminant
        // domain is widened in a future migration without updating
        // this code path.
        other => err(
            500,
            "internal_error",
            format!("unexpected terminal state: {}", other.as_str()),
        ),
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

    // ─── A4: §10.0 ErrorEnvelope wire-shape pins ─────────────────
    //
    // Coverage for admin_handlers.rs error sites. ~50 call sites
    // funnel through `err()`, `feature_disabled`, or
    // `map_{snapshot,restore}_error`; testing each helper once is
    // sufficient to prevent a regression that drops the `message`
    // field at every call site that flows through it.

    use crate::error_envelope::test_helpers::body_json;
    use crate::restore_handler::RestoreHandlerError;
    use crate::snapshot_handler::SnapshotHandlerError;

    #[compio::test]
    async fn a4_admin_unauthorized_envelope() {
        let resp = unauthorized();
        assert_eq!(resp.status().as_u16(), 401);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "unauthorized");
        assert!(body["message"].is_string());
    }

    #[compio::test]
    async fn a4_admin_err_helper_envelope_all_statuses() {
        let resp = err(400, "invalid_user_id", "invalid user_id filter");
        let body = body_json(resp).await;
        assert_eq!(body["error"], "invalid_user_id");
        assert_eq!(body["message"], "invalid user_id filter");

        let resp = err(404, "sandbox_not_found", "sandbox not found");
        let body = body_json(resp).await;
        assert_eq!(body["error"], "sandbox_not_found");
        assert_eq!(body["message"], "sandbox not found");

        let resp = err(500, "pg_query_failed", "query: connection refused");
        let body = body_json(resp).await;
        assert_eq!(body["error"], "pg_query_failed");
        assert_eq!(body["message"], "query: connection refused");

        let resp = err(503, "pg_disabled", "pg integration disabled");
        let body = body_json(resp).await;
        assert_eq!(body["error"], "pg_disabled");
        assert_eq!(body["message"], "pg integration disabled");
    }

    #[compio::test]
    async fn a4_feature_disabled_envelope() {
        let resp = feature_disabled();
        assert_eq!(resp.status().as_u16(), 501);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "feature_disabled");
        assert!(body["message"]
            .as_str()
            .unwrap()
            .contains("SANDBOX_SNAPSHOT_ENABLED"));
    }

    #[compio::test]
    async fn a4_map_snapshot_error_state_mismatch_envelope() {
        let resp = map_snapshot_error(SnapshotHandlerError::StateMismatch {
            current: "snapshotted",
        });
        assert_eq!(resp.status().as_u16(), 409);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "state_mismatch");
        assert!(body["message"].is_string(), "missing `message` per §10.0");
        // §10.0 table — kind-specific extras must round-trip.
        assert_eq!(body["expected"], "running");
        assert_eq!(body["current"], "snapshotted");
    }

    #[compio::test]
    async fn a4_map_snapshot_error_not_found_envelope() {
        let resp = map_snapshot_error(SnapshotHandlerError::NotFound(
            "sbx_abc".to_string(),
        ));
        assert_eq!(resp.status().as_u16(), 404);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "not_found");
        assert!(body["message"].is_string());
        assert_eq!(body["sandbox_id"], "sbx_abc");
    }

    #[compio::test]
    async fn a4_map_restore_error_vm_index_unavailable_envelope() {
        let resp = map_restore_error(RestoreHandlerError::VmIndexUnavailable {
            requested: 7,
        });
        assert_eq!(resp.status().as_u16(), 503);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "vm_index_unavailable");
        assert!(body["message"].is_string());
        assert_eq!(body["requested"], 7);
    }

    #[compio::test]
    async fn a4_map_restore_error_state_mismatch_envelope() {
        let resp = map_restore_error(RestoreHandlerError::StateMismatch {
            current: "running",
        });
        assert_eq!(resp.status().as_u16(), 409);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "state_mismatch");
        assert!(body["message"].is_string());
        assert_eq!(body["expected"], "snapshotted");
        assert_eq!(body["current"], "running");
    }

    // ─── S4: sanitization pins — raw driver text must never appear ──
    //
    // Per security review r4 (S4): A4's uniform-envelope migration
    // funneled raw `compio_postgres::Error` / `ch-remote` error
    // strings into the wire-visible `message` field. These tests pin
    // the no-leak invariant: feed a Display impl whose string
    // contains realistic pg-DSN / SQL-fragment / ch-remote-binary-path
    // shrapnel; assert the response body does NOT contain it.

    /// Mimics a `compio_postgres::Error` rendered via Display:
    /// includes host:port, schema, and a SQL fragment.
    const PG_DSN_LEAK_SAMPLE: &str =
        "db error: connecting to host=pg-primary.internal port=5432 \
         user=sandbox_admin schema=sandbox failed: FATAL \
         password authentication failed for user \"sandbox_admin\" \
         (SQLSTATE 28P01) while executing \
         SELECT sandbox_id FROM sandbox.sandboxes WHERE user_id=$1";

    /// Mimics a `ch-remote` failure: process arg-vec + a host fs path.
    const CH_REMOTE_LEAK_SAMPLE: &str =
        "ch-remote: /usr/local/libexec/cloud-hypervisor/ch-remote \
         --api-socket /run/sandbox/alloc/abc123/api.sock snapshot \
         file:///var/lib/sandbox/snapshots/sbx_xxx exited with status 1: \
         Error: SnapshotReceive: Permission denied (os error 13)";

    #[compio::test]
    async fn admin_error_does_not_leak_pg_dsn_in_message() {
        // err_safe sanitizes pg-style errors into "database error".
        let resp = err_safe(
            500,
            "pg_query_failed",
            "database error",
            PG_DSN_LEAK_SAMPLE,
        );
        let body = body_json(resp).await;
        assert_eq!(body["error"], "pg_query_failed", "code is the stable contract");
        assert_eq!(body["message"], "database error", "message is fixed prose");
        // Wire body must not carry host, schema, SQL fragment, or
        // sqlstate from the raw pg error.
        let body_str = body.to_string();
        for needle in [
            "pg-primary.internal",
            "5432",
            "sandbox_admin",
            "SQLSTATE",
            "28P01",
            "FROM sandbox.sandboxes",
            "WHERE user_id",
        ] {
            assert!(
                !body_str.contains(needle),
                "raw pg-error fragment `{needle}` leaked into wire body: {body_str}",
            );
        }
    }

    #[compio::test]
    async fn admin_error_does_not_leak_sql_fragment_in_message() {
        // Whatever the code field, the message must not carry SQL
        // text. Test all four "database error"-class codes.
        for code in [
            "pg_query_failed",
            "pg_tx_begin_failed",
            "gdpr_delete_sandboxes_failed",
            "audit_insert_failed",
        ] {
            let resp = err_safe(500, code, "database error", PG_DSN_LEAK_SAMPLE);
            let body = body_json(resp).await;
            let body_str = body.to_string();
            assert!(
                !body_str.contains("SELECT") && !body_str.contains("FROM sandbox."),
                "code={code} leaked SQL into body: {body_str}",
            );
            assert_eq!(body["message"], "database error");
        }
    }

    #[compio::test]
    async fn admin_error_does_not_leak_ch_remote_path_in_message() {
        // Snapshot/restore handlers funnel ch-remote stderr into
        // SnapshotHandlerError::ChRemote(String). The map_*_error
        // path now routes through err_safe → "hypervisor error".
        let resp = map_snapshot_error(SnapshotHandlerError::ChRemote(
            CH_REMOTE_LEAK_SAMPLE.to_string(),
        ));
        let body = body_json(resp).await;
        assert_eq!(body["error"], "ch_remote_failed");
        assert_eq!(body["message"], "hypervisor error");
        let body_str = body.to_string();
        for needle in [
            "/usr/local/libexec",
            "ch-remote",
            "/run/sandbox/alloc",
            "/var/lib/sandbox",
            "api.sock",
            "Permission denied",
            "os error 13",
        ] {
            assert!(
                !body_str.contains(needle),
                "raw ch-remote fragment `{needle}` leaked into wire body: {body_str}",
            );
        }
    }

    #[compio::test]
    async fn admin_error_internal_message_is_fixed_prose() {
        // SnapshotHandlerError::Internal carries an arbitrary String
        // from inner layers — could be a panic message, a backtrace,
        // anything. The message field must collapse to fixed prose.
        let resp = map_snapshot_error(SnapshotHandlerError::Internal(
            "panicked at 'index out of bounds' in registry.rs:847".to_string(),
        ));
        let body = body_json(resp).await;
        assert_eq!(body["error"], "internal_error");
        assert_eq!(body["message"], "internal error");
        let body_str = body.to_string();
        assert!(
            !body_str.contains("panicked") && !body_str.contains("registry.rs"),
            "internal-error raw text leaked: {body_str}",
        );
    }

    #[compio::test]
    async fn admin_error_keeps_public_identifiers_in_envelope() {
        // The threat model is internal infrastructure leaking. Public
        // identifiers (sandbox_id, user_id, requested vm_index) that
        // the client itself supplied are FINE to keep — operators
        // need them for diagnostic clarity. This test pins that
        // sanitization does NOT over-strip.
        let resp = map_snapshot_error(SnapshotHandlerError::NotFound(
            "sbx_abc123".to_string(),
        ));
        let body = body_json(resp).await;
        assert_eq!(body["error"], "not_found");
        assert_eq!(
            body["sandbox_id"], "sbx_abc123",
            "client-supplied identifier must survive sanitization",
        );

        let resp = map_restore_error(RestoreHandlerError::VmIndexUnavailable {
            requested: 42,
        });
        let body = body_json(resp).await;
        assert_eq!(body["requested"], 42, "structural extras must survive");
    }

    // ─── C-7-LT-PR2 wake handler wire-format tests ───────────────
    //
    // Each test calls `render_wake_poll_response` directly with a
    // hand-rolled `WakeJobRow` (pure pg-free fixture). The function
    // is extracted from `poll_wake` for this purpose. End-to-end
    // ntex integration is the cluster-smoke gate; these pin the
    // wire shape so a refactor that drops `message` or renames
    // `error` to `error_code` fails CI before the cluster cycle.

    use crate::db::{WakeErrorCode, WakeJobRow, WakeJobState};

    fn make_wake_row(state: WakeJobState) -> WakeJobRow {
        WakeJobRow {
            wake_id: "wak_0Bk3Np4qR5sT7uV8wYz1A2".to_string(),
            sandbox_id: "sbx_AbCdEfGhIjKlMnOpQrStUv".to_string(),
            state,
            error_code: None,
            error_message: None,
            started_at_secs: 1_700_000_000,
            updated_at_secs: 1_700_000_007,
            ready_at_secs: None,
            agent_url: None,
            lessee: "host_id_xyz".to_string(),
            lessee_updated_at_secs: 1_700_000_007,
        }
    }

    #[compio::test]
    async fn r16_api1_failed_state_body_uses_error_and_message_keys() {
        // R16-API1 #1: the failed-state body uses §10.0
        // `error`/`message` keys, NOT the proposal's pre-review
        // `error_code`/`error_message`. This is the single highest-
        // value test of the PR — if it regresses, the wire format
        // diverges from every other endpoint in the crate.
        let mut row = make_wake_row(WakeJobState::Failed);
        row.error_code = Some(WakeErrorCode::LivezTimeout);
        row.error_message = Some("/livez never returned 200".to_string());

        let resp = render_wake_poll_response(&row);
        assert_eq!(resp.status().as_u16(), 200, "failed-state is HTTP 200 OK");
        let body = body_json(resp).await;

        // §10.0 envelope fields — REQUIRED.
        assert_eq!(body["error"], "livez_timeout", "wire code per R16-API1 #3");
        assert_eq!(body["message"], "/livez never returned 200");

        // Wake-specific extras — flat at the top level.
        assert_eq!(body["state"], "failed");
        assert_eq!(body["wake_id"], row.wake_id);
        assert_eq!(body["sandbox_id"], row.sandbox_id);

        // Anti-test: the proposal's pre-review field names MUST NOT
        // appear. (Belt-and-suspenders against a regression that
        // copies the proposal text verbatim.)
        assert!(
            body.get("error_code").is_none(),
            "deprecated `error_code` key must NOT be on the wire (R16-API1 #1)"
        );
        assert!(
            body.get("error_message").is_none(),
            "deprecated `error_message` key must NOT be on the wire (R16-API1 #1)"
        );
    }

    #[compio::test]
    async fn r16_api1_failed_state_renders_every_wake_error_code() {
        // Every variant of WakeErrorCode must render with the
        // wire_code() string, not the as_str() pg form. Drift between
        // the two is exactly the bug R16-API1 #3 forbids.
        for code in [
            WakeErrorCode::SlotUnavailable,
            WakeErrorCode::SourceTeardownTimeout,
            WakeErrorCode::RestoreFailed,
            WakeErrorCode::LivezTimeout,
            WakeErrorCode::ClockResyncFailed,
            WakeErrorCode::RegisterFailed,
            WakeErrorCode::Internal,
        ] {
            let mut row = make_wake_row(WakeJobState::Failed);
            row.error_code = Some(code);
            row.error_message = Some("explanation".to_string());
            let resp = render_wake_poll_response(&row);
            let body = body_json(resp).await;
            assert_eq!(
                body["error"], code.wire_code(),
                "wire code drift for {:?}", code
            );
        }
    }

    #[compio::test]
    async fn poll_wake_intermediate_state_returns_202_with_state() {
        // Every non-terminal state → 202 + state + timestamps.
        for state in [
            WakeJobState::Pending,
            WakeJobState::ReservingSlot,
            WakeJobState::Restoring,
            WakeJobState::LivezPolling,
            WakeJobState::ClockResyncing,
            WakeJobState::Registering,
        ] {
            let row = make_wake_row(state);
            let resp = render_wake_poll_response(&row);
            assert_eq!(
                resp.status().as_u16(),
                202,
                "intermediate state {:?} must be 202", state
            );
            let body = body_json(resp).await;
            assert_eq!(body["state"], state.as_str());
            assert_eq!(body["wake_id"], row.wake_id);
            assert_eq!(body["sandbox_id"], row.sandbox_id);
            assert_eq!(body["started_at"], row.started_at_secs);
        }
    }

    #[compio::test]
    async fn poll_wake_terminal_ok_returns_200_with_agent_url() {
        let mut row = make_wake_row(WakeJobState::Ok);
        row.ready_at_secs = Some(1_700_000_009);
        row.agent_url = Some("http://10.0.100.2:7777".to_string());

        let resp = render_wake_poll_response(&row);
        assert_eq!(resp.status().as_u16(), 200);
        let body = body_json(resp).await;
        assert_eq!(body["state"], "ok");
        assert_eq!(body["agent_url"], "http://10.0.100.2:7777");
        assert_eq!(body["ready_at"], 1_700_000_009);
    }

    #[compio::test]
    async fn poll_wake_failed_with_missing_message_falls_back() {
        // Defensive: a row with state=failed but error_message=NULL
        // (shouldn't happen post-PR2, but pg could regress) must still
        // emit a well-formed §10.0 envelope. The message is opaque
        // fallback prose.
        let mut row = make_wake_row(WakeJobState::Failed);
        row.error_code = Some(WakeErrorCode::Internal);
        row.error_message = None;

        let resp = render_wake_poll_response(&row);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "internal_error");
        assert!(body["message"].is_string());
        assert!(
            !body["message"].as_str().unwrap().is_empty(),
            "fallback message must be non-empty so clients have a string to display"
        );
    }

    #[compio::test]
    async fn poll_wake_failed_with_missing_error_code_defaults_to_internal() {
        // Defense-in-depth: state=failed but error_code=NULL maps to
        // `internal_error` on the wire. Migration-tolerance: future
        // CHECK domain expansion + downgrade path mustn't crash the
        // reader.
        let mut row = make_wake_row(WakeJobState::Failed);
        row.error_code = None;
        row.error_message = Some("opaque".to_string());

        let resp = render_wake_poll_response(&row);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "internal_error");
        assert_eq!(body["state"], "failed");
    }

    #[test]
    fn wake_query_default_is_no_override() {
        // The default shape MUST resolve `sync=None` so the handler
        // routes to the wake_response_mode-driven default branch. Any
        // regression that gives `Some(0)` would also still route async
        // (the predicate is `matches!(q.sync, Some(1))`); covered as a
        // contract test rather than a behavior test.
        let q = WakeQuery::default();
        assert!(q.sync.is_none());
    }

    #[test]
    fn wake_query_sync_one_triggers_sync_branch() {
        // Predicate the handler uses: `matches!(q.sync, Some(1))`.
        // Pin the only literal that flips to the legacy path. Other
        // values (`sync=0`, `sync=2`, …) are treated as "no override"
        // so a typo doesn't accidentally lock the operator into
        // legacy.
        let q = WakeQuery { sync: Some(1) };
        assert!(matches!(q.sync, Some(1)));
        let q = WakeQuery { sync: Some(0) };
        assert!(!matches!(q.sync, Some(1)));
        let q = WakeQuery { sync: Some(2) };
        assert!(!matches!(q.sync, Some(1)));
        let q = WakeQuery { sync: None };
        assert!(!matches!(q.sync, Some(1)));
    }
}

