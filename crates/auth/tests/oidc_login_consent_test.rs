//! P4 native login + consent front door for the platform OP `/authorize` flow.

mod common;

use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::SigningKey;
use http::Method;
use ntex::web;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::identity::{password, totp};
use zeroship_auth::oidc::Issuer;
use zeroship_auth::server;
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::{sessions as session_store, totp as totp_store};

use common::{location, pkce_challenge_s256, pkce_verifier, read_set_cookie, test_auth_config};

const ISSUER: &str = "https://auth.zeroship.test";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/native-cb";
const SECTOR: &str = "https://native-app.zeroship.test";
const PASSWORD: &str = "correct native password phrase";

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    id_token: String,
    token_type: String,
    scope: String,
}

struct Fixture {
    srv: ntex::web::test::TestServer,
    auth_base: String,
    db: Arc<Client>,
    cfg: Arc<zeroship_auth::config::AuthConfig>,
    client_id: String,
    app_id: Uuid,
    user_id: Uuid,
    email: String,
    http: cyper::Client,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot() -> Option<Self> {
        let Some(db_url) = db_url() else {
            eprintln!("[op_login_consent_test] skip (AUTH_DB_URL or CONTROL_TEST_DB unset)");
            return None;
        };
        let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(err) = pg_connection.run().await {
                eprintln!("[op_login_consent_test] pg connection error: {err}");
            }
        })
        .detach();
        let db = Arc::new(pg_client);
        let issuer = Arc::new(test_issuer());
        issuer
            .publish_active_key(&db)
            .await
            .expect("publish active OP key");

        let user_id = Uuid::new_v4();
        let app_id = Uuid::new_v4();
        let client_id = format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(&app_id));
        let email = format!("p4-{}@zeroship.test", Uuid::new_v4().simple());
        seed_user_client(&db, user_id, app_id, &client_id, &email).await;

        let cfg = Arc::new(test_auth_config(
            &db_url,
            "http://127.0.0.1:4445",
            "http://127.0.0.1:4444",
        ));
        let admin = HydraAdmin::new("http://127.0.0.1:4445");
        let admin_state = admin.clone();
        let cfg_state = cfg.clone();
        let db_state = db.clone();
        let issuer_state = issuer.clone();
        let refresh_pool_state =
            zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
        let srv = web::test::server(move || {
            let admin_state = admin_state.clone();
            let cfg_state = cfg_state.clone();
            let db_state = db_state.clone();
            let issuer_state = issuer_state.clone();
            let refresh_pool_state = refresh_pool_state.clone();
            async move {
                web::App::new()
                    .state(admin_state)
                    .state(cfg_state)
                    .state(db_state)
                    .state(issuer_state)
                    .state(refresh_pool_state)
                    .middleware(SecurityHeaders::default())
                    .configure(server::configure(false, false))
            }
        })
        .await;

        Some(Self {
            auth_base: srv.url("").trim_end_matches('/').to_string(),
            srv,
            db,
            cfg,
            client_id,
            app_id,
            user_id,
            email,
            http: cyper::Client::new(),
        })
    }

    async fn cleanup(self) {
        cleanup_seeded_rows(&self.db, self.user_id, self.app_id, &self.client_id).await;
        drop(self.srv);
    }

    async fn create_session_cookie(&self) -> String {
        let session = session_store::create(
            &self.db,
            &session_store::CreateSession {
                user_id: self.user_id,
                auth_method: "pwd",
                amr: vec!["pwd".to_string()],
                acr: None,
                expected_credential_version: None,
                idle_minutes: session_cookie::IDLE_MINUTES,
                absolute_hours: session_cookie::ABSOLUTE_HOURS,
            },
        )
        .await
        .expect("create native session");
        session_cookie::set_cookie(&session.id, true)
            .split(';')
            .next()
            .expect("session cookie pair")
            .to_string()
    }

    async fn insert_grant(&self, scopes: &[&str]) {
        let scopes = sorted_scopes(scopes);
        self.db
            .execute(
                "INSERT INTO zeroship.oauth_grants \
                     (user_id, client_id, granted_scopes, granted_at, updated_at) \
                 VALUES ($1, $2, $3, NOW(), NOW()) \
                 ON CONFLICT (user_id, client_id) DO UPDATE \
                 SET granted_scopes = EXCLUDED.granted_scopes, updated_at = NOW()",
                &[&self.user_id, &self.client_id, &scopes],
            )
            .await
            .expect("insert grant");
    }
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn end_to_end_native_authorize_login_consent_token_flow() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let verifier = pkce_verifier();
    let authorize_path = authorize_path(&fx.client_id, "openid email", &verifier, "state-e2e", Some("nonce-e2e"));

    let resp = get(&fx, &authorize_path, None).await;
    assert_eq!(resp.status().as_u16(), 303);
    let login_loc = location(&resp);
    assert!(login_loc.starts_with("/login?return_to="), "login redirect: {login_loc}");
    assert_eq!(relative_query_param(&login_loc, "return_to").as_deref(), Some(authorize_path.as_str()));

    let login_get = get(&fx, &login_loc, None).await;
    assert_eq!(login_get.status().as_u16(), 200);
    let csrf = read_set_cookie(&login_get, "zsidp_csrf").expect("login csrf");
    let login_body = form(&[
        ("csrf", csrf.as_str()),
        ("email", fx.email.as_str()),
        ("password", PASSWORD),
        ("return_to", authorize_path.as_str()),
    ]);
    let login_post = post_form(&fx, "/login", &login_body, Some(&format!("zsidp_csrf={csrf}"))).await;
    assert_eq!(login_post.status().as_u16(), 303);
    assert_eq!(location(&login_post), authorize_path);
    let session = read_set_cookie(&login_post, "zsidp_session").expect("native session cookie");
    let cookies = format!("zsidp_session={session}");

    let after_login = get(&fx, &authorize_path, Some(&cookies)).await;
    assert_eq!(after_login.status().as_u16(), 303);
    let consent_loc = location(&after_login);
    assert!(consent_loc.starts_with("/consent?return_to="), "consent redirect: {consent_loc}");

    let consent_get = get(&fx, &consent_loc, Some(&cookies)).await;
    assert_eq!(consent_get.status().as_u16(), 200);
    let csrf = read_set_cookie(&consent_get, "zsidp_csrf").expect("consent csrf");
    let accept_body = form(&[
        ("csrf", csrf.as_str()),
        ("return_to", authorize_path.as_str()),
    ]);
    let accept = post_form(
        &fx,
        "/consent/accept",
        &accept_body,
        Some(&format!("{cookies}; zsidp_csrf={csrf}")),
    )
    .await;
    assert_eq!(accept.status().as_u16(), 303);
    assert_eq!(location(&accept), authorize_path);
    assert_grant_and_relay_alias(&fx).await;

    let final_authorize = get(&fx, &authorize_path, Some(&cookies)).await;
    assert_eq!(final_authorize.status().as_u16(), 303);
    let callback = location(&final_authorize);
    assert!(callback.starts_with(REDIRECT_URI), "callback: {callback}");
    let code = absolute_query_param(&callback, "code").expect("authorization code");
    assert_eq!(absolute_query_param(&callback, "state").as_deref(), Some("state-e2e"));
    assert_eq!(absolute_query_param(&callback, "iss").as_deref(), Some(ISSUER));

    let token = exchange_code(&fx, &code, &verifier).await;
    assert_eq!(token.token_type, "Bearer");
    assert!(token.scope.contains("openid"));
    assert!(!token.access_token.is_empty());
    assert!(!token.id_token.is_empty());

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn consent_covered_short_circuits_and_new_scope_bounces() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    fx.insert_grant(&["openid", "email"]).await;
    let cookies = fx.create_session_cookie().await;
    let verifier = pkce_verifier();
    let covered = authorize_path(&fx.client_id, "openid email", &verifier, "state-covered", Some("nonce-covered"));
    let resp = get(&fx, &covered, Some(&cookies)).await;
    assert_eq!(resp.status().as_u16(), 303);
    assert!(
        location(&resp).starts_with(REDIRECT_URI),
        "covered consent must issue code directly"
    );

    let new_scope = authorize_path(
        &fx.client_id,
        "openid email read:notes",
        &pkce_verifier(),
        "state-new-scope",
        Some("nonce-new"),
    );
    let resp = get(&fx, &new_scope, Some(&cookies)).await;
    assert_eq!(resp.status().as_u16(), 303);
    assert!(
        location(&resp).starts_with("/consent?return_to="),
        "new uncovered scope must prompt for consent"
    );

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn consent_deny_redirects_access_denied_to_registered_redirect_uri() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let cookies = fx.create_session_cookie().await;
    let verifier = pkce_verifier();
    let authorize_path = authorize_path(&fx.client_id, "openid email", &verifier, "state-deny", Some("nonce-deny"));
    let consent_loc = format!(
        "/consent?{}",
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("return_to", &authorize_path)
            .finish()
    );
    let consent_get = get(&fx, &consent_loc, Some(&cookies)).await;
    assert_eq!(consent_get.status().as_u16(), 200);
    let csrf = read_set_cookie(&consent_get, "zsidp_csrf").expect("csrf");
    let body = form(&[
        ("csrf", csrf.as_str()),
        ("return_to", authorize_path.as_str()),
    ]);
    let deny = post_form(
        &fx,
        "/consent/deny",
        &body,
        Some(&format!("{cookies}; zsidp_csrf={csrf}")),
    )
    .await;
    assert_eq!(deny.status().as_u16(), 303);
    let loc = location(&deny);
    assert!(loc.starts_with(REDIRECT_URI), "deny location: {loc}");
    assert_eq!(absolute_query_param(&loc, "error").as_deref(), Some("access_denied"));
    assert_eq!(absolute_query_param(&loc, "state").as_deref(), Some("state-deny"));

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn open_redirect_guards_keep_login_and_consent_on_safe_targets() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    // Each iteration consumes a LOGIN_EIP rate-limit token (capacity 5), so keep
    // this to ≤5 distinct full-login vectors. The exhaustive control-char matrix
    // (NUL/TAB/CR/LF/DEL) is unit-tested in `return_to::tests`; here we prove the
    // e2e WIRING: off-origin forms fall back, and one CRLF vector pins MED-1
    // (an interior CR/LF must not ride a "valid" return_to into the redirect).
    for bad in [
        "//evil.com",
        "https://evil.com",
        "/safe\\evil",
        "/me\r\nSet-Cookie: zs=1",
    ] {
        let login_get = get(&fx, &format!("/login?return_to={}", urlencoding(bad)), None).await;
        assert_eq!(login_get.status().as_u16(), 200);
        let csrf = read_set_cookie(&login_get, "zsidp_csrf").expect("csrf");
        let body = form(&[
            ("csrf", csrf.as_str()),
            ("email", fx.email.as_str()),
            ("password", PASSWORD),
            ("return_to", bad),
        ]);
        let post = post_form(&fx, "/login", &body, Some(&format!("zsidp_csrf={csrf}"))).await;
        assert_eq!(post.status().as_u16(), 303);
        assert_eq!(location(&post), "/me", "bad return_to {bad:?} must fall back");
    }

    let cookies = fx.create_session_cookie().await;
    let valid_path = authorize_path(&fx.client_id, "openid email", &pkce_verifier(), "state-open", Some("nonce-open"));
    let consent_get = get(
        &fx,
        &format!("/consent?{}", form(&[("return_to", valid_path.as_str())])),
        Some(&cookies),
    )
    .await;
    assert_eq!(consent_get.status().as_u16(), 200);
    let csrf = read_set_cookie(&consent_get, "zsidp_csrf").expect("csrf");
    let body = form(&[("csrf", csrf.as_str()), ("return_to", "https://evil.com")]);
    let accept = post_form(
        &fx,
        "/consent/accept",
        &body,
        Some(&format!("{cookies}; zsidp_csrf={csrf}")),
    )
    .await;
    // Rejected — never an off-origin redirect. The Location is the safe
    // fallback or an error page, but must never be the attacker's absolute URL.
    assert!(
        !location(&accept).starts_with("http"),
        "consent accept must not redirect off-origin: {}",
        location(&accept)
    );

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn consent_requires_session_and_csrf() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let authorize_path = authorize_path(&fx.client_id, "openid email", &pkce_verifier(), "state-gates", Some("nonce-gates"));
    let consent_loc = format!("/consent?{}", form(&[("return_to", authorize_path.as_str())]));
    let no_session = get(&fx, &consent_loc, None).await;
    assert_eq!(no_session.status().as_u16(), 303);
    assert!(location(&no_session).starts_with("/login?return_to="));

    let body = form(&[("return_to", authorize_path.as_str())]);
    let no_csrf = post_form(&fx, "/consent/accept", &body, None).await;
    assert_eq!(no_csrf.status().as_u16(), 403);

    let cookies = fx.create_session_cookie().await;
    let body = form(&[("csrf", "wrong"), ("return_to", authorize_path.as_str())]);
    let bad_csrf = post_form(&fx, "/consent/accept", &body, Some(&format!("{cookies}; zsidp_csrf=right"))).await;
    assert_eq!(bad_csrf.status().as_u16(), 403);

    // /consent/deny is equally CSRF-gated (LOW-3): a session with a mismatched
    // token is rejected, so a cross-site forced deny is impossible.
    let deny_bad_csrf = post_form(
        &fx,
        "/consent/deny",
        &form(&[("csrf", "wrong"), ("return_to", authorize_path.as_str())]),
        Some(&format!("{cookies}; zsidp_csrf=right")),
    )
    .await;
    assert_eq!(deny_bad_csrf.status().as_u16(), 403);

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn native_consent_rejects_scope_outside_client_registration() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let cookies = fx.create_session_cookie().await;
    // The client is registered for openid/profile/email only; `payments:charge`
    // is over-broad. `load_native_consent_context` must reject it (LOW-3) BEFORE
    // rendering the grant form, so an attacker-widened return_to can never put a
    // scope the client isn't allowed in front of the user to approve.
    let over_broad = authorize_path(
        &fx.client_id,
        "openid email payments:charge",
        &pkce_verifier(),
        "state-ob",
        Some("nonce-ob"),
    );
    let resp = get(
        &fx,
        &format!("/consent?{}", form(&[("return_to", over_broad.as_str())])),
        Some(&cookies),
    )
    .await;
    // The grant page sets a CSRF cookie; the rejection error page does not —
    // the absence of `zsidp_csrf` proves the consent form was NOT rendered.
    assert!(
        read_set_cookie(&resp, "zsidp_csrf").is_none(),
        "over-broad scope must render the error page, not the consent grant form"
    );
    // And it must never bounce to the RP carrying a code.
    assert_ne!(resp.status().as_u16(), 303);

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn totp_login_preserves_native_return_to() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let secret = totp::generate_secret();
    let key = totp::key_from_config(&fx.cfg.totp_enc_key).expect("totp key");
    totp_store::enroll(
        &fx.db,
        fx.user_id,
        &totp::encrypt_secret(&key, fx.user_id, &secret).expect("encrypt totp"),
    )
    .await
    .expect("enroll totp");
    let (_, hashes) = totp::generate_backup_codes().expect("backup codes");
    totp_store::confirm(&fx.db, fx.user_id, &hashes)
        .await
        .expect("confirm totp");

    let authorize_path = authorize_path(&fx.client_id, "openid email", &pkce_verifier(), "state-totp", Some("nonce-totp"));
    let login_get = get(&fx, &format!("/login?{}", form(&[("return_to", authorize_path.as_str())])), None).await;
    let csrf = read_set_cookie(&login_get, "zsidp_csrf").expect("login csrf");
    let body = form(&[
        ("csrf", csrf.as_str()),
        ("email", fx.email.as_str()),
        ("password", PASSWORD),
        ("return_to", authorize_path.as_str()),
    ]);
    let password_step = post_form(&fx, "/login", &body, Some(&format!("zsidp_csrf={csrf}"))).await;
    assert_eq!(password_step.status().as_u16(), 200);
    assert!(
        read_set_cookie(&password_step, "zsidp_session").is_none(),
        "password step must not mint a session before TOTP"
    );
    let csrf = read_set_cookie(&password_step, "zsidp_csrf").expect("totp csrf");
    let stash = read_set_cookie(&password_step, "zsidp_2fa").expect("totp stash");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let code = totp::code_at(&secret, now);
    let body = form(&[
        ("csrf", csrf.as_str()),
        ("code", code.as_str()),
        ("return_to", authorize_path.as_str()),
    ]);
    let done = post_form(
        &fx,
        "/login/2fa",
        &body,
        Some(&format!("zsidp_csrf={csrf}; zsidp_2fa={stash}")),
    )
    .await;
    assert_eq!(done.status().as_u16(), 303);
    assert_eq!(location(&done), authorize_path);
    assert!(read_set_cookie(&done, "zsidp_session").is_some());

    fx.cleanup().await;
}

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("CONTROL_TEST_DB"))
        .ok()
}

fn test_issuer() -> Issuer {
    let signing = SigningKey::from_bytes(&[42u8; 32]);
    Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string()).expect("issuer")
}

async fn seed_user_client(db: &Client, user_id: Uuid, app_id: Uuid, client_id: &str, email: &str) {
    let phc = password::hash(PASSWORD).expect("password hash");
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name, password_hash) \
         VALUES ($1, $2::citext, NOW(), 'P4 User', $3)",
        &[&user_id, &email, &phc],
    )
    .await
    .expect("seed user");
    db.execute(
        "INSERT INTO zeroship.plans \
            (id, name, runtime_limits_json, assignable_by_creator) \
         VALUES ('free', 'Free', '{}'::jsonb, TRUE) \
         ON CONFLICT (id) DO NOTHING",
        &[],
    )
    .await
    .expect("seed free plan");
    db.execute(
        "INSERT INTO zeroship.apps (id, name, api_key, api_key_hash) \
         VALUES ($1, $2, $3, $4)",
        &[
            &app_id,
            &format!("p4-native-app-{}", app_id.simple()),
            &format!("api-{app_id}"),
            &format!("hash-{app_id}"),
        ],
    )
    .await
    .expect("seed app");
    let scopes = vec![
        "openid".to_string(),
        "profile".to_string(),
        "email".to_string(),
        "read:notes".to_string(),
    ];
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id) \
         VALUES ($1, 'P4 native OP test', $2, $3, FALSE, $1)",
        &[&client_id, &vec![REDIRECT_URI.to_string()], &scopes],
    )
    .await
    .expect("seed oauth client");
    db.execute(
        "INSERT INTO zeroship.app_oauth_clients (app_id, client_id, sector_identifier) \
         VALUES ($1, $2, $3)",
        &[&app_id, &client_id, &SECTOR],
    )
    .await
    .expect("seed app oauth client");
    db.execute(
        "INSERT INTO zeroship.app_scope_defs (app_id, scope_id, label, description) \
         VALUES ($1, 'read:notes', 'Read notes', 'Read your notes')",
        &[&app_id],
    )
    .await
    .expect("seed app scope def");
    db.execute(
        "INSERT INTO zeroship.app_user_identities (app_client_id, global_user_id, pairwise_sub) \
         VALUES ($1, $2, $3)",
        &[&client_id, &user_id, &format!("pws_test_{}", user_id.simple())],
    )
    .await
    .expect("seed app identity");
}

async fn cleanup_seeded_rows(db: &Client, user_id: Uuid, app_id: Uuid, client_id: &str) {
    let _ = db
        .execute("DELETE FROM zeroship.oauth_authorization_codes WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.oauth_grants WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.app_user_identities WHERE app_client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.idp_sessions WHERE user_id = $1", &[&user_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.app_oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await;
}

fn authorize_path(client_id: &str, scope: &str, verifier: &str, state: &str, nonce: Option<&str>) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("scope", scope)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("state", state)
        .append_pair("code_challenge", &pkce_challenge_s256(verifier))
        .append_pair("code_challenge_method", "S256");
    if let Some(nonce) = nonce {
        serializer.append_pair("nonce", nonce);
    }
    format!("/authorize?{}", serializer.finish())
}

#[allow(clippy::future_not_send)]
async fn get(fx: &Fixture, path: &str, cookie: Option<&str>) -> cyper::Response {
    let mut req = fx
        .http
        .request(Method::GET, format!("{}{}", fx.auth_base, path))
        .expect("build GET");
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie).expect("cookie");
    }
    req.send().await.expect("send GET")
}

#[allow(clippy::future_not_send)]
async fn post_form(fx: &Fixture, path: &str, body: &str, cookie: Option<&str>) -> cyper::Response {
    let mut req = fx
        .http
        .request(Method::POST, format!("{}{}", fx.auth_base, path))
        .expect("build POST")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type");
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie).expect("cookie");
    }
    req.body(body.to_string()).send().await.expect("send POST")
}

#[allow(clippy::future_not_send)]
async fn exchange_code(fx: &Fixture, code: &str, verifier: &str) -> TokenResponse {
    let body = form(&[
        ("grant_type", "authorization_code"),
        ("client_id", fx.client_id.as_str()),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("code_verifier", verifier),
    ]);
    let resp = post_form(fx, "/token", &body, None).await;
    assert_eq!(resp.status().as_u16(), 200, "token status");
    resp.json::<TokenResponse>().await.expect("token json")
}

async fn assert_grant_and_relay_alias(fx: &Fixture) {
    let row = fx
        .db
        .query_one(
            "SELECT granted_scopes FROM zeroship.oauth_grants \
             WHERE user_id = $1 AND client_id = $2",
            &[&fx.user_id, &fx.client_id],
        )
        .await
        .expect("grant row");
    let scopes: Vec<String> = row.get("granted_scopes");
    assert!(scopes.contains(&"openid".to_string()));
    assert!(scopes.contains(&"email".to_string()));
    let alias: Option<String> = fx
        .db
        .query_one(
            "SELECT relay_email FROM zeroship.app_user_identities \
             WHERE app_client_id = $1 AND global_user_id = $2",
            &[&fx.client_id, &fx.user_id],
        )
        .await
        .expect("identity row")
        .get("relay_email");
    assert!(alias.as_deref().is_some_and(|value| value.ends_with("@relay.zeroship.localhost")));
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

fn sorted_scopes(scopes: &[&str]) -> Vec<String> {
    let mut scopes = scopes.iter().map(|scope| (*scope).to_string()).collect::<Vec<_>>();
    scopes.sort();
    scopes.dedup();
    scopes
}

fn relative_query_param(raw: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(&format!("http://auth.zeroship.test{raw}")).ok()?;
    parsed.query_pairs().find_map(|(name, value)| {
        if name == key {
            Some(value.into_owned())
        } else {
            None
        }
    })
}

fn absolute_query_param(raw: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(raw).ok()?;
    parsed.query_pairs().find_map(|(name, value)| {
        if name == key {
            Some(value.into_owned())
        } else {
            None
        }
    })
}

fn urlencoding(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[allow(dead_code)]
fn code_hash(code: &str) -> Vec<u8> {
    Sha256::digest(code.as_bytes()).to_vec()
}
