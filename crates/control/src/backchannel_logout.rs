//! `POST /oidc/backchannel-logout` — OIDC BCL 1.0 RP endpoint for the
//! control plane.
//!
//! Hydra POSTs a signed `logout_token` JWT here when a user signs out
//! via `/oauth2/sessions/logout`. The control plane verifies the token
//! (see [`zeroship_core::logout_token`]) and revokes the user's console
//! session(s) on `console.zeroship.ai`.
//!
//! Mirror of `crates/gateway/src/backchannel_logout.rs` (Phase 7 U1.2)
//! scoped to the console origin: same verifier + revoke-by-sub
//! semantics, against the `auth.console_sessions` table instead of
//! `auth.gateway_sessions`. The URI is the stable
//! `https://console.zeroship.ai/oidc/backchannel-logout` registered on
//! the `console.zeroship.ai` client in
//! `ops/auth-clients.example.toml`.
//!
//! Response contract:
//! - 200 + `cache-control: no-store` on success
//! - 400 + `cache-control: no-store` on any verify failure (with a
//!   short text body; hydra logs the body so it shows up in the auth
//!   server's debug surface)
//!
//! Phase 7 U2.

use std::sync::Arc;

use ntex::web::{self, types::Form, types::State, HttpResponse};
use serde::Deserialize;

use crate::console_sessions;
use crate::AppState;

/// Form body shape per OIDC BCL §2.5 — a single `logout_token` field,
/// `x-www-form-urlencoded`. ntex's `Form<T>` parses that automatically.
#[derive(Debug, Deserialize)]
pub struct LogoutForm {
    pub logout_token: String,
}

/// Verify the inbound `logout_token` and revoke the affected console
/// sessions.
///
/// Revocation policy (Phase 7 minimum): revoke ALL console sessions
/// for the `sub`. There is only one console origin, so this maps to
/// every row in `auth.console_sessions` where `user_id = sub`. The
/// spec also supports a `sid`-only token but we don't currently
/// correlate hydra's session id to our `console_sessions.id`, so even
/// when `sid` is present we fall back to revoke-by-`sub`. If only
/// `sid` is set and `sub` is absent, the token is still verified but
/// no session rows are touched — log it for debug visibility.
#[allow(clippy::future_not_send)]
pub async fn handle(
    form: Form<LogoutForm>,
    state: State<Arc<AppState>>,
) -> HttpResponse {
    // The expected ID-token `iss` claim. `ConsoleOidcRp` derives this
    // inline in `finish_callback` from `auth_public` with a trailing
    // slash — match the same shape here so the verifier accepts the
    // tokens hydra actually emits per its `urls.self.issuer`.
    let issuer = format!(
        "{}/",
        state.oidc_rp.auth_public.trim_end_matches('/')
    );
    let aud = state.oidc_rp.client_id.clone(); // "console.zeroship.ai"

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
            tracing::warn!(error = %e, "console backchannel_logout: verify failed");
            return HttpResponse::BadRequest()
                .header("cache-control", "no-store")
                .body("invalid logout_token");
        }
    };

    // Revoke. The verifier guarantees at least one of sub/sid is
    // present, but only sub maps to a row filter today (the
    // `console_sessions.user_id` column).
    match token.sub.as_deref() {
        Some(sub) => {
            let revoked = console_sessions::revoke_all_for_user(&state.auth_pg, sub)
                .await
                .unwrap_or_else(|e| {
                    tracing::error!(error = %e, "console revoke_all_for_user failed");
                    0
                });
            tracing::info!(
                sub = %sub,
                sid = ?token.sid,
                revoked,
                "console backchannel_logout: sessions revoked"
            );
        }
        None => {
            // sid-only path. Token verified fine but we can't map
            // it to a session row. Phase 7 deferred per-sid
            // revocation; surface a warn log so the operator sees
            // the gap.
            tracing::warn!(
                sid = ?token.sid,
                "console backchannel_logout: sid-only token; no session correlation available, no rows revoked"
            );
        }
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
