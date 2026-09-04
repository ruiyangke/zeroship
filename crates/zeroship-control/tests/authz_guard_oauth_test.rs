//! Regression coverage for AuthzGuard's platform OAuth bearer branch.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use compio_postgres::{connect, NoTls};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ntex::http::StatusCode;
use ntex::web::{self, test, HttpResponse};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    api, authz_guard::AuthzGuard, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};
use zeroship_authz::{Action, Resource};
use zeroship_core::auth_provider::{AuthProvider, PlatformConfig, PlatformProvider};
use zeroship_core::config::{Secret, SourceKind};
use zeroship_core::device_grant::{
    OP_PROVIDER, PLATFORM_CLI_CLIENT_ID, PLATFORM_CLI_REGISTERED_SCOPES,
};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const PLATFORM_ISSUER: &str = "https://auth.zeroship.test";
const PLATFORM_OP_ISSUER: &str = "https://auth.zeroship.test/oauth2";
const PLATFORM_KID: &str = "platform-control-authz-kid";
const PLATFORM_KEY_SEED: u8 = 31;

fn db_url() -> String {
    crate::common::require_control_db()
}

fn auth_role_db_url(database_url: &str) -> String {
    let mut parsed = url::Url::parse(database_url).expect("database URL must be absolute");
    parsed
        .set_username("zeroship_auth")
        .expect("set auth database user");
    parsed
        .set_password(Some("zeroship_auth"))
        .expect("set auth database password");
    parsed.to_string()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-authz-guard-oauth-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    user_id: Uuid,
    app_id: Option<Uuid>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    _jwks: Option<PlatformJwksMock>,
    _op: Option<PlatformOp>,
}

impl Fixture {
    async fn cleanup(&self) {
        let _ = self
            .state
            .control_pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&self.user_id],
            )
            .await;
        if let Some(app_id) = self.app_id {
            let app_id_text = app_id.to_string();
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.app_members WHERE app_id = $1 OR user_id = $2",
                    &[&app_id_text, &self.user_id],
                )
                .await;
            let _ = self
                .state
                .control_pg
                .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
                .await;
        }
        let _ = self
            .state
            .control_pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&self.user_id])
            .await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn fixture_with_platform(label: &str, user_id: Uuid) -> Option<Fixture> {
    let jwks = PlatformJwksMock::start();
    let auth_provider = platform_auth_provider(jwks.jwks_url());
    fixture_with_auth_provider(label, user_id, auth_provider, Some(jwks)).await
}

async fn fixture_with_auth_provider(
    label: &str,
    user_id: Uuid,
    auth_provider: Arc<AuthProvider>,
    jwks: Option<PlatformJwksMock>,
) -> Option<Fixture> {
    let db_url = db_url();

    let (control_pg_client, control_pg_conn) = connect(&db_url, NoTls).await.expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();

    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
    let registry = Registry::new(&db_url).await.expect("registry");
    zeroship_control::plan_catalog::seed_plans(&registry).await.expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
            workflow_blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new(String::new()),
        stripe_base_url: "https://api.stripe.com".to_string(),
        gateway_url: "http://127.0.0.1:9".to_string(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        origin_scheme: zeroship_core::config::OriginScheme::Https,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg: Arc::new(control_pg_client),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        auth_provider,
        // No platform deploy-token mint here: that is control's OUTBOUND
        // destination for the device flow, and no fixture below drives one.
        provider_registry: zeroship_control::metering::provider::builtin_registry(),
        billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
        billing_stream: None,
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: std::sync::Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    });

    insert_user(&state, user_id, label).await;
    Some(Fixture {
        state,
        user_id,
        app_id: None,
        blob_root,
        deploy_tmp_dir,
        _jwks: jwks,
        _op: None,
    })
}

async fn insert_user(state: &AppState, user_id: Uuid, label: &str) {
    let email = format!("{label}-{user_id}@zeroship.test");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, $3, NOW())",
            &[&user_id, &email, &label],
        )
        .await
        .expect("insert oauth test user");
}

/// Create an app owned by the fixture's principal (binds the owner membership).
async fn create_app(fx: &mut Fixture, label: &str) -> Uuid {
    let owner = fx.user_id;
    create_app_owned_by(fx, label, owner).await
}

/// Create an app owned by an arbitrary `owner_id` (which may differ from the
/// fixture principal). Used to exercise the "principal is only a viewer of an
/// app someone else owns" case — the create now binds an owner membership, so
/// tests that need the principal to NOT be the owner must seed a distinct owner.
async fn create_app_owned_by(fx: &mut Fixture, label: &str, owner_id: Uuid) -> Uuid {
    if owner_id != fx.user_id {
        // The owner must exist (FK on app_members.user_id → users.id).
        insert_user(&fx.state, owner_id, &format!("{label}-owner")).await;
    }
    let app_name = format!("{label}-{}", Uuid::new_v4().simple());
    let record = fx
        .state
        .registry
        .create_app(&app_name, &zeroship_control::plan_catalog::free_plan_id(), &owner_id)
        .await
        .expect("create app");
    fx.app_id = Some(record.id);
    record.id
}

async fn grant_app_member(state: &AppState, app_id: Uuid, user_id: Uuid, role: &str) {
    // app_members.app_id is a uuid column — bind the Uuid directly (binding a
    // String panics with WrongType against the uuid column).
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, $3)",
            &[&app_id, &user_id, &role],
        )
        .await
        .expect("insert app member");
}

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(
            web::App::new()
                .state($fx.state.clone())
                .service(
                    web::resource("/api/apps")
                        .route(web::post().to(api::create_app))
                        .route(web::get().to(api::list_apps)),
                )
                .service(
                    web::resource("/api/apps/{id}")
                        .route(web::get().to(api::get_app)),
                )
                .service(
                    web::resource("/api/apps/{id}/archive")
                        .route(web::put().to(api::archive_app))
                        .route(web::delete().to(api::unarchive_app)),
                )
                .service(
                    web::resource("/api/apps/{id}/deploy")
                        .route(web::post().to(api::deploy)),
                )
                .service(
                    web::resource("/raw-app/{id}")
                        .route(web::get().to(raw_app_read)),
                )
                .service(
                    web::resource("/raw-app/{id}/deploy-check")
                        .route(web::post().to(raw_app_deploy_check)),
                ),
        )
        .await
    }};
}

async fn raw_app_read(
    path: web::types::Path<String>,
    authz: AuthzGuard,
    state: web::types::State<Arc<AppState>>,
) -> web::HttpResponse {
    match authz
        .require(
            Action::AppsRead,
            Resource::App {
                id: path.into_inner(),
            },
            &state,
        )
        .await
    {
        Ok(()) => web::HttpResponse::NoContent().finish(),
        Err(resp) => resp,
    }
}

async fn raw_app_deploy_check(
    path: web::types::Path<String>,
    authz: AuthzGuard,
    state: web::types::State<Arc<AppState>>,
) -> web::HttpResponse {
    match authz
        .require(
            Action::AppsDeploy,
            Resource::App {
                id: path.into_inner(),
            },
            &state,
        )
        .await
    {
        Ok(()) => web::HttpResponse::Ok().json(&json!({
            "principal_id": authz.principal_id.to_string(),
        })),
        Err(resp) => resp,
    }
}

fn bearer_for(token: &str) -> String {
    format!("Bearer {token}")
}

fn bearer_for_scope(subject: Uuid, scope: &str) -> String {
    bearer_for(&platform_token(subject, scope, PLATFORM_ISSUER))
}

fn bearer_for_subject(subject: impl ToString, scope: &str) -> String {
    bearer_for(&platform_token_subject(
        subject,
        scope,
        PLATFORM_ISSUER,
        vec!["control.zeroship.ai"],
        "zeroship-cli",
    ))
}

fn platform_auth_provider(jwks_url: String) -> Arc<AuthProvider> {
    platform_auth_provider_for(PLATFORM_ISSUER, jwks_url)
}

fn platform_auth_provider_for(issuer: &str, jwks_url: String) -> Arc<AuthProvider> {
    Arc::new(AuthProvider::platform(PlatformProvider::new(
        PlatformConfig::new(issuer, Some(jwks_url)).expect("platform config"),
    )))
}

fn platform_token(subject: Uuid, scope: &str, issuer: &str) -> String {
    platform_token_with_client_id(subject, scope, issuer, "zeroship-cli")
}

fn platform_token_with_client_id(
    subject: Uuid,
    scope: &str,
    issuer: &str,
    client_id: &str,
) -> String {
    platform_token_subject(
        subject.to_string(),
        scope,
        issuer,
        vec!["control.zeroship.ai"],
        client_id,
    )
}

fn platform_token_subject(
    subject: impl ToString,
    scope: &str,
    issuer: &str,
    aud: Vec<&str>,
    client_id: &str,
) -> String {
    let now = unix_now_secs();
    let claims = json!({
        "iss": issuer,
        "sub": subject.to_string(),
        "aud": aud,
        "exp": now + 3600,
        "iat": now,
        "nbf": now.saturating_sub(1),
        "jti": Uuid::new_v4().to_string(),
        "client_id": client_id,
        "scope": scope,
    });
    let mut header = Header::new(Algorithm::EdDSA);
    header.typ = Some("at+jwt".to_string());
    header.kid = Some(PLATFORM_KID.to_string());
    encode(&header, &claims, &platform_encoding_key()).expect("platform token")
}

fn platform_encoding_key() -> EncodingKey {
    let sk = SigningKey::from_bytes(&[PLATFORM_KEY_SEED; 32]);
    let pkcs8 = sk.to_pkcs8_der().expect("encode pkcs8");
    EncodingKey::from_ed_der(pkcs8.as_bytes())
}

fn platform_jwks_body() -> String {
    let sk = SigningKey::from_bytes(&[PLATFORM_KEY_SEED; 32]);
    json!({
        "keys": [{
            "kid": PLATFORM_KID,
            "kty": "OKP",
            "alg": "EdDSA",
            "crv": "Ed25519",
            "x": URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes()),
        }]
    })
    .to_string()
}

struct PlatformJwksMock {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PlatformJwksMock {
    fn start() -> Self {
        let body = Arc::new(RwLock::new(platform_jwks_body()));
        let factory_body = body.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("control-platform-jwks-mock")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let server = web::test::server(move || {
                        let body = factory_body.clone();
                        async move {
                            web::App::new().state(body).service(
                                web::resource("/.well-known/jwks.json")
                                    .route(web::get().to(platform_jwks_handler)),
                            )
                        }
                    })
                    .await;
                    let addr = server.addr();
                    started_tx.send(addr).expect("send platform jwks addr");
                    let _ = shutdown_rx.recv();
                    drop(server);
                });
        });
        let addr = started_rx.recv().expect("platform jwks mock starts");
        Self {
            base: format!("http://{addr}"),
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn jwks_url(&self) -> String {
        format!("{}/.well-known/jwks.json", self.base)
    }
}

impl Drop for PlatformJwksMock {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn platform_jwks_handler(body: web::types::State<Arc<RwLock<String>>>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("application/json")
        .body(body.read().expect("jwks body lock").clone())
}

struct PlatformOp {
    base: String,
    issuer: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PlatformOp {
    fn start(database_url: String) -> Self {
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("control-platform-op")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let (pg_client, pg_connection) = connect(&database_url, NoTls)
                        .await
                        .expect("platform OP pg connect");
                    compio::runtime::spawn(async move {
                        let _ = pg_connection.run().await;
                    })
                    .detach();
                    let pg = Arc::new(pg_client);

                    zeroship_auth::oidc::device_token::reconcile_platform_cli_client(
                        pg.as_ref(),
                    )
                    .await
                    .expect("reconcile platform CLI client as zeroship_auth");
                    zeroship_auth::oidc::device_token::reconcile_platform_cli_client(
                        pg.as_ref(),
                    )
                    .await
                    .expect("platform CLI reconciliation is idempotent");

                    let mut cfg = zeroship_auth::config::AuthConfig::parse_from([
                        "zeroship-auth",
                        "--addr",
                        "127.0.0.1:0",
                        "--frame-ancestor-origins",
                        "https://console.zeroship.test",
                        "--mail-from-email",
                        "test@zeroship.test",
                        "--mail-from-name",
                        "Test",
                        "--public-url",
                        PLATFORM_ISSUER,
                    ]);
                    cfg.settings.database_url =
                        Secret::supplied(SourceKind::Env, Some(database_url.clone()));
                    cfg.settings.stash_signing_key = Secret::supplied(
                        SourceKind::Env,
                        Some("test-stash-key-not-for-prod-32bytes!".to_string()),
                    );
                    cfg.settings.totp_enc_key = Secret::supplied(
                        SourceKind::Env,
                        Some(
                            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f"
                                .to_string(),
                        ),
                    );
                    let cfg = Arc::new(cfg);
                    let issuer_url = cfg.op_issuer_url();
                    assert_eq!(issuer_url, PLATFORM_OP_ISSUER);
                    let signing = SigningKey::generate(&mut rand::rngs::OsRng);
                    let issuer = Arc::new(
                        zeroship_auth::oidc::Issuer::from_signing_key(
                            &signing,
                            [17_u8; 32],
                            issuer_url.clone(),
                        )
                        .expect("platform OP issuer"),
                    );
                    let signing_kid = issuer.kid().to_string();
                    let public_jwk = issuer.public_jwk().clone();
                    pg.execute(
                        "INSERT INTO zeroship.signing_keys \
                            (kid, alg, public_jwk, status, activated_at) \
                         VALUES ($1, 'EdDSA', $2, 'active', NOW())",
                        &[&signing_kid, &public_jwk],
                    )
                    .await
                    .expect("register isolated platform OP key");
                    let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(
                        database_url.clone(),
                        2,
                    );

                    let cfg_state = cfg.clone();
                    let pg_state = pg.clone();
                    let issuer_state = issuer.clone();
                    let refresh_pool_state = refresh_pool.clone();
                    let server = web::test::server(move || {
                        let cfg_state = cfg_state.clone();
                        let pg_state = pg_state.clone();
                        let issuer_state = issuer_state.clone();
                        let refresh_pool_state = refresh_pool_state.clone();
                        async move {
                            web::App::new()
                                .state(cfg_state)
                                .state(pg_state)
                                .state(issuer_state)
                                .state(refresh_pool_state)
                                .configure(zeroship_auth::server::configure(false, false))
                        }
                    })
                    .await;
                    let _ = started_tx.send((server.addr(), issuer_url));
                    loop {
                        match shutdown_rx.try_recv() {
                            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
                            Err(mpsc::TryRecvError::Empty) => {
                                compio::time::sleep(StdDuration::from_millis(10)).await;
                            }
                        }
                    }
                    drop(server);
                    pg.execute(
                        "DELETE FROM zeroship.signing_keys WHERE kid = $1",
                        &[&signing_kid],
                    )
                    .await
                    .expect("remove isolated platform OP key");
                });
        });
        let (addr, issuer) = started_rx
            .recv_timeout(StdDuration::from_secs(15))
            .expect("platform OP starts within 15 seconds");
        Self {
            base: format!("http://{addr}"),
            issuer,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn jwks_url(&self) -> String {
        format!("{}/oauth2/.well-known/jwks.json", self.base)
    }
}

impl Drop for PlatformOp {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn assert_platform_cli_registration(pg: &compio_postgres::Client) {
    let row = pg
        .query_one(
            "SELECT client_name, client_uri, logo_uri, redirect_uris, scopes, \
                    skip_consent, created_by, client_secret_hash, refresh_allowed, \
                    token_endpoint_auth_method, brokered, backchannel_logout_uri, \
                    NOT EXISTS ( \
                        SELECT 1 FROM zeroship.app_oauth_clients aoc \
                        WHERE aoc.client_id = oauth_clients.client_id \
                    ) AS has_no_app_extension \
             FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&PLATFORM_CLI_CLIENT_ID],
        )
        .await
        .expect("load reconciled platform CLI client");
    // The REGISTERED list, which is the issuable ceiling plus `offline_access`:
    // the CLI has to be able to ask for a refresh token, and the
    // device-authorization endpoint checks the request against this column.
    let expected_scopes = PLATFORM_CLI_REGISTERED_SCOPES
        .iter()
        .map(|scope| (*scope).to_string())
        .collect::<Vec<_>>();
    assert_eq!(row.get::<_, String>("client_name"), "zeroship CLI");
    assert!(row.get::<_, Option<String>>("client_uri").is_none());
    assert!(row.get::<_, Option<String>>("logo_uri").is_none());
    assert!(row.get::<_, Vec<String>>("redirect_uris").is_empty());
    assert_eq!(row.get::<_, Vec<String>>("scopes"), expected_scopes);
    assert!(row.get::<_, bool>("skip_consent"));
    assert!(row.get::<_, Option<Uuid>>("created_by").is_none());
    assert!(row.get::<_, Option<String>>("client_secret_hash").is_none());
    assert!(row.get::<_, bool>("refresh_allowed"));
    assert_eq!(row.get::<_, String>("token_endpoint_auth_method"), "none");
    assert!(!row.get::<_, bool>("brokered"));
    assert!(row
        .get::<_, Option<String>>("backchannel_logout_uri")
        .is_none());
    assert!(row.get::<_, bool>("has_no_app_extension"));
}

#[compio::test]
async fn op_cli_device_token_authorizes_control_endpoint() {
    let user_id = Uuid::new_v4();
    let database_url = db_url();
    let op = PlatformOp::start(auth_role_db_url(&database_url));
    let auth_provider = platform_auth_provider_for(&op.issuer, op.jwks_url());
    let Some(mut fx) = fixture_with_auth_provider(
        "op-cli-control",
        user_id,
        auth_provider,
        None,
    )
    .await
    else {
        return;
    };
    fx._op = Some(op);
    assert_platform_cli_registration(&fx.state.control_pg).await;
    let app_id = create_app(&mut fx, "op-cli-control").await;

    let http = cyper::Client::new();
    let rejected_scope_form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", PLATFORM_CLI_CLIENT_ID)
        .append_pair("scope", "apps:deploy billing:write")
        .finish();
    let rejected_scope = http
        .request(
            http::Method::POST,
            format!(
                "{}/oauth2/device/authorization",
                fx._op.as_ref().expect("platform OP fixture").base
            ),
        )
        .expect("build rejected OP device authorization request")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("rejected device authorization content type")
        .body(rejected_scope_form)
        .send()
        .await
        .expect("send rejected OP device authorization request");
    let rejected_scope_status = rejected_scope.status().as_u16();
    let rejected_scope_body = rejected_scope
        .text()
        .await
        .expect("read rejected OP device authorization response");
    assert_eq!(
        rejected_scope_status, 400,
        "out-of-policy CLI scope was accepted: {rejected_scope_body}"
    );
    let rejected_scope: Value = serde_json::from_str(&rejected_scope_body)
        .expect("decode rejected OP device authorization response");
    assert_eq!(rejected_scope["error"], "invalid_scope");

    let authorization_form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", PLATFORM_CLI_CLIENT_ID)
        .append_pair("scope", "apps:deploy apps:read")
        .finish();
    let authorization = http
        .request(
            http::Method::POST,
            format!(
                "{}/oauth2/device/authorization",
                fx._op.as_ref().expect("platform OP fixture").base
            ),
        )
        .expect("build OP device authorization request")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("device authorization content type")
        .body(authorization_form)
        .send()
        .await
        .expect("send OP device authorization request");
    let authorization_status = authorization.status().as_u16();
    let authorization_body = authorization
        .text()
        .await
        .expect("read OP device authorization response");
    assert_eq!(
        authorization_status, 200,
        "OP device authorization failed: {authorization_body}"
    );
    let authorization: Value = serde_json::from_str(&authorization_body)
        .expect("decode OP device authorization response");
    let device_code = authorization["device_code"]
        .as_str()
        .expect("device authorization returns device_code");
    let user_code = authorization["user_code"]
        .as_str()
        .expect("device authorization returns user_code");
    let sid = Uuid::new_v4().to_string();
    let approved = fx
        .state
        .control_pg
        .execute(
            "UPDATE zeroship.device_grants \
             SET principal_id = $1, sid = $2, \
                 auth_credential_version = \
                    (SELECT credential_version FROM zeroship.users WHERE id = $1), \
                 status = 'approved' \
             WHERE user_code = $3 AND provider = $4",
            &[&user_id, &sid, &user_code, &OP_PROVIDER],
        )
        .await
        .expect("approve OP device grant");
    assert_eq!(approved, 1, "approve exactly one OP device grant");

    let token_form = url::form_urlencoded::Serializer::new(String::new())
        .append_pair(
            "grant_type",
            "urn:ietf:params:oauth:grant-type:device_code",
        )
        .append_pair("device_code", device_code)
        .append_pair("client_id", PLATFORM_CLI_CLIENT_ID)
        .finish();
    let token_response = http
        .request(
            http::Method::POST,
            format!(
                "{}/oauth2/token",
                fx._op.as_ref().expect("platform OP fixture").base
            ),
        )
        .expect("build OP device token request")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("device token content type")
        .body(token_form)
        .send()
        .await
        .expect("send OP device token request");
    let token_status = token_response.status().as_u16();
    let token_body = token_response
        .text()
        .await
        .expect("read OP device token response");
    assert_eq!(token_status, 200, "OP token exchange failed: {token_body}");
    let token: Value = serde_json::from_str(&token_body).expect("decode OP token response");
    let access_token = token["access_token"]
        .as_str()
        .expect("OP token response returns access_token");
    let encoded_claims = access_token
        .split('.')
        .nth(1)
        .expect("OP access token contains claims");
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded_claims)
            .expect("decode OP access token claims"),
    )
    .expect("parse OP access token claims");
    assert_eq!(claims["sub"], user_id.to_string());
    assert_eq!(claims["aud"], "control.zeroship.ai");
    assert_eq!(claims["client_id"], PLATFORM_CLI_CLIENT_ID);
    assert_eq!(claims["scope"], "apps:deploy apps:read");
    // Minutes, not the 12-hour ceiling this assertion used to pin at 43_200.
    //
    // The bound is a LITERAL on purpose. Comparing to
    // `zeroship_auth::oidc::ACCESS_TOKEN_TTL_SECS` - which is what stood here
    // briefly - proves only that control sees what auth emitted, and passes
    // for any value of it: the constant was set to `12 * 60 * 60` and every
    // gate in the tree stayed green.
    //
    // This is the CONSUMER's vantage. `crates/auth` bounds the constant and
    // the emitted token; here the token has crossed a service boundary and
    // been parsed by the crate that actually authorizes with it.
    let lifetime = claims["exp"].as_i64().expect("OP token exp")
        - claims["iat"].as_i64().expect("OP token iat");
    assert!(
        (120..=30 * 60).contains(&lifetime),
        "control received a {lifetime}s CLI access token; it must be minutes, because \
         nothing recalls a bearer this long-lived except the token_revocations marker"
    );

    let control = init_control!(fx);
    let deploy_request = test::TestRequest::post()
        .uri(&format!("/raw-app/{app_id}/deploy-check"))
        .header("authorization", bearer_for(access_token))
        .to_request();
    let deploy_response = test::call_service(&control, deploy_request).await;
    let deploy_status = deploy_response.status();
    let deploy_body = test::read_body(deploy_response).await;
    let deploy_body_text = String::from_utf8_lossy(&deploy_body).to_string();

    let app_request = test::TestRequest::get()
        .uri(&format!("/api/apps/{app_id}"))
        .header("authorization", bearer_for(access_token))
        .to_request();
    let app_response = test::call_service(&control, app_request).await;
    let app_status = app_response.status();
    let app_body = test::read_body(app_response).await;
    let app_body_text = String::from_utf8_lossy(&app_body).to_string();

    fx.cleanup().await;
    drop(control);
    drop(http);
    drop(fx);
    common::drain_pg().await;

    assert_eq!(
        deploy_status,
        StatusCode::OK,
        "OP-issued CLI token was rejected by control deploy authz: {deploy_body_text}"
    );
    let control_body: Value =
        serde_json::from_str(&deploy_body_text).expect("decode control response");
    assert_eq!(control_body["principal_id"], user_id.to_string());
    assert_eq!(
        app_status,
        StatusCode::OK,
        "OP-issued CLI token was rejected by a production control endpoint: {app_body_text}"
    );
    // The response's advertised lifetime, bounded by the same literals as the
    // token's own `exp - iat` above, and asserted to AGREE with it. Agreement
    // is the wiring; the bound is the value. This assertion read `43_200`
    // until the CLI moved to this grant, and it is the second of two lifetime
    // assertions in this test - updating only the first is how a 12-hour
    // `expires_in` would have survived here.
    let advertised = token["expires_in"].as_i64().expect("OP token expires_in");
    assert_eq!(
        advertised, lifetime,
        "the response advertises {advertised}s but the token itself lives {lifetime}s"
    );
    assert!(
        (120..=30 * 60).contains(&advertised),
        "the OP advertised a {advertised}s CLI access token; it must be minutes"
    );
    assert_eq!(token["scope"], "apps:deploy apps:read");
}

#[compio::test]
async fn platform_issuer_accepts_valid_token_and_rejects_unknown_issuer() {
    let user_id = Uuid::new_v4();
    let Some(mut fx) = fixture_with_platform("platform-issuer", user_id).await else {
        return;
    };
    let app_id = create_app(&mut fx, "platform-issuer").await;
    let app = init_control!(fx);

    let platform_deploy = platform_token(user_id, "apps:deploy", PLATFORM_ISSUER);
    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{app_id}/deploy-check"))
        .header("authorization", bearer_for(&platform_deploy))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("platform body json");
    assert_eq!(body["principal_id"], user_id.to_string());

    let unknown_issuer = platform_token(user_id, "apps:read", "https://unknown-issuer.test");
    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer_for(&unknown_issuer))
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    fx.cleanup().await;

    // Teardown: the service and the fixture both hold connections, and locals
    // are dropped only after the body returns - by which point the runtime is
    // gone and the sockets can no longer be closed. Drop them explicitly, then
    // wait for the close to land.
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn platform_access_token_revocation_marker_rejects_within_cache_ttl() {
    let user_id = Uuid::new_v4();
    let Some(mut fx) = fixture_with_platform("platform-revoked", user_id).await else {
        return;
    };
    let app_id = create_app(&mut fx, "platform-revoked").await;
    let app = init_control!(fx);

    let token = platform_token(user_id, "apps:deploy", PLATFORM_ISSUER);
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
             VALUES ('zeroship-cli', $1, NOW() + INTERVAL '5 seconds') \
             ON CONFLICT (client_id, sub) DO UPDATE \
             SET revoked_after = EXCLUDED.revoked_after",
            &[&user_id.to_string()],
        )
        .await
        .expect("insert platform token revocation marker");

    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{app_id}/deploy-check"))
        .header("authorization", bearer_for(&token))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    fx.state
        .control_pg
        .execute(
            "DELETE FROM zeroship.token_revocations WHERE client_id = 'zeroship-cli' AND sub = $1",
            &[&user_id.to_string()],
        )
        .await
        .expect("cleanup platform token revocation marker");
    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn bearer_verifier_directly_accepts_oauth_and_rejects_revoked_platform_token() {
    let user_id = Uuid::new_v4();
    let Some(fx) = fixture_with_platform("bearer-verifier", user_id).await else {
        return;
    };

    let oauth_token = platform_token(user_id, "apps:read apps:deploy", PLATFORM_ISSUER);
    let oauth = fx
        .state
        .bearer_verifier()
        .verify_bearer(&oauth_token, None, "direct-oauth".to_string())
        .await
        .expect("OAuth bearer verifies directly");
    assert_eq!(oauth.principal_id, user_id);
    assert!(oauth.token_policy.is_some());

    // A bearer that is not a platform OAuth token is refused outright. There is
    // no second local verifier behind the OAuth arm any more: the PAT branch
    // that used to run first is gone with the token type.
    assert!(
        fx.state
            .bearer_verifier()
            .verify_bearer(
                "not-a-jwt",
                Some("127.0.0.1".parse().expect("test IP parses")),
                "direct-garbage".to_string(),
            )
            .await
            .is_err(),
        "a bearer the platform issuer did not sign must be rejected"
    );

    let revoked_client_id = "zeroship-cli-revoked";
    let platform =
        platform_token_with_client_id(user_id, "apps:deploy", PLATFORM_ISSUER, revoked_client_id);
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
             VALUES ($2, $1, NOW() + INTERVAL '5 seconds') \
             ON CONFLICT (client_id, sub) DO UPDATE \
             SET revoked_after = EXCLUDED.revoked_after",
            &[&user_id.to_string(), &revoked_client_id],
        )
        .await
        .expect("insert direct platform token revocation marker");
    assert!(
        fx.state
            .bearer_verifier()
            .verify_bearer(&platform, None, "direct-revoked".to_string())
            .await
            .is_err(),
        "revoked platform bearer must be rejected by BearerVerifier"
    );

    let _ = fx
        .state
        .control_pg
        .execute(
            "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
            &[&revoked_client_id, &user_id.to_string()],
        )
        .await;
    fx.cleanup().await;

    // No `app`/ntex test service in this test - it drives `bearer_verifier()`
    // directly. Only the fixture's Postgres client needs to be dropped.
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn oauth_token_with_apps_read_can_list_apps() {
    let user_id = Uuid::new_v4();
    let Some(fx) = fixture_with_platform("apps-read", user_id).await else {
        return;
    };
    let app = init_control!(fx);
    let request_id = format!("req_h4_{}", Uuid::new_v4().simple());

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer_for_scope(user_id, "apps:read apps:deploy"))
        .header("x-request-id", request_id.as_str())
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::OK);
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT request_id \
             FROM zeroship.authz_decisions \
             WHERE actor_user_id = $1 AND action = 'apps:read' \
             ORDER BY occurred_at DESC \
             LIMIT 1",
            &[&user_id],
        )
        .await
        .expect("select authz decision");
    assert_eq!(
        rows.first()
            .and_then(|row| row.get::<_, Option<String>>("request_id"))
            .as_deref(),
        Some(request_id.as_str())
    );

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn oauth_token_owned_by_anonymized_user_returns_401() {
    let user_id = Uuid::new_v4();
    let Some(fx) = fixture_with_platform("anonymized-owner", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.users \
             SET disabled_at = NOW(), anonymized_at = NOW(), \
                 credential_version = credential_version + 1 \
             WHERE id = $1",
            &[&user_id],
        )
        .await
        .expect("anonymize OAuth owner");
    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer_for_scope(user_id, "apps:read"))
        .to_request();
    let status = test::call_service(&app, req).await.status();

    fx.cleanup().await;
    drop(app);
    drop(fx);
    common::drain_pg().await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[compio::test]
async fn oauth_token_ignores_standard_oidc_scopes() {
    let user_id = Uuid::new_v4();
    let Some(fx) = fixture_with_platform("oidc-scopes", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    // Native platform token carrying standard OIDC scopes alongside the one
    // resource-server scope. The authz path must ignore openid/offline_access/
    // profile/email/address/phone and honor only `apps:read` → GET /api/apps OK.
    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header(
            "authorization",
            bearer_for_scope(
                user_id,
                "openid offline_access profile email address phone apps:read",
            ),
        )
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::OK);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// F3 regression — drives the REAL self-service path end to end with NO
/// pre-seeded `app_members` / platform role:
///
///   1. A default-role creator (OAuth token, scopes `apps:write apps:read`)
///      POSTs `/api/apps` → 201. This exercises the broadened create gate AND
///      `registry::create_app` binding the principal as the app's `owner` row.
///   2. The same creator GETs `/api/apps` → 200 and sees EXACTLY the app they
///      just created (ownership-scoped list).
///   3. Another creator's app (owned by a different principal) is NOT visible.
///
/// Before the fix step 1 was a 403 (no policy granted a default-role creator
/// `apps:write` on `Resource::Any`) and there was no owner-binding at all, so a
/// creator was locked out of their own apps. This test must NOT pre-seed any
/// `app_members` row for the principal — the owner row has to come from the
/// production create path.
#[compio::test]
async fn creator_self_service_creates_and_lists_only_own_apps() {
    let user_id = Uuid::new_v4();
    let Some(mut fx) = fixture_with_platform("self-service", user_id).await else {
        return;
    };

    // Seed ANOTHER creator's app (different owner) directly. It must never show
    // up in this principal's scoped list.
    let other_owner = Uuid::new_v4();
    insert_user(&fx.state, other_owner, "self-service-other").await;
    let other_app = fx
        .state
        .registry
        .create_app(
            &format!("otherapp-{}", Uuid::new_v4().simple()),
            &zeroship_control::plan_catalog::free_plan_id(),
            &other_owner,
        )
        .await
        .expect("create other-owner app");

    let app = init_control!(fx);

    // 1. Create an app as the default-role creator via the production handler.
    let create_name = format!("mine-{}", Uuid::new_v4().simple());
    let req = test::TestRequest::post()
        .uri("/api/apps")
        .header("authorization", bearer_for_scope(user_id, "apps:write apps:read"))
        .set_json(&json!({ "name": create_name }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "default-role creator must be able to create their own app"
    );
    let body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("create body json");
    let created_id = body["id"].as_str().expect("created app id").to_string();
    // Track for fixture cleanup (FK cascade removes the owner membership).
    fx.app_id = Some(Uuid::parse_str(&created_id).expect("uuid"));

    // 2. List — the creator sees their app, scoped to ownership.
    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer_for_scope(user_id, "apps:write apps:read"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("list body json");
    let ids: Vec<String> = list
        .as_array()
        .expect("list is array")
        .iter()
        .map(|app| app["id"].as_str().expect("app id").to_string())
        .collect();
    assert!(
        ids.contains(&created_id),
        "creator must see their OWN app in the scoped list (got {ids:?})"
    );
    // 3. The other creator's app must NOT leak into this principal's list.
    assert!(
        !ids.contains(&other_app.id.to_string()),
        "scoped list must NOT include another tenant's app"
    );

    // Cleanup the other-owner app + user (fixture cleanup handles `created_id`).
    let _ = fx
        .state
        .control_pg
        .execute(
            "DELETE FROM zeroship.app_members WHERE app_id = $1",
            &[&other_app.id],
        )
        .await;
    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[&other_app.id])
        .await;
    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&other_owner])
        .await;
    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn oauth_token_without_required_scope_returns_403() {
    let user_id = Uuid::new_v4();
    let Some(mut fx) = fixture_with_platform("missing-deploy", user_id).await else {
        return;
    };
    let app_id = create_app(&mut fx, "missing-deploy").await;
    let app = init_control!(fx);

    let req = test::TestRequest::post()
        .uri(&format!("/api/apps/{app_id}/deploy"))
        .header("authorization", bearer_for_scope(user_id, "apps:read"))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::FORBIDDEN);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn invalid_oauth_token_returns_401() {
    let user_id = Uuid::new_v4();
    let Some(fx) = fixture_with_platform("invalid-token", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", "Bearer not-a-jwt")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = test::read_body(resp).await;
    assert!(
        !body.is_empty(),
        "invalid platform bearer should return a non-empty 401 response"
    );

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn oauth_token_wrong_audience_returns_401() {
    let user_id = Uuid::new_v4();
    let Some(fx) = fixture_with_platform("wrong-audience", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    let token = platform_token_subject(
        user_id,
        "apps:read",
        PLATFORM_ISSUER,
        vec!["gateway"],
        "zeroship-cli",
    );
    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer_for(&token))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn invalid_oauth_sub_returns_401() {
    let user_id = Uuid::new_v4();
    let Some(fx) = fixture_with_platform("invalid-sub", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer_for_subject("not-a-uuid", "apps:read"))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn unknown_scope_returns_401_not_silently_dropped() {
    let user_id = Uuid::new_v4();
    let Some(fx) = fixture_with_platform("unknown-scope", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer_for_scope(user_id, "apps:read bogus:scope"))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn invalid_app_resource_id_returns_400_before_cedar() {
    let user_id = Uuid::new_v4();
    let Some(fx) = fixture_with_platform("invalid-resource-id", user_id).await else {
        return;
    };
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/raw-app/app%22%3B%20permit%20%28principal%2C%20action%2C%20resource%29%3B")
        .header("authorization", bearer_for_scope(user_id, "apps:read"))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::BAD_REQUEST);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn oauth_token_subset_of_user_two_call_enforcement() {
    let user_id = Uuid::new_v4();
    let Some(mut fx) = fixture_with_platform("token-subset", user_id).await else {
        return;
    };
    let app_id = create_app(&mut fx, "token-subset").await;
    let app = init_control!(fx);

    let req = test::TestRequest::get()
        .uri("/api/apps")
        .header("authorization", bearer_for_scope(user_id, "apps:read"))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::OK);

    let req = test::TestRequest::put()
        .uri(&format!("/api/apps/{app_id}/archive"))
        .header("authorization", bearer_for_scope(user_id, "apps:read"))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::FORBIDDEN);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Seed the `zeroship.identity_links` marker WITHOUT granting anything
/// through it, then grant exactly `grants`.
///
/// This is the shape an operator leaves behind after narrowing: the principal
/// has been provisioned once (so the marker exists) and some grant rows have
/// since been deleted. Keying off the marker rather than the grant count is
/// what makes "operator revoked everything" distinguishable from "never
/// provisioned" - see `crates/zeroship-control/src/identity_bridge.rs`.
async fn seed_grants(state: &AppState, principal_id: Uuid, grants: &[&str]) {
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.identity_links \
                (principal_id, provider, provider_subject, email) \
             VALUES ($1, 'platform', $2, NULL) \
             ON CONFLICT (provider, provider_subject) DO NOTHING",
            &[&principal_id, &principal_id.to_string()],
        )
        .await
        .expect("seed identity_links marker");
    for grant in grants {
        state
            .control_pg
            .execute(
                "INSERT INTO zeroship.principal_grants (principal_id, grant_name) \
                 VALUES ($1, $2) ON CONFLICT (principal_id, grant_name) DO NOTHING",
                &[&principal_id, grant],
            )
            .await
            .expect("seed principal grant");
    }
}

/// Both tables carry a plain FK to `zeroship.users` with no ON DELETE action
/// (`db/migrations-ts/20260702000600_constraints_indexes_fks.ts:159,195`), so
/// the fixture's `DELETE FROM zeroship.users` is REFUSED while these rows
/// exist - and it is a `let _ =`, so the refusal is silent and the user row
/// simply leaks. Anything that seeds or materializes them must clear them
/// here first.
async fn clear_grants(state: &AppState, principal_id: Uuid) {
    for sql in [
        "DELETE FROM zeroship.principal_grants WHERE principal_id = $1",
        "DELETE FROM zeroship.identity_links WHERE principal_id = $1",
    ] {
        state
            .control_pg
            .execute(sql, &[&principal_id])
            .await
            .expect("clear seeded grant state");
    }
}

async fn stored_grants(state: &AppState, principal_id: Uuid) -> Vec<String> {
    state
        .control_pg
        .query(
            "SELECT grant_name FROM zeroship.principal_grants \
             WHERE principal_id = $1 ORDER BY grant_name",
            &[&principal_id],
        )
        .await
        .expect("read principal grants")
        .iter()
        .map(|row| row.get::<_, String>("grant_name"))
        .collect()
}

async fn seeding_marker_count(state: &AppState, principal_id: Uuid) -> i64 {
    state
        .control_pg
        .query_one(
            "SELECT COUNT(*)::INT8 AS n FROM zeroship.identity_links \
             WHERE principal_id = $1",
            &[&principal_id],
        )
        .await
        .expect("count identity links")
        .get::<_, i64>("n")
}

/// A creator's FIRST CLI request must not be narrowed to nothing, and must
/// leave the default grants behind for the operator to narrow later.
///
/// Login moved off control's `/api/device/token` in `5ae8c7f7d`, which was the
/// only thing that had ever written a platform creator's grant rows. So a
/// platform-native principal reaches control with no `principal_grants` and no
/// `identity_links` marker at all. If control simply intersected the token's
/// scope with that empty set, every first `zeroship deploy` would 403 - which
/// is why the intersection treats an UNSEEDED principal as holding the default
/// CLI set rather than holding nothing.
///
/// Both halves are asserted because either one alone passes for the wrong
/// reason: authorizing without materializing leaves the operator with no rows
/// to delete (so `an_operator_deleting_a_grant_row_narrows_the_next_cli_request`
/// below would have nothing to narrow), and materializing without authorizing
/// is the 403 this exists to prevent.
#[compio::test]
async fn a_first_cli_request_is_authorized_and_materializes_the_default_grants() {
    let user_id = Uuid::new_v4();
    let Some(mut fx) = fixture_with_platform("first-cli", user_id).await else {
        return;
    };
    let app_id = create_app(&mut fx, "first-cli").await;
    let app = init_control!(fx);

    assert_eq!(
        stored_grants(&fx.state, user_id).await,
        Vec::<String>::new(),
        "the measurement needs a principal that starts with no grant rows"
    );
    assert_eq!(
        seeding_marker_count(&fx.state, user_id).await,
        0,
        "the measurement needs a principal that starts with no seeding marker"
    );

    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{app_id}/deploy-check"))
        .header("authorization", bearer_for_scope(user_id, "apps:deploy apps:read"))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(
        status,
        StatusCode::OK,
        "an unseeded principal's first CLI token was narrowed to nothing"
    );

    assert_eq!(
        stored_grants(&fx.state, user_id).await,
        vec![
            "apps:archive",
            "apps:deploy",
            "apps:read",
            "apps:write",
            "secrets:read",
        ],
        "the default CLI grants were not materialized, so an operator has no row to delete"
    );
    assert_eq!(
        seeding_marker_count(&fx.state, user_id).await,
        1,
        "materializing without the marker lets a later request re-seed revoked grants"
    );

    clear_grants(&fx.state, user_id).await;
    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// The capability this whole change exists to restore: an operator DELETE
/// against `zeroship.principal_grants` narrows what a CLI token can do.
///
/// The token carries `apps:deploy apps:read` - the OP issued it against the
/// client registration and knows nothing about this principal's grants. The
/// principal holds only `apps:read`. So the read must succeed and the deploy
/// must not, from one and the same bearer.
///
/// What this does NOT show: that the OP stopped issuing `apps:deploy`. It did
/// not, and that is the design - the registration is a coarse ceiling and the
/// entitlement check is control's, at request time. `crates/auth`'s
/// `the_cli_device_grant_caps_scope_to_the_client_registration_only` pins the
/// other half.
#[compio::test]
async fn an_operator_deleting_a_grant_row_narrows_the_next_cli_request() {
    let user_id = Uuid::new_v4();
    let Some(mut fx) = fixture_with_platform("narrowed-cli", user_id).await else {
        return;
    };
    let app_id = create_app(&mut fx, "narrowed-cli").await;
    seed_grants(&fx.state, user_id, &["apps:read"]).await;
    let app = init_control!(fx);

    let read = test::TestRequest::get()
        .uri(&format!("/api/apps/{app_id}"))
        .header("authorization", bearer_for_scope(user_id, "apps:deploy apps:read"))
        .to_request();
    let read_status = test::call_service(&app, read).await.status();

    let deploy = test::TestRequest::post()
        .uri(&format!("/raw-app/{app_id}/deploy-check"))
        .header("authorization", bearer_for_scope(user_id, "apps:deploy apps:read"))
        .to_request();
    let deploy_status = test::call_service(&app, deploy).await.status();

    assert_eq!(
        read_status,
        StatusCode::OK,
        "narrowing removed a grant the principal still holds"
    );
    assert_eq!(
        deploy_status,
        StatusCode::FORBIDDEN,
        "a token scope the principal no longer holds a grant for was honored anyway"
    );

    // The revoked grant must stay revoked: nothing on the request path may
    // re-seed a principal whose marker already exists.
    assert_eq!(
        stored_grants(&fx.state, user_id).await,
        vec!["apps:read"],
        "a request re-seeded grants an operator had deleted"
    );

    clear_grants(&fx.state, user_id).await;
    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn viewer_role_cannot_use_granted_apps_archive_scope() {
    let user_id = Uuid::new_v4();
    let Some(mut fx) = fixture_with_platform("user-subset", user_id).await else {
        return;
    };
    // The app is owned by a DIFFERENT principal; `user_id` is only a viewer.
    // (create_app now binds the creator as owner, so the principal-under-test
    // must NOT be the creator for this "viewer-only" scenario.)
    let owner_id = Uuid::new_v4();
    let app_id = create_app_owned_by(&mut fx, "user-subset", owner_id).await;
    grant_app_member(&fx.state, app_id, user_id, "viewer").await;
    // The principal is entitled to `apps:archive` and the token carries it, so
    // the 403 below is the ROLE check refusing a viewer. Seed the grant
    // explicitly so the assertion does not depend on just-in-time default CLI
    // grant materialization elsewhere in the request path.
    seed_grants(&fx.state, user_id, &["apps:archive"]).await;
    let app = init_control!(fx);

    let req = test::TestRequest::put()
        .uri(&format!("/api/apps/{app_id}/archive"))
        .header("authorization", bearer_for_scope(user_id, "apps:archive"))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::FORBIDDEN);

    clear_grants(&fx.state, user_id).await;
    fx.cleanup().await;
    let _ = fx
        .state
        .control_pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&owner_id])
        .await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
