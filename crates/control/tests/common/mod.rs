pub mod authz_fixture;
pub mod stripe_mock;

use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::sync::OnceLock;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use ed25519_dalek::SigningKey;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ntex::web::{self, HttpResponse};
use serde_json::json;
use uuid::Uuid;
use zeroship_core::auth_provider::{AuthProvider, PlatformConfig, PlatformProvider};
use zeroship_control::Registry;

pub const PLATFORM_ISSUER: &str = "https://auth.zeroship.test/oauth2";
const PLATFORM_KID: &str = "platform-control-test-kid";
const PLATFORM_KEY_SEED: u8 = 47;

pub struct PlatformJwks {
    base: String,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PlatformJwks {
    pub fn start() -> Self {
        let body = Arc::new(RwLock::new(platform_jwks_body()));
        let factory_body = body.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("control-common-platform-jwks")
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

    pub fn jwks_url(&self) -> String {
        format!("{}/.well-known/jwks.json", self.base)
    }
}

impl Drop for PlatformJwks {
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

pub fn platform_auth_provider(jwks_url: String) -> Arc<AuthProvider> {
    Arc::new(AuthProvider::Platform(PlatformProvider::new(
        PlatformConfig::new(PLATFORM_ISSUER, Some(jwks_url)).expect("platform config"),
    )))
}

pub fn platform_token(subject: Uuid, scope: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let claims = json!({
        "iss": PLATFORM_ISSUER,
        "sub": subject.to_string(),
        "aud": "control.zeroship.ai",
        "exp": now + 3600,
        "iat": now,
        "nbf": now.saturating_sub(1),
        "jti": Uuid::new_v4().to_string(),
        "client_id": "zeroship-cli",
        "scope": scope,
    });
    let mut header = Header::new(Algorithm::EdDSA);
    header.typ = Some("at+jwt".to_string());
    header.kid = Some(PLATFORM_KID.to_string());
    encode(&header, &claims, &platform_encoding_key()).expect("platform token")
}

pub fn platform_bearer(subject: Uuid, scope: &str) -> String {
    format!("Bearer {}", platform_token(subject, scope))
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

/// Idempotently seed the built-in plan tiers (free/pro/unlimited) into the test
/// DB's plan catalog so `create_app`/`set_plan` (which validate `plan_id`
/// against `zeroship.plans` since PR4) accept the built-in ids. A no-op on a
/// re-run (ON CONFLICT DO UPDATE on deterministic `pln_…` ids). Tests that
/// create apps with [`zeroship_control::bootstrap_console::free_plan_id`] call
/// this in their setup first.
#[allow(dead_code)]
pub async fn ensure_builtin_plans(registry: &Registry) {
    zeroship_control::bootstrap_console::seed_plans(registry)
        .await
        .expect("seed built-in plans for test");
}

#[allow(dead_code)]
pub async fn seed_usage_total(
    pg: &compio_postgres::Client,
    app: Uuid,
    period_start_unix: i64,
    metric: &str,
    total: i64,
) {
    seed_metric_catalog(pg, metric).await;
    pg.execute(
        "INSERT INTO zeroship.usage_aggregates AS u (app_id, period, metric, total, updated_at) \
         VALUES ($1, $2::date, $3, $4, NOW()) \
         ON CONFLICT (app_id, period, metric) DO UPDATE SET \
           total = EXCLUDED.total, updated_at = NOW()",
        &[&app, &period_date(period_start_unix), &metric, &total],
    )
    .await
    .expect("seed usage total");
}

#[allow(dead_code)]
pub async fn seed_usage_delta(
    pg: &compio_postgres::Client,
    app: Uuid,
    period_start_unix: i64,
    metric: &str,
    delta: i64,
) {
    seed_metric_catalog(pg, metric).await;
    pg.execute(
        "INSERT INTO zeroship.usage_aggregates AS u (app_id, period, metric, total, updated_at) \
         VALUES ($1, $2::date, $3, $4, NOW()) \
         ON CONFLICT (app_id, period, metric) DO UPDATE SET \
           total = u.total + EXCLUDED.total, updated_at = NOW()",
        &[&app, &period_date(period_start_unix), &metric, &delta],
    )
    .await
    .expect("seed usage delta");
}

#[allow(dead_code)]
pub async fn seed_metric_catalog(pg: &compio_postgres::Client, metric: &str) {
    pg.execute(
        "INSERT INTO zeroship.billing_metrics (metric, kind, unit) \
         VALUES ($1, 'platform', 'op') \
         ON CONFLICT (metric) DO UPDATE SET unit = EXCLUDED.unit",
        &[&metric],
    )
    .await
    .expect("seed billing metric");
}

#[allow(dead_code)]
pub fn period_date(period_start_unix: i64) -> chrono::NaiveDate {
    use chrono::{Datelike, TimeZone};
    let dt = chrono::Utc
        .timestamp_opt(period_start_unix, 0)
        .single()
        .unwrap_or_else(chrono::Utc::now);
    chrono::NaiveDate::from_ymd_opt(dt.year(), dt.month(), 1)
        .expect("valid first-of-month period")
}

#[allow(dead_code)]
pub fn isolated_closed_period_now() -> i64 {
    use chrono::TimeZone;

    static NOW: OnceLock<i64> = OnceLock::new();
    *NOW.get_or_init(|| {
        let offset = (Uuid::new_v4().as_u128() % 2400) as i32;
        let year = 2030 + offset / 12;
        let month = (offset % 12) as u32 + 1;
        chrono::Utc
            .with_ymd_and_hms(year, month, 15, 12, 0, 0)
            .single()
            .expect("valid isolated billing period")
            .timestamp()
    })
}

#[allow(dead_code)]
pub fn lite_billing_stack(
    registry: Registry,
    stripe_base_url: String,
    tax_provider: Arc<dyn zeroship_control::tax::TaxProvider>,
) -> Arc<zeroship_control::metering::provider::BillingStack> {
    use zeroship_control::metering::provider::{
        BillingStack, ControlLiteStore, LiteStore, ProviderCtx, StaticSecretResolver,
    };
    use zeroship_control::{SecretString, StripeStore};

    let store: Arc<dyn LiteStore> = Arc::new(ControlLiteStore::new(
        registry.clone(),
        StripeStore::new(registry.clone()),
        SecretString::new("sk_test_mock".to_string()),
        stripe_base_url,
        tax_provider,
    ));
    let ctx = ProviderCtx::new(
        serde_json::json!({}),
        Arc::new(StaticSecretResolver::default()),
        Some(store),
    );
    let provider = zeroship_control::metering::provider::builtin_registry()
        .build("lite", &ctx)
        .expect("test lite billing provider builds");
    Arc::new(BillingStack {
        meter: Arc::clone(&provider),
        invoicer: provider,
    })
}
