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
//! # The role arm needs a control that the apply committed something
//!
//! "An apply mints no binding role and retires none" is satisfied by an apply
//! that did nothing at all, so the arm reads the ENGINE JOURNAL either side of
//! the apply and requires its high-water mark to have moved. The journal is
//! `__zeroship_schema_migrations` in the database's own schema, written by the
//! engine as each version commits, so a delta that reached the cluster moved it
//! and a fully skipped apply did not.

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

/// One bearer, one principal, one database. Authorization is not what these
/// arms measure - `apply_api_test` drives the real seat resolver - so it is the
/// narrowest authenticator that still refuses everything else.
#[derive(Debug)]
struct SingleDatabaseAuthenticator {
    token: String,
    principal: UserId,
    database: DatabaseId,
}

#[async_trait(?Send)]
impl Authenticator for SingleDatabaseAuthenticator {
    async fn verify_action(
        &self,
        token: &str,
        database_id: &DatabaseId,
        required_action: Action,
        _request_ip: Option<IpAddr>,
        _request_id: &str,
    ) -> Result<VerifiedCaller, AuthError> {
        if token != self.token {
            return Err(AuthError::Unauthorized);
        }
        if database_id != &self.database || required_action != Action::DatabaseMigrate {
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
/// control plane for readiness.
fn service_state(
    tenant_url: &str,
    principal: &UserId,
    database: &DatabaseId,
) -> (Arc<MigrationServiceState>, PathBuf) {
    let tmp = tmpdir("service");
    let authenticator = Arc::new(SingleDatabaseAuthenticator {
        token: "good-token".to_owned(),
        principal: principal.clone(),
        database: database.clone(),
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
        &second,
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

/// A database NO APP IS BOUND TO is migratable through the route.
///
/// This is the state `zeroship db create` leaves behind: `active`, with its
/// schema, its migrator and its two capability roles minted, and no binding -
/// because binding is a separate explicit act. Every other command can address
/// such a database; this is the one that could not, and the reason was that an
/// app id sat in the path and a live-binding admission hung off it.
///
/// # The absence is MEASURED, on both sides of the apply
///
/// "No app is bound" asserted by a fixture that simply did not write a binding
/// is an assumption, so the binding count is read out of the control plane
/// before the request. It is read again afterwards for the other direction: an
/// apply that MINTED an edge to make itself legal would satisfy the first read
/// and be exactly the coupling this removes.
///
/// # The success is read off the cluster, not off the reply
///
/// A 200 says the handler returned; the table in `db_<dbs>` says the DDL
/// reached the tenant.
#[ntex::test]
async fn an_apply_reaches_a_database_no_app_is_bound_to() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = connect(&fixture::migrated_url()).await;
    let world = World::new(&pg, "apply-unbound").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let datastore = pass(&reconciler).await;
    // No app is declared in this world at all, so there is nothing a binding
    // could name even by accident.
    let database = world.declare_database(&pg, &datastore, "ledger").await;
    pass(&reconciler).await;

    assert_eq!(
        bindings_on(&pg, &database).await,
        0,
        "the world must declare no binding, or this arm measures the bound case"
    );
    assert_eq!(
        tables_in(&cluster, &database).await,
        Vec::<String>::new(),
        "the database must be converged and empty before the apply"
    );

    let principal = seed_user(&pg).await;
    let (state, tmp) = service_state(cluster_fixture.url(), &principal, &database);
    let service = test::init_service(
        web::App::new()
            .state(state)
            .configure(zeroship_migrate_server::configure),
    )
    .await;
    let request = test::TestRequest::post()
        .uri(&format!(
            "/v1/databases/{}/migrations/apply",
            database.as_str()
        ))
        .header("authorization", "Bearer good-token")
        .set_json(&create_notes_request())
        .to_request();
    let response = test::call_service(&service, request).await;
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&test::read_body(response).await).expect("a JSON body");
    assert_eq!(
        status,
        StatusCode::OK,
        "an unbound database must be migratable: {body}"
    );

    let tables = tables_in(&cluster, &database).await;
    assert!(
        tables.contains(&"notes".to_owned()),
        "the apply must have written its table into the unbound database: {tables:?}"
    );
    assert_eq!(
        bindings_on(&pg, &database).await,
        0,
        "the apply must not have minted an app edge to make itself legal"
    );

    let _ = std::fs::remove_dir_all(tmp);
}

/// How many apps reach this database, straight off the control plane.
async fn bindings_on(pg: &Client, database: &DatabaseId) -> i64 {
    pg.query_one(
        "SELECT count(*)::int8 AS n FROM zeroship.database_bindings WHERE database_id = $1",
        &[&database.as_str()],
    )
    .await
    .expect("count the database's bindings")
    .get("n")
}

/// An apply that commits a schema delta mints no binding role and retires none.
///
/// A binding role is the object the two membership edges hang off, and it is
/// per BINDING and nothing else. Moving it is what a REVOKE does, so a creator
/// changing their own schema must not move it: a creator would otherwise be
/// able to fence their own running build, and every co-tenant of the database
/// with it.
///
/// # Two applies, and a control on each
///
/// Two, because an apply that mints on the FIRST delta and not the second, or
/// the other way round, is refuted by only one of them. The control on each is
/// the ENGINE JOURNAL, read either side: "nothing was minted" is satisfied by
/// an apply that did nothing at all, so the frontier has to have moved for the
/// role assertion to be about an apply that reached the cluster. The report's
/// own `applied` list is asserted beside it, because the journal and the
/// service's report are two different witnesses to the same commit.
#[ntex::test]
async fn an_apply_that_commits_a_schema_delta_mints_no_binding_role_and_retires_none() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = connect(&fixture::migrated_url()).await;
    let world = World::new(&pg, "apply-roles").await;
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

    // THE STARTING STATE, read rather than assumed: the pass minted exactly one
    // binding role, and it is the one the data plane composes for this edge.
    let minted = binding_roles(&cluster).await;
    assert_eq!(
        minted,
        vec![
            database_derivation::binding_role_name(&binding).expect("the role name fits")
        ],
        "the pass mints one role per binding and nothing else"
    );

    // AND NO APPLY HAS RUN: the engine journal is what an apply installs, so a
    // database that has none has had no delta committed into it. This is the
    // baseline the two frontier readings below are growth FROM.
    assert!(
        !journal_exists(&cluster, &database).await,
        "a database no apply has touched carries no engine journal"
    );

    // ---- APPLY 1. The first delta this database has ever seen.
    let report = apply_through(cluster_fixture.url(), &database, &principal, &[NOTES]).await;
    assert!(
        !report.applied.is_empty(),
        "the control for the role assertion below: this apply must have committed \
         a version, not skipped one: {report:?}"
    );
    let after = journal_frontier(&cluster, &database).await;
    assert!(
        after > 0,
        "the engine journal must have advanced past its empty state ({after}), or \
         the role assertion below is about an apply that touched nothing"
    );
    assert_eq!(
        binding_roles(&cluster).await,
        minted,
        "a committed schema delta mints no binding role and retires none"
    );

    // ---- APPLY 2. A second delta on the same database, for the same reason.
    let before = after;
    let report = apply_through(cluster_fixture.url(), &database, &principal, &[NOTES, TAGS]).await;
    assert!(
        !report.applied.is_empty(),
        "the second delta must commit too: {report:?}"
    );
    let after = journal_frontier(&cluster, &database).await;
    assert!(
        after > before,
        "the journal must advance again ({before} -> {after})"
    );
    assert_eq!(
        binding_roles(&cluster).await,
        minted,
        "the second delta moves the role catalog no more than the first did"
    );

    // AND THE TABLES LANDED, so the applies above were schema changes rather
    // than requests the service accepted and dropped.
    let tables = tables_in(&cluster, &database).await;
    for expected in [NOTES.table, TAGS.table] {
        assert!(
            tables.contains(&expected.to_owned()),
            "{expected} must be in the database's own schema: {tables:?}"
        );
    }
}

/// Every `zs_bind_` role on the cluster, in catalog order.
///
/// The WHOLE set rather than one name: an assertion that only looked for the
/// role it expected would pass over an apply that minted a second one beside
/// it, which is exactly what a rotation used to do.
async fn binding_roles(cluster: &Client) -> Vec<String> {
    cluster
        .query(
            "SELECT rolname FROM pg_roles WHERE left(rolname, 8) = 'zs_bind_' ORDER BY rolname",
            &[],
        )
        .await
        .expect("read the role catalog")
        .iter()
        .map(|row| row.get("rolname"))
        .collect()
}

/// Whether the engine has installed its journal in this database's schema.
///
/// An apply creates it, so its absence is the state a database no apply has
/// reached. `to_regclass` answers `NULL` rather than raising, which is what
/// lets this be asked of a schema that may not carry the table.
async fn journal_exists(cluster: &Client, database: &DatabaseId) -> bool {
    let schema = database_derivation::schema_name(database);
    cluster
        .query_one(
            "SELECT to_regclass($1) IS NOT NULL AS present",
            &[&format!("{schema}.__zeroship_schema_migrations")],
        )
        .await
        .expect("read the catalog")
        .get("present")
}

/// The engine journal's high-water mark in one database's schema.
///
/// `event_seq` is `GENERATED ALWAYS AS IDENTITY` on an append-only table, so an
/// applied migration moves it and a fully skipped apply does not. Naming the
/// engine's table here couples this control to the engine deliberately: a
/// rename makes the read fail with `42P01` on a table `PostgreSQL` names,
/// rather than silently reporting that nothing was applied.
async fn journal_frontier(cluster: &Client, database: &DatabaseId) -> i64 {
    let schema = database_derivation::schema_name(database);
    cluster
        .query_one(
            &format!(
                "SELECT coalesce(max(event_seq), 0)::bigint AS frontier \
                   FROM \"{schema}\".\"__zeroship_schema_migrations\""
            ),
            &[],
        )
        .await
        .expect("read the engine journal")
        .get("frontier")
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
    let tmp = tmpdir("apply");
    let report = apply_ir_documents(
        tenant_url,
        &tmp,
        database,
        &request,
        &policy_config(),
        principal,
    )
    .await
    .unwrap_or_else(|error| panic!("apply into {}: {error}", database.as_str()));
    let _ = std::fs::remove_dir_all(tmp);
    report
}
