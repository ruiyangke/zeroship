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
    AuthProvider, ConfiguredProvider, PlatformConfig, PlatformProvider,
    ProviderAuthz, SupabaseConfig, SupabaseProvider,
};
use zeroship_core::config::OriginScheme;
use zeroship_core::device_grant;
use zeroship_authz::{Action, Resource};

mod common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const TEST_CONTROL_KEY: &str = "test-control-key";
const SUPABASE_ANON_KEY: &str = "test-anon-key";
const SUPABASE_SERVICE_ROLE_KEY: &str = "test-service-role-key";
const SUPABASE_JWT_SECRET: &str = "test-supabase-jwt-secret-at-least-32-bytes";

fn db_url() -> String {
    zeroship_core::test_env!("CONTROL_TEST_DB")
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

/// A well-formed random user code.
///
/// Production now validates every incoming `user_code` against
/// `device_grant::valid_user_code` before it ever reaches the database (see
/// `device_approve`), so the old hand-built fixture codes like
/// `format!("PLAT-{}", ...)` (two groups, and characters like `1`/`A` outside
/// `USER_CODE_ALPHABET`) are rejected on sight. This reuses the exact
/// generator the real `/api/device/auth` handler calls, so a code minted here
/// is guaranteed to pass the validator; it does NOT guarantee the code is
/// absent from the table (a caller who wants an "unknown code" negative case
/// must not insert this code anywhere).
fn random_user_code() -> String {
    device_grant::generate_user_code(&mut rand::thread_rng())
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
    issuer: Arc<Issuer>,
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
        // The configured `platform_issuer` must end in `device_grant::OP_PATH_PREFIX`
        // ("/oauth2"), because `mint_platform_deploy_token` and `verification_uri`
        // both derive their target from `device_grant::op_public_url(platform_issuer)`,
        // which returns `None` for an issuer with no such suffix. The real auth
        // service issuer already carries this suffix (its doc comment: "The auth
        // service builds its issuer as `{public_url}{OP_PATH_PREFIX}`"); this mock
        // must match that shape or `/api/device/auth` fails closed with a 500
        // before this suite ever reaches the parts it means to test. The mint
        // itself is still served at `{base}/internal/platform-token` (root, no
        // `/oauth2`), matching how `handle_platform_auth_request` below routes it -
        // `op_public_url` strips the suffix back off before the mint call.
        let issuer_url = format!("{base}{}", device_grant::OP_PATH_PREFIX);
        let issuer = Arc::new(
            Issuer::from_signing_key(&signing, [13u8; 32], issuer_url)
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
            issuer,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn jwks_url(&self) -> String {
        format!("{}/.well-known/jwks.json", self.base)
    }

    /// The configured `platform_issuer` string, `{base}/oauth2`. What
    /// `device_grant::op_public_url` strips back down to `base`.
    fn issuer(&self) -> &str {
        self.issuer.issuer()
    }

    fn issue_access_token(&self, principal_id: Uuid, scopes: &[String]) -> String {
        self.issue_access_token_for_audience(principal_id, scopes, "control.zeroship.ai")
    }

    /// Mint an otherwise valid platform access token for an arbitrary audience.
    ///
    /// Everything else is identical to [`Self::issue_access_token`]: same
    /// issuer, same signing key, same subject, same client. The audience is the
    /// single variable, which is what makes the refusal attributable to the
    /// audience check rather than to the token being malformed.
    fn issue_access_token_for_audience(
        &self,
        principal_id: Uuid,
        scopes: &[String],
        audience: &str,
    ) -> String {
        let principal_id = principal_id.to_string();
        self.issuer
            .issue_principal_access_token(&PrincipalAccessTokenMint {
                principal_id: &principal_id,
                audience,
                client_id: "zeroship-console",
                scopes,
                ttl_secs: None,
            })
            .expect("platform approval token")
    }

    /// The `client_id` this mock stamps into every token it issues. The
    /// revocation marker is keyed on it.
    fn token_client_id(&self) -> &'static str {
        "zeroship-console"
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

#[derive(Clone, Copy)]
enum FixtureProvider {
    Platform,
    DualIssuer,
    /// Supabase with no platform OP configured. The device flow cannot run
    /// here: there is nothing to mint a platform deploy token through.
    SupabaseOnly,
}

impl Fixture {
    async fn new() -> Self {
        Self::new_with_provider(FixtureProvider::DualIssuer).await
    }

    async fn new_with_provider(provider: FixtureProvider) -> Self {
        Self::build(provider, Quota::per_minute(10_000, 100), false).await
    }

    /// Builds a fixture whose admin limiter can actually be reached.
    ///
    /// Both knobs move together, and that is the point. The default quota is
    /// deliberately too high to cross, so a test that wants to see a 429 must
    /// lower it - but the limiter bucket lives in Postgres and is keyed by the
    /// resolved caller identity, so every test in this binary shares one bucket
    /// unless the caller can be told apart. `X-Forwarded-For` is what tells
    /// them apart, and it is ignored unless `trust_proxy` is on. Lowering the
    /// quota without it drains the shared bucket and every later test in the
    /// file starts getting 429 where it asserted 401.
    async fn new_probing_the_admin_limiter(
        provider: FixtureProvider,
        admin_quota: Quota,
    ) -> Self {
        Self::build(provider, admin_quota, true).await
    }

    async fn build(provider: FixtureProvider, admin_quota: Quota, trust_proxy: bool) -> Self {
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
        zeroship_control::plan_catalog::seed_plans(&registry)
            .await
            .expect("seed built-in plans");
        let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY)
            .expect("env store");
        let stripe_store = StripeStore::new(registry.clone());
        let blob_root = tmpdir("blob");
        let deploy_tmp_dir = tmpdir("deploy");
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
        let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
            zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                .expect("workflow blob store"),
        );
        let supabase = ConfiguredProvider::Supabase(SupabaseProvider::new(
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
            PlatformConfig::new(mock_platform.issuer().to_string(), Some(mock_platform.jwks_url()))
                .expect("valid platform config"),
        );
        let auth_provider = Arc::new(match provider {
            FixtureProvider::Platform => AuthProvider::platform(platform),
            FixtureProvider::DualIssuer => AuthProvider::new(vec![
                ConfiguredProvider::Platform(platform),
                supabase,
            ])
            .expect("distinct fixture issuers"),
        });

        let state = Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            blob_store,
            workflow_blob_store,
            control_key: SecretString::new(TEST_CONTROL_KEY.to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "https://api.stripe.com".to_string(),
            gateway_url: "http://127.0.0.1:9".to_string(),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(admin_quota)),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            // Public URLs follow deployment topology.
            origin_scheme: OriginScheme::Https,
            trust_proxy,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg: Arc::new(control_pg_client),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(zeroship_authn::PatIssuer::generate_ephemeral()),
            auth_provider,
        provider_registry: zeroship_control::metering::provider::builtin_registry(),
        billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
        billing_stream: None,
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

    /// The GoTrue issuer these fixtures mint test tokens with.
    ///
    /// Was `auth_provider.issuer()`, which returned "the legacy issuer" for the
    /// dual arm and "the only issuer" otherwise - a rule that had no meaning
    /// once the verifier became a set. Asking for the Supabase issuer by name
    /// says what the caller actually wants.
    fn issuer(&self) -> &str {
        self.state
            .auth_provider
            .supabase_issuer()
            .expect("fixture trusts Supabase")
    }

    fn track_hash(&mut self, device_code: &str) -> String {
        let hash = sha256_hex(device_code);
        if !self.device_hashes.contains(&hash) {
            self.device_hashes.push(hash.clone());
        }
        hash
    }

    /// Insert a pending `provider = 'platform'` grant directly.
    ///
    /// For tests whose subject is the APPROVAL rule, so the row is a fixture
    /// rather than something `/api/device/auth` has to be driven to produce.
    async fn insert_pending_platform_grant(&mut self, device_code_hash: &str, user_code: &str) {
        if !self.device_hashes.iter().any(|seen| seen == device_code_hash) {
            self.device_hashes.push(device_code_hash.to_string());
        }
        self.state
            .control_pg
            .execute(
                "INSERT INTO zeroship.device_grants \
                    (device_code_hash, user_code, provider, scope, expires_at) \
                 VALUES ($1, $2, 'platform', 'apps:deploy apps:read apps:write', \
                         NOW() + INTERVAL '10 minutes')",
                &[&device_code_hash, &user_code],
            )
            .await
            .expect("insert pending platform device grant");
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
                &zeroship_control::plan_catalog::free_plan_id(),
                &principal_id,
            )
            .await
            .expect("create app owned by device principal");
        self.app_ids.push(app.id);
        app.id
    }

    /// Insert a bare `zeroship.users` row for a platform principal, with NO
    /// grants and no `identity_links` marker.
    ///
    /// Used to pre-insert principal_grants rows here: a platform principal
    /// genuinely has zero grants until `identity_bridge::ensure_platform_creator_grants`
    /// JIT-provisions them on the first approved poll, and a fixture that
    /// pre-granted all three masked exactly that: `deploy_scopes_for_principal`
    /// was never exercised against an empty grant set, so a deploy token
    /// minted with `scope: ""` for a real first-time creator went unnoticed.
    /// Tests that need pre-existing grants call [`Self::grant_scopes`]
    /// explicitly instead.
    async fn create_platform_principal(&mut self) -> Uuid {
        let principal_id = Uuid::new_v4();
        let email = format!(
            "platform-device-{}@zeroship.test",
            Uuid::new_v4().simple()
        );
        self.state
            .control_pg
            .execute(
                "INSERT INTO zeroship.users (id, email, email_verified_at, name) \
                 VALUES ($1, $2::citext, NOW(), 'Platform Device User')",
                &[&principal_id, &email],
            )
            .await
            .expect("insert platform device user");
        self.users.push(principal_id);
        principal_id
    }

    /// Grant specific scopes directly, bypassing JIT provisioning.
    ///
    /// For tests that need pre-existing grants visible at the call site -
    /// asserting scope capping, or that a revoked grant is not resurrected -
    /// rather than hidden inside a fixture constructor.
    async fn grant_scopes(&mut self, principal_id: Uuid, scopes: &[&str]) {
        if !self.users.contains(&principal_id) {
            self.users.push(principal_id);
        }
        for scope in scopes {
            self.state
                .control_pg
                .execute(
                    "INSERT INTO zeroship.principal_grants (principal_id, grant_name) \
                     VALUES ($1, $2) \
                     ON CONFLICT (principal_id, grant_name) DO NOTHING",
                    &[&principal_id, scope],
                )
                .await
                .expect("insert granted scope");
        }
    }

    /// Mark a principal as already having gone through creator-grant JIT
    /// provisioning, without granting anything.
    ///
    /// Inserts the same `identity_links(provider='platform', provider_subject
    /// = principal_id::text)` once-only row `ensure_platform_creator_grants`
    /// writes on a principal's first approved poll. Combined with
    /// [`Self::grant_scopes`] for a reduced set, this simulates "an operator
    /// revoked a previously-seeded grant" so a test can assert the next
    /// device-flow poll does not resurrect it.
    async fn mark_creator_grants_seeded(&mut self, principal_id: Uuid) {
        if !self.users.contains(&principal_id) {
            self.users.push(principal_id);
        }
        self.state
            .control_pg
            .execute(
                "INSERT INTO zeroship.identity_links \
                    (principal_id, provider, provider_subject, email) \
                 SELECT $1, 'platform', $1::text, u.email::text \
                 FROM zeroship.users u WHERE u.id = $1 \
                 ON CONFLICT (provider, provider_subject) DO NOTHING",
                &[&principal_id],
            )
            .await
            .expect("insert platform identity link marker");
    }

    /// Pins that a rejected/unauthorized approval attempt left the row
    /// untouched. Does NOT pin anything about `platform_access_token_enc`:
    /// that column is dead (no production code reads or writes it anymore -
    /// minting moved to the poll, and it never wrote a token before this
    /// point in the flow either way), so asserting it here would not
    /// discriminate old behavior from new.
    async fn assert_device_grant_pending(&self, device_code_hash: &str) {
        let row = self
            .state
            .control_pg
            .query_one(
                "SELECT status, principal_id \
                 FROM zeroship.device_grants \
                 WHERE device_code_hash = $1",
                &[&device_code_hash],
            )
            .await
            .expect("pending device grant row");
        assert_eq!(row.get::<_, String>("status"), "pending");
        assert_eq!(row.get::<_, Option<Uuid>>("principal_id"), None);
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
            // Any principal that reached the "approved" arm of `device_token`
            // - regardless of which provider originally authenticated it -
            // gets an ADDITIONAL `identity_links(provider='platform')` row
            // from `ensure_platform_creator_grants`'s once-only marker. That
            // FK has no `onDelete`, so leaving this row behind makes the
            // `DELETE FROM zeroship.users` below fail silently (swallowed by
            // `let _ =`) and leak a row into every later test run.
            let subject = user_id.to_string();
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.identity_links \
                     WHERE provider = 'platform' AND provider_subject = $1",
                    &[&subject],
                )
                .await;
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
async fn platform_token_approves_device_grant_under_platform_provider() {
    let mut fx = Fixture::new_with_provider(FixtureProvider::Platform).await;
    let principal_id = fx.create_platform_principal().await;
    let device_code = format!("platform-device-{}", Uuid::new_v4().simple());
    let device_code_hash = fx.track_hash(&device_code);
    let user_code = random_user_code();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.device_grants \
                (device_code_hash, user_code, provider, scope, expires_at) \
             VALUES ($1, $2, 'platform', 'apps:deploy apps:read apps:write', \
                     NOW() + INTERVAL '10 minutes')",
            &[&device_code_hash, &user_code],
        )
        .await
        .expect("insert pending platform device grant");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(device_handlers::configure),
    )
    .await;

    let absent_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown at the end of this test.
    let absent_status = test::call_service(&app, absent_req).await.status();
    assert_eq!(absent_status, StatusCode::UNAUTHORIZED);
    fx.assert_device_grant_pending(&device_code_hash).await;

    let invalid_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", "Bearer not-a-jwt")
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    let invalid_status = test::call_service(&app, invalid_req).await.status();
    assert_eq!(invalid_status, StatusCode::UNAUTHORIZED);
    fx.assert_device_grant_pending(&device_code_hash).await;

    let wrong_provider_token = gotrue_token(
        &fx._mock_supabase.issuer(),
        &Uuid::new_v4().to_string(),
        "wrong-provider@zeroship.test",
        "authenticated",
    );
    let wrong_provider_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&wrong_provider_token))
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    let wrong_provider_status = test::call_service(&app, wrong_provider_req).await.status();
    assert_eq!(wrong_provider_status, StatusCode::UNAUTHORIZED);
    fx.assert_device_grant_pending(&device_code_hash).await;

    let approval_token = fx
        ._mock_platform
        .issue_access_token(principal_id, &[]);
    let approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&approval_token))
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    let approve_status = test::call_service(&app, approve_req).await.status();

    assert_eq!(approve_status, StatusCode::NO_CONTENT);
    let approved = fx
        .state
        .control_pg
        .query_one(
            "SELECT status, principal_id \
             FROM zeroship.device_grants \
             WHERE device_code_hash = $1",
            &[&device_code_hash],
        )
        .await
        .expect("approved platform device grant");
    assert_eq!(approved.get::<_, String>("status"), "approved");
    assert_eq!(approved.get::<_, Uuid>("principal_id"), principal_id);

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
async fn dual_issuer_gotrue_device_flow_enforces_hashing_auth_and_one_time_use() {
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
    // Derived from the configured platform issuer via `device_grant::op_public_url`,
    // not from `app_base_domain` - the fixture's mock platform issuer is
    // `{mock_platform.base}/oauth2`, so the `/device` page's origin is
    // `mock_platform.base` with that suffix stripped back off.
    assert_eq!(
        auth_body["verification_uri"].as_str(),
        Some(format!("{}/device", fx._mock_platform.base).as_str())
    );
    assert_eq!(
        auth_body["verification_uri_complete"].as_str(),
        Some(format!("{}/device?user_code={user_code}", fx._mock_platform.base).as_str())
    );

    let device_code_hash = fx.track_hash(device_code);
    let row = fx
        .state
        .control_pg
        .query_one(
            "SELECT device_code_hash, user_code, status, provider, scope \
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
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown at the end of this test.
    let no_bearer_status = test::call_service(&app, no_bearer_req).await.status();
    assert_eq!(no_bearer_status, StatusCode::UNAUTHORIZED);
    fx.assert_device_grant_pending(&device_code_hash).await;

    let invalid_bearer_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", "Bearer not-a-jwt")
        .set_json(&json!({
            "user_code": user_code
        }))
        .to_request();
    let invalid_bearer_status = test::call_service(&app, invalid_bearer_req).await.status();
    assert_eq!(invalid_bearer_status, StatusCode::UNAUTHORIZED);
    fx.assert_device_grant_pending(&device_code_hash).await;

    let subject = Uuid::new_v4().to_string();
    let email = format!("device-flow-{}@zeroship.test", Uuid::new_v4().simple());
    let wrong_role_token = gotrue_token(fx.issuer(), &subject, &email, "anon");
    let wrong_role_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&wrong_role_token))
        .set_json(&json!({
            "user_code": user_code
        }))
        .to_request();
    let wrong_role_status = test::call_service(&app, wrong_role_req).await.status();
    assert_eq!(wrong_role_status, StatusCode::UNAUTHORIZED);
    fx.assert_device_grant_pending(&device_code_hash).await;

    let bearer_token = gotrue_token(fx.issuer(), &subject, &email, "authenticated");

    // Malformed shape ("ZZZZ-ZZZZ" - two groups, and the old 8-char format
    // control used to mint): `device_grant::valid_user_code` rejects this
    // before the handler ever runs the lookup UPDATE. This test cannot prove
    // the DB was never queried (the response is identical to a well-formed
    // miss below), only that the response is a 400 `invalid_user_code`.
    let malformed_approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&bearer_token))
        .set_json(&json!({
            "user_code": "ZZZZ-ZZZZ"
        }))
        .to_request();
    let malformed_approve_resp = test::call_service(&app, malformed_approve_req).await;
    assert_eq!(malformed_approve_resp.status(), StatusCode::BAD_REQUEST);
    let malformed_body: Value =
        serde_json::from_slice(&test::read_body(malformed_approve_resp).await)
            .expect("malformed approve body json");
    assert_eq!(malformed_body["error"], "invalid_user_code");

    // Well-formed but never inserted: this is the lookup-miss path
    // specifically, distinct from the format-rejection case above - the code
    // passes `valid_user_code` and the handler's UPDATE affects zero rows.
    let unknown_approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&bearer_token))
        .set_json(&json!({
            "user_code": random_user_code()
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
    let approve_status = test::call_service(&app, approve_req).await.status();
    assert_eq!(approve_status, StatusCode::NO_CONTENT);
    let principal_id = fx.track_principal_for_subject(&subject).await;

    // Approval binds a principal and nothing more now: no token is minted or
    // stored here (that moved to the poll below), so there is nothing left
    // to assert about `platform_access_token_enc` at this point in the flow.
    let row = fx
        .state
        .control_pg
        .query_one(
            "SELECT status, principal_id \
             FROM zeroship.device_grants \
             WHERE device_code_hash = $1",
            &[&device_code_hash],
        )
        .await
        .expect("approved row");
    assert_eq!(row.get::<_, String>("status"), "approved");
    assert_eq!(row.get::<_, Uuid>("principal_id"), principal_id);

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
        deploy_token.split('.').count(),
        3,
        "minted token must be a compact JWS"
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
    let deploy_check_status = test::call_service(&app, deploy_check_req).await.status();
    assert_eq!(
        deploy_check_status,
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

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Starting a device flow is rate limited.
///
/// `device_auth` takes no credential and every accepted call writes a row to
/// `zeroship.device_grants`, so before this limit one caller could fill that
/// table without ever holding a token. The limiter runs ahead of the provider
/// check for the same reason: probing a disabled endpoint should cost the
/// caller its allowance too.
///
/// Unlike the deploy limiter test this caller is deliberately UNAUTHENTICATED,
/// because that is the endpoint's real threat model - there is no extractor in
/// front of the handler to reject it first.
#[compio::test]
async fn device_auth_is_rate_limited() {
    // Either provider would do now: `device_auth` mints grant rows under both,
    // so the accepted calls really do insert the thing being bounded. DualIssuer
    // is kept because it is the wider configuration. (This comment used to say
    // Platform was unusable here - "under the Platform-only provider
    // `device_auth` is disabled and answers 400 before it can mint anything".
    // That was true, and was the bug `ensure_platform_device_provider` carried;
    // it read as a design note rather than a defect, which is part of why it
    // survived.)
    let fx = Fixture::new_probing_the_admin_limiter(
        FixtureProvider::DualIssuer,
        Quota::per_minute(5, 60),
    )
    .await;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(device_handlers::configure),
    )
    .await;

    // The bucket lives in Postgres and outlives the process, so a narrow
    // identity space would eventually reuse one that has already spent tokens.
    let r = Uuid::new_v4().as_u128();
    let caller_ip = format!(
        "10.{}.{}.{}",
        (r >> 16) as u8,
        (r >> 8) as u8,
        (r as u8) | 1
    );

    let mut statuses = Vec::new();
    for _ in 0..31 {
        let req = test::TestRequest::post()
            .uri("/api/device/auth")
            .header("x-forwarded-for", caller_ip.as_str())
            .set_json(&json!({ "client_id": "zeroship-cli" }))
            .to_request();
        statuses.push(test::call_service(&app, req).await.status());
    }

    assert!(
        statuses.contains(&StatusCode::TOO_MANY_REQUESTS),
        "31 anonymous device-auth calls must trip the limiter; got {statuses:?}",
    );
    let minted = statuses.iter().filter(|s| s.is_success()).count();
    assert!(
        minted > 0 && minted < statuses.len(),
        "the limiter must bound grant creation without disabling it; {minted} of {} calls minted",
        statuses.len(),
    );

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// The control-side device flow must work on the SHIPPED DEFAULT provider.
///
/// `ZEROSHIP_AUTH_PROVIDER` unset or `platform` maps to `AuthProvider::Platform`
/// (`control_auth_provider_kind`, crates/control/src/main.rs), and
/// `AuthProvider::Platform::supabase_url()` returns `None` by construction.
/// While `ensure_platform_device_provider` also demanded a Supabase URL,
/// `/api/device/auth` answered 400 `unsupported_provider` on every default
/// deployment, so `zeroship login` could not even start.
///
/// This walks the whole CONTROL side under the platform-only provider: start
/// -> approve with a platform OAuth bearer -> redeem the one-time token.
///
/// `create_platform_principal` no longer pre-inserts `principal_grants`, so
/// this test's `scope == "apps:deploy apps:read apps:write"` assertion below
/// is also, incidentally, proof that `identity_bridge::ensure_platform_creator_grants`
/// JIT-provisions the full deploy scope for a principal with zero grants.
/// `device_token_jit_provisions_full_deploy_scope_for_a_new_platform_principal`
/// exists as a standalone, narrower test of that same claim.
///
/// What it does NOT cover: the browser leg. Nothing the auth service renders
/// under `AuthProviderKind::Native` posts to `/api/device/approve` - its
/// `/device` page drives the native OP grant, which reads
/// `zeroship.device_grants` rows with `provider = 'op'`, not the
/// `provider = 'platform'` rows control writes here. A human still cannot
/// approve this grant from the returned `verification_uri`.
#[compio::test]
async fn platform_only_provider_completes_the_control_device_flow() {
    let mut fx = Fixture::new_with_provider(FixtureProvider::Platform).await;
    let principal_id = fx.create_platform_principal().await;

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
            "scope": "openid offline_access apps:deploy apps:read apps:write"
        }))
        .to_request();
    let auth_resp = test::call_service(&app, auth_req).await;
    assert_eq!(
        auth_resp.status(),
        StatusCode::OK,
        "the shipped default provider must be able to start a device flow"
    );
    let auth_body: Value =
        serde_json::from_slice(&test::read_body(auth_resp).await).expect("auth body json");
    let device_code = auth_body["device_code"].as_str().expect("device_code");
    let user_code = auth_body["user_code"].as_str().expect("user_code");
    let device_code_hash = fx.track_hash(device_code);
    fx.assert_device_grant_pending(&device_code_hash).await;

    let approval_token = fx._mock_platform.issue_access_token(principal_id, &[]);
    let approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&approval_token))
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    let approve_status = test::call_service(&app, approve_req).await.status();
    assert_eq!(approve_status, StatusCode::NO_CONTENT);

    let token_req = test::TestRequest::post()
        .uri("/api/device/token")
        .set_json(&json!({
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        }))
        .to_request();
    let token_resp = test::call_service(&app, token_req).await;
    assert_eq!(token_resp.status(), StatusCode::OK);
    let token_body: Value =
        serde_json::from_slice(&test::read_body(token_resp).await).expect("token body json");
    assert_eq!(token_body["provider"], "platform");
    assert_eq!(token_body["principal_id"], principal_id.to_string());
    assert_eq!(token_body["scope"], "apps:deploy apps:read apps:write");
    let access_token = token_body["access_token"].as_str().expect("access_token");
    let verified = fx
        .state
        .auth_provider
        .verify_token(access_token)
        .await
        .expect("minted platform token verifies");
    assert_eq!(
        verified.provider_authz,
        ProviderAuthz::OAuthScope("apps:deploy apps:read apps:write".to_string())
    );

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// A bearer minted for a DIFFERENT audience must not approve a device grant.
///
/// `zeroship_authn`'s `oauth_guard_from_bearer` has always gated the same token
/// type on `aud`; this handler accepted `ProviderAuthz::OAuthScope(_)`
/// unconditionally, so any platform-issued token verified here regardless of
/// which resource server it was minted for.
///
/// Fails on the pre-fix code: the approval returned 204 and the grant went to
/// `approved`.
///
/// What it does NOT catch: whether the audience the handler compares against is
/// the RIGHT one for this deployment. It asserts agreement with
/// `state.expected_oauth_audience`, which is the same value `authn` compares
/// against, and nothing here would notice if that value were misconfigured.
#[compio::test]
async fn a_bearer_for_another_audience_cannot_approve_a_device_grant() {
    let mut fx = Fixture::new_with_provider(FixtureProvider::Platform).await;
    let principal_id = fx.create_platform_principal().await;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(device_handlers::configure),
    )
    .await;

    let user_code = random_user_code();
    let device_code_hash = format!("{:064x}", rand::random::<u128>());
    fx.insert_pending_platform_grant(&device_code_hash, &user_code)
        .await;

    let foreign_token = fx._mock_platform.issue_access_token_for_audience(
        principal_id,
        &[],
        "some-other-resource.zeroship.ai",
    );
    let approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&foreign_token))
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    let approve_status = test::call_service(&app, approve_req).await.status();
    assert_eq!(
        approve_status,
        StatusCode::UNAUTHORIZED,
        "a token minted for another audience must not approve a deploy grant"
    );
    fx.assert_device_grant_pending(&device_code_hash).await;

    // The one-variable control: the SAME principal, the SAME mock issuer, the
    // SAME grant row, only the audience corrected. Without this the test could
    // not tell "the audience check fired" from "this fixture cannot approve
    // anything".
    let good_token = fx._mock_platform.issue_access_token(principal_id, &[]);
    let approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&good_token))
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    assert_eq!(
        test::call_service(&app, approve_req).await.status(),
        StatusCode::NO_CONTENT,
        "the correct audience must still approve"
    );

    fx.cleanup().await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// A bearer whose token family has been revoked must not approve a grant.
///
/// Without this check a stolen bearer could start a device flow
/// (`/api/device/auth` is unauthenticated), approve it with itself, and poll
/// out a fresh deploy token whose `iat` is later than the revocation marker -
/// so revoking the family, and the deploy token's own TTL, bounded nothing.
///
/// Fails on the pre-fix code: the approval returned 204.
///
/// WHAT THIS TEST DOES NOT PROVE, and it matters: that any production code path
/// ever writes the marker this test inserts by hand. It does not. The only
/// writer of `zeroship.token_revocations` is
/// `crates/control/src/oauth_grants_handlers.rs`, which keys the row on a
/// PAIRWISE subject derived for a per-app RP client, while the token reaching
/// this handler carries a raw principal UUID as its `sub`. So the check is
/// correct and it agrees with `authn`, but on today's tree no operator action
/// can arm it for this family. Fixing that is a separate change; this test is
/// written so it exercises the check rather than passing because the check is
/// unreachable.
#[compio::test]
async fn a_revoked_bearer_cannot_approve_a_device_grant() {
    let mut fx = Fixture::new_with_provider(FixtureProvider::Platform).await;
    let principal_id = fx.create_platform_principal().await;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(device_handlers::configure),
    )
    .await;

    let user_code = random_user_code();
    let device_code_hash = format!("{:064x}", rand::random::<u128>());
    fx.insert_pending_platform_grant(&device_code_hash, &user_code)
        .await;

    let token = fx._mock_platform.issue_access_token(principal_id, &[]);
    // `revoked_after` is set in the FUTURE relative to the token's `iat` so the
    // marker unambiguously covers it; `family_revoked_at` compares the two, and
    // a marker stamped at the same whole second as `iat` would make the outcome
    // depend on clock granularity rather than on the rule.
    let client_id = fx._mock_platform.token_client_id();
    let subject = principal_id.to_string();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.token_revocations (client_id, sub, revoked_after) \
             VALUES ($1, $2, NOW() + INTERVAL '1 hour') \
             ON CONFLICT (client_id, sub) DO UPDATE SET revoked_after = EXCLUDED.revoked_after",
            &[&client_id, &subject],
        )
        .await
        .expect("insert revocation marker");

    let approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&token))
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    assert_eq!(
        test::call_service(&app, approve_req).await.status(),
        StatusCode::UNAUTHORIZED,
        "a revoked token family must not approve a deploy grant"
    );
    fx.assert_device_grant_pending(&device_code_hash).await;

    fx.state
        .control_pg
        .execute(
            "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
            &[&client_id, &subject],
        )
        .await
        .expect("delete revocation marker");

    fx.cleanup().await;
    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// A GoTrue bearer must still be refused when the provider has no Supabase.
///
/// One-variable partner of the test above: same platform-only provider, same
/// pending `provider = 'platform'` grant, only the bearer's provenance
/// differs. Dropping the Supabase clause from `ensure_platform_device_provider`
/// must not widen who may approve a grant, and this assertion holds on BOTH
/// sides of that change - which is what makes the pair discriminating rather
/// than two tests that move together.
///
/// The grant row is inserted directly rather than through `/api/device/auth`
/// on purpose: this test has to be runnable, and green, on the code where
/// `/api/device/auth` still answers 400 under a platform-only provider.
/// `platform_token_approves_device_grant_under_platform_provider` asserts the
/// same refusal in passing; this one exists standalone so the control can be
/// run, and seen to pass, on its own.
///
/// What it does NOT cover: the `supabase_url().is_none()` arm inside
/// `device_approval_principal`. That arm is unreachable, and the argument had
/// to be re-derived when `LegacyAuthProvider` was replaced by
/// `AuthProvider::new(Vec<ConfiguredProvider>)` - the old form named a type
/// that no longer exists. Under the set model: the only producer of
/// `ProviderAuthz::GoTrueRole` is `ConfiguredProvider::Supabase`, and
/// `AuthProvider::supabase_url()` answers `Some` for exactly the sets holding
/// such an element, so a bearer that verified as `GoTrueRole` proves the
/// element is present. Set SIZE is irrelevant; a set holding both backends
/// still answers `Some`. Here the GoTrue bearer never verifies at all, so
/// approval fails one step earlier, in `verified_device_approval_bearer`.
#[compio::test]
async fn platform_only_provider_refuses_a_gotrue_bearer() {
    let mut fx = Fixture::new_with_provider(FixtureProvider::Platform).await;
    let device_code = format!("platform-gotrue-{}", Uuid::new_v4().simple());
    let device_code_hash = fx.track_hash(&device_code);
    let user_code = random_user_code();
    fx.state
        .control_pg
        .execute(
            "INSERT INTO zeroship.device_grants \
                (device_code_hash, user_code, provider, scope, expires_at) \
             VALUES ($1, $2, 'platform', 'apps:deploy apps:read apps:write', \
                     NOW() + INTERVAL '10 minutes')",
            &[&device_code_hash, &user_code],
        )
        .await
        .expect("insert pending platform device grant");

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(device_handlers::configure),
    )
    .await;

    let gotrue_bearer = gotrue_token(
        &fx._mock_supabase.issuer(),
        &Uuid::new_v4().to_string(),
        "gotrue-under-platform@zeroship.test",
        "authenticated",
    );
    let approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&gotrue_bearer))
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    let approve_status = test::call_service(&app, approve_req).await.status();
    assert_eq!(
        approve_status,
        StatusCode::UNAUTHORIZED,
        "a GoTrue bearer must not approve a grant on a provider with no Supabase"
    );
    fx.assert_device_grant_pending(&device_code_hash).await;

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// Without a platform OP the flow must refuse to start, not start and strand.
///
/// This is the clause that survives in `ensure_platform_device_provider` after
/// the Supabase clause was dropped, and it is load-bearing: approval mints
/// through the OP's `/internal/platform-token`, so a Supabase-only deployment
/// that accepted `/api/device/auth` would hand out a device code that can never
/// be redeemed and leave a row in `zeroship.device_grants` per attempt.
///
/// Added because a mutation showed nothing pinned it: replacing the whole guard
/// body with `Ok(())` left the device suite at 5 passed, 0 failed. The suite
/// covered the clause that was wrong and none of the one that was right.
#[compio::test]
async fn supabase_only_provider_cannot_start_a_device_flow() {
    let fx = Fixture::new_with_provider(FixtureProvider::SupabaseOnly).await;

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
            "scope": "openid offline_access apps:deploy apps:read apps:write"
        }))
        .to_request();
    let auth_resp = test::call_service(&app, auth_req).await;
    assert_eq!(auth_resp.status(), StatusCode::BAD_REQUEST);
    let auth_body: Value =
        serde_json::from_slice(&test::read_body(auth_resp).await).expect("auth body json");
    assert_eq!(token_error(auth_body), "unsupported_provider");

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// A principal with NO grants at all gets the full deploy scope on its first
/// approved poll.
///
/// `create_platform_principal` inserts only a bare `zeroship.users` row now -
/// no `principal_grants`, no `identity_links` marker - so
/// `identity_bridge::ensure_platform_creator_grants` runs its once-only seed
/// path for the first time inside `device_token`'s "approved" arm, before
/// `deploy_scopes_for_principal` computes the token's scope. This asserts
/// both ends of that: the `principal_grants` table ends up holding all three
/// `DEFAULT_CREATOR_GRANTS`, and the minted token's scope reflects them.
///
/// What it does NOT cover: idempotency of a SECOND login (that a revoked
/// grant stays revoked on a later poll) - that is
/// `device_token_keeps_reduced_grants_when_identity_link_already_seeded`.
#[compio::test]
async fn device_token_jit_provisions_full_deploy_scope_for_a_new_platform_principal() {
    let mut fx = Fixture::new_with_provider(FixtureProvider::Platform).await;
    let principal_id = fx.create_platform_principal().await;

    let no_grants = fx
        .state
        .control_pg
        .query_one(
            "SELECT COUNT(*)::INT8 AS n FROM zeroship.principal_grants WHERE principal_id = $1",
            &[&principal_id],
        )
        .await
        .expect("count principal_grants before approval")
        .get::<_, i64>("n");
    assert_eq!(no_grants, 0, "fixture must start this principal with zero grants");

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
            "scope": "apps:deploy apps:read apps:write"
        }))
        .to_request();
    let auth_resp = test::call_service(&app, auth_req).await;
    assert_eq!(auth_resp.status(), StatusCode::OK);
    let auth_body: Value =
        serde_json::from_slice(&test::read_body(auth_resp).await).expect("auth body json");
    let device_code = auth_body["device_code"].as_str().expect("device_code");
    let user_code = auth_body["user_code"].as_str().expect("user_code");
    fx.track_hash(device_code);

    let approval_token = fx._mock_platform.issue_access_token(principal_id, &[]);
    let approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&approval_token))
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    assert_eq!(
        test::call_service(&app, approve_req).await.status(),
        StatusCode::NO_CONTENT
    );

    let token_req = test::TestRequest::post()
        .uri("/api/device/token")
        .set_json(&json!({
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        }))
        .to_request();
    let token_resp = test::call_service(&app, token_req).await;
    assert_eq!(token_resp.status(), StatusCode::OK);
    let token_body: Value =
        serde_json::from_slice(&test::read_body(token_resp).await).expect("token body json");
    assert_eq!(token_body["scope"], "apps:deploy apps:read apps:write");

    let granted: Vec<String> = fx
        .state
        .control_pg
        .query(
            "SELECT grant_name FROM zeroship.principal_grants \
             WHERE principal_id = $1 ORDER BY grant_name",
            &[&principal_id],
        )
        .await
        .expect("query principal_grants after approval")
        .iter()
        .map(|row| row.get::<_, String>("grant_name"))
        .collect();
    assert_eq!(granted, vec!["apps:deploy", "apps:read", "apps:write"]);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// A principal whose `identity_links` seeding marker already exists, but
/// whose grants an operator later reduced, keeps the reduced set - a second
/// device-flow login must not resurrect a revoked grant.
///
/// `mark_creator_grants_seeded` inserts exactly the marker row
/// `ensure_platform_creator_grants` writes on first seed, WITHOUT granting
/// anything through it; `grant_scopes` then grants only `apps:read`
/// explicitly, standing in for "the operator revoked apps:write and
/// apps:deploy after the original seed". If `ensure_platform_creator_grants`
/// keyed off `principal_grants` being empty instead of the identity_links
/// marker, this principal (zero matching grants at the time of the check,
/// same as a brand-new principal) would get all three re-granted here.
///
/// What it does NOT cover: the ordinary first-seed path (see
/// `device_token_jit_provisions_full_deploy_scope_for_a_new_platform_principal`),
/// or an operator revoking a grant mid-session (there is no session to
/// revoke mid-flight; grants are read fresh on every poll).
#[compio::test]
async fn device_token_keeps_reduced_grants_when_identity_link_already_seeded() {
    let mut fx = Fixture::new_with_provider(FixtureProvider::Platform).await;
    let principal_id = fx.create_platform_principal().await;
    fx.mark_creator_grants_seeded(principal_id).await;
    fx.grant_scopes(principal_id, &["apps:read"]).await;

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
            "scope": "apps:deploy apps:read apps:write"
        }))
        .to_request();
    let auth_resp = test::call_service(&app, auth_req).await;
    assert_eq!(auth_resp.status(), StatusCode::OK);
    let auth_body: Value =
        serde_json::from_slice(&test::read_body(auth_resp).await).expect("auth body json");
    let device_code = auth_body["device_code"].as_str().expect("device_code");
    let user_code = auth_body["user_code"].as_str().expect("user_code");
    fx.track_hash(device_code);

    let approval_token = fx._mock_platform.issue_access_token(principal_id, &[]);
    let approve_req = test::TestRequest::post()
        .uri("/api/device/approve")
        .header("authorization", bearer(&approval_token))
        .set_json(&json!({ "user_code": user_code }))
        .to_request();
    assert_eq!(
        test::call_service(&app, approve_req).await.status(),
        StatusCode::NO_CONTENT
    );

    let token_req = test::TestRequest::post()
        .uri("/api/device/token")
        .set_json(&json!({
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
        }))
        .to_request();
    let token_resp = test::call_service(&app, token_req).await;
    assert_eq!(token_resp.status(), StatusCode::OK);
    let token_body: Value =
        serde_json::from_slice(&test::read_body(token_resp).await).expect("token body json");
    assert_eq!(
        token_body["scope"], "apps:read",
        "a previously-seeded principal must not have a revoked grant resurrected"
    );
    let access_token = token_body["access_token"].as_str().expect("access_token");
    let verified = fx
        .state
        .auth_provider
        .verify_token(access_token)
        .await
        .expect("minted platform token verifies");
    assert_eq!(
        verified.provider_authz,
        ProviderAuthz::OAuthScope("apps:read".to_string())
    );

    let granted: Vec<String> = fx
        .state
        .control_pg
        .query(
            "SELECT grant_name FROM zeroship.principal_grants \
             WHERE principal_id = $1 ORDER BY grant_name",
            &[&principal_id],
        )
        .await
        .expect("query principal_grants after second approval")
        .iter()
        .map(|row| row.get::<_, String>("grant_name"))
        .collect();
    assert_eq!(
        granted,
        vec!["apps:read"],
        "ensure_platform_creator_grants must not re-insert apps:deploy/apps:write \
         once the identity_links marker already exists"
    );

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}
