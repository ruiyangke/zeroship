//! `POST /oidc/backchannel-logout` — OIDC BCL 1.0 RP endpoint.
//!
//! Hydra POSTs a signed `logout_token` JWT here when a user signs out
//! via `/oauth2/sessions/logout`. The gateway verifies the token (see
//! [`zeroship_core::logout_token`]) and revokes the user's app
//! sessions.
//!
//! The endpoint is registered at the gateway-host level (not per
//! creator-app subdomain) because the URI must be stable for every
//! `backchannel_logout_uri` registered with hydra. In production that
//! lives at `https://api.zeroship.ai/oidc/backchannel-logout` — see
//! `ops/auth-clients.example.toml`.
//!
//! Response contract:
//! - 200 + `cache-control: no-store` on success
//! - 400 + `cache-control: no-store` on any verify failure (with a
//!   short text body; hydra logs the body so it shows up in the auth
//!   server's debug surface)
//!
//! Phase 7 U1.2.

use std::sync::Arc;

use ntex::web::{self, types::Form, types::State, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::sessions;
use crate::GateState;

/// Form body shape per OIDC BCL §2.5 — a single `logout_token` field,
/// `x-www-form-urlencoded`. ntex's `Form<T>` parses that automatically.
#[derive(Debug, Deserialize)]
pub struct LogoutForm {
    pub logout_token: String,
}

/// Verify the inbound `logout_token` and revoke the affected gateway
/// sessions.
///
/// Revocation policy (Phase 7 minimum): revoke ALL sessions for the
/// `sub` (across every hosted app on this gateway). The spec also
/// supports a `sid`-only token but we don't currently correlate
/// hydra's session id to our `gateway_sessions.id`, so even when `sid`
/// is present we fall back to revoke-by-`sub`. If only `sid` is set
/// and `sub` is absent, the token is still verified but no session
/// rows are touched — log it for debug visibility.
#[allow(clippy::future_not_send)]
pub async fn handle(
    form: Form<LogoutForm>,
    state: State<Arc<GateState>>,
) -> HttpResponse {
    // `state.oidc_rp.issuer` is the canonical hydra issuer string
    // (built from `auth_ui_url` at boot, optionally overridden via
    // `OidcRp::with_issuer` in tests).
    let issuer = state.oidc_rp.issuer.clone();

    // Per-app BCL disambiguation (auth-sdk Slice 1d, spec §1.2). Each per-app
    // OAuth client registers its own `backchannel_logout_uri` with its own
    // `aud` (= the per-app `client_id`, `oac_<base62>`). Peek the token's `aud`
    // (routing only — the signature is still verified below) to learn which
    // client it is for; a per-app client resolves to one app's subdomain so we
    // revoke only THAT app's sessions. The legacy shared `gateway` client
    // (`state.oidc_rp.client_id`) still revokes across the subject's gateway
    // sessions for that aud.
    //
    // `revoke_scope` carries the app subdomain when a per-app client matched
    // (revoke only that app), or `None` for the shared-client all-apps path.
    let aud_candidates =
        zeroship_core::logout_token::unverified_aud_candidates(&form.logout_token);
    let (aud, revoke_scope): (String, Option<String>) = aud_candidates
        .iter()
        .find_map(|cand| {
            state
                .routes
                .lookup_by_oauth_client_id(cand)
                .map(|(_id, route)| (cand.clone(), Some(route.entry.name.clone())))
        })
        .unwrap_or_else(|| (state.oidc_rp.client_id.clone(), None));

    let token = match zeroship_core::logout_token::verify(
        &state.oidc_rp.jwks,
        &form.logout_token,
        &issuer,
        &aud,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "backchannel_logout: verify failed");
            return HttpResponse::BadRequest()
                .header("cache-control", "no-store")
                .body("invalid logout_token");
        }
    };

    let now_secs = unix_now_secs();
    if !state.logout_jti_cache.insert(
        &token.jti,
        now_secs,
        zeroship_core::logout_token::LOGOUT_JTI_TTL_SECS,
    ) {
        tracing::warn!(
            jti = %token.jti,
            sub = ?token.sub,
            sid = ?token.sid,
            "backchannel_logout: replayed logout_token jti; skipping revocation"
        );
        return HttpResponse::Ok()
            .header("cache-control", "no-store")
            .finish();
    }

    // Revoke. The verifier guarantees at least one of sub/sid is
    // present, but only sub maps to a row filter today (the
    // `gateway_sessions.user_id` column).
    // Check out ONE pooled connection for the whole revocation block.
    // Every DB touch below (session revoke, wrapper-subject revoke, audit
    // insert) runs sequentially with no outbound HTTP in between — the
    // `logout_token` verify (which may fetch JWKS) already completed
    // above — so a single short-lived checkout covers the block and is
    // released on drop at the end of the `if let`. `pool` is bound first
    // so it outlives the `conn` borrowed from it (drop is reverse
    // declaration order).
    let pool = match state.db.as_ref() {
        Some(db_cfg) => match crate::db::checkout(db_cfg).await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::error!(error = %e, "backchannel_logout: pg pool checkout failed");
                None
            }
        },
        None => None,
    };
    let conn = match pool.as_ref() {
        Some(pool) => match pool.get().await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::error!(error = %e, "backchannel_logout: pg pool checkout failed");
                None
            }
        },
        None => None,
    };
    if let Some(conn) = conn.as_deref() {
        match token.sub.as_deref() {
            Some(sub) => {
                let revoked = match revoke_scope.as_deref() {
                    // Per-app client matched (Slice 1d §1.2): revoke ONLY this
                    // app's sessions for the subject. We do NOT push the subject
                    // into the global wrapper denylist — that is a cross-app
                    // nuke; a per-app BCL must not log the user out of other
                    // apps.
                    Some(app_name) => sessions::revoke_app_sessions_for_user(conn, app_name, sub)
                        .await
                        .unwrap_or_else(|e| {
                            tracing::error!(
                                error = %e,
                                app_id = %app_name,
                                "backchannel_logout: revoke_app_sessions_for_user failed"
                            );
                            0
                        }),
                    // Shared `gateway` client (legacy path): revoke across the
                    // subject's gateway sessions and push the subject into the
                    // wrapper denylist (the original all-apps semantics).
                    None => {
                        if let Some(subject) = zeroship_core::wrapper_revocation::subject_uuid(sub)
                        {
                            if let Err(e) = zeroship_core::wrapper_revocation::revoke_subject(
                                conn, subject,
                            )
                            .await
                            {
                                tracing::error!(
                                    error = %e,
                                    sub = %sub,
                                    "backchannel_logout: wrapper subject revoke failed"
                                );
                            }
                        } else {
                            tracing::warn!(
                                sub = %sub,
                                "backchannel_logout: non-UUID sub cannot enter wrapper denylist"
                            );
                        }
                        sessions::revoke_all_for_user(conn, sub).await.unwrap_or_else(|e| {
                            tracing::error!(
                                error = %e,
                                "backchannel_logout: revoke_all_for_user failed"
                            );
                            0
                        })
                    }
                };
                tracing::info!(
                    sub = %sub,
                    sid = ?token.sid,
                    app = ?revoke_scope,
                    revoked,
                    "backchannel_logout: sessions revoked"
                );
                emit_revocation_audit(conn, &aud, sub, token.sid.as_deref(), &token.jti, revoked)
                    .await;
            }
            None => {
                // sid-only path. Token verified fine but we can't map
                // it to a session row. Phase 7 deferred per-sid
                // revocation; surface a debug log so the operator
                // sees the gap.
                tracing::warn!(
                    sid = ?token.sid,
                    "backchannel_logout: sid-only token; no session correlation available, no rows revoked"
                );
            }
        }
    } else {
        // The `--db ""` smoke-mode case; nothing to revoke. Still
        // return 200 because the verifier did its job and there's
        // nothing for the operator to retry.
        tracing::warn!(
            "backchannel_logout: gateway has no DB configured; verify succeeded but no sessions to revoke"
        );
    }

    HttpResponse::Ok()
        .header("cache-control", "no-store")
        .finish()
}

/// Mount the route onto an ntex `App` factory. Registered at
/// `POST /oidc/backchannel-logout`. The route is unconditionally
/// available (not gated on anything) — hydra's webhook surface needs
/// a stable URL.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/oidc/backchannel-logout").route(web::post().to(handle)),
    );
}

fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

async fn emit_revocation_audit(
    db: &compio_postgres::Client,
    client_id: &str,
    sub: &str,
    sid: Option<&str>,
    jti: &str,
    revoked: u64,
) {
    let detail = json!({
        "surface": "gateway",
        "sub": sub,
        "sid": sid,
        "jti": jti,
        "revoked": revoked,
    });
    if let Err(e) = db
        .execute(
            "INSERT INTO auth.audit_events \
                (event_type, outcome, client_id, auth_method, detail) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                &"backchannel_logout_revoke",
                &"success",
                &client_id,
                &"oidc_backchannel_logout",
                &detail,
            ],
        )
        .await
    {
        tracing::warn!(error = %e, "backchannel_logout: audit insert failed");
    }
}
