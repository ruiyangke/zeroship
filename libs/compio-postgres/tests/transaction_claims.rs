//! Mutation-sensitive coverage for runtime guarantees documented by
//! `Transaction`.

use compio_postgres::{Client, Error, NoTls};
use futures_util::FutureExt;
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::cell::Cell;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Once;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

const PANIC_COMMIT_LOG: &str = "executing statement batch: COMMIT";
const PANIC_ROLLBACK_LOG: &str = "executing statement batch: ROLLBACK";
const PANIC_START_LOG: &str = "executing statement batch: START TRANSACTION";

struct PanicOnTransactionLog;

thread_local! {
    static PANIC_COMMIT_ARMED: Cell<bool> = const { Cell::new(false) };
    static PANIC_ROLLBACK_ARMED: Cell<bool> = const { Cell::new(false) };
    static PANIC_START_ARMED: Cell<bool> = const { Cell::new(false) };
}

impl Log for PanicOnTransactionLog {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Debug
    }

    fn log(&self, record: &Record<'_>) {
        if self.enabled(record.metadata())
            && record.args().to_string().contains(PANIC_COMMIT_LOG)
            && PANIC_COMMIT_ARMED.with(|armed| armed.replace(false))
        {
            panic!("panic requested before COMMIT was encoded");
        }
        if self.enabled(record.metadata())
            && record.args().to_string().contains(PANIC_ROLLBACK_LOG)
            && PANIC_ROLLBACK_ARMED.with(|armed| armed.replace(false))
        {
            panic!("panic requested before ROLLBACK was encoded");
        }
        if self.enabled(record.metadata())
            && record.args().to_string().contains(PANIC_START_LOG)
            && PANIC_START_ARMED.with(|armed| armed.replace(false))
        {
            panic!("panic requested before START TRANSACTION was encoded");
        }
    }

    fn flush(&self) {}
}

/// Require the caught panic to be THE INJECTED ONE.
///
/// The three tests below asserted `panic.is_err()` until 2026-08-23 - "a panic
/// occurred". Any panic from the operation satisfies that, including one the
/// arming never caused: a failed `expect` inside the driver, or the arm never
/// firing at all while something else went wrong. And when the arm does not
/// fire it stays SET, because `replace(false)` only runs on the path that
/// panics, so the next test to run on this thread inherits it. Naming the
/// payload is what makes these tests about the injection they perform.
#[track_caller]
fn assert_injected_panic<T>(outcome: Result<T, Box<dyn std::any::Any + Send>>, expected: &str) {
    let payload = outcome.err().unwrap_or_else(|| {
        panic!("the {expected:?} injection did not panic, so nothing below is about it")
    });
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&'static str>().copied())
        .unwrap_or("<non-string panic payload>");
    assert_eq!(
        message, expected,
        "a DIFFERENT panic was caught, so the injected one may never have fired"
    );
}

static PANIC_TRANSACTION_LOGGER: PanicOnTransactionLog = PanicOnTransactionLog;
static INSTALL_PANIC_TRANSACTION_LOGGER: Once = Once::new();

fn install_transaction_panic_logger() {
    INSTALL_PANIC_TRANSACTION_LOGGER.call_once(|| {
        log::set_logger(&PANIC_TRANSACTION_LOGGER).expect("install the transaction panic logger");
        log::set_max_level(LevelFilter::Debug);
    });
}

fn arm_commit_log_panic() {
    install_transaction_panic_logger();
    PANIC_COMMIT_ARMED.with(|armed| armed.set(true));
}

fn arm_rollback_log_panic() {
    install_transaction_panic_logger();
    PANIC_ROLLBACK_ARMED.with(|armed| armed.set(true));
}

fn arm_start_log_panic() {
    install_transaction_panic_logger();
    PANIC_START_ARMED.with(|armed| armed.set(true));
}

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {}", common::error_chain(&error));
        }
    })
    .detach();
    Ok(client)
}

/// `Transaction::bind` reports a parameter-arity mistake the same way its
/// siblings do: as an `Err`, not a panic, and not a `Portal` bound to the
/// wrong shape.
///
/// The doc comment on `bind` claimed a panic until 2026-08-21 and the code
/// never panicked. Making the code match the comment was the wrong direction:
/// `query`, `execute` and the `_raw` forms all return `Error::parameters` for
/// the identical mistake, so panicking here would have made the outcome depend
/// on which method the caller reached for. The comment was corrected instead,
/// and this test is what keeps the two from drifting apart again.
///
/// `catch_unwind` is deliberate. Asserting only on the `Err` would still pass
/// if someone reintroduced the panic, because the panic would abort the test
/// before the assertion ran and `#[compio::test]` would report the failure as
/// a panic rather than as this claim being violated.
#[compio::test]
async fn bind_reports_parameter_count_mismatch_as_an_error() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let transaction = client.transaction().await.unwrap();
        let statement = transaction.prepare("SELECT $1::int4").await.unwrap();

        let outcome = AssertUnwindSafe(transaction.bind(&statement, &[]))
            .catch_unwind()
            .await;

        let complaint = match outcome {
            Err(_) => Some("panicked".to_string()),
            Ok(Ok(_)) => Some("returned Ok(Portal) for a statement expecting 1 parameter".to_string()),
            Ok(Err(error)) => {
                // The counts both appear, and in the right roles: one
                // parameter expected, none supplied.
                let chain = common::error_chain(&error);
                if chain.contains("expected 1 parameters but got 0") {
                    None
                } else {
                    Some(format!("returned Err({chain}), which does not name both counts"))
                }
            }
        };

        // The failed bind must not have left the transaction unusable.
        let still_alive: i32 = transaction
            .query_one("SELECT 7::int4", &[])
            .await
            .expect("the transaction stopped working after a rejected bind")
            .get(0);
        assert_eq!(still_alive, 7);
        transaction.rollback().await.unwrap();

        if let Some(complaint) = complaint {
            panic!("Transaction::bind {complaint}; it must report arity mistakes as an Err");
        }
    })
    .await
    .expect("bind arity claim test exceeded its 10 second deadline");
}

#[compio::test]
async fn successful_commit_clears_dirty_after_nested_savepoint_drop() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

        let mut transaction = client.transaction().await.unwrap();
        let nested = transaction.transaction().await.unwrap();
        drop(nested);
        assert!(
            transaction.client().is_dirty(),
            "dropping the nested savepoint did not mark its client dirty"
        );

        transaction.commit().await.unwrap();
        assert!(
            !client.is_dirty(),
            "a successful commit left the nested rollback's dirty flag set"
        );
    })
    .await
    .expect("commit dirty-state claim test exceeded its 10 second deadline");
}

#[compio::test]
async fn successful_rollback_clears_dirty_after_nested_savepoint_drop() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

        let mut transaction = client.transaction().await.unwrap();
        let nested = transaction.transaction().await.unwrap();
        drop(nested);
        assert!(
            transaction.client().is_dirty(),
            "dropping the nested savepoint did not mark its client dirty"
        );

        transaction.rollback().await.unwrap();
        assert!(
            !client.is_dirty(),
            "a successful rollback left the nested rollback's dirty flag set"
        );
    })
    .await
    .expect("rollback dirty-state claim test exceeded its 10 second deadline");
}

#[compio::test]
async fn panicking_transaction_start_setup_preserves_a_preexisting_transaction() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let table = common::test_object_name("cpg_panicking_start");
        client
            .batch_execute(&format!(
                "CREATE TEMP TABLE {table} (value int NOT NULL); BEGIN; \
                 INSERT INTO {table} VALUES (1)"
            ))
            .await
            .unwrap();

        arm_start_log_panic();
        let panic = AssertUnwindSafe(client.build_transaction().start())
            .catch_unwind()
            .await;
        // `assert_injected_panic` consumes the outcome, which is what releases
        // the borrow on `client` that the old `drop(panic)` here existed for.
        assert_injected_panic(panic, "panic requested before START TRANSACTION was encoded");

        client.simple_query("").await.unwrap();
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::InTransaction),
            "panicking START setup rolled back the pre-existing transaction"
        );
        let count: i64 = client
            .query_one_scalar(&format!("SELECT count(*)::int8 FROM {table}"), &[])
            .await
            .expect("the pre-existing transaction was unusable after START setup panicked");
        assert_eq!(count, 1, "panicking START setup discarded existing writes");
        client.batch_execute("ROLLBACK").await.unwrap();
    })
    .await
    .expect("panicking transaction-start test exceeded its watchdog");
}

#[compio::test]
async fn panicking_commit_setup_rolls_back_before_the_next_operation() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let table = common::test_object_name("cpg_panicking_commit");
        client
            .batch_execute(&format!("CREATE TEMP TABLE {table} (value int NOT NULL)"))
            .await
            .unwrap();

        let transaction = client.transaction().await.unwrap();
        transaction
            .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
            .await
            .unwrap();

        arm_commit_log_panic();
        let panic = AssertUnwindSafe(transaction.commit()).catch_unwind().await;
        assert_injected_panic(panic, "panic requested before COMMIT was encoded");

        client.simple_query("").await.unwrap();
        let count: i64 = client
            .query_one_scalar(&format!("SELECT count(*)::int8 FROM {table}"), &[])
            .await
            .expect("the operation after the panicking commit was unusable");
        assert_eq!(count, 0, "the panicking commit left its writes live");
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle),
            "the panicking commit left the next operation inside its transaction"
        );
    })
    .await
    .expect("panicking commit cleanup test exceeded its watchdog");
}

#[compio::test]
async fn panicking_rollback_setup_rolls_back_before_the_next_operation() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let table = common::test_object_name("cpg_panicking_rollback");
        client
            .batch_execute(&format!("CREATE TEMP TABLE {table} (value int NOT NULL)"))
            .await
            .unwrap();

        let transaction = client.transaction().await.unwrap();
        transaction
            .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
            .await
            .unwrap();

        arm_rollback_log_panic();
        let panic = AssertUnwindSafe(transaction.rollback())
            .catch_unwind()
            .await;
        assert_injected_panic(panic, "panic requested before ROLLBACK was encoded");

        client.simple_query("").await.unwrap();
        let count: i64 = client
            .query_one_scalar(&format!("SELECT count(*)::int8 FROM {table}"), &[])
            .await
            .expect("the operation after the panicking rollback was unusable");
        assert_eq!(count, 0, "the panicking rollback left its writes live");
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle),
            "the panicking rollback left the next operation inside its transaction"
        );
    })
    .await
    .expect("panicking rollback cleanup test exceeded its watchdog");
}

#[compio::test]
async fn abandoned_savepoint_creation_cleans_its_server_name() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let name = common::test_object_name("cpg_abandoned_savepoint_creation");
        let mut transaction = client.transaction().await.unwrap();

        {
            let mut creation = Box::pin(transaction.savepoint(name.clone()));
            let mut context = Context::from_waker(Waker::noop());
            assert!(
                matches!(creation.as_mut().poll(&mut context), Poll::Pending),
                "savepoint creation completed before its queued request could be abandoned"
            );
        }

        transaction.simple_query("").await.unwrap();
        let release = transaction
            .batch_execute(&format!("RELEASE SAVEPOINT {name}"))
            .await
            .expect_err("the abandoned savepoint name remained defined on the server");
        assert_eq!(
            release.code(),
            Some(&compio_postgres::error::SqlState::S_E_INVALID_SPECIFICATION),
            "RELEASE failed for a reason other than the savepoint being absent: {}",
            common::error_chain(&release)
        );

        transaction.rollback().await.unwrap();
        let value: i32 = client
            .query_one_scalar("SELECT 42::int4", &[])
            .await
            .expect("the cleanup left the connection unusable");
        assert_eq!(value, 42);
    })
    .await
    .expect("abandoned savepoint cleanup test exceeded its watchdog");
}


#[compio::test]
async fn nonpositive_portal_limits_return_every_row() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let transaction = client.transaction().await.unwrap();
        let statement = transaction
            .prepare("SELECT i::int4 FROM generate_series(1, 5) AS i ORDER BY i")
            .await
            .unwrap();

        for max_rows in [0, -1] {
            let portal = transaction.bind(&statement, &[]).await.unwrap();
            let rows = transaction.query_portal(&portal, max_rows).await.unwrap();
            let values = rows
                .iter()
                .map(|row| row.get::<_, i32>(0))
                .collect::<Vec<_>>();
            assert_eq!(
                values,
                [1, 2, 3, 4, 5],
                "query_portal({max_rows}) did not return every row"
            );
        }

        transaction.rollback().await.unwrap();
    })
    .await
    .expect("portal row-limit claim test exceeded its 10 second deadline");
}

#[compio::test]
async fn abandoned_failed_nested_commit_recovers_before_the_next_outer_operation() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let name = common::test_object_name("cpg_failed_nested_commit");
        let mut transaction = client.transaction().await.unwrap();
        let nested = transaction.savepoint(name).await.unwrap();
        let failure = nested
            .batch_execute("SELECT 1 / 0")
            .await
            .expect_err("the nested transaction did not enter its aborted state");
        assert_eq!(
            failure.code(),
            Some(&compio_postgres::error::SqlState::DIVISION_BY_ZERO)
        );

        {
            let mut commit = Box::pin(nested.commit());
            let mut context = Context::from_waker(Waker::noop());
            assert!(
                matches!(commit.as_mut().poll(&mut context), Poll::Pending),
                "failed nested commit completed before it could be abandoned"
            );
        }

        let value: i32 = transaction
            .query_one("SELECT 7::int4", &[])
            .await
            .expect("failed nested commit recovery did not precede the next outer operation")
            .get(0);
        assert_eq!(value, 7);
        transaction.rollback().await.unwrap();
    })
    .await
    .expect("failed nested commit abandonment test exceeded its watchdog");
}

/// How many of this test's portals the server currently reports.
///
/// One query text, used for both the control and the claim, so the two cannot
/// drift into asking different questions - which is the failure that would make
/// the control stop guarding anything.
async fn portal_count(transaction: &compio_postgres::Transaction<'_>) -> i64 {
    transaction
        .query_one(
            "SELECT count(*)::int8 FROM pg_cursors \
             WHERE name LIKE 'p%' \
             AND statement LIKE '%cpg_abandoned_bind_portal%'",
            &[],
        )
        .await
        .expect("could not inspect portals")
        .get(0)
}

#[compio::test]
async fn abandoning_bind_before_bind_complete_closes_the_server_portal() {
    const SQL: &str = "SELECT 83::int4 /* cpg_abandoned_bind_portal */";
    const BARRIER: &str = "SELECT 84::int4 /* cpg_abandoned_bind_barrier */";

    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let transaction = client.transaction().await.unwrap();
        let statement = transaction.prepare(SQL).await.unwrap();

        // THE PROBE MUST BE ABLE TO SEE A PORTAL, checked before it is used to
        // conclude that none exists. The assertion below is `count == 0` over
        // `pg_cursors WHERE name LIKE 'p%' AND statement LIKE '%...%'` - two
        // predicates about how the driver names portals and how PostgreSQL
        // reports them, neither of which the test controls. If either stopped
        // matching, the count would be 0 for a reason that has nothing to do
        // with cleanup and the test would pass while measuring nothing. So
        // bind one and require the probe to find it. Measured 2026-08-23: it
        // does, and this now fails rather than going quiet if that changes.
        {
            let live = transaction
                .bind(&statement, &[])
                .await
                .expect("bind a portal the probe is meant to find");
            let visible: i64 = portal_count(&transaction).await;
            assert_eq!(
                visible, 1,
                "the pg_cursors probe cannot see a portal that certainly exists, so a zero \
                 below would say nothing about the abandoned one"
            );
            drop(live);
        }

        let mut bind = Box::pin(transaction.bind(&statement, &[]));
        let mut context = Context::from_waker(Waker::noop());
        assert!(
            matches!(bind.as_mut().poll(&mut context), Poll::Pending),
            "bind completed before its queued response could be abandoned"
        );
        drop(bind);

        transaction.simple_query(BARRIER).await.unwrap();
        let leaked: i64 = portal_count(&transaction).await;
        assert_eq!(leaked, 0, "an abandoned bind left its named portal alive");

        transaction.rollback().await.unwrap();
    })
    .await
    .expect("abandoned-bind cleanup claim exceeded its watchdog");
}

/// A suspended portal resumes where it left off, and an exhausted one is empty
/// rather than an error.
///
/// The only place three successive `query_portal` calls appear today is an
/// OBSERVER test, and it asserts the chunk SIZES -- `[2, 2, 1]`. That is a
/// proxy for "the portal continued": it rules out a portal that restarts every
/// time, because the sizes would read `[2, 2, 2]`, but it says nothing about
/// WHICH rows each chunk carried. Nothing in the tree asserts the values.
///
/// The fourth call is the edge nobody states. Once the portal is drained the
/// server answers `CommandComplete` with no rows, and a caller draining a
/// cursor in a loop needs that to be an empty result rather than an error or a
/// hang -- which is exactly the shape of the loop anyone writes around this API.
#[compio::test]
async fn a_suspended_portal_resumes_and_then_empties() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let transaction = client.transaction().await.unwrap();
        let statement = transaction
            .prepare("SELECT i::int4 FROM generate_series(1, 5) AS i ORDER BY i")
            .await
            .unwrap();
        let portal = transaction.bind(&statement, &[]).await.unwrap();

        let mut chunks = Vec::new();
        for _ in 0..4 {
            let rows = transaction
                .query_portal(&portal, 2)
                .await
                .expect("a portal fetch must not fail, drained or not");
            chunks.push(rows.iter().map(|row| row.get::<_, i32>(0)).collect::<Vec<_>>());
        }

        assert_eq!(
            chunks,
            vec![vec![1, 2], vec![3, 4], vec![5], Vec::<i32>::new()],
            "the portal did not resume where it left off, or the drained fetch \
             was not empty"
        );
    })
    .await
    .expect("portal resume test exceeded its watchdog");
}

/// Count this test's rows from a SESSION THAT IS NOT THE ONE UNDER TEST.
///
/// Asking the committing session whether its own rows survived is the question
/// that lies consistently: it would read its own snapshot either way. A second
/// connection can only see rows a real COMMIT made durable.
async fn rows_visible_to_another_session(url: &str, table: &str) -> i64 {
    let observer = connect(url)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    observer
        .query_one(&format!("SELECT count(*)::int8 FROM \"{table}\""), &[])
        .await
        .expect("the observer session could not count the rows")
        .get(0)
}

/// `Transaction::commit` must not answer `Ok` for a transaction PostgreSQL
/// threw away.
///
/// A `COMMIT` sent inside an aborted transaction block is not an error:
/// PostgreSQL runs it, discards every change, and answers `CommandComplete`
/// with the tag `ROLLBACK` (measured with psql - `COMMIT;` after `SELECT 1/0`
/// prints `ROLLBACK`). `finish_batch_execute` drops command tags on the floor,
/// so the only signal that the write was lost never reached the caller and
/// `commit()` returned `Ok(())` over discarded data.
///
/// The nested form of the same mistake already reported an error - the
/// savepoint arm of `commit` inspects `transaction_status()` and cleans up - so
/// a caller's outcome depended on whether the transaction happened to be a
/// savepoint.
#[compio::test]
async fn commit_after_a_failed_statement_reports_that_the_server_rolled_back() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let table = common::test_object_name("cpg_commit_after_failure");
        client
            .batch_execute(&format!("CREATE TABLE \"{table}\" (id int4 PRIMARY KEY)"))
            .await
            .unwrap();

        let transaction = client.transaction().await.unwrap();
        transaction
            .batch_execute(&format!("INSERT INTO \"{table}\" VALUES (1)"))
            .await
            .unwrap();
        let failure = transaction
            .batch_execute("SELECT 1 / 0")
            .await
            .expect_err("the transaction did not enter its aborted state");
        assert_eq!(
            failure.code(),
            Some(&compio_postgres::error::SqlState::DIVISION_BY_ZERO)
        );

        let outcome = transaction.commit().await;

        let surviving = rows_visible_to_another_session(&url, &table).await;
        client
            .batch_execute(&format!("DROP TABLE \"{table}\""))
            .await
            .unwrap();

        assert_eq!(
            surviving, 0,
            "the fixture is wrong: PostgreSQL kept the row, so there is no lost \
             write for commit() to under-report"
        );
        let error = outcome.expect_err(
            "commit() reported success for a transaction PostgreSQL rolled back, \
             and the row it claimed to commit is gone",
        );
        assert!(
            error.is_transaction_rolled_back(),
            "commit() failed for some other reason, so this test is not about \
             the discarded transaction: {}",
            common::error_chain(&error)
        );
    })
    .await
    .expect("commit-after-failure claim exceeded its watchdog");
}

/// The control for the claim above, differing in ONE variable: nothing in this
/// transaction failed.
///
/// A `commit` that reports an error for every transaction, or that reads the
/// command tag inverted, turns this red.
#[compio::test]
async fn commit_without_a_failed_statement_still_commits() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let table = common::test_object_name("cpg_commit_clean");
        client
            .batch_execute(&format!("CREATE TABLE \"{table}\" (id int4 PRIMARY KEY)"))
            .await
            .unwrap();

        let transaction = client.transaction().await.unwrap();
        transaction
            .batch_execute(&format!("INSERT INTO \"{table}\" VALUES (1)"))
            .await
            .unwrap();
        let outcome = transaction.commit().await;

        let surviving = rows_visible_to_another_session(&url, &table).await;
        client
            .batch_execute(&format!("DROP TABLE \"{table}\""))
            .await
            .unwrap();

        outcome.expect("a transaction with no failed statement must commit");
        assert_eq!(
            surviving, 1,
            "the committed row is not visible to another session"
        );
    })
    .await
    .expect("clean-commit control exceeded its watchdog");
}

/// The second control, differing from the claim in ONE variable: the commit is
/// a savepoint's, so the server answers `RELEASE` rather than `COMMIT`.
///
/// It pins the SHAPE of the fix as much as the outcome. A nested `commit`
/// sends `RELEASE`, whose tag is `RELEASE` (measured with psql), so the
/// inverted spelling of the check - "the tag was not COMMIT" - rejects this
/// healthy nested commit while leaving the claim above green.
#[compio::test]
async fn a_healthy_nested_commit_is_not_mistaken_for_a_rollback() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let mut client = connect(&url)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let table = common::test_object_name("cpg_nested_commit_tag");
        client
            .batch_execute(&format!("CREATE TABLE \"{table}\" (id int4 PRIMARY KEY)"))
            .await
            .unwrap();

        let mut transaction = client.transaction().await.unwrap();
        // Both outcomes are carried past the DROP rather than asserted where
        // they are produced: this test creates a real table in a database other
        // suites share, and an assertion that fires between CREATE and DROP
        // leaves it behind.
        let nested_outcome = {
            let nested = transaction.transaction().await.unwrap();
            nested
                .batch_execute(&format!("INSERT INTO \"{table}\" VALUES (1)"))
                .await
                .unwrap();
            nested.commit().await
        };
        let outcome = transaction.commit().await;

        let surviving = rows_visible_to_another_session(&url, &table).await;
        client
            .batch_execute(&format!("DROP TABLE \"{table}\""))
            .await
            .unwrap();

        nested_outcome.expect("a nested commit over a healthy savepoint must succeed");
        outcome.expect("the outer commit must succeed");
        assert_eq!(surviving, 1, "the nested-then-outer commit lost its row");
    })
    .await
    .expect("nested-commit control exceeded its watchdog");
}
