//! P3 closed-world `/authorize` + `/token` auth-code + PKCE tests.

mod common;

use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use ntex::web;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::oidc::issuer::oidc_at_hash;
use zeroship_auth::oidc::metadata::jwks_document;
use zeroship_auth::oidc::{
    AccessTokenClaims, IdTokenClaims, Issuer, ACCESS_TOKEN_TYP, ID_TOKEN_TYP,
};
use zeroship_auth::server;
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::sessions as session_store;

use common::{location, pkce_challenge_s256, pkce_verifier, test_auth_config};

const ISSUER: &str = "https://auth.zeroship.test/oauth2";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/cb";
const SECTOR: &str = "https://app-auth-code.zeroship.test";

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    id_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
}

struct SeededUserProfile {
    email: String,
    name: String,
    avatar_url: String,
}

struct Fixture {
    srv: ntex::web::test::TestServer,
    auth_base: String,
    db: Arc<Client>,
    issuer: Arc<Issuer>,
    client_id: String,
    app_id: Uuid,
    user_id: Uuid,
    user_email: String,
    user_name: String,
    user_avatar_url: String,
    session_id: Uuid,
    session_cookie: String,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot() -> Option<Self> {
        Self::boot_with_email_verified(true).await
    }

    #[allow(clippy::future_not_send)]
    async fn boot_with_email_verified(email_verified: bool) -> Option<Self> {
        let Some(db_url) = db_url() else {
            eprintln!("[op_authorization_code_test] skip (AUTH_DB_URL or CONTROL_TEST_DB unset)");
            return None;
        };
        let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(err) = pg_connection.run().await {
                eprintln!("[op_authorization_code_test] pg connection error: {err}");
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
        let client_id = format!("oac_p3_{}", Uuid::new_v4().simple());
        let app_name = format!("p3-auth-code-{}", Uuid::new_v4().simple());
        let user_profile =
            seed_user_client(&db, user_id, app_id, &app_name, &client_id, email_verified).await;
        let session = session_store::create(
            &db,
            &session_store::CreateSession {
                user_id,
                auth_method: "pwd",
                amr: vec!["pwd".to_string()],
                acr: None,
                expected_credential_version: None,
                idle_minutes: zeroship_auth::sessions::login::IDLE_MINUTES,
                absolute_hours: zeroship_auth::sessions::login::ABSOLUTE_HOURS,
            },
        )
        .await
        .expect("create idp session");
        let session_cookie = session_cookie::set_cookie(&session.id, true)
            .split(';')
            .next()
            .expect("cookie pair")
            .to_string();

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
            issuer,
            client_id,
            app_id,
            user_id,
            user_email: user_profile.email,
            user_name: user_profile.name,
            user_avatar_url: user_profile.avatar_url,
            session_id: session.id,
            session_cookie,
        })
    }

    async fn cleanup(self) {
        cleanup_seeded_rows(&self.db, self.user_id, self.app_id, &self.client_id).await;
        drop(self.srv);
    }
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn authorize_token_happy_path_mints_pairwise_access_and_nonce_at_hash_id_token() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let verifier = pkce_verifier();
    let nonce = format!("nc-{}", Uuid::new_v4().simple());

    let authorize = send_authorize(&fx, REDIRECT_URI, &verifier, Some(&nonce), None)
        .await
        .expect("authorize response");
    assert_eq!(authorize.status().as_u16(), 303, "authorize must use 303");
    assert_eq!(
        authorize
            .headers()
            .get("referrer-policy")
            .and_then(|value| value.to_str().ok()),
        Some("no-referrer")
    );
    let loc = location(&authorize);
    let code = query_param(&loc, "code").expect("code in redirect");
    assert_eq!(query_param(&loc, "state").as_deref(), Some("state-123"));
    assert_eq!(query_param(&loc, "iss").as_deref(), Some(fx.issuer.issuer()));

    let token = exchange_code(&fx, &code, REDIRECT_URI, &verifier)
        .await
        .expect("token response");
    assert_eq!(token.token_type, "Bearer");
    assert!(token.expires_in > 0);
    assert!(token.scope.contains("openid"));

    let jwks = jwks_document(&fx.db).await.expect("jwks");
    let access = verify_with_jwks::<AccessTokenClaims>(
        &jwks,
        &token.access_token,
        fx.issuer.issuer(),
        &format!("app:{}", fx.app_id),
        ACCESS_TOKEN_TYP,
    )
    .expect("verify access token");
    assert_eq!(access.client_id, fx.client_id);
    assert_eq!(
        access.sub,
        fx.issuer.pairwise_subject(&fx.user_id.to_string(), SECTOR)
    );

    let id = verify_with_jwks::<IdTokenClaims>(
        &jwks,
        &token.id_token,
        fx.issuer.issuer(),
        &fx.client_id,
        ID_TOKEN_TYP,
    )
    .expect("verify id token");
    assert_eq!(id.sub, access.sub);
    assert_eq!(id.sid, fx.session_id.to_string());
    assert_eq!(id.nonce, nonce);
    assert_eq!(id.at_hash, oidc_at_hash(&token.access_token));

    let replay = token_request(&fx, &code, REDIRECT_URI, &verifier)
        .await
        .expect("second token response");
    assert_eq!(replay.status().as_u16(), 400, "code must be one-use");
    assert_error(replay, "invalid_grant").await;

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn id_token_includes_email_and_profile_claims_when_scopes_granted() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let verifier = pkce_verifier();
    let nonce = format!("nc-{}", Uuid::new_v4().simple());

    let authorize = send_authorize_with_scope(
        &fx,
        REDIRECT_URI,
        &verifier,
        Some(&nonce),
        "openid email profile",
    )
    .await
    .expect("authorize response");
    assert_eq!(authorize.status().as_u16(), 303);
    let code = query_param(&location(&authorize), "code").expect("code in redirect");
    let token = exchange_code(&fx, &code, REDIRECT_URI, &verifier)
        .await
        .expect("token response");

    let jwks = jwks_document(&fx.db).await.expect("jwks");
    let id = verify_with_jwks::<Value>(
        &jwks,
        &token.id_token,
        fx.issuer.issuer(),
        &fx.client_id,
        ID_TOKEN_TYP,
    )
    .expect("verify id token");
    assert_eq!(id["email"].as_str(), Some(fx.user_email.as_str()));
    assert_eq!(id["email_verified"].as_bool(), Some(true));
    assert_eq!(id["name"].as_str(), Some(fx.user_name.as_str()));
    assert_eq!(id["picture"].as_str(), Some(fx.user_avatar_url.as_str()));

    let access = verify_with_jwks::<Value>(
        &jwks,
        &token.access_token,
        fx.issuer.issuer(),
        &format!("app:{}", fx.app_id),
        ACCESS_TOKEN_TYP,
    )
    .expect("verify access token");
    assert_identity_claims_absent(&access);

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn id_token_omits_identity_claims_without_email_and_profile_scopes() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let verifier = pkce_verifier();
    let nonce = format!("nc-{}", Uuid::new_v4().simple());

    let authorize = send_authorize_with_scope(&fx, REDIRECT_URI, &verifier, Some(&nonce), "openid")
        .await
        .expect("authorize response");
    assert_eq!(authorize.status().as_u16(), 303);
    let code = query_param(&location(&authorize), "code").expect("code in redirect");
    let token = exchange_code(&fx, &code, REDIRECT_URI, &verifier)
        .await
        .expect("token response");
    assert_eq!(token.scope, "openid");

    let jwks = jwks_document(&fx.db).await.expect("jwks");
    let id = verify_with_jwks::<Value>(
        &jwks,
        &token.id_token,
        fx.issuer.issuer(),
        &fx.client_id,
        ID_TOKEN_TYP,
    )
    .expect("verify id token");
    assert_identity_claims_absent(&id);

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn id_token_email_verified_false_for_unverified_user() {
    let Some(fx) = Fixture::boot_with_email_verified(false).await else {
        return;
    };
    let verifier = pkce_verifier();
    let nonce = format!("nc-{}", Uuid::new_v4().simple());

    let authorize =
        send_authorize_with_scope(&fx, REDIRECT_URI, &verifier, Some(&nonce), "openid email")
            .await
            .expect("authorize response");
    assert_eq!(authorize.status().as_u16(), 303);
    let code = query_param(&location(&authorize), "code").expect("code in redirect");
    let token = exchange_code(&fx, &code, REDIRECT_URI, &verifier)
        .await
        .expect("token response");

    let jwks = jwks_document(&fx.db).await.expect("jwks");
    let id = verify_with_jwks::<Value>(
        &jwks,
        &token.id_token,
        fx.issuer.issuer(),
        &fx.client_id,
        ID_TOKEN_TYP,
    )
    .expect("verify id token");
    assert_eq!(id["email"].as_str(), Some(fx.user_email.as_str()));
    assert_eq!(id["email_verified"].as_bool(), Some(false));
    assert!(!id.as_object().expect("claims object").contains_key("name"));
    assert!(!id.as_object().expect("claims object").contains_key("picture"));

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn pkce_negatives_missing_plain_and_wrong_verifier_are_rejected() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let verifier = pkce_verifier();
    let nonce = format!("nc-{}", Uuid::new_v4().simple());

    let missing = send_authorize_without_pkce(&fx, REDIRECT_URI, Some(&nonce))
        .await
        .expect("missing pkce authorize response");
    assert_eq!(missing.status().as_u16(), 400);
    assert_error(missing, "invalid_request").await;

    let plain = send_authorize_with_method(&fx, REDIRECT_URI, &verifier, "plain", Some(&nonce))
        .await
        .expect("plain authorize response");
    assert_eq!(plain.status().as_u16(), 400);
    assert_error(plain, "invalid_request").await;

    let authorize = send_authorize(&fx, REDIRECT_URI, &verifier, Some(&nonce), None)
        .await
        .expect("valid authorize");
    let code = query_param(&location(&authorize), "code").expect("code");
    let wrong = pkce_verifier();
    let token = token_request(&fx, &code, REDIRECT_URI, &wrong)
        .await
        .expect("wrong verifier token response");
    assert_eq!(token.status().as_u16(), 400);
    assert_error(token, "invalid_grant").await;

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn redirect_uri_must_exact_match_registered_value() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let verifier = pkce_verifier();
    let nonce = format!("nc-{}", Uuid::new_v4().simple());

    for bad_redirect in [
        "http://127.0.0.1:9999/cb/extra",
        "http://127.0.0.1:9999/c",
        "http://127.0.0.1:9999/cb?next=/extra",
    ] {
        let resp = send_authorize(&fx, bad_redirect, &verifier, Some(&nonce), None)
            .await
            .expect("redirect mismatch response");
        assert_eq!(resp.status().as_u16(), 400, "{bad_redirect}");
        assert!(
            resp.headers().get("location").is_none(),
            "redirect mismatch must not redirect"
        );
        assert_error(resp, "invalid_request").await;
    }

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn expired_authorization_code_is_rejected() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let verifier = pkce_verifier();
    let nonce = format!("nc-{}", Uuid::new_v4().simple());
    let authorize = send_authorize(&fx, REDIRECT_URI, &verifier, Some(&nonce), None)
        .await
        .expect("authorize response");
    let code = query_param(&location(&authorize), "code").expect("code");
    expire_code(&fx.db, &code).await;

    let token = token_request(&fx, &code, REDIRECT_URI, &verifier)
        .await
        .expect("expired token response");
    assert_eq!(token.status().as_u16(), 400);
    assert_error(token, "invalid_grant").await;

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

async fn seed_user_client(
    db: &Client,
    user_id: Uuid,
    app_id: Uuid,
    app_name: &str,
    client_id: &str,
    email_verified: bool,
) -> SeededUserProfile {
    let email = format!("p3-{}@zeroship.test", Uuid::new_v4().simple());
    let name = format!("P3 User {}", Uuid::new_v4().simple());
    let avatar_url = format!("https://cdn.zeroship.test/avatars/{user_id}.png");
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name, avatar_url) \
         VALUES ($1, $2::citext, CASE WHEN $3 THEN NOW() ELSE NULL END, $4, $5)",
        &[&user_id, &email, &email_verified, &name, &avatar_url],
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
            &app_name,
            &format!("api-{app_id}"),
            &format!("hash-{app_id}"),
        ],
    )
    .await
    .expect("seed app");
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id) \
         VALUES ($1, 'P3 OP test', $2, $3, FALSE, $1)",
        &[
            &client_id,
            &vec![REDIRECT_URI.to_string()],
            &vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string(),
            ],
        ],
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
        "INSERT INTO zeroship.oauth_grants \
             (user_id, client_id, granted_scopes, granted_at, updated_at) \
         VALUES ($1, $2, $3, NOW(), NOW())",
        &[
            &user_id,
            &client_id,
            &vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string(),
            ],
        ],
    )
    .await
    .expect("seed oauth grant");
    SeededUserProfile {
        email,
        name,
        avatar_url,
    }
}

async fn cleanup_seeded_rows(db: &Client, user_id: Uuid, app_id: Uuid, client_id: &str) {
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_authorization_codes WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_grants WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.app_user_identities WHERE app_client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&user_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.app_oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await;
}

#[allow(clippy::future_not_send)]
async fn send_authorize(
    fx: &Fixture,
    redirect_uri: &str,
    verifier: &str,
    nonce: Option<&str>,
    state: Option<&str>,
) -> Result<cyper::Response, cyper::Error> {
    send_authorize_with_scope_and_method(
        fx,
        redirect_uri,
        verifier,
        "S256",
        nonce.or(Some("nonce-123")),
        "openid profile email",
    )
    .await
    .map(|resp| {
        let _ = state;
        resp
    })
}

#[allow(clippy::future_not_send)]
async fn send_authorize_with_scope(
    fx: &Fixture,
    redirect_uri: &str,
    verifier: &str,
    nonce: Option<&str>,
    scope: &str,
) -> Result<cyper::Response, cyper::Error> {
    send_authorize_with_scope_and_method(fx, redirect_uri, verifier, "S256", nonce, scope)
        .await
}

#[allow(clippy::future_not_send)]
async fn send_authorize_with_method(
    fx: &Fixture,
    redirect_uri: &str,
    verifier: &str,
    method: &str,
    nonce: Option<&str>,
) -> Result<cyper::Response, cyper::Error> {
    send_authorize_with_scope_and_method(
        fx,
        redirect_uri,
        verifier,
        method,
        nonce,
        "openid profile email",
    )
    .await
}

#[allow(clippy::future_not_send)]
async fn send_authorize_with_scope_and_method(
    fx: &Fixture,
    redirect_uri: &str,
    verifier: &str,
    method: &str,
    nonce: Option<&str>,
    scope: &str,
) -> Result<cyper::Response, cyper::Error> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer
        .append_pair("client_id", &fx.client_id)
        .append_pair("response_type", "code")
        .append_pair("scope", scope)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", "state-123")
        .append_pair("code_challenge", &pkce_challenge_s256(verifier))
        .append_pair("code_challenge_method", method);
    if let Some(nonce) = nonce {
        serializer.append_pair("nonce", nonce);
    }
    let url = format!("{}/oauth2/authorize?{}", fx.auth_base, serializer.finish());
    cyper::Client::new()
        .request(http::Method::GET, url)
        .expect("build GET /authorize")
        .header("cookie", fx.session_cookie.clone())
        .expect("cookie")
        .send()
        .await
}

#[allow(clippy::future_not_send)]
async fn send_authorize_without_pkce(
    fx: &Fixture,
    redirect_uri: &str,
    nonce: Option<&str>,
) -> Result<cyper::Response, cyper::Error> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer
        .append_pair("client_id", &fx.client_id)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid profile email")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", "state-123")
        .append_pair("code_challenge_method", "S256");
    if let Some(nonce) = nonce {
        serializer.append_pair("nonce", nonce);
    }
    let url = format!("{}/oauth2/authorize?{}", fx.auth_base, serializer.finish());
    cyper::Client::new()
        .request(http::Method::GET, url)
        .expect("build GET /authorize without pkce")
        .header("cookie", fx.session_cookie.clone())
        .expect("cookie")
        .send()
        .await
}

#[allow(clippy::future_not_send)]
async fn exchange_code(
    fx: &Fixture,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenResponse, cyper::Error> {
    let resp = token_request(fx, code, redirect_uri, verifier).await?;
    assert_eq!(resp.status().as_u16(), 200, "token response status");
    resp.json::<TokenResponse>().await
}

#[allow(clippy::future_not_send)]
async fn token_request(
    fx: &Fixture,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<cyper::Response, cyper::Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", &fx.client_id)
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_verifier", verifier)
        .finish();
    cyper::Client::new()
        .request(http::Method::POST, format!("{}/oauth2/token", fx.auth_base))
        .expect("build POST /token")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(body)
        .send()
        .await
}

async fn expire_code(db: &Client, code: &str) {
    let hash = code_hash(code);
    db.execute(
        "UPDATE zeroship.oauth_authorization_codes \
         SET expires_at = NOW() - INTERVAL '1 second' \
         WHERE code_hash = $1",
        &[&hash],
    )
    .await
    .expect("expire code");
}

fn query_param(raw_url: &str, name: &str) -> Option<String> {
    url::Url::parse(raw_url).ok()?.query_pairs().find_map(|(key, value)| {
        if key == name {
            Some(value.into_owned())
        } else {
            None
        }
    })
}

fn code_hash(code: &str) -> Vec<u8> {
    Sha256::digest(code.as_bytes()).to_vec()
}

async fn assert_error(resp: cyper::Response, expected: &str) {
    let body = resp
        .json::<serde_json::Value>()
        .await
        .expect("oauth error json");
    assert_eq!(body["error"], expected);
}

fn assert_identity_claims_absent(claims: &Value) {
    let object = claims.as_object().expect("claims object");
    for claim in ["email", "email_verified", "name", "picture"] {
        assert!(
            !object.contains_key(claim),
            "identity claim {claim} must be absent"
        );
    }
}

fn verify_with_jwks<T: DeserializeOwned>(
    jwks: &Value,
    token: &str,
    expected_iss: &str,
    expected_aud: &str,
    expected_typ: &str,
) -> Result<T, String> {
    let header = decode_header(token).map_err(|err| format!("decode header: {err}"))?;
    if header.alg != Algorithm::EdDSA {
        return Err(format!("unexpected alg {:?}", header.alg));
    }
    if header.typ.as_deref() != Some(expected_typ) {
        return Err(format!("unexpected typ {:?}", header.typ));
    }
    let kid = header.kid.ok_or_else(|| "missing kid".to_string())?;
    let key = jwks["keys"]
        .as_array()
        .ok_or_else(|| "jwks keys is not an array".to_string())?
        .iter()
        .find(|key| key["kid"] == kid && key["alg"] == "EdDSA" && key["kty"] == "OKP")
        .ok_or_else(|| format!("no matching EdDSA key {kid}"))?;
    let x = key["x"]
        .as_str()
        .ok_or_else(|| format!("key {kid} missing x"))?;
    let decoding = DecodingKey::from_ed_components(x).map_err(|err| format!("ed key: {err}"))?;

    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[expected_iss]);
    validation.set_audience(&[expected_aud]);
    decode::<T>(token, &decoding, &validation)
        .map(|data| data.claims)
        .map_err(|err| format!("jwt verify: {err}"))
}
