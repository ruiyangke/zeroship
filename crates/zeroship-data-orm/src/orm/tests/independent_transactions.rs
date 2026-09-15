//! Concurrent transaction lanes, the re-entrant-root refusal, typed callback
//! errors and the connection probe.

#![expect(
    clippy::future_not_send,
    reason = "ORM fixtures use local compio sessions"
)]

use super::fixtures::CollectionFixture;
use super::*;
use futures::{
    channel::oneshot,
    future::{select, Either},
    FutureExt,
};
use std::time::Duration;

schema! {
    pub lane_schema {
        lane_rows {
            #[orm(primary_key)]
            id: Text,
            label: Text,
        }
    }
}
use lane_schema::lane_rows;

#[derive(Debug, FromRow)]
#[orm(entity = lane_rows)]
struct LaneRow {
    id: String,
    label: String,
}

/// A host error that is not a database error, for the typed-callback arms.
#[derive(Debug)]
enum Refusal {
    Declined,
    Database(DbError),
}
impl From<DbError> for Refusal {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

fn row_fields() -> Value {
    value!({
        "id": {"type":"string", "primaryKey":true, "required":true},
        "label": {"type":"string", "required":true}
    })
}

const ROW_COLUMNS: &str = "id TEXT PRIMARY KEY, label TEXT NOT NULL";

async fn postgres_fixture() -> CollectionFixture {
    CollectionFixture::postgres_from_table_definition("lane_rows", row_fields(), ROW_COLUMNS).await
}

/// The same fixture with native models installed, for the arms that need an
/// entity alias.
async fn typed_postgres_fixture() -> CollectionFixture {
    let mut owner = postgres_fixture().await;
    owner.database = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        lane_schema::schema(),
    )
    .unwrap();
    owner
}

async fn seed(owner: &CollectionFixture, ids: &[&str]) {
    for id in ids {
        owner
            .database
            .collection("lane_rows")
            .unwrap()
            .insert(value!({"id": *id, "label": "original"}))
            .await
            .unwrap();
    }
}

fn assert_code(error: DbError, expected: &str) {
    let matched = matches!(&error, DbError::ValidationFailed { code, .. } if *code == expected);
    assert!(matched, "expected {expected}: {}", error.into_string());
}

fn postgres_backend(owner: &CollectionFixture) -> &crate::backend::postgres::PostgresBackend {
    owner
        .database
        .backend
        .get::<crate::backend::postgres::PostgresBackend>()
        .unwrap()
}

fn qualified(owner: &CollectionFixture, table: &str) -> String {
    format!(
        "{}.{table}",
        crate::sql::mapping::quote_ident(owner.database.binding.schema().as_str())
    )
}

async fn label_of(owner: &CollectionFixture, id: &str) -> String {
    let rows = owner
        .postgres_oracle(
            "lane_rows",
            &format!("SELECT label FROM {{table}} WHERE id = '{id}'"),
        )
        .await;
    assert_eq!(rows.len(), 1, "row '{id}' is missing");
    rows[0].get::<_, String>(0)
}

/// Update one row through a handle's own transaction.
async fn set_label(db: &Database, id: &str, label: &str) -> Result<(), DbError> {
    db.collection("lane_rows")?
        .update(value!({ "id": id }), value!({ "label": label }))
        .await?;
    Ok(())
}

/// **A root handle re-entering its own lane is refused, not parked.**
///
/// The lane is held by the callback that is polling this call, so the claim can
/// never be released from here: parking is a self-deadlock that `PostgreSQL`
/// cannot see and that only a caller-side timeout ends.
///
/// CONTROL: the same nesting through the transaction handle opens a savepoint
/// and commits, so the refusal is about which handle re-entered, not about
/// nesting.
#[compio::test]
async fn a_root_handle_refuses_a_top_level_transaction_inside_a_callback() {
    let owner = postgres_fixture().await;
    let root = owner.database.clone();
    let refused = compio::time::timeout(
        Duration::from_secs(5),
        owner.database.transaction(|tx| async move {
            // CONTROL: the transaction handle nests through a savepoint.
            tx.transaction(|inner| async move {
                inner
                    .collection("lane_rows")?
                    .insert(value!({"id":"nested","label":"savepoint"}))
                    .await?;
                Ok::<_, DbError>(())
            })
            .await?;
            // SUBJECT: the root handle re-enters the lane its own callback holds.
            let refused = root
                .transaction(|_| async move { Ok::<_, DbError>(()) })
                .await
                .expect_err("a root handle cannot open a second top-level transaction here");
            Ok::<_, DbError>(refused)
        }),
    )
    .await
    .expect("a nested top-level transaction must be refused, not parked until a caller timeout")
    .expect("the enclosing transaction must commit");
    assert_code(refused, "nested_top_level_transaction");

    let rows = owner
        .postgres_oracle("lane_rows", "SELECT id FROM {table} ORDER BY id")
        .await;
    assert_eq!(
        rows.len(),
        1,
        "the control savepoint's row must have committed with its enclosing transaction"
    );
    assert_eq!(rows[0].get::<_, String>(0), "nested");
    owner.close().await;
}

/// **An independent handle commits while the original's transaction is open.**
///
/// The first transaction holds a row lock and its callback parks; the fork
/// writes an unrelated row and commits before it settles. Its committed change
/// also reaches the process broker, which is the shared sink a fork must not
/// lose events to.
#[compio::test]
async fn an_independent_handle_commits_while_the_original_holds_a_row_lock() {
    let owner = postgres_fixture().await;
    seed(&owner, &["held", "free"]).await;
    let app = owner.database.binding.app_id().to_owned();
    crate::cdc::broker::drain_current_thread_subscriptions();
    let sub = crate::cdc::broker::subscribe(&app, "lane_rows");

    let fork = owner
        .database
        .independent()
        .expect("PostgreSQL admits independent lanes");

    let (held, ready) = oneshot::channel();
    let (release, resume) = oneshot::channel();
    let holding = owner
        .database
        .transaction(|tx| async move {
            set_label(&tx, "held", "holder").await?;
            held.send(()).unwrap();
            resume.await.unwrap();
            Ok::<_, DbError>(())
        })
        .boxed_local();
    let holding = match select(ready, holding).await {
        Either::Left((ready, holding)) => {
            ready.unwrap();
            holding
        }
        Either::Right((result, _)) => panic!("the holding callback ended early: {result:?}"),
    };
    assert!(
        sub.pop().is_none(),
        "an open transaction must publish nothing"
    );

    compio::time::timeout(
        Duration::from_secs(5),
        fork.transaction(|tx| async move {
            set_label(&tx, "free", "fork").await?;
            Ok::<_, DbError>(())
        }),
    )
    .await
    .expect("an independent lane must not queue behind the original's transaction")
    .expect("the fork's transaction commits");

    let published = sub.pop().expect("the fork's commit must reach the broker");
    match published {
        crate::cdc::broker::SubscriptionMessage::Change(event) => {
            assert_eq!(event.collection, "lane_rows");
            assert_eq!(event.pk.as_deref(), Some("free"));
        }
        other => panic!("unexpected broker message: {other:?}"),
    }

    release.send(()).unwrap();
    holding.await.expect("the original transaction commits");
    assert_eq!(label_of(&owner, "held").await, "holder");
    assert_eq!(label_of(&owner, "free").await, "fork");
    sub.close();
    crate::cdc::broker::drain_current_thread_subscriptions();
    owner.close().await;
}

/// CONTROL for the arm above: two clones share one lane, so the second
/// transaction cannot start until the first settles.
#[compio::test]
async fn two_clones_of_one_handle_serialize_their_transactions() {
    let owner = postgres_fixture().await;
    seed(&owner, &["held", "free"]).await;
    let clone = owner.database.clone();

    let (held, ready) = oneshot::channel();
    let (release, resume) = oneshot::channel();
    let holding = owner
        .database
        .transaction(|tx| async move {
            set_label(&tx, "held", "holder").await?;
            held.send(()).unwrap();
            resume.await.unwrap();
            Ok::<_, DbError>(())
        })
        .boxed_local();
    let holding = match select(ready, holding).await {
        Either::Left((ready, holding)) => {
            ready.unwrap();
            holding
        }
        Either::Right((result, _)) => panic!("the holding callback ended early: {result:?}"),
    };

    let mut queued = Box::pin(clone.transaction(|tx| async move {
        set_label(&tx, "free", "clone").await?;
        Ok::<_, DbError>(())
    }));
    assert!(
        compio::time::timeout(Duration::from_millis(500), &mut queued)
            .await
            .is_err(),
        "a clone shares the lane, so its transaction must wait for the first to settle"
    );
    assert_eq!(
        label_of(&owner, "free").await,
        "original",
        "the queued transaction must not have written anything yet"
    );

    release.send(()).unwrap();
    holding.await.expect("the first transaction commits");
    compio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("the released lane must admit the queued transaction")
        .expect("the queued transaction commits");
    assert_eq!(label_of(&owner, "free").await, "clone");
    owner.close().await;
}

/// **A lock wait in one fork does not stall another.**
///
/// A raw peer holds the row lock, so the first fork's update waits on the
/// server. The second fork's transaction is admitted and commits anyway.
///
/// CONTROL: the same wait on a clone of one handle stalls the second
/// transaction, because the lane is held by the transaction that is waiting.
#[compio::test]
async fn a_lock_wait_in_one_fork_does_not_stall_another() {
    let owner = postgres_fixture().await;
    seed(&owner, &["held", "free"]).await;
    let table = qualified(&owner, "lane_rows");
    let peer = postgres_backend(&owner).pool().acquire().await.unwrap();
    peer.batch_execute(&format!(
        "BEGIN; UPDATE {table} SET label = 'peer' WHERE id = 'held';"
    ))
    .await
    .unwrap();

    let waiter = owner.database.independent().unwrap();
    let other = owner.database.independent().unwrap();
    let mut waiting = Box::pin(waiter.transaction(|tx| async move {
        set_label(&tx, "held", "waiter").await?;
        Ok::<_, DbError>(())
    }));
    assert!(
        compio::time::timeout(Duration::from_millis(300), &mut waiting)
            .await
            .is_err(),
        "the fork's update must be blocked by the peer's row lock, or this test proves nothing"
    );

    compio::time::timeout(
        Duration::from_secs(5),
        other.transaction(|tx| async move {
            set_label(&tx, "free", "unblocked").await?;
            Ok::<_, DbError>(())
        }),
    )
    .await
    .expect("a lock wait in one fork must not stall another")
    .expect("the second fork commits");

    peer.batch_execute("COMMIT").await.unwrap();
    compio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("the released row lock must let the waiting fork finish")
        .expect("the waiting fork commits");
    assert_eq!(label_of(&owner, "held").await, "waiter");
    assert_eq!(label_of(&owner, "free").await, "unblocked");

    // CONTROL: one lane, and the second transaction waits behind the first's
    // lock wait rather than reaching the server at all.
    peer.batch_execute(&format!(
        "BEGIN; UPDATE {table} SET label = 'peer' WHERE id = 'held';"
    ))
    .await
    .unwrap();
    let clone = owner.database.clone();
    let mut blocked = Box::pin(owner.database.transaction(|tx| async move {
        set_label(&tx, "held", "never").await?;
        Ok::<_, DbError>(())
    }));
    assert!(
        compio::time::timeout(Duration::from_millis(300), &mut blocked)
            .await
            .is_err(),
        "the clone's update must be blocked by the peer's row lock"
    );
    let mut queued = Box::pin(clone.transaction(|tx| async move {
        set_label(&tx, "free", "never").await?;
        Ok::<_, DbError>(())
    }));
    assert!(
        compio::time::timeout(Duration::from_millis(300), &mut queued)
            .await
            .is_err(),
        "on one lane the second transaction waits behind the first's lock wait"
    );
    assert_eq!(label_of(&owner, "free").await, "unblocked");
    peer.batch_execute("ROLLBACK").await.unwrap();
    compio::time::timeout(Duration::from_secs(5), blocked)
        .await
        .expect("the released lock lets the first clone settle")
        .expect("the first clone commits");
    compio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("the released lane lets the queued clone settle")
        .expect("the queued clone commits");
    drop(peer);
    owner.close().await;
}

/// A handle over a pool sized for this test, against the fixture's server.
async fn bounded_pool_handle(
    owner: &CollectionFixture,
    max_size: usize,
    acquire: Duration,
) -> Database {
    let url = owner.server_url();
    let mut config = compio_postgres::PoolConfig::default();
    config
        .max_size(max_size)
        .min_idle(max_size)
        .acquire_timeout(acquire);
    let pool = Rc::new(
        compio_postgres::Pool::connect_with_pool_config(&url, config)
            .await
            .expect("bounded pool"),
    );
    let backend = Rc::new(crate::backend::postgres::PostgresBackend::new(
        pool,
        url,
        ProjectKeySource::unavailable(),
    ));
    Database::from_schema(
        owner.database.binding.clone(),
        crate::backend_handle::BackendHandle::new(backend),
        Schema::from_collections(vec![("lane_rows".into(), row_fields())]).unwrap(),
    )
    .unwrap()
}

/// **Concurrency is bounded by the pool, and the bound is a queue with a
/// deadline rather than a wall.**
#[compio::test]
async fn a_forks_transaction_fails_with_the_acquire_timeout_when_the_pool_is_full() {
    let owner = postgres_fixture().await;
    seed(&owner, &["held", "free"]).await;
    let bounded = bounded_pool_handle(&owner, 1, Duration::from_millis(400)).await;
    let fork = bounded.independent().unwrap();

    let (held, ready) = oneshot::channel();
    let (release, resume) = oneshot::channel();
    let holding = bounded
        .transaction(|tx| async move {
            set_label(&tx, "held", "holder").await?;
            held.send(()).unwrap();
            resume.await.unwrap();
            Ok::<_, DbError>(())
        })
        .boxed_local();
    let holding = match select(ready, holding).await {
        Either::Left((ready, holding)) => {
            ready.unwrap();
            holding
        }
        Either::Right((result, _)) => panic!("the holding callback ended early: {result:?}"),
    };

    let refused = compio::time::timeout(
        Duration::from_secs(5),
        fork.transaction(|tx| async move {
            set_label(&tx, "free", "fork").await?;
            Ok::<_, DbError>(())
        }),
    )
    .await
    .expect("the fork must fail on the acquire timeout rather than wait forever")
    .expect_err("the only connection is held by the first transaction");
    let message = refused.into_string();
    assert!(
        message.contains("acquisition"),
        "the refusal must name the acquire timeout so the ceiling is visible: {message}"
    );

    // CONTROL: the ceiling is a queue, not a wall.
    release.send(()).unwrap();
    holding.await.expect("the first transaction commits");
    compio::time::timeout(
        Duration::from_secs(5),
        fork.transaction(|tx| async move {
            set_label(&tx, "free", "fork").await?;
            Ok::<_, DbError>(())
        }),
    )
    .await
    .expect("the returned lease must admit the fork")
    .expect("the fork commits once a connection is free");
    assert_eq!(label_of(&owner, "free").await, "fork");
    drop(fork);
    drop(bounded);
    owner.close().await;
}

/// **A fork unmasks exactly what the handle it came from could.**
///
/// CONTROL: an unmasked column reads the same either way.
/// REJECTION CONTROL: a context that never installed the policy refuses the
/// same unmask, so the fork's success is the policy and not a default.
#[compio::test]
async fn a_fork_inherits_the_installed_mask_policy() {
    let fields = value!({
        "label": {"type":"string"},
        "secret": {"type":"string", "mask":{"kind":"full","classification":"pii"},
            "storage":{"valueColumn":"secret","rawColumn":"__zs_raw__secret"}}
    });
    let owner = CollectionFixture::postgres("records", fields.clone()).await;
    owner
        .database
        .install_mask_policy(value!({"support":["pii"]}))
        .unwrap();
    owner
        .database
        .collection("records")
        .unwrap()
        .insert(value!({"label":"visible","secret":"classified"}))
        .await
        .unwrap();

    let unmask = value!({
        "unmask":["secret"],
        "actor":{"kind":"support","id":"usr_reader"},
        "unmaskReason":"mask policy parity"
    });
    let fork = owner.database.independent().unwrap();
    let Output::Rows { rows, .. } = fork
        .collection("records")
        .unwrap()
        .find(value!({"label":"visible"}), unmask.clone())
        .await
        .expect("the fork inherits the policy that permits this unmask")
    else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["secret"], value!("classified"));
    // CONTROL: the unmasked column is unaffected by any of this.
    assert_eq!(rows[0]["label"], value!("visible"));

    let stranger = Database::from_schema(
        owner.database.binding.clone(),
        owner.database.backend.clone(),
        Schema::from_collections(vec![(
            "records".into(),
            crate::tests::fixtures::schema::generated_fields(fields),
        )])
        .unwrap(),
    )
    .unwrap();
    let refused = stranger
        .collection("records")
        .unwrap()
        .find(value!({"label":"visible"}), unmask)
        .await
        .expect_err("a context without the policy must refuse the same unmask");
    assert!(
        matches!(&refused, DbError::Coded { code, .. } if code == "unmask_not_permitted"),
        "expected unmask_not_permitted: {}",
        refused.into_string()
    );
    owner.close().await;
}

/// **A read source does not cross forks.**
///
/// CONTROL: the same alias used on the handle that built it returns its row.
#[compio::test]
async fn a_read_source_from_one_fork_is_refused_inside_another() {
    let owner = typed_postgres_fixture().await;
    seed(&owner, &["held"]).await;
    let first = owner.database.independent().unwrap();
    let second = owner.database.independent().unwrap();
    let alias = first
        .entity::<lane_rows::Entity>()
        .unwrap()
        .alias("rows")
        .unwrap();

    let refused = second
        .transaction(|tx| async move {
            let error = tx
                .from(&alias)
                .select(alias.row::<LaneRow>())
                .expect_err("another fork's read source must be refused");
            // CONTROL: the same alias on its own handle builds and runs.
            let rows: Vec<LaneRow> = first
                .from(&alias)
                .select(alias.row::<LaneRow>())?
                .all()
                .await?;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id, "held");
            assert_eq!(rows[0].label, "original");
            Ok::<_, DbError>(error)
        })
        .await
        .expect("the callback itself must succeed");
    assert_code(refused, "invalid_read");
    owner.close().await;
}

/// **A callback's own error type survives the transaction.**
///
/// `Ok(Err(domain))` commits the work that preceded the refusal;
/// `Err(domain)` rolls it back. Both return the refusal itself.
async fn domain_errors_commit_or_roll_back(owner: &CollectionFixture) {
    let committed: Result<(), Refusal> = owner
        .database
        .transaction(|tx| async move {
            tx.collection("lane_rows")?
                .insert(value!({"id":"kept","label":"committed"}))
                .await?;
            Ok::<_, Refusal>(Err(Refusal::Declined))
        })
        .await
        .expect("a committed transaction returns its callback's value");
    assert!(matches!(committed, Err(Refusal::Declined)));

    let rolled_back = owner
        .database
        .transaction(|tx| async move {
            tx.collection("lane_rows")?
                .insert(value!({"id":"gone","label":"rolled back"}))
                .await?;
            Err::<(), _>(Refusal::Declined)
        })
        .await
        .expect_err("a callback error rolls back and is returned");
    assert!(matches!(rolled_back, Refusal::Declined));

    let Output::Count(kept) = owner
        .database
        .collection("lane_rows")
        .unwrap()
        .count(value!({"id":"kept"}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected a count")
    };
    let Output::Count(gone) = owner
        .database
        .collection("lane_rows")
        .unwrap()
        .count(value!({"id":"gone"}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected a count")
    };
    assert_eq!(kept, 1, "Ok(Err(domain)) must commit");
    assert_eq!(gone, 0, "Err(domain) must roll back");
}

#[compio::test]
async fn postgres_domain_errors_commit_or_roll_back() {
    let owner = postgres_fixture().await;
    domain_errors_commit_or_roll_back(&owner).await;
    owner.close().await;
}

#[compio::test]
async fn sqlite_domain_errors_commit_or_roll_back() {
    let owner =
        CollectionFixture::sqlite_from_table_definition("lane_rows", row_fields(), ROW_COLUMNS)
            .await;
    domain_errors_commit_or_roll_back(&owner).await;
    owner.close().await;
}

/// **A failure that only surfaces at COMMIT arrives as the caller's type.**
///
/// The constraint is deferred, so both inserts succeed and the transaction is
/// refused by the settle rather than by a statement.
///
/// CONTROL: the same transaction with distinct labels commits.
#[compio::test]
async fn a_deferred_unique_violation_at_commit_reaches_the_callers_error_type() {
    let owner = CollectionFixture::postgres_from_table_definition(
        "lane_rows",
        row_fields(),
        "id TEXT PRIMARY KEY, label TEXT NOT NULL, \
         CONSTRAINT lane_rows_label_unique UNIQUE (label) DEFERRABLE INITIALLY DEFERRED",
    )
    .await;

    let refused = owner
        .database
        .transaction(|tx| async move {
            let rows = tx.collection("lane_rows")?;
            rows.insert(value!({"id":"first","label":"same"})).await?;
            rows.insert(value!({"id":"second","label":"same"})).await?;
            Ok::<_, Refusal>(())
        })
        .await
        .expect_err("a deferred unique violation must fail the commit");
    let Refusal::Database(error) = refused else {
        panic!("the settle failure must arrive through the caller's conversion: {refused:?}");
    };
    assert!(
        !error.into_string().is_empty(),
        "the converted database error must still carry its report"
    );
    assert!(
        owner
            .postgres_oracle("lane_rows", "SELECT id FROM {table}")
            .await
            .is_empty(),
        "a failed commit leaves nothing behind"
    );

    // CONTROL: the same shape without the conflict commits both rows.
    owner
        .database
        .transaction(|tx| async move {
            let rows = tx.collection("lane_rows")?;
            rows.insert(value!({"id":"first","label":"one"})).await?;
            rows.insert(value!({"id":"second","label":"two"})).await?;
            Ok::<_, Refusal>(())
        })
        .await
        .expect("distinct labels commit");
    assert_eq!(
        owner
            .postgres_oracle("lane_rows", "SELECT id FROM {table}")
            .await
            .len(),
        2
    );
    owner.close().await;
}

/// **`SQLite` refuses an independent handle before any SQL.**
///
/// CONTROL: ordinary transactions and the connection probe still work on the
/// handle that was refused.
#[compio::test]
async fn sqlite_refuses_an_independent_handle() {
    let owner =
        CollectionFixture::sqlite_from_table_definition("lane_rows", row_fields(), ROW_COLUMNS)
            .await;
    assert_code(
        owner
            .database
            .independent()
            .expect_err("the SQLite actor reserves one transaction connection per app"),
        "unsupported_backend_feature",
    );

    owner
        .database
        .transaction(|tx| async move {
            tx.collection("lane_rows")?
                .insert(value!({"id":"kept","label":"committed"}))
                .await?;
            Ok::<_, DbError>(())
        })
        .await
        .expect("the refused handle still runs ordinary transactions");
    owner
        .database
        .check_connection()
        .await
        .expect("the SQLite probe answers");
    let Output::Count(rows) = owner
        .database
        .collection("lane_rows")
        .unwrap()
        .count(value!({}), value!({}))
        .await
        .unwrap()
    else {
        panic!("expected a count")
    };
    assert_eq!(rows, 1);
    owner.close().await;
}

/// **The probe takes no lane and recovers from a server that closed its
/// connections.**
#[compio::test]
async fn check_connection_answers_beside_a_transaction_and_after_a_terminated_backend() {
    let owner = postgres_fixture().await;
    owner
        .database
        .check_connection()
        .await
        .expect("a healthy pool answers");

    owner
        .database
        .transaction(|tx| async move {
            tx.collection("lane_rows")?
                .insert(value!({"id":"kept","label":"committed"}))
                .await?;
            compio::time::timeout(Duration::from_secs(5), tx.check_connection())
                .await
                .expect("the probe must not queue behind this transaction's lane")?;
            Ok::<_, DbError>(())
        })
        .await
        .expect("the transaction commits");

    owner
        .postgres_oracle(
            "lane_rows",
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = current_database() AND pid <> pg_backend_pid()",
        )
        .await;
    // The server closed the pooled connections; a probe that answered with a
    // dead lease, or never reopened one, fails on this bound.
    compio::time::timeout(Duration::from_secs(10), async {
        loop {
            if owner.database.check_connection().await.is_ok() {
                return;
            }
            compio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the probe must recover after the server terminated the pooled connections");
    owner.close().await;
}

/// **An unreachable server ends the probe with an error inside its wait.**
///
/// CONTROL: the same probe on the same handle answers while the server is up.
#[compio::test]
async fn check_connection_fails_once_the_server_is_gone() {
    let mut owner = postgres_fixture().await;
    // The probe's wait is the pool's, so this handle states it rather than
    // leaving the test to race the default.
    let probed = bounded_pool_handle(&owner, 1, Duration::from_millis(500)).await;
    probed
        .check_connection()
        .await
        .expect("CONTROL: the probe answers while the server is up");

    owner.stop_server();
    let error = compio::time::timeout(Duration::from_secs(10), probed.check_connection())
        .await
        .expect("the probe must end within its own wait once the server is gone")
        .expect_err("an unreachable server cannot answer a probe");
    assert!(
        !error.into_string().is_empty(),
        "the refusal must say something an operator can act on"
    );
    // The schema this fixture created went with the server, so it is dropped
    // rather than closed.
    drop(probed);
    drop(owner);
}
