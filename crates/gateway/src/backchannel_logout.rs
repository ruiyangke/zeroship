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
    let (aud, revoke_scope, revoke_sector): (
        String,
        Option<uuid::Uuid>,
        Option<String>,
    ) = aud_candidates
        .iter()
        .find_map(|cand| {
            // The per-app session/anchor rows are keyed by the app's STABLE
            // UUID (`apps.id`), not the renameable subdomain slug — so the
            // per-app revoke scope carries the app id, never `route.entry.name`.
            state.routes.lookup_by_oauth_client_id(cand).map(|(id, route)| {
                (cand.clone(), Some(id), route.entry.sector_identifier.clone())
            })
        })
        .unwrap_or_else(|| (state.oidc_rp.client_id.clone(), None, None));

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
    let mut conn = match pool.as_ref() {
        Some(pool) => match pool.get().await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::error!(error = %e, "backchannel_logout: pg pool checkout failed");
                None
            }
        },
        None => None,
    };
    // M1 fix: the anchor refresh families deleted inside the DB block, to be
    // revoked at Hydra AFTER the connection is released (no conn held across the
    // outbound HTTP). `anchor_user` is the parsed `sub` UUID used to rebuild the
    // per-family AEAD AAD for the decrypt.
    let mut anchor_families: Vec<crate::anchors::DeletedFamily> = Vec::new();
    let mut anchor_user: Option<uuid::Uuid> = None;
    // `&mut Client`: the RLS-scoped `revoke_app_sessions_for_user` needs it (it
    // opens a tenant-GUC transaction). The non-RLS `wrapper_revocation` +
    // audit-insert calls below reborrow it immutably; every touch is sequential
    // (no overlapping borrow) and no outbound HTTP runs between them.
    if let Some(conn) = conn.as_deref_mut() {
        match token.sub.as_deref() {
            Some(sub) => {
                let revoked = match revoke_scope {
                    // Per-app client matched (Slice 1d §1.2): revoke ONLY this
                    // app's sessions for the subject. We do NOT push the subject
                    // into the global wrapper denylist — that is a cross-app
                    // nuke; a per-app BCL must not log the user out of other
                    // apps. `app_id` is the app's STABLE UUID — the canonical
                    // session/anchor key.
                    Some(app_id) => {
                        // PER-APP token-family marker (Batch A fix 4): write
                        // `zeroship.token_revocations` on the SAME `(client_id,
                        // pws_)` key the gateway arms read, so the user's live
                        // wrapper / raw-Hydra access token for THIS app dies on
                        // the next request — not just their gateway sessions.
                        // `aud` is the per-app `oac_…` client; `pws_` is
                        // `derive_pairwise(sub, sector)` under the app's apex
                        // sector. Best-effort: a marker write failure must not
                        // block the session revoke. Skipped (with a warn) when
                        // the route carries no sector yet.
                        if let Some(sector) = revoke_sector.as_deref() {
                            let pws = zeroship_core::auth::derive_pairwise(
                                &state.pairwise_salt,
                                sub,
                                sector,
                            );
                            if let Err(e) = zeroship_core::wrapper_revocation::revoke_family(
                                &*conn, &aud, &pws,
                            )
                            .await
                            {
                                tracing::error!(
                                    error = %e,
                                    client_id = %aud,
                                    "backchannel_logout: per-app token-family marker write failed"
                                );
                            }
                            // SAME-NODE write-side bust (R1d): a BCL landing on
                            // THIS node busts the local cache entry for
                            // `(aud, pws)` so the just-written marker is seen
                            // immediately here; sibling nodes rely on the TTL
                            // backstop.
                            state.revocation_cache.invalidate(&aud, &pws);
                        } else {
                            tracing::warn!(
                                app_id = %app_id,
                                "backchannel_logout: per-app BCL has no sector_identifier; \
                                 skipping token-family marker (sessions still revoked)"
                            );
                        }
                        // M1 fix: the per-app BCL must ALSO durably terminate the
                        // 30-day SDK reload-recovery anchor for this `(app_id,
                        // user)` — otherwise `GET /__zeroship/auth/session?mint=1`
                        // reads the surviving anchor, runs a Hydra refresh, and
                        // re-mints a cookie whose `iat` post-dates the family
                        // marker, silently resurrecting the session the logout
                        // killed. Delete EVERY anchor row for the user at this app
                        // ("this app, every device" — the same teardown `/signout
                        // {scope:'global'}` does) and stash the removed Hydra
                        // refresh families for the best-effort Hydra revoke AFTER
                        // the DB block (no conn is held across the outbound HTTP —
                        // the round-6 BLOCKER invariant this file documents).
                        //
                        // `sub` is the global user UUID; parse it once. A non-UUID
                        // sub (never a real end-user) skips the anchor teardown
                        // (there is no anchor keyed by it). The anchor delete is the
                        // authoritative step; the Hydra revoke is defense-in-depth.
                        match uuid::Uuid::parse_str(sub) {
                            Ok(global_user_id) => {
                                match crate::anchors::delete_all_for_user(
                                    &mut *conn, app_id, global_user_id,
                                )
                                .await
                                {
                                    Ok(deleted) => {
                                        anchor_families = deleted;
                                        anchor_user = Some(global_user_id);
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            error = %e,
                                            app_id = %app_id,
                                            "backchannel_logout: anchor delete_all_for_user failed"
                                        );
                                    }
                                }
                            }
                            Err(_) => {
                                tracing::warn!(
                                    app_id = %app_id,
                                    "backchannel_logout: sub is not a UUID; skipping anchor teardown"
                                );
                            }
                        }

                        sessions::revoke_app_sessions_for_user(&mut *conn, app_id, sub)
                            .await
                            .unwrap_or_else(|e| {
                                tracing::error!(
                                    error = %e,
                                    app_id = %app_id,
                                    "backchannel_logout: revoke_app_sessions_for_user failed"
                                );
                                0
                            })
                    }
                    // No per-app client matched (the logout_token's aud is the
                    // shared `gateway` client, not a per-app `oac_…`). Under RLS
                    // (changeset 0025) the gateway connects as the non-bypass
                    // `zeroship_gateway` role, so a single cross-tenant
                    // `WHERE user_id=$1` over EVERY app is not expressible — and
                    // the former `revoke_all_for_user` fan-out has been removed.
                    // Every real BCL now arrives under a per-app client (each
                    // per-app OAuth client registers its OWN
                    // `backchannel_logout_uri` with its own `aud`), so this
                    // branch is a logged no-op rather than a cross-tenant nuke.
                    None => {
                        tracing::warn!(
                            sub = %sub,
                            aud = %aud,
                            "backchannel_logout: logout_token aud is not a per-app client; \
                             no per-app scope to revoke under RLS (no rows touched)"
                        );
                        0
                    }
                };
                tracing::info!(
                    sub = %sub,
                    sid = ?token.sid,
                    app = ?revoke_scope,
                    revoked,
                    "backchannel_logout: sessions revoked"
                );
                emit_revocation_audit(&*conn, &aud, sub, token.sid.as_deref(), &token.jti, revoked)
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

    // Release the pooled connection BEFORE the outbound Hydra revoke (the
    // round-6 BLOCKER invariant: no DB conn is ever held across outbound HTTP).
    drop(conn);
    drop(pool);

    // M1 fix (best-effort, defense-in-depth): revoke each anchor refresh family
    // deleted above at Hydra so the rotating refresh grant is killed at the
    // source, not just locally. The anchor rows are already gone (the
    // authoritative step); a Hydra hiccup here is logged, never surfaced.
    if let Some(global_user_id) = anchor_user {
        let sub_str = global_user_id.to_string();
        for fam in &anchor_families {
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
