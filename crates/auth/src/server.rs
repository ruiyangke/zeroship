//! ntex routes wiring. Bootstrap of routes/handlers happens here; the
//! handlers themselves live in `src/ui/`, `src/identity/`, etc.

use std::sync::Arc;

use ntex::web;
use zeroship_core::oidc_verify::JwksCache;

use crate::config::AuthConfig;
use crate::headers::{RequestContextMiddleware, SecurityHeaders};
use crate::hydra_client::HydraAdmin;
use crate::ui;
use zeroship_mailer::{Mailer, RelayForwardMailer};

/// Bundled stylesheet served at `/static/style.css`. Compiled into the
/// binary at build time so the runtime has no filesystem dependency.
const STATIC_CSS: &str = include_str!("../static/style.css");

/// Register every route the auth server exposes.
///
/// State (`Arc<HydraAdmin>`-equivalent, `Arc<AuthConfig>`, `Arc<Client>`,
/// and optionally `Arc<JwksCache>` for Google) is registered on the `App`
/// in [`run`]; this function only wires URL paths to handlers.
///
/// `google_enabled` gates the `/oauth/google/*` routes — when Google
/// `OAuth` credentials are not configured we don't register dead routes
/// that would return runtime "missing `JwksCache` state" errors.
/// `github_enabled` gates `/oauth/github/*` the same way; GitHub has no
/// `JwksCache` (it's OAuth 2.0, not OIDC), but the predicate keeps the
/// route table small and makes "no upstream creds → no upstream route"
/// uniform across providers.
pub fn configure(
    google_enabled: bool,
    github_enabled: bool,
) -> impl Fn(&mut web::ServiceConfig) {
    move |cfg: &mut web::ServiceConfig| {
        cfg.service(healthz)
            .service(readyz)
            .service(style)
            .service(
                web::resource("/login")
                    .route(web::get().to(ui::login::get))
                    .route(web::post().to(ui::login::post)),
            )
            // ISS-11: TOTP second-factor step. Reached only after `/login` POST
            // verified the password for a 2FA-enabled user and set the signed
            // `__Host-zsidp_2fa` challenge cookie. POST-only (the challenge form
            // is rendered by the `/login` POST response, not a GET).
            .service(
                web::resource("/login/2fa")
                    .route(web::post().to(ui::login::post_2fa)),
            )
            .service(
                web::resource("/signup")
                    .route(web::get().to(ui::signup::get))
                    .route(web::post().to(ui::signup::post)),
            )
            .service(
                web::resource("/consent")
                    .route(web::get().to(ui::consent::get_consent)),
            )
            .service(
                web::resource("/consent/accept")
                    .route(web::post().to(ui::consent::post_consent_accept)),
            )
            .service(
                web::resource("/consent/deny")
                    .route(web::post().to(ui::consent::post_consent_deny)),
            )
            .service(
                web::resource("/device")
                    .route(web::get().to(ui::device::get))
                    .route(web::post().to(ui::device::post)),
            )
            // RP-initiated logout (OIDC Session Management §5). hydra's
            // `urls.logout` config points here; the RP redirects to
            // hydra's `end_session_endpoint`, hydra issues a
            // `logout_challenge` and 302s to this route.
            .service(
                web::resource("/logout")
                    .route(web::get().to(ui::logout::get))
                    .route(web::post().to(ui::logout::post)),
            )
            // `/link` is always registered — it's hit only via a pending
            // token issued by the federation callbacks, so a route that
            // exists without configured providers harms nothing and lets
            // sub-commit-1 unit tests exercise the GET/POST without
            // requiring Google/GitHub creds.
            .service(
                web::resource("/link")
                    .route(web::get().to(ui::link::get))
                    .route(web::post().to(ui::link::post)),
            )
            // `/me` is the signed-in user's profile page; `/me/unlink/<provider>`
            // is the POST target for the per-identity Unlink form. Both
            // require an `__Host-zsidp_session` cookie — the handlers
            // themselves do the validation (no middleware gate yet).
            .service(web::resource("/me").route(web::get().to(ui::me::get)))
            .service(
                web::resource("/me/unlink/{provider}")
                    .route(web::post().to(ui::me::unlink)),
            )
            // ISS-10: active-session visibility + single-session revoke. Same
            // authenticated `__Host-zsidp_session` gate as `/me` (the handlers
            // resolve the caller from the cookie). `GET /me/sessions` lists the
            // caller's active sessions across both session tables;
            // `POST /me/sessions/{id}/revoke` revokes ONE of the caller's own
            // sessions (scoped `user_id = caller` — the IDOR guard).
            .service(
                web::resource("/me/sessions")
                    .route(web::get().to(ui::sessions::list)),
            )
            .service(
                web::resource("/me/sessions/{id}/revoke")
                    .route(web::post().to(ui::sessions::revoke)),
            )
            // ISS-12: account deletion (GDPR Art. 17). `/me/delete` begins the
            // request (soft-disable + 30-day schedule + confirm email);
            // `/me/delete/cancel` reverses it within the grace window. The
            // irreversible erasure runs later in `cron::account_reaper`.
            .service(
                web::resource("/me/delete")
                    .route(web::post().to(ui::account_deletion::request)),
            )
            .service(
                web::resource("/me/delete/cancel")
                    .route(web::post().to(ui::account_deletion::cancel)),
            )
            // ISS-11: TOTP two-factor self-service. Same authenticated
            // `__Host-zsidp_session` + CSRF gate as the rest of `/me` (the
            // handlers resolve the caller from the cookie). `enroll` mints a
            // pending secret + provisioning URI; `confirm` verifies the first
            // code, activates 2FA, and returns one-time backup codes; `disable`
            // removes the credential after a code/password re-auth.
            .service(
                web::resource("/me/2fa/enroll")
                    .route(web::post().to(ui::totp::enroll)),
            )
            .service(
                web::resource("/me/2fa/confirm")
                    .route(web::post().to(ui::totp::confirm)),
            )
            .service(
                web::resource("/me/2fa/disable")
                    .route(web::post().to(ui::totp::disable)),
            )
            // Magic-link login (P5-U4). Universal — always registered,
            // no per-provider gating.
            .service(
                web::resource("/magic/start").route(web::post().to(ui::magic::start)),
            )
            .service(
                web::resource("/magic/await").route(web::get().to(ui::magic::await_code)),
            )
            .service(
                web::resource("/magic/verify").route(web::get().to(ui::magic::verify)),
            )
            .service(
                web::resource("/magic/verify/redeem")
                    .route(web::post().to(ui::magic::verify_redeem)),
            )
            .service(
                web::resource("/magic/complete")
                    .route(web::post().to(ui::magic::complete)),
            )
            // Email verification (P5-U5). Token issued at /signup lands on
            // GET /verify, then POST /verify/redeem consumes it and sets
            // zeroship.users.email_verified_at = NOW().
            .service(web::resource("/verify").route(web::get().to(ui::verify::get)))
            .service(
                web::resource("/verify/redeem").route(web::post().to(ui::verify::post_redeem)),
            )
            // Password reset (P5-U6). /forgot issues a 1h reset token
            // (enumeration-resistant); /reset redeems it and updates
            // zeroship.users.password_hash.
            .service(
                web::resource("/forgot")
                    .route(web::get().to(ui::forgot::get))
                    .route(web::post().to(ui::forgot::post)),
            )
            .service(
                web::resource("/reset")
                    .route(web::get().to(ui::reset::get))
                    .route(web::post().to(ui::reset::post)),
            )
            // Postmark bounce/complaint webhook (P5-U7). Always
            // registered — the handler 401s when
            // `postmark_webhook_user`/`postmark_webhook_password`
            // aren't configured so misrouted traffic doesn't silently
            // succeed in dev.
            .service(
                web::resource("/webhooks/postmark")
                    .route(web::post().to(ui::webhooks::postmark)),
            )
            // SES-SNS bounce/complaint webhook (P6-U3). Always
            // registered — auth is by RSA-SHA1 signature against the
            // SigningCertURL cert (anti-SSRF allowlist enforced), so
            // there's no environment-level on/off switch.
            .service(
                web::resource("/webhooks/ses-sns")
                    .route(web::post().to(ui::webhooks::ses_sns)),
            )
            // GoTrue Send Email hook. Server-to-server Standard-Webhooks HMAC
            // auth, so this route is deliberately outside the browser CSRF
            // form flow. When GoTrue enables this hook it fully bypasses its
            // own SMTP path; the handler sends through zeroship_mailer.
            .service(
                web::resource("/hooks/gotrue/send-email")
                    .route(web::post().to(ui::gotrue_email_hook::send_email)),
            )
            // Relay inbound webhook (Slice 5b). The Postmark Inbound server
            // POSTs parsed-JSON mail sent to `{alias}@{relay_domain}`; the
            // handler resolves the alias → real inbox and re-originates the
            // message from the relay identity. Always registered — the handler
            // 401s when `relay_inbound_user`/`relay_inbound_password` aren't
            // configured so an unverified POST can't drive a forward/suppression
            // spoof (sub-spec §4.3 step 1).
            .service(
                web::resource("/webhooks/relay-inbound")
                    // Bound the pre-auth raw-body buffer (the handler reads the
                    // payload as `Bytes` and parses JSON only AFTER the Basic-auth
                    // gate — sub-spec §4.3 step 1). 5 MiB is generous for a parsed
                    // inbound message (headers + text/html bodies) yet caps the
                    // cost an unauthenticated POST can impose. Scoped to this
                    // resource so other handlers keep ntex's 256 KiB default.
                    .state(web::types::PayloadConfig::new(5 * 1024 * 1024))
                    .route(web::post().to(ui::webhooks::relay_inbound)),
            );

        if google_enabled {
            cfg.service(
                web::resource("/oauth/google/start")
                    .route(web::get().to(ui::oauth_google::start)),
            )
            .service(
                web::resource("/oauth/google/callback")
                    .route(web::get().to(ui::oauth_google::callback)),
            );
        }

        if github_enabled {
            cfg.service(
                web::resource("/oauth/github/start")
                    .route(web::get().to(ui::oauth_github::start)),
            )
            .service(
                web::resource("/oauth/github/callback")
                    .route(web::get().to(ui::oauth_github::callback)),
            );
        }
    }
}

#[web::get("/healthz")]
async fn healthz() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({ "ok": true }))
}

#[web::get("/readyz")]
async fn readyz() -> web::HttpResponse {
    // Phase 1 readiness is process-up. Phase 1 Task 17 wires PG + hydra reachability.
    web::HttpResponse::Ok().json(&serde_json::json!({ "ready": true }))
}

#[web::get("/static/style.css")]
async fn style() -> web::HttpResponse {
    let mut r = web::HttpResponse::Ok();
    r.content_type("text/css; charset=utf-8");
    r.body(STATIC_CSS)
}

/// Bind and run the ntex HTTP server.
///
/// Threads shared-state slots through ntex's `App::state`:
///
/// - `HydraAdmin` — hydra admin API client (cheap to clone; holds an
///   internal `cyper::Client`).
/// - `Arc<AuthConfig>` — the parsed config; used by handlers for the
///   `insecure_dev` cookie flag and hydra URLs.
/// - `Arc<compio_postgres::Client>` — the PG client; `Client` is not
///   itself `Clone`, so it must be wrapped before being shared across
///   worker tasks.
/// - `Arc<JwksCache>` — Google's JWKS cache, ONLY registered when
///   Google `OAuth` is enabled. The `/oauth/google/*` routes are gated on
///   the same predicate, so handlers always find this state present at
///   request time.
/// - `Arc<dyn Mailer>` — outbound transactional mailer (stdout / SMTP /
///   Resend). Selected by `--mailer` in [`crate::main`]; threaded
///   uniformly so handlers can always extract `State<Arc<dyn Mailer>>`.
/// - `RelayForwardMailer` — the SECOND, dedicated relay-forward mailer
///   (sub-spec §5.2a). Built from `--relay-forward-mailer` (SMTP/stdout,
///   never Resend) with its own `AUTH_RELAY_SMTP_*` identity. A newtype so
///   `State<RelayForwardMailer>` is distinct from the transactional state.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if binding fails or the
/// server loop exits with an error.
//
// ntex's per-thread server future is intentionally `!Send` (it holds
// per-worker state in `Rc`s). Marking `run` `!Send` is a structural
// property of `ntex::web::server`, not an actionable defect.
#[allow(clippy::future_not_send)]
pub async fn run(
    cfg: Arc<AuthConfig>,
    admin: HydraAdmin,
    db: Arc<compio_postgres::Client>,
    google_jwks: Option<Arc<JwksCache>>,
    mailer: Arc<dyn Mailer>,
    relay_forward_mailer: RelayForwardMailer,
) -> std::io::Result<()> {
    let addr = cfg.addr.clone();
    let google_enabled = google_jwks.is_some();
    let github_enabled = cfg.github_client_id.is_some();
    // Console origin(s) the framed login routes admit via `frame-ancestors`
    // (immersive iframe login, §4.3). Cloned out of the config so the
    // route-aware `SecurityHeaders` middleware can be rebuilt per worker thread.
    let frame_ancestor_origins = cfg.frame_ancestor_origins.clone();

    web::server(async move || {
        let mut app = web::App::new()
            .state(admin.clone())
            .state(cfg.clone())
            .state(db.clone())
            .state(mailer.clone())
            // The dedicated relay-forward mailer (§5.2a). A distinct newtype
            // so `State<RelayForwardMailer>` doesn't collide with the
            // transactional `State<Arc<dyn Mailer>>`.
            .state(relay_forward_mailer.clone())
            .middleware(RequestContextMiddleware)
            .middleware(SecurityHeaders::new(frame_ancestor_origins.clone()));
        if let Some(jwks) = google_jwks.clone() {
            app = app.state(jwks);
        }
        app.configure(configure(google_enabled, github_enabled))
    })
    .bind(&addr)?
    .run()
    .await
}
