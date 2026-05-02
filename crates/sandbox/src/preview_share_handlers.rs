//! Phase-3 controller-side mint/list/revoke handlers
//! (preview-URL § III "API specification").
//!
//! All four endpoints live under `/sandboxes/{id}/preview/{port}/share`:
//!
//! - `POST` — mint a fresh token (creator-bearer; ownership-gated).
//! - `GET` — list audit-metadata for tokens this sandbox has issued.
//! - `DELETE` — rotate the secret + clear `previous` (zero-grace);
//!   wipes the audit table.
//! - `DELETE /{token_id}` — per-token revoke. **Phase 5** — returns
//!   `501 Not Implemented` with `code: "deferred-to-phase-5"` so the
//!   API surface is reserved.
//!
//! ## Auth
//!
//! Every handler reuses the existing `?user_id=<id>` ownership gate
//! that `require_owner` provides for the rest of the controller's
//! API. Coalesced 404 on auth-failure / not-owner / sandbox-not-found
//! per round-6 H4. The bearer-token check fires before that.
//!
//! ## Rate-limit
//!
//! `POST` is rate-limited at 100 mints / sandbox / day via a simple
//! in-memory token bucket (preview-URL § VI R-14). Production-grade
//! quota tracking lives elsewhere; this is the v1 minimum.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ntex::http::StatusCode;
use ntex::web::{self, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::preview_share::{
    fresh_token_id, is_known_scope, mint, TokenClaims, MINT_TTL_MAX_SECS,
    MINT_TTL_MIN_SECS,
};
use crate::registry::PreviewAuditEntry;
use crate::{auth, AppState};
use zeroship_core::preview_ports::{is_proxyable_port, DEFAULT_DENY};

type State = web::types::State<Arc<AppState>>;

/// Per-sandbox-per-day mint cap (§ VI R-14).
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
    /// present. Phase-5 GA promotes this to a hard requirement.
    #[serde(default)]
    pub iss: Option<String>,
}

fn unauthorized() -> HttpResponse {
    HttpResponse::Unauthorized().json(&json!({
        "error": "unauthorized",
        "code": "auth_required",
    }))
}

fn not_found() -> HttpResponse {
    HttpResponse::NotFound().json(&json!({
        "error": "not found",
        "code": "not_found",
    }))
}

fn bad_request(code: &'static str, msg: &str) -> HttpResponse {
    HttpResponse::BadRequest().json(&json!({"error": msg, "code": code}))
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
    let id: Uuid = sandbox_id_str.parse().map_err(|_| not_found())?;
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
        return HttpResponse::TooManyRequests().json(&json!({
            "error": "rate limited",
            "code": "rate_limited",
            "retry_after_secs": 86_400 - (now % 86_400),
        }));
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

    // Persist (best-effort). The seal helper isn't hooked up at the
    // mint path in v1 — explicit-DELETE wipes both in-memory + on-disk
    // via the sandbox-stop pathway today. Phase-5 GA wires per-mint
    // re-seal.

    HttpResponse::Ok().json(&json!({
        "token": token,
        "token_id": token_id,
        "issued_at_unix": now,
        "expires_at_unix": claims.exp,
        "scope": claims.scope,
        "secret_version": ring.sv_current,
        // Public share URL — Phase 4 lands real DNS; until then we
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
                "token_id": r.token_id,
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
    HttpResponse::Ok().json(&json!({
        "revoked": "all",
        "secret_version_current": sv,
        "grace": "none",
        "note": "explicit DELETE is zero-grace; all prior tokens are now invalid.",
    }))
}

/// `DELETE /sandboxes/{id}/preview/{port}/share/{token_id}` — Phase 5
/// per-token revoke stub (preview-URL § II.4 "Per-token revocation").
/// Returns 501 with `code: "deferred-to-phase-5"` so the API surface
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
    HttpResponse::build(StatusCode::NOT_IMPLEMENTED).json(&json!({
        "error": "per-token revoke not implemented in v1",
        "code": "deferred-to-phase-5",
        "note": "use DELETE /sandboxes/{id}/preview/{port}/share to rotate the \
                 whole secret; this rotates ALL tokens at once.",
    }))
}

/// Render a sandbox slug for the public preview hostname. Mirrors
/// `preview::compute_preview_host` — DNS labels are case-insensitive
/// AND the slug is restricted to lowercase alphanumeric + `-` (round-6
/// CRITICAL-5). For v1 we lower-case + strip non-alphanumerics; Phase-4
/// wires the typed-id lowercase emitter directly.
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
}
