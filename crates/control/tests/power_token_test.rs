//! R4 — `POST /internal/power-token` mint, the grant-gated, identity-bound,
//! server-side capability boundary.
//!
//! These tests drive the REAL handler (`power_token::mint_power_token`) through
//! `ntex::web::test` against live Postgres (the same path the worker hits), with
//! real gateway-signed `ZeroShip-User` headers (HMAC over the worker_key) and
//! real anchor rows. No shims. They prove the load-bearing security properties:
//!
//!   - happy: an authorized, trusted (platform-privileged) app with a
//!     sufficient grant mints an `aud=control` token bound to the correct
//!     pairwise user + capped scopes — and that token authenticates against the
//!     control public API via AuthzGuard.
//!   - BOUNDARY: an ORDINARY (non-trusted) app CANNOT mint an `aud=control`
//!     token — `403 forbidden_audience`. (The headline regression.)
//!   - identity forge: a ZeroShip-User signed with the WRONG worker_key (or
//!     absent / tampered) yields NO token — `401 unauthenticated_identity`.
//!   - scope ceiling: requesting a scope beyond the grant → `403 scope_required`.
//!   - step-up: a deploy/secret-class scope with a STALE auth_time → `403
//!     step_up_required`; with a FRESH auth_time → minted.
//!   - control_key boundary: the mint requires the worker↔control control_key;
//!     a call without it is rejected by the internal-auth gate.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use compio_postgres::{connect, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::json;
use uuid::Uuid;

use zeroship_authz::{Action, Resource};
use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::authz_guard::AuthzGuard;
use zeroship_control::power_token::PowerTokenClaims;
use zeroship_control::{
    oidc_rp, power_token, token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
};
use zeroship_core::auth::{derive_pairwise, sign_zeroship_user_header};
use zeroship_core::hydra::HydraIntrospector;
use zeroship_core::power_token::CONTROL_PLANE_AUDIENCE;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const WORKER_KEY: &str = "test-worker-key-0123456789abcdef";
const CONTROL_KEY: &str = "test-control-key";
const AUDIENCE: &str = "control.zeroship.ai";
// A realistic (non-zero) salt so derive_pairwise is exercised meaningfully.
const PAIRWISE_SALT: [u8; 32] = [7u8; 32];
const SECTOR: &str = "https://console.zeroship.localhost";

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("zs-power-token-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    /// The seeded app id + its oauth client id (passed via X-ZS-App-Id).
    app_id: Uuid,
    client_id: String,
    /// The global user behind the anchor + their derived pairwise subject.
    user_id: Uuid,
    pws: String,
}

/// Build an AppState whose `trusted_oauth_clients` set is exactly `trusted`
/// (so the boundary test can seed a NON-trusted app), seed a user + app +
/// oauth client + anchor, and return the fixture. `granted_scopes` is the
/// anchor's grant ceiling; `auth_time` is the authenticating-event instant.
async fn fixture(
    label: &str,
    granted_scopes: &[&str],
    auth_time: chrono::DateTime<chrono::Utc>,
    make_client_trusted: bool,
) -> Option<Fixture> {
    let db_url = db_url()?;

    let (auth_pg_client, auth_pg_conn) = connect(&db_url, NoTls).await.expect("auth-pg connect");
    compio::runtime::spawn(async move {
        let _ = auth_pg_conn.run().await;
    })
    .detach();

    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
    let registry = Registry::new(&db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let vfs: Arc<dyn BundleStore + Send + Sync> =
        Arc::new(LocalFs::new(blob_root.join("legacy-bundles")).expect("vfs"));
    let oidc_rp = Arc::new(oidc_rp::ConsoleOidcRp::new(
        "http://localhost:4444",
        "console.zeroship.ai",
        "test-oidc-secret".to_string(),
        b"test-stash-key".to_vec(),
    ));

    // Seed a user + app + oauth client + app_oauth_clients extension.
    let app_id = Uuid::new_v4();
    let client_id = format!("oac_{}", app_id.simple());
    let user_id = Uuid::new_v4();
    let email = format!("{label}-{user_id}@zeroship.test");

    auth_pg_client
        .execute(
            "INSERT INTO auth.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, 'Power Token Test User', NOW())",
            &[&user_id, &email],
        )
        .await
        .expect("insert user");
    auth_pg_client
        .execute(
            "INSERT INTO control.apps (id, name, api_key) VALUES ($1, $2, $3)",
            &[
                &app_id,
                &format!("app-{}", app_id.simple()),
                &format!("ak_{}", Uuid::new_v4().simple()),
            ],
        )
        .await
        .expect("insert app");
    let redirect_uris = vec![format!("https://{client_id}.example/cb")];
    let oauth_scopes = vec!["apps:read", "apps:deploy", "env:read", "secrets:write"];
    auth_pg_client
        .execute(
            "INSERT INTO control.oauth_clients \
                (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id) \
             VALUES ($1, $2, $3, $4, false, $1)",
            &[
                &client_id,
                &format!("Client {client_id}"),
                &redirect_uris,
                &oauth_scopes,
            ],
        )
        .await
        .expect("insert oauth client");
    auth_pg_client
        .execute(
            "INSERT INTO control.app_oauth_clients (app_id, client_id, sector_identifier) \
             VALUES ($1, $2, $3)",
            &[&app_id, &client_id, &SECTOR],
        )
        .await
        .expect("insert app_oauth_clients");

    // Seed a live anchor for (app, user) with the grant ceiling + auth_time.
    let scopes_vec: Vec<String> = granted_scopes.iter().map(|s| s.to_string()).collect();
    auth_pg_client
        .execute(
            "INSERT INTO auth.app_session_anchors \
                (app_id, client_id, global_user_id, refresh_token_enc, refresh_family_id, \
                 granted_scopes, auth_time, abs_expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, NOW() + interval '30 days')",
            &[
                &app_id.to_string(),
                &client_id,
                &user_id,
                &b"enc".to_vec(),
                &format!("rfam_{}", Uuid::new_v4().simple()),
                &scopes_vec,
                &auth_time,
            ],
        )
        .await
        .expect("insert anchor");

    let pws = derive_pairwise(&PAIRWISE_SALT, &user_id.to_string(), SECTOR);

    let trusted = if make_client_trusted {
        [client_id.clone()].into_iter().collect()
    } else {
        // An empty trusted set → the seeded app is NOT platform-privileged.
        std::collections::HashSet::new()
    };

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        vfs,
        blob_store,
        control_key: SecretString::new(CONTROL_KEY.to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(WORKER_KEY.to_string()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: false,
        trust_proxy: false,
        deploy_tmp_dir,
        oidc_rp,
        auth_pg: Arc::new(auth_pg_client),
        auth_db_url: db_url.to_string(),
        hydra_admin_url: "http://localhost:4445".to_string(),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: trusted,
        expected_oauth_audience: AUDIENCE.to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        pat_issuer: Arc::new(token_handlers::PatIssuer::dev_insecure()),
        power_token_issuer: Arc::new(power_token::PowerTokenIssuer::dev_insecure(
            AUDIENCE.to_string(),
        )),
        hydra_introspector: Arc::new(HydraIntrospector::new("http://localhost:4445")),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        pairwise_salt: PAIRWISE_SALT,
    });

    Some(Fixture {
        state,
        app_id,
        client_id,
        user_id,
        pws,
    })
}

async fn cleanup(fx: &Fixture) {
    let pg = &fx.state.auth_pg;
    let app_text = fx.app_id.to_string();
    let _ = pg
        .execute(
            "DELETE FROM auth.app_session_anchors WHERE app_id = $1",
            &[&app_text],
        )
        .await;
    let _ = pg
        .execute(
            "DELETE FROM control.app_oauth_clients WHERE app_id = $1",
            &[&fx.app_id],
        )
        .await;
    let _ = pg
        .execute(
            "DELETE FROM control.oauth_clients WHERE client_id = $1",
            &[&fx.client_id],
        )
        .await;
    let _ = pg
        .execute("DELETE FROM control.apps WHERE id = $1", &[&fx.app_id])
        .await;
    let _ = pg
        .execute("DELETE FROM auth.users WHERE id = $1", &[&fx.user_id])
        .await;
}

/// Build the gateway-signed ZeroShip-User header for `pws_` with `scopes`,
/// signed with `signing_key`. The gateway signs with the worker_key; a forged
/// header signed with a different key must be rejected control-side.
fn signed_user_header(pws: &str, scopes: &[&str], signing_key: &[u8]) -> String {
    let user = json!({
        "id": pws,
        "email": "user@relay.test",
        "name": "User",
        "email_verified": true,
        "scopes": scopes,
    });
    let user_json = serde_json::to_vec(&user).unwrap();
    sign_zeroship_user_header(signing_key, &user_json, Uuid::new_v4())
}

/// Build the test app routing to the mint handler. Inline (the ntex service
/// factory types don't name cleanly through a helper return).
macro_rules! init_mint_app {
    ($state:expr) => {{
        test::init_service(
            web::App::new().state($state).service(
                web::resource("/internal/power-token")
                    .route(web::post().to(power_token::mint_power_token)),
            ),
        )
        .await
    }};
}

fn post_mint(
    pws: &str,
    header_scopes: &[&str],
    app_id: Uuid,
    audience: &str,
    requested_scopes: &[&str],
    signing_key: &[u8],
    with_control_key: bool,
) -> test::TestRequest {
    let header = signed_user_header(pws, header_scopes, signing_key);
    let body = json!({ "audience": audience, "scopes": requested_scopes });
    let mut req = test::TestRequest::post()
        .uri("/internal/power-token")
        .header("zeroship-user", header)
        .header("x-zs-app-id", app_id.to_string())
        .set_json(&body);
    if with_control_key {
        req = req.header("authorization", format!("Bearer {CONTROL_KEY}"));
    }
    req
}

macro_rules! require_db {
    ($fx:expr) => {
        match $fx {
            Some(f) => f,
            None => {
                eprintln!("[power_token_test] AUTH_DB_URL not set — skipping");
                return;
            }
        }
    };
}

#[compio::test]
async fn happy_path_mints_aud_control_token_for_trusted_app() {
    let fx = require_db!(fixture("happy", &["apps:read", "env:read"], Utc::now(), true).await);
    let app = init_mint_app!(fx.state.clone());

    let req = post_mint(
        &fx.pws,
        &["apps:read", "env:read"],
        fx.app_id,
        CONTROL_PLANE_AUDIENCE,
        &["apps:read"],
        WORKER_KEY.as_bytes(),
        true,
    );
    let resp = test::call_service(&app, req.to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK, "trusted app + sufficient grant should mint");
    let body: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let token = body["access_token"].as_str().expect("access_token present");
    assert_eq!(body["scopes"], json!(["apps:read"]), "capped scopes echoed");

    // The minted token verifies + is bound to the correct global user + capped
    // scope (NOT the over-broad grant).
    let claims: PowerTokenClaims = fx.state.power_token_issuer.verify(token).expect("verify");
    assert_eq!(claims.sub, fx.user_id.to_string(), "bound to the global user");
    assert_eq!(claims.aud, AUDIENCE, "aud = control");
    assert_eq!(claims.pws, fx.pws, "carries the pairwise subject");
    assert_eq!(claims.scope, "apps:read", "scope capped to the request");
    assert!(claims.exp > claims.iat, "short-lived");

    cleanup(&fx).await;
}

#[compio::test]
async fn happy_path_token_authenticates_against_control_authz() {
    // The minted token must authenticate against the control public API via
    // AuthzGuard's bearer path (the faithful end-to-end of fetchAs).
    let fx = require_db!(fixture("authz", &["apps:read"], Utc::now(), true).await);
    let (token, _exp) = fx
        .state
        .power_token_issuer
        .issue(fx.user_id, &fx.pws, &fx.client_id, &["apps:read".to_string()], Utc::now().timestamp(), 300)
        .expect("issue");

    // Drive a request carrying the token through AuthzGuard.
    async fn probe(guard: AuthzGuard, state: web::types::State<Arc<AppState>>) -> web::HttpResponse {
        match guard.require(Action::AppsRead, Resource::Any, &state).await {
            Ok(()) => web::HttpResponse::Ok().body("allowed"),
            Err(resp) => resp,
        }
    }
    let app = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/apps").route(web::get().to(probe)),
        ),
    )
    .await;
    let req = test::TestRequest::get()
        .uri("/apps")
        .header("authorization", format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "power token should authenticate apps:read");
    cleanup(&fx).await;
}

#[compio::test]
async fn boundary_ordinary_app_cannot_mint_control_audience() {
    // THE HEADLINE REGRESSION. The app is NOT in trusted_oauth_clients.
    let fx = require_db!(fixture("boundary", &["apps:read"], Utc::now(), false).await);
    let app = init_mint_app!(fx.state.clone());

    let req = post_mint(
        &fx.pws,
        &["apps:read"],
        fx.app_id,
        CONTROL_PLANE_AUDIENCE,
        &["apps:read"],
        WORKER_KEY.as_bytes(),
        true,
    );
    let resp = test::call_service(&app, req.to_request()).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "an ordinary creator app must NOT mint an aud=control token"
    );
    let body: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["error"], "forbidden_audience");
    cleanup(&fx).await;
}

#[compio::test]
async fn forged_identity_with_wrong_worker_key_is_rejected() {
    let fx = require_db!(fixture("forge", &["apps:read"], Utc::now(), true).await);
    let app = init_mint_app!(fx.state.clone());

    // Sign the ZeroShip-User header with the WRONG key — an attacker who does
    // not hold worker_key cannot mint a valid identity assertion.
    let req = post_mint(
        &fx.pws,
        &["apps:read"],
        fx.app_id,
        CONTROL_PLANE_AUDIENCE,
        &["apps:read"],
        b"attacker-key-not-the-worker-key!",
        true,
    );
    let resp = test::call_service(&app, req.to_request()).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "forged identity → 401");
    let body: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["error"], "unauthenticated_identity");
    cleanup(&fx).await;
}

#[compio::test]
async fn absent_identity_header_is_rejected() {
    let fx = require_db!(fixture("absent", &["apps:read"], Utc::now(), true).await);
    let app = init_mint_app!(fx.state.clone());

    let body = json!({ "audience": CONTROL_PLANE_AUDIENCE, "scopes": ["apps:read"] });
    let req = test::TestRequest::post()
        .uri("/internal/power-token")
        .header("authorization", format!("Bearer {CONTROL_KEY}"))
        .header("x-zs-app-id", fx.app_id.to_string())
        .set_json(&body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "no identity header → 401");
    cleanup(&fx).await;
}

#[compio::test]
async fn forged_identity_for_another_user_yields_no_token() {
    // Even a VALID-key-signed header for a pws_ that has NO anchor (i.e. a
    // different user / unconsented) must not mint — there is no grant to bind.
    let fx = require_db!(fixture("other-user", &["apps:read"], Utc::now(), true).await);
    let app = init_mint_app!(fx.state.clone());

    let other_pws = derive_pairwise(&PAIRWISE_SALT, &Uuid::new_v4().to_string(), SECTOR);
    let req = post_mint(
        &other_pws,
        &["apps:read"],
        fx.app_id,
        CONTROL_PLANE_AUDIENCE,
        &["apps:read"],
        WORKER_KEY.as_bytes(),
        true,
    );
    let resp = test::call_service(&app, req.to_request()).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a pws_ with no anchor for this app cannot mint"
    );
    let body: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["error"], "consent_required");
    cleanup(&fx).await;
}

#[compio::test]
async fn scope_beyond_grant_ceiling_is_denied() {
    // Grant ceiling = ["apps:read"]; the app requests "apps:deploy".
    let fx = require_db!(fixture("ceiling", &["apps:read"], Utc::now(), true).await);
    let app = init_mint_app!(fx.state.clone());

    let req = post_mint(
        &fx.pws,
        &["apps:read"],
        fx.app_id,
        CONTROL_PLANE_AUDIENCE,
        &["apps:deploy"], // NOT in the ceiling
        WORKER_KEY.as_bytes(),
        true,
    );
    let resp = test::call_service(&app, req.to_request()).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "over-ceiling scope denied");
    let body: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["error"], "scope_required");
    cleanup(&fx).await;
}

#[compio::test]
async fn step_up_scope_with_stale_auth_time_is_rejected() {
    // Grant includes apps:deploy; auth_time is 1 hour ago (stale for step-up).
    let stale = Utc::now() - chrono::Duration::seconds(3600);
    let fx = require_db!(fixture("stepup-stale", &["apps:read", "apps:deploy"], stale, true).await);
    let app = init_mint_app!(fx.state.clone());

    let req = post_mint(
        &fx.pws,
        &["apps:read", "apps:deploy"],
        fx.app_id,
        CONTROL_PLANE_AUDIENCE,
        &["apps:deploy"], // elevated scope
        WORKER_KEY.as_bytes(),
        true,
    );
    let resp = test::call_service(&app, req.to_request()).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "stale auth_time on deploy → step-up");
    let body: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["error"], "step_up_required");
    cleanup(&fx).await;
}

#[compio::test]
async fn step_up_scope_with_fresh_auth_time_is_allowed() {
    // Same as above but auth_time is now (fresh) — the deploy mint proceeds.
    let fx = require_db!(
        fixture("stepup-fresh", &["apps:read", "apps:deploy"], Utc::now(), true).await
    );
    let app = init_mint_app!(fx.state.clone());

    let req = post_mint(
        &fx.pws,
        &["apps:read", "apps:deploy"],
        fx.app_id,
        CONTROL_PLANE_AUDIENCE,
        &["apps:deploy"],
        WORKER_KEY.as_bytes(),
        true,
    );
    let resp = test::call_service(&app, req.to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK, "fresh auth_time on deploy should mint");
    let body: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let claims: PowerTokenClaims = fx
        .state
        .power_token_issuer
        .verify(body["access_token"].as_str().unwrap())
        .expect("verify");
    assert_eq!(claims.scope, "apps:deploy");
    cleanup(&fx).await;
}

#[compio::test]
async fn missing_control_key_is_rejected_by_internal_gate() {
    // The worker↔control control_key is the channel gate. A call without it
    // (e.g. anything that bypassed the runtime-mediated op) is rejected.
    let fx = require_db!(fixture("nokey", &["apps:read"], Utc::now(), true).await);
    let app = init_mint_app!(fx.state.clone());

    let req = post_mint(
        &fx.pws,
        &["apps:read"],
        fx.app_id,
        CONTROL_PLANE_AUDIENCE,
        &["apps:read"],
        WORKER_KEY.as_bytes(),
        false, // NO control_key
    );
    let resp = test::call_service(&app, req.to_request()).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "no control_key → internal auth rejects"
    );
    cleanup(&fx).await;
}
