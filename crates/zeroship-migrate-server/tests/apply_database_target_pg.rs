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
//!
//! # The rotation arm measures a PAIR, in one catalog read
//!
//! An apply that changes the schema mints `E+1` and retires `E-1`. Asserting
//! only the first passes over a rotation that swept every earlier epoch, which
//! fences apps that are serving correctly; asserting only the second passes
//! over one that retired without minting, which fences all of them. So the arm
//! requires `E+1` present, `E-1` gone and `E` standing at the same instant,
//! and pairs the whole thing with a re-post that commits no delta, where the
//! head must not move at all.

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
use zeroship_core::{database_derivation, BindingId, DatabaseId};
use zeroship_id::{AppId, UserId};
use zeroship_migrate_server::apply::{apply_ir_documents, ApplyMigrationsRequest};
use zeroship_migrate_server::auth::{AuthError, Authenticator, VerifiedCaller};
use zeroship_migrate_server::datastore::cluster;
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
        &fixture::migrated_url(),
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


/// An apply that changes the schema rotates the epoch: it mints `E+1` for
/// every live binding, retires `E-1`, and never touches `E`.
///
/// # Why three applies
///
/// The first has no predecessor to retire - a database converges at epoch `0` -
/// so it can only exhibit the mint. The second is where both halves are
/// measurable at once, and where the load-bearing pair lives: `E-1` is gone
/// AND `E` is still standing, in the same catalog read. A rotation that swept
/// by epoch rather than by predecessor would satisfy the first assertion and
/// fence every app serving on `E`.
///
/// The third is the control for the word "changes". It re-posts the documents
/// the second applied, so every version is already journalled and no schema
/// delta commits; a rotation that fired on every REQUEST rather than on every
/// committed delta would move the head here, and the binding an isolate
/// resolved a moment ago would name a role that is about to be retired for
/// nothing. It also pins the asymmetry: the head stays, and the retirement
/// still runs, because a retirement that waited to learn whether a delta
/// followed would be running after the DDL it must precede.
#[ntex::test]
async fn an_apply_that_changes_the_schema_rotates_the_epoch_and_leaves_the_live_one_standing() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = connect(&fixture::migrated_url()).await;
    let world = World::new(&pg, "apply-rotation").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let datastore = pass(&reconciler).await;
    let app = world.app(&pg, "shop").await;
    let app = AppId::parse(&app).expect("the world mints canonical app ids");
    let database = world.declare_database(&pg, &datastore, "ledger").await;
    let binding = world
        .declare_binding(&pg, app.as_str(), &database, DatabaseCapability::ReadWrite)
        .await;
    pass(&reconciler).await;
    let principal = seed_user(&pg).await;

    // THE STARTING STATE, read rather than assumed. A database converges at
    // epoch 0 on both sides and its binding role carries that epoch, so every
    // move below is attributable to an apply.
    assert_eq!(
        cluster_epoch(&cluster, &database).await,
        Some(0),
        "the reconciler converges a declared database at the epoch control declared"
    );
    assert_eq!(
        control_epoch(&pg, &database).await,
        0,
        "control's projection starts where the cluster does"
    );
    assert!(
        role_exists(&cluster, &binding_role(&binding, 0)).await,
        "the pass minted this binding's role at the converged epoch"
    );
    assert!(
        !role_exists(&cluster, &binding_role(&binding, 1)).await,
        "nothing has rotated yet, so the next epoch's role must not exist: without \
         this the mint assertion below could pass over a role the fixture created"
    );

    // ---- APPLY 1: 0 -> 1. Nothing to retire; the mint is what is measurable.
    apply_through(cluster_fixture.url(), &app, &database, &principal, &[NOTES]).await;
    assert_eq!(
        cluster_epoch(&cluster, &database).await,
        Some(1),
        "an apply that committed a schema delta advances the CLUSTER's head, which \
         is the authority for every binding role name"
    );
    assert_eq!(
        control_epoch(&pg, &database).await,
        1,
        "and projects it onto control, which is what a binding is composed from"
    );
    assert!(
        role_exists(&cluster, &binding_role(&binding, 1)).await,
        "the widen minted this live binding's role at the new epoch; without it \
         control names a role the cluster does not carry"
    );
    assert!(
        role_exists(&cluster, &binding_role(&binding, 0)).await,
        "epoch 0 is the head this apply ran against, and a serving app is never \
         fenced: the apply retires E-1, not E"
    );

    // ---- APPLY 2: 1 -> 2. Both halves, in one catalog read.
    apply_through(
        cluster_fixture.url(),
        &app,
        &database,
        &principal,
        &[NOTES, TAGS],
    )
    .await;
    assert_eq!(
        cluster_epoch(&cluster, &database).await,
        Some(2),
        "the second delta advances the head again"
    );
    assert_eq!(
        control_epoch(&pg, &database).await,
        2,
        "and the projection follows it"
    );
    assert!(
        role_exists(&cluster, &binding_role(&binding, 2)).await,
        "the new epoch's role is minted"
    );
    assert!(
        role_exists(&cluster, &binding_role(&binding, 1)).await,
        "THE PAIR: the epoch this apply ran against is still assumable. An isolate \
         built against it goes on serving while the rotation happens underneath"
    );
    assert!(
        !role_exists(&cluster, &binding_role(&binding, 0)).await,
        "THE PAIR: E-1 is gone, so an isolate two shapes behind fails at SET LOCAL \
         ROLE with 22023 rather than reading a schema it was not built against"
    );

    // ---- APPLY 3: the control. Nothing commits, so nothing rotates.
    let report = apply_through(
        cluster_fixture.url(),
        &app,
        &database,
        &principal,
        &[NOTES, TAGS],
    )
    .await;
    assert!(
        report.applied.is_empty() && !report.skipped.is_empty(),
        "the control only measures what it claims to if this apply committed no \
         schema delta at all: {report:?}"
    );
    assert_eq!(
        cluster_epoch(&cluster, &database).await,
        Some(2),
        "a rotation is owed by a committed schema delta, not by a request"
    );
    assert!(
        !role_exists(&cluster, &binding_role(&binding, 3)).await,
        "and no epoch beyond the head is minted"
    );
    assert!(
        role_exists(&cluster, &binding_role(&binding, 2)).await,
        "the head's own role stands, whatever this apply did or did not commit"
    );
    assert!(
        !role_exists(&cluster, &binding_role(&binding, 1)).await,
        "the RETIREMENT is unconditional where the MINT is not, and that asymmetry \
         is the only shape the ordering rule admits: the retirement runs before any \
         DDL commits, when nothing yet knows whether a delta will follow. So the two \
         live epochs are a CAP and not a floor - an apply that commits nothing still \
         narrows the window an isolate on E-1 has to re-resolve in"
    );
}

/// This binding's role name at one epoch, composed the way the data plane
/// composes it.
fn binding_role(binding: &BindingId, epoch: u32) -> String {
    database_derivation::binding_role_name(binding, epoch).expect("the role name fits")
}

async fn role_exists(cluster: &Client, role: &str) -> bool {
    !cluster
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
        .await
        .expect("read the role catalog")
        .is_empty()
}

/// The epoch the CLUSTER holds, which is the authority.
async fn cluster_epoch(cluster: &Client, database: &DatabaseId) -> Option<i32> {
    cluster
        .query_opt(
            &format!(
                "SELECT schema_epoch FROM {}.{} WHERE database_id = $1",
                cluster::ADMIN_SCHEMA,
                cluster::EPOCH_TABLE
            ),
            &[&database.as_str()],
        )
        .await
        .expect("read the cluster's epoch table")
        .map(|row| row.get("schema_epoch"))
}

/// The epoch CONTROL projects, which is what a binding is composed from.
async fn control_epoch(pg: &Client, database: &DatabaseId) -> i32 {
    pg.query_one(
        "SELECT schema_epoch FROM zeroship.databases WHERE id = $1",
        &[&database.as_str()],
    )
    .await
    .expect("the database row must exist to be read")
    .get("schema_epoch")
}

/// One `.ir.json` document, by name and by the table it creates.
struct Document {
    filename: &'static str,
    name: &'static str,
    table: &'static str,
}

const NOTES: Document = Document {
    filename: "0001_create_notes.ir.json",
    name: "create_notes",
    table: "notes",
};

const TAGS: Document = Document {
    filename: "0002_create_tags.ir.json",
    name: "create_tags",
    table: "tags",
};

/// Apply a document set through the service's own apply.
///
/// The WHOLE set every time, because that is what the CLI posts and what
/// `attest_complete_history` requires: a request naming only the new file is
/// refused for an incomplete history, not applied.
async fn apply_through(
    tenant_url: &str,
    app: &AppId,
    database: &DatabaseId,
    principal: &UserId,
    documents: &[Document],
) -> zeroship_migrate_server::apply::ApplyMigrationsResponse {
    let body = json!({
        "kind": "ir",
        "descriptor_sha256": DESCRIPTOR,
        "documents": documents
            .iter()
            .map(|document| json!({
                "filename": document.filename,
                "body": {
                    "ir_version": 1,
                    "name": document.name,
                    "ops": [{
                        "op": "createTable",
                        "name": document.table,
                        "columns": [{"name": "title", "type": "text", "nullable": false}]
                    }]
                }
            }))
            .collect::<Vec<_>>(),
    });
    let request: ApplyMigrationsRequest =
        serde_json::from_value(body).expect("the fixture is a legal request");
    let tmp = tmpdir("rotation");
    let report = apply_ir_documents(
        tenant_url,
        &fixture::migrated_url(),
        &tmp,
        zeroship_migrate_server::apply::ApplyTarget {
            app_id: app,
            database_id: database,
        },
        &request,
        &policy_config(),
        principal,
    )
    .await
    .unwrap_or_else(|error| panic!("apply into {}: {error}", database.as_str()));
    let _ = std::fs::remove_dir_all(tmp);
    report
}
