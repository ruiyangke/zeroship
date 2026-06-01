//! End-to-end headless in-page password login (`POST /password`) against a live
//! Hydra + the in-process `crates/auth` server.
//!
//! This is the credential→code path the gateway uses for in-page login: the
//! gateway POSTs the user's credentials (plus the browser's PKCE
//! `code_challenge`, `state`, `nonce`) to `/password` with the gateway↔auth
//! shared secret as `Authorization: Bearer <key>`; the auth service verifies the
//! credentials, runs the headless OAuth dance, and returns `{ code, state? }`.
//! The test then exchanges that code at Hydra's `/oauth2/token` and asserts the
//! id_token carries `amr=["pwd"]`.
//!
//! Skips when `AUTH_DB_URL` and `HYDRA_ADMIN_URL` aren't BOTH set (env-skip),
//! exactly like `e2e_password.rs` — the live happy path needs a reachable Hydra.
//! The credential-failure arms (wrong password, locked, rate-limit) return
//! BEFORE the Hydra dance, and the shared-secret gate returns before any DB
//! access; those are additionally covered OFFLINE by the unit tests in
//! `crates/auth/src/ui/password.rs` (`secret_gate_*`).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ntex::web;
use serde::Deserialize;
use uuid::Uuid;

use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::server;

mod common;
use common::{
    cleanup_rate_limits_like, cleanup_user, pkce_challenge_s256, pkce_verifier, test_auth_config,
};

const INTERNAL_KEY: &str = "test-internal-key-gateway-to-auth-32b!";

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TokenResponse {
    access_token: String,
    id_token: String,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
}

/// `{ code, state? }` success body shape.
#[derive(Debug, Deserialize)]
struct PasswordGrantResponse {
    code: String,
    #[serde(default)]
    state: Option<String>,
}

/// Boot shared scaffolding (PG, hydra admin, in-process server with the internal
/// key set, a registered first-party skip_consent client + its oauth_clients
/// row). Returns `None` on env-skip.
struct Scaffold {
    srv: web::test::TestServer,
    auth_base: String,
    admin: HydraAdmin,
    pg: Arc<compio_postgres::Client>,
    http: cyper::Client,
    test_client_id: String,
    test_secret: String,
    test_redirect: &'static str,
    hydra_public: String,
}

async fn boot() -> Option<Scaffold> {
    let (Ok(db_url), Ok(hydra_admin_url)) = (
        std::env::var("AUTH_DB_URL"),
        std::env::var("HYDRA_ADMIN_URL"),
    ) else {
        eprintln!("[e2e_password_grant] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return None;
    };
    let hydra_public = std::env::var("HYDRA_PUBLIC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());

    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[e2e_password_grant] pg connection driver: {e}");
        }
    })
    .detach();
    let pg = Arc::new(pg_client);

    let admin = HydraAdmin::new(&hydra_admin_url);
    let mut cfg = test_auth_config(&db_url, &hydra_admin_url, &hydra_public);
    // The headless endpoint requires the gateway↔auth shared secret. The test
    // fixture leaves it empty (gate disabled); set it so we exercise the gate.
    cfg.auth_internal_key = INTERNAL_KEY.to_string();
    let cfg = Arc::new(cfg);

    let admin_state = admin.clone();
    let cfg_state = cfg.clone();
    let db_state = pg.clone();
    let srv = web::test::server(move || {
        let admin_state = admin_state.clone();
        let cfg_state = cfg_state.clone();
        let db_state = db_state.clone();
        async move {
            web::App::new()
                .state(admin_state)
                .state(cfg_state)
                .state(db_state)
                .middleware(SecurityHeaders)
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    // First-party client (skip_consent=true) so the headless silent self-grant
    // path is eligible (invariant III).
    let test_client_id = format!("grant-{}", Uuid::new_v4().simple());
    let test_secret = "grant-test-secret-do-not-use".to_string();
    let test_redirect = "http://127.0.0.1:9999/cb";
    admin
        .create_client(&OAuth2Client {
            client_id: test_client_id.clone(),
            client_name: Some("grant test".into()),
            client_secret: Some(test_secret.clone()),
            grant_types: vec!["authorization_code".into()],
            response_types: vec!["code".into()],
            redirect_uris: vec![test_redirect.into()],
            post_logout_redirect_uris: vec![],
            scope: "openid profile email".into(),
            token_endpoint_auth_method: "client_secret_post".into(),
            subject_type: "public".into(),
            access_token_strategy: None,
            id_token_signed_response_alg: Some("EdDSA".into()),
            audience: vec![],
            skip_consent: true,
            require_consent: false,
            require_logout_consent: false,
            frontchannel_logout_uri: None,
            backchannel_logout_uri: None,
        })
        .await
        .expect("create test client");
    // Mirror the control-plane oauth_clients row so the silent-consent grant
    // upsert satisfies its FK (see e2e_password.rs for the rationale).
    pg.execute(
        "INSERT INTO zeroship.oauth_clients \
             (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id) \
         VALUES ($1, $2, $3, $4, TRUE, $1) \
         ON CONFLICT (client_id) DO NOTHING",
        &[
            &test_client_id,
            &"grant test",
            &vec![test_redirect.to_string()],
            &vec!["openid".to_string(), "profile".to_string(), "email".to_string()],
        ],
    )
    .await
    .expect("seed zeroship.oauth_clients");

    Some(Scaffold {
        srv,
        auth_base,
        admin,
        pg,
        http: cyper::Client::new(),
        test_client_id,
        test_secret,
        test_redirect,
        hydra_public,
    })
}

/// Hash a password off the event loop and insert a user. `locked` sets
/// `locked_until` an hour into the future.
async fn seed_user(pg: &compio_postgres::Client, email: &str, password: &str, locked: bool) {
    let phc = compio::runtime::spawn_blocking({
        let pw = password.to_string();
        move || zeroship_auth::identity::password::hash(&pw)
    })
    .await
    .expect("hash spawn")
    .expect("hash ok");
    if locked {
        pg.execute(
            "INSERT INTO zeroship.users (email, name, password_hash, locked_until) \
             VALUES ($1::citext, $2, $3, NOW() + INTERVAL '1 hour')",
            &[&email, &"Locked User", &phc.as_str()],
        )
        .await
        .expect("insert locked user");
    } else {
        pg.execute(
            "INSERT INTO zeroship.users (email, name, password_hash) VALUES ($1::citext, $2, $3)",
            &[&email, &"Grant User", &phc.as_str()],
        )
        .await
        .expect("insert user");
    }
}

/// POST /password with the internal-key bearer. `bearer` lets negatives omit /
/// corrupt it. Returns (status, body-text).
async fn post_password(
    http: &cyper::Client,
    auth_base: &str,
    bearer: Option<&str>,
    body: &serde_json::Value,
) -> (u16, String) {
    let mut req = http
        .request(http::Method::POST, format!("{auth_base}/password"))
        .expect("build POST /password")
        .header("content-type", "application/json")
        .expect("content-type");
    if let Some(b) = bearer {
        req = req
            .header("authorization", format!("Bearer {b}"))
            .expect("authz header");
    }
    let resp = req
        .body(serde_json::to_vec(body).expect("encode body"))
        .send()
        .await
        .expect("send POST /password");
    let status = resp.status().as_u16();
    let text = resp.text().await.expect("body text");
    (status, text)
}

#[ntex::test]
async fn e2e_password_grant() {
    let Some(sc) = boot().await else { return };

    let email = format!("grant-{}@zeroship.test", Uuid::new_v4().simple());
    let password = "supersecurepassword-grant-test";
    let locked_email = format!("locked-{}@zeroship.test", Uuid::new_v4().simple());
    let locked_password = "locked-user-password-1234";
    seed_user(&sc.pg, &email, password, false).await;
    seed_user(&sc.pg, &locked_email, locked_password, true).await;

    // Drain any stale rate-limit budget for these emails.
    cleanup_rate_limits_like(
        &sc.pg,
        &["login:%grant-%", "login:%locked-%"],
    )
    .await;

    let verifier = pkce_verifier();
    let challenge = pkce_challenge_s256(&verifier);
    let state_token = format!("st-{}", Uuid::new_v4().simple());
    let nonce = format!("nc-{}", Uuid::new_v4().simple());

    let base_body = |email: &str, pw: &str| {
        serde_json::json!({
            "email": email,
            "password": pw,
            "client_id": sc.test_client_id,
            "redirect_uri": sc.test_redirect,
            "scope": "openid profile email",
            "state": state_token,
            "nonce": nonce,
            "code_challenge": challenge,
            "code_challenge_method": "S256",
        })
    };

    // ── NEGATIVE: missing shared secret → 401 (before any DB/credential work) ──
    let (status, _body) =
        post_password(&sc.http, &sc.auth_base, None, &base_body(&email, password)).await;
    assert_eq!(status, 401, "missing internal secret must be 401");

    // ── NEGATIVE: wrong shared secret → 403 ──────────────────────────────────
    let (status, _body) = post_password(
        &sc.http,
        &sc.auth_base,
        Some("wrong-internal-key"),
        &base_body(&email, password),
    )
    .await;
    assert_eq!(status, 403, "wrong internal secret must be 403");

    // ── NEGATIVE: wrong password → 401 (dummy-hash arm runs server-side) ─────
    let (status, body) = post_password(
        &sc.http,
        &sc.auth_base,
        Some(INTERNAL_KEY),
        &base_body(&email, "the-wrong-password-zzz"),
    )
    .await;
    assert_eq!(status, 401, "wrong password must be 401 (got body: {body})");
    assert!(
        body.contains("invalid_credentials"),
        "wrong-password body should carry invalid_credentials: {body}"
    );

    // ── NEGATIVE: missing-user (any password) → 401, opaque + same as wrong-pw ─
    let ghost = format!("ghost-{}@zeroship.test", Uuid::new_v4().simple());
    let (status, body) = post_password(
        &sc.http,
        &sc.auth_base,
        Some(INTERNAL_KEY),
        &base_body(&ghost, "any-password-here-123"),
    )
    .await;
    assert_eq!(status, 401, "missing user must be 401 (dummy-hash enumeration defense)");
    assert!(body.contains("invalid_credentials"), "missing user body: {body}");
    cleanup_rate_limits_like(&sc.pg, &["login:%ghost-%"]).await;

    // ── NEGATIVE: locked account → 403 ──────────────────────────────────────
    let (status, body) = post_password(
        &sc.http,
        &sc.auth_base,
        Some(INTERNAL_KEY),
        &base_body(&locked_email, locked_password),
    )
    .await;
    assert_eq!(status, 403, "locked account must be 403 (got body: {body})");

    // ── NEGATIVE: rate-limit → 429 ──────────────────────────────────────────
    // The per-(email,ip) bucket is 5 tokens. We've already spent some on this
    // email above; hammer the per-email bucket until it throttles. Use the
    // wrong password so we never actually log in.
    cleanup_rate_limits_like(&sc.pg, &["login:%grant-%"]).await;
    let mut saw_429 = false;
    for _ in 0..12 {
        let (status, _b) = post_password(
            &sc.http,
            &sc.auth_base,
            Some(INTERNAL_KEY),
            &base_body(&email, "the-wrong-password-zzz"),
        )
        .await;
        if status == 429 {
            saw_429 = true;
            break;
        }
    }
    assert!(saw_429, "expected a 429 after exhausting the rate-limit bucket");

    // ── HAPPY PATH: correct credentials → {code}; exchange → amr=[pwd] ───────
    // Fresh PKCE pair + drained buckets so the success isn't throttled.
    cleanup_rate_limits_like(&sc.pg, &["login:%grant-%"]).await;
    let verifier = pkce_verifier();
    let challenge = pkce_challenge_s256(&verifier);
    let state_token = format!("st-{}", Uuid::new_v4().simple());
    let nonce = format!("nc-{}", Uuid::new_v4().simple());
    let body = serde_json::json!({
        "email": email,
        "password": password,
        "client_id": sc.test_client_id,
        "redirect_uri": sc.test_redirect,
        "scope": "openid profile email",
        "state": state_token,
        "nonce": nonce,
        "code_challenge": challenge,
        "code_challenge_method": "S256",
    });
    let (status, raw) = post_password(&sc.http, &sc.auth_base, Some(INTERNAL_KEY), &body).await;
    assert_eq!(status, 200, "happy path expected 200, got {status}: {raw}");
    let grant: PasswordGrantResponse =
        serde_json::from_str(&raw).unwrap_or_else(|e| panic!("decode grant {e}: {raw}"));
    assert!(!grant.code.is_empty(), "code must be non-empty");
    assert_eq!(
        grant.state.as_deref(),
        Some(state_token.as_str()),
        "state must round-trip"
    );

    // Exchange the code at Hydra's /oauth2/token with the PKCE verifier.
    let token_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", &grant.code)
        .append_pair("redirect_uri", sc.test_redirect)
        .append_pair("client_id", &sc.test_client_id)
        .append_pair("client_secret", &sc.test_secret)
        .append_pair("code_verifier", &verifier)
        .finish();
    let resp = sc
        .http
        .request(http::Method::POST, format!("{}/oauth2/token", sc.hydra_public))
        .expect("build POST /oauth2/token")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(token_body)
        .send()
        .await
        .expect("send POST /oauth2/token");
    let tstatus = resp.status().as_u16();
    let tbody = resp.text().await.expect("token body");
    assert!(
        (200..300).contains(&tstatus),
        "token exchange failed: {tstatus} {tbody}"
    );
    let tr: TokenResponse =
        serde_json::from_str(&tbody).unwrap_or_else(|e| panic!("token decode {e}: {tbody}"));
    assert_eq!(tr.token_type.to_ascii_lowercase(), "bearer");
    assert!(tr.expires_in > 0);

    // Decode the id_token claims and assert amr=["pwd"] + nonce round-trip.
    let claims_b64 = tr.id_token.split('.').nth(1).expect("id_token claims segment");
    let claims_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(claims_b64)
        .expect("base64url decode claims");
    let claims: serde_json::Value =
        serde_json::from_slice(&claims_bytes).expect("claims json");
    eprintln!("[e2e_password_grant] id_token claims: {claims}");

    let amr = claims["amr"].as_array().expect("amr claim is an array");
    assert!(
        amr.iter().any(|v| v.as_str() == Some("pwd")),
        "amr must contain pwd: {amr:?}"
    );
    assert_eq!(
        claims["nonce"].as_str(),
        Some(nonce.as_str()),
        "nonce must round-trip"
    );
    assert!(
        claims["sub"].as_str().is_some_and(|s| !s.is_empty()),
        "sub must be a non-empty string"
    );
    // Identity claims surfaced via the silent self-grant id_token session.
    assert_eq!(
        claims["email"].as_str(),
        Some(email.as_str()),
        "email claim must match the verified user (profile/email scopes granted)"
    );

    // Sanity: the returned code is a real authorization code (callback would
    // have carried it). Also verify it's single-use — a second exchange fails.
    let replay_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", &grant.code)
        .append_pair("redirect_uri", sc.test_redirect)
        .append_pair("client_id", &sc.test_client_id)
        .append_pair("client_secret", &sc.test_secret)
        .append_pair("code_verifier", &verifier)
        .finish();
    let replay = sc
        .http
        .request(http::Method::POST, format!("{}/oauth2/token", sc.hydra_public))
        .expect("build replay")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(replay_body)
        .send()
        .await
        .expect("send replay");
    assert!(
        !(200..300).contains(&replay.status().as_u16()),
        "authorization code must be single-use (replay should fail)"
    );

    // Cleanup.
    let _ = sc.admin.delete_client(&sc.test_client_id).await;
    cleanup_user(&sc.pg, &email).await;
    cleanup_user(&sc.pg, &locked_email).await;
    let _ = sc
        .pg
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&sc.test_client_id],
        )
        .await;
    compio::time::sleep(Duration::from_millis(50)).await;
    drop(sc.srv);
}
