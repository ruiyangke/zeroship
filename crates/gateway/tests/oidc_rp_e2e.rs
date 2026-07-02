//! Gateway OIDC RP end-to-end test against the platform OP.
//!
//! This is the P5a-3b-ii shape: no OP admin/client registration. The test
//! seeds a per-app brokered `oac_` client directly in `zeroship.oauth_clients`,
//! boots `crates/auth` in-process with a broker master secret, then drives the
//! native OP `/oauth2/authorize` + `/login` flow. `OidcRp::finish_callback`
//! exchanges the code as that per-app client by deriving the broker secret from the same
//! master and verifies the OP id_token (`iss` with the fixed `/oauth2` prefix,
//! `aud = oac_...`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser as _;
use compio_postgres::{Client, NoTls};
use ed25519_dalek::SigningKey;
use ntex::web::{self, test};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::identity::password;
use zeroship_auth::oidc::{BrokerSecrets, Issuer, PrincipalAccessTokenMint};
use zeroship_auth::server;
use zeroship_bundle::{AssetEntry, AuthLevel, Manifest, ResourceEntry, StaticAction};
use zeroship_gateway::blob_cache::{BlobCache, DiskBlobCache};
use zeroship_gateway::enforce;
use zeroship_gateway::idempotency;
use zeroship_gateway::oidc_rp::{BrokerSecret, BrowserAuthorizeParams, OidcRp, TokenSet};
use zeroship_gateway::proxy::HashRing;
use zeroship_gateway::sessions::{create, revoke_app_sessions_for_user, validate, NewSession};
use zeroship_gateway::sync::RouteCache;
use zeroship_gateway::{session_token, GateConfig, GateState};

const ISSUER: &str = "https://auth.zeroship.ai/oauth2";
const REDIRECT_URI: &str = "http://127.0.0.1:9999/__zeroship/auth/callback";
const SECTOR: &str = "https://gateway-e2e.zeroship.test";
const APP_HOST: &str = "gateway-e2e.zeroship.test";
const APP_NAME: &str = "gateway-e2e";
const PASSWORD: &str = "gateway-test-password-with-enough-bytes-1234";
const BROKER_MASTER: &[u8] = b"gateway-oidc-rp-e2e-broker-master-32-bytes";
const GATEWAY_ISS: &str = "https://api.zeroship.ai";

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("CONTROL_TEST_DB"))
        .ok()
}

fn location(resp: &cyper::Response) -> String {
    resp.headers()
        .get(http::header::LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn read_set_cookie(resp: &cyper::Response, name: &str) -> Option<String> {
    for hv in resp.headers().get_all(http::header::SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        let first = s.split(';').next().unwrap_or("");
        if let Some((n, v)) = first.split_once('=') {
            if n.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn query_param(raw_url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn relative_query_param(path: &str, key: &str) -> Option<String> {
    let url = url::Url::parse(&format!("http://auth.test{path}")).ok()?;
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish()
}

fn write_secret_file(path: &std::path::Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write secret file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("secret file permissions");
    }
}

#[derive(Debug, Clone)]
struct MemoryBlobStore {
    hash: String,
    body: bytes::Bytes,
}

#[async_trait::async_trait(?Send)]
impl zeroship_bundle::BlobStore for MemoryBlobStore {
    async fn get_blob(&self, h: &str) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
        if h == self.hash {
            Ok(self.body.clone())
        } else {
            Err(zeroship_bundle::BlobError::NotFound(h.to_string()))
        }
    }

    fn local_path(&self, _h: &str) -> Option<std::path::PathBuf> {
        None
    }

    async fn put_blob(
        &self,
        _h: &str,
        _d: &[u8],
    ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
        Ok(zeroship_bundle::PutOutcome::Wrote)
    }

    async fn put_blob_stream(
        &self,
        _h: &str,
        _s: u64,
        _r: &mut dyn std::io::Read,
    ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
        Ok(zeroship_bundle::PutOutcome::Wrote)
    }

    async fn has_blob(&self, h: &str) -> Result<bool, zeroship_bundle::BlobError> {
        Ok(h == self.hash)
    }

    async fn get_blob_to_file(
        &self,
        h: &str,
        _out: &compio::fs::File,
        _expected_size: Option<u64>,
        _max_bytes: u64,
    ) -> Result<u64, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound(h.to_string()))
    }

    async fn put_manifest(
        &self,
        _a: &Uuid,
        _d: &str,
        _j: &[u8],
    ) -> Result<(), zeroship_bundle::BlobError> {
        Ok(())
    }

    async fn get_manifest(
        &self,
        _a: &Uuid,
        _d: &str,
    ) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
        Err(zeroship_bundle::BlobError::NotFound("unused".into()))
    }

    async fn delete_app_manifests(
        &self,
        _a: &Uuid,
    ) -> Result<(), zeroship_bundle::BlobError> {
        Ok(())
    }
}

fn gateway_signing() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

fn protected_static_manifest(body: bytes::Bytes) -> (Manifest, MemoryBlobStore) {
    let hash = hex::encode(Sha256::digest(&body));
    let mut assets = HashMap::new();
    assets.insert(
        "/private.txt".to_string(),
        AssetEntry {
            hash: hash.clone(),
            content_type: "text/plain; charset=utf-8".to_string(),
            size: body.len() as u64,
            cache: None,
            updated_at: 0,
            variants: HashMap::new(),
        },
    );

    let mut resources = HashMap::new();
    resources.insert(
        "/private".to_string(),
        ResourceEntry {
            auth: Some(AuthLevel::User),
            r#static: Some(StaticAction {
                r#try: vec!["/private.txt".to_string()],
            }),
            ..Default::default()
        },
    );

    (
        Manifest {
            version: 1,
            assets,
            resources,
            ..Manifest::default()
        },
        MemoryBlobStore { hash, body },
    )
}

fn build_gateway_state(auth_base: &str, app_id: Uuid, client_id: &str) -> Arc<GateState> {
    let body = bytes::Bytes::from_static(b"ok");
    let (manifest, blob_store) = protected_static_manifest(body);
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("zsgate-oidc-e2e-{}", Uuid::new_v4().simple()));
    let disk_cache = DiskBlobCache::new(tmp, 1024 * 1024).expect("disk cache");

    let signing_key = gateway_signing();
    let session_issuer =
        session_token::Issuer::new(&signing_key, GATEWAY_ISS.to_string()).expect("session issuer");
    let session_verifier =
        session_token::Verifier::new(&signing_key.verifying_key(), GATEWAY_ISS.to_string());
    let oidc_rp = OidcRp::new(
        auth_base.to_string(),
        BrokerSecret::from_bytes(BROKER_MASTER.to_vec()).expect("gateway broker secret"),
        b"gateway-e2e-stash-signing-key-32-bytes!".to_vec(),
    )
    .with_issuer(ISSUER);

    let state = Arc::new(GateState {
        config: GateConfig {
            control_url: String::new(),
            control_key: String::new(),
            worker_urls: vec!["http://0.0.0.0:0".to_string()],
            poll_interval_secs: 5,
            worker_key: "gateway-e2e-worker-key".to_string(),
            auth_ui_url: auth_base.to_string(),
            insecure_dev: false,
            trust_proxy: false,
            public_url: GATEWAY_ISS.to_string(),
        },
        routes: RouteCache::new(),
        hash_ring: HashRing::new(vec!["http://0.0.0.0:0".to_string()], 1),
        rate_limiters: enforce::RateLimitRegistry::new(100, 100),
        per_rule_rate_limits: enforce::PerRuleRateLimitRegistry::new(),
        concurrency: enforce::ConcurrencyRegistry::new(100),
        blob_store: Arc::new(blob_store),
        blob_cache: BlobCache::new(1024 * 1024),
        disk_cache,
        idempotency_store: Arc::new(idempotency::InMemoryIdempotencyStore::new()),
        oidc_rp: Arc::new(oidc_rp),
        db: None,
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        revocation_cache: Arc::new(zeroship_core::wrapper_revocation::RevocationCache::new()),
        signing_key: Some(Arc::new(signing_key)),
        prev_signing_key: None,
        session_issuer: Some(Arc::new(session_issuer)),
        session_verifier: Some(Arc::new(session_verifier)),
        anchor_enc_key: zeroship_core::crypto::derive_key("gateway-e2e-anchor-key"),
        pairwise_salt: zeroship_core::crypto::derive_key("gateway-e2e-pairwise-salt"),
        meter: Arc::new(zeroship_metering::Meter::new()),
    });

    let mut routes = HashMap::new();
    routes.insert(
        app_id,
        zeroship_core::types::RouteEntry {
            name: APP_NAME.to_string(),
            plan_id: "free".to_string(),
            api_key_hash: "unused".to_string(),
            deploy_hash: None,
            manifest,
            oauth_client_id: Some(client_id.to_string()),
            sector_identifier: Some(SECTOR.to_string()),
            spend_state: zeroship_core::types::SpendState::Allow,
            account_state: zeroship_core::types::AccountState::Active,
        },
    );
    state
        .routes
        .update(routes, &state.rate_limiters, &state.concurrency);

    state
}

async fn start_platform_op(
    db_url: &str,
    pg_client: Arc<Client>,
    issuer: Arc<Issuer>,
) -> test::TestServer {
    let mut cfg = test_auth_config(db_url);
    let key_dir = std::env::temp_dir().join(format!("gateway-op-refresh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&key_dir).expect("refresh key dir");
    let hash_key_file = key_dir.join("refresh-hmac.keys");
    let idem_key_file = key_dir.join("refresh-idem.key");
    write_secret_file(
        &hash_key_file,
        b"1:000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    );
    write_secret_file(&idem_key_file, b"refresh-idem-key-material-32-bytes");
    cfg.refresh_hash_key_file = Some(hash_key_file);
    cfg.refresh_idem_key_file = Some(idem_key_file);
    let cfg = Arc::new(cfg);
    let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.to_string(), 4);

    test::server(move || {
        let cfg_state = cfg.clone();
        let db_state = pg_client.clone();
        let issuer_state = issuer.clone();
        let refresh_pool_state = refresh_pool.clone();
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
    .await
}

async fn connect_test_db(db_url: &str) -> Arc<Client> {
    let (pg_client, pg_connection) = compio_postgres::connect(db_url, NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[oidc_rp_e2e] pg connection driver: {e}");
        }
    })
    .detach();
    Arc::new(pg_client)
}

async fn drive_login_to_code(auth_base: &str, auth_url: &str, email: &str) -> String {
    let http = cyper::Client::new();

    let first = http
        .request(http::Method::GET, auth_url)
        .expect("build GET /authorize")
        .send()
        .await
        .expect("send GET /authorize");
    assert_eq!(first.status().as_u16(), 303);
    let login_loc = location(&first);
    assert!(login_loc.starts_with("/login?return_to="), "login redirect: {login_loc}");
    let return_to = relative_query_param(&login_loc, "return_to").expect("return_to");

    let login_get = http
        .request(http::Method::GET, format!("{auth_base}{login_loc}"))
        .expect("build GET /login")
        .send()
        .await
        .expect("send GET /login");
    assert_eq!(login_get.status().as_u16(), 200);
    let csrf = read_set_cookie(&login_get, "zsidp_csrf").expect("login csrf");
    let login_body = form(&[
        ("csrf", csrf.as_str()),
        ("email", email),
        ("password", PASSWORD),
        ("return_to", return_to.as_str()),
    ]);
    let login_post = http
        .request(http::Method::POST, format!("{auth_base}/login"))
        .expect("build POST /login")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("zsidp_csrf={csrf}"))
        .expect("cookie")
        .body(login_body)
        .send()
        .await
        .expect("send POST /login");
    assert_eq!(login_post.status().as_u16(), 303);
    assert_eq!(location(&login_post), return_to);
    let session = read_set_cookie(&login_post, "zsidp_session").expect("session cookie");

    let final_authorize = http
        .request(http::Method::GET, format!("{auth_base}{return_to}"))
        .expect("build GET /authorize with session")
        .header("cookie", format!("zsidp_session={session}"))
        .expect("cookie")
        .send()
        .await
        .expect("send GET /authorize with session");
    assert_eq!(final_authorize.status().as_u16(), 303);
    let cb_url = location(&final_authorize);
    assert!(cb_url.starts_with(REDIRECT_URI), "callback: {cb_url}");
    assert_eq!(query_param(&cb_url, "iss").as_deref(), Some(ISSUER));
    query_param(&cb_url, "code").expect("code param")
}

async fn browser_pkce_tokens(rp: &OidcRp, auth_base: &str, client_id: &str, email: &str) -> TokenSet {
    let verifier = zeroship_core::pkce::generate_verifier();
    let challenge = zeroship_core::pkce::s256_challenge(&verifier);
    let state = format!("st-{}", Uuid::new_v4().simple());
    let nonce = format!("nonce-{}", Uuid::new_v4().simple());
    let params = BrowserAuthorizeParams {
        code_challenge: &challenge,
        state: &state,
        nonce: &nonce,
        scope: "openid offline_access email profile",
        redirect_uri: REDIRECT_URI,
        prompt: None,
        idp_hint: None,
    };
    let auth_url = rp.build_browser_authorize_url(client_id, &params);
    let code = drive_login_to_code(auth_base, &auth_url, email).await;
    rp.exchange_code_public(client_id, &code, &verifier, REDIRECT_URI)
        .await
        .expect("exchange code for token set")
}

fn test_auth_config(db_url: &str) -> AuthConfig {
    let mut cfg = AuthConfig::parse_from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        "--db-url",
        db_url,
        "--dev-insecure",
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
        "--mail-from-email",
        "test@zeroship.test",
        "--mail-from-name",
        "Test",
        "--public-url",
        "http://localhost:0",
    ]);
    cfg.resolve(zeroship_core::config::AuthSection::default());
    cfg
}

async fn seed_user_client(
    db: &Client,
    user_id: Uuid,
    app_id: Uuid,
    client_id: &str,
    email: &str,
) {
    let phc = password::hash(PASSWORD).expect("password hash");
    db.execute(
        "INSERT INTO zeroship.users (id, email, email_verified_at, name, password_hash) \
         VALUES ($1, $2::citext, NOW(), 'Gateway E2E User', $3)",
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
            &format!("gateway-e2e-app-{}", app_id.simple()),
            &format!("api-{app_id}"),
            &format!("hash-{app_id}"),
        ],
    )
    .await
    .expect("seed app");
    let scopes = vec![
        "openid".to_string(),
        "offline_access".to_string(),
        "email".to_string(),
        "profile".to_string(),
    ];
    db.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent, \
             token_endpoint_auth_method, brokered, refresh_allowed) \
         VALUES ($1, 'Gateway OP e2e', $2, $3, TRUE, 'client_secret_basic', TRUE, TRUE)",
        &[&client_id, &vec![REDIRECT_URI.to_string()], &scopes],
    )
    .await
    .expect("seed brokered oauth client");
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

async fn cleanup(db: &Client, user_id: Uuid, app_id: Uuid, client_id: &str) {
    let _ = db
        .execute("DELETE FROM zeroship.oauth_grants WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.idp_sessions WHERE user_id = $1", &[&user_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await;
    let _ = db
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await;
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn gateway_bearer_rejects_real_op_id_token_but_accepts_access_token() {
    let Some(db_url) = db_url() else {
        eprintln!("[oidc_rp_e2e] skip (AUTH_DB_URL or CONTROL_TEST_DB unset)");
        return;
    };

    let pg_client = connect_test_db(&db_url).await;
    let signing = SigningKey::from_bytes(&[42u8; 32]);
    let broker =
        BrokerSecrets::new(BROKER_MASTER.to_vec(), None).expect("auth broker secrets");
    let issuer = Arc::new(
        Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string())
            .expect("issuer")
            .with_broker_secrets(broker),
    );
    issuer
        .publish_active_key(&pg_client)
        .await
        .expect("publish active OP key");

    let user_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let client_id = format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(&app_id));
    let email = format!("gw-op-h2-{}@zeroship.test", Uuid::new_v4().simple());
    seed_user_client(&pg_client, user_id, app_id, &client_id, &email).await;

    let srv = start_platform_op(&db_url, pg_client.clone(), issuer).await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();
    let rp = OidcRp::new(
        auth_base.clone(),
        BrokerSecret::from_bytes(BROKER_MASTER.to_vec()).expect("gateway broker secret"),
        b"gateway-e2e-stash-signing-key-32-bytes!".to_vec(),
    )
    .with_issuer(ISSUER);
    let tokens = browser_pkce_tokens(&rp, &auth_base, &client_id, &email).await;
    let id_token = tokens.id_token.as_deref().expect("openid flow returns id_token");

    let state = build_gateway_state(&auth_base, app_id, &client_id);
    let app = test::init_service(web::App::new().state(state).service(
        web::resource("/{tail}*")
            .route(web::route().to(zeroship_gateway::router::handle_subdomain)),
    ))
    .await;

    let access_req = test::TestRequest::get()
        .uri("/private")
        .header(http::header::HOST, APP_HOST)
        .header(
            http::header::AUTHORIZATION,
            format!("Bearer {}", tokens.access_token),
        )
        .to_request();
    let access_resp = test::call_service(&app, access_req).await;
    assert_eq!(
        access_resp.status().as_u16(),
        200,
        "real OP at+jwt access token must authenticate the user route"
    );

    let id_req = test::TestRequest::get()
        .uri("/private")
        .header(http::header::HOST, APP_HOST)
        .header(http::header::AUTHORIZATION, format!("Bearer {id_token}"))
        .to_request();
    let id_resp = test::call_service(&app, id_req).await;
    assert_eq!(
        id_resp.status().as_u16(),
        401,
        "OP id_token must not authenticate as a Bearer access token"
    );

    cleanup(&pg_client, user_id, app_id, &client_id).await;
    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn gateway_bearer_rejects_access_token_for_different_resource_audience() {
    let Some(db_url) = db_url() else {
        eprintln!("[oidc_rp_e2e] skip (AUTH_DB_URL or CONTROL_TEST_DB unset)");
        return;
    };

    let pg_client = connect_test_db(&db_url).await;
    let signing = SigningKey::from_bytes(&[42u8; 32]);
    let issuer = Arc::new(
        Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string()).expect("issuer"),
    );
    issuer
        .publish_active_key(&pg_client)
        .await
        .expect("publish active OP key");

    let app_id = Uuid::new_v4();
    let client_id = format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(&app_id));
    let srv = start_platform_op(&db_url, pg_client, issuer.clone()).await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();
    let state = build_gateway_state(&auth_base, app_id, &client_id);
    let app = test::init_service(web::App::new().state(state).service(
        web::resource("/{tail}*")
            .route(web::route().to(zeroship_gateway::router::handle_subdomain)),
    ))
    .await;

    let wrong_resource_audience = format!("app:{}", Uuid::new_v4());
    let scopes = vec!["openid".to_string(), "email".to_string()];
    let global_user = Uuid::new_v4().to_string();
    let wrong_aud_token = issuer
        .issue_principal_access_token(&PrincipalAccessTokenMint {
            principal_id: &global_user,
            audience: &wrong_resource_audience,
            client_id: &client_id,
            scopes: &scopes,
            ttl_secs: Some(300),
        })
        .expect("mint wrong-audience access token");

    let req = test::TestRequest::get()
        .uri("/private")
        .header(http::header::HOST, APP_HOST)
        .header(
            http::header::AUTHORIZATION,
            format!("Bearer {wrong_aud_token}"),
        )
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status().as_u16(),
        401,
        "access token with the route client_id but a different resource aud must reject"
    );

    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn gateway_oidc_rp_full_dance_against_platform_op() {
    let Some(db_url) = db_url() else {
        eprintln!("[oidc_rp_e2e] skip (AUTH_DB_URL or CONTROL_TEST_DB unset)");
        return;
    };

    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[oidc_rp_e2e] pg connection driver: {e}");
        }
    })
    .detach();
    let pg_client = Arc::new(pg_client);

    let signing = SigningKey::from_bytes(&[42u8; 32]);
    let broker =
        BrokerSecrets::new(BROKER_MASTER.to_vec(), None).expect("auth broker secrets");
    let issuer = Arc::new(
        Issuer::from_signing_key(&signing, [9u8; 32], ISSUER.to_string())
            .expect("issuer")
            .with_broker_secrets(broker),
    );
    issuer
        .publish_active_key(&pg_client)
        .await
        .expect("publish active OP key");

    let user_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();
    let client_id = format!("oac_{}", zeroship_core::typed_id::uuid_to_base62(&app_id));
    let email = format!("gw-op-{}@zeroship.test", Uuid::new_v4().simple());
    seed_user_client(&pg_client, user_id, app_id, &client_id, &email).await;

    let mut cfg = test_auth_config(&db_url);
    let key_dir = std::env::temp_dir().join(format!("gateway-op-refresh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&key_dir).expect("refresh key dir");
    let hash_key_file = key_dir.join("refresh-hmac.keys");
    let idem_key_file = key_dir.join("refresh-idem.key");
    write_secret_file(
        &hash_key_file,
        b"1:000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    );
    write_secret_file(&idem_key_file, b"refresh-idem-key-material-32-bytes");
    cfg.refresh_hash_key_file = Some(hash_key_file);
    cfg.refresh_idem_key_file = Some(idem_key_file);
    let cfg = Arc::new(cfg);
    let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
    let srv = {
        let cfg_state = cfg.clone();
        let db_state = pg_client.clone();
        let issuer_state = issuer.clone();
        let refresh_pool_state = refresh_pool.clone();
        web::test::server(move || {
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
        .await
    };
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    let rp = OidcRp::new(
        auth_base.clone(),
        BrokerSecret::from_bytes(BROKER_MASTER.to_vec()).expect("gateway broker secret"),
        b"gateway-e2e-stash-signing-key-32-bytes!".to_vec(),
    )
    .with_issuer(ISSUER);

    let (auth_url, stash) =
        rp.build_authorize_redirect(&client_id, "/some/path", REDIRECT_URI);
    assert!(auth_url.starts_with(&format!("{auth_base}/oauth2/authorize?")), "{auth_url}");
    assert!(auth_url.contains(&format!("client_id={client_id}")), "{auth_url}");
    assert!(auth_url.contains("scope=openid+offline_access+email+profile"), "{auth_url}");

    let http = cyper::Client::new();

    let first = http
        .request(http::Method::GET, &auth_url)
        .expect("build GET /authorize")
        .send()
        .await
        .expect("send GET /authorize");
    assert_eq!(first.status().as_u16(), 303);
    let login_loc = location(&first);
    assert!(login_loc.starts_with("/login?return_to="), "login redirect: {login_loc}");
    let return_to = relative_query_param(&login_loc, "return_to").expect("return_to");

    let login_get = http
        .request(http::Method::GET, format!("{auth_base}{login_loc}"))
        .expect("build GET /login")
        .send()
        .await
        .expect("send GET /login");
    assert_eq!(login_get.status().as_u16(), 200);
    let csrf = read_set_cookie(&login_get, "zsidp_csrf").expect("login csrf");
    let login_body = form(&[
        ("csrf", csrf.as_str()),
        ("email", email.as_str()),
        ("password", PASSWORD),
        ("return_to", return_to.as_str()),
    ]);
    let login_post = http
        .request(http::Method::POST, format!("{auth_base}/login"))
        .expect("build POST /login")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("zsidp_csrf={csrf}"))
        .expect("cookie")
        .body(login_body)
        .send()
        .await
        .expect("send POST /login");
    assert_eq!(login_post.status().as_u16(), 303);
    assert_eq!(location(&login_post), return_to);
    let session = read_set_cookie(&login_post, "zsidp_session").expect("session cookie");

    let final_authorize = http
        .request(http::Method::GET, format!("{auth_base}{return_to}"))
        .expect("build GET /authorize with session")
        .header("cookie", format!("zsidp_session={session}"))
        .expect("cookie")
        .send()
        .await
        .expect("send GET /authorize with session");
    assert_eq!(final_authorize.status().as_u16(), 303);
    let cb_url = location(&final_authorize);
    assert!(cb_url.starts_with(REDIRECT_URI), "callback: {cb_url}");
    assert_eq!(query_param(&cb_url, "state").as_deref(), query_param(&auth_url, "state").as_deref());
    assert_eq!(query_param(&cb_url, "iss").as_deref(), Some(ISSUER));
    let code = query_param(&cb_url, "code").expect("code param");
    let state = query_param(&cb_url, "state").expect("state param");

    // MAJOR-1 regression: redeeming this stash under a DIFFERENT route client_id
    // must fail closed (per-app isolation is an enforced invariant, not merely an
    // emergent property of __Host- cookie origin-isolation). The client check is
    // before the code exchange, so the one-time code is untouched for the real
    // call below.
    let mismatch = rp
        .finish_callback(&code, &state, &stash, "oac_someotherapp000000000000")
        .await;
    assert!(
        matches!(mismatch, Err(zeroship_gateway::oidc_rp::OidcRpError::ClientMismatch)),
        "stash redeemed under a mismatched route client_id must fail ClientMismatch, got {mismatch:?}"
    );

    let (claims, original_path, granted_scopes) = rp
        .finish_callback(&code, &state, &stash, &client_id)
        .await
        .expect("finish_callback");
    assert_eq!(original_path, "/some/path");
    assert_eq!(claims.sub, user_id.to_string());
    assert!(granted_scopes.contains(&"openid".to_string()));
    assert!(granted_scopes.contains(&"offline_access".to_string()));

    let (mut sess_client, sess_connection) =
        compio_postgres::connect(&db_url, NoTls)
            .await
            .expect("connect pg (session store)");
    compio::runtime::spawn(async move {
        if let Err(e) = sess_connection.run().await {
            eprintln!("[oidc_rp_e2e] session-store pg driver: {e}");
        }
    })
    .detach();
    let session = create(
        &mut sess_client,
        &NewSession {
            user_id: &claims.sub,
            sid: claims.sid.as_deref(),
            app_id,
            email: claims.email.as_deref(),
            name: claims.name.as_deref(),
            avatar_url: claims.picture.as_deref(),
            email_verified: claims.email_verified.unwrap_or(false),
            granted_scopes: &granted_scopes,
            auth_time: claims.auth_time,
            amr: claims.amr.as_deref().unwrap_or(&[]),
        },
    )
    .await
    .expect("session create");

    let validated = validate(&mut sess_client, session.id, app_id)
        .await
        .expect("validate")
        .expect("session validates");
    assert_eq!(validated.user_id, claims.sub);
    assert_eq!(validated.granted_scopes, granted_scopes);

    revoke_app_sessions_for_user(&mut sess_client, app_id, &claims.sub)
        .await
        .expect("revoke");
    let after_revoke = validate(&mut sess_client, session.id, app_id)
        .await
        .expect("validate post-revoke");
    assert!(after_revoke.is_none(), "session must not validate after revoke");

    cleanup(&pg_client, user_id, app_id, &client_id).await;
    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
}
