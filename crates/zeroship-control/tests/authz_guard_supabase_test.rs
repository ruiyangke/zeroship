//! Regression coverage for AuthzGuard's Supabase GoTrue bearer branch.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use compio_postgres::{connect, NoTls};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_authz::{Action, Resource};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::{
    authz_guard::AuthzGuard, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};
use zeroship_core::auth_provider::{AuthProvider, SupabaseConfig, SupabaseProvider};
use zeroship_core::{AppId, UserId};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const SUPABASE_URL: &str = "https://project.supabase.test";
const SUPABASE_ISSUER: &str = "https://project.supabase.test/auth/v1";
const SUPABASE_ANON_KEY: &str = "test-anon-key";
const SUPABASE_JWT_SECRET: &str = "test-supabase-jwt-secret-at-least-32-bytes";

fn db_url() -> String {
    crate::common::require_control_db()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-authz-guard-supabase-{label}-{}",
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

fn gotrue_token(subject: &str, role: &str) -> String {
    gotrue_token_with(subject, role, SUPABASE_ISSUER, unix_now_secs() + 3600)
}

fn gotrue_token_with(subject: &str, role: &str, issuer: &str, exp: u64) -> String {
    let claims = json!({
        "iss": issuer,
        "sub": subject,
        "aud": "authenticated",
        "exp": exp,
        "iat": unix_now_secs(),
        "nbf": unix_now_secs().saturating_sub(1),
        "email": format!("{subject}@gotrue.test"),
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

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    users: Vec<UserId>,
    subjects: Vec<String>,
    apps: Vec<AppId>,
}

impl Fixture {
    /// A REFUSAL, not a skip, when the database will not take a connection.
    ///
    /// `db_url()` has already preflighted the DSN, so a failure here is the
    /// server declining THIS connection - most often the `max_connections`
    /// ceiling, with something in the process holding connections open across
    /// tests. Both arms used to announce a skip, which cargo counts as a pass,
    /// so a connection ceiling reached mid-run turned every remaining test in
    /// this module green without executing one of them.
    async fn new(label: &str) -> Self {
        let db_url = db_url();

        let (control_pg_client, control_pg_conn) = match connect(&db_url, NoTls).await {
            Ok(pg) => pg,
            Err(err) => common::refuse_missing_backend(
                "a connection to the control test database",
                &format!("the database preflighted clean but refused this connection: {err}"),
                "If the server is up, this is usually its connection ceiling; look\n\
                 \x20   for a test in this binary holding clients open across bodies\n\
                 \x20   (`common::drain_pg` is what waits for them to close). If it is\n\
                 \x20   down, bring the backends up and rewrite the overlay from them:\n\
                 \x20     tests/provision_test_backends.sh",
            ),
        };
        compio::runtime::spawn(async move {
            let _ = control_pg_conn.run().await;
        })
        .detach();

        let registry = match Registry::new(&db_url).await {
            Ok(registry) => registry,
            Err(err) => common::refuse_missing_backend(
                "a registry pool on the control test database",
                &format!("the registry could not open its pool: {err}"),
                "If the server is up, this is usually its connection ceiling; look\n\
                 \x20   for a test in this binary holding clients open across bodies\n\
                 \x20   (`common::drain_pg` is what waits for them to close). If it is\n\
                 \x20   down, bring the backends up and rewrite the overlay from them:\n\
                 \x20     tests/provision_test_backends.sh",
            ),
        };
        zeroship_control::plan_catalog::seed_plans(&registry)
            .await
            .expect("seed built-in plans");
        let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
        let stripe_store = StripeStore::new(registry.clone());
        let blob_root = tmpdir(&format!("blob-{label}"));
        let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
        let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
            zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                .expect("workflow blob store"),
        );
        let auth_provider = Arc::new(AuthProvider::supabase(SupabaseProvider::new(
            SupabaseConfig::new(
                SUPABASE_URL,
                SUPABASE_ANON_KEY,
                None,
                Some(SUPABASE_JWT_SECRET.to_string()),
                None,
                SUPABASE_ISSUER,
            )
            .expect("valid test Supabase config"),
        )));

        let state = Arc::new(AppState {
            service_auth: std::sync::Arc::new(
                zeroship_core::service_peers::ServiceAuth::unconfigured(),
            ),
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
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
            worker_enrolment: zeroship_control::worker_enrolment::EnrolmentEnvelope::closed(),
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
            notifier: Arc::new(zeroship_control::notify::RecordingNotifier::new()),
            mailer: std::sync::Arc::new(zeroship_mailer::RecordingMailer::new()),
            pairwise_salt: [0u8; 32],
            projected_charge_cache: Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        });

        Self {
            state,
            blob_root,
            deploy_tmp_dir,
            users: Vec::new(),
            subjects: Vec::new(),
            apps: Vec::new(),
        }
    }

    async fn seed_linked_principal(&mut self, subject: &str, grants: &[&str]) -> UserId {
        let principal_id = UserId::mint();
        let email = format!("supabase-{}@zeroship.test", principal_id.as_str());
        self.state
            .control_pg
            .execute(
                "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
                 VALUES ($1, $2::citext, 'Supabase Test User', NOW())",
                &[&principal_id.as_str(), &email],
            )
            .await
            .expect("insert supabase test user");
        self.state
            .control_pg
            .execute(
                "INSERT INTO zeroship.identity_links \
                    (principal_id, provider, provider_subject, email) \
                 VALUES ($1, 'supabase', $2, $3)",
                &[&principal_id.as_str(), &subject, &email],
            )
            .await
            .expect("insert supabase identity link");
        for grant in grants {
            self.state
                .control_pg
                .execute(
                    "INSERT INTO zeroship.principal_grants (principal_id, grant_name) \
                     VALUES ($1, $2)",
                    &[&principal_id.as_str(), grant],
                )
                .await
                .expect("insert principal grant");
        }
        self.users.push(principal_id.clone());
        self.subjects.push(subject.to_string());
        principal_id
    }

    async fn create_owned_app(&mut self, owner_id: &UserId, label: &str) -> AppId {
        let record = self
            .state
            .registry
            .create_app(
                &format!("{label}-{}", Uuid::new_v4().simple()),
                &zeroship_control::plan_catalog::free_plan_id(),
                owner_id,
                None,
                None,
            )
            .await
            .expect("create owned app");
        self.apps.push(record.id.clone());
        record.id
    }

    async fn cleanup(&self) {
        for user_id in &self.users {
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                    &[&user_id.as_str()],
                )
                .await;
        }
        for app_id in &self.apps {
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.organization_members om \
                 USING zeroship.apps a JOIN zeroship.projects p ON p.id = a.project_id \
                 WHERE om.organization_id = p.organization_id AND a.id = $1",
                    &[&app_id.as_str()],
                )
                .await;
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.apps WHERE id = $1",
                    &[&app_id.as_str()],
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
                    &[&user_id.as_str()],
                )
                .await;
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.users WHERE id = $1",
                    &[&user_id.as_str()],
                )
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

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(web::App::new().state($fx.state.clone()).service(
            web::resource("/raw-app/{id}/deploy-check").route(web::post().to(raw_app_deploy_check)),
        ))
        .await
    }};
}

async fn raw_app_deploy_check(
    path: web::types::Path<String>,
    authz: AuthzGuard,
    state: web::types::State<Arc<AppState>>,
) -> web::HttpResponse {
    let Ok(id) = AppId::parse(&path.into_inner()) else {
        return web::HttpResponse::BadRequest().finish();
    };
    match authz
        .require(
            Action::AppsDeploy,
            Resource::App { id },
            &state,
        )
        .await
    {
        Ok(()) => web::HttpResponse::Ok().json(&json!({
            "principal_id": authz.principal_id.as_str(),
        })),
        Err(resp) => resp,
    }
}

#[compio::test]
async fn gotrue_authenticated_token_resolves_linked_principal_and_deploy_grant() {
    let mut fx = Fixture::new("positive").await;
    let subject = Uuid::new_v4().to_string();
    let principal_id = fx
        .seed_linked_principal(&subject, &["apps:deploy", "apps:read"])
        .await;
    let app_id = fx
        .create_owned_app(&principal_id, "supabase-positive")
        .await;
    let app = init_control!(fx);
    let token = gotrue_token(&subject, "authenticated");

    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{}/deploy-check", app_id.as_str()))
        .header("authorization", bearer(&token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(&test::read_body(resp).await).expect("body json");
    assert_eq!(body["principal_id"], principal_id.as_str());

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
async fn gotrue_token_linked_to_anonymized_user_returns_401() {
    let mut fx = Fixture::new("anonymized-owner").await;
    let subject = Uuid::new_v4().to_string();
    let principal_id = fx.seed_linked_principal(&subject, &["apps:deploy"]).await;
    let app_id = fx
        .create_owned_app(&principal_id, "supabase-anonymized")
        .await;
    let app = init_control!(fx);
    let token = gotrue_token(&subject, "authenticated");

    fx.state
        .control_pg
        .execute(
            "UPDATE zeroship.users \
             SET disabled_at = NOW(), anonymized_at = NOW(), \
                 credential_version = credential_version + 1 \
             WHERE id = $1",
            &[&principal_id.as_str()],
        )
        .await
        .expect("anonymize GoTrue principal");
    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{}/deploy-check", app_id.as_str()))
        .header("authorization", bearer(&token))
        .to_request();
    let status = test::call_service(&app, req).await.status();

    fx.cleanup().await;
    drop(app);
    drop(fx);
    common::drain_pg().await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[compio::test]
async fn gotrue_unlinked_subject_is_unauthorized() {
    let fx = Fixture::new("unlinked").await;
    let app = init_control!(fx);
    let token = gotrue_token(&Uuid::new_v4().to_string(), "authenticated");

    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{}/deploy-check", Uuid::new_v4()))
        .header("authorization", bearer(&token))
        .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown below.
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn gotrue_non_authenticated_role_is_unauthorized() {
    let mut fx = Fixture::new("role").await;
    let subject = Uuid::new_v4().to_string();
    fx.seed_linked_principal(&subject, &["apps:deploy"]).await;
    let app = init_control!(fx);
    let token = gotrue_token(&subject, "service_role");

    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{}/deploy-check", Uuid::new_v4()))
        .header("authorization", bearer(&token))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn gotrue_principal_without_deploy_grant_is_forbidden() {
    let mut fx = Fixture::new("no-deploy").await;
    let subject = Uuid::new_v4().to_string();
    let principal_id = fx.seed_linked_principal(&subject, &["apps:read"]).await;
    let app_id = fx
        .create_owned_app(&principal_id, "supabase-no-deploy")
        .await;
    let app = init_control!(fx);
    let token = gotrue_token(&subject, "authenticated");

    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{}/deploy-check", app_id.as_str()))
        .header("authorization", bearer(&token))
        .to_request();
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, StatusCode::FORBIDDEN);

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn gotrue_expired_or_wrong_issuer_token_is_unauthorized() {
    let mut fx = Fixture::new("verify-rejects").await;
    let subject = Uuid::new_v4().to_string();
    fx.seed_linked_principal(&subject, &["apps:deploy"]).await;
    let app = init_control!(fx);

    for token in [
        gotrue_token_with(
            &subject,
            "authenticated",
            SUPABASE_ISSUER,
            unix_now_secs().saturating_sub(60),
        ),
        gotrue_token_with(
            &subject,
            "authenticated",
            "https://wrong-issuer.example/auth/v1",
            unix_now_secs() + 3600,
        ),
    ] {
        let req = test::TestRequest::post()
            .uri(&format!("/raw-app/{}/deploy-check", Uuid::new_v4()))
            .header("authorization", bearer(&token))
            .to_request();
        let status = test::call_service(&app, req).await.status();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    fx.cleanup().await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}
