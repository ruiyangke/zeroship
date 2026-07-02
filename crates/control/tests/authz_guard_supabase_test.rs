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
    authz_guard::AuthzGuard, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
};
use zeroship_core::auth_provider::{
    AuthProvider, SupabaseConfig, SupabaseProvider,
};

#[allow(dead_code)]
mod common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const SUPABASE_URL: &str = "https://project.supabase.test";
const SUPABASE_ISSUER: &str = "https://project.supabase.test/auth/v1";
const SUPABASE_ANON_KEY: &str = "test-anon-key";
const SUPABASE_JWT_SECRET: &str = "test-supabase-jwt-secret-at-least-32-bytes";

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("AUTH_DB_URL"))
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
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
    users: Vec<Uuid>,
    subjects: Vec<String>,
    apps: Vec<Uuid>,
}

impl Fixture {
    async fn new(label: &str) -> Option<Self> {
        let Some(db_url) = db_url() else {
            eprintln!(
                "[authz_guard_supabase_test] CONTROL_TEST_DB/AUTH_DB_URL/PG_TEST_URL not set - skipping"
            );
            return None;
        };

        let (control_pg_client, control_pg_conn) = match connect(&db_url, NoTls).await {
            Ok(pg) => pg,
            Err(err) => {
                eprintln!(
                    "[authz_guard_supabase_test] test DB unreachable ({err}) - skipping"
                );
                return None;
            }
        };
        compio::runtime::spawn(async move {
            let _ = control_pg_conn.run().await;
        })
        .detach();

        let registry = match Registry::new(&db_url).await {
            Ok(registry) => registry,
            Err(err) => {
                eprintln!(
                    "[authz_guard_supabase_test] registry DB connect failed ({err}) - skipping"
                );
                return None;
            }
        };
        zeroship_control::bootstrap_console::seed_plans(&registry)
            .await
            .expect("seed built-in plans");
        let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false)
            .expect("env store");
        let stripe_store = StripeStore::new(registry.clone());
        let blob_root = tmpdir(&format!("blob-{label}"));
        let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
        let auth_provider = Arc::new(AuthProvider::Supabase(SupabaseProvider::new(
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
            insecure_dev: false,
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

        Some(Self {
            state,
            blob_root,
            deploy_tmp_dir,
            users: Vec::new(),
            subjects: Vec::new(),
            apps: Vec::new(),
        })
    }

    async fn seed_linked_principal(&mut self, subject: &str, grants: &[&str]) -> Uuid {
        let principal_id = Uuid::new_v4();
        let email = format!("supabase-{principal_id}@zeroship.test");
        self.state
            .control_pg
            .execute(
                "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
                 VALUES ($1, $2::citext, 'Supabase Test User', NOW())",
                &[&principal_id, &email],
            )
            .await
            .expect("insert supabase test user");
        self.state
            .control_pg
            .execute(
                "INSERT INTO zeroship.identity_links \
                    (principal_id, provider, provider_subject, email) \
                 VALUES ($1, 'supabase', $2, $3)",
                &[&principal_id, &subject, &email],
            )
            .await
            .expect("insert supabase identity link");
        for grant in grants {
            self.state
                .control_pg
                .execute(
                    "INSERT INTO zeroship.principal_grants (principal_id, grant_name) \
                     VALUES ($1, $2)",
                    &[&principal_id, grant],
                )
                .await
                .expect("insert principal grant");
        }
        self.users.push(principal_id);
        self.subjects.push(subject.to_string());
        principal_id
    }

    async fn create_owned_app(&mut self, owner_id: Uuid, label: &str) -> Uuid {
        let record = self
            .state
            .registry
            .create_app(
                &format!("{label}-{}", Uuid::new_v4().simple()),
                &zeroship_control::bootstrap_console::free_plan_id(),
                &owner_id,
            )
            .await
            .expect("create owned app");
        self.apps.push(record.id);
        record.id
    }

    async fn cleanup(&self) {
        for user_id in &self.users {
            let _ = self
                .state
                .control_pg
                .execute(
                    "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                    &[user_id],
                )
                .await;
        }
        for app_id in &self.apps {
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

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(web::App::new().state($fx.state.clone()).service(
            web::resource("/raw-app/{id}/deploy-check")
                .route(web::post().to(raw_app_deploy_check)),
        ))
        .await
    }};
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

#[compio::test]
async fn gotrue_authenticated_token_resolves_linked_principal_and_deploy_grant() {
    let Some(mut fx) = Fixture::new("positive").await else {
        return;
    };
    let subject = Uuid::new_v4().to_string();
    let principal_id = fx
        .seed_linked_principal(&subject, &["apps:deploy", "apps:read"])
        .await;
    let app_id = fx.create_owned_app(principal_id, "supabase-positive").await;
    let app = init_control!(fx);
    let token = gotrue_token(&subject, "authenticated");

    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{app_id}/deploy-check"))
        .header("authorization", bearer(&token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("body json");
    assert_eq!(body["principal_id"], principal_id.to_string());

    fx.cleanup().await;
}

#[compio::test]
async fn gotrue_unlinked_subject_is_unauthorized() {
    let Some(fx) = Fixture::new("unlinked").await else {
        return;
    };
    let app = init_control!(fx);
    let token = gotrue_token(&Uuid::new_v4().to_string(), "authenticated");

    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{}/deploy-check", Uuid::new_v4()))
        .header("authorization", bearer(&token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    fx.cleanup().await;
}

#[compio::test]
async fn gotrue_non_authenticated_role_is_unauthorized() {
    let Some(mut fx) = Fixture::new("role").await else {
        return;
    };
    let subject = Uuid::new_v4().to_string();
    fx.seed_linked_principal(&subject, &["apps:deploy"]).await;
    let app = init_control!(fx);
    let token = gotrue_token(&subject, "service_role");

    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{}/deploy-check", Uuid::new_v4()))
        .header("authorization", bearer(&token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    fx.cleanup().await;
}

#[compio::test]
async fn gotrue_principal_without_deploy_grant_is_forbidden() {
    let Some(mut fx) = Fixture::new("no-deploy").await else {
        return;
    };
    let subject = Uuid::new_v4().to_string();
    let principal_id = fx.seed_linked_principal(&subject, &["apps:read"]).await;
    let app_id = fx.create_owned_app(principal_id, "supabase-no-deploy").await;
    let app = init_control!(fx);
    let token = gotrue_token(&subject, "authenticated");

    let req = test::TestRequest::post()
        .uri(&format!("/raw-app/{app_id}/deploy-check"))
        .header("authorization", bearer(&token))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    fx.cleanup().await;
}

#[compio::test]
async fn gotrue_expired_or_wrong_issuer_token_is_unauthorized() {
    let Some(mut fx) = Fixture::new("verify-rejects").await else {
        return;
    };
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
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    fx.cleanup().await;
}
