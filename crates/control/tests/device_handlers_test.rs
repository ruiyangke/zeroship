//! End-to-end coverage for the platform-mediated device flow.
//!
//! This test intentionally hard-fails without CONTROL_TEST_DB. The device flow
//! is a security-sensitive DB protocol: skipping would hide migration/crypto/
//! one-time-redemption regressions.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use compio_postgres::{connect, NoTls};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ntex::http::StatusCode;
use ntex::web::{self, test, HttpResponse};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    device_handlers, token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
};
use zeroship_core::auth_provider::{
    AuthProvider, SupabaseConfig, SupabaseProvider,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const SUPABASE_ANON_KEY: &str = "test-anon-key";
const SUPABASE_SERVICE_ROLE_KEY: &str = "test-service-role-key";
const SUPABASE_JWT_SECRET: &str = "test-supabase-jwt-secret-at-least-32-bytes";

fn db_url() -> String {
    std::env::var("CONTROL_TEST_DB")
        .expect("CONTROL_TEST_DB must be set so device_handlers_test runs against Postgres")
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-device-handlers-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after Unix epoch")
        .as_secs()
}

fn gotrue_token(issuer: &str, subject: &str, email: &str, role: &str) -> String {
    let claims = json!({
        "iss": issuer,
        "sub": subject,
        "aud": "authenticated",
        "exp": unix_now_secs() + 3600,
        "iat": unix_now_secs(),
        "nbf": unix_now_secs().saturating_sub(1),
        "email": email,
        "session_id": Uuid::new_v4().to_string(),
        "role": role,
    });
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(SUPABASE_JWT_SECRET.as_bytes()),
    )
    .expect("HS256 GoTrue token")
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

struct MockSupabase {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockSupabase {
    fn start() -> Self {
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("control-device-supabase-mock")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let server = web::test::server(move || async move {
                        web::App::new().service(
                            web::resource("/auth/v1/admin/users/{id}")
                                .route(web::get().to(admin_user)),
                        )
                    })
                    .await;
                    let addr = server.addr();
                    started_tx.send(addr).expect("send mock server addr");
                    let _ = shutdown_rx.recv();
                    drop(server);
                });
        });
        let addr = started_rx.recv().expect("mock server starts");
        Self {
            base: format!("http://{addr}"),
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn issuer(&self) -> String {
        format!("{}/auth/v1", self.base)
    }
}

impl Drop for MockSupabase {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn admin_user() -> HttpResponse {
    HttpResponse::Ok().json(&json!({
        "email_confirmed_at": "2026-06-30T00:00:00Z"
    }))
}

struct Fixture {
    state: Arc<AppState>,
    _mock_supabase: MockSupabase,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    users: Vec<Uuid>,
    subjects: Vec<String>,
    device_hashes: Vec<String>,
}

impl Fixture {
    async fn new() -> Self {
        let db_url = db_url();
        let mock_supabase = MockSupabase::start();
        let issuer = mock_supabase.issuer();
        let (control_pg_client, control_pg_conn) =
            connect(&db_url, NoTls).await.expect("control-pg connect");
        compio::runtime::spawn(async move {
            if let Err(err) = control_pg_conn.run().await {
                eprintln!("[device_handlers_test] pg connection error: {err}");
            }
        })
        .detach();

        let registry = Registry::new(&db_url).await.expect("registry");
        zeroship_control::bootstrap_console::seed_plans(&registry)
            .await
            .expect("seed built-in plans");
        let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false)
            .expect("env store");
        let stripe_store = StripeStore::new(registry.clone());
        let blob_root = tmpdir("blob");
        let deploy_tmp_dir = tmpdir("deploy");
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
        let auth_provider = Arc::new(AuthProvider::Supabase(SupabaseProvider::new(
            SupabaseConfig::new(
                mock_supabase.base.clone(),
                SUPABASE_ANON_KEY,
                Some(SUPABASE_SERVICE_ROLE_KEY.to_string()),
                Some(SUPABASE_JWT_SECRET.to_string()),
                None,
                issuer,
            )
            .expect("valid test Supabase config"),
        )));

        let state = Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            blob_store,
            control_key: SecretString::new("test-control-key".to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "https://api.stripe.com".to_string(),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            insecure_dev: true,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg: Arc::new(control_pg_client),
            hydra_admin_url: String::new(),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(token_handlers::PatIssuer::dev_insecure()),
            auth_provider,
            logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
            metering_provider: zeroship_control::metering::provider::build_provider(
                &zeroship_control::metering::provider::MeteringProviderConfig::native(),
            )
            .expect("native provider builds"),
            tax_provider: zeroship_control::tax::build_tax_provider(
                &zeroship_control::tax::TaxProviderConfig::native(),
            )
            .expect("native tax provider builds"),
            notifier: Arc::new(zeroship_control::notify::RecordingNotifier::new()),
            pairwise_salt: [0u8; 32],
            projected_charge_cache: Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        });

        Self {
            state,
            _mock_supabase: mock_supabase,
            blob_root,
            deploy_tmp_dir,
            users: Vec::new(),
            subjects: Vec::new(),
            device_hashes: Vec::new(),
        }
    }

    fn issuer(&self) -> &str {
        self.state.auth_provider.issuer()
    }

    fn track_hash(&mut self, device_code: &str) -> String {
        let hash = sha256_hex(device_code);
        if !self.device_hashes.contains(&hash) {
            self.device_hashes.push(hash.clone());
        }
        hash
    }

    async fn track_principal_for_subject(&mut self, subject: &str) -> Uuid {
        if !self.subjects.iter().any(|seen| seen == subject) {
            self.subjects.push(subject.to_string());
        }
        let row = self
            .state
            .control_pg
            .query_one(
                "SELECT principal_id \
                 FROM zeroship.identity_links \
                 WHERE provider = 'supabase' AND provider_subject = $1",
                &[&subject],
            )
            .await
            .expect("identity link exists");
        let principal_id: Uuid = row.get("principal_id");
        if !self.users.contains(&principal_id) {
            self.users.push(principal_id);
        }
        principal_id
    }

    async fn cleanup(&self) {
        for hash in &self.device_hashes {
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.device_grants WHERE device_code_hash = $1",
                    &[hash],
                )
                .await;
        }
        for subject in &self.subjects {
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.identity_links \
                     WHERE provider = 'supabase' AND provider_subject = $1",
                    &[subject],
                )
                .await;
        }
        for user_id in &self.users {
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.principal_grants WHERE principal_id = $1",
                    &[user_id],
                )
                .await;
            let _ = self
                .state
                .control_pg
                .execute("DELETE FROM zeroship.users WHERE id = $1", &[user_id])
                .await;
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

fn sha256_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn token_error(body: Value) -> String {
    body["error"].as_str().expect("error string").to_string()
}

#[compio::test]
async fn platform_device_flow_enforces_hashing_auth_encryption_interval_expiry_and_one_time_use() {
    let mut fx = Fixture::new().await;
    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(device_handlers::configure),
    )
    .await;

    let auth_req = test::TestRequest::post()
        .uri("/api/device/auth")
        .set_json(&json!({
            "client_id": "zeroship-cli",
            "scope": "openid offline_access apps:deploy apps:read"
        }))
        .to_request();
    let auth_resp = test::call_service(&app, auth_req).await;
    assert_eq!(auth_resp.status(), StatusCode::OK);
    let auth_body: Value =
        serde_json::from_slice(&test::read_body(auth_resp).await).expect("auth body json");
    let device_code = auth_body["device_code"].as_str().expect("device_code");
    let user_code = auth_body["user_code"].as_str().expect("user_code");
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(device_code)
        .expect("device_code is base64url");
    assert_eq!(decoded.len(), 32, "device_code must carry 256 bits");
    assert_eq!(auth_body["interval"], 5);
    assert_eq!(auth_body["expires_in"], 600);
    assert!(auth_body["verification_uri"]
        .as_str()
        .expect("verification_uri")
        .ends_with("/device"));
    assert!(auth_body["verification_uri_complete"]
        .as_str()
        .expect("verification_uri_complete")
        .contains(user_code));

    let device_code_hash = fx.track_hash(device_code);
    let row = fx
        .state
        .control_pg
        .query_one(
            "SELECT device_code_hash, user_code, status, provider, scope, \
                    gotrue_refresh_token_enc IS NULL AS refresh_missing \
             FROM zeroship.device_grants \
             WHERE user_code = $1",
            &[&user_code],
        )
        .await
        .expect("device grant row");
    assert_eq!(row.get::<_, String>("device_code_hash"), device_code_hash);
    assert_ne!(row.get::<_, String>("device_code_hash"), device_code);
    assert_eq!(row.get::<_, String>("user_code"), user_code);
    assert_eq!(row.get::<_, String>("status"), "pending");
    assert_eq!(row.get::<_, String>("provider"), "supabase");
    assert_eq!(
        row.get::<_, Option<String>>("scope").as_deref(),
        Some("openid offline_access apps:deploy apps:read")
    );
    assert!(row.get::<_, bool>("refresh_missing"));

    let pending_req = test::TestRequest::post()
        .uri("/api/device/token")
        .set_json(&json!({
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        }))
        .to_request();
    let pending_resp = test::call_service(&app, pending_req).await;
    assert_eq!(pending_resp.status(), StatusCode::BAD_REQUEST);
    let pending_body: Value =
        serde_json::from_slice(&test::read_body(pending_resp).await).expect("pending body json");
    assert_eq!(token_error(pending_body), "authorization_pending");

    let slow_req = test::TestRequest::post()
        .uri("/api/device/token")
        .set_json(&json!({
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        }))
        .to_request();
    let slow_resp = test::call_service(&app, slow_req).await;
    assert_eq!(slow_resp.status(), StatusCode::BAD_REQUEST);
    let slow_body: Value =
        serde_json::from_slice(&test::read_body(slow_resp).await).expect("slow body json");
    assert_eq!(token_error(slow_body), "slow_down");

    let no_bearer_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .set_json(&json!({
            "user_code": user_code,
            "refresh_token": "gotrue-refresh-secret"
        }))
        .to_request();
    let no_bearer_resp = test::call_service(&app, no_bearer_req).await;
    assert_eq!(no_bearer_resp.status(), StatusCode::UNAUTHORIZED);

    let invalid_bearer_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", "Bearer not-a-jwt")
        .set_json(&json!({
            "user_code": user_code,
            "refresh_token": "gotrue-refresh-secret"
        }))
        .to_request();
    let invalid_bearer_resp = test::call_service(&app, invalid_bearer_req).await;
    assert_eq!(invalid_bearer_resp.status(), StatusCode::UNAUTHORIZED);

    let subject = Uuid::new_v4().to_string();
    let email = format!("device-flow-{}@zeroship.test", Uuid::new_v4().simple());
    let bearer_token = gotrue_token(fx.issuer(), &subject, &email, "authenticated");

    let unknown_approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&bearer_token))
        .set_json(&json!({
            "user_code": "ZZZZ-ZZZZ",
            "refresh_token": "gotrue-refresh-secret"
        }))
        .to_request();
    let unknown_approve_resp = test::call_service(&app, unknown_approve_req).await;
    assert_eq!(unknown_approve_resp.status(), StatusCode::BAD_REQUEST);
    let unknown_body: Value =
        serde_json::from_slice(&test::read_body(unknown_approve_resp).await)
            .expect("unknown approve body json");
    assert_eq!(unknown_body["error"], "invalid_user_code");

    let approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&bearer_token))
        .set_json(&json!({
            "user_code": user_code,
            "refresh_token": "gotrue-refresh-secret"
        }))
        .to_request();
    let approve_resp = test::call_service(&app, approve_req).await;
    assert_eq!(approve_resp.status(), StatusCode::NO_CONTENT);
    let principal_id = fx.track_principal_for_subject(&subject).await;

    let row = fx
        .state
        .control_pg
        .query_one(
            "SELECT status, principal_id, gotrue_refresh_token_enc \
             FROM zeroship.device_grants \
             WHERE device_code_hash = $1",
            &[&device_code_hash],
        )
        .await
        .expect("approved row");
    assert_eq!(row.get::<_, String>("status"), "approved");
    assert_eq!(row.get::<_, Uuid>("principal_id"), principal_id);
    let ciphertext: Vec<u8> = row.get("gotrue_refresh_token_enc");
    assert!(
        !String::from_utf8_lossy(&ciphertext).contains("gotrue-refresh-secret"),
        "refresh token must not be stored in plaintext"
    );
    let key = zeroship_core::crypto::derive_key(TEST_MASTER_KEY);
    let decrypted = zeroship_core::crypto::decrypt(
        &key,
        &device_handlers::device_refresh_aad(&device_code_hash),
        &ciphertext,
    )
    .expect("decrypt stored refresh token");
    assert_eq!(decrypted, b"gotrue-refresh-secret");

    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.device_grants \
             SET last_polled_at = NOW() - INTERVAL '6 seconds' \
             WHERE device_code_hash = $1",
            &[&device_code_hash],
        )
        .await
        .expect("age last poll");

    let approved_token_req = test::TestRequest::post()
        .uri("/api/device/token")
        .set_json(&json!({
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        }))
        .to_request();
    let approved_token_resp = test::call_service(&app, approved_token_req).await;
    assert_eq!(approved_token_resp.status(), StatusCode::OK);
    let approved_body: Value =
        serde_json::from_slice(&test::read_body(approved_token_resp).await)
            .expect("approved token body json");
    assert_eq!(approved_body["refresh_token"], "gotrue-refresh-secret");
    assert_eq!(approved_body["provider"], "supabase");
    assert_eq!(approved_body["token_type"], "Bearer");
    assert_eq!(approved_body["anon_key"], SUPABASE_ANON_KEY);
    assert!(approved_body["token_endpoint"]
        .as_str()
        .expect("token_endpoint")
        .ends_with("/auth/v1/token?grant_type=refresh_token"));

    let remaining = fx
        .state
        .control_pg
        .query_one(
            "SELECT COUNT(*)::INT8 AS n \
             FROM zeroship.device_grants \
             WHERE device_code_hash = $1",
            &[&device_code_hash],
        )
        .await
        .expect("count after redemption")
        .get::<_, i64>("n");
    assert_eq!(remaining, 0, "approved grant must be one-time use");

    let second_req = test::TestRequest::post()
        .uri("/api/device/token")
        .set_json(&json!({
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        }))
        .to_request();
    let second_resp = test::call_service(&app, second_req).await;
    assert_eq!(second_resp.status(), StatusCode::BAD_REQUEST);
    let second_body: Value =
        serde_json::from_slice(&test::read_body(second_resp).await).expect("second body json");
    assert_eq!(token_error(second_body), "expired_token");

    let expired_auth_req = test::TestRequest::post()
        .uri("/api/device/auth")
        .set_json(&json!({
            "client_id": "zeroship-cli",
            "scope": "device-expired-test"
        }))
        .to_request();
    let expired_auth_resp = test::call_service(&app, expired_auth_req).await;
    assert_eq!(expired_auth_resp.status(), StatusCode::OK);
    let expired_auth_body: Value =
        serde_json::from_slice(&test::read_body(expired_auth_resp).await)
            .expect("expired auth body json");
    let expired_device_code = expired_auth_body["device_code"]
        .as_str()
        .expect("expired device_code");
    let expired_hash = fx.track_hash(expired_device_code);
    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.device_grants \
             SET expires_at = NOW() - INTERVAL '1 second' \
             WHERE device_code_hash = $1",
            &[&expired_hash],
        )
        .await
        .expect("expire grant");

    let expired_token_req = test::TestRequest::post()
        .uri("/api/device/token")
        .set_json(&json!({
            "device_code": expired_device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        }))
        .to_request();
    let expired_token_resp = test::call_service(&app, expired_token_req).await;
    assert_eq!(expired_token_resp.status(), StatusCode::BAD_REQUEST);
    let expired_body: Value =
        serde_json::from_slice(&test::read_body(expired_token_resp).await)
            .expect("expired body json");
    assert_eq!(token_error(expired_body), "expired_token");

    let denied_auth_req = test::TestRequest::post()
        .uri("/api/device/auth")
        .set_json(&json!({
            "client_id": "zeroship-cli",
            "scope": "device-denied-test"
        }))
        .to_request();
    let denied_auth_resp = test::call_service(&app, denied_auth_req).await;
    assert_eq!(denied_auth_resp.status(), StatusCode::OK);
    let denied_auth_body: Value =
        serde_json::from_slice(&test::read_body(denied_auth_resp).await)
            .expect("denied auth body json");
    let denied_device_code = denied_auth_body["device_code"]
        .as_str()
        .expect("denied device_code");
    let denied_hash = fx.track_hash(denied_device_code);
    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.device_grants \
             SET status = 'denied' \
             WHERE device_code_hash = $1",
            &[&denied_hash],
        )
        .await
        .expect("deny grant");

    let denied_token_req = test::TestRequest::post()
        .uri("/api/device/token")
        .set_json(&json!({
            "device_code": denied_device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        }))
        .to_request();
    let denied_token_resp = test::call_service(&app, denied_token_req).await;
    assert_eq!(denied_token_resp.status(), StatusCode::BAD_REQUEST);
    let denied_body: Value =
        serde_json::from_slice(&test::read_body(denied_token_resp).await)
            .expect("denied body json");
    assert_eq!(token_error(denied_body), "access_denied");

    fx.cleanup().await;
}
