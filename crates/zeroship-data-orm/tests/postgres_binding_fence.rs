//! The runtime fence, end to end: an ORM session narrows to the binding role a
//! real reconciler granted, reaches its own schema, and is refused everything
//! else.
//!
//! **Nothing here hand-rolls a role.** The ladder is created by
//! `zeroship_migrate_server::datastore::cluster` - the same functions the
//! per-cluster reconciler calls - and the session is opened by
//! `zeroship_data_orm::orm::Database`, the same type a creator dispatch runs
//! through. If the two composed different names, every arm below would fail at
//! session setup instead of measuring what it is about.
//!
//! **Every denial carries its control.** A refusal arm over a cluster that
//! granted nothing passes for the wrong reason, so each one is paired with the
//! permitted case it differs from in a single variable.
//!
//! **The server is a throwaway container, and that is not a convenience.**
//! `pg_authid` and `pg_auth_members` are cluster-shared, so the role DDL below
//! would be visible to every database on a shared instance.

#[path = "../../../tests/fixtures/postgres/mod.rs"]
mod postgres_fixture;

use compio_postgres::error::SqlState;
use compio_postgres::{Client, NoTls};

use zeroship_core::database_derivation;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{BindingId, DatabaseId};
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::encryption::ProjectKeySource;
use zeroship_data_orm::error::{DbError, GRANT_REVOKED, SCHEMA_EPOCH_STALE};
use zeroship_data_orm::orm::{Database, Output};
use zeroship_data_orm::schema::{CollectionSchema, ColumnSchema, LogicalType, Schema};
use zeroship_data_orm::value;
use zeroship_migrate_server::apply::WORKER_ROLE;
use zeroship_migrate_server::datastore::cluster;

/// The password the fixture gives the worker login. The container is thrown
/// away with the test.
const WORKER_PASSWORD: &str = "fixture";

/// The epoch the reconciler converges these databases at.
const LIVE_EPOCH: i32 = 1;

/// The deploy pin is `postgres:16` (`deploy/compose/docker-compose.yml`), and
/// the grant options the whole ladder rests on do not exist below it.
const MINIMUM_SERVER_VERSION_NUM: i32 = 160_000;

/// Connect a raw client and drive its protocol loop.
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

/// Refuse a server older than the pin before measuring anything.
async fn require_pinned_major(admin: &Client) {
    let version: i32 = admin
        .query_one("SELECT current_setting('server_version_num')::int", &[])
        .await
        .expect("the server reports its version")
        .get(0);
    assert!(
        version >= MINIMUM_SERVER_VERSION_NUM,
        "this fence describes PostgreSQL {MINIMUM_SERVER_VERSION_NUM} and above; \
         the fixture server reports server_version_num {version}"
    );
}

/// The collection both databases declare.
///
/// Two databases declaring the SAME collection name is deliberate: it is what
/// makes the schema, rather than the collection, the thing that separates them.
fn orders_schema() -> Schema {
    let mut id = ColumnSchema::new(LogicalType::Integer);
    id.primary_key = true;
    let total = ColumnSchema::new(LogicalType::Integer);
    Schema::new(vec![(
        "orders".into(),
        CollectionSchema::new([("id".into(), id), ("total".into(), total)]),
    )])
}

/// What an APPLY does over a converged schema, not what the reconciler does.
///
/// The reconciler mints the roles and grants `USAGE` on the schema; which
/// COLUMNS a capability may touch comes from the owner's own migration IR.
/// Spelling it here keeps that boundary visible rather than hiding it behind a
/// reconciler that quietly granted table-wide privileges.
async fn seed_table(admin: &Client, database: &DatabaseId, total: i32) {
    let schema = database_derivation::schema_name(database);
    let readwrite =
        database_derivation::capability_role_name(database, DatabaseCapability::ReadWrite)
            .expect("the fixture capability role name fits");
    admin
        .batch_execute(&format!(
            "CREATE TABLE \"{schema}\".orders (id int PRIMARY KEY, total int NOT NULL);
             INSERT INTO \"{schema}\".orders VALUES (1, {total});
             GRANT SELECT (id, total), INSERT (id, total), UPDATE (id, total) \
                 ON \"{schema}\".orders TO \"{readwrite}\";"
        ))
        .await
        .expect("an apply's column grants over a converged schema");
}

/// Everything the reconciler does to a cluster for one (app, database) pair.
async fn converge(
    admin: &mut Client,
    database: &DatabaseId,
    binding: &BindingId,
) {
    let epoch = cluster::converge_database(admin, database, LIVE_EPOCH)
        .await
        .expect("the reconciler converges the database");
    assert_eq!(epoch, LIVE_EPOCH, "the cluster records the declared epoch");
    cluster::grant_binding(
        admin,
        binding,
        database,
        DatabaseCapability::ReadWrite,
        LIVE_EPOCH,
    )
    .await
    .expect("the reconciler grants the binding's two edges");
}

/// The URL the worker login opens, built from the fixture's own.
fn worker_url(base: &str) -> String {
    let mut url = url::Url::parse(base).expect("the fixture URL parses");
    url.set_username(WORKER_ROLE)
        .expect("the fixture URL accepts a username");
    url.set_password(Some(WORKER_PASSWORD))
        .expect("the fixture URL accepts a password");
    url.to_string()
}

/// Read the seeded row through the ORM, under `binding`.
///
/// This is the whole point of the target: the statement is qualified and the
/// session narrowed by the ORM from the binding, not by this fixture.
async fn read_total(url: &str, binding: DbBinding) -> Result<i64, DbError> {
    let database = Database::connect(
        binding,
        zeroship_data_orm::ConnectOptions::new(url, ProjectKeySource::unavailable()),
        orders_schema(),
    )
    .await?;
    let found = database
        .collection("orders")?
        .find(value!({ "id": 1 }), value!({}))
        .await?;
    let Output::Rows { rows, .. } = found else {
        panic!("find must return rows");
    };
    assert_eq!(rows.len(), 1, "the seeded row must be present to be read");
    rows[0]["total"]
        .as_i64()
        .ok_or_else(|| DbError::internal("total decoded as something other than an integer"))
}

/// Bring a cluster to the state every arm below starts from: bootstrapped, two
/// converged databases with a seeded table each, two granted bindings, and a
/// worker login that can authenticate.
struct Fence {
    url: String,
    mine: DatabaseId,
    theirs: DatabaseId,
    my_edge: BindingId,
    their_edge: BindingId,
    admin: Client,
}

impl Fence {
    async fn build(url: String) -> Self {
        let mut admin = connect(&url).await;
        require_pinned_major(&admin).await;
        cluster::apply_bootstrap_corpus(&admin)
            .await
            .expect("the datastore bootstrap corpus applies");

        let mine = DatabaseId::mint();
        let theirs = DatabaseId::mint();
        let my_edge = BindingId::mint();
        let their_edge = BindingId::mint();
        converge(&mut admin, &mine, &my_edge).await;
        converge(&mut admin, &theirs, &their_edge).await;
        seed_table(&admin, &mine, 42).await;
        seed_table(&admin, &theirs, 99).await;

        admin
            .batch_execute(&format!(
                "ALTER ROLE \"{WORKER_ROLE}\" PASSWORD '{WORKER_PASSWORD}'"
            ))
            .await
            .expect("the operator supplies the worker's authentication material");

        Self {
            url: worker_url(&url),
            mine,
            theirs,
            my_edge,
            their_edge,
            admin,
        }
    }

    fn binding_at(&self, database: &DatabaseId, edge: &BindingId, epoch: u32) -> DbBinding {
        DbBinding::to_database("app_fence", "deploy_fence", database.clone(), edge.clone(), epoch)
            .expect("the fixture ids compose a legal role name")
    }

    fn mine(&self) -> DbBinding {
        self.binding_at(&self.mine, &self.my_edge, LIVE_EPOCH as u32)
    }

    fn theirs(&self) -> DbBinding {
        self.binding_at(&self.theirs, &self.their_edge, LIVE_EPOCH as u32)
    }
}

/// Release the driver's sockets before the container that serves them.
async fn drain() {
    assert!(
        compio_postgres::drain_connections(std::time::Duration::from_secs(10)).await,
        "the test's PostgreSQL connections must close before the container stops"
    );
}

/// **The property.** A session narrowed to a binding reaches its own schema and
/// is refused another's with `42501`.
///
/// The two controls are the two permitted reads. Without them a cluster whose
/// grants were never issued would satisfy the refusal below.
#[compio::test]
async fn a_narrowed_session_reaches_its_own_database_and_is_refused_its_neighbours() {
    let postgres = postgres_fixture::Postgres::start();
    let fence = Fence::build(postgres.url()).await;

    // CONTROL: each binding reads its own database through the ORM.
    assert_eq!(
        read_total(&fence.url, fence.mine())
            .await
            .expect("a live binding reaches its own database"),
        42
    );
    assert_eq!(
        read_total(&fence.url, fence.theirs())
            .await
            .expect("the neighbour's binding reaches the neighbour's database"),
        99
    );

    // THE SUBJECT, differing in one variable: the same edge, the neighbour's
    // database. The session narrows to a role that holds nothing on that
    // schema, so PostgreSQL refuses the statement.
    let crossed = read_total(
        &fence.url,
        fence.binding_at(&fence.theirs, &fence.my_edge, LIVE_EPOCH as u32),
    )
    .await
    .expect_err("a binding must not reach a database it does not name");
    assert!(
        crossed.message_str().contains("permission denied for schema"),
        "the refusal must be PostgreSQL's schema denial, not a local check: {crossed}"
    );

    drop(fence);
    drain().await;
}

/// A revoked binding is a terminal `GRANT_REVOKED`, not a retryable one.
///
/// The reconciler withdraws both edges and leaves the role standing precisely
/// so the data plane can tell a revocation from a retired epoch by SQLSTATE
/// alone. Its control is the same read before the revoke.
#[compio::test]
async fn a_revoked_binding_is_reported_as_a_terminal_grant_refusal() {
    let postgres = postgres_fixture::Postgres::start();
    let fence = Fence::build(postgres.url()).await;

    // CONTROL: the binding reads before anything is revoked.
    assert_eq!(
        read_total(&fence.url, fence.mine())
            .await
            .expect("a live binding reaches its own database"),
        42
    );

    cluster::revoke_binding(
        &fence.admin,
        &fence.my_edge,
        &fence.mine,
        DatabaseCapability::ReadWrite,
        LIVE_EPOCH,
    )
    .await
    .expect("the reconciler withdraws both of the binding's edges");

    let refused = read_total(&fence.url, fence.mine())
        .await
        .expect_err("a revoked binding must be refused");
    assert_eq!(
        refused.code(),
        GRANT_REVOKED,
        "a revoked binding is terminal and must not be reported as a retired epoch: {refused}"
    );

    // The role survived the revoke, which is what keeps the two refusals
    // distinguishable. Read it from the catalog rather than inferring it.
    let role = database_derivation::binding_role_name(&fence.my_edge, LIVE_EPOCH as u32)
        .expect("the fixture role name fits");
    let rows = fence
        .admin
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
        .await
        .expect("read the role catalog");
    assert_eq!(rows.len(), 1, "revoking must not drop the binding role");

    // The neighbour is untouched: the revoke reached one edge, not the login.
    assert_eq!(
        read_total(&fence.url, fence.theirs())
            .await
            .expect("the neighbour's binding is unaffected by another's revoke"),
        99
    );

    drop(fence);
    drain().await;
}

/// An isolate built against a retired epoch is told its shape moved.
///
/// The epoch is the last component of the role name, so a binding at an epoch
/// the cluster never minted names a role that does not exist - `22023`, which
/// is retryable and re-resolvable, not the terminal `42501` above. Its control
/// is the same edge at the live epoch.
#[compio::test]
async fn a_binding_at_an_epoch_the_cluster_never_minted_is_reported_as_stale() {
    let postgres = postgres_fixture::Postgres::start();
    let fence = Fence::build(postgres.url()).await;

    // CONTROL: the live epoch reads.
    assert_eq!(
        read_total(&fence.url, fence.mine())
            .await
            .expect("the live epoch reaches the database"),
        42
    );

    let stale = read_total(
        &fence.url,
        fence.binding_at(&fence.mine, &fence.my_edge, LIVE_EPOCH as u32 + 1),
    )
    .await
    .expect_err("an epoch the cluster never minted must be refused");
    assert_eq!(
        stale.code(),
        SCHEMA_EPOCH_STALE,
        "a role that does not exist is a re-resolvable epoch condition, \
         not a terminal grant refusal: {stale}"
    );

    drop(fence);
    drain().await;
}

/// The worker login carries no ambient authority over a converged database.
///
/// This is what `WITH INHERIT FALSE` buys and what the whole narrowing rests
/// on: a statement that reached the cluster without narrowing fails closed
/// rather than running with the union of every binding on the shared login. Its
/// control is the same statement under the binding role.
#[compio::test]
async fn the_shared_worker_login_reaches_no_converged_schema_without_narrowing() {
    let postgres = postgres_fixture::Postgres::start();
    let fence = Fence::build(postgres.url()).await;
    let schema = database_derivation::schema_name(&fence.mine);

    let worker = connect(&fence.url).await;
    let ambient = worker
        .query(
            &format!("SELECT total FROM \"{schema}\".orders WHERE id = 1"),
            &[],
        )
        .await
        .expect_err("a binding's privileges must not be ambient on the shared login");
    assert_eq!(
        ambient
            .as_db_error()
            .expect("the server refused rather than the transport")
            .code(),
        &SqlState::INSUFFICIENT_PRIVILEGE
    );

    // CONTROL: the same row, through the ORM, under the binding.
    assert_eq!(
        read_total(&fence.url, fence.mine())
            .await
            .expect("the binding reaches what the bare login cannot"),
        42
    );

    drop(worker);
    drop(fence);
    drain().await;
}

/// One app holds a transaction on each of its two databases at once.
///
/// **This is the lane key, measured as behaviour.** A lane keyed on the tenant
/// alone would make the second database's top-level transaction a re-entrant
/// claim on the first's lane - refused with `nested_top_level_transaction` -
/// because the first's callback is being polled when the second asks. Keyed on
/// the tenant AND the database, the two are different lanes.
///
/// Its control is the read each transaction performs: without them a pair of
/// claims that succeeded and then reached nothing would pass.
#[compio::test]
async fn one_app_holds_a_transaction_on_each_of_its_two_databases() {
    let postgres = postgres_fixture::Postgres::start();
    let fence = Fence::build(postgres.url()).await;

    let to_mine = Database::connect(
        fence.mine(),
        zeroship_data_orm::ConnectOptions::new(&fence.url, ProjectKeySource::unavailable()),
        orders_schema(),
    )
    .await
    .expect("open the first database");
    // The SAME app, its second binding. `Fence::binding_at` stamps one tenant
    // on both, so the only thing separating these handles is the database.
    // ON THE FIRST HANDLE'S CONTEXT, which is what makes this arm measure the
    // key. Transaction lanes live on the context, so two handles built through
    // `Database::connect` would hold two lane maps and never contend at all -
    // and a worker thread holds ONE context for every database an app reaches.
    let second_binding = fence.binding_at(&fence.theirs, &fence.their_edge, LIVE_EPOCH as u32);
    let second_backend =
        zeroship_data_orm::ConnectOptions::new(&fence.url, ProjectKeySource::unavailable())
            .connect()
            .await
            .expect("open the second backend");
    let to_theirs = to_mine
        .context()
        .with(|| {
            zeroship_data_orm::descriptor::install_collections(&second_binding, orders_schema())?;
            Ok::<_, DbError>(Database::new(
                to_mine.context().clone(),
                second_binding,
                second_backend,
            ))
        })
        .expect("install the second database's schema on the shared context");
    assert_eq!(
        to_mine.binding().app_id(),
        to_theirs.binding().app_id(),
        "the control for the claim below: one tenant, two databases"
    );

    let inner = to_mine
        .transaction(|first| async move {
            let Output::Rows { rows, .. } = first
                .collection("orders")?
                .find(value!({ "id": 1 }), value!({}))
                .await?
            else {
                panic!("find must return rows");
            };
            assert_eq!(rows[0]["total"].as_i64(), Some(42));

            // A top-level transaction on the OTHER database, opened while this
            // callback is being polled.
            to_theirs
                .transaction(|second| async move {
                    let Output::Rows { rows, .. } = second
                        .collection("orders")?
                        .find(value!({ "id": 1 }), value!({}))
                        .await?
                    else {
                        panic!("find must return rows");
                    };
                    Ok::<_, DbError>(rows[0]["total"].as_i64())
                })
                .await
        })
        .await
        .expect("a second database's transaction must not collide with the first's lane");
    assert_eq!(
        inner,
        Some(99),
        "the second transaction must read its OWN database"
    );

    drop(to_mine);
    drop(fence);
    drain().await;
}
