//! End-to-end OIDC code+PKCE flow against a live hydra + the in-process
//! `crates/auth` server.
//!
//! Skips if `AUTH_DB_URL` and `HYDRA_ADMIN_URL` aren't both set. Live path
//! drives every component built across Phase 1 + Phase 2:
//!
//! - hydra admin client (create client, get login/consent challenges, accept)
//! - migrations (table existence)
//! - GET /login (renders form, returns CSRF cookie)
//! - POST /signup (creates user, redirects)
//! - POST /login (verifies credentials, accepts login)
//! - GET /consent (first-party skip path, accepts consent)
//! - The full OIDC code+PKCE round-trip including token exchange
//!
//! Hydra is configured with `urls.self.public = https://auth.zeroship.ai/`
//! but is reachable at `http://127.0.0.1:4444`. Every URL hydra hands us
//! (login redirect, `accept_login` `redirect_to`, consent `redirect_to`,
//! token `redirect_to`) is host-rewritten to `127.0.0.1:4444` before we
//! follow it; the query-string carries the only data that matters
//! (`login_challenge`, `consent_challenge`, `code`, etc.).
//!
//! Hydra's session cookies are scoped to `auth.zeroship.ai` by Set-Cookie's
//! Domain attribute. Our hand-rolled cookie jar ignores Domain and replays
//! every cookie back to hydra on every hop — hydra matches by name.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ntex::web;
use serde::Deserialize;
use uuid::Uuid;

use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::mailer::{Mailer, StdoutMailer};
use zeroship_auth::server;

mod common;
use common::{
    assert_redirect, cleanup_user, extract_query_param, location, pkce_challenge_s256,
    pkce_verifier, read_set_cookie, rewrite_to_hydra_loopback, test_auth_config, CookieJar,
};

// ─── Helpers ─────────────────────────────────────────────────────────────

/// Hydra's `POST /oauth2/token` response body. Fields not asserted on are
/// still validated by serde (must be present + correct type).
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // every field is asserted on OR retained for future expansion
struct TokenResponse {
    access_token: String,
    id_token: String,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

// ─── Test ────────────────────────────────────────────────────────────────

#[ntex::test]
async fn e2e_password_flow() {
    // 0. Env-skip check.
    let (Ok(db_url), Ok(hydra_admin_url)) = (
        std::env::var("AUTH_DB_URL"),
        std::env::var("HYDRA_ADMIN_URL"),
    ) else {
        eprintln!("[e2e_password] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };
    let hydra_public = std::env::var("HYDRA_PUBLIC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());

    // 1. Connect PG.
    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[e2e_password] pg connection driver: {e}");
        }
    })
    .detach();

    let pg_client = Arc::new(pg_client);
    let admin = HydraAdmin::new(&hydra_admin_url);
    // `--dev-insecure` drops Secure flag so the cyper client sees cookies on http://
    let cfg = Arc::new(test_auth_config(&db_url, &hydra_admin_url, &hydra_public));

    // 2. Boot the auth server via `ntex::web::test::server` — runs the
    //    factory in a worker thread with its own ntex system; returns a
    //    handle with `.url(path)`. State (db, admin, cfg) is `Arc`-clone-shared
    //    across that thread boundary.
    let admin_state = admin.clone();
    let cfg_state = cfg.clone();
    let db_state = pg_client.clone();
    let mailer_state: Arc<dyn Mailer> = Arc::new(StdoutMailer);
    let srv = web::test::server(move || {
        let admin_state = admin_state.clone();
        let cfg_state = cfg_state.clone();
        let db_state = db_state.clone();
        let mailer_state = mailer_state.clone();
        async move {
            web::App::new()
                .state(admin_state)
                .state(cfg_state)
                .state(db_state)
                .state(mailer_state)
                .middleware(SecurityHeaders)
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();
    eprintln!("[e2e_password] auth server at {auth_base}");

    // 3. Register a test OIDC client with hydra. EdDSA is the platform's
    //    primary algorithm and is now ensured by per-algorithm bootstrap
    //    (see crates/auth/src/bootstrap/keys.rs).
    let test_client_id = format!("e2e-{}", Uuid::new_v4().simple());
    let test_redirect = "http://127.0.0.1:9999/cb"; // never actually fetched
    let test_secret = "e2e-test-secret-do-not-use-in-prod".to_string();
    let client_spec = OAuth2Client {
        client_id: test_client_id.clone(),
        client_name: Some("e2e test".into()),
        client_secret: Some(test_secret.clone()),
        grant_types: vec!["authorization_code".into(), "refresh_token".into()],
        response_types: vec!["code".into()],
        redirect_uris: vec![test_redirect.into()],
        post_logout_redirect_uris: vec![],
        scope: "openid offline_access".into(),
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
    };
    admin
        .create_client(&client_spec)
        .await
        .expect("create test client");

    // In production the control plane creates an OAuth RP in BOTH hydra and
    // `zeroship.oauth_clients`; the consent flow records the granted scopes in
    // `zeroship.oauth_grants`, whose `client_id` FK references
    // `zeroship.oauth_clients`. This test registers the client only with hydra
    // (above), so we must mirror the control-plane row here — otherwise the
    // skip-consent silent-accept upsert fails the FK and the consent handler
    // renders an error page (200) instead of redirecting (302).
    pg_client
        .execute(
            "INSERT INTO zeroship.oauth_clients \
                 (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id) \
             VALUES ($1, $2, $3, $4, TRUE, $1) \
             ON CONFLICT (client_id) DO NOTHING",
            &[
                &test_client_id,
                &"e2e test",
                &vec![test_redirect.to_string()],
                &vec!["openid".to_string(), "offline_access".to_string()],
            ],
        )
        .await
        .expect("seed zeroship.oauth_clients for consent grant FK");

    // Cleanup guard via scope-exit: hydra client + user row removal at end.
    // We do this inline (after the assertions) rather than via a Drop guard
    // because the cleanup is async and we want failures to be visible.

    // 4. Generate PKCE pair + state + nonce.
    let verifier = pkce_verifier();
    let challenge = pkce_challenge_s256(&verifier);
    let state_token = format!("st-{}", Uuid::new_v4().simple());
    let nonce = format!("nc-{}", Uuid::new_v4().simple());

    let http = cyper::Client::new();
    let mut jar = CookieJar::default();

    // 5. GET hydra /oauth2/auth → 302 to https://auth.zeroship.ai/login?login_challenge=…
    let auth_url = {
        let q = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &test_client_id)
            .append_pair("response_type", "code")
            .append_pair("scope", "openid offline_access")
            .append_pair("redirect_uri", test_redirect)
            .append_pair("state", &state_token)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .finish();
        format!("{hydra_public}/oauth2/auth?{q}")
    };
    let resp = http
        .request(http::Method::GET, &auth_url)
        .expect("build /oauth2/auth")
        .send()
        .await
        .expect("send /oauth2/auth");
    assert_redirect(&resp, "hydra /oauth2/auth → /login");
    jar.absorb(&resp);
    let login_loc = location(&resp);
    let login_challenge = extract_query_param(&login_loc, "login_challenge")
        .expect("login_challenge param in hydra redirect");
    eprintln!("[e2e_password] login_challenge acquired");

    // 6. GET /login on our auth server → renders form, sets CSRF cookie.
    let login_get_url = format!("{auth_base}/login?login_challenge={login_challenge}");
    let resp = http
        .request(http::Method::GET, &login_get_url)
        .expect("build GET /login")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send GET /login");
    assert!(
        resp.status().is_success(),
        "GET /login expected 200, got {}",
        resp.status()
    );
    let csrf_cookie = read_set_cookie(&resp, "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /login");
    jar.set("zsidp_csrf", &csrf_cookie);

    // 7. POST /signup — create the user. login_challenge is preserved.
    let email = format!("e2e-{}@zeroship.test", Uuid::new_v4().simple());
    let password = "supersecurepassword-e2e-test";
    let name = "E2E Test";

    let signup_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf_cookie)
        .append_pair("name", name)
        .append_pair("email", &email)
        .append_pair("password", password)
        .finish();
    let signup_url = format!("{auth_base}/signup?login_challenge={login_challenge}");
    let resp = http
        .request(http::Method::POST, &signup_url)
        .expect("build POST /signup")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", jar.header())
        .expect("cookie header")
        .body(signup_body)
        .send()
        .await
        .expect("send POST /signup");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "POST /signup expected 302, got {}",
        resp.status()
    );
    jar.absorb(&resp);

    // 8. GET /login again to obtain a fresh CSRF cookie for the POST.
    let resp = http
        .request(http::Method::GET, &login_get_url)
        .expect("build GET /login (2)")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send GET /login (2)");
    assert!(resp.status().is_success(), "GET /login (2) expected 200");
    let csrf_cookie = read_set_cookie(&resp, "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /login (2)");
    jar.set("zsidp_csrf", &csrf_cookie);

    // 9. POST /login — accept_login → 302 to hydra (https://auth.zeroship.ai/oauth2/auth?…).
    let login_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf_cookie)
        .append_pair("email", &email)
        .append_pair("password", password)
        .finish();
    let resp = http
        .request(http::Method::POST, &login_get_url)
        .expect("build POST /login")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", jar.header())
        .expect("cookie header")
        .body(login_body)
        .send()
        .await
        .expect("send POST /login");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "POST /login expected 302 (got {})",
        resp.status()
    );
    jar.absorb(&resp);
    let to_hydra = location(&resp);
    assert!(
        !to_hydra.is_empty(),
        "POST /login should set Location to hydra redirect_to"
    );

    // 10. Follow that redirect back to hydra → 302 to /consent.
    let to_hydra_local = rewrite_to_hydra_loopback(&to_hydra);
    let resp = http
        .request(http::Method::GET, &to_hydra_local)
        .expect("build GET hydra-from-login")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send GET hydra-from-login");
    assert_redirect(&resp, "hydra (post-login) → /consent");
    jar.absorb(&resp);
    let consent_loc = location(&resp);
    let consent_challenge = extract_query_param(&consent_loc, "consent_challenge")
        .expect("consent_challenge param in hydra redirect");

    // 11. GET /consent on our auth server — skip path → 302 to hydra.
    let consent_url = format!("{auth_base}/consent?consent_challenge={consent_challenge}");
    let resp = http
        .request(http::Method::GET, &consent_url)
        .expect("build GET /consent")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send GET /consent");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "GET /consent skip-path expected 302 (got {})",
        resp.status()
    );
    jar.absorb(&resp);
    let to_hydra = location(&resp);

    // 12. Follow that redirect → 302 to redirect_uri with `code`.
    let to_hydra_local = rewrite_to_hydra_loopback(&to_hydra);
    let resp = http
        .request(http::Method::GET, &to_hydra_local)
        .expect("build GET hydra-from-consent")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send GET hydra-from-consent");
    assert_redirect(&resp, "hydra (post-consent) → RP redirect_uri");
    let cb_url = location(&resp);
    assert!(
        cb_url.starts_with(test_redirect),
        "callback URL must start with registered redirect_uri (got: {cb_url})"
    );
    let returned_state = extract_query_param(&cb_url, "state");
    assert_eq!(
        returned_state.as_deref(),
        Some(state_token.as_str()),
        "state must round-trip"
    );
    let code = extract_query_param(&cb_url, "code").expect("authorization code");

    // 13. Exchange the code at hydra /oauth2/token.
    let token_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", &code)
        .append_pair("redirect_uri", test_redirect)
        .append_pair("client_id", &test_client_id)
        .append_pair("client_secret", &test_secret)
        .append_pair("code_verifier", &verifier)
        .finish();
    let token_endpoint = format!("{hydra_public}/oauth2/token");
    let resp = http
        .request(http::Method::POST, token_endpoint)
        .expect("build POST /oauth2/token")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(token_body)
        .send()
        .await
        .expect("send POST /oauth2/token");
    let status = resp.status().as_u16();
    let body = resp.text().await.expect("token body");
    assert!(
        (200..300).contains(&status),
        "token exchange failed: {status} {body}"
    );

    let tr: TokenResponse = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("token decode {e}: {body}"));
    assert_eq!(tr.token_type.to_ascii_lowercase(), "bearer");
    assert!(tr.expires_in > 0);
    assert!(!tr.access_token.is_empty(), "access_token empty");
    assert_eq!(
        tr.id_token.split('.').count(),
        3,
        "id_token must be a JWS (got {})",
        tr.id_token
    );
    // Refresh token issued because `offline_access` was requested.
    assert!(
        tr.refresh_token.as_deref().is_some_and(|t| !t.is_empty()),
        "refresh_token expected with offline_access scope"
    );
    let _ = tr.scope; // hydra echoes granted scopes; not asserted.

    // 14. Decode ID-token claims (no signature verification — that would
    //     require pulling hydra's JWKS, which is orthogonal to "did Phase 2
    //     work"). Asserts the claims shape — `iss`, `aud`, `sub`, `nonce`.
    let claims_b64 = tr.id_token.split('.').nth(1).expect("id_token claims segment");
    let claims_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(claims_b64)
        .expect("base64url decode claims");
    let claims: serde_json::Value =
        serde_json::from_slice(&claims_bytes).expect("claims json");
    eprintln!("[e2e_password] id_token claims: {claims}");

    let iss = claims["iss"].as_str().expect("iss claim");
    // Hydra stamps `iss` from its own configured public URL, which is
    // environment-dependent: the historical fixture used
    // `https://auth.zeroship.ai/`, the docker-compose deployment uses
    // `http://auth.zeroship.localhost`, and a bare loopback hydra uses
    // `http://127.0.0.1:4444`. Accept whichever this hydra issued.
    assert!(
        iss == "https://auth.zeroship.ai/"
            || iss.starts_with("http://auth.zeroship.localhost")
            || iss.starts_with("https://auth.zeroship.localhost")
            || iss.starts_with("http://127.0.0.1:4444"),
        "unexpected iss: {iss}"
    );
    let aud = claims["aud"].as_array().expect("aud claim is array");
    assert!(
        aud.iter().any(|v| v.as_str() == Some(test_client_id.as_str())),
        "aud must contain test client_id: {aud:?}"
    );
    assert!(
        claims["sub"].as_str().is_some_and(|s| !s.is_empty()),
        "sub must be a non-empty string"
    );
    assert_eq!(
        claims["nonce"].as_str(),
        Some(nonce.as_str()),
        "nonce must round-trip"
    );

    // 15. Cleanup — delete client + delete user row + revoke session.
    //     Deleting the user cascades to zeroship.oauth_grants (user_id FK);
    //     we then drop the seeded zeroship.oauth_clients row.
    admin
        .delete_client(&test_client_id)
        .await
        .expect("delete test client");
    cleanup_user(&pg_client, &email).await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&test_client_id],
        )
        .await;

    // Give the server a beat to flush any pending audit writes before
    // we tear down its thread; otherwise the test occasionally races
    // the spawned audit::emit task on shutdown.
    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
}
