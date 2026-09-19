//! The cluster reconciler, against a live control plane and a live tenant
//! cluster.
//!
//! Two servers, because the design's central constraint is that there are two:
//! control declares into ITS database and the reconciler makes ANOTHER server
//! match, with no transaction spanning them. A target that faked either side
//! could not exhibit the one property every arm here turns on - that a control
//! row and the catalog it describes are written separately and can disagree.
//!
//! # The reconciler runs as the role the service actually has
//!
//! The control connection is opened as `zeroship_control`, the least-privilege
//! login `deploy/compose/docker-compose.yml` gives this service, so every
//! statement the reconciler issues is exercised against the grants
//! `db/migrations-ts/20260919000200_database_entities.ts` actually hands it.
//! The FIXTURE's own setup runs as the superuser, because seeding an
//! organization is not what is under test.
//!
//! # Every refusal is paired with a control differing in one variable
//!
//! A denial arm on a fixture that granted nothing passes for the wrong reason.
//! Three pairs are load-bearing rather than decorative:
//!
//! - the reap's completeness gate: the same pass, twice, differing only in
//!   whether the login may read `zeroship.database_bindings`;
//! - the schema rule: the same undeclared schema, twice, differing only in
//!   whether a `deleting` row declares it;
//! - the version floor: two clusters differing only in their major.

mod fixture;

#[path = "fixture/tenant.rs"]
mod tenant;

use std::sync::Arc;

use compio_postgres::error::SqlState;
use compio_postgres::{Client, NoTls};
use uuid::Uuid;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{database_derivation, BindingId, DatabaseId};
use zeroship_migrate_server::apply::WORKER_ROLE;
use zeroship_migrate_server::datastore::cluster::{ADMIN_SCHEMA, EPOCH_TABLE, RELAY_ROLE};
use zeroship_migrate_server::datastore::control::{ControlStore, SoleZone};
use zeroship_migrate_server::datastore::{PassReport, ReconcileError, Reconciler};

/// The password the fixture gives a tenant login. The cluster is thrown away
/// with the test.
const TENANT_PASSWORD: &str = "fixture";

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

/// The control database as the superuser, for fixture setup only.
async fn control_superuser() -> Client {
    connect(&fixture::migrated_url()).await
}

/// The control database as `zeroship_control`: the login the migration service
/// actually opens, so the reconciler's statements are measured against the
/// grants it actually holds.
async fn control_as_service() -> Arc<Client> {
    let mut url = url::Url::parse(&fixture::migrated_url()).expect("the fixture DSN parses");
    url.set_username("zeroship_control")
        .expect("the DSN accepts a username");
    url.set_password(Some("zeroship_control"))
        .expect("the DSN accepts a password");
    Arc::new(connect(url.as_str()).await)
}

// ---------------------------------------------------------------------------
// The control-plane world this test declares into
// ---------------------------------------------------------------------------

struct World {
    zone: String,
    organization: String,
    project: String,
}

impl World {
    /// One zone, one organization, one project, seeded directly.
    ///
    /// The zone is private to the test so the datastore this test registers is
    /// invisible to placement in any other test's zone.
    async fn new(pg: &Client, label: &str) -> Self {
        let unique = Uuid::new_v4().simple().to_string();
        let zone = zeroship_core::typed_id::generate("ezn");
        pg.execute(
            "INSERT INTO zeroship.execution_zones (id, name, status) \
             VALUES ($1, $2, 'active')",
            &[&zone, &format!("{label}-{unique}")],
        )
        .await
        .expect("declare this test's execution zone");

        pg.execute(
            "INSERT INTO zeroship.plans (id, name, runtime_limits_json) \
             VALUES ('free', 'free', '{}'::json) ON CONFLICT (id) DO NOTHING",
            &[],
        )
        .await
        .expect("the plan an app row references");

        let organization = zeroship_core::typed_id::generate("org");
        pg.execute(
            "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
             VALUES ($1, $2, $3, $4)",
            &[
                &organization,
                &format!("{label}-{unique}"),
                &label,
                &format!("{label}-{unique}@example.test"),
            ],
        )
        .await
        .expect("seed this test's organization");

        let project = zeroship_core::typed_id::generate("prj");
        pg.execute(
            "INSERT INTO zeroship.projects \
                 (id, organization_id, slug, name, execution_zone_id) \
             VALUES ($1, $2, $3, $4, $5)",
            &[
                &project,
                &organization,
                &format!("{label}-{unique}"),
                &label,
                &zone,
            ],
        )
        .await
        .expect("seed this test's project in this test's zone");

        Self {
            zone,
            organization,
            project,
        }
    }

    async fn app(&self, pg: &Client, label: &str) -> String {
        let app = zeroship_core::typed_id::generate("app");
        pg.execute(
            "INSERT INTO zeroship.apps \
                 (id, name, plan_id, project_id, organization_id, execution_zone_id) \
             VALUES ($1, $2, 'free', $3, $4, $5)",
            &[
                &app,
                &format!("{label}-{}", Uuid::new_v4().simple()),
                &self.project,
                &self.organization,
                &self.zone,
            ],
        )
        .await
        .expect("seed an app in this test's project");
        app
    }

    /// Declare a database, exactly as `zeroship_control::databases::create`
    /// does: `provisioning`, on a datastore placement chose.
    async fn declare_database(&self, pg: &Client, datastore: &str, name: &str) -> DatabaseId {
        let database = DatabaseId::mint();
        pg.execute(
            "INSERT INTO zeroship.databases \
                 (id, project_id, execution_zone_id, datastore_id, name, status) \
             VALUES ($1, $2, $3, $4, $5, 'provisioning')",
            &[
                &database.as_str(),
                &self.project,
                &self.zone,
                &datastore,
                &name,
            ],
        )
        .await
        .expect("declare a database on this test's datastore");
        database
    }

    /// Declare a binding, exactly as `zeroship_control::databases::bind` does.
    async fn declare_binding(
        &self,
        pg: &Client,
        app: &str,
        database: &DatabaseId,
        capability: DatabaseCapability,
    ) -> BindingId {
        let binding = BindingId::mint();
        pg.execute(
            "INSERT INTO zeroship.database_bindings \
                 (id, app_id, database_id, project_id, capability, status) \
             VALUES ($1, $2, $3, $4, $5, 'pending')",
            &[
                &binding.as_str(),
                &app,
                &database.as_str(),
                &self.project,
                &capability.as_wire(),
            ],
        )
        .await
        .expect("declare a binding in this test's project");
        binding
    }
}

// ---------------------------------------------------------------------------
// Reading state back
// ---------------------------------------------------------------------------

async fn datastore_row(pg: &Client, datastore: &str) -> (String, Option<String>) {
    let row = pg
        .query_one(
            "SELECT status, last_error FROM zeroship.datastores WHERE id = $1",
            &[&datastore],
        )
        .await
        .expect("the datastore row must exist to be read");
    (row.get("status"), row.get("last_error"))
}

async fn database_status(pg: &Client, database: &DatabaseId) -> Option<String> {
    pg.query_opt(
        "SELECT status FROM zeroship.databases WHERE id = $1",
        &[&database.as_str()],
    )
    .await
    .expect("read the database row")
    .map(|row| row.get("status"))
}

async fn binding_row(pg: &Client, binding: &BindingId) -> (String, i32, i32, Option<String>) {
    let row = pg
        .query_one(
            "SELECT status, generation, observed_generation, last_error \
               FROM zeroship.database_bindings WHERE id = $1",
            &[&binding.as_str()],
        )
        .await
        .expect("the binding row must exist to be read");
    (
        row.get("status"),
        row.get("generation"),
        row.get("observed_generation"),
        row.get("last_error"),
    )
}

/// Every `zs_`-prefixed role plus the two platform logins, sorted.
async fn platform_roles(cluster: &Client) -> Vec<String> {
    let rows = cluster
        .query(
            "SELECT rolname FROM pg_roles \
              WHERE left(rolname, 3) = 'zs_' OR rolname = $1 OR rolname = $2 \
              ORDER BY rolname",
            &[&WORKER_ROLE, &RELAY_ROLE],
        )
        .await
        .expect("read the role catalog");
    rows.iter().map(|row| row.get("rolname")).collect()
}

/// `(granted role, member, inherit_option, set_option)` for every membership
/// among the platform's own roles, sorted.
///
/// The two options ARE the fence, and they are recorded per membership at grant
/// time, so reading them from the catalog is the only way to know which shape
/// was actually issued.
async fn memberships(cluster: &Client) -> Vec<(String, String, bool, bool)> {
    let rows = cluster
        .query(
            "SELECT granted.rolname AS granted, \
                    member.rolname  AS member, \
                    membership.inherit_option, \
                    membership.set_option \
               FROM pg_auth_members membership \
               JOIN pg_roles granted ON granted.oid = membership.roleid \
               JOIN pg_roles member  ON member.oid  = membership.member \
              WHERE left(granted.rolname, 3) = 'zs_' \
              ORDER BY granted.rolname, member.rolname",
            &[],
        )
        .await
        .expect("read the membership catalog");
    rows.iter()
        .map(|row| {
            (
                row.get("granted"),
                row.get("member"),
                row.get("inherit_option"),
                row.get("set_option"),
            )
        })
        .collect()
}

async fn schemas(cluster: &Client) -> Vec<String> {
    let rows = cluster
        .query(
            "SELECT nspname FROM pg_catalog.pg_namespace \
              WHERE left(nspname, 3) = 'db_' OR nspname = $1 ORDER BY nspname",
            &[&ADMIN_SCHEMA],
        )
        .await
        .expect("read the namespace catalog");
    rows.iter().map(|row| row.get("nspname")).collect()
}

async fn cluster_epoch(cluster: &Client, database: &DatabaseId) -> Option<i32> {
    cluster
        .query_opt(
            &format!(
                "SELECT schema_epoch FROM {ADMIN_SCHEMA}.{EPOCH_TABLE} WHERE database_id = $1"
            ),
            &[&database.as_str()],
        )
        .await
        .expect("read the cluster's epoch table")
        .map(|row| row.get("schema_epoch"))
}

/// Everything about the cluster and the control rows that a converging pass
/// could move, in one comparable value.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    roles: Vec<String>,
    memberships: Vec<(String, String, bool, bool)>,
    schemas: Vec<String>,
    datastore: (String, Option<String>),
    databases: Vec<(String, String, i32)>,
    bindings: Vec<(String, String, i32, i32, Option<String>)>,
}

async fn snapshot(cluster: &Client, pg: &Client, datastore: &str) -> Snapshot {
    let databases = pg
        .query(
            "SELECT id, status, schema_epoch FROM zeroship.databases \
              WHERE datastore_id = $1 ORDER BY id",
            &[&datastore],
        )
        .await
        .expect("read this datastore's databases");
    let bindings = pg
        .query(
            "SELECT binding.id, binding.status, binding.generation, \
                    binding.observed_generation, binding.last_error \
               FROM zeroship.database_bindings binding \
               JOIN zeroship.databases database ON database.id = binding.database_id \
              WHERE database.datastore_id = $1 ORDER BY binding.id",
            &[&datastore],
        )
        .await
        .expect("read this datastore's bindings");
    Snapshot {
        roles: platform_roles(cluster).await,
        memberships: memberships(cluster).await,
        schemas: schemas(cluster).await,
        datastore: datastore_row(pg, datastore).await,
        databases: databases
            .iter()
            .map(|row| {
                (
                    row.get("id"),
                    row.get("status"),
                    row.get("schema_epoch"),
                )
            })
            .collect(),
        bindings: bindings
            .iter()
            .map(|row| {
                (
                    row.get("id"),
                    row.get("status"),
                    row.get("generation"),
                    row.get("observed_generation"),
                    row.get("last_error"),
                )
            })
            .collect(),
    }
}

/// Run a pass and refuse anything but a completed one.
async fn pass(reconciler: &Reconciler) -> (String, PassReport) {
    let (datastore, report) = reconciler
        .reconcile_once()
        .await
        .expect("the reconciler pass must complete");
    (datastore.as_str().to_owned(), report)
}

/// The server's `ErrorResponse`, or a panic naming what arrived instead. A
/// transport failure carries no SQLSTATE, and treating one as "some error" is
/// how a denial arm stops measuring the denial.
fn server_error(error: &compio_postgres::Error) -> &compio_postgres::error::DbError {
    error
        .as_db_error()
        .unwrap_or_else(|| panic!("expected a server error response, got: {error}"))
}

/// Read one row under `role`, the way the data plane narrows: `SET LOCAL ROLE`
/// as the first statement of an explicit transaction, reverted at rollback.
async fn read_under_role(
    client: &mut Client,
    role: &str,
    schema: &str,
) -> Result<i32, compio_postgres::Error> {
    let transaction = client.transaction().await?;
    let narrowed = async {
        transaction
            .simple_query(&format!("SET LOCAL ROLE \"{role}\""))
            .await?;
        transaction
            .query(
                &format!("SELECT total FROM \"{schema}\".orders WHERE id = 1"),
                &[],
            )
            .await
    }
    .await;
    // Explicit on both paths: a statement that failed leaves the session in an
    // aborted transaction, and the next arm's refusal would then be 25P02
    // rather than the one being measured.
    let rolled_back = transaction.rollback().await;
    let rows = narrowed?;
    rolled_back?;
    assert_eq!(rows.len(), 1, "the fixture row must be present to be read");
    Ok(rows[0].get("total"))
}

/// Give a converged database the one table the fence arms read.
///
/// This is what an APPLY does, not what the reconciler does: the reconciler
/// mints the roles and grants `USAGE` on the schema, and which COLUMNS each
/// capability may touch comes from the owner's own migration IR. Spelling it
/// here keeps that boundary visible instead of hiding it behind a reconciler
/// that quietly granted table-wide privileges.
async fn seed_table(cluster: &Client, database: &DatabaseId, total: i32) {
    let schema = database_derivation::schema_name(database);
    let readwrite =
        database_derivation::capability_role_name(database, DatabaseCapability::ReadWrite)
            .expect("the fixture capability role name fits");
    let readonly =
        database_derivation::capability_role_name(database, DatabaseCapability::ReadOnly)
            .expect("the fixture capability role name fits");
    cluster
        .batch_execute(&format!(
            "CREATE TABLE \"{schema}\".orders (id int PRIMARY KEY, total int NOT NULL);
             INSERT INTO \"{schema}\".orders VALUES (1, {total});
             GRANT SELECT (id, total), INSERT (id, total), UPDATE (id, total) \
                 ON \"{schema}\".orders TO \"{readwrite}\";
             GRANT SELECT (id, total) ON \"{schema}\".orders TO \"{readonly}\";"
        ))
        .await
        .expect("an apply's column grants over a converged schema");
}

// ---------------------------------------------------------------------------
// Arms
// ---------------------------------------------------------------------------

/// Registration, bootstrap, convergence, and then a pass that changes nothing.
///
/// The idempotency claim is measured on OBSERVABLE STATE, not on statements
/// issued: the convergence statements all state a desired state and are re-run
/// every pass, so what proves the second pass is a no-op is a byte-identical
/// snapshot of the role catalog, the membership catalog, the namespace catalog
/// and the control rows.
#[ntex::test]
async fn a_pass_registers_bootstraps_and_converges_then_a_second_pass_changes_nothing() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = control_superuser().await;
    let world = World::new(&pg, "converge").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    // PASS 1: nothing is declared on this cluster yet, so this is registration
    // and bootstrap alone.
    let (datastore, first) = pass(&reconciler).await;
    assert!(first.registered, "the first pass inserts the registry row");
    assert!(
        first.datastore_activated,
        "the bootstrap corpus applied, so the cluster enters rotation"
    );
    assert_eq!(
        datastore_row(&pg, &datastore).await,
        ("active".to_owned(), None),
        "a bootstrapped cluster carries no error"
    );
    assert_eq!(
        platform_roles(&cluster).await,
        vec![RELAY_ROLE.to_owned(), WORKER_ROLE.to_owned()],
        "the corpus creates the two platform logins and nothing else"
    );
    assert!(
        schemas(&cluster).await.contains(&ADMIN_SCHEMA.to_owned()),
        "the corpus creates the platform's own schema"
    );

    // THE LOGIN IS CREATED WITHOUT A PASSWORD, so it cannot yet authenticate.
    // A cluster's authentication material is deployment, and a bootstrap that
    // baked a known password into every tenant cluster's worker login would be
    // worse than the manual step it saves.
    let unusable = compio_postgres::connect(
        &cluster_fixture.url_as(WORKER_ROLE, TENANT_PASSWORD),
        NoTls,
    )
    .await
    .err()
    .expect("a passwordless login must not authenticate");
    let unusable = server_error(&unusable);
    assert_eq!(
        unusable.code().code().get(0..2),
        Some("28"),
        "the refusal must be class 28, invalid authorization specification, and not a \
         connectivity failure that would pass for one: {unusable:?}"
    );

    // PASS 2: one database and two bindings on it, declared the way the
    // control-plane surface declares them.
    let database = world.declare_database(&pg, &datastore, "orders").await;
    let reader = world.app(&pg, "reader").await;
    let writer = world.app(&pg, "writer").await;
    let readonly = world
        .declare_binding(&pg, &reader, &database, DatabaseCapability::ReadOnly)
        .await;
    let readwrite = world
        .declare_binding(&pg, &writer, &database, DatabaseCapability::ReadWrite)
        .await;

    let (_, second) = pass(&reconciler).await;
    assert_eq!(
        second.databases_activated,
        vec![database.clone()],
        "the declared database converged"
    );
    assert_eq!(second.bindings_activated.len(), 2, "{second:?}");
    assert_eq!(
        database_status(&pg, &database).await,
        Some("active".to_owned())
    );
    for binding in [&readonly, &readwrite] {
        let (status, generation, observed, last_error) = binding_row(&pg, binding).await;
        assert_eq!(status, "active", "binding {}", binding.as_str());
        assert_eq!(
            observed,
            generation,
            "a converged binding's observed generation has caught up"
        );
        assert_eq!(last_error, None);
    }

    // The cluster is the authority on the epoch, and the role names carry it.
    assert_eq!(
        cluster_epoch(&cluster, &database).await,
        Some(0),
        "converging a database writes its epoch row"
    );

    // THE LADDER, read out of the catalog rather than inferred from the
    // statements that were sent. `set_option = false` on every
    // binding-to-database edge and `inherit_option = false` on every
    // worker-to-binding edge is the whole fence.
    let schema = database_derivation::schema_name(&database);
    let migrator = database_derivation::migrator_role_name(&database).expect("fits");
    let rw = database_derivation::capability_role_name(&database, DatabaseCapability::ReadWrite)
        .expect("fits");
    let ro = database_derivation::capability_role_name(&database, DatabaseCapability::ReadOnly)
        .expect("fits");
    let readonly_role =
        database_derivation::binding_role_name(&readonly, 0).expect("fits");
    let readwrite_role =
        database_derivation::binding_role_name(&readwrite, 0).expect("fits");
    let roles = platform_roles(&cluster).await;
    for expected in [&migrator, &rw, &ro, &readonly_role, &readwrite_role] {
        assert!(roles.contains(expected), "`{expected}` must exist: {roles:?}");
    }
    assert!(
        schemas(&cluster).await.contains(&schema),
        "the database's schema must exist"
    );

    let mut expected_edges = vec![
        (ro.clone(), readonly_role.clone(), true, false),
        (rw.clone(), readwrite_role.clone(), true, false),
        (readonly_role.clone(), WORKER_ROLE.to_owned(), false, true),
        (readwrite_role.clone(), WORKER_ROLE.to_owned(), false, true),
    ];
    expected_edges.sort();
    let mut observed_edges: Vec<_> = memberships(&cluster)
        .await
        .into_iter()
        .filter(|(_, member, _, _)| member != "postgres")
        .collect();
    observed_edges.sort();
    assert_eq!(
        observed_edges, expected_edges,
        "every edge, and no other: a binding inherits its database role and may not \
         assume it, and the worker may assume a binding and inherits nothing"
    );
    assert!(
        !observed_edges
            .iter()
            .any(|(granted, member, _, _)| member == WORKER_ROLE && granted.starts_with("zs_db_")),
        "the worker must hold no direct membership in a database role"
    );

    // PASS 3: the state is converged, so nothing moves.
    let before = snapshot(&cluster, &pg, &datastore).await;
    let (_, third) = pass(&reconciler).await;
    let after = snapshot(&cluster, &pg, &datastore).await;
    assert!(
        third.changed_nothing(),
        "a pass over converged state must change nothing: {third:?}"
    );
    assert_eq!(
        before, after,
        "the catalog and the control rows must be identical across a repeat pass"
    );
    assert!(
        !before.roles.is_empty() && !before.memberships.is_empty(),
        "the control for the comparison above: it must not be comparing two empty sets"
    );
}

/// A converged binding reaches its own database and is refused another's, and
/// the fence is the membership shape rather than anything in a process.
#[ntex::test]
async fn a_converged_binding_reaches_its_own_database_and_is_refused_its_neighbour() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = control_superuser().await;
    let world = World::new(&pg, "fence").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let (datastore, _) = pass(&reconciler).await;
    let mine = world.declare_database(&pg, &datastore, "mine").await;
    let theirs = world.declare_database(&pg, &datastore, "theirs").await;
    let my_app = world.app(&pg, "mine").await;
    let their_app = world.app(&pg, "theirs").await;
    let my_binding = world
        .declare_binding(&pg, &my_app, &mine, DatabaseCapability::ReadWrite)
        .await;
    let their_binding = world
        .declare_binding(&pg, &their_app, &theirs, DatabaseCapability::ReadWrite)
        .await;
    let (_, report) = pass(&reconciler).await;
    assert_eq!(report.bindings_activated.len(), 2, "{report:?}");

    seed_table(&cluster, &mine, 42).await;
    seed_table(&cluster, &theirs, 99).await;
    cluster
        .batch_execute(&format!(
            "ALTER ROLE \"{WORKER_ROLE}\" PASSWORD '{TENANT_PASSWORD}'"
        ))
        .await
        .expect("the operator supplies the worker's authentication material");

    let mut worker = connect(&cluster_fixture.url_as(WORKER_ROLE, TENANT_PASSWORD)).await;
    let my_schema = database_derivation::schema_name(&mine);
    let their_schema = database_derivation::schema_name(&theirs);
    let my_role = database_derivation::binding_role_name(&my_binding, 0).expect("fits");
    let their_role = database_derivation::binding_role_name(&their_binding, 0).expect("fits");

    // CONTROLS FIRST. Without these the denials below would be satisfied by a
    // cluster that granted nothing at all.
    assert_eq!(
        read_under_role(&mut worker, &my_role, &my_schema)
            .await
            .expect("a live binding reaches its own database"),
        42
    );
    assert_eq!(
        read_under_role(&mut worker, &their_role, &their_schema)
            .await
            .expect("the neighbour's binding reaches the neighbour's database"),
        99
    );

    // One binding, the other database.
    let crossed = read_under_role(&mut worker, &my_role, &their_schema)
        .await
        .expect_err("a binding must not reach a database it does not name");
    assert_eq!(
        server_error(&crossed).code(),
        &SqlState::INSUFFICIENT_PRIVILEGE
    );
    assert_eq!(
        server_error(&crossed).message(),
        format!("permission denied for schema {their_schema}")
    );

    // The worker login with no narrowing at all. This is what `WITH INHERIT
    // FALSE` buys: a statement that forgets to narrow fails closed rather than
    // running with the union of every binding on the login.
    let ambient = worker
        .query(
            &format!("SELECT total FROM \"{my_schema}\".orders WHERE id = 1"),
            &[],
        )
        .await
        .expect_err("a binding's privileges must not be ambient on the shared login");
    assert_eq!(
        server_error(&ambient).code(),
        &SqlState::INSUFFICIENT_PRIVILEGE
    );

    // The DATABASE role, which carries every co-tenant binding's privileges at
    // once. This is what `WITH SET FALSE` buys.
    let database_role =
        database_derivation::capability_role_name(&mine, DatabaseCapability::ReadWrite)
            .expect("fits");
    let assumed = read_under_role(&mut worker, &database_role, &my_schema)
        .await
        .expect_err("the worker must not assume a database role");
    assert_eq!(
        server_error(&assumed).code(),
        &SqlState::INSUFFICIENT_PRIVILEGE
    );
    assert_eq!(
        server_error(&assumed).message(),
        format!("permission denied to set role \"{database_role}\"")
    );
}

/// Revoking withdraws both edges and leaves the role standing.
///
/// The role's survival is the whole arm: the data plane separates a revoked
/// binding from a retired schema epoch by SQLSTATE alone, so a reconciler that
/// dropped the role on revoke would report a terminal refusal as a retryable
/// one.
#[ntex::test]
async fn revoking_withdraws_both_edges_and_keeps_the_role_so_the_refusal_stays_42501() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = control_superuser().await;
    let world = World::new(&pg, "revoke").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let (datastore, _) = pass(&reconciler).await;
    let database = world.declare_database(&pg, &datastore, "shared").await;
    let leaving = world.app(&pg, "leaving").await;
    let staying = world.app(&pg, "staying").await;
    let leaving_binding = world
        .declare_binding(&pg, &leaving, &database, DatabaseCapability::ReadWrite)
        .await;
    let staying_binding = world
        .declare_binding(&pg, &staying, &database, DatabaseCapability::ReadWrite)
        .await;
    pass(&reconciler).await;

    seed_table(&cluster, &database, 7).await;
    cluster
        .batch_execute(&format!(
            "ALTER ROLE \"{WORKER_ROLE}\" PASSWORD '{TENANT_PASSWORD}'"
        ))
        .await
        .expect("the operator supplies the worker's authentication material");
    let mut worker = connect(&cluster_fixture.url_as(WORKER_ROLE, TENANT_PASSWORD)).await;
    let schema = database_derivation::schema_name(&database);
    let leaving_role = database_derivation::binding_role_name(&leaving_binding, 0).expect("fits");
    let staying_role = database_derivation::binding_role_name(&staying_binding, 0).expect("fits");

    // Control: both co-tenants read before anything is withdrawn.
    assert_eq!(
        read_under_role(&mut worker, &leaving_role, &schema)
            .await
            .expect("the binding about to be revoked reads first"),
        7
    );
    assert_eq!(
        read_under_role(&mut worker, &staying_role, &schema)
            .await
            .expect("its co-tenant reads"),
        7
    );

    pg.execute(
        "UPDATE zeroship.database_bindings SET status = 'revoking' WHERE id = $1",
        &[&leaving_binding.as_str()],
    )
    .await
    .expect("declare the revocation");

    let (_, report) = pass(&reconciler).await;
    assert_eq!(
        report.bindings_revoked,
        vec![leaving_binding.clone()],
        "{report:?}"
    );
    let (status, generation, observed, _) = binding_row(&pg, &leaving_binding).await;
    assert_eq!(status, "revoked");
    assert_eq!(observed, generation);

    // THE ROLE SURVIVES, so the refusal is 42501 and not 22023.
    assert!(
        platform_roles(&cluster).await.contains(&leaving_role),
        "a revoked binding keeps its role"
    );
    let refused = read_under_role(&mut worker, &leaving_role, &schema)
        .await
        .expect_err("a revoked binding must not be assumable");
    assert_eq!(
        server_error(&refused).code(),
        &SqlState::INSUFFICIENT_PRIVILEGE,
        "role-exists-but-not-a-member is 42501; 22023 would say the epoch is stale"
    );
    assert_eq!(
        server_error(&refused).message(),
        format!("permission denied to set role \"{leaving_role}\"")
    );

    // The co-tenant is the control for the revoke: it has to bite one binding
    // and only one, or it is a database-wide outage rather than a withdrawal.
    assert_eq!(
        read_under_role(&mut worker, &staying_role, &schema)
            .await
            .expect("revoking one binding must not disturb its co-tenant"),
        7
    );
}

/// The reap drops an undeclared binding role, and ONLY after a complete read.
///
/// The pair is one variable: the same pass, against the same cluster, with the
/// same undeclared role standing, differing only in whether the control login
/// may read `zeroship.database_bindings`. Without the second half the first
/// would pass over a reconciler that never reaps anything at all.
#[ntex::test]
async fn the_reap_drops_an_undeclared_role_only_when_the_declaration_read_completed() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = control_superuser().await;
    let world = World::new(&pg, "reap").await;
    let service = control_as_service().await;
    let reconciler = Reconciler::new(
        ControlStore::new(Arc::clone(&service)),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let (datastore, _) = pass(&reconciler).await;
    let database = world.declare_database(&pg, &datastore, "reaped").await;
    let staying_app = world.app(&pg, "staying").await;
    let leaving_app = world.app(&pg, "leaving").await;
    let staying = world
        .declare_binding(&pg, &staying_app, &database, DatabaseCapability::ReadOnly)
        .await;
    let leaving = world
        .declare_binding(&pg, &leaving_app, &database, DatabaseCapability::ReadOnly)
        .await;
    pass(&reconciler).await;

    let staying_role = database_derivation::binding_role_name(&staying, 0).expect("fits");
    let leaving_role = database_derivation::binding_role_name(&leaving, 0).expect("fits");
    assert!(
        platform_roles(&cluster).await.contains(&leaving_role),
        "the control for the whole arm: the role to be reaped exists first"
    );

    // Unbind DELETEs the row, which is what leaves the role undeclared.
    pg.execute(
        "DELETE FROM zeroship.database_bindings WHERE id = $1",
        &[&leaving.as_str()],
    )
    .await
    .expect("unbind");

    // A login that may do everything the reconciler needs EXCEPT read the
    // binding declarations. One variable.
    let probe = format!("zs_probe_{}", Uuid::new_v4().simple());
    pg.batch_execute(&format!(
        "CREATE ROLE \"{probe}\" LOGIN PASSWORD '{TENANT_PASSWORD}';
         GRANT USAGE ON SCHEMA zeroship TO \"{probe}\";
         GRANT SELECT, INSERT, UPDATE ON zeroship.datastores TO \"{probe}\";
         GRANT SELECT, UPDATE, DELETE ON zeroship.databases TO \"{probe}\";
         GRANT SELECT ON zeroship.execution_zones TO \"{probe}\";"
    ))
    .await
    .expect("create the incomplete-read probe login");
    let mut probe_url = url::Url::parse(&fixture::migrated_url()).expect("the fixture DSN parses");
    probe_url.set_username(&probe).expect("username");
    probe_url
        .set_password(Some(TENANT_PASSWORD))
        .expect("password");
    let blinded = Reconciler::new(
        ControlStore::new(Arc::new(connect(probe_url.as_str()).await)),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let refused = blinded
        .reconcile_once()
        .await
        .expect_err("a pass whose declaration read is refused must not complete");
    assert!(
        matches!(refused, ReconcileError::Control(_)),
        "the failure must be the control-plane read, got: {refused}"
    );
    let roles = platform_roles(&cluster).await;
    assert!(
        roles.contains(&leaving_role),
        "an incomplete declaration read must reap NOTHING: {roles:?}"
    );
    assert!(roles.contains(&staying_role), "{roles:?}");
    assert!(
        roles.contains(&WORKER_ROLE.to_owned()) && roles.contains(&RELAY_ROLE.to_owned()),
        "and it must certainly not sweep the platform logins: {roles:?}"
    );

    // The control: the same pass, with the read completing.
    let (_, report) = pass(&reconciler).await;
    assert_eq!(
        report.roles_reaped,
        vec![leaving_role.clone()],
        "a complete read reaps exactly the undeclared role: {report:?}"
    );
    let roles = platform_roles(&cluster).await;
    assert!(!roles.contains(&leaving_role), "{roles:?}");
    assert!(
        roles.contains(&staying_role),
        "the declared co-tenant survives: {roles:?}"
    );
    assert!(
        roles.contains(&WORKER_ROLE.to_owned()) && roles.contains(&RELAY_ROLE.to_owned()),
        "the platform logins are never reap candidates: {roles:?}"
    );
    assert!(
        report.unattributed_roles.is_empty(),
        "every zs_ role on this cluster is one the composer produced: {report:?}"
    );
}

/// A schema is never destroyed by absence, and is destroyed on a `deleting`
/// declaration.
///
/// The asymmetry is the point. A reaped role is recoverable - the next pass
/// re-grants it - and a dropped schema is not, so an absence may authorize the
/// first and never the second.
#[ntex::test]
async fn an_undeclared_schema_is_reported_and_a_deleting_declaration_removes_it() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = control_superuser().await;
    let world = World::new(&pg, "teardown").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let (datastore, _) = pass(&reconciler).await;
    let database = world.declare_database(&pg, &datastore, "doomed").await;
    pass(&reconciler).await;
    seed_table(&cluster, &database, 5).await;
    let schema = database_derivation::schema_name(&database);
    let migrator = database_derivation::migrator_role_name(&database).expect("fits");

    // THE ABSENCE ARM. The row is gone; the data is not.
    pg.execute(
        "DELETE FROM zeroship.databases WHERE id = $1",
        &[&database.as_str()],
    )
    .await
    .expect("remove the declaring row without declaring a teardown");
    let (_, orphaned) = pass(&reconciler).await;
    assert!(
        orphaned.undeclared_database_objects.contains(&schema),
        "an undeclared schema is REPORTED: {orphaned:?}"
    );
    assert!(
        orphaned.undeclared_database_objects.contains(&migrator),
        "and so is the role that owns it: {orphaned:?}"
    );
    assert!(
        schemas(&cluster).await.contains(&schema),
        "an undeclared schema must still be standing"
    );
    let surviving: i64 = cluster
        .query_one(&format!("SELECT count(*) AS rows FROM \"{schema}\".orders"), &[])
        .await
        .expect("the orphaned schema's table must still be readable")
        .get("rows");
    assert_eq!(surviving, 1, "and its rows must still be there");
    assert!(
        platform_roles(&cluster).await.contains(&migrator),
        "the owner role is not swept either"
    );

    // THE DECLARATION ARM: the same schema, one variable - a row that says it
    // is to go.
    pg.execute(
        "INSERT INTO zeroship.databases \
             (id, project_id, execution_zone_id, datastore_id, name, status) \
         VALUES ($1, $2, $3, $4, 'doomed', 'deleting')",
        &[
            &database.as_str(),
            &world.project,
            &world.zone,
            &datastore,
        ],
    )
    .await
    .expect("declare the teardown");
    let (_, torn_down) = pass(&reconciler).await;
    assert_eq!(
        torn_down.databases_deleted,
        vec![database.clone()],
        "{torn_down:?}"
    );
    assert!(
        !schemas(&cluster).await.contains(&schema),
        "a declared teardown removes the schema"
    );
    assert!(
        !platform_roles(&cluster).await.contains(&migrator),
        "and its roles, after it"
    );
    assert_eq!(
        cluster_epoch(&cluster, &database).await,
        None,
        "and its epoch row"
    );
    assert_eq!(
        database_status(&pg, &database).await,
        None,
        "and only then the control row"
    );
}

/// A `deleting` database an app still binds is refused, not destroyed.
#[ntex::test]
async fn a_deleting_database_an_app_still_binds_is_refused() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = control_superuser().await;
    let world = World::new(&pg, "bound-teardown").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let (datastore, _) = pass(&reconciler).await;
    let database = world.declare_database(&pg, &datastore, "bound").await;
    let app = world.app(&pg, "holder").await;
    world
        .declare_binding(&pg, &app, &database, DatabaseCapability::ReadWrite)
        .await;
    pass(&reconciler).await;
    seed_table(&cluster, &database, 3).await;
    let schema = database_derivation::schema_name(&database);

    pg.execute(
        "UPDATE zeroship.databases SET status = 'deleting' WHERE id = $1",
        &[&database.as_str()],
    )
    .await
    .expect("declare the teardown while a binding still stands");

    let (_, report) = pass(&reconciler).await;
    assert!(
        report.databases_deleted.is_empty(),
        "a bound database must not be destroyed: {report:?}"
    );
    assert_eq!(report.failures.len(), 1, "{report:?}");
    assert!(
        report.failures[0].contains("still bound by"),
        "the refusal must name the binding that holds it: {report:?}"
    );
    assert!(
        schemas(&cluster).await.contains(&schema),
        "the schema is untouched"
    );
    assert_eq!(
        database_status(&pg, &database).await,
        Some("deleting".to_owned()),
        "and the declaration stands, so an operator can unbind and retry"
    );

    // The control: unbind, and the same declaration is honoured.
    pg.execute(
        "DELETE FROM zeroship.database_bindings WHERE database_id = $1",
        &[&database.as_str()],
    )
    .await
    .expect("unbind");
    let (_, after) = pass(&reconciler).await;
    assert_eq!(after.databases_deleted, vec![database.clone()], "{after:?}");
    assert!(!schemas(&cluster).await.contains(&schema));
}

/// A worker login that already holds a database role directly stops the pass.
///
/// One such membership carries every co-tenant binding's privileges on that
/// database at once and survives revoking any single binding, and `PostgreSQL`
/// reports nothing unusual about it.
#[ntex::test]
async fn a_direct_worker_membership_in_a_database_role_refuses_the_pass() {
    let cluster_fixture = tenant::Cluster::start();
    let cluster = connect(cluster_fixture.url()).await;
    let pg = control_superuser().await;
    let world = World::new(&pg, "posture").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let (datastore, _) = pass(&reconciler).await;
    let database = world.declare_database(&pg, &datastore, "posture").await;
    let app = world.app(&pg, "posture").await;
    let binding = world
        .declare_binding(&pg, &app, &database, DatabaseCapability::ReadWrite)
        .await;
    // The control: with the fence intact the same pass converges.
    let (_, converged) = pass(&reconciler).await;
    assert_eq!(converged.bindings_activated, vec![binding], "{converged:?}");

    let database_role =
        database_derivation::capability_role_name(&database, DatabaseCapability::ReadWrite)
            .expect("fits");
    cluster
        .batch_execute(&format!(
            "GRANT \"{database_role}\" TO \"{WORKER_ROLE}\""
        ))
        .await
        .expect("the membership PostgreSQL will not complain about");

    let refused = reconciler
        .reconcile_once()
        .await
        .expect_err("a pass must not converge onto an already open fence");
    let text = refused.to_string();
    assert!(text.contains(&database_role), "{text}");
    assert!(text.contains(WORKER_ROLE), "{text}");
    let (_, last_error) = datastore_row(&pg, &datastore).await;
    assert_eq!(
        last_error.as_deref().map(str::to_owned),
        Some(text),
        "the datastore row records why its cluster stopped converging"
    );
}

/// A cluster below the version floor is refused by VERSION, and nothing is
/// bootstrapped onto it.
///
/// The refusal is not left to the grant syntax failing. Below 16
/// `pg_auth_members` carries no `inherit_option` and no `set_option`, so the
/// fence would not be weaker there, it would be absent - and an operator adding
/// capacity would read a confusing SQL error instead of the reason.
#[ntex::test]
async fn a_cluster_below_the_version_floor_is_refused_and_never_bootstrapped() {
    let (image, tag) = tenant::PRE_FENCE_IMAGE;
    let cluster_fixture = tenant::Cluster::of(image, tag);
    let cluster = connect(cluster_fixture.url()).await;
    let pg = control_superuser().await;
    let world = World::new(&pg, "old-major").await;
    let reconciler = Reconciler::new(
        ControlStore::new(control_as_service().await),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    // The control for the whole arm: this really is a server below the floor,
    // and its catalog really does lack the columns the fence is recorded in.
    let version: i32 = cluster
        .query_one("SELECT current_setting('server_version_num')::int AS v", &[])
        .await
        .expect("the fixture server reports its version")
        .get("v");
    assert!(version < 160_000, "the fixture must be below the floor");
    let fence_columns: i64 = cluster
        .query_one(
            "SELECT count(*) AS columns FROM pg_attribute \
              WHERE attrelid = 'pg_auth_members'::regclass \
                AND attname IN ('inherit_option', 'set_option')",
            &[],
        )
        .await
        .expect("read the membership catalog's shape")
        .get("columns");
    assert_eq!(
        fence_columns, 0,
        "below 16 the membership options the fence rides on do not exist"
    );

    let refused = reconciler
        .reconcile_once()
        .await
        .expect_err("a cluster below the floor must be refused");
    assert!(
        matches!(refused, ReconcileError::UnsupportedServerVersion(_)),
        "the refusal must be about the version, got: {refused}"
    );
    let text = refused.to_string();
    assert!(text.contains(&version.to_string()), "{text}");
    assert!(text.contains("160000"), "{text}");

    // The cluster is registered so an operator can see it, and stays out of
    // rotation with the reason on the row.
    let row = pg
        .query_one(
            "SELECT id, status, last_error FROM zeroship.datastores \
              WHERE execution_zone_id = $1",
            &[&world.zone],
        )
        .await
        .expect("the refused cluster is still registered");
    assert_eq!(row.get::<_, String>("status"), "pending");
    assert_eq!(row.get::<_, Option<String>>("last_error"), Some(text));

    // And nothing was bootstrapped onto it.
    assert!(
        platform_roles(&cluster).await.is_empty(),
        "a refused cluster gets no platform login"
    );
    assert!(
        !schemas(&cluster).await.contains(&ADMIN_SCHEMA.to_owned()),
        "and no platform schema"
    );
}

/// With more than one zone declared, an unconfigured service refuses to choose.
///
/// A cluster may register itself because reaching it proves it exists. A ZONE
/// proves nothing - it gates which join signers may mint workers - so a service
/// that invented one would undo that rule.
#[ntex::test]
async fn an_unconfigured_zone_refuses_to_choose_among_several() {
    let cluster_fixture = tenant::Cluster::start();
    let pg = control_superuser().await;
    // Two of this test's own, so the ambiguity holds whatever else is declared.
    let first = World::new(&pg, "zone-a").await;
    World::new(&pg, "zone-b").await;
    let service = control_as_service().await;
    let store = ControlStore::new(Arc::clone(&service));

    assert!(
        matches!(
            store.sole_active_zone().await.expect("read the zone table"),
            SoleZone::Many(_)
        ),
        "the fixture must make the choice ambiguous, or the refusal below is vacuous"
    );

    let unconfigured = Reconciler::new(
        ControlStore::new(Arc::clone(&service)),
        cluster_fixture.url(),
        None,
    );
    let refused = unconfigured
        .reconcile_once()
        .await
        .expect_err("an ambiguous zone must not be chosen");
    assert!(
        matches!(refused, ReconcileError::Zone(_)),
        "got: {refused}"
    );
    assert!(
        refused.to_string().contains("migrate_server.execution_zone"),
        "the refusal must name the setting that resolves it: {refused}"
    );

    // The control: one variable - the zone is configured - and the same
    // reconciler against the same cluster completes.
    let configured = Reconciler::new(
        ControlStore::new(service),
        cluster_fixture.url(),
        Some(first.zone.clone()),
    );
    let (datastore, report) = pass(&configured).await;
    assert!(report.registered, "{report:?}");
    assert_eq!(
        datastore_row(&pg, &datastore).await.0,
        "active",
        "a configured zone registers and bootstraps"
    );
}

/// One cluster is one row, however many services hold its credential.
///
/// Registration is keyed on the cluster's own identity rather than a chosen
/// name, so this is idempotent by construction - and a second service declaring
/// a DIFFERENT zone for it is a refusal rather than a silent move of every
/// database on it.
#[ntex::test]
async fn two_services_on_one_cluster_converge_on_one_row_and_a_zone_change_is_refused() {
    let cluster_fixture = tenant::Cluster::start();
    let pg = control_superuser().await;
    let world = World::new(&pg, "identity").await;
    let elsewhere = World::new(&pg, "elsewhere").await;
    let service = control_as_service().await;

    let first = Reconciler::new(
        ControlStore::new(Arc::clone(&service)),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );
    let second = Reconciler::new(
        ControlStore::new(Arc::clone(&service)),
        cluster_fixture.url(),
        Some(world.zone.clone()),
    );

    let (one, first_report) = pass(&first).await;
    assert!(first_report.registered);
    let (two, second_report) = pass(&second).await;
    assert_eq!(one, two, "two services on one cluster name one datastore");
    assert!(
        !second_report.registered,
        "the second service inserts nothing: {second_report:?}"
    );
    let rows: i64 = pg
        .query_one(
            "SELECT count(*) AS rows FROM zeroship.datastores WHERE execution_zone_id = $1",
            &[&world.zone],
        )
        .await
        .expect("count this test's registry rows")
        .get("rows");
    assert_eq!(rows, 1, "one cluster, one row");

    // A service configured against this cluster but declaring another zone.
    let misplaced = Reconciler::new(
        ControlStore::new(service),
        cluster_fixture.url(),
        Some(elsewhere.zone.clone()),
    );
    let refused = misplaced
        .reconcile_once()
        .await
        .expect_err("a datastore does not change zones");
    let text = refused.to_string();
    assert!(text.contains(&world.zone), "{text}");
    assert!(text.contains(&elsewhere.zone), "{text}");
    assert_eq!(
        datastore_row(&pg, &one).await.0,
        "active",
        "and the standing row is not moved"
    );
}
