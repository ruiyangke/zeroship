//! Integration tests for billing G1 (Stream-2): Connect onboarding ownership
//! verification + server-stamped application fee.
//!
//! FAITHFUL by construction: the tests drive the REAL ntex HTTP handlers
//! (`onboard`/`callback`/`connect_checkout`/`set_fee_policy`) through a real
//! `AuthzGuard` (a real PAT) against a live, migrated Postgres, and the REAL
//! `cyper`-based `StripeClient` against a localhost **mock-Stripe-Connect** HTTP
//! server that speaks the `/v1/accounts`, `/v1/account_links`,
//! `/v1/payment_intents` shapes and RECORDS every request. No stubbed client on
//! the wire path: form encoding, headers, round-trip, and JSON parse all run.
//!
//! Requires `CONTROL_TEST_DB`; silent skip otherwise. The DB must have changeset
//! 0044 applied.

#![allow(clippy::future_not_send)]

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_authz::{policy_hash, Action, Effect, Policy, Resource, Statement};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    stripe_handlers, token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-connect-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

// ===========================================================================
// Mock-Stripe-Connect HTTP server.
// ===========================================================================

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    idempotency_key: Option<String>,
    body: String,
}

#[derive(Default)]
struct MockState {
    requests: Vec<RecordedRequest>,
    /// acct_… → metadata.creator_id we stamped at create time. A `retrieve`
    /// echoes this back so the callback can verify ownership.
    accounts: HashMap<String, String>,
    /// Force the next created account id (so a test can pin a known acct_…).
    next_account_id: Option<String>,
    /// Onboarding flags returned by `retrieve_account`.
    charges_enabled: bool,
    payouts_enabled: bool,
    details_submitted: bool,
}

#[derive(Clone)]
struct MockStripe {
    state: Arc<Mutex<MockState>>,
    base_url: String,
}

impl MockStripe {
    fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    /// The form-encoded body of the first POST to `path`.
    fn first_body(&self, method: &str, path: &str) -> Option<String> {
        self.requests()
            .into_iter()
            .find(|r| r.method == method && r.path == path)
            .map(|r| r.body)
    }

    fn set_flags(&self, charges: bool, payouts: bool, details: bool) {
        let mut st = self.state.lock().unwrap();
        st.charges_enabled = charges;
        st.payouts_enabled = payouts;
        st.details_submitted = details;
    }
}

async fn start_mock_stripe() -> MockStripe {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let base_url = format!("http://{addr}");
    let state = Arc::new(Mutex::new(MockState {
        charges_enabled: true,
        payouts_enabled: true,
        details_submitted: true,
        ..MockState::default()
    }));
    let accept_state = Arc::clone(&state);

    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else { break };
            let conn_state = Arc::clone(&accept_state);
            compio::runtime::spawn(async move {
                serve_conn(stream, conn_state).await;
            })
            .detach();
        }
    })
    .detach();

    MockStripe { state, base_url }
}

async fn serve_conn(mut stream: TcpStream, state: Arc<Mutex<MockState>>) {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        loop {
            let Some((req, consumed)) = try_parse_request(&acc) else { break };
            acc.drain(0..consumed);
            let response = handle_mock_request(&req, &state);
            if stream.write_all(response).await.0.is_err() {
                return;
            }
        }
        let buf = vec![0u8; 4096];
        let compio::BufResult(n, buf) = stream.read(buf).await;
        match n {
            Ok(0) | Err(_) => return,
            Ok(read) => acc.extend_from_slice(&buf[..read]),
        }
    }
}

fn try_parse_request(buf: &[u8]) -> Option<(RecordedRequest, usize)> {
    let text = std::str::from_utf8(buf).ok()?;
    let header_end = text.find("\r\n\r\n")?;
    let head = &text[..header_end];
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    let mut idempotency_key = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            match key.as_str() {
                "content-length" => content_length = val.parse().unwrap_or(0),
                "idempotency-key" => idempotency_key = Some(val),
                _ => {}
            }
        }
    }

    let body_start = header_end + 4;
    if buf.len() < body_start + content_length {
        return None;
    }
    let body = String::from_utf8_lossy(&buf[body_start..body_start + content_length]).to_string();
    Some((
        RecordedRequest { method, path, idempotency_key, body },
        body_start + content_length,
    ))
}

fn handle_mock_request(req: &RecordedRequest, state: &Arc<Mutex<MockState>>) -> Vec<u8> {
    // GET /v1/accounts/{id} — retrieve. Echo the stored metadata.creator_id +
    // the configured onboarding flags.
    if req.method == "GET" && req.path.starts_with("/v1/accounts/") {
        let acct = req.path.trim_start_matches("/v1/accounts/").to_string();
        let st = state.lock().unwrap();
        let creator = st.accounts.get(&acct).cloned().unwrap_or_default();
        let body = format!(
            r#"{{"id":"{acct}","object":"account","charges_enabled":{c},"payouts_enabled":{p},"details_submitted":{d},"metadata":{{"creator_id":"{creator}"}}}}"#,
            c = st.charges_enabled,
            p = st.payouts_enabled,
            d = st.details_submitted,
        );
        drop(st);
        state.lock().unwrap().requests.push(req.clone());
        return http_200_json(&body);
    }

    let json: String = if req.method == "POST" && req.path == "/v1/accounts" {
        // Create an Express account. Record the metadata.creator_id so retrieve
        // can echo it (ownership signal).
        let creator = form_param(&req.body, "metadata[creator_id]").unwrap_or_default();
        let mut st = state.lock().unwrap();
        let acct = st
            .next_account_id
            .take()
            .unwrap_or_else(|| format!("acct_{}", hexish()));
        st.accounts.insert(acct.clone(), creator);
        st.requests.push(req.clone());
        return http_200_json(&format!(r#"{{"id":"{acct}","object":"account"}}"#));
    } else if req.path == "/v1/account_links" {
        let acct = form_param(&req.body, "account").unwrap_or_default();
        format!(
            r#"{{"object":"account_link","url":"https://connect.stripe.test/setup/{acct}"}}"#
        )
    } else if req.path == "/v1/payment_intents" {
        format!(
            r#"{{"id":"pi_mock_{0}","object":"payment_intent","client_secret":"pi_mock_{0}_secret"}}"#,
            hexish()
        )
    } else {
        r#"{"id":"obj_mock","object":"unknown"}"#.to_string()
    };

    state.lock().unwrap().requests.push(req.clone());
    http_200_json(&json)
}

fn form_param(body: &str, name: &str) -> Option<String> {
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if percent_decode(k) == name {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn http_200_json(json: &str) -> Vec<u8> {
    let body = json.to_string().into_bytes();
    let mut resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(&body);
    resp
}

/// 16 lowercase-alnum chars — a valid `acct_…` suffix shape (no underscore).
fn hexish() -> String {
    Uuid::new_v4().simple().to_string()[..16].to_string()
}

// ===========================================================================
// Fixture.
// ===========================================================================

struct Fixture {
    state: Arc<AppState>,
    mock: MockStripe,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn build_fixture(db_url: &str, label: &str) -> Fixture {
    let mock = start_mock_stripe().await;
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));

    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls)
            .await
            .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new("sk_test_mock".to_string()),
        stripe_base_url: mock.base_url.clone(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: false,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg: Arc::new(control_pg_client),
        hydra_admin_url: "http://127.0.0.1:9".to_string(),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        pat_issuer: Arc::new(token_handlers::PatIssuer::dev_insecure()),
        hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
            "http://127.0.0.1:9",
        )),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider: zeroship_control::metering::provider::build_provider(
            &zeroship_control::metering::provider::MeteringProviderConfig::native(),
        )
        .expect("native provider builds"),
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

    Fixture { state, mock, blob_root, deploy_tmp_dir }
}

// ---------------------------------------------------------------------------
// PAT + principal helpers (faithful AuthzGuard).
// ---------------------------------------------------------------------------

struct Pat {
    user_id: Uuid,
    token_id: Uuid,
    token: String,
}

impl Pat {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

async fn make_user(state: &AppState, label: &str) -> Uuid {
    let id = Uuid::now_v7();
    let email = format!("{label}-{}@zeroship.test", id.simple());
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2, $3, NOW())",
            &[&id, &email, &label.to_string()],
        )
        .await
        .expect("insert user");
    id
}

async fn issue_pat(state: &AppState, user_id: Uuid, role: Option<&str>, policy: Policy) -> Pat {
    if let Some(role) = role {
        state
            .control_pg
            .execute(
                "INSERT INTO zeroship.platform_admin_roles (user_id, role, granted_by) \
                 VALUES ($1, $2, $1) ON CONFLICT DO NOTHING",
                &[&user_id, &role],
            )
            .await
            .expect("insert platform role");
    }
    let token_id = Uuid::new_v4();
    let policies = policy.to_json_value();
    let hash = policy_hash(&policies);
    let expires_at = Utc::now() + Duration::days(1);
    let token = state
        .pat_issuer
        .issue(token_id, user_id, hash.clone(), expires_at)
        .expect("issue PAT");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.permission_tokens \
                (id, owner_id, kind, name, policies, policy_hash, expires_at) \
             VALUES ($1, $2, 'pat', 'connect PAT', $3, $4, $5)",
            &[&token_id, &user_id, &policies, &hash, &expires_at],
        )
        .await
        .expect("insert PAT row");
    Pat { user_id, token_id, token }
}

/// Self-service policy: BillingWrite on the creator's OWN app surface — the
/// creator upper bound. Crucially does NOT grant Resource::Any (operator-only).
fn billing_write_self() -> Policy {
    Policy {
        name: "creator self".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::BillingWrite],
            // A self-scoped grant the creator legitimately holds; it is NOT
            // Resource::Any, so the operator-only `set_fee_policy` denies it.
            resources: vec![Resource::App { id: Uuid::new_v4().to_string() }],
            conditions: Vec::new(),
        }],
    }
}

fn billing_write_any() -> Policy {
    Policy {
        name: "operator".to_owned(),
        statements: vec![Statement {
            effect: Effect::Allow,
            actions: vec![Action::BillingWrite],
            resources: vec![Resource::Any],
            conditions: Vec::new(),
        }],
    }
}

async fn cleanup(state: &AppState, creators: &[Uuid], pats: &[&Pat]) {
    let pg = &state.control_pg;
    for c in creators {
        let _ = pg.execute("DELETE FROM zeroship.creator_fee_policy WHERE creator_id = $1", &[c]).await;
        let _ = pg.execute("DELETE FROM zeroship.payouts WHERE creator_id = $1", &[c]).await;
        let _ = pg.execute("DELETE FROM zeroship.creator_account_history WHERE creator_id = $1", &[c]).await;
        let _ = pg.execute("DELETE FROM zeroship.creator_accounts WHERE creator_id = $1", &[c]).await;
    }
    for p in pats {
        let _ = pg.execute("DELETE FROM zeroship.authz_decisions WHERE token_id = $1", &[&p.token_id]).await;
        let _ = pg.execute("DELETE FROM zeroship.permission_tokens WHERE id = $1", &[&p.token_id]).await;
        let _ = pg.execute("DELETE FROM zeroship.platform_admin_roles WHERE user_id = $1", &[&p.user_id]).await;
    }
    let _ = pg.execute("DELETE FROM zeroship.users WHERE id = ANY($1)", &[&creators.to_vec()]).await;
}

// ===========================================================================
// Tests.
// ===========================================================================

#[compio::test]
async fn onboard_returns_real_account_link() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "onboard").await;
    let creator = make_user(&fx.state, "creator").await;
    let pat = issue_pat(&fx.state, creator, None, billing_write_self()).await;

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/creators/{id}/stripe/onboard")
                .route(web::post().to(stripe_handlers::onboard)),
        ),
    )
    .await;

    let req = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/onboard"))
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), StatusCode::OK, "onboard should succeed");
    let bytes = test::read_body(resp).await;
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("response is JSON");
    let link = body["url"].as_str().expect("url present");
    assert!(
        link.starts_with("https://connect.stripe.test/setup/acct_"),
        "must be a REAL account_links URL (not the old express_login placeholder), got: {link}"
    );
    // The account_links POST happened against the REAL Stripe wire.
    assert!(
        fx.mock.first_body("POST", "/v1/account_links").is_some(),
        "onboard must POST /v1/account_links"
    );
    // The created account carries metadata.creator_id (the ownership signal).
    let create_body = fx.mock.first_body("POST", "/v1/accounts").expect("account create");
    assert!(
        create_body.contains(&creator.to_string().replace('-', "%2D"))
            || create_body.contains(&creator.to_string()),
        "create account must stamp metadata[creator_id]"
    );

    cleanup(&fx.state, &[creator], &[&pat]).await;
}

#[compio::test]
async fn callback_rejects_acct_not_owned_by_creator() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "callback-forge").await;
    let creator = make_user(&fx.state, "creator").await;
    let attacker = make_user(&fx.state, "attacker").await;
    let pat = issue_pat(&fx.state, creator, None, billing_write_self()).await;

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::scope("/api/creators/{id}")
                .service(web::resource("/stripe/onboard").route(web::post().to(stripe_handlers::onboard)))
                .service(web::resource("/stripe/callback").route(web::post().to(stripe_handlers::callback))),
        ),
    )
    .await;

    // Creator onboards → mints THEIR acct_… (stored server-side).
    let onboard = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/onboard"))
        .header("authorization", pat.bearer())
        .to_request();
    let resp = test::call_service(&svc, onboard).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Pre-seed a FOREIGN account in the mock that belongs to the ATTACKER, with
    // a real acct_… shape. The creator forges a callback claiming this acct_….
    let foreign_acct = format!("acct_{}", "f00dbabecafe1234");
    fx.mock
        .state
        .lock()
        .unwrap()
        .accounts
        .insert(foreign_acct.clone(), attacker.to_string());

    let forge = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/callback"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({ "stripe_account_id": foreign_acct }))
        .to_request();
    let resp = test::call_service(&svc, forge).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a forged/foreign acct_… that doesn't match the creator's onboarded account MUST be rejected (ISS-30)"
    );

    cleanup(&fx.state, &[creator, attacker], &[&pat]).await;
}

#[compio::test]
async fn callback_accepts_owned_account_and_persists_flags() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "callback-ok").await;
    let creator = make_user(&fx.state, "creator").await;
    let pat = issue_pat(&fx.state, creator, None, billing_write_self()).await;
    fx.mock.set_flags(true, true, true);

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::scope("/api/creators/{id}")
                .service(web::resource("/stripe/onboard").route(web::post().to(stripe_handlers::onboard)))
                .service(web::resource("/stripe/callback").route(web::post().to(stripe_handlers::callback))),
        ),
    )
    .await;

    let onboard = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/onboard"))
        .header("authorization", pat.bearer())
        .to_request();
    assert_eq!(test::call_service(&svc, onboard).await.status(), StatusCode::OK);

    // Callback with NO body acct hint — server retrieves + verifies the stored acct.
    let cb = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/callback"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({}))
        .to_request();
    let resp = test::call_service(&svc, cb).await;
    assert_eq!(resp.status(), StatusCode::OK, "owned account verifies");
    let bytes = test::read_body(resp).await;
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("response is JSON");
    assert_eq!(body["charges_enabled"], serde_json::json!(true));

    // The verified flags are persisted.
    let row = fx
        .state
        .control_pg
        .query_one(
            "SELECT charges_enabled, payouts_enabled, details_submitted \
             FROM zeroship.creator_accounts WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("account row");
    assert!(row.get::<_, bool>("charges_enabled"));
    assert!(row.get::<_, bool>("payouts_enabled"));
    assert!(row.get::<_, bool>("details_submitted"));

    cleanup(&fx.state, &[creator], &[&pat]).await;
}

#[compio::test]
async fn checkout_stamps_server_fee_not_client_value() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "checkout-fee").await;
    let creator = make_user(&fx.state, "creator").await;
    let pat = issue_pat(&fx.state, creator, None, billing_write_self()).await;

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::scope("/api/creators/{id}")
                .service(web::resource("/stripe/onboard").route(web::post().to(stripe_handlers::onboard)))
                .service(web::resource("/stripe/callback").route(web::post().to(stripe_handlers::callback)))
                .service(web::resource("/connect/checkout").route(web::post().to(stripe_handlers::connect_checkout))),
        ),
    )
    .await;

    // Onboard to mint a connected account, then complete onboarding (callback
    // persists charges_enabled=true — the M1 gate requires a ready account).
    let onboard = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/onboard"))
        .header("authorization", pat.bearer())
        .to_request();
    assert_eq!(test::call_service(&svc, onboard).await.status(), StatusCode::OK);
    let cb = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/callback"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({}))
        .to_request();
    assert_eq!(test::call_service(&svc, cb).await.status(), StatusCode::OK);

    // No fee policy row → DEFAULT 15%. Charge $200.00 (20000 cents). A MALICIOUS
    // client tries to set application_fee_amount=1 in the body — it has no wire
    // path and MUST be ignored.
    let checkout = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/connect/checkout"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({
            "amount_cents": 20000,
            "currency": "usd",
            "cart_id": "cart-1",
            // Attacker-supplied fields the server must NOT honor:
            "application_fee_amount": 1,
            "application_fee_percent": 0,
            "applicationFeePercent": 0
        }))
        .to_request();
    let resp = test::call_service(&svc, checkout).await;
    assert_eq!(resp.status(), StatusCode::OK, "checkout should succeed");
    let bytes = test::read_body(resp).await;
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("response is JSON");
    // The SERVER-resolved fee is 15% of 20000 = 3000.
    assert_eq!(body["application_fee_cents"], serde_json::json!(3000));

    // FAITHFUL: assert the REAL Stripe wire carried application_fee_amount=3000,
    // transfer_data[destination]=acct_…, and amount=20000 — NOT the client's 1.
    let pi_body = fx.mock.first_body("POST", "/v1/payment_intents").expect("PI created");
    assert!(
        pi_body.contains("application_fee_amount=3000"),
        "the Connect charge must carry the SERVER fee (3000), got: {pi_body}"
    );
    assert!(
        !pi_body.contains("application_fee_amount=1"),
        "the client-supplied fee (1) must be IGNORED, got: {pi_body}"
    );
    assert!(pi_body.contains("amount=20000"), "charge amount, got: {pi_body}");
    assert!(
        pi_body.contains("transfer_data%5Bdestination%5D=acct_")
            || pi_body.contains("transfer_data[destination]=acct_"),
        "must route to the connected account, got: {pi_body}"
    );

    cleanup(&fx.state, &[creator], &[&pat]).await;
}

#[compio::test]
async fn checkout_honors_operator_set_fee_policy() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "checkout-policy").await;
    let creator = make_user(&fx.state, "creator").await;
    let op = make_user(&fx.state, "operator").await;
    let creator_pat = issue_pat(&fx.state, creator, None, billing_write_self()).await;
    let op_pat = issue_pat(&fx.state, op, Some("billing"), billing_write_any()).await;

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::scope("/api/creators/{id}")
                .service(web::resource("/stripe/onboard").route(web::post().to(stripe_handlers::onboard)))
                .service(web::resource("/stripe/callback").route(web::post().to(stripe_handlers::callback)))
                .service(web::resource("/connect/checkout").route(web::post().to(stripe_handlers::connect_checkout)))
                .service(web::resource("/fee-policy").route(web::put().to(stripe_handlers::set_fee_policy))),
        ),
    )
    .await;

    // Onboard the creator, then complete onboarding (charges_enabled=true).
    let onboard = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/onboard"))
        .header("authorization", creator_pat.bearer())
        .to_request();
    assert_eq!(test::call_service(&svc, onboard).await.status(), StatusCode::OK);
    let cb = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/callback"))
        .header("authorization", creator_pat.bearer())
        .set_json(&serde_json::json!({}))
        .to_request();
    assert_eq!(test::call_service(&svc, cb).await.status(), StatusCode::OK);

    // Operator sets a 25% policy capped at $40 (4000 cents).
    let set = test::TestRequest::put()
        .uri(&format!("/api/creators/{creator}/fee-policy"))
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({ "kind": "percent", "percent_bps": 2500, "cap_cents": 4000 }))
        .to_request();
    assert_eq!(
        test::call_service(&svc, set).await.status(),
        StatusCode::NO_CONTENT,
        "operator may set the fee policy"
    );

    // Charge $200 → 25% = 5000, capped to 4000.
    let checkout = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/connect/checkout"))
        .header("authorization", creator_pat.bearer())
        .set_json(&serde_json::json!({ "amount_cents": 20000, "currency": "usd", "cart_id": "c2" }))
        .to_request();
    let resp = test::call_service(&svc, checkout).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = test::read_body(resp).await;
    let body: serde_json::Value = serde_json::from_slice(&bytes).expect("response is JSON");
    assert_eq!(body["application_fee_cents"], serde_json::json!(4000), "25% capped at 4000");

    cleanup(&fx.state, &[creator, op], &[&creator_pat, &op_pat]).await;
}

/// M1 (RED→GREEN): a creator who ran `onboard` but whose Stripe account is NOT
/// yet `charges_enabled` MUST NOT reach the charge path. `connect_checkout`
/// returns 400 and posts NO PaymentIntent.
#[compio::test]
async fn checkout_rejected_when_charges_not_enabled() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "checkout-not-ready").await;
    let creator = make_user(&fx.state, "creator").await;
    let pat = issue_pat(&fx.state, creator, None, billing_write_self()).await;
    // The account exists but onboarding is incomplete: charges are NOT enabled.
    fx.mock.set_flags(false, false, false);

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::scope("/api/creators/{id}")
                .service(web::resource("/stripe/onboard").route(web::post().to(stripe_handlers::onboard)))
                .service(web::resource("/stripe/callback").route(web::post().to(stripe_handlers::callback)))
                .service(web::resource("/connect/checkout").route(web::post().to(stripe_handlers::connect_checkout))),
        ),
    )
    .await;

    // Onboard mints the acct_… (charges_enabled defaults to false on the row).
    let onboard = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/onboard"))
        .header("authorization", pat.bearer())
        .to_request();
    assert_eq!(test::call_service(&svc, onboard).await.status(), StatusCode::OK);

    // Callback verifies + persists the (false) flags from Stripe's truth.
    let cb = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/callback"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({}))
        .to_request();
    assert_eq!(test::call_service(&svc, cb).await.status(), StatusCode::OK);

    let pis_before = fx
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/payment_intents")
        .count();

    // Checkout must be REJECTED with 400 before any PI POST.
    let checkout = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/connect/checkout"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({ "amount_cents": 20000, "currency": "usd", "cart_id": "cart-x" }))
        .to_request();
    let resp = test::call_service(&svc, checkout).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "checkout must be rejected when the connected account is not charges_enabled (M1)"
    );

    let pis_after = fx
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/payment_intents")
        .count();
    assert_eq!(
        pis_before, pis_after,
        "no PaymentIntent may be POSTed when charges are not enabled (M1)"
    );

    cleanup(&fx.state, &[creator], &[&pat]).await;
}

/// M2 (RED→GREEN): an empty `cart_id` must be rejected (it would otherwise
/// collapse every checkout for a creator onto ONE idempotency key, replaying a
/// stale charge for a different amount). Two checkouts with empty cart_id and
/// different amounts must NOT return the same PaymentIntent. After the fix the
/// empty cart_id is a 400 — no PI is created at all, so no stale replay.
#[compio::test]
async fn checkout_rejects_empty_cart_id_no_stale_replay() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "checkout-cartid").await;
    let creator = make_user(&fx.state, "creator").await;
    let pat = issue_pat(&fx.state, creator, None, billing_write_self()).await;
    fx.mock.set_flags(true, true, true);

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::scope("/api/creators/{id}")
                .service(web::resource("/stripe/onboard").route(web::post().to(stripe_handlers::onboard)))
                .service(web::resource("/stripe/callback").route(web::post().to(stripe_handlers::callback)))
                .service(web::resource("/connect/checkout").route(web::post().to(stripe_handlers::connect_checkout))),
        ),
    )
    .await;

    let onboard = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/onboard"))
        .header("authorization", pat.bearer())
        .to_request();
    assert_eq!(test::call_service(&svc, onboard).await.status(), StatusCode::OK);
    let cb = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/callback"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({}))
        .to_request();
    assert_eq!(test::call_service(&svc, cb).await.status(), StatusCode::OK);

    // First checkout with NO cart_id, amount $200.
    let c1 = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/connect/checkout"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({ "amount_cents": 20000, "currency": "usd" }))
        .to_request();
    let r1 = test::call_service(&svc, c1).await;
    assert_eq!(
        r1.status(),
        StatusCode::BAD_REQUEST,
        "an absent/empty cart_id must be rejected (M2 — would otherwise replay a stale charge)"
    );

    // Second checkout with NO cart_id, DIFFERENT amount $50.
    let c2 = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/connect/checkout"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({ "amount_cents": 5000, "currency": "usd" }))
        .to_request();
    let r2 = test::call_service(&svc, c2).await;
    assert_eq!(r2.status(), StatusCode::BAD_REQUEST);

    // Neither created a PaymentIntent → no stale replay is even possible.
    let pis = fx
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/payment_intents")
        .count();
    assert_eq!(pis, 0, "rejected empty-cart_id checkouts must not POST any PaymentIntent (M2)");

    // And an EXPLICIT cart_id still works AND folds amount into the idempotency
    // key — two carts with different amounts get DISTINCT idempotency keys.
    let c3 = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/connect/checkout"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({ "amount_cents": 20000, "currency": "usd", "cart_id": "cart-A" }))
        .to_request();
    assert_eq!(test::call_service(&svc, c3).await.status(), StatusCode::OK);
    let c4 = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/connect/checkout"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({ "amount_cents": 5000, "currency": "usd", "cart_id": "cart-A" }))
        .to_request();
    assert_eq!(test::call_service(&svc, c4).await.status(), StatusCode::OK);

    let keys: Vec<String> = fx
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.path == "/v1/payment_intents")
        .filter_map(|r| r.idempotency_key)
        .collect();
    assert_eq!(keys.len(), 2, "both explicit-cart checkouts POSTed a PI");
    assert_ne!(
        keys[0], keys[1],
        "same cart_id but DIFFERENT amount must yield distinct idempotency keys (M2)"
    );

    cleanup(&fx.state, &[creator], &[&pat]).await;
}

/// m2 (RED→GREEN): an operator cannot set floor_cents > cap_cents (it would pin
/// every fee to the cap regardless of percent). The handler rejects it 400.
#[compio::test]
async fn fee_policy_rejects_floor_above_cap() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "fee-floor-cap").await;
    let creator = make_user(&fx.state, "creator").await;
    let op = make_user(&fx.state, "operator").await;
    let op_pat = issue_pat(&fx.state, op, Some("billing"), billing_write_any()).await;

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/creators/{id}/fee-policy")
                .route(web::put().to(stripe_handlers::set_fee_policy)),
        ),
    )
    .await;

    let set = test::TestRequest::put()
        .uri(&format!("/api/creators/{creator}/fee-policy"))
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({
            "kind": "percent", "percent_bps": 1500, "floor_cents": 1000, "cap_cents": 100
        }))
        .to_request();
    let resp = test::call_service(&svc, set).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "floor_cents > cap_cents must be rejected (m2)"
    );
    let n = fx
        .state
        .control_pg
        .query("SELECT 1 FROM zeroship.creator_fee_policy WHERE creator_id = $1", &[&creator])
        .await
        .expect("query")
        .len();
    assert_eq!(n, 0, "the rejected floor>cap policy must not persist");

    cleanup(&fx.state, &[creator, op], &[&op_pat]).await;
}

/// m1 (RED→GREEN): a malformed currency is rejected with 400 before any Stripe
/// call.
#[compio::test]
async fn checkout_rejects_bad_currency() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "checkout-currency").await;
    let creator = make_user(&fx.state, "creator").await;
    let pat = issue_pat(&fx.state, creator, None, billing_write_self()).await;
    fx.mock.set_flags(true, true, true);

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::scope("/api/creators/{id}")
                .service(web::resource("/stripe/onboard").route(web::post().to(stripe_handlers::onboard)))
                .service(web::resource("/stripe/callback").route(web::post().to(stripe_handlers::callback)))
                .service(web::resource("/connect/checkout").route(web::post().to(stripe_handlers::connect_checkout))),
        ),
    )
    .await;

    let onboard = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/onboard"))
        .header("authorization", pat.bearer())
        .to_request();
    assert_eq!(test::call_service(&svc, onboard).await.status(), StatusCode::OK);
    let cb = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/stripe/callback"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({}))
        .to_request();
    assert_eq!(test::call_service(&svc, cb).await.status(), StatusCode::OK);

    let checkout = test::TestRequest::post()
        .uri(&format!("/api/creators/{creator}/connect/checkout"))
        .header("authorization", pat.bearer())
        .set_json(&serde_json::json!({ "amount_cents": 20000, "currency": "US Dollars", "cart_id": "cart-z" }))
        .to_request();
    let resp = test::call_service(&svc, checkout).await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a non ^[a-z]{{3}}$ currency must be rejected (m1)"
    );

    cleanup(&fx.state, &[creator], &[&pat]).await;
}

#[compio::test]
async fn fee_policy_set_is_operator_only() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "fee-authz").await;
    let creator = make_user(&fx.state, "creator").await;
    let op = make_user(&fx.state, "operator").await;
    // The creator holds a SELF-scoped BillingWrite (NOT Resource::Any).
    let creator_pat = issue_pat(&fx.state, creator, None, billing_write_self()).await;
    let op_pat = issue_pat(&fx.state, op, Some("billing"), billing_write_any()).await;

    let svc = test::init_service(
        web::App::new().state(fx.state.clone()).service(
            web::resource("/api/creators/{id}/fee-policy")
                .route(web::put().to(stripe_handlers::set_fee_policy)),
        ),
    )
    .await;

    // A creator trying to set THEIR OWN fee policy → 403 (privilege escalation).
    let creator_set = test::TestRequest::put()
        .uri(&format!("/api/creators/{creator}/fee-policy"))
        .header("authorization", creator_pat.bearer())
        .set_json(&serde_json::json!({ "kind": "percent", "percent_bps": 0 }))
        .to_request();
    let resp = test::call_service(&svc, creator_set).await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a creator must NOT set/lower their own fee policy (ISS-29)"
    );
    // No row was written.
    let n = fx
        .state
        .control_pg
        .query(
            "SELECT 1 FROM zeroship.creator_fee_policy WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .expect("query")
        .len();
    assert_eq!(n, 0, "the rejected creator write must not persist a policy");

    // An operator may set it.
    let op_set = test::TestRequest::put()
        .uri(&format!("/api/creators/{creator}/fee-policy"))
        .header("authorization", op_pat.bearer())
        .set_json(&serde_json::json!({ "kind": "percent", "percent_bps": 1000 }))
        .to_request();
    let resp = test::call_service(&svc, op_set).await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "operator may set the fee policy");

    cleanup(&fx.state, &[creator, op], &[&creator_pat, &op_pat]).await;
}
