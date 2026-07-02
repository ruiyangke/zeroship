//! End-to-end coverage for the platform-mediated device flow.
//!
//! This test intentionally hard-fails without CONTROL_TEST_DB. The device flow
//! is a security-sensitive DB protocol: skipping would hide migration/crypto/
//! one-time-redemption regressions.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use compio_postgres::{connect, NoTls};
use ed25519_dalek::SigningKey;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ntex::http::StatusCode;
use ntex::web::{self, test, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_auth::oidc::{Issuer, PrincipalAccessTokenMint, ACCESS_TOKEN_TTL_SECS};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    authz_guard::AuthzGuard, device_handlers, AppState, EnvStore, Quota,
    RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::auth_provider::{
    AuthProvider, DualIssuerProvider, LegacyAuthProvider, PlatformConfig, PlatformProvider,
    ProviderAuthz, SupabaseConfig, SupabaseProvider,
};
use zeroship_authz::{Action, Resource};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const TEST_CONTROL_KEY: &str = "test-control-key";
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

struct MockPlatformAuth {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockPlatformAuth {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind platform auth mock");
        listener
            .set_nonblocking(true)
            .expect("set platform auth mock nonblocking");
        let base = format!("http://{}", listener.local_addr().expect("platform auth addr"));
        let signing = SigningKey::from_bytes(&[41u8; 32]);
        let issuer = Arc::new(
            Issuer::from_signing_key(&signing, [13u8; 32], base.clone())
                .expect("platform issuer"),
        );
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread_issuer = issuer.clone();
        let thread = thread::spawn(move || {
            loop {
                if shutdown_rx.try_recv().is_ok() {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => handle_platform_auth_request(&mut stream, &thread_issuer),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(err) => panic!("platform auth mock accept: {err}"),
                }
            }
        });
        Self {
            base,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn jwks_url(&self) -> String {
        format!("{}/.well-known/jwks.json", self.base)
    }
}

impl Drop for MockPlatformAuth {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug)]
struct MockHttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct MockMintRequest {
    principal_id: String,
    audience: String,
    client_id: String,
    #[serde(default)]
    scopes: Vec<String>,
    ttl_secs: Option<i64>,
}

fn handle_platform_auth_request(stream: &mut TcpStream, issuer: &Issuer) {
    let request = read_mock_http_request(stream);
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/.well-known/jwks.json") => {
            write_mock_json(stream, 200, &json!({ "keys": [issuer.public_jwk().clone()] }));
        }
        ("POST", "/internal/platform-token") => {
            let authorized = request
                .headers
                .iter()
                .find(|(name, _)| name == "authorization")
                .is_some_and(|(_, value)| value == &format!("Bearer {TEST_CONTROL_KEY}"));
            if !authorized {
                write_mock_json(stream, 401, &json!({"error": "unauthorized"}));
                return;
            }
            let body: MockMintRequest =
                serde_json::from_slice(&request.body).expect("mock mint body json");
            let token = issuer
                .issue_principal_access_token(&PrincipalAccessTokenMint {
                    principal_id: &body.principal_id,
                    audience: &body.audience,
                    client_id: &body.client_id,
                    scopes: &body.scopes,
                    ttl_secs: body.ttl_secs,
                })
                .expect("mock mint token");
            write_mock_json(
                stream,
                200,
                &json!({
                    "access_token": token,
                    "token_type": "Bearer",
                    "expires_in": body.ttl_secs.unwrap_or(ACCESS_TOKEN_TTL_SECS),
                    "scope": body.scopes.join(" "),
                    "provider": "platform",
                }),
            );
        }
        _ => write_mock_json(stream, 404, &json!({"error": "not_found"})),
    }
}

fn read_mock_http_request(stream: &mut TcpStream) -> MockHttpRequest {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("set platform auth read timeout");
    let mut bytes = Vec::new();
    let mut buf = [0_u8; 1024];
    loop {
        let n = stream.read(&mut buf).expect("read platform auth request");
        assert_ne!(n, 0, "platform auth client closed before headers");
        bytes.extend_from_slice(&buf[..n]);
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("platform auth header terminator")
        + 4;
    let header_text = String::from_utf8_lossy(&bytes[..header_end]);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().expect("platform auth request line");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().expect("platform auth method").to_string();
    let path = parts.next().expect("platform auth path").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let n = stream.read(&mut buf).expect("read platform auth body");
        assert_ne!(n, 0, "platform auth client closed before body");
        bytes.extend_from_slice(&buf[..n]);
    }
    MockHttpRequest {
        method,
        path,
        headers,
        body: bytes[header_end..header_end + content_length].to_vec(),
    }
}

fn write_mock_json(stream: &mut TcpStream, status: u16, value: &Value) {
    let body = serde_json::to_string(value).expect("mock json body");
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Test",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write platform auth response");
}

async fn admin_user() -> HttpResponse {
    HttpResponse::Ok().json(&json!({
        "email_confirmed_at": "2026-06-30T00:00:00Z"
    }))
}

struct Fixture {
    state: Arc<AppState>,
    _mock_supabase: MockSupabase,
    _mock_platform: MockPlatformAuth,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    users: Vec<Uuid>,
    app_ids: Vec<Uuid>,
    subjects: Vec<String>,
    device_hashes: Vec<String>,
}

impl Fixture {
    async fn new() -> Self {
        let db_url = db_url();
        let mock_supabase = MockSupabase::start();
        let mock_platform = MockPlatformAuth::start();
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
        let legacy = LegacyAuthProvider::Supabase(SupabaseProvider::new(
            SupabaseConfig::new(
                mock_supabase.base.clone(),
                SUPABASE_ANON_KEY,
                Some(SUPABASE_SERVICE_ROLE_KEY.to_string()),
                Some(SUPABASE_JWT_SECRET.to_string()),
                None,
                issuer,
            )
            .expect("valid test Supabase config"),
        ));
        let platform = PlatformProvider::new(
            PlatformConfig::new(mock_platform.base.clone(), Some(mock_platform.jwks_url()))
                .expect("valid platform config"),
        );
        let auth_provider = Arc::new(AuthProvider::DualIssuer(DualIssuerProvider::new(
            platform, legacy,
        )));

        let state = Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            blob_store,
            control_key: SecretString::new(TEST_CONTROL_KEY.to_string()),
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
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(zeroship_authn::PatIssuer::dev_insecure()),
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
            _mock_platform: mock_platform,
            blob_root,
            deploy_tmp_dir,
            users: Vec::new(),
            app_ids: Vec::new(),
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

    async fn create_app_for_principal(&mut self, principal_id: Uuid) -> Uuid {
        let name = format!("device-flow-{}", Uuid::new_v4().simple());
        let app = self
            .state
            .registry
            .create_app(
                &name,
                &zeroship_control::bootstrap_console::free_plan_id(),
                &principal_id,
            )
            .await
            .expect("create app owned by device principal");
        self.app_ids.push(app.id);
        app.id
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
        for app_id in &self.app_ids {
            let _ = self
                .state
                .control_pg
                .execute("DELETE FROM zeroship.app_members WHERE app_id = $1", &[app_id])
                .await;
            let _ = self
                .state
                .control_pg
                .execute("DELETE FROM zeroship.apps WHERE id = $1", &[app_id])
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

async fn deploy_check(
    path: web::types::Path<String>,
    state: web::types::State<Arc<AppState>>,
    guard: AuthzGuard,
) -> HttpResponse {
    match guard
        .require(
            Action::AppsDeploy,
            Resource::App {
                id: path.into_inner(),
            },
            &state,
        )
        .await
    {
        Ok(()) => HttpResponse::Ok().json(&json!({"authorized": true})),
        Err(resp) => resp,
    }
}

#[compio::test]
async fn platform_device_flow_enforces_hashing_auth_encryption_interval_expiry_and_one_time_use() {
    let mut fx = Fixture::new().await;
    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(device_handlers::configure)
            .service(web::resource("/apps/{id}/deploy-check").route(web::post().to(deploy_check))),
    )
    .await;

    let auth_req = test::TestRequest::post()
        .uri("/api/device/auth")
        .set_json(&json!({
            "client_id": "zeroship-cli",
            "scope": "openid offline_access apps:deploy apps:read apps:write"
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
                    platform_access_token_enc IS NULL AS token_missing \
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
    assert_eq!(row.get::<_, String>("provider"), "platform");
    assert_eq!(
        row.get::<_, Option<String>>("scope").as_deref(),
        Some("openid offline_access apps:deploy apps:read apps:write")
    );
    assert!(row.get::<_, bool>("token_missing"));

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
            "user_code": user_code
        }))
        .to_request();
    let no_bearer_resp = test::call_service(&app, no_bearer_req).await;
    assert_eq!(no_bearer_resp.status(), StatusCode::UNAUTHORIZED);

    let invalid_bearer_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", "Bearer not-a-jwt")
        .set_json(&json!({
            "user_code": user_code
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
            "user_code": "ZZZZ-ZZZZ"
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
            "user_code": user_code
        }))
        .to_request();
    let approve_resp = test::call_service(&app, approve_req).await;
    assert_eq!(approve_resp.status(), StatusCode::NO_CONTENT);
    let principal_id = fx.track_principal_for_subject(&subject).await;

    let row = fx
        .state
        .control_pg
        .query_one(
            "SELECT status, principal_id, platform_access_token_enc \
             FROM zeroship.device_grants \
             WHERE device_code_hash = $1",
            &[&device_code_hash],
        )
        .await
        .expect("approved row");
    assert_eq!(row.get::<_, String>("status"), "approved");
    assert_eq!(row.get::<_, Uuid>("principal_id"), principal_id);
    let ciphertext: Vec<u8> = row.get("platform_access_token_enc");
    let key = zeroship_core::crypto::derive_key(TEST_MASTER_KEY);
    let decrypted = zeroship_core::crypto::decrypt(
        &key,
        &device_handlers::device_access_token_aad(&device_code_hash),
        &ciphertext,
    )
    .expect("decrypt stored platform access token");
    let stored_access_token =
        String::from_utf8(decrypted).expect("stored platform access token utf8");
    assert_eq!(
        stored_access_token.split('.').count(),
        3,
        "stored token must be a compact JWS"
    );
    assert!(
        !String::from_utf8_lossy(&ciphertext).contains(&stored_access_token),
        "platform access token must not be stored in plaintext"
    );

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
    assert_eq!(approved_body["provider"], "platform");
    assert_eq!(approved_body["token_type"], "Bearer");
    assert_eq!(approved_body["principal_id"], principal_id.to_string());
    assert_eq!(
        approved_body["scope"],
        "apps:deploy apps:read apps:write"
    );
    assert!(approved_body["expires_in"].as_u64().expect("expires_in") > 0);
    let deploy_token = approved_body["access_token"]
        .as_str()
        .expect("access_token")
        .to_string();
    assert_eq!(
        deploy_token, stored_access_token,
        "poll must return the exact OP-issued token bound at approval"
    );
    let verified = fx
        .state
        .auth_provider
        .verify_token(&deploy_token)
        .await
        .expect("platform token verifies through DualIssuer/PlatformProvider");
    assert_eq!(verified.provider_subject, principal_id.to_string());
    assert_eq!(
        verified.provider_authz,
        ProviderAuthz::OAuthScope("apps:deploy apps:read apps:write".to_string())
    );
    assert_eq!(
        verified.aud.as_deref(),
        Some(&["control.zeroship.ai".to_string()][..])
    );
    let app_id = fx.create_app_for_principal(principal_id).await;
    let deploy_check_req = test::TestRequest::post()
        .uri(&format!("/apps/{app_id}/deploy-check"))
        .header("authorization", bearer(&deploy_token))
        .to_request();
    let deploy_check_resp = test::call_service(&app, deploy_check_req).await;
    assert_eq!(
        deploy_check_resp.status(),
        StatusCode::OK,
        "minted token should authorize apps:deploy through oauth_guard_from_bearer"
    );

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

    let limited_subject = Uuid::new_v4().to_string();
    let limited_email = format!(
        "device-limited-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let limited_user_id = Uuid::new_v4();
    fx.users.push(limited_user_id);
    fx.subjects.push(limited_subject.clone());
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, email_verified_at, name) \
             VALUES ($1, $2::citext, NOW(), 'Limited Device User')",
            &[&limited_user_id, &limited_email],
        )
        .await
        .expect("insert limited user");
    for grant in ["apps:deploy", "apps:read"] {
        fx.state
            .control_pg
            .execute(
                "INSERT INTO zeroship.principal_grants (principal_id, grant_name) \
                 VALUES ($1, $2)",
                &[&limited_user_id, &grant],
            )
            .await
            .expect("insert limited grant");
    }

    let limited_auth_req = test::TestRequest::post()
        .uri("/api/device/auth")
        .set_json(&json!({
            "client_id": "zeroship-cli",
            "scope": "openid offline_access apps:deploy apps:read apps:write"
        }))
        .to_request();
    let limited_auth_resp = test::call_service(&app, limited_auth_req).await;
    assert_eq!(limited_auth_resp.status(), StatusCode::OK);
    let limited_auth_body: Value =
        serde_json::from_slice(&test::read_body(limited_auth_resp).await)
            .expect("limited auth body json");
    let limited_device_code = limited_auth_body["device_code"]
        .as_str()
        .expect("limited device_code");
    let limited_user_code = limited_auth_body["user_code"]
        .as_str()
        .expect("limited user_code");
    fx.track_hash(limited_device_code);
    let limited_bearer =
        gotrue_token(fx.issuer(), &limited_subject, &limited_email, "authenticated");
    let limited_approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&limited_bearer))
        .set_json(&json!({
            "user_code": limited_user_code
        }))
        .to_request();
    let limited_approve_resp = test::call_service(&app, limited_approve_req).await;
    assert_eq!(limited_approve_resp.status(), StatusCode::NO_CONTENT);
    let limited_token_req = test::TestRequest::post()
        .uri("/api/device/token")
        .set_json(&json!({
            "device_code": limited_device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        }))
        .to_request();
    let limited_token_resp = test::call_service(&app, limited_token_req).await;
    assert_eq!(limited_token_resp.status(), StatusCode::OK);
    let limited_token_body: Value =
        serde_json::from_slice(&test::read_body(limited_token_resp).await)
            .expect("limited token body json");
    assert_eq!(limited_token_body["principal_id"], limited_user_id.to_string());
    assert_eq!(limited_token_body["scope"], "apps:deploy apps:read");
    let limited_token = limited_token_body["access_token"]
        .as_str()
        .expect("limited access_token");
    let limited_verified = fx
        .state
        .auth_provider
        .verify_token(limited_token)
        .await
        .expect("limited platform token verifies");
    assert_eq!(
        limited_verified.provider_authz,
        ProviderAuthz::OAuthScope("apps:deploy apps:read".to_string())
    );

    fx.cleanup().await;
}
