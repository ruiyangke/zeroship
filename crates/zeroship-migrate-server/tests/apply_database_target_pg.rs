//! An apply lands in the schema of the DATABASE it named, and only in that
//! one.
//!
//! Two servers, because the property needs both: a control plane that declares
//! databases and bindings, and a tenant cluster a reconciler has converged.
//! Nothing here hand-rolls a schema or a role - `db_<dbs>`, `zs_db_<dbs>_mig`
//! and the two capability roles are minted by
//! `zeroship_migrate_server::datastore::cluster`, so what these arms measure is
//! the shape production creates rather than the shape a fixture guessed.
//!
//! # Both halves of the first claim matter
//!
//! An apply that wrote into EVERY database an app holds would satisfy an
//! assertion that only looked at the named one. So the first arm asserts the
//! table is in the second database AND absent from the first, with the first
//! database converged, bound and empty - the control that makes the absence
//! mean something.
//!
//! # The refusal is named, not merely observed
//!
//! Several guards on this route answer with a 4xx, and an assertion that could
//! not tell them apart would pass over a deleted fence. The admission arm
//! asserts the specific `database_not_bound` kind, the database named in the
//! body, and the binding call in the remedy - then flips the ONE variable that
//! makes the binding live and requires the same request to succeed.

mod fixture;

#[path = "fixture/tenant.rs"]
mod tenant;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use compio_postgres::{Client, NoTls};
use fixture::world::World;
use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_authz::Action;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{database_derivation, DatabaseId};
use zeroship_id::{AppId, UserId};
use zeroship_migrate_server::apply::{apply_ir_documents, ApplyMigrationsRequest};
use zeroship_migrate_server::auth::{AuthError, Authenticator, VerifiedCaller};
use zeroship_migrate_server::datastore::control::ControlStore;
use zeroship_migrate_server::datastore::Reconciler;
use zeroship_migrate_server::policy::ManagedPolicyConfig;
use zeroship_migrate_server::rate_limit::MutationRateLimiter;
use zeroship_migrate_server::MigrationServiceState;

const SEAL_KEY: &[u8] = b"apply database target seal key 32 bytes";

/// A syntactically valid descriptor hash. Nothing on this route compares it.
const DESCRIPTOR: &str = "1111111111111111111111111111111111111111111111111111111111111111";

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

async fn connect(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|error| panic!("connect to {url}: {error}"));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

/// The control database as `zeroship_control`, the login the migration service
/// actually opens, so the reconciler runs under the grants it actually holds.
async fn control_as_service() -> Arc<Client> {
    let mut url = url::Url::parse(&fixture::migrated_url()).expect("the fixture DSN parses");
    url.set_username("zeroship_control")
        .expect("the DSN accepts a username");
    url.set_password(Some("zeroship_control"))
        .expect("the DSN accepts a password");
    Arc::new(connect(url.as_str()).await)
}

// ---------------------------------------------------------------------------
// The service under test
// ---------------------------------------------------------------------------

/// One bearer, one principal, one app. Authorization is not what these arms
/// measure, so it is the narrowest authenticator that still refuses everything
/// else.
#[derive(Debug)]
struct SingleAppAuthenticator {
    token: String,
    principal: UserId,
    app: AppId,
}

#[async_trait(?Send)]
impl Authenticator for SingleAppAuthenticator {
    async fn verify_action(
        &self,
        token: &str,
        app_id: &AppId,
        required_action: Action,
        _request_ip: Option<IpAddr>,
        _request_id: &str,
    ) -> Result<VerifiedCaller, AuthError> {
        if token != self.token {
            return Err(AuthError::Unauthorized);
        }
        if app_id != &self.app || required_action != Action::AppsDeploy {
            return Err(AuthError::Forbidden);
        }
        Ok(VerifiedCaller {
            principal_id: self.principal.clone(),
        })
    }
}

#[derive(Debug)]
struct AllowAllMutations;

#[async_trait(?Send)]
impl MutationRateLimiter for AllowAllMutations {
    async fn consume(
        &self,
        _source_ip: Option<IpAddr>,
    ) -> Result<zeroship_authn::rate_limit::RateLimitDecision, String> {
        Ok(zeroship_authn::rate_limit::RateLimitDecision::Allowed)
    }
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-apply-target-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("create the request scratch directory");
    path
}

fn policy_config() -> ManagedPolicyConfig {
    ManagedPolicyConfig::default_confined(SEAL_KEY.to_vec(), 1).expect("the confined ceiling")
}

/// The migration service, pointed at the TENANT cluster for DDL and at the
/// control plane for the apply ledger and the binding admission.
fn service_state(
    tenant_url: &str,
    principal: &UserId,
    app: &AppId,
) -> (Arc<MigrationServiceState>, PathBuf) {
    let tmp = tmpdir("service");
    let authenticator = Arc::new(SingleAppAuthenticator {
        token: "good-token".to_owned(),
        principal: principal.clone(),
        app: app.clone(),
    });
    (
        Arc::new(MigrationServiceState::new(
            tenant_url.to_owned(),
            fixture::migrated_url(),
            tmp.clone(),
            authenticator,
            Arc::new(AllowAllMutations),
            false,
            policy_config(),
        )),
        tmp,
    )
}

fn create_notes_request() -> Value {
    json!({
        "kind": "ir",
        "descriptor_sha256": DESCRIPTOR,
        "documents": [{
            "filename": "0001_create_notes.ir.json",
            "body": {
                "ir_version": 1,
                "name": "create_notes",
                "ops": [{
                    "op": "createTable",
                    "name": "notes",
                    "columns": [
                        {"name": "title", "type": "text", "nullable": false},
                        {"name": "body", "type": "text"}
                    ]
                }]
            }
        }]
    })
}

// ---------------------------------------------------------------------------
// Reading the tenant catalog back
// ---------------------------------------------------------------------------

/// Every top-level table in one database's schema, sorted.
async fn tables_in(cluster: &Client, database: &DatabaseId) -> Vec<String> {
    let schema = database_derivation::schema_name(database);
    cluster
        .query(
            "SELECT c.relname FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') \
                AND NOT c.relispartition \
              ORDER BY c.relname",
            &[&schema],
        )
        .await
        .expect("read the tenant catalog")
        .iter()
        .map(|row| row.get::<_, String>("relname"))
        .collect()
}

/// The owner of a relation, as `pg_roles` spells it.
async fn owner_of(cluster: &Client, database: &DatabaseId, table: &str) -> String {
    let schema = database_derivation::schema_name(database);
    cluster
        .query_one(
            "SELECT r.rolname FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
               JOIN pg_roles r ON r.oid = c.relowner \
              WHERE n.nspname = $1 AND c.relname = $2",
            &[&schema, &table],
        )
        .await
        .expect("the relation must exist to have an owner")
        .get("rolname")
}

/// The principal an apply is attributed to. The platform must hold the user
/// before a request can name them.
async fn seed_user(pg: &Client) -> UserId {
    let user = UserId::mint();
    pg.execute(
        "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
         VALUES ($1, $2::citext, 'apply target', NOW())",
        &[
            &user.as_str(),
            &format!("apply-{}@zeroship.test", user.as_str()),
        ],
    )
    .await
    .expect("seed the principal the apply ledger names");
    user
}

/// Run one reconciler pass and refuse anything but a completed one.
async fn pass(reconciler: &Reconciler) -> String {
    let (datastore, _) = reconciler
        .reconcile_once()
        .await
        .expect("the reconciler pass must complete");
    datastore.as_str().to_owned()
}

// ---------------------------------------------------------------------------
// The arms
// ---------------------------------------------------------------------------

/// An app bound to TWO databases migrates a table into the SECOND one.
///
/// The first database is converged and bound and stays empty, which is what
/// makes the absence measurable: a change that wrote into both would pass an
/// assertion that looked only at the second.
#[ntex::test]
async fn a_table_migrates_into_the_named_database_and_not_into_the_other() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = connect(&fixture::migrated_url()).await;
    let world = World::new(&pg, "apply-target").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let datastore = pass(&reconciler).await;
    let app = world.app(&pg, "shop").await;
    let app = AppId::parse(&app).expect("the world mints canonical app ids");
    let first = world.declare_database(&pg, &datastore, "primary").await;
    let second = world.declare_database(&pg, &datastore, "analytics").await;
    for database in [&first, &second] {
        world
            .declare_binding(&pg, app.as_str(), database, DatabaseCapability::ReadWrite)
            .await;
    }
    pass(&reconciler).await;

    // THE CONTROL, taken before the apply: both schemas exist and neither
    // carries a creator table, so the assertions below measure this apply.
    assert_eq!(
        tables_in(&cluster, &first).await,
        Vec::<String>::new(),
        "the first database must be converged and empty before the apply"
    );
    assert_eq!(
        tables_in(&cluster, &second).await,
        Vec::<String>::new(),
        "the second database must be converged and empty before the apply"
    );

    let request: ApplyMigrationsRequest =
        serde_json::from_value(create_notes_request()).expect("the fixture is a legal request");
    let tmp = tmpdir("apply");
    let report = apply_ir_documents(
        cluster_fixture.url(),
        &tmp,
        zeroship_migrate_server::apply::ApplyTarget {
            app_id: &app,
            database_id: &second,
        },
        &request,
        &policy_config(),
        &seed_user(&pg).await,
    )
    .await
    .expect("an apply naming a live-bound database succeeds");
    // The engine names what it applied by its own ids, so the report is only
    // evidence that something ran; what it ran, and where, is read out of the
    // catalog below.
    assert!(
        !report.applied.is_empty() && report.skipped.is_empty(),
        "the apply must have advanced the journal rather than skipping: {report:?}"
    );

    let second_tables = tables_in(&cluster, &second).await;
    assert!(
        second_tables.contains(&"notes".to_owned()),
        "the table must land in the database the request named: {second_tables:?}"
    );
    assert_eq!(
        tables_in(&cluster, &first).await,
        Vec::<String>::new(),
        "the OTHER database this app is bound to must be untouched"
    );

    // The schema's own owner ran the DDL. A table owned by anything else would
    // sit inside a schema the reconciler re-asserts ownership of on its next
    // pass, and the two would then disagree.
    assert_eq!(
        owner_of(&cluster, &second, "notes").await,
        database_derivation::migrator_role_name(&second).expect("the migrator role name fits"),
        "the apply runs as the database's own migrator"
    );

    let _ = std::fs::remove_dir_all(tmp);
}

/// An apply naming a database the app holds no LIVE binding to is refused, and
/// the same request succeeds once that binding is live.
///
/// The two halves differ in exactly one variable: the binding's
/// `observed_generation`. Everything else - the app, the database, the schema
/// on the cluster, the bearer, the body - is identical, so the refusal cannot
/// be attributed to anything the second half also had.
#[ntex::test]
async fn an_apply_naming_a_database_without_a_live_binding_is_refused() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = connect(&fixture::migrated_url()).await;
    let world = World::new(&pg, "apply-admission").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let datastore = pass(&reconciler).await;
    let app = world.app(&pg, "shop").await;
    let app = AppId::parse(&app).expect("the world mints canonical app ids");
    let database = world.declare_database(&pg, &datastore, "ledger").await;
    // A binding for a DIFFERENT app on the same database, so the database is
    // converged and reachable and the only thing missing is THIS app's edge.
    let neighbour = world.app(&pg, "neighbour").await;
    world
        .declare_binding(&pg, &neighbour, &database, DatabaseCapability::ReadWrite)
        .await;
    pass(&reconciler).await;
    assert_eq!(
        tables_in(&cluster, &database).await,
        Vec::<String>::new(),
        "the database must be converged before either half runs"
    );

    let principal = seed_user(&pg).await;
    let (state, tmp) = service_state(cluster_fixture.url(), &principal, &app);
    let service = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    let uri = format!(
        "/v1/apps/{}/databases/{}/migrations/apply",
        app.as_str(),
        database.as_str()
    );
    let post = || {
        test::TestRequest::post()
            .uri(&uri)
            .header("authorization", "Bearer good-token")
            .set_json(&create_notes_request())
            .to_request()
    };

    let refused = test::call_service(&service, post()).await;
    let refused_status = refused.status();
    let refused_body: Value =
        serde_json::from_slice(&test::read_body(refused).await).expect("a JSON refusal");
    assert_eq!(
        refused_status,
        StatusCode::CONFLICT,
        "an unbound database is a conflict, not an authorization failure: {refused_body}"
    );
    assert_eq!(
        refused_body["error"], "database_not_bound",
        "the SPECIFIC guard must be named: several guards on this route answer 4xx \
         and an assertion that could not tell them apart would pass over a deleted \
         fence: {refused_body}"
    );
    assert!(
        refused_body["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains(database.as_str())),
        "the refusal must name the database it is about: {refused_body}"
    );
    assert_eq!(
        refused_body["remedy"],
        json!(format!(
            "POST /api/databases/{}/bindings",
            database.as_str()
        )),
        "the refusal must name the call that fixes it: {refused_body}"
    );
    assert_eq!(
        tables_in(&cluster, &database).await,
        Vec::<String>::new(),
        "a refused apply must leave no DDL behind"
    );
    // THE CONTROL. One variable moves: this app's binding becomes live.
    let binding = world
        .declare_binding(&pg, app.as_str(), &database, DatabaseCapability::ReadWrite)
        .await;
    pass(&reconciler).await;
    let (status, generation, observed): (String, i32, i32) = {
        let row = pg
            .query_one(
                "SELECT status, generation, observed_generation \
                   FROM zeroship.database_bindings WHERE id = $1",
                &[&binding.as_str()],
            )
            .await
            .expect("the binding row must exist to be read");
        (
            row.get("status"),
            row.get("generation"),
            row.get("observed_generation"),
        )
    };
    assert_eq!(status, "active");
    assert!(
        observed >= generation,
        "the fixture must actually have converged the binding before the control runs"
    );

    let admitted = test::call_service(&service, post()).await;
    let admitted_status = admitted.status();
    let admitted_body: Value =
        serde_json::from_slice(&test::read_body(admitted).await).expect("a JSON reply");
    assert_eq!(
        admitted_status,
        StatusCode::OK,
        "the identical request must succeed once the binding is live: {admitted_body}"
    );
    let tables = tables_in(&cluster, &database).await;
    assert!(
        tables.contains(&"notes".to_owned()),
        "the admitted apply must have written its table: {tables:?}"
    );

    let _ = std::fs::remove_dir_all(tmp);
}

