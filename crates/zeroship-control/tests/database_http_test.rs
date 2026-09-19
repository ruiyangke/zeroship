//! The database routes through ntex, with the real `AuthzGuard`.
//!
//! The operation-level suite is `database_surface_test`; this one exists for
//! the half that suite cannot see. Two things are only true once a request goes
//! through the stack:
//!
//! - **`zeroship_control::databases::configure` is mounted.** A module whose
//!   routes nothing registers is unreachable however correct its statements
//!   are, and the operation suite would still be green.
//! - **`Resource::Database` RESOLVES.** `zeroship_authz::authority::resolve`
//!   turns a `dbs_` id into the owning project by reading
//!   `zeroship.databases.project_id`; before this surface existed nothing
//!   constructed that variant, so nothing exercised that arm. A resolve that
//!   returned no project would deny EVERY caller, including the owner - which
//!   is why the owner arm below is the real assertion and the stranger arm is
//!   its control. Either alone is satisfied by a broken resolve: a
//!   deny-everything gate passes the stranger arm, and a permit-everything gate
//!   passes the owner arm.

use std::path::PathBuf;
use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use ntex::http::StatusCode;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::organizations::{self, CreateOrganizationBody, CreateProjectBody};
use zeroship_control::{
    databases, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::{AppId, UserId};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("zship-dbhttp-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn build_test_state(db_url: &str, label: &str) -> Fixture {
    let (control_pg_client, control_pg_conn) =
        connect(db_url, NoTls).await.expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();

    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));

    let state = Arc::new(AppState {
        service_auth: Arc::new(zeroship_core::service_peers::ServiceAuth::unconfigured()),
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
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        origin_scheme: zeroship_core::config::OriginScheme::Https,
        trust_proxy: false,
        worker_enrolment: zeroship_control::worker_join::EnrolmentEnvelope::closed(),
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg: Arc::new(control_pg_client),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        auth_provider: zeroship_control::platform_auth_provider(
            common::PLATFORM_ISSUER,
            Some(common::platform_jwks_url()),
        ),
        provider_registry: zeroship_control::metering::provider::builtin_registry(),
        billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
        billing_stream: None,
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        mailer: Arc::new(zeroship_mailer::RecordingMailer::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    });

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
    }
}

async fn seed_user(pg: &Client, label: &str) -> UserId {
    let id = UserId::mint();
    let email = format!("{label}-{}@zeroship.test", id.as_str());
    pg.execute(
        "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
         VALUES ($1, $2::citext, $3, NOW())",
        &[&id.as_str(), &email, &label],
    )
    .await
    .expect("insert user");
    id
}

/// Every scope the vocabulary can express, so the wrapper policy narrows
/// nothing and the Cedar decision is the only thing that can refuse.
fn all_scopes() -> String {
    zeroship_authz::Scope::ALL
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn bearer(user: &UserId) -> String {
    format!(
        "Bearer {}",
        common::platform_token_for_client(user, &all_scopes(), common::CONSOLE_CLIENT_ID)
    )
}

/// The whole surface through the router, owner against stranger on every route.
///
/// One test rather than six, because the fixture is a full `AppState` and the
/// question each route answers is the same one. What varies between the two
/// callers is ONE thing: a seat in the organization behind the project.
#[compio::test]
async fn the_database_routes_are_mounted_and_gate_on_the_database_resource() {
    let url = common::require_control_db();
    let fx = build_test_state(&url, "routes").await;
    let pg = fx.state.control_pg.clone();
    common::ensure_builtin_plans(&fx.state.registry).await;

    let owner = seed_user(&pg, "db-owner").await;
    let stranger = seed_user(&pg, "db-stranger").await;
    let organization = organizations::create_organization(
        &fx.state.registry,
        &owner,
        &CreateOrganizationBody {
            name: format!("routes {}", Uuid::new_v4().simple()),
            slug: Some(format!("routes-{}", Uuid::new_v4().simple())),
            billing_email: None,
        },
        None,
    )
    .await
    .expect("create organization");
    let project = organizations::create_project(
        &fx.state.registry,
        &owner,
        &organization.id,
        &CreateProjectBody {
            name: "routes".to_string(),
            slug: Some(format!("routes-{}", Uuid::new_v4().simple())),
        },
        None,
    )
    .await
    .expect("create project");

    // The operator's side of the world: a cluster registered in the project's
    // zone. Control never inserts one.
    let datastore_id = zeroship_core::typed_id::generate("dst");
    let system_identifier =
        i64::from(u32::from_le_bytes(Uuid::new_v4().as_bytes()[..4].try_into().expect("four bytes")));
    pg.execute(
        "INSERT INTO zeroship.datastores (id, system_identifier, execution_zone_id, status) \
         SELECT $1, $2, p.execution_zone_id, 'active' \
           FROM zeroship.projects p WHERE p.id = $3",
        &[&datastore_id, &system_identifier, &project.id],
    )
    .await
    .expect("register a fixture datastore in the project's zone");

    let app_id = AppId::mint();
    pg.execute(
        "INSERT INTO zeroship.apps \
             (id, name, plan_id, project_id, organization_id, execution_zone_id) \
         SELECT $1, $2, $3, p.id, p.organization_id, p.execution_zone_id \
           FROM zeroship.projects p WHERE p.id = $4",
        &[
            &app_id.as_str(),
            &format!("routes-{}", Uuid::new_v4().simple()),
            &zeroship_control::plan_catalog::free_plan_id(),
            &project.id,
        ],
    )
    .await
    .expect("seed app in the project");

    let service = test::init_service(
        web::App::new()
            .state(fx.state.clone())
            .configure(databases::configure),
    )
    .await;

    // 1. CREATE, named at the project.
    let created = test::call_service(
        &service,
        test::TestRequest::post()
            .uri(&format!("/api/projects/{}/databases", project.id))
            .header("authorization", bearer(&owner))
            .set_json(&serde_json::json!({ "name": "main" }))
            .to_request(),
    )
    .await;
    let create_status = created.status();
    let create_body: serde_json::Value =
        serde_json::from_slice(&test::read_body(created).await).expect("the create body is JSON");
    let database_id = create_body["id"].as_str().unwrap_or_default().to_owned();

    let create_refused = test::call_service(
        &service,
        test::TestRequest::post()
            .uri(&format!("/api/projects/{}/databases", project.id))
            .header("authorization", bearer(&stranger))
            .set_json(&serde_json::json!({ "name": "intruder" }))
            .to_request(),
    )
    .await
    .status();

    // 2. BIND, named at the DATABASE. This is the first request in the tree
    //    whose Cedar resource is `Resource::Database`.
    let bound = test::call_service(
        &service,
        test::TestRequest::post()
            .uri(&format!("/api/databases/{database_id}/bindings"))
            .header("authorization", bearer(&owner))
            .set_json(&serde_json::json!({
                "app_id": app_id.as_str(),
                "capability": "readonly",
            }))
            .to_request(),
    )
    .await
    .status();
    let bind_refused = test::call_service(
        &service,
        test::TestRequest::post()
            .uri(&format!("/api/databases/{database_id}/bindings"))
            .header("authorization", bearer(&stranger))
            .set_json(&serde_json::json!({
                "app_id": app_id.as_str(),
                "capability": "readwrite",
            }))
            .to_request(),
    )
    .await
    .status();

    // 3. LIST the bindings, also at the database.
    let listed = test::call_service(
        &service,
        test::TestRequest::get()
            .uri(&format!("/api/databases/{database_id}/bindings"))
            .header("authorization", bearer(&owner))
            .to_request(),
    )
    .await;
    let list_status = listed.status();
    let list_body: serde_json::Value =
        serde_json::from_slice(&test::read_body(listed).await).expect("the listing body is JSON");
    let list_refused = test::call_service(
        &service,
        test::TestRequest::get()
            .uri(&format!("/api/databases/{database_id}/bindings"))
            .header("authorization", bearer(&stranger))
            .to_request(),
    )
    .await
    .status();

    // 4. DELETE while bound: the refusal a creator can act on, through HTTP.
    let delete_while_bound = test::call_service(
        &service,
        test::TestRequest::delete()
            .uri(&format!("/api/databases/{database_id}"))
            .header("authorization", bearer(&owner))
            .to_request(),
    )
    .await;
    let bound_status = delete_while_bound.status();
    let bound_body: serde_json::Value = serde_json::from_slice(&test::read_body(delete_while_bound).await)
        .expect("the refusal body is JSON");

    // 5. UNBIND, then DELETE.
    let unbound = test::call_service(
        &service,
        test::TestRequest::delete()
            .uri(&format!(
                "/api/databases/{database_id}/bindings/{}",
                app_id.as_str()
            ))
            .header("authorization", bearer(&owner))
            .to_request(),
    )
    .await
    .status();
    let deleted = test::call_service(
        &service,
        test::TestRequest::delete()
            .uri(&format!("/api/databases/{database_id}"))
            .header("authorization", bearer(&owner))
            .to_request(),
    )
    .await
    .status();

    // Teardown before the assertions, so a failing expectation still leaves the
    // shared database as it found it.
    for statement in [
        "DELETE FROM zeroship.database_bindings WHERE project_id = $1",
        "DELETE FROM zeroship.databases WHERE project_id = $1",
        "DELETE FROM zeroship.apps WHERE project_id = $1",
    ] {
        let _ = pg.execute(statement, &[&project.id]).await;
    }
    let _ = pg
        .execute(
            "DELETE FROM zeroship.datastores WHERE id = $1",
            &[&datastore_id],
        )
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.projects WHERE id = $1", &[&project.id])
        .await;
    let _ = pg
        .execute(
            "DELETE FROM zeroship.app_audit WHERE resource = $1",
            &[&organization.id],
        )
        .await;
    let _ = pg
        .execute(
            "DELETE FROM zeroship.organizations WHERE id = $1",
            &[&organization.id],
        )
        .await;
    for user in [&owner, &stranger] {
        let _ = pg
            .execute(
                "DELETE FROM zeroship.authz_decisions WHERE actor_user_id = $1",
                &[&user.as_str()],
            )
            .await;
        let _ = pg
            .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.as_str()])
            .await;
    }
    drop(service);
    drop(pg);
    drop(fx);
    common::drain_pg().await;

    assert_eq!(
        create_status,
        StatusCode::CREATED,
        "the project-scoped create route must be mounted and admit its owner"
    );
    assert!(
        database_id.starts_with("dbs_"),
        "the create response must carry the minted database id, got {create_body}"
    );
    assert_eq!(
        create_refused,
        StatusCode::FORBIDDEN,
        "a stranger must not create a database in someone else's project"
    );

    assert_eq!(
        bound,
        StatusCode::CREATED,
        "the owner must reach a Database-typed resource; a resolve that found no \
         project would deny here too"
    );
    assert_eq!(
        bind_refused,
        StatusCode::FORBIDDEN,
        "a stranger must not bind an app to someone else's database"
    );

    assert_eq!(list_status, StatusCode::OK);
    assert_eq!(
        list_body["bindings"][0]["capability"], "readonly",
        "the listing must carry the capability each binding holds, got {list_body}"
    );
    assert_eq!(
        list_refused,
        StatusCode::FORBIDDEN,
        "a stranger must not read someone else's bindings"
    );

    assert_eq!(
        bound_status,
        StatusCode::CONFLICT,
        "deleting a bound database is a conflict, not a server error"
    );
    assert_eq!(bound_body["error"], "database has bindings");
    assert_eq!(
        bound_body["bindings"][0]["app_id"], app_id.as_str(),
        "the refusal body must name the app to unbind, got {bound_body}"
    );

    assert_eq!(unbound, StatusCode::NO_CONTENT);
    assert_eq!(
        deleted,
        StatusCode::NO_CONTENT,
        "an unbound database deletes through the route"
    );
}
