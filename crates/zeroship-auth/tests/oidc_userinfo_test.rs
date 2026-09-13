//! OIDC Core §5.3 `/oauth2/userinfo` tests.

use crate::common;
use common::{auth_server::AuthServer, database::Database};
use ed25519_dalek::{pkcs8::EncodePrivateKey, SigningKey};

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::Client;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_auth::oidc::metadata::jwks_document;
use zeroship_auth::oidc::{IdTokenClaims, Issuer, ACCESS_TOKEN_TYP, ID_TOKEN_TYP};
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::sessions as session_store;

use common::{location, pkce_challenge_s256, pkce_verifier};

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
    server: AuthServer,
    db: Arc<Client>,
    issuer: Arc<Issuer>,
    client_id: String,
    app_id: zeroship_core::AppId,
    user_id: zeroship_core::UserId,
    user_email: String,
    user_name: String,
    user_avatar_url: String,
    session_cookie: String,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot(database: &Database) -> Self {
        let db = Arc::new(database.connect().await);
        let issuer = Arc::new(test_issuer(ISSUER));

        let user_id = zeroship_core::UserId::mint();
        let app_id = zeroship_core::AppId::mint();
        let client_id = format!("oac_userinfo_{}", Uuid::new_v4().simple());
        let app_name = format!("userinfo-{}", Uuid::new_v4().simple());
        let user_profile =
            seed_user_client(&db, &issuer, &user_id, &app_id, &app_name, &client_id).await;
        let session = session_store::create(
            &db,
            &session_store::CreateSession {
                user_id: user_id.clone(),
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
        let session_cookie = session_cookie::set_cookie(&session.id)
            .split(';')
            .next()
            .expect("cookie pair")
            .to_string();

        let server = AuthServer::with_issuer(database, issuer.clone()).await;
        Self {
            server,
            db,
            issuer,
            client_id,
            app_id,
            user_id: user_id.clone(),
            user_email: user_profile.email,
            user_name: user_profile.name,
            user_avatar_url: user_profile.avatar_url,
            session_cookie,
        }
    }
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_returns_scope_gated_claims_for_valid_token() {
    Database::run(async |database| {
        let fx = Fixture::boot(database).await;
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
        assert!(resp
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json")));
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
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_omits_identity_claims_without_scopes() {
    Database::run(async |database| {
        let fx = Fixture::boot(database).await;
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
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_rejects_missing_and_bad_tokens() {
    Database::run(async |database| {
        let fx = Fixture::boot(database).await;

        let missing = userinfo_get(&fx, None).await.expect("missing bearer");
        assert_missing_token(&missing);

        let garbage = userinfo_get_with_authorization(&fx, "Bearer not-a-jwt")
            .await
            .expect("garbage bearer");
        assert_invalid_token(garbage);

        let token = issue_token(&fx, "openid").await;
        let tampered = userinfo_get(&fx, Some(&tamper_token(&token.access_token)))
            .await
            .expect("tampered bearer");
        assert_invalid_token(tampered);

        let alg_none = userinfo_get(
            &fx,
            Some(&unsigned_none_token(&access_claims(&fx), fx.issuer.kid())),
        )
        .await
        .expect("alg none bearer");
        assert_invalid_token(alg_none);

        let expired = userinfo_get(&fx, Some(&expired_access_token(&fx)))
            .await
            .expect("expired bearer");
        assert_invalid_token(expired);

        let wrong_issuer = userinfo_get(&fx, Some(&wrong_issuer_access_token(&fx)))
            .await
            .expect("wrong issuer bearer");
        assert_invalid_token(wrong_issuer);
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_rejects_id_token_used_as_access_token() {
    Database::run(async |database| {
        let fx = Fixture::boot(database).await;
        let token = issue_token(&fx, "openid email profile").await;

        let resp = userinfo_get(&fx, Some(&token.id_token))
            .await
            .expect("id token as bearer");
        assert_invalid_token(resp);
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_rejects_token_without_openid_scope() {
    Database::run(async |database| {
        let fx = Fixture::boot(database).await;
        // A validly-signed OP access token minted for the app resource audience but
        // WITHOUT `openid` must not be usable as an identity oracle (OIDC Core §5.3).
        let access_token =
            access_token_with_scopes(&fx, &fx.issuer, &["email", "profile"], Some(600));

        let resp = userinfo_get(&fx, Some(&access_token))
            .await
            .expect("no-openid bearer");
        assert_insufficient_scope(&resp);
    })
    .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn userinfo_rejects_disabled_user() {
    Database::run(async |database| {
        let fx = Fixture::boot(database).await;
        let token = issue_token(&fx, "openid email profile").await;

        // Disable the account AFTER the token was minted: a still-live access token
        // must stop leaking identity once the user is terminated (MED-2).
        fx.db
            .execute(
                "UPDATE zeroship.users SET disabled_at = NOW() WHERE id = $1",
                &[&fx.user_id.as_str()],
            )
            .await
            .expect("disable user");

        let resp = userinfo_get(&fx, Some(&token.access_token))
            .await
            .expect("disabled-user bearer");
        assert_invalid_token(resp);
    })
    .await;
}

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[12; 32])
}

fn test_issuer(issuer: &str) -> Issuer {
    let signing = signing_key();
    Issuer::from_signing_key(&signing, [9u8; 32], issuer.to_string()).expect("issuer")
}

#[allow(clippy::future_not_send)]
async fn seed_user_client(
    db: &Client,
    issuer: &Issuer,
    user_id: &zeroship_core::UserId,
    app_id: &zeroship_core::AppId,
    app_name: &str,
    client_id: &str,
) -> SeededUserProfile {
    let email = format!("userinfo-{}@zeroship.test", Uuid::new_v4().simple());
    let name = format!("UserInfo User {}", Uuid::new_v4().simple());
    let avatar_url = format!("https://cdn.zeroship.test/avatars/{}.png", user_id.as_str());
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name, avatar_url) \
         VALUES ($1, $2::citext, NOW(), $3, $4)",
        &[&user_id.as_str(), &email, &name, &avatar_url],
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
    // An app row needs a project, and a project needs an organization. Nothing
    // here asserts on authority, so the organization is left member-less.
    let project_id = common::unowned_project(db).await;
    db.execute(
        "INSERT INTO zeroship.apps (id, name, project_id, organization_id) \
         SELECT $1, $2, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $3",
        &[&app_id.as_str(), &app_name, &project_id],
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
        &[&app_id.as_str(), &client_id, &SECTOR],
    )
    .await
    .expect("seed app oauth client");
    db.execute(
        "INSERT INTO zeroship.oauth_grants \
             (user_id, client_id, granted_scopes, granted_at, updated_at) \
         VALUES ($1, $2, $3, NOW(), NOW())",
        &[
            &user_id.as_str(),
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

    let pairwise_sub = issuer.pairwise_subject(user_id, SECTOR);
    db.execute(
        "INSERT INTO zeroship.app_user_identities \
            (app_client_id, global_user_id, pairwise_sub) \
         VALUES ($1, $2, $3)",
        &[&client_id, &user_id.as_str(), &pairwise_sub],
    )
    .await
    .expect("seed app user identity");
    SeededUserProfile {
        email,
        name,
        avatar_url,
    }
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
    let url = format!(
        "{}/oauth2/authorize?{}",
        fx.server.auth_base,
        serializer.finish()
    );
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
        .request(
            http::Method::POST,
            format!("{}/oauth2/token", fx.server.auth_base),
        )
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
async fn userinfo_get(fx: &Fixture, token: Option<&str>) -> Result<cyper::Response, cyper::Error> {
    let authorization = token.map(|token| format!("Bearer {token}"));
    userinfo_get_with_authorization(fx, authorization.as_deref().unwrap_or("")).await
}

#[allow(clippy::future_not_send)]
async fn userinfo_get_with_authorization(
    fx: &Fixture,
    authorization: &str,
) -> Result<cyper::Response, cyper::Error> {
    let req = cyper::Client::new()
        .request(
            http::Method::GET,
            format!("{}/oauth2/userinfo", fx.server.auth_base),
        )
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
        .request(
            http::Method::POST,
            format!("{}/oauth2/userinfo", fx.server.auth_base),
        )
        .expect("build POST /userinfo")
        .header("authorization", format!("Bearer {token}"))
        .expect("authorization")
        .send()
        .await
}

fn assert_invalid_token(resp: cyper::Response) {
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

/// A valid access token that lacks the `openid` scope → 403 `insufficient_scope`.
fn assert_insufficient_scope(resp: &cyper::Response) {
    assert_eq!(resp.status().as_u16(), 403);
    assert_eq!(
        www_authenticate(resp).as_deref(),
        Some(r#"Bearer error="insufficient_scope", scope="openid""#)
    );
}

fn query_param(raw_url: &str, name: &str) -> Option<String> {
    url::Url::parse(raw_url)
        .ok()?
        .query_pairs()
        .find_map(|(key, value)| {
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
        "sub": fx.issuer.pairwise_subject(&fx.user_id, SECTOR),
        "aud": format!("app:{}", fx.app_id.as_str()),
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

/// Sign explicit claims with the fixture key, including scope sets that the
/// real `/token` flow cannot return. The supplied issuer sets the identity
/// claim and key identifier; the subject comes from the fixture's person.
fn access_token_with_scopes(
    fx: &Fixture,
    issuer: &Issuer,
    scopes: &[&str],
    ttl_secs: Option<i64>,
) -> String {
    let mut claims = access_claims(fx);
    let now = unix_timestamp();
    claims["iss"] = json!(issuer.issuer());
    claims["scope"] = json!(scopes.join(" "));
    claims["iat"] = json!(now);
    claims["exp"] = json!(now + ttl_secs.unwrap_or(600));
    let key = signing_key().to_pkcs8_der().expect("fixture signing key");
    let mut header = jsonwebtoken::Header::new(Algorithm::EdDSA);
    header.typ = Some(ACCESS_TOKEN_TYP.into());
    header.kid = Some(issuer.kid().into());
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_ed_der(key.as_bytes()),
    )
    .expect("sign explicit access-token claims")
}

fn tamper_token(token: &str) -> String {
    let (prefix, signature) = token.rsplit_once('.').expect("signed JWT");
    let mut signature = signature.to_owned();
    let replacement = if signature.starts_with('a') { "b" } else { "a" };
    signature.replace_range(..1, replacement);
    format!("{prefix}.{signature}")
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_secs()
        .try_into()
        .expect("timestamp fits i64")
}
