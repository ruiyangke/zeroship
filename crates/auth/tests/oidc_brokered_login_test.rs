//! P5a-2 gateway-brokered login OP tests.

mod common;

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use ntex::web;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::oidc::metadata::jwks_document;
use zeroship_auth::oidc::{
    AccessTokenClaims, BrokerSecrets, IdTokenClaims, Issuer, ACCESS_TOKEN_TYP, ID_TOKEN_TYP,
};
use zeroship_auth::server;
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::sessions as session_store;

use common::{location, pkce_challenge_s256, pkce_verifier, test_auth_config};

const ISSUER: &str = "https://auth.zeroship.test/oauth2";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/cb";
const UNREGISTERED_REDIRECT_URI: &str = "http://127.0.0.1:9999/not-registered";
const SECTOR: &str = "https://brokered-app.zeroship.test";
const BROKER_CURRENT: &[u8] = b"p5a-current-broker-master-secret-32-bytes";
const BROKER_PREVIOUS: &[u8] = b"p5a-previous-broker-master-secret-32-bytes";

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    id_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Clone, Copy)]
enum ClientKind {
    Brokered,
    NonBrokered,
}

impl ClientKind {
    fn is_brokered(self) -> bool {
        matches!(self, Self::Brokered)
    }
}

struct Fixture {
    srv: ntex::web::test::TestServer,
    auth_base: String,
    db: Arc<Client>,
    issuer: Arc<Issuer>,
    client_id: String,
    app_id: Uuid,
    user_id: Uuid,
    session_cookie: String,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot(kind: ClientKind, previous: Option<&[u8]>) -> Option<Self> {
        let Some(db_url) = db_url() else {
            eprintln!("[oidc_brokered_login_test] skip (AUTH_DB_URL or CONTROL_TEST_DB unset)");
            return None;
        };
        let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(err) = pg_connection.run().await {
                eprintln!("[oidc_brokered_login_test] pg connection error: {err}");
            }
        })
        .detach();
        let db = Arc::new(pg_client);

        let broker_secrets =
            BrokerSecrets::new(BROKER_CURRENT.to_vec(), previous.map(|bytes| bytes.to_vec()))
                .expect("broker secrets");
        let issuer = Arc::new(test_issuer().with_broker_secrets(broker_secrets));
        issuer
            .publish_active_key(&db)
            .await
            .expect("publish active OP key");

        let user_id = Uuid::new_v4();
        let app_id = Uuid::new_v4();
        let client_prefix = if kind.is_brokered() {
            "oac_p5a"
        } else {
            "oac_p5a_public"
        };
        let client_id = format!("{client_prefix}_{}", Uuid::new_v4().simple());
        let app_name = format!("p5a-brokered-{}", Uuid::new_v4().simple());
        seed_user_client(&db, user_id, app_id, &app_name, &client_id, kind).await;

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

        let mut cfg = test_auth_config(
            &db_url,
            "http://127.0.0.1:4445",
            "http://127.0.0.1:4444",
        );
        // Refresh-token issuance (offline_access) needs the HMAC + idempotency
        // keys — brokered clients keep the refresh anchor, so the fixture must
        // configure them for the MED-2 refresh path.
        let key_dir = std::env::temp_dir().join(format!("p5a-broker-keys-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&key_dir).expect("key dir");
        let hash_key_file = key_dir.join("refresh-hmac.keys");
        let idem_key_file = key_dir.join("refresh-idem.key");
        write_owner_only(
            &hash_key_file,
            b"1:000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        );
        write_owner_only(&idem_key_file, b"refresh-idem-key-material-32-bytes");
        cfg.refresh_hash_key_file = Some(hash_key_file);
        cfg.refresh_idem_key_file = Some(idem_key_file);
        let cfg = Arc::new(cfg);
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
async fn brokered_code_exchange_without_broker_secret_is_invalid_client() {
    let Some(fx) = Fixture::boot(ClientKind::Brokered, None).await else {
        return;
    };

    let verifier = pkce_verifier();
    let code = authorize_code(&fx, REDIRECT_URI, &verifier).await;
    let missing = token_request(&fx, &code, REDIRECT_URI, &verifier, None)
        .await
        .expect("missing-secret token response");
    assert_invalid_client_without_tokens(missing).await;

    let verifier = pkce_verifier();
    let code = authorize_code(&fx, REDIRECT_URI, &verifier).await;
    let wrong = token_request(&fx, &code, REDIRECT_URI, &verifier, Some("wrong-secret"))
        .await
        .expect("wrong-secret token response");
    assert_invalid_client_without_tokens(wrong).await;

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn brokered_code_exchange_with_derived_secret_yields_global_sub_id_token_and_pairwise_access() {
    let Some(fx) = Fixture::boot(ClientKind::Brokered, None).await else {
        return;
    };
    let verifier = pkce_verifier();
    let code = authorize_code(&fx, REDIRECT_URI, &verifier).await;
    let secret = zeroship_core::auth::derive_broker_secret(BROKER_CURRENT, &fx.client_id);

    let token = exchange_code(&fx, &code, REDIRECT_URI, &verifier, Some(&secret))
        .await
        .expect("token response");
    let jwks = jwks_document(&fx.db).await.expect("jwks");
    let id = verify_with_jwks::<IdTokenClaims>(
        &jwks,
        &token.id_token,
        fx.issuer.issuer(),
        &fx.client_id,
        ID_TOKEN_TYP,
    )
    .expect("verify id token");
    assert_eq!(id.sub, fx.user_id.to_string());

    let access = verify_with_jwks::<AccessTokenClaims>(
        &jwks,
        &token.access_token,
        fx.issuer.issuer(),
        &format!("app:{}", fx.app_id),
        ACCESS_TOKEN_TYP,
    )
    .expect("verify access token");
    assert_eq!(
        access.sub,
        fx.issuer.pairwise_subject(&fx.user_id.to_string(), SECTOR)
    );
    assert_ne!(id.sub, access.sub);

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn brokered_rotation_previous_master_secret_still_accepted() {
    let Some(fx) = Fixture::boot(ClientKind::Brokered, Some(BROKER_PREVIOUS)).await else {
        return;
    };
    let verifier = pkce_verifier();
    let code = authorize_code(&fx, REDIRECT_URI, &verifier).await;
    let previous_secret =
        zeroship_core::auth::derive_broker_secret(BROKER_PREVIOUS, &fx.client_id);

    let token = exchange_code(&fx, &code, REDIRECT_URI, &verifier, Some(&previous_secret))
        .await
        .expect("previous broker secret accepted");
    assert!(!token.id_token.is_empty());
    assert!(!token.access_token.is_empty());

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn non_brokered_client_unchanged_pairwise_and_no_secret_required() {
    let Some(fx) = Fixture::boot(ClientKind::NonBrokered, None).await else {
        return;
    };
    let verifier = pkce_verifier();
    let code = authorize_code(&fx, REDIRECT_URI, &verifier).await;

    let token = exchange_code(&fx, &code, REDIRECT_URI, &verifier, None)
        .await
        .expect("non-brokered token response");
    let jwks = jwks_document(&fx.db).await.expect("jwks");
    let id = verify_with_jwks::<IdTokenClaims>(
        &jwks,
        &token.id_token,
        fx.issuer.issuer(),
        &fx.client_id,
        ID_TOKEN_TYP,
    )
    .expect("verify id token");
    assert_eq!(
        id.sub,
        fx.issuer.pairwise_subject(&fx.user_id.to_string(), SECTOR)
    );

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn brokered_client_still_enforces_exact_redirect_match() {
    let Some(fx) = Fixture::boot(ClientKind::Brokered, None).await else {
        return;
    };
    let verifier = pkce_verifier();

    let resp = send_authorize(&fx, UNREGISTERED_REDIRECT_URI, &verifier)
        .await
        .expect("authorize response");
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.json::<Value>().await.expect("oauth error json");
    assert_eq!(body["error"], "invalid_request");
    assert!(body["code"].is_null(), "authorize error must not return a code");

    fx.cleanup().await;
}

// MED-2: a brokered client authenticates the REFRESH grant by derive-and-compare
// (it has no stored secret hash). Without the brokered-first branch in
// authenticate_for_refresh, a brokered refresh would fail against the NULL hash.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn brokered_refresh_grant_requires_broker_secret() {
    let Some(fx) = Fixture::boot(ClientKind::Brokered, None).await else {
        return;
    };
    let verifier = pkce_verifier();
    let secret = zeroship_core::auth::derive_broker_secret(BROKER_CURRENT, &fx.client_id);

    // Mint a refresh token via the brokered authz_code flow (offline_access + secret).
    let resp = send_authorize_scoped(&fx, REDIRECT_URI, &verifier, "openid offline_access")
        .await
        .expect("authorize response");
    assert_eq!(resp.status().as_u16(), 303, "authorize status");
    let code = query_param(&location(&resp), "code").expect("code in redirect");
    let tokens = exchange_code(&fx, &code, REDIRECT_URI, &verifier, Some(&secret))
        .await
        .expect("token response");
    let refresh_token = tokens
        .refresh_token
        .expect("brokered client with offline_access issues a refresh token");

    // WITHOUT the broker secret → invalid_client (the pinned MED-2 failure).
    let no_secret = refresh_request(&fx, &refresh_token, None)
        .await
        .expect("refresh without secret");
    assert_eq!(no_secret.status().as_u16(), 400);
    assert_eq!(
        no_secret.json::<Value>().await.expect("json")["error"],
        "invalid_client",
        "brokered refresh without the broker secret must be invalid_client"
    );

    // WITH the derived broker secret → rotates successfully (proves the
    // brokered-first derive-and-compare path is wired for refresh).
    let ok = refresh_request(&fx, &refresh_token, Some(&secret))
        .await
        .expect("refresh with secret");
    assert_eq!(
        ok.status().as_u16(),
        200,
        "brokered refresh WITH the broker secret must succeed"
    );
    // The refresh grant returns access + refresh + scope (no id_token), so parse
    // as a generic value rather than the authz_code TokenResponse shape.
    let rotated = ok.json::<Value>().await.expect("refresh json");
    assert!(
        rotated["access_token"].as_str().is_some_and(|t| !t.is_empty()),
        "brokered refresh must return a new access token: {rotated}"
    );

    fx.cleanup().await;
}

#[allow(clippy::future_not_send)]
async fn refresh_request(
    fx: &Fixture,
    refresh_token: &str,
    broker_secret: Option<&str>,
) -> Result<cyper::Response, cyper::Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("client_id", &fx.client_id)
        .append_pair("refresh_token", refresh_token)
        .finish();
    let mut req = cyper::Client::new()
        .request(http::Method::POST, format!("{}/oauth2/token", fx.auth_base))
        .expect("build POST /token")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type");
    if let Some(secret) = broker_secret {
        let basic = STANDARD.encode(format!("{}:{secret}", fx.client_id));
        req = req
            .header("authorization", format!("Basic {basic}"))
            .expect("authorization");
    }
    req.body(body).send().await
}

fn write_owner_only(path: &std::path::Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes).expect("write secret file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod 0600 secret file");
}

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("CONTROL_TEST_DB"))
        .ok()
}

fn test_issuer() -> Issuer {
    let signing = SigningKey::from_bytes(&[43u8; 32]);
    Issuer::from_signing_key(&signing, [13u8; 32], ISSUER.to_string()).expect("issuer")
}

async fn seed_user_client(
    db: &Client,
    user_id: Uuid,
    app_id: Uuid,
    app_name: &str,
    client_id: &str,
    kind: ClientKind,
) {
    let email = format!("p5a-{}@zeroship.test", Uuid::new_v4().simple());
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name) \
         VALUES ($1, $2::citext, NOW(), 'P5a User')",
        &[&user_id, &email],
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
    // Brokered clients allow offline_access + refresh (the gateway's oac_
    // clients keep the 30-day refresh anchor) so the MED-2 refresh path is
    // exercisable; existing tests request narrower scopes and are unaffected.
    let scopes = vec!["openid".to_string(), "offline_access".to_string()];
    let brokered = kind.is_brokered();
    let token_endpoint_auth_method = if brokered {
        "client_secret_basic"
    } else {
        "none"
    };
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent, hydra_client_id, \
             token_endpoint_auth_method, brokered, refresh_allowed) \
         VALUES ($1, 'P5a brokered OP test', $2, $3, FALSE, $1, $4, $5, $5)",
        &[
            &client_id,
            &vec![REDIRECT_URI.to_string()],
            &scopes,
            &token_endpoint_auth_method,
            &brokered,
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
        &[&user_id, &client_id, &scopes],
    )
    .await
    .expect("seed oauth grant");
}

async fn cleanup_seeded_rows(db: &Client, user_id: Uuid, app_id: Uuid, client_id: &str) {
    let _ = db
        .execute(
            "DELETE FROM zeroship.oauth_authorization_codes WHERE client_id = $1",
            &[&client_id],
        )
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.oauth_grants WHERE client_id = $1", &[&client_id])
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
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id]).await;
    let _ = db.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id]).await;
}

async fn authorize_code(fx: &Fixture, redirect_uri: &str, verifier: &str) -> String {
    let resp = send_authorize(fx, redirect_uri, verifier)
        .await
        .expect("authorize response");
    assert_eq!(resp.status().as_u16(), 303, "authorize status");
    query_param(&location(&resp), "code").expect("code in redirect")
}

#[allow(clippy::future_not_send)]
async fn send_authorize(
    fx: &Fixture,
    redirect_uri: &str,
    verifier: &str,
) -> Result<cyper::Response, cyper::Error> {
    send_authorize_scoped(fx, redirect_uri, verifier, "openid").await
}

#[allow(clippy::future_not_send)]
async fn send_authorize_scoped(
    fx: &Fixture,
    redirect_uri: &str,
    verifier: &str,
    scope: &str,
) -> Result<cyper::Response, cyper::Error> {
    let nonce = format!("nc-{}", Uuid::new_v4().simple());
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &fx.client_id)
        .append_pair("response_type", "code")
        .append_pair("scope", scope)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", "state-123")
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &pkce_challenge_s256(verifier))
        .append_pair("code_challenge_method", "S256")
        .finish();
    cyper::Client::new()
        .request(http::Method::GET, format!("{}/oauth2/authorize?{query}", fx.auth_base))
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
    broker_secret: Option<&str>,
) -> Result<TokenResponse, cyper::Error> {
    let resp = token_request(fx, code, redirect_uri, verifier, broker_secret).await?;
    assert_eq!(resp.status().as_u16(), 200, "token response status");
    resp.json::<TokenResponse>().await
}

#[allow(clippy::future_not_send)]
async fn token_request(
    fx: &Fixture,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
    broker_secret: Option<&str>,
) -> Result<cyper::Response, cyper::Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", &fx.client_id)
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("code_verifier", verifier)
        .finish();
    let mut req = cyper::Client::new()
        .request(http::Method::POST, format!("{}/oauth2/token", fx.auth_base))
        .expect("build POST /token")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type");
    if let Some(secret) = broker_secret {
        let basic = STANDARD.encode(format!("{}:{secret}", fx.client_id));
        req = req
            .header("authorization", format!("Basic {basic}"))
            .expect("authorization");
    }
    req.body(body).send().await
}

async fn assert_invalid_client_without_tokens(resp: cyper::Response) {
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.json::<Value>().await.expect("oauth error json");
    assert_eq!(body["error"], "invalid_client");
    assert!(
        body.get("access_token").is_none() && body.get("id_token").is_none(),
        "invalid_client response must not include tokens: {body}"
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

fn verify_with_jwks<T: DeserializeOwned>(
    jwks: &Value,
    token: &str,
    expected_iss: &str,
    expected_aud: &str,
    expected_typ: &str,
) -> Result<T, String> {
    let header = decode_header(token).map_err(|err| format!("header: {err}"))?;
    if header.alg != Algorithm::EdDSA {
        return Err(format!("unexpected alg: {:?}", header.alg));
    }
    if header.typ.as_deref() != Some(expected_typ) {
        return Err(format!("unexpected typ: {:?}", header.typ));
    }
    let kid = header.kid.ok_or_else(|| "missing kid".to_string())?;
    let keys = jwks["keys"]
        .as_array()
        .ok_or_else(|| "jwks keys missing".to_string())?;
    let jwk = keys
        .iter()
        .find(|jwk| jwk["kid"].as_str() == Some(kid.as_str()))
        .ok_or_else(|| format!("kid {kid} not found"))?;
    let x = jwk["x"].as_str().ok_or_else(|| "jwk x missing".to_string())?;
    let decoding_key =
        DecodingKey::from_ed_components(x).map_err(|err| format!("ed key: {err}"))?;
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.algorithms = vec![Algorithm::EdDSA];
    validation.set_issuer(&[expected_iss]);
    validation.set_audience(&[expected_aud]);
    validation.validate_nbf = true;
    validation.leeway = 0;
    decode::<T>(token, &decoding_key, &validation)
        .map(|data| data.claims)
        .map_err(|err| format!("decode: {err}"))
}
