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
//! - 200 + `cache-control: no-store` on success or already-processed replay
//! - 400 + `cache-control: no-store` on any verify failure (with a
//!   short text body; hydra logs the body so it shows up in the auth
//!   server's debug surface)
//! - 5xx + `cache-control: no-store` on a retryable local processing failure
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
/// Revocation policy: for per-app clients, prefer `sid` and revoke only local
/// sessions that originated from that OP session. If the logout token lacks
/// `sid`, fall back to revoking all sessions for the token's `sub` at that app.
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
    // revoke only THAT app's sessions.
    //
    // `revoke_scope` carries the app's STABLE UUID (`apps.id`) when a per-app
    // client matched (revoke only that app — the canonical session/anchor
    // key), or `None` for the shared-client all-apps path.
    // `revoke_sector` carries that app's `sector_identifier` so the per-app
    // branch can derive the same `pws_…` the gateway projects, to write the
    // PER-APP token-family marker (Batch A fix 4) — a per-app BCL must kill the
    // user's live wrapper / raw-Hydra access token for THAT app, not just its
    // gateway sessions. `None` sector ⇒ the marker write is skipped (no live
    // wrapper to revoke without a sector).
    let aud_candidates =
        zeroship_core::logout_token::unverified_aud_candidates(&form.logout_token);
    let Some((aud, revoke_scope, revoke_sector)) = aud_candidates
        .iter()
        .find_map(|cand| {
            // The per-app session/anchor rows are keyed by the app's STABLE
            // UUID (`apps.id`), not the renameable subdomain slug — so the
            // per-app revoke scope carries the app id, never `route.entry.name`.
            state.routes.lookup_by_oauth_client_id(cand).map(|(id, route)| {
                (cand.clone(), Some(id), route.entry.sector_identifier.clone())
            })
        }) else {
            tracing::warn!(
                aud = ?aud_candidates,
                "backchannel_logout: no provisioned per-app client matched logout_token audience"
            );
            return HttpResponse::BadRequest()
                .header("cache-control", "no-store")
                .body("invalid logout_token");
        };

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
    if state.logout_jti_cache.contains(&token.jti, now_secs) {
        tracing::warn!(
            jti = %token.jti,
            sub = ?token.sub,
            sid = ?token.sid,
            "backchannel_logout: replayed logout_token jti; already processed"
        );
        return HttpResponse::Ok()
            .header("cache-control", "no-store")
            .finish();
    }

    // Revoke. The verifier guarantees at least one of sub/sid is present.
    // Prefer sid when present; sub is the all-sessions fallback.
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
                return retryable_processing_error();
            }
        },
        None => {
            tracing::error!("backchannel_logout: gateway has no DB configured");
            return retryable_processing_error();
        }
    };
    let mut conn = match pool.as_ref() {
        Some(pool) => match pool.get().await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::error!(error = %e, "backchannel_logout: pg pool checkout failed");
                return retryable_processing_error();
            }
        },
        None => None,
    };
    // M1 fix: the anchor refresh families deleted inside the DB block, to be
    // revoked at Hydra AFTER the connection is released (no conn held across the
    // outbound HTTP). Each family is paired with its global user id so we can
    // rebuild the per-family AEAD AAD for the decrypt.
    let mut anchor_families: Vec<(uuid::Uuid, crate::anchors::DeletedFamily)> = Vec::new();
    // `&mut Client`: the RLS-scoped `revoke_app_sessions_for_user` needs it (it
    // opens a tenant-GUC transaction). The non-RLS `wrapper_revocation` +
    // audit-insert calls below reborrow it immutably; every touch is sequential
    // (no overlapping borrow) and no outbound HTTP runs between them.
    if let Some(conn) = conn.as_deref_mut() {
        let Some(app_id) = revoke_scope else {
            tracing::warn!(
                aud = %aud,
                "backchannel_logout: logout_token aud is not a per-app client; \
                 no per-app scope to revoke under RLS (no rows touched)"
            );
            emit_revocation_audit(
                &*conn,
                &aud,
                token.sub.as_deref(),
                token.sid.as_deref(),
                &token.jti,
                0,
            )
            .await;
            return HttpResponse::Ok()
                .header("cache-control", "no-store")
                .finish();
        };

        let mut revoked: u64 = 0;
        if let Some(sid) = token.sid.as_deref() {
            let users = match sessions::revoke_app_sessions_for_sid(
                &mut *conn,
                app_id,
                sid,
                token.sub.as_deref(),
            )
            .await
            {
                Ok(users) => users,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        app_id = %app_id,
                        sid = %sid,
                        "backchannel_logout: revoke_app_sessions_for_sid failed"
                    );
                    return retryable_processing_error();
                }
            };
            revoked = users.len() as u64;
            if users.is_empty() {
                if let Some(sub) = token.sub.as_deref() {
                    tracing::warn!(
                        app_id = %app_id,
                        sid = %sid,
                        sub = %sub,
                        "backchannel_logout: sid matched zero sessions; falling back to app-scoped sub revoke"
                    );
                    revoked = match revoke_by_sub(
                        &mut *conn,
                        &state,
                        &aud,
                        app_id,
                        revoke_sector.as_deref(),
                        sub,
                        &mut anchor_families,
                    )
                    .await
                    {
                        Ok(revoked) => revoked,
                        Err(e) => {
                            tracing::error!(
                                error = %e,
                                app_id = %app_id,
                                sid = %sid,
                                "backchannel_logout: sub fallback revocation failed"
                            );
                            return retryable_processing_error();
                        }
                    };
                }
            }
            for global_user_id in users {
                if let Err(e) = teardown_per_app_user(
                    &mut *conn,
                    &state,
                    &aud,
                    app_id,
                    revoke_sector.as_deref(),
                    global_user_id,
                    &mut anchor_families,
                )
                .await
                {
                    tracing::error!(
                        error = %e,
                        app_id = %app_id,
                        sid = %sid,
                        "backchannel_logout: sid-scoped teardown failed"
                    );
                    return retryable_processing_error();
                }
            }
        } else if let Some(sub) = token.sub.as_deref() {
            revoked = match revoke_by_sub(
                &mut *conn,
                &state,
                &aud,
                app_id,
                revoke_sector.as_deref(),
                sub,
                &mut anchor_families,
            )
            .await
            {
                Ok(revoked) => revoked,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        app_id = %app_id,
                        "backchannel_logout: revoke_app_sessions_for_user failed"
                    );
                    return retryable_processing_error();
                }
            };
        }

        tracing::info!(
            sub = ?token.sub,
            sid = ?token.sid,
            app = %app_id,
            revoked,
            "backchannel_logout: sessions revoked"
        );
        emit_revocation_audit(
            &*conn,
            &aud,
            token.sub.as_deref(),
            token.sid.as_deref(),
            &token.jti,
            revoked,
        )
        .await;
    } else {
        return retryable_processing_error();
    }

    if !state.logout_jti_cache.insert(
        &token.jti,
        now_secs,
        zeroship_core::logout_token::LOGOUT_JTI_TTL_SECS,
    ) {
        tracing::warn!(
            jti = %token.jti,
            sub = ?token.sub,
            sid = ?token.sid,
            "backchannel_logout: logout_token jti was processed concurrently"
        );
        return HttpResponse::Ok()
            .header("cache-control", "no-store")
            .finish();
    }

    // Release the pooled connection BEFORE the outbound Hydra revoke (the
    // round-6 BLOCKER invariant: no DB conn is ever held across outbound HTTP).
    drop(conn);
    drop(pool);

    // M1 fix (best-effort, defense-in-depth): revoke each anchor refresh family
    // deleted above at Hydra so the rotating refresh grant is killed at the
    // source, not just locally. The anchor rows are already gone (the
    // authoritative step); a Hydra hiccup here is logged, never surfaced.
    for (global_user_id, fam) in &anchor_families {
        let sub_str = global_user_id.to_string();
        let aad = anchor_aad(&fam.client_id, &sub_str);
        let refresh = match zeroship_core::crypto::decrypt(
            &state.anchor_enc_key,
            &aad,
            &fam.refresh_token_enc,
        ) {
            Ok(pt) => String::from_utf8_lossy(&pt).into_owned(),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "backchannel_logout: anchor refresh decrypt failed (skip Hydra revoke)"
                );
                continue;
            }
        };
        if let Err(e) = state.oidc_rp.revoke_token_public(&fam.client_id, &refresh).await {
            // RFC 7009 §2.2: best-effort — the anchor row is already gone.
            tracing::warn!(
                error = %e,
                "backchannel_logout: Hydra refresh revoke best-effort failure"
            );
        }
    }

    HttpResponse::Ok()
        .header("cache-control", "no-store")
        .finish()
}

/// AEAD additional-authenticated-data for an anchor's server-held refresh
/// family ciphertext. MUST match the format `browser_auth.rs` uses when it
/// encrypts (`zs-anchor-refresh:{client_id}:{sub}`), or the decrypt fails.
fn anchor_aad(client_id: &str, sub: &str) -> Vec<u8> {
    format!("zs-anchor-refresh:{client_id}:{sub}").into_bytes()
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

fn retryable_processing_error() -> HttpResponse {
    HttpResponse::ServiceUnavailable()
        .header("cache-control", "no-store")
        .body("temporary logout processing failure")
}

async fn revoke_by_sub(
    conn: &mut compio_postgres::Client,
    state: &GateState,
    client_id: &str,
    app_id: uuid::Uuid,
    sector: Option<&str>,
    sub: &str,
    anchor_families: &mut Vec<(uuid::Uuid, crate::anchors::DeletedFamily)>,
) -> Result<u64, crate::error::GatewayError> {
    let global_user_id = match uuid::Uuid::parse_str(sub) {
        Ok(id) => Some(id),
        Err(_) => {
            tracing::warn!(
                app_id = %app_id,
                sub = %sub,
                "backchannel_logout: sub is not a UUID; skipping per-app teardown"
            );
            None
        }
    };
    if let Some(global_user_id) = global_user_id {
        teardown_per_app_user(
            conn,
            state,
            client_id,
            app_id,
            sector,
            global_user_id,
            anchor_families,
        )
        .await?;
    }
    sessions::revoke_app_sessions_for_user(conn, app_id, sub).await
}

async fn teardown_per_app_user(
    conn: &mut compio_postgres::Client,
    state: &GateState,
    client_id: &str,
    app_id: uuid::Uuid,
    sector: Option<&str>,
    global_user_id: uuid::Uuid,
    anchor_families: &mut Vec<(uuid::Uuid, crate::anchors::DeletedFamily)>,
) -> Result<(), crate::error::GatewayError> {
    let global_sub = global_user_id.to_string();
    if let Some(sector) = sector {
        let pws = zeroship_core::auth::derive_pairwise(&state.pairwise_salt, &global_sub, sector);
        if let Err(e) = zeroship_core::wrapper_revocation::revoke_family(conn, client_id, &pws).await
        {
            tracing::error!(
                error = %e,
                client_id = %client_id,
                "backchannel_logout: per-app token-family marker write failed"
            );
            return Err(crate::error::GatewayError::Db(format!(
                "backchannel_logout token-family marker: {e}"
            )));
        }
        state.revocation_cache.invalidate(client_id, &pws);
    } else {
        tracing::warn!(
            app_id = %app_id,
            "backchannel_logout: per-app BCL has no sector_identifier; \
             skipping token-family marker (sessions still revoked)"
        );
    }

    match crate::anchors::delete_all_for_user(conn, app_id, global_user_id).await {
        Ok(deleted) => {
            anchor_families.extend(deleted.into_iter().map(|fam| (global_user_id, fam)));
        }
        Err(e) => {
            tracing::error!(
                error = %e,
                app_id = %app_id,
                "backchannel_logout: anchor delete_all_for_user failed"
            );
            return Err(e);
        }
    }
    Ok(())
}

async fn emit_revocation_audit(
    db: &compio_postgres::Client,
    client_id: &str,
    sub: Option<&str>,
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
            "INSERT INTO zeroship.audit_events \
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
