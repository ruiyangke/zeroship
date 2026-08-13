//! Client authentication on `POST /oauth2/token`, `grant_type=authorization_code`.
//!
//! A client that was issued credentials must present them on every token
//! request (RFC 6749 4.1.3 / 3.2.1). The registered `token_endpoint_auth_method`
//! is what decides that, not the `brokered` flag: a confidential client can be
//! non-brokered (the DB CHECK only pins `brokered => client_secret_basic`, not
//! the converse), and a leaked code plus its verifier must not be redeemable
//! without the secret.
//!
//! The three registration shapes the endpoint has to keep apart:
//!   - confidential non-brokered (`client_secret_basic` + stored hash)
//!   - public (`none`, no hash) - PKCE is the only credential, by design
//!   - brokered (per-app broker secret, derived not stored)

mod common;

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use compio_postgres::{connect, Client, NoTls};
use ed25519_dalek::SigningKey;
use ntex::web;
use serde_json::Value;
use uuid::Uuid;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::oidc::{BrokerSecrets, Issuer};
use zeroship_auth::server;
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::sessions as session_store;
use zeroship_core::auth::hash_api_key;

use common::{location, pkce_challenge_s256, pkce_verifier, test_auth_config};

const ISSUER: &str = "https://auth.zeroship.test/oauth2";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/cb";
const SECTOR: &str = "https://token-client-auth.zeroship.test";
const CLIENT_SECRET: &str = "token-client-auth-secret-32-bytes-min";
const BROKER_MASTER: &[u8] = b"token-client-auth-broker-master-secret-32";

/// How the client is registered in `zeroship.oauth_clients`.
#[derive(Clone, Copy)]
enum ClientKind {
    /// `client_secret_basic` + a stored hash, `brokered = false`. The shape the
    /// control plane's builder client takes.
    ConfidentialNonBrokered,
    /// `none`, no stored hash. Every creator app's end-user client.
    Public,
    /// `brokered = true`: authenticates by derive-and-compare, not by hash.
    Brokered,
}

impl ClientKind {
    fn auth_method(self) -> &'static str {
        match self {
            Self::Public => "none",
            Self::ConfidentialNonBrokered | Self::Brokered => "client_secret_basic",
        }
    }

    fn secret_hash(self) -> Option<String> {
        matches!(self, Self::ConfidentialNonBrokered).then(|| hash_api_key(CLIENT_SECRET))
    }

    fn is_brokered(self) -> bool {
        matches!(self, Self::Brokered)
    }
}

struct Fixture {
    srv: ntex::web::test::TestServer,
    auth_base: String,
    db: Arc<Client>,
    client_id: String,
    app_id: Uuid,
    user_id: Uuid,
    session_cookie: String,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot(kind: ClientKind) -> Self {
        let db_url = zeroship_core::declared_env!(external, "AUTH_DB_URL", zeroship_core::config::TestHarness)
            .or_else(|| zeroship_core::test_env!("CONTROL_TEST_DB"))
            .expect("AUTH_DB_URL is required for oidc_token_client_auth_test");
        let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(err) = pg_connection.run().await {
                eprintln!("[oidc_token_client_auth_test] pg connection error: {err}");
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
        let client_id = format!("oac_tca_{}", Uuid::new_v4().simple());
        let app_name = format!("token-client-auth-{}", Uuid::new_v4().simple());
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
        let session_cookie = session_cookie::set_cookie(&session.id)
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

        Self {
            auth_base: srv.url("").trim_end_matches('/').to_string(),
            srv,
            db,
            client_id,
            app_id,
            user_id,
            session_cookie,
        }
    }

    #[allow(clippy::future_not_send)]
    async fn cleanup(self) {
        cleanup_seeded_rows(&self.db, self.user_id, self.app_id, &self.client_id).await;
        drop(self.srv);
    }

    fn basic_auth(&self, secret: &str) -> String {
        format!(
            "Basic {}",
            STANDARD.encode(format!("{}:{secret}", self.client_id))
        )
    }
}

/// The defect this suite pins: a confidential client that never sent its secret
/// still redeemed the code. Anyone who captures the code and the verifier (a
/// Referer leak, a proxy access log) could mint tokens as the client.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn confidential_client_code_exchange_without_client_auth_is_rejected() {
    let fx = Fixture::boot(ClientKind::ConfidentialNonBrokered).await;
    let verifier = pkce_verifier();
    let code = authorize_code(&fx, &verifier).await;

    let resp = token_request(&fx, &code, &verifier, None)
        .await
        .expect("token response");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "a client registered with a secret must authenticate on authorization_code"
    );
    let body = resp.json::<Value>().await.expect("oauth error json");
    assert_eq!(body["error"], "invalid_client");
    assert!(
        body.get("access_token").is_none() && body.get("id_token").is_none(),
        "rejected exchange must not return tokens: {body}"
    );

    fx.cleanup().await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn confidential_client_code_exchange_with_basic_credentials_succeeds() {
    let fx = Fixture::boot(ClientKind::ConfidentialNonBrokered).await;
    let verifier = pkce_verifier();
    let code = authorize_code(&fx, &verifier).await;

    let resp = token_request(&fx, &code, &verifier, Some(&fx.basic_auth(CLIENT_SECRET)))
        .await
        .expect("token response");
    assert_eq!(resp.status().as_u16(), 200, "correct credentials must exchange");
    let body = resp.json::<Value>().await.expect("token json");
    assert!(
        body["access_token"].as_str().is_some_and(|t| !t.is_empty()),
        "exchange must return an access token: {body}"
    );
    assert!(
        body["id_token"].as_str().is_some_and(|t| !t.is_empty()),
        "openid exchange must return an id token: {body}"
    );

    fx.cleanup().await;
}

/// RFC 6749 5.2: `invalid_client` on a request that carried the `Authorization`
/// header is 401 with a matching challenge, not the bare 400 the endpoint used
/// to return for every error class.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn confidential_client_bad_secret_is_401_with_www_authenticate() {
    let fx = Fixture::boot(ClientKind::ConfidentialNonBrokered).await;
    let verifier = pkce_verifier();
    let code = authorize_code(&fx, &verifier).await;

    let resp = token_request(&fx, &code, &verifier, Some(&fx.basic_auth("wrong-secret")))
        .await
        .expect("token response");
    assert_eq!(resp.status().as_u16(), 401);
    let challenge = resp
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        challenge.starts_with("Basic "),
        "401 must carry a challenge matching the scheme the client used, got {challenge:?}"
    );
    let body = resp.json::<Value>().await.expect("oauth error json");
    assert_eq!(body["error"], "invalid_client");

    fx.cleanup().await;
}

/// The intended public path: creator apps hold no secret, so PKCE alone
/// redeems the code. Over-blocking here breaks every deployed app.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn public_client_code_exchange_without_client_auth_succeeds() {
    let fx = Fixture::boot(ClientKind::Public).await;
    let verifier = pkce_verifier();
    let code = authorize_code(&fx, &verifier).await;

    let resp = token_request(&fx, &code, &verifier, None)
        .await
        .expect("token response");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "a public client is PKCE-only and must still exchange without a secret"
    );
    let body = resp.json::<Value>().await.expect("token json");
    assert!(body["access_token"].as_str().is_some_and(|t| !t.is_empty()));

    fx.cleanup().await;
}

/// Brokered clients keep their own control: the per-app secret is derived from
/// the platform master, never stored as a hash. The confidential-client gate
/// must not swallow that route.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn brokered_client_still_authenticates_by_broker_secret() {
    let fx = Fixture::boot(ClientKind::Brokered).await;

    let verifier = pkce_verifier();
    let code = authorize_code(&fx, &verifier).await;
    let missing = token_request(&fx, &code, &verifier, None)
        .await
        .expect("token response without broker secret");
    assert_eq!(missing.status().as_u16(), 401);
    assert_eq!(
        missing.json::<Value>().await.expect("oauth error json")["error"],
        "invalid_client"
    );

    let verifier = pkce_verifier();
    let code = authorize_code(&fx, &verifier).await;
    let secret = zeroship_core::auth::derive_broker_secret(BROKER_MASTER, &fx.client_id);
    let ok = token_request(&fx, &code, &verifier, Some(&fx.basic_auth(&secret)))
        .await
        .expect("token response with broker secret");
    assert_eq!(
        ok.status().as_u16(),
        200,
        "the derived broker secret must remain the brokered client's credential"
    );

    fx.cleanup().await;
}

fn test_issuer() -> Issuer {
    let signing = SigningKey::from_bytes(&[57u8; 32]);
    let broker_secrets = BrokerSecrets::new(BROKER_MASTER.to_vec(), None).expect("broker secrets");
    Issuer::from_signing_key(&signing, [23u8; 32], ISSUER.to_string())
        .expect("issuer")
        .with_broker_secrets(broker_secrets)
}

async fn seed_user_client(
    db: &Client,
    user_id: Uuid,
    app_id: Uuid,
    app_name: &str,
    client_id: &str,
    kind: ClientKind,
) {
    let email = format!("tca-{}@zeroship.test", Uuid::new_v4().simple());
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name) \
         VALUES ($1, $2::citext, NOW(), 'Token Client Auth User')",
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
    let scopes = vec!["openid".to_string()];
    let auth_method = kind.auth_method();
    let secret_hash = kind.secret_hash();
    let brokered = kind.is_brokered();
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent, \
             client_secret_hash, token_endpoint_auth_method, brokered, refresh_allowed) \
         VALUES ($1, 'token client auth test', $2, $3, FALSE, $4, $5, $6, FALSE)",
        &[
            &client_id,
            &vec![REDIRECT_URI.to_string()],
            &scopes,
            &secret_hash,
            &auth_method,
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
    for statement in [
        "DELETE FROM zeroship.oauth_authorization_codes WHERE client_id = $1",
        "DELETE FROM zeroship.oauth_grants WHERE client_id = $1",
        "DELETE FROM zeroship.app_user_identities WHERE app_client_id = $1",
        "DELETE FROM zeroship.app_oauth_clients WHERE client_id = $1",
    ] {
        let _ = db.execute(statement, &[&client_id]).await;
    }
    let _ = db
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&user_id],
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
async fn authorize_code(fx: &Fixture, verifier: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &fx.client_id)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("state", "state-tca")
        .append_pair("nonce", &format!("nc-{}", Uuid::new_v4().simple()))
        .append_pair("code_challenge", &pkce_challenge_s256(verifier))
        .append_pair("code_challenge_method", "S256")
        .finish();
    let resp = cyper::Client::new()
        .request(
            http::Method::GET,
            format!("{}/oauth2/authorize?{query}", fx.auth_base),
        )
        .expect("build GET /authorize")
        .header("cookie", fx.session_cookie.clone())
        .expect("cookie")
        .send()
        .await
        .expect("authorize response");
    assert_eq!(resp.status().as_u16(), 303, "authorize status");
    query_param(&location(&resp), "code").expect("code in redirect")
}

#[allow(clippy::future_not_send)]
async fn token_request(
    fx: &Fixture,
    code: &str,
    verifier: &str,
    authorization: Option<&str>,
) -> Result<cyper::Response, cyper::Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", &fx.client_id)
        .append_pair("code", code)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("code_verifier", verifier)
        .finish();
    let mut req = cyper::Client::new()
        .request(http::Method::POST, format!("{}/oauth2/token", fx.auth_base))
        .expect("build POST /token")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type");
    if let Some(value) = authorization {
        req = req
            .header("authorization", value.to_string())
            .expect("authorization");
    }
    req.body(body).send().await
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
