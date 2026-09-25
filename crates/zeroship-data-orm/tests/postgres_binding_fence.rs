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
use zeroship_data_orm::driver::Session;
use zeroship_data_orm::error::{BeginIntent, DbError, GRANT_REVOKED};
use zeroship_data_orm::orm::{Database, Output};
use zeroship_data_orm::schema::{CollectionSchema, ColumnSchema, LogicalType, Schema};
use zeroship_data_orm::orm::Value;
use zeroship_data_orm::value;
use zeroship_migrate_server::apply::WORKER_ROLE;
use zeroship_migrate_server::datastore::cluster;

/// The password the fixture gives the worker login. The container is thrown
/// away with the test.
const WORKER_PASSWORD: &str = "fixture";

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
    cluster::converge_database(admin, database)
        .await
        .expect("the reconciler converges the database");
    cluster::grant_binding(admin, binding, database, DatabaseCapability::ReadWrite)
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

    fn binding_at(&self, database: &DatabaseId, edge: &BindingId) -> DbBinding {
        DbBinding::to_database(
            "app_fence",
            "deploy_fence",
            database.clone(),
            edge.clone(),
            DatabaseCapability::ReadWrite,
        )
        .expect("the fixture ids compose a legal role name")
    }

    fn mine(&self) -> DbBinding {
        self.binding_at(&self.mine, &self.my_edge)
    }

    fn theirs(&self) -> DbBinding {
        self.binding_at(&self.theirs, &self.their_edge)
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
        fence.binding_at(&fence.theirs, &fence.my_edge),
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

/// A revoked binding is a terminal `GRANT_REVOKED`, and its ROLE SURVIVES.
///
/// The reconciler withdraws both edges and leaves the role standing precisely
/// so the refusal stays `42501` - the role exists and this session may not
/// assume it - rather than the `22023` a dropped role would produce. Its
/// control is the same read before the revoke, and the catalog read below is
/// what distinguishes a withdrawn membership from a removed object.
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
    )
    .await
    .expect("the reconciler withdraws both of the binding's edges");

    let refused = read_total(&fence.url, fence.mine())
        .await
        .expect_err("a revoked binding must be refused");
    assert_eq!(
        refused.code(),
        GRANT_REVOKED,
        "a revoked binding is terminal and must be reported as the withdrawn \
         membership it is: {refused}"
    );

    // The role survived the revoke, which is what keeps the refusal `42501`.
    // Read it from the catalog rather than inferring it.
    let role = database_derivation::binding_role_name(&fence.my_edge)
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
    let second_binding = fence.binding_at(&fence.theirs, &fence.their_edge);
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

/// A change on one of an app's two databases reaches that database's
/// subscriber and NOT the other's.
///
/// **This is the CDC routing key, measured as behaviour.** Both databases
/// declare a collection of the same name (`orders`, per [`orders_schema`]) and
/// both bindings carry the same tenant, so the app id and the collection are
/// identical on the two subscriptions: the DATABASE is the only thing that can
/// tell them apart. A broker keyed on `(app_id, collection)` alone puts both
/// subscriptions in one bucket and delivers every change to both - and the
/// design names this the one re-key whose omission is silent rather than loud,
/// because an extra event is indistinguishable from a legitimate one at the
/// subscriber.
///
/// **The absence is green for free unless the scenario really arose**, so
/// nothing here is assumed:
///
/// - both bindings' app ids are compared TO EACH OTHER, not each found
///   non-empty, and their database ids are compared to each other too;
/// - both really reach their own database, proven by a read through the ORM
///   under each binding, which is what "live binding" means on a cluster the
///   reconciler converged - a binding whose grant is missing fails at session
///   setup instead;
/// - both really declare `orders`, proven by the read each one performs and by
///   comparing the two subscriptions' collection names to each other;
/// - the POSITIVE delivery is asserted in BOTH directions, so an arm where
///   nothing was ever published cannot pass.
#[compio::test]
async fn one_app_s_two_databases_do_not_cross_deliver_a_shared_collection_name() {
    use zeroship_data_orm::cdc::broker::{self, SubscriptionMessage};

    let postgres = postgres_fixture::Postgres::start();
    let fence = Fence::build(postgres.url()).await;

    let to_mine = Database::connect(
        fence.mine(),
        zeroship_data_orm::ConnectOptions::new(&fence.url, ProjectKeySource::unavailable()),
        orders_schema(),
    )
    .await
    .expect("open the first database");
    let to_theirs = Database::connect(
        fence.theirs(),
        zeroship_data_orm::ConnectOptions::new(&fence.url, ProjectKeySource::unavailable()),
        orders_schema(),
    )
    .await
    .expect("open the second database");

    // PRECONDITION 1 - one tenant. Compared to each other; a pair of
    // non-empty app ids would say nothing.
    assert_eq!(
        to_mine.binding().app_id(),
        to_theirs.binding().app_id(),
        "the arm is about ONE app reaching two databases"
    );
    // PRECONDITION 2 - two databases, and two physical schemas derived from
    // them. Equal ids would make the whole arm vacuous.
    assert_ne!(
        to_mine.binding().database(),
        to_theirs.binding().database(),
        "the two bindings must address DIFFERENT databases"
    );
    assert_ne!(
        to_mine.binding().schema().as_str(),
        to_theirs.binding().schema().as_str(),
        "two databases derive two physical schemas"
    );

    // PRECONDITION 3 - the app holds a LIVE binding to each, and each database
    // really declares `orders`. A read through the ORM proves both at once: the
    // session narrows with the binding role the reconciler granted, and the
    // statement is qualified with that binding's own schema. The two seeded
    // totals differ, so each read also proves it reached ITS OWN database.
    assert_eq!(
        read_total(&fence.url, fence.mine())
            .await
            .expect("the first binding is live and its database declares orders"),
        42
    );
    assert_eq!(
        read_total(&fence.url, fence.theirs())
            .await
            .expect("the second binding is live and its database declares orders"),
        99
    );

    let my_route = to_mine.binding().route();
    let their_route = to_theirs.binding().route();
    let mine = broker::subscribe(&my_route, "orders");
    let theirs = broker::subscribe(&their_route, "orders");
    // PRECONDITION 4 - the two subscriptions name the SAME collection. Without
    // this the databases would be separated by the collection rather than by
    // the routing key, and the arm would measure nothing.
    assert_eq!(
        mine.collection(),
        theirs.collection(),
        "both databases declare a collection of the same name"
    );

    // A write on the FIRST database.
    to_mine
        .collection("orders")
        .expect("the first database declares orders")
        .insert(value!({ "id": 2, "total": 7 }))
        .await
        .expect("insert into the first database");

    match mine.pop() {
        Some(SubscriptionMessage::Change(event)) => {
            assert_eq!(event.collection, "orders");
            assert_eq!(event.route, my_route);
            assert_eq!(event.pk.as_deref(), Some("2"));
        }
        other => panic!("the writing database's subscriber must receive its change; got {other:?}"),
    }
    assert!(
        theirs.pop().is_none(),
        "a change on the first database must NOT reach the second database's subscription"
    );

    // The mirror, so neither direction can be right by accident.
    to_theirs
        .collection("orders")
        .expect("the second database declares orders")
        .insert(value!({ "id": 3, "total": 11 }))
        .await
        .expect("insert into the second database");

    match theirs.pop() {
        Some(SubscriptionMessage::Change(event)) => {
            assert_eq!(event.collection, "orders");
            assert_eq!(event.route, their_route);
            assert_eq!(event.pk.as_deref(), Some("3"));
        }
        other => panic!("the writing database's subscriber must receive its change; got {other:?}"),
    }
    assert!(
        mine.pop().is_none(),
        "a change on the second database must NOT reach the first database's subscription"
    );

    mine.close();
    theirs.close();
    drop(to_mine);
    drop(to_theirs);
    drop(fence);
    drain().await;
}

// ---------------------------------------------------------------------------
// The audited raw-column read
// ---------------------------------------------------------------------------
//
// A masked field occupies two physical columns and the real value lives in the
// `__zs_raw__` sibling. `zeroship_migrate_server::capability_grants` withholds
// that sibling from BOTH capability roles, so the read an unmask performs is
// not one the session's own binding role can make: it has to assume the
// database's unmask role for exactly that statement and narrow straight back.
//
// Everything below runs on a cluster the real reconciler converged and against
// the grants the real apply-time converger emits, so an arm cannot pass because
// a fixture granted the column it is about.

use zeroship_data_orm::schema::MaskSchema;
use zeroship_data_orm::sql::mapping::raw_column_name;
use zeroship_migrate_server::{capability_grants, provisioning};

/// The collection whose real values the arms below reach.
const MASKED_COLLECTION: &str = "people";

/// The seeded row's identity and its two real values.
const ROW_PK: &str = "p1";
const REAL_SSN: &str = "123-45-6789";
const MASKED_SSN: &str = "***-**-6789";

/// One masked string field, one masked NUMBER field, and one ordinary column.
///
/// `amount`'s real value is stored as `NaN`, which the server returns happily
/// and the row decoder refuses. It is the only failure shape that leaves the
/// creator's transaction LIVE, so it is the one that can observe a raw read
/// that left the session elevated -
/// `a_failed_raw_read_leaves_the_creators_next_statement_on_the_binding_role`
/// is what it exists for. `nickname` is the non-raw column the unmask role must
/// not reach.
fn people_schema() -> Schema {
    let mut id = ColumnSchema::new(LogicalType::Text);
    id.primary_key = true;
    let mut ssn = ColumnSchema::new(LogicalType::Text);
    ssn.required = false;
    ssn.mask = Some(MaskSchema {
        kind: "last4".into(),
        classification: "spi".into(),
    });
    let mut nickname = ColumnSchema::new(LogicalType::Text);
    nickname.required = false;
    let mut amount = ColumnSchema::new(LogicalType::Number);
    amount.required = false;
    amount.mask = Some(MaskSchema {
        kind: "full".into(),
        classification: "spi".into(),
    });
    Schema::new(vec![(
        MASKED_COLLECTION.into(),
        CollectionSchema::new([
            ("id".into(), id),
            ("ssn".into(), ssn),
            ("nickname".into(), nickname),
            ("amount".into(), amount),
        ]),
    )])
}

/// The policy that lets the fixture's actor see `spi`.
///
/// Installed explicitly rather than relying on the no-policy fallback: that
/// fallback grants the reserved `auto` kind, and `sanitize_app_actor` strips
/// `auto` off anything arriving through the creator surface, so a `find` that
/// claimed it would be denied before any SQL ran and the arm would measure the
/// authorization rather than the privilege.
fn auditor_policy() -> Value {
    value!({ "auditor": ["spi"] })
}

/// The read options a creator sends to unmask one column.
fn unmask_opts(column: &str) -> Value {
    value!({
        "unmask": [column],
        "actor": { "kind": "auditor", "id": "operator-1" },
        "unmaskReason": "binding fence arm"
    })
}

/// A cluster converged for one database, holding one masked table with the
/// grants an apply emits over it.
struct MaskedFence {
    url: String,
    database: DatabaseId,
    edge: BindingId,
}

impl MaskedFence {
    async fn build(url: String) -> Self {
        let mut admin = connect(&url).await;
        require_pinned_major(&admin).await;
        cluster::apply_bootstrap_corpus(&admin)
            .await
            .expect("the datastore bootstrap corpus applies");

        let database = DatabaseId::mint();
        let edge = BindingId::mint();
        cluster::converge_database(&mut admin, &database)
            .await
            .expect("the reconciler converges the database");
        cluster::grant_binding(&admin, &edge, &database, DatabaseCapability::ReadWrite)
            .await
            .expect("the reconciler grants the binding's edges");

        let schema = database_derivation::schema_name(&database);
        let ssn_raw = raw_column_name("ssn");
        let amount_raw = raw_column_name("amount");
        admin
            .batch_execute(&format!(
                "CREATE TABLE \"{schema}\".\"{MASKED_COLLECTION}\" (
                     id text PRIMARY KEY,
                     ssn text,
                     \"{ssn_raw}\" text,
                     nickname text,
                     amount numeric,
                     \"{amount_raw}\" numeric
                 );
                 INSERT INTO \"{schema}\".\"{MASKED_COLLECTION}\"
                     VALUES ('{ROW_PK}', '{MASKED_SSN}', '{REAL_SSN}', 'nick', NULL, 'NaN');"
            ))
            .await
            .expect("the masked fixture table is legal DDL");

        provisioning::provision_audit_unmask_table(&admin, &schema)
            .await
            .expect("the migration service creates the app's unmask audit table");
        let readwrite =
            database_derivation::capability_role_name(&database, DatabaseCapability::ReadWrite)
                .expect("the fixture capability role name fits");
        let readonly =
            database_derivation::capability_role_name(&database, DatabaseCapability::ReadOnly)
                .expect("the fixture capability role name fits");
        provisioning::grant_audit_unmask_to_capabilities(&admin, &schema, &readwrite, &readonly)
            .await
            .expect("a bound session may append its own audit row");

        // The apply-time converger, over the live catalog: every capability
        // grant and the unmask role's `SELECT` on the real-value columns, in
        // the one emission production uses.
        capability_grants::grant_capability_columns(&admin, &database)
            .await
            .expect("the apply converges this schema's column grants");

        admin
            .batch_execute(&format!(
                "ALTER ROLE \"{WORKER_ROLE}\" PASSWORD '{WORKER_PASSWORD}'"
            ))
            .await
            .expect("the operator supplies the worker's authentication material");

        Self {
            url: worker_url(&url),
            database,
            edge,
        }
    }

    fn binding(&self) -> DbBinding {
        DbBinding::to_database(
            "app_unmask",
            "deploy_unmask",
            self.database.clone(),
            self.edge.clone(),
            DatabaseCapability::ReadWrite,
        )
        .expect("the fixture ids compose a legal role name")
    }

    async fn open(&self) -> Database {
        let database = Database::connect(
            self.binding(),
            zeroship_data_orm::ConnectOptions::new(&self.url, ProjectKeySource::unavailable()),
            people_schema(),
        )
        .await
        .expect("the creator opens its database");
        database
            .install_mask_policy(auditor_policy())
            .expect("the app declares its mask policy at boot");
        database
    }

    /// Run one statement as the worker login under `role`, the way the data
    /// plane narrows. Mirrors the tenant fence's helper of the same name.
    async fn under_role(
        &self,
        role: &str,
        sql: &str,
    ) -> Result<Vec<compio_postgres::Row>, compio_postgres::Error> {
        let mut worker = connect(&self.url).await;
        let transaction = worker.transaction().await?;
        transaction
            .simple_query(&format!("SET LOCAL ROLE \"{role}\""))
            .await?;
        let rows = transaction.query(sql, &[]).await?;
        transaction.rollback().await?;
        Ok(rows)
    }

    fn table(&self) -> String {
        format!(
            "\"{}\".\"{MASKED_COLLECTION}\"",
            database_derivation::schema_name(&self.database)
        )
    }

    fn binding_role(&self) -> String {
        database_derivation::binding_role_name(&self.edge).expect("the fixture role name fits")
    }

    fn unmask_role(&self) -> String {
        database_derivation::unmask_role_name(&self.database).expect("the fixture role name fits")
    }
}

/// One unmasked read through the creator's own `find`.
async fn unmasked_ssn(database: &Database) -> Result<Value, DbError> {
    let found = database
        .collection(MASKED_COLLECTION)?
        .find(value!({ "id": ROW_PK }), unmask_opts("ssn"))
        .await?;
    let Output::Rows { mut rows, .. } = found else {
        panic!("find must return rows");
    };
    assert_eq!(rows.len(), 1, "the seeded row must be present to be read");
    Ok(rows.remove(0)["ssn"].clone())
}

/// The masked value the same read returns with no `unmask` option.
async fn masked_ssn(database: &Database) -> Result<Value, DbError> {
    let found = database
        .collection(MASKED_COLLECTION)?
        .find(value!({ "id": ROW_PK }), value!({}))
        .await?;
    let Output::Rows { mut rows, .. } = found else {
        panic!("find must return rows");
    };
    assert_eq!(rows.len(), 1, "the seeded row must be present to be read");
    let wrapper = rows.remove(0)["ssn"].clone();
    assert_eq!(
        wrapper["classification"],
        Value::from("spi"),
        "an ordinary read must return the masked-value wrapper, not a bare \
         string - otherwise the control below is not about a masked field: \
         {wrapper:?}"
    );
    Ok(wrapper["masked"].clone())
}

/// **The property.** An audited unmask reaches the real value on a session
/// narrowed to the binding role, on both routes a dispatch can take.
///
/// The two CONTROLS are the ordinary read beside each: it must return the MASK,
/// which is what proves the row is reachable, the column is masked, and the
/// arm is not passing because the raw value happened to be projected anyway.
#[compio::test]
async fn an_audited_unmask_reaches_the_real_value_a_binding_role_cannot_select() {
    let postgres = postgres_fixture::Postgres::start();
    let fence = MaskedFence::build(postgres.url()).await;
    let database = fence.open().await;

    // CONTROL: the ordinary read returns the mask.
    assert_eq!(
        masked_ssn(&database)
            .await
            .expect("the binding role reads the mask column"),
        Value::from(MASKED_SSN)
    );

    // SUBJECT, autocommit route.
    assert_eq!(
        unmasked_ssn(&database)
            .await
            .expect("an audited unmask must reach the real value"),
        Value::from(REAL_SSN)
    );

    // SUBJECT, the creator's own transaction: the same read on the lane the
    // creator's statements run on, with its own control beside it.
    let inside = database
        .transaction(|tx| async move {
            assert_eq!(
                masked_ssn(&tx).await?,
                Value::from(MASKED_SSN),
                "the control: the ordinary read inside the transaction"
            );
            unmasked_ssn(&tx).await
        })
        .await
        .expect("an audited unmask inside a creator transaction must reach the real value");
    assert_eq!(inside, Value::from(REAL_SSN));

    drop(database);
    drop(fence);
    drain().await;
}

/// The privilege is not AMBIENT: under the binding role the real-value column
/// is refused, and so is a projection that would sweep it up.
///
/// Each arm differs from the permitted read in one variable - which column the
/// statement names - and the control is the mask column, which must come back.
#[compio::test]
async fn the_binding_role_is_refused_the_real_value_column_and_the_whole_row() {
    let postgres = postgres_fixture::Postgres::start();
    let fence = MaskedFence::build(postgres.url()).await;
    let table = fence.table();
    let role = fence.binding_role();
    let ssn_raw = raw_column_name("ssn");

    // CONTROL: the binding role reads the mask column and the identity.
    let permitted = fence
        .under_role(&role, &format!("SELECT ssn FROM {table} WHERE id = '{ROW_PK}'"))
        .await
        .expect("the binding role reads the columns its capability was granted");
    assert_eq!(permitted.len(), 1);
    assert_eq!(permitted[0].get::<_, &str>("ssn"), MASKED_SSN);

    for (what, sql) in [
        (
            "the real-value column by name",
            format!("SELECT \"{ssn_raw}\" FROM {table} WHERE id = '{ROW_PK}'"),
        ),
        (
            "a whole-row projection",
            format!("SELECT * FROM {table} WHERE id = '{ROW_PK}'"),
        ),
    ] {
        let error = fence
            .under_role(&role, &sql)
            .await
            .err()
            .unwrap_or_else(|| panic!("{what} must be refused under the binding role"));
        assert_eq!(
            error
                .as_db_error()
                .unwrap_or_else(|| panic!("{what}: the server must refuse, not the transport"))
                .code(),
            &SqlState::INSUFFICIENT_PRIVILEGE,
            "{what} must be refused with 42501"
        );
    }

    drop(fence);
    drain().await;
}

/// The unmask role reaches the real-value column and the identity the read
/// addresses, and NOTHING else on that table.
///
/// Its control is the permitted read: without it a role that had been granted
/// nothing at all would satisfy every refusal below.
#[compio::test]
async fn the_unmask_role_reaches_the_real_value_and_no_other_column_or_verb() {
    let postgres = postgres_fixture::Postgres::start();
    let fence = MaskedFence::build(postgres.url()).await;
    let table = fence.table();
    let role = fence.unmask_role();
    let ssn_raw = raw_column_name("ssn");

    // CONTROL: the statement the data plane compiles, under this role.
    let permitted = fence
        .under_role(
            &role,
            &format!("SELECT \"{ssn_raw}\" AS raw FROM {table} WHERE id = '{ROW_PK}'"),
        )
        .await
        .expect("the unmask role reads the real value it was granted");
    assert_eq!(permitted.len(), 1);
    assert_eq!(permitted[0].get::<_, &str>("raw"), REAL_SSN);

    for (what, sql) in [
        (
            "a non-raw column it was not granted",
            format!("SELECT nickname FROM {table} WHERE id = '{ROW_PK}'"),
        ),
        (
            "the mask column",
            format!("SELECT ssn FROM {table} WHERE id = '{ROW_PK}'"),
        ),
        (
            "a whole-row projection",
            format!("SELECT * FROM {table} WHERE id = '{ROW_PK}'"),
        ),
        (
            "an INSERT",
            format!("INSERT INTO {table} (id) VALUES ('p2') RETURNING id"),
        ),
        (
            "an UPDATE",
            format!("UPDATE {table} SET nickname = 'x' WHERE id = '{ROW_PK}' RETURNING id"),
        ),
        (
            "a DELETE",
            format!("DELETE FROM {table} WHERE id = '{ROW_PK}' RETURNING id"),
        ),
    ] {
        let error = fence
            .under_role(&role, &sql)
            .await
            .err()
            .unwrap_or_else(|| panic!("{what} must be refused under the unmask role"));
        assert_eq!(
            error
                .as_db_error()
                .unwrap_or_else(|| panic!("{what}: the server must refuse, not the transport"))
                .code(),
            &SqlState::INSUFFICIENT_PRIVILEGE,
            "{what} must be refused with 42501"
        );
    }

    drop(fence);
    drain().await;
}


/// The bracket gives the binding role back, whether the read succeeded or not.
///
/// Measured on the session ITSELF rather than through a `find`, because the ORM
/// is not what this is about and its transaction reducer would hide the answer:
/// a failed operation POISONS the lane (`transaction/reducer`, Invariant 13),
/// so no creator statement runs after one and the reducer's refusal - not the
/// role - is what a higher arm would observe. The hazard the bracket exists for
/// is one level below that: the session is the creator's own, handed straight
/// back, and a `SET LOCAL ROLE` left on it lasts for the rest of the
/// transaction.
///
/// `current_user` is the instrument, so the elevation is OBSERVED and not
/// inferred. Three readings, each differing from the next in one step:
///
/// - before the bracket, the binding role - the setup batch narrowed to it;
/// - INSIDE the bracket, the unmask role - without this reading the restore
///   assertions below would hold on a session that never elevated at all;
/// - after it, the binding role again, on both the success arm and the failure
///   arm.
///
/// The failing read is `'NaN'::numeric`: the server ANSWERS it and the row
/// decoder refuses the value, which is the one failure shape that leaves the
/// transaction live. A statement the server itself rejects aborts it, and then
/// every later statement is refused `25P02` whatever role it would have run as
/// - so that failure cannot leak the elevation and cannot measure the restore.
#[compio::test]
async fn the_unmask_bracket_gives_the_binding_role_back_on_both_outcomes() {
    let postgres = postgres_fixture::Postgres::start();
    let fence = MaskedFence::build(postgres.url()).await;
    let binding = fence.binding();
    let backend = zeroship_data_orm::ConnectOptions::new(&fence.url, ProjectKeySource::unavailable())
        .connect()
        .await
        .expect("the worker login opens a backend");
    let session = backend
        .open_tx_session(&binding, BeginIntent::Default)
        .await
        .expect("the creator's transaction opens and narrows to its binding role");

    let current_user = async |session: &Session| -> String {
        session
            .query("SELECT current_user AS u", &[])
            .await
            .expect("current_user is readable under any role")[0]["u"]
            .as_str()
            .expect("current_user is text")
            .to_owned()
    };

    assert_eq!(
        current_user(&session).await,
        fence.binding_role(),
        "the setup batch must have narrowed to the binding role"
    );

    // SUBJECT 1 - a read that succeeds. Its statement reports the role it ran
    // under, so the elevation is measured rather than assumed.
    let during = backend
        .read_unmasked(
            &binding,
            Some(&session),
            "SELECT current_user AS \"_raw\"",
            &[],
        )
        .await
        .expect("the bracketed read runs");
    assert_eq!(
        during[0]["_raw"],
        Value::from(fence.unmask_role()),
        "the bracketed statement must run as the database's unmask role"
    );
    assert_eq!(
        current_user(&session).await,
        fence.binding_role(),
        "a successful bracketed read must narrow straight back"
    );

    // SUBJECT 2 - the same bracket over a read the server answers and the row
    // decoder refuses. One variable differs: the value that comes back.
    let failed = backend
        .read_unmasked(
            &binding,
            Some(&session),
            "SELECT 'NaN'::numeric AS \"_raw\"",
            &[],
        )
        .await
        .expect_err("a value the row decoder refuses must surface as an error");
    assert!(
        failed.to_string().contains("_raw"),
        "the failure must be the decode of the projected column: {failed}"
    );
    assert_eq!(
        current_user(&session).await,
        fence.binding_role(),
        "a FAILED bracketed read must narrow back too, or the creator's next \
         statement on this session runs as the unmask role"
    );

    session.discard();
    drop(backend);
    drop(fence);
    drain().await;
}
