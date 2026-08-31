//! Coverage for control's RFC 9728 protected-resource metadata.
//!
//! This file used to cover control's own RFC 8628 device flow end to end. That
//! flow is gone: `zeroship login` drives the OP's device grant, and control's
//! `/api/device/{auth,approve,token}` had no caller left. What survives is the
//! one thing control still serves an unauthenticated CLI - the document naming
//! the authorization server whose tokens control accepts.
//!
//! This test intentionally hard-fails without a test database: `AppState`
//! owns a live `Registry`, and a fixture that silently skipped would report
//! a green that measured nothing.

use std::path::PathBuf;
use std::sync::Arc;

use compio_postgres::{connect, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    device_handlers, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::auth_provider::{
    AuthProvider, ConfiguredProvider, PlatformConfig, PlatformProvider, SupabaseConfig,
    SupabaseProvider,
};
use zeroship_core::config::OriginScheme;
use zeroship_core::device_grant;

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const TEST_CONTROL_KEY: &str = "test-control-key";
const SUPABASE_ANON_KEY: &str = "test-anon-key";
const SUPABASE_JWT_SECRET: &str = "test-supabase-jwt-secret-at-least-32-bytes";

/// The issuer a platform fixture advertises.
///
/// A literal, not a listener: both tests here read the CONFIGURED string back
/// out of the document and never send a request to it, so standing a mock OP
/// up would only add a port to the test's failure modes.
const PLATFORM_ISSUER: &str = "https://auth.zeroship.test/oauth2";
const SUPABASE_URL: &str = "https://project.supabase.test";
const SUPABASE_ISSUER: &str = "https://project.supabase.test/auth/v1";

fn db_url() -> String {
    zeroship_core::config::test_database_url_opt()
        .expect("a test database must be configured (zeroship_core::config::test_database_url_opt) so device_handlers_test runs against Postgres")
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-device-handlers-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("create test dir");
    path
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

/// Which providers the fixture trusts. The only axis these two tests move.
#[derive(Clone, Copy)]
enum FixtureProvider {
    Platform,
    /// Supabase with no platform OP configured - the one state in which there
    /// is legitimately no authorization server to advertise.
    SupabaseOnly,
}

impl Fixture {
    async fn new(provider: FixtureProvider) -> Self {
        let db_url = db_url();
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
        let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
        let stripe_store = StripeStore::new(registry.clone());
        let blob_root = tmpdir("blob");
        let deploy_tmp_dir = tmpdir("deploy");
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
        let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
            zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                .expect("workflow blob store"),
        );

        let auth_provider = Arc::new(match provider {
            FixtureProvider::Platform => AuthProvider::platform(PlatformProvider::new(
                PlatformConfig::new(PLATFORM_ISSUER, None).expect("valid platform config"),
            )),
            FixtureProvider::SupabaseOnly => AuthProvider::new(vec![ConfiguredProvider::Supabase(
                SupabaseProvider::new(
                    SupabaseConfig::new(
                        SUPABASE_URL,
                        SUPABASE_ANON_KEY,
                        None,
                        Some(SUPABASE_JWT_SECRET.to_string()),
                        None,
                        SUPABASE_ISSUER,
                    )
                    .expect("valid test Supabase config"),
                ),
            )])
            .expect("single supabase fixture issuer"),
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
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: OriginScheme::Https,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg: Arc::new(control_pg_client),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
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
            blob_root,
            deploy_tmp_dir,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

/// `zeroship login` holds ONE configured URL - control's - and learns the OP
/// from this document. The value it must carry is therefore not "an issuer"
/// but THE issuer `zeroship_authn::BearerVerifier` pins `iss` to, because a
/// token minted anywhere else is a token control refuses.
#[compio::test]
async fn protected_resource_metadata_names_the_issuer_control_verifies_against() {
    let fx = Fixture::new(FixtureProvider::Platform).await;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(device_handlers::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(device_grant::PROTECTED_RESOURCE_METADATA_PATH)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("metadata body json");

    let configured = fx
        .state
        .auth_provider
        .platform_issuer()
        .expect("platform fixture configures an issuer")
        .to_string();
    assert_eq!(
        body["authorization_servers"],
        json!([configured]),
        "the advertised OP must be the one whose tokens this control accepts"
    );
    assert_eq!(body["resource"], fx.state.expected_oauth_audience.as_str());
    // The CLI appends the OP's protocol paths to this string, so a value that
    // is not the OP's protocol root would send the device request somewhere
    // that cannot serve it.
    assert!(
        configured.ends_with(device_grant::OP_PATH_PREFIX),
        "advertised issuer is not an OP protocol root: {configured}"
    );

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

/// The one-variable control for the case above: same request, same route, and
/// the single difference is whether a platform OP is configured at all. A
/// deployment with none has no authorization server to advertise, and naming
/// one anyway would send the CLI somewhere control would refuse tokens from.
#[compio::test]
async fn protected_resource_metadata_is_absent_without_a_platform_op() {
    let fx = Fixture::new(FixtureProvider::SupabaseOnly).await;

    let app = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(device_handlers::configure),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(device_grant::PROTECTED_RESOURCE_METADATA_PATH)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    drop(app);
    drop(fx);
    common::drain_pg().await;
}
