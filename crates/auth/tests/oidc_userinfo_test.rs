//! OIDC Core §5.3 `/oauth2/userinfo` tests.

mod common;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use ntex::web;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::oidc::metadata::jwks_document;
use zeroship_auth::oidc::{
    AccessTokenMint, IdTokenClaims, Issuer, ACCESS_TOKEN_TYP, ID_TOKEN_TYP,
};
use zeroship_auth::server;
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::sessions as session_store;

use common::{location, pkce_challenge_s256, pkce_verifier, test_auth_config};

const ISSUER: &str = "https://auth.zeroship.test/oauth2";
const WRONG_ISSUER: &str = "https://wrong-auth.zeroship.test";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/cb";
const SECTOR: &str = "https://app-userinfo.zeroship.test";

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    id_token: String,
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
    session_cookie: String,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot() -> Option<Self> {
        let Some(db_url) = db_url() else {
            eprintln!("[oidc_userinfo_test] skip (AUTH_DB_URL or CONTROL_TEST_DB unset)");
            return None;
        };
        let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(err) = pg_connection.run().await {
                eprintln!("[oidc_userinfo_test] pg connection error: {err}");
            }
        })
        .detach();
        let db = Arc::new(pg_client);

        let issuer = Arc::new(test_issuer(ISSUER));
        issuer
            .publish_active_key(&db)
            .await
            .expect("publish active OP key");

        let user_id = Uuid::new_v4();
        let app_id = Uuid::new_v4();
        let client_id = format!("oac_userinfo_{}", Uuid::new_v4().simple());
        let app_name = format!("userinfo-{}", Uuid::new_v4().simple());
        let user_profile =
            seed_user_client(&db, &issuer, user_id, app_id, &app_name, &client_id).await;
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

        let cfg = Arc::new(test_auth_config(&db_url));
        let cfg_state = cfg.clone();
        let db_state = db.clone();
        let issuer_state = issuer.clone();
        let refresh_pool_state =
            zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
        let srv = web::test::server(move || {
            let cfg_state = cfg_state.clone();
            let db_state = db_state.clone();
            let issuer_state = issuer_state.clone();
            let refresh_pool_state = refresh_pool_state.clone();
            async move {
                web::App::new()
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
async fn userinfo_returns_scope_gated_claims_for_valid_token() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let token = issue_token(&fx, "openid email profile").await;
    let jwks = jwks_document(&fx.db).await.expect("jwks");
    let id = verify_with_jwks::<IdTokenClaims>(
        &jwks,
        &token.id_token,
        fx.issuer.issuer(),
        &fx.client_id,
        ID_TOKEN_TYP,
    )
    .expect("verify id token");

    let resp = userinfo_get(&fx, Some(&token.access_token))
        .await
        .expect("GET /userinfo");
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    assert!(
        resp.headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
    );
    let body = resp.json::<Value>().await.expect("userinfo json");
    assert_eq!(body["sub"].as_str(), Some(id.sub.as_str()));
    assert_eq!(body["email"].as_str(), Some(fx.user_email.as_str()));
    assert_eq!(body["email_verified"].as_bool(), Some(true));
    assert_eq!(body["name"].as_str(), Some(fx.user_name.as_str()));
    assert_eq!(body["picture"].as_str(), Some(fx.user_avatar_url.as_str()));

    let post = userinfo_post(&fx, &token.access_token)
        .await
        .expect("POST /userinfo");
    assert_eq!(post.status().as_u16(), 200);
    let post_body = post.json::<Value>().await.expect("userinfo POST json");
    assert_eq!(post_body["sub"].as_str(), Some(id.sub.as_str()));
    assert_eq!(post_body["email"].as_str(), Some(fx.user_email.as_str()));

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_omits_identity_claims_without_scopes() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let token = issue_token(&fx, "openid").await;
    let jwks = jwks_document(&fx.db).await.expect("jwks");
    let id = verify_with_jwks::<IdTokenClaims>(
        &jwks,
        &token.id_token,
        fx.issuer.issuer(),
        &fx.client_id,
        ID_TOKEN_TYP,
    )
    .expect("verify id token");

    let resp = userinfo_get(&fx, Some(&token.access_token))
        .await
        .expect("GET /userinfo");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.json::<Value>().await.expect("userinfo json");
    assert_eq!(body["sub"].as_str(), Some(id.sub.as_str()));
    assert_identity_claims_absent(&body);

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_rejects_missing_and_bad_tokens() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };

    let missing = userinfo_get(&fx, None).await.expect("missing bearer");
    assert_missing_token(&missing);

    let garbage = userinfo_get_with_authorization(&fx, "Bearer not-a-jwt")
        .await
        .expect("garbage bearer");
    assert_invalid_token(garbage).await;

    let token = issue_token(&fx, "openid").await;
    let tampered = userinfo_get(&fx, Some(&tamper_token(&token.access_token)))
        .await
        .expect("tampered bearer");
    assert_invalid_token(tampered).await;

    let alg_none = userinfo_get(
        &fx,
        Some(&unsigned_none_token(&access_claims(&fx), fx.issuer.kid())),
    )
    .await
    .expect("alg none bearer");
    assert_invalid_token(alg_none).await;

    let expired = userinfo_get(&fx, Some(&expired_access_token(&fx)))
        .await
        .expect("expired bearer");
    assert_invalid_token(expired).await;

    let wrong_issuer = userinfo_get(&fx, Some(&wrong_issuer_access_token(&fx)))
        .await
        .expect("wrong issuer bearer");
    assert_invalid_token(wrong_issuer).await;

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_rejects_id_token_used_as_access_token() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let token = issue_token(&fx, "openid email profile").await;

    let resp = userinfo_get(&fx, Some(&token.id_token))
        .await
        .expect("id token as bearer");
    assert_invalid_token(resp).await;

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_rejects_token_without_openid_scope() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    // A validly-signed OP access token minted for the app resource audience but
    // WITHOUT `openid` must not be usable as an identity oracle (OIDC Core §5.3).
    let access_token = access_token_with_scopes(&fx, &fx.issuer, &["email", "profile"], Some(600));

    let resp = userinfo_get(&fx, Some(&access_token))
        .await
        .expect("no-openid bearer");
    assert_insufficient_scope(&resp);

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_rejects_disabled_user() {
    let Some(fx) = Fixture::boot().await else {
        return;
    };
    let token = issue_token(&fx, "openid email profile").await;

    // Disable the account AFTER the token was minted: a still-live access token
    // must stop leaking identity once the user is terminated (MED-2).
    fx.db
        .execute(
            "UPDATE zeroship.users SET disabled_at = NOW() WHERE id = $1",
            &[&fx.user_id],
        )
        .await
        .expect("disable user");

    let resp = userinfo_get(&fx, Some(&token.access_token))
        .await
        .expect("disabled-user bearer");
    assert_invalid_token(resp).await;

    fx.cleanup().await;
}

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("CONTROL_TEST_DB"))
        .ok()
}

fn test_issuer(issuer: &str) -> Issuer {
    let signing = SigningKey::from_bytes(&[42u8; 32]);
    Issuer::from_signing_key(&signing, [9u8; 32], issuer.to_string()).expect("issuer")
}

#[allow(clippy::future_not_send)]
async fn seed_user_client(
    db: &Client,
    issuer: &Issuer,
    user_id: Uuid,
    app_id: Uuid,
    app_name: &str,
    client_id: &str,
) -> SeededUserProfile {
    let email = format!("userinfo-{}@zeroship.test", Uuid::new_v4().simple());
    let name = format!("UserInfo User {}", Uuid::new_v4().simple());
    let avatar_url = format!("https://cdn.zeroship.test/avatars/{user_id}.png");
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name, avatar_url) \
         VALUES ($1, $2::citext, NOW(), $3, $4)",
        &[&user_id, &email, &name, &avatar_url],
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
            (client_id, client_name, redirect_uris, scopes, skip_consent) \
         VALUES ($1, 'UserInfo OP test', $2, $3, FALSE)",
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

    let pairwise_sub = issuer.pairwise_subject(&user_id.to_string(), SECTOR);
    db.execute(
        "INSERT INTO zeroship.app_user_identities \
            (app_client_id, global_user_id, pairwise_sub) \
         VALUES ($1, $2, $3)",
        &[&client_id, &user_id, &pairwise_sub],
    )
    .await
    .expect("seed app user identity");
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
async fn issue_token(fx: &Fixture, scope: &str) -> TokenResponse {
    let verifier = pkce_verifier();
    let nonce = format!("nc-{}", Uuid::new_v4().simple());
    let authorize = send_authorize_with_scope(fx, REDIRECT_URI, &verifier, Some(&nonce), scope)
        .await
        .expect("authorize response");
    assert_eq!(authorize.status().as_u16(), 303);
    let code = query_param(&location(&authorize), "code").expect("code in redirect");
    exchange_code(fx, &code, REDIRECT_URI, &verifier)
        .await
        .expect("token response")
}

#[allow(clippy::future_not_send)]
async fn send_authorize_with_scope(
    fx: &Fixture,
    redirect_uri: &str,
    verifier: &str,
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
        .append_pair("code_challenge_method", "S256");
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
async fn exchange_code(
    fx: &Fixture,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenResponse, cyper::Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", &fx.client_id)
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_verifier", verifier)
        .finish();
    let resp = cyper::Client::new()
        .request(http::Method::POST, format!("{}/oauth2/token", fx.auth_base))
        .expect("build POST /token")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(body)
        .send()
        .await?;
    assert_eq!(resp.status().as_u16(), 200, "token response status");
    resp.json::<TokenResponse>().await
}

#[allow(clippy::future_not_send)]
async fn userinfo_get(
    fx: &Fixture,
    token: Option<&str>,
) -> Result<cyper::Response, cyper::Error> {
    let authorization = token.map(|token| format!("Bearer {token}"));
    userinfo_get_with_authorization(fx, authorization.as_deref().unwrap_or("")).await
}

#[allow(clippy::future_not_send)]
async fn userinfo_get_with_authorization(
    fx: &Fixture,
    authorization: &str,
) -> Result<cyper::Response, cyper::Error> {
    let req = cyper::Client::new()
        .request(http::Method::GET, format!("{}/oauth2/userinfo", fx.auth_base))
        .expect("build GET /userinfo");
    let req = if authorization.is_empty() {
        req
    } else {
        req.header("authorization", authorization)
            .expect("authorization")
    };
    req.send().await
}

#[allow(clippy::future_not_send)]
async fn userinfo_post(fx: &Fixture, token: &str) -> Result<cyper::Response, cyper::Error> {
    cyper::Client::new()
        .request(http::Method::POST, format!("{}/oauth2/userinfo", fx.auth_base))
        .expect("build POST /userinfo")
        .header("authorization", format!("Bearer {token}"))
        .expect("authorization")
        .send()
        .await
}

async fn assert_invalid_token(resp: cyper::Response) {
    assert_eq!(resp.status().as_u16(), 401);
    assert_eq!(
        resp.headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok()),
        Some(r#"Bearer error="invalid_token""#)
    );
}

fn www_authenticate(resp: &cyper::Response) -> Option<String> {
    resp.headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// RFC 6750 §3.1: a request with no credential gets a bare `Bearer` challenge
/// (no `error`), distinct from the `invalid_token` challenge for a bad one.
fn assert_missing_token(resp: &cyper::Response) {
    assert_eq!(resp.status().as_u16(), 401);
    assert_eq!(www_authenticate(resp).as_deref(), Some("Bearer"));
}

/// A valid access token that lacks the `openid` scope → 403 insufficient_scope.
fn assert_insufficient_scope(resp: &cyper::Response) {
    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(
        www_authenticate(resp).as_deref(),
        Some(r#"Bearer error="insufficient_scope", scope="openid""#)
    );
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

fn access_claims(fx: &Fixture) -> Value {
    let now = unix_timestamp();
    json!({
        "iss": fx.issuer.issuer(),
        "sub": fx.issuer.pairwise_subject(&fx.user_id.to_string(), SECTOR),
        "aud": format!("app:{}", fx.app_id),
        "exp": now + 600,
        "iat": now,
        "jti": Uuid::new_v4().to_string(),
        "client_id": fx.client_id.clone(),
        "scope": "openid"
    })
}

fn unsigned_none_token(claims: &Value, kid: &str) -> String {
    let header = json!({
        "alg": "none",
        "typ": ACCESS_TOKEN_TYP,
        "kid": kid,
    });
    format!(
        "{}.{}.",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("header json")),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("claims json")),
    )
}

fn expired_access_token(fx: &Fixture) -> String {
    issue_access_token_with_issuer(fx, &fx.issuer, Some(-60))
}

fn wrong_issuer_access_token(fx: &Fixture) -> String {
    let issuer = test_issuer(WRONG_ISSUER);
    issue_access_token_with_issuer(fx, &issuer, Some(600))
}

fn issue_access_token_with_issuer(fx: &Fixture, issuer: &Issuer, ttl_secs: Option<i64>) -> String {
    access_token_with_scopes(fx, issuer, &["openid"], ttl_secs)
}

/// Mint a signed OP access token with an arbitrary scope set — used to build
/// tokens the real `/token` flow won't return (e.g. no `openid`, so no paired
/// id_token). `issuer` both signs and supplies the pairwise sub.
fn access_token_with_scopes(
    fx: &Fixture,
    issuer: &Issuer,
    scopes: &[&str],
    ttl_secs: Option<i64>,
) -> String {
    let user_id = fx.user_id.to_string();
    let audience = format!("app:{}", fx.app_id);
    let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_string()).collect();
    issuer
        .issue_access_token(&AccessTokenMint {
            user_id: &user_id,
            sector: SECTOR,
            audience: &audience,
            client_id: &fx.client_id,
            scopes: &scopes,
            ttl_secs,
        })
        .expect("issue access token")
}

fn tamper_token(token: &str) -> String {
    let mut tampered = token.to_string();
    let last = tampered.pop().expect("non-empty token");
    tampered.push(if last == 'a' { 'b' } else { 'a' });
    tampered
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_secs()
        .try_into()
        .expect("timestamp fits i64")
}
