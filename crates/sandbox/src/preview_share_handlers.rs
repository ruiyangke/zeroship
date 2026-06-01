//! Controller-side mint/list/revoke handlers
//! (`docs/proposals/sandbox-preview-urls.md` §III "API specification").
//!
//! All four endpoints live under `/sandboxes/{id}/preview/{port}/share`:
//!
//! - `POST` — mint a fresh token (creator-bearer; ownership-gated).
//! - `GET` — list audit-metadata for tokens this sandbox has issued.
//! - `DELETE` — rotate the secret + clear `previous` (zero-grace);
//!   wipes the audit table.
//! - `DELETE /{token_id}` — per-token revoke. Returns
//!   `501 Not Implemented` with `error: "deferred_to_phase_5"` so the
//!   API surface is reserved until per-token revoke is implemented.
//!
//! ## Auth
//!
//! Every handler reuses the existing `?user_id=<id>` ownership gate
//! that `require_owner` provides for the rest of the controller's
//! API. Coalesced 404 on auth-failure / not-owner / sandbox-not-found
//! per uniform-auth rule. The bearer-token check fires before that.
//!
//! ## Rate-limit
//!
//! `POST` is rate-limited at 100 mints / sandbox / day via a simple
//! in-memory token bucket (`docs/proposals/sandbox-preview-urls.md`
//! §VI). Production-grade quota tracking lives elsewhere; this is the
//! minimum controller-side guard.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error_envelope::{error_response, ErrorEnvelope};
use crate::preview_share::{
    fresh_token_id, is_known_scope, mint, TokenClaims, MINT_TTL_MAX_SECS,
    MINT_TTL_MIN_SECS,
};
use crate::registry::PreviewAuditEntry;
use crate::{auth, AppState};
use zeroship_core::preview_ports::{is_proxyable_port, DEFAULT_DENY};

type State = web::types::State<Arc<AppState>>;

/// Per-sandbox-per-day mint cap.
const PER_SANDBOX_DAILY_MINT_CAP: u32 = 100;

/// In-memory token bucket. Keyed on `sandbox_id`. Refills at midnight
/// UTC (rounded to UTC-day boundaries; cheap and good enough for v1).
#[derive(Default)]
pub struct MintRateLimiter {
    /// Maps `sandbox_id` → (current-day-key, mints-this-day).
    by_sandbox: Mutex<HashMap<Uuid, (u64, u32)>>,
}

impl std::fmt::Debug for MintRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintRateLimiter").finish_non_exhaustive()
    }
}

impl MintRateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take one mint slot. Returns `true` if accepted, `false` if the
    /// sandbox already minted `PER_SANDBOX_DAILY_MINT_CAP` tokens
    /// today (UTC).
    pub fn take(&self, sandbox_id: Uuid, now_unix: u64) -> bool {
        let day = now_unix / 86_400;
        let mut g = self.by_sandbox.lock().unwrap();
        let entry = g.entry(sandbox_id).or_insert((day, 0));
        if entry.0 != day {
            *entry = (day, 0);
        }
        if entry.1 >= PER_SANDBOX_DAILY_MINT_CAP {
            return false;
        }
        entry.1 += 1;
        true
    }
}

#[derive(Debug, Deserialize)]
pub struct MintBody {
    pub expires_in_secs: u64,
    pub scope: String,
    /// Optional issuer typed-id (creator's `usr_…`). Recorded into
    /// the audit log; written into the token's `iss` claim if
    /// present. A later GA pass can promote this to a hard
    /// requirement.
    #[serde(default)]
    pub iss: Option<String>,
}

fn unauthorized() -> HttpResponse {
    error_response(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "authentication required",
    )
}

fn not_found() -> HttpResponse {
    error_response(StatusCode::NOT_FOUND, "not_found", "not found")
}

fn bad_request(code: &'static str, msg: &str) -> HttpResponse {
    error_response(StatusCode::BAD_REQUEST, code, msg.to_string())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Lift the request's `?user_id=…` query param. Same parser as
/// `handlers::require_owner`. Returns an empty string if missing.
fn query_user_id(req: &HttpRequest) -> String {
    req.uri()
        .query()
        .and_then(|q| {
            q.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                if k == "user_id" {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default()
}

/// Authorize the caller. Returns `Some(sandbox_id)` on success, else
/// the appropriate uniform 401/404 response. Coalesced like
/// `preview_proxy::authorize` — bad auth → 401; everything else → 404.
fn authorize_owner(
    req: &HttpRequest,
    state: &AppState,
    sandbox_id_str: &str,
    port: u16,
) -> Result<Uuid, HttpResponse> {
    if !auth::check(req, state) {
        return Err(unauthorized());
    }
    let user_id = query_user_id(req);
    if user_id.is_empty() {
        // Coalesce with not-found: don't leak that auth would have
        // succeeded had user_id been present.
        return Err(not_found());
    }
    // : accept the typed-id form returned
    // by `POST /sandboxes` AND the bare UUID form for back-compat.
    // Both map to 404 (uniform-no-existence-oracle) on parse failure.
    let id: Uuid = zeroship_core::typed_id::parse_with_prefix(sandbox_id_str, "sbx")
        .ok()
        .or_else(|| sandbox_id_str.parse().ok())
        .ok_or_else(not_found)?;
    if !is_proxyable_port(port, DEFAULT_DENY) {
        return Err(not_found());
    }
    match state.sandboxes.get(&id) {
        Some(info) if info.user_id == user_id => Ok(id),
        _ => Err(not_found()),
    }
}

/// `POST /sandboxes/{id}/preview/{port}/share` — mint a token.
pub async fn mint_share(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, u16)>,
    body: web::types::Json<MintBody>,
) -> HttpResponse {
    let (sandbox_id_str, port) = path.into_inner();
    let id = match authorize_owner(&req, &state, &sandbox_id_str, port) {
        Ok(id) => id,
        Err(r) => return r,
    };

    // Body validation — § III: 60 ≤ expires_in_secs ≤ 1 week.
    if body.expires_in_secs < MINT_TTL_MIN_SECS
        || body.expires_in_secs > MINT_TTL_MAX_SECS
    {
        return bad_request(
            "invalid_expires",
            "expires_in_secs must be in [60, 604800]",
        );
    }
    if !is_known_scope(&body.scope) {
        return bad_request(
            "invalid_scope",
            "scope must be \"ro\" or \"rw\"",
        );
    }

    // Rate-limit before any HMAC work.
    let now = unix_now();
    let rl = state
        .mint_rate_limiter
        .as_ref()
        .expect("mint_rate_limiter wired in AppState");
    if !rl.take(id, now) {
        return ErrorEnvelope::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "share token mint rate limit exceeded (100 per sandbox per day)",
        )
        .with_extra(json!({
            "retry_after_secs": 86_400 - (now % 86_400),
        }))
        .into_response();
    }

    // Mint a secret ring on first use; idempotent for the rest.
    let ring = match state.sandboxes.ensure_preview_secret(id) {
        Some(r) => r,
        None => return not_found(),
    };

    // Build claims + sign.
    let token_id = fresh_token_id();
    let claims = TokenClaims {
        aud: "preview".into(),
        sbx: sandbox_id_str.clone(),
        port,
        iss: body.iss.clone(),
        iat: now,
        exp: now + body.expires_in_secs,
        sv: ring.sv_current,
        tid: token_id.clone(),
        scope: body.scope.clone(),
    };
    let token = mint(&claims, &ring.current);

    // Append audit row.
    state.sandboxes.append_audit(
        id,
        PreviewAuditEntry {
            token_id: token_id.clone(),
            port,
            issued_at_unix: now,
            expires_at_unix: claims.exp,
            scope: claims.scope.clone(),
            secret_version: ring.sv_current,
            iss: claims.iss.clone(),
            last_used_at_unix: 0,
            use_count: 0,
        },
    );

    // Persist-on-mint (best-effort). A controller crash between the
    // in-memory ring/audit update and the next sealed-record write
    // would lose the freshly-minted ring otherwise (the per-token
    // audit metadata is in pg now, round-8). Failures here are
    // logged and swallowed.
    persist_preview_state(&state, id).await;

    // Per-token audit metadata now lives in pg. Write the share row
    // synchronously; on error, log and continue.
    if let Some(db) = state.database.as_ref() {
        // Migration 0003 widened the schema CHECK from base62 to
        // base64url so we can store `tok_<raw_tid>` byte-exactly. The
        // earlier `replace(['-','_'], "x")` munge collapsed distinct
        // tokens whose raw tids differed only in `-` vs `_` (or
        // happened to contain `x`).
        let typed_token_id = format!("tok_{token_id}");
        let row = crate::db::ShareRow {
            // Storage form is `tok_<raw_tid>`; the API-returned
            // `share_token_id` is `shr_<raw_tid>`. Same payload after
            // the prefix, distinct prefixes to mark API surface vs
            // storage surface.
            token_id: typed_token_id.clone(),
            sandbox_id: sandbox_id_str.clone(),
            port,
            scope: claims.scope.clone(),
            secret_version: claims.sv as i32,
            issued_at_secs: now,
            expires_at_secs: claims.exp,
            iss: claims.iss.clone(),
        };
        if let Err(e) = db.insert_share(&row).await {
            tracing::warn!(
                sandbox_id = %sandbox_id_str,
                error = %e,
                "sandbox/preview_share: pg insert_share failed (non-fatal)"
            );
        }
        // Skip the audit event entirely when the registry has no
        // record of the sandbox owner. The
        // pre-fix synthetic `usr_unknown` would have failed the
        // user_id CHECK on zeroship.events anyway (the row's
        // user_id must match `^usr_[0-9A-Za-z]{20,40}$`); falling
        // back to a warn-and-continue is honest about the
        // missing-info case and keeps the audit log self-consistent.
        if let Some(owner) = state.sandboxes.get(&id).map(|i| i.user_id) {
            let evt_data = serde_json::json!({
                "token_id": format!("shr_{token_id}"),
                "port": port,
                "scope": claims.scope,
                "expires_at": claims.exp,
            });
            let event = crate::db::Database::new_event(
                &sandbox_id_str,
                &owner,
                "share.minted",
                evt_data.to_string(),
            );
            let _ = db.insert_event(&event).await;
        } else {
            tracing::warn!(
                sandbox_id = %sandbox_id_str,
                "sandbox/preview_share: sandbox owner not in registry; skipping share.minted audit event"
            );
        }
    }

    HttpResponse::Ok().json(&json!({
        "token": token,
        // Wire-stable token_id is `shr_` + raw `tid` claim base64url;
        // prefix is presentation-only. The internal `tid` claim and
        // audit-table key stay byte-exact (no migration of stored
        // rows).
        "token_id": format!("shr_{token_id}"),
        "issued_at_unix": now,
        "expires_at_unix": claims.exp,
        "scope": claims.scope,
        "secret_version": ring.sv_current,
        // Public share URL — real DNS is not wired yet, so we
        // emit the canonical pattern so the UI can render-and-copy.
        "share_url": format!(
            "https://preview-{slug}-{port}.preview.zeroship.dev/__zsbx_share?t={token}",
            slug = sandbox_slug(&sandbox_id_str),
            port = port,
            token = token,
        ),
    }))
}

/// `GET /sandboxes/{id}/preview/{port}/share` — list audit metadata.
/// Token bytes are NOT retrievable; the holder's URL is the only
/// place they live.
pub async fn list_share(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, u16)>,
) -> HttpResponse {
    let (sandbox_id_str, port) = path.into_inner();
    let id = match authorize_owner(&req, &state, &sandbox_id_str, port) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let rows: Vec<_> = state
        .sandboxes
        .list_audit(id)
        .into_iter()
        .filter(|r| r.port == port)
        .map(|r| {
            json!({
                // Wire-stable token_id is `shr_` + raw `tid` claim
                // base64url; prefix is presentation-only.
                "token_id": format!("shr_{}", r.token_id),
                "issued_at_unix": r.issued_at_unix,
                "expires_at_unix": r.expires_at_unix,
                "scope": r.scope,
                "secret_version": r.secret_version,
                "iss": r.iss,
                "last_used_at_unix": r.last_used_at_unix,
                "use_count": r.use_count,
            })
        })
        .collect();
    let sv_current = state
        .sandboxes
        .preview_secret(id)
        .map(|s| s.sv_current)
        .unwrap_or(0);
    HttpResponse::Ok().json(&json!({
        "tokens": rows,
        "secret_version_current": sv_current,
    }))
}

/// `DELETE /sandboxes/{id}/preview/{port}/share` — rotate-and-clear
/// (zero-grace). Bumps the secret AND clears `previous` immediately.
pub async fn revoke_all_share(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, u16)>,
) -> HttpResponse {
    let (sandbox_id_str, port) = path.into_inner();
    let id = match authorize_owner(&req, &state, &sandbox_id_str, port) {
        Ok(id) => id,
        Err(r) => return r,
    };
    let new_ring = state.sandboxes.rotate_preview_secret(id, true);
    let sv = new_ring.map(|r| r.sv_current).unwrap_or(0);
    // Persist-on-rotate (best-effort). The sandbox itself isn't
    // dropped — the sealed record is updated, NOT removed; restart
    // restore must read back the new (post-rotate) state. See doc
    // § II.4 "Revocation".
    persist_preview_state(&state, id).await;

    // Pg-side rotation revokes every existing share row for this
    // sandbox so the validator can refuse them.
    if let Some(db) = state.database.as_ref() {
        if let Err(e) = db.rotate_share_secret(id).await {
            tracing::warn!(
                sandbox_id = %id,
                error = %e,
                "sandbox/preview_share: pg rotate_share_secret failed (non-fatal)"
            );
        }
    }
    HttpResponse::Ok().json(&json!({
        "revoked": "all",
        "secret_version_current": sv,
        "grace": "none",
        "note": "explicit DELETE is zero-grace; all prior tokens are now invalid.",
    }))
}

/// Snapshot the registry's current preview state for `sandbox_id` and
/// hand it to the backend's seal-with-preview-state helper. Best-
/// effort: a backend that has no persistence configured returns
/// `Ok(false)` (nothing to write); an I/O failure is logged at WARN
/// and swallowed. The API caller never fails on seal failure — the
/// in-memory ring/audit is authoritative for live traffic.
async fn persist_preview_state(state: &Arc<AppState>, sandbox_id: Uuid) {
    let info = match state.sandboxes.get(&sandbox_id) {
        Some(i) => i,
        None => return,
    };
    let (secrets, audit) = state.sandboxes.preview_state_for_seal(sandbox_id);
    match state
        .backend
        .seal_with_preview_state(sandbox_id, &info, secrets, audit)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            // Persistence disabled OR sandbox-id not in backend's
            // session map. Persistence is off by
            // default; this is the normal path for that mode.
        }
        Err(e) => {
            tracing::warn!(
                sandbox_id = %sandbox_id,
                error = %e,
                "sandbox/preview_share persist-on-mint/rotate failed (non-fatal; in-memory state is authoritative; restart-restore degraded for this mint)"
            );
        }
    }
}

/// `DELETE /sandboxes/{id}/preview/{port}/share/{token_id}` — per-token
/// revoke stub (`preview-URL` § II.4 "Per-token revocation").
/// Returns 501 with `error: "deferred_to_phase_5"` so the API surface
/// is reserved without claiming functionality v1 doesn't have.
pub async fn revoke_one_share(
    req: HttpRequest,
    state: State,
    path: web::types::Path<(String, u16, String)>,
) -> HttpResponse {
    let (sandbox_id_str, port, _token_id) = path.into_inner();
    // Still authenticate so a hostile probe doesn't leak the existence
    // of the endpoint without auth.
    if let Err(r) = authorize_owner(&req, &state, &sandbox_id_str, port) {
        return r;
    }
    ErrorEnvelope::new(
        StatusCode::NOT_IMPLEMENTED,
        "deferred_to_phase_5",
        "per-token revoke not implemented in v1",
    )
    .with_extra(json!({
        "note": "use DELETE /sandboxes/{id}/preview/{port}/share to rotate the \
                 whole secret; this rotates ALL tokens at once.",
    }))
    .into_response()
}

/// Render a sandbox slug for the public preview hostname. Mirrors
/// `preview::compute_preview_host` — DNS labels are case-insensitive
/// AND the slug is restricted to lowercase alphanumeric + `-`. For v1
/// we lower-case + strip non-alphanumerics; a later DNS pass can wire
/// the typed-id lowercase emitter directly.
fn sandbox_slug(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_caps_at_100_per_day() {
        let rl = MintRateLimiter::new();
        let id = Uuid::now_v7();
        let now = 1_000_000;
        for _ in 0..100 {
            assert!(rl.take(id, now));
        }
        assert!(!rl.take(id, now), "101st mint must be denied");
        // New day → bucket resets.
        assert!(rl.take(id, now + 86_400));
    }

    #[test]
    fn rate_limit_per_sandbox_independent() {
        let rl = MintRateLimiter::new();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        let now = 1_000_000;
        for _ in 0..100 {
            rl.take(a, now);
        }
        assert!(!rl.take(a, now));
        // sandbox b's bucket is fresh.
        assert!(rl.take(b, now));
    }

    #[test]
    fn sandbox_slug_lowercases_and_strips() {
        assert_eq!(
            sandbox_slug("11111111-2222-3333-4444-555555555555"),
            "11111111222233334444555555555555"
        );
        assert_eq!(sandbox_slug("ABC-123"), "abc123");
    }

    // ─── A4: §10.0 ErrorEnvelope wire-shape pins ─────────────────
    //
    // Coverage for preview_share_handlers.rs error sites. Pre-A4
    // these emitted `{"error":<prose>,"code":<code>}`; now they
    // funnel through `error_response` / `ErrorEnvelope` carrying
    // `error` (code) + `message` (human prose) per §10.0.

    use crate::error_envelope::test_helpers::body_json;

    #[compio::test]
    async fn a4_share_unauthorized_envelope() {
        let resp = unauthorized();
        assert_eq!(resp.status().as_u16(), 401);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "unauthorized");
        assert!(body["message"].is_string());
        assert!(body.get("code").is_none(), "duplicate `code` field removed");
    }

    #[compio::test]
    async fn a4_share_not_found_envelope() {
        let resp = not_found();
        assert_eq!(resp.status().as_u16(), 404);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "not_found");
        assert!(body["message"].is_string());
        assert!(body.get("code").is_none());
    }

    #[compio::test]
    async fn a4_share_bad_request_envelope() {
        let resp = bad_request("invalid_expires", "expires_in_secs out of range");
        assert_eq!(resp.status().as_u16(), 400);
        let body = body_json(resp).await;
        assert_eq!(body["error"], "invalid_expires");
        assert_eq!(body["message"], "expires_in_secs out of range");
    }
}
