//! SQLite transactions contracts.
use super::fixtures::*;

use crate::tests::fixtures::Host;

use zeroship_data_orm::backend::sqlite::reservation::{CancelCleanup, TerminalOutcome};

use zeroship_data_orm::backend::sqlite::session::TerminalIntent;

use zeroship_data_orm::error::DbError;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

/// Inner savepoint rolled back to → only the outer write survives the
/// COMMIT. Mirrors `nested_inner_reject_rolls_back_to_savepoint_outer_continues`
/// at the SQL level.
#[test]
fn nested_savepoint_rollback_to_keeps_outer_sqlite() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let client = backend
                .fixture_session("default")
                .await
                .expect("acquire client");

            backend
                .execute_fixture_on(
                    &client,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            // Top-level BEGIN (what the orchestrator emits for a non-nested tx).
            backend
                .execute_fixture_on(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            backend
                .execute_fixture_on(&client, "INSERT INTO notes (title) VALUES ('outer')", &[])
                .await
                .expect("outer insert");

            // Nested transaction → SAVEPOINT. The literal name here is this arm's
            // own, NOT the orchestrator's: dispatch emits `zs_sp_<frame sequence>`
            // minted by `reducer::frames::FrameStack`, which never derives a name
            // from the depth and never reuses one. What this arm rules on is the
            // SQLite engine's savepoint semantics, which are name-agnostic.
            backend
                .execute_fixture_on(&client, "SAVEPOINT zs_sp_1", &[])
                .await
                .expect("SAVEPOINT");
            backend
                .execute_fixture_on(
                    &client,
                    "INSERT INTO notes (title) VALUES ('inner-doomed')",
                    &[],
                )
                .await
                .expect("inner insert");
            // Inner callback rejected → ROLLBACK TO SAVEPOINT (inner reverts,
            // outer tx continues — not poisoned).
            backend
                .execute_fixture_on(&client, "ROLLBACK TO SAVEPOINT zs_sp_1", &[])
                .await
                .expect("ROLLBACK TO SAVEPOINT");

            // Outer continues + COMMITs.
            backend
                .execute_fixture_on(&client, "INSERT INTO notes (title) VALUES ('outer-2')", &[])
                .await
                .expect("outer insert 2 after savepoint rollback");
            backend
                .execute_fixture_on(&client, "COMMIT", &[])
                .await
                .expect("COMMIT");

            // Only the two outer rows survive; the inner row was rolled back
            // to the savepoint.
            let rows = client
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count");
            assert_eq!(
                rows[0][0].as_deref(),
                Some("2"),
                "inner SAVEPOINT row must be reverted by ROLLBACK TO; both outer rows survive"
            );
            let titles = client
                .query("SELECT title FROM notes ORDER BY id", &[])
                .await
                .expect("titles");
            assert_eq!(titles[0][0].as_deref(), Some("outer"));
            assert_eq!(titles[1][0].as_deref(), Some("outer-2"));
        });
    })
}

/// Inner savepoint released → both inner and outer writes persist after
/// COMMIT. Mirrors `nested_inner_resolve_releases_savepoint`.
#[test]
fn nested_savepoint_release_keeps_both_sqlite() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let client = backend
                .fixture_session("default")
                .await
                .expect("acquire client");

            backend
                .execute_fixture_on(
                    &client,
                    "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)",
                    &[],
                )
                .await
                .expect("create table");

            backend
                .execute_fixture_on(&client, "BEGIN", &[])
                .await
                .expect("BEGIN");
            backend
                .execute_fixture_on(&client, "SAVEPOINT zs_sp_1", &[])
                .await
                .expect("SAVEPOINT");
            backend
                .execute_fixture_on(
                    &client,
                    "INSERT INTO notes (title) VALUES ('inner-kept')",
                    &[],
                )
                .await
                .expect("inner insert");
            // Inner callback resolved → RELEASE SAVEPOINT.
            backend
                .execute_fixture_on(&client, "RELEASE SAVEPOINT zs_sp_1", &[])
                .await
                .expect("RELEASE SAVEPOINT");
            backend
                .execute_fixture_on(
                    &client,
                    "INSERT INTO notes (title) VALUES ('outer-kept')",
                    &[],
                )
                .await
                .expect("outer insert");
            backend
                .execute_fixture_on(&client, "COMMIT", &[])
                .await
                .expect("COMMIT");

            let rows = client
                .query("SELECT COUNT(*) FROM notes", &[])
                .await
                .expect("count");
            assert_eq!(
                rows[0][0].as_deref(),
                Some("2"),
                "RELEASE SAVEPOINT then COMMIT must persist both the inner and outer rows"
            );
        });
    })
}

/// A statement that takes far longer than any assertion window below.
///
/// A recursive CTE counting to 400 million: pure CPU inside SQLite's VDBE with
/// no I/O, so `sqlite3_interrupt` is the only thing that ends it early.
const LONG_RUNNING_SQL: &str = "WITH RECURSIVE c(x) AS (\
     SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 400000000\
   ) SELECT COUNT(*) FROM c";

/// The concurrency arm: app A's autocommit **reads** proceed while A holds an
/// open explicit transaction, and do not observe its uncommitted write.
///
/// It says reads deliberately. WAL gives concurrent readers, not concurrent
/// writers: an autocommit *write* issued here would contend for the single
/// write lock `tx_conn` is holding and wait out `busy_timeout`. That limit is
/// real on any number of connections and SC-2 states it in the same breath.
#[test]
fn an_autocommit_read_proceeds_while_the_app_holds_an_open_transaction() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let probe = backend.autocommit_client();
            backend
                .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
                .await
                .expect("create table");
            backend
                .execute_fixture("INSERT INTO t (v) VALUES ('committed')", &[])
                .await
                .expect("seed");

            let tx = backend
                .fixture_session("default")
                .await
                .expect("acquire tx client");
            backend
                .execute_fixture_on(&tx, "BEGIN", &[])
                .await
                .expect("BEGIN");
            backend
                .execute_fixture_on(&tx, "INSERT INTO t (v) VALUES ('uncommitted')", &[])
                .await
                .expect("write inside the transaction (takes the write lock)");

            // The read runs on op_conn while tx_conn holds an open write
            // transaction. Before SC-2 it ran on that same connection and saw the
            // uncommitted row.
            let rows = probe
                .query("SELECT v FROM t ORDER BY id", &[])
                .await
                .expect("autocommit read while a transaction is open");
            assert_eq!(
                rows.len(),
                1,
                "the autocommit read must not observe the open transaction's \
             uncommitted write; got {rows:?}"
            );
            assert_eq!(rows[0][0].as_deref(), Some("committed"));

            backend
                .execute_fixture_on(&tx, "ROLLBACK", &[])
                .await
                .expect("ROLLBACK");
        });
    })
}

/// A command naming a reservation that does not own its connection is refused
/// with a typed error - not run on whatever connection is free.
#[test]
fn a_command_bearing_a_foreign_reservation_is_refused() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
                .await
                .expect("create table");

            let foreign = backend.unregistered_transaction_client_for_tests();
            let err = backend
                .execute_fixture_on(&foreign, "INSERT INTO t (v) VALUES ('leaked')", &[])
                .await
                .expect_err("a foreign reservation must be refused");
            match &err {
                DbError::ValidationFailed { code, .. } => {
                    assert_eq!(*code, "reservation_not_owner", "got {err:?}");
                }
                other => panic!("expected a typed reservation refusal, got {other:?}"),
            }

            // The refusal has to be a refusal, not a warning: nothing ran.
            let rows = backend
                .autocommit_client()
                .query("SELECT COUNT(*) FROM t", &[])
                .await
                .expect("count after the refusal");
            assert_eq!(
                rows[0][0].as_deref(),
                Some("0"),
                "the refused command must not have executed"
            );
        });
    })
}

/// The interrupt arm: cancellation takes effect **during** a long-running
/// statement, not after it.
///
/// Pre-SC-2 the actor's own comment said the opposite - "the SQL has already
/// committed (or rolled back) by then" - because nothing could reach a running
/// statement.
///
/// **What proves "during" is the cleanup, not the error and not the clock.**
/// This doc comment used to say the error proved it - that `statement_cancelled`
/// "can only come from `SQLITE_INTERRUPT`". It cannot: the *pre-start* path
/// (`enter_running` refusing, `cancelled_before_start`) produces
/// `Cancelled { NoSqlStarted }`, whose `into_result` carries the identical
/// `statement_cancelled` code, and `matches!(outcome, Cancelled { .. })` is
/// satisfied by both. The only thing that separates them is the `cleanup`
/// field, so this arm asserts on it: `RolledBack` / `AlreadyRolledBack` means
/// SQL was in flight and something had to be undone, `NoSqlStarted` means the
/// statement never began - a different code path reaching the same error code,
/// ruled on by `a_cancel_before_execution_starts_stops_the_actor_from_running`
/// in `backend::sqlite::reservation`'s unit tests. The elapsed-time assertion
/// stays a backstop for the case where nothing interrupts and the test would
/// otherwise sit for minutes; it is not the discriminator, and it cannot be -
/// a pre-start cancellation returns *faster*, not slower.
#[test]
fn a_cancellation_interrupts_a_statement_that_is_already_running() {
    Host::test(|host| {
        host.run(async {
            use std::time::{Duration, Instant};

            let (backend, _dir) = fresh_backend(host);
            let tx = backend
                .fixture_session("default")
                .await
                .expect("acquire tx client");
            let cancel = tx
                .cancel_handle()
                .expect("a transaction handle must expose a cancel handle");

            let runner = tx.clone();
            let query =
                compio::runtime::spawn(async move { runner.query(LONG_RUNNING_SQL, &[]).await });

            // Let the actor reach `Running` and start stepping. The protocol does
            // not depend on this sleep - the progress latch covers the window
            // where `Running` is stored but SQLite has not stepped yet - but
            // sleeping first is what makes this test exercise the *interrupt*
            // path rather than the pre-start path, which has its own arm.
            compio::time::sleep(Duration::from_millis(300)).await;

            let started = Instant::now();
            let outcome = cancel.cancel().await.expect("cancel acknowledged");
            let query_result = query.await.expect("query task joined");
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(12),
                "cancellation did not interrupt the running statement; it took {elapsed:?}"
            );
            let err = query_result.expect_err("an interrupted query must not return rows");
            match &err {
                DbError::Coded { code, .. } => assert_eq!(
                    code, "statement_cancelled",
                    "an interrupt must surface as a cancellation, not an opaque \
                 database error; got {err:?}"
                ),
                other => panic!("expected the cancellation code, got {other:?}"),
            }
            let TerminalOutcome::Cancelled { cleanup, .. } = &outcome else {
                panic!(
                    "the actor must acknowledge a cancellation after rolling back; \
                 got {outcome:?}"
                );
            };
            assert!(
                matches!(
                    cleanup,
                    CancelCleanup::RolledBack | CancelCleanup::AlreadyRolledBack
                ),
                "this arm claims the statement was interrupted mid-execution, so the \
             cancellation had something to undo. `NoSqlStarted` here would mean the \
             pre-start path ran instead - the same error code, a different code \
             path, and nothing about the interrupt proved. got {cleanup:?}"
            );
        });
    })
}

/// SC-2 case 3: a cancellation arriving after the outcome was decided is a
/// question, not a command.
///
/// The transaction commits; only then is it cancelled. The commit must stand
/// and the cancellation must report `AlreadyCompleted` - a naive implementation
/// sends `ROLLBACK` here and destroys a durable write.
#[test]
fn a_cancellation_after_commit_does_not_roll_the_commit_back() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
                .await
                .expect("create table");

            let tx = backend
                .fixture_session("default")
                .await
                .expect("acquire tx client");
            let cancel = tx.cancel_handle().expect("cancel handle");
            backend
                .execute_fixture_on(&tx, "BEGIN", &[])
                .await
                .expect("BEGIN");
            backend
                .execute_fixture_on(&tx, "INSERT INTO t (v) VALUES ('durable')", &[])
                .await
                .expect("insert");
            let committed = backend
                .settle_transaction_for_tests(&tx, TerminalIntent::Commit)
                .await
                .expect("commit");
            assert_eq!(committed, TerminalOutcome::Committed);

            let outcome = cancel.cancel().await.expect("cancel acknowledged");
            assert!(
                matches!(outcome, TerminalOutcome::AlreadyCompleted(_)),
                "a cancellation after the terminal was claimed must report \
             AlreadyCompleted; got {outcome:?}"
            );

            let rows = backend
                .autocommit_client()
                .query("SELECT v FROM t", &[])
                .await
                .expect("read after the late cancellation");
            assert_eq!(
                rows.len(),
                1,
                "the committed write must survive a cancellation that arrived \
             after the commit; got {rows:?}"
            );
            assert_eq!(rows[0][0].as_deref(), Some("durable"));
        });
    })
}

/// **The data-destroying shape, with no duplicate cancel anywhere.**
///
/// A lease dropped without settling retires through `Release`/`unbind_tx`, and
/// that path does not claim the reservation's terminal - it stays `PENDING`. A
/// cancel handle taken from that lease therefore still wins its claim later,
/// arbitrarily far in the future. By then `tx_conn` can belong to an entirely
/// different transaction, and `run_cancel` used to issue its `ROLLBACK`
/// unconditionally: it destroyed the *current* owner's writes.
///
/// The second half of the damage is the part a creator sees. The stale cancel
/// leaves the current owner's `tx_bound` untouched, so its `COMMIT` still
/// runs - onto a connection SQLite has already returned to autocommit. That
/// commit errors, `classify_commit` correctly refuses to guess, and the
/// creator is told `commit_indeterminate`: "nobody knows whether your write
/// landed", for a write that was silently rolled back.
///
/// Nothing here cancels twice, so `claim_cancelled`'s idempotency does not
/// close it. Ownership is what closes it.
#[test]
fn a_cancel_for_a_retired_reservation_does_not_roll_back_the_next_transaction() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
                .await
                .expect("create table");

            // R1 takes the lane, writes, and is dropped WITHOUT settling. Its
            // cancel handle outlives it - which is the whole point: a guard held
            // by a dropped future is exactly how SC-1 step 9 will arm this.
            let stale_cancel = {
                let first = backend
                    .fixture_session("default")
                    .await
                    .expect("acquire the first transaction");
                let cancel = first.cancel_handle().expect("cancel handle for R1");
                backend
                    .execute_fixture_on(&first, "BEGIN", &[])
                    .await
                    .expect("BEGIN on R1");
                backend
                    .execute_fixture_on(&first, "INSERT INTO t (v) VALUES ('r1')", &[])
                    .await
                    .expect("write inside R1");
                cancel
            };

            // R2 takes the lane R1 gave up, and opens its own transaction.
            let second = backend
                .fixture_session("default")
                .await
                .expect("acquire the second transaction");
            backend
                .execute_fixture_on(&second, "BEGIN", &[])
                .await
                .expect("BEGIN on R2");
            backend
                .execute_fixture_on(&second, "INSERT INTO t (v) VALUES ('r2')", &[])
                .await
                .expect("write inside R2");

            // The stale cancellation lands while R2's transaction is open.
            let stale_outcome = stale_cancel
                .cancel()
                .await
                .expect("the actor must answer a stale cancellation, not hang");
            assert_eq!(
                stale_outcome,
                TerminalOutcome::Cancelled {
                    cleanup: CancelCleanup::AlreadyRetired,
                    cause: None
                },
                "a cancellation for a reservation that no longer owns tx_conn must report \
             that it cleaned up nothing. `RolledBack` here is the failure: the only \
             transaction there to roll back belongs to somebody else. got \
             {stale_outcome:?}"
            );

            // R2 must be untouched: its COMMIT is a real commit, not an
            // indeterminate one, and its row is on disk.
            let committed = backend
                .settle_transaction_for_tests(&second, TerminalIntent::Commit)
                .await
                .expect("settle R2");
            assert_eq!(
                committed,
                TerminalOutcome::Committed,
                "R2's commit must be a confirmed commit. A stale cancellation that rolled \
             its transaction back leaves SQLite in autocommit, so COMMIT errors and \
             this reports CommitIndeterminate - the creator is told the fate is \
             unknown for a write that was destroyed."
            );

            let rows = backend
                .autocommit_client()
                .query("SELECT v FROM t ORDER BY id", &[])
                .await
                .expect("read after the stale cancellation");
            assert_eq!(
                rows.len(),
                1,
                "exactly R2's row should survive: R1's died with its unsettled lease, \
             R2's must survive the stale cancel; got {rows:?}"
            );
            assert_eq!(rows[0][0].as_deref(), Some("r2"));
        });
    })
}

/// A cancellation is a claim on one terminal, and a claim can be won once.
///
/// `claim_cancelled` used to return `true` for a terminal already reading
/// `CLAIMED_CANCELLED`, so a second `Cancel` re-entered the cleanup path and
/// issued a second `ROLLBACK` on the lane. Here the second cancel must instead
/// be answered with what the first one decided.
#[test]
fn a_second_cancellation_is_answered_not_re_executed() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
                .await
                .expect("create table");

            let tx = backend
                .fixture_session("default")
                .await
                .expect("acquire tx client");
            let cancel = tx.cancel_handle().expect("cancel handle");
            backend
                .execute_fixture_on(&tx, "BEGIN", &[])
                .await
                .expect("BEGIN");
            backend
                .execute_fixture_on(&tx, "INSERT INTO t (v) VALUES ('doomed')", &[])
                .await
                .expect("write inside the transaction");

            let first = cancel.cancel().await.expect("first cancel acknowledged");
            assert!(
                matches!(first, TerminalOutcome::Cancelled { .. }),
                "the first cancellation wins the terminal and rolls back; got {first:?}"
            );

            let second = cancel.cancel().await.expect("second cancel acknowledged");
            let TerminalOutcome::AlreadyCompleted(inner) = &second else {
                panic!(
                    "a second cancellation must be told what the first one decided, not \
                 granted the terminal again; got {second:?}"
                );
            };
            assert!(
                matches!(**inner, TerminalOutcome::Cancelled { .. }),
                "and the answer it is told must be the first cancellation's own \
             outcome; got {inner:?}"
            );
        });
    })
}

/// The autocommit lane has an ownership rule too, and it is a *lifetime* rule:
/// one reservation, one command.
#[test]
fn a_spent_autocommit_reservation_is_refused_as_a_non_owner() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
                .await
                .expect("create table");

            let spent = backend.spent_autocommit_reservation_for_tests();
            backend
                .exec_on_reservation_for_tests(&spent, "INSERT INTO t (v) VALUES ('first')", &[])
                .await
                .expect("the reservation's one command must run");

            let err = backend
                .exec_on_reservation_for_tests(&spent, "INSERT INTO t (v) VALUES ('second')", &[])
                .await
                .expect_err("a spent autocommit reservation must be refused");
            match &err {
                DbError::ValidationFailed { code, .. } => assert_eq!(
                    *code, "reservation_not_owner",
                    "a stale reservation is an ownership failure, not a cancellation. \
                 `statement_cancelled` here names the wrong thing: nothing was \
                 cancelled. got {err:?}"
                ),
                other => panic!("expected a typed reservation refusal, got {other:?}"),
            }

            let rows = backend
                .autocommit_client()
                .query("SELECT COUNT(*) FROM t", &[])
                .await
                .expect("count after the refusal");
            assert_eq!(
                rows[0][0].as_deref(),
                Some("1"),
                "the refusal must be a refusal: only the first command ran"
            );
        });
    })
}

/// Two apps hold open transactions at the same time on one session.
///
/// **This is the arm that fails before the per-app lane.** Against the shared
/// connection app B's acquire returned
/// `transaction_connection_busy: "db: this SQLite session already holds an open
/// transaction on tx_conn"` - a refusal caused entirely by another tenant.
#[test]
fn two_apps_hold_transactions_at_the_same_time() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_a")
                .await
                .expect("attach app_a");
            backend
                .attach_app_file("app_b")
                .await
                .expect("attach app_b");
            for app in ["app_a", "app_b"] {
                backend
                    .execute_fixture(
                        &format!("CREATE TABLE \"{app}\".\"t\" (id INTEGER PRIMARY KEY, v TEXT)"),
                        &[],
                    )
                    .await
                    .expect("create table");
            }

            let a = backend
                .fixture_session("app_a")
                .await
                .expect("app_a acquires its transaction connection");
            backend
                .execute_fixture_on(&a, "BEGIN", &[])
                .await
                .expect("BEGIN a");
            backend
                .execute_fixture_on(&a, "INSERT INTO \"app_a\".\"t\" (v) VALUES ('a')", &[])
                .await
                .expect("app_a writes inside its transaction");

            // The whole defect: this used to be refused because app A - a
            // DIFFERENT tenant - was holding the one transaction connection.
            let b = backend
                .fixture_session("app_b")
                .await
                .expect("app_b must get its own transaction connection while app_a holds one");
            backend
                .execute_fixture_on(&b, "BEGIN", &[])
                .await
                .expect("BEGIN b");
            backend
                .execute_fixture_on(&b, "INSERT INTO \"app_b\".\"t\" (v) VALUES ('b')", &[])
                .await
                .expect("app_b writes inside its transaction");

            // Both settle independently, and each one's write lands in its own
            // file: two connections, two transactions, no interleaving.
            assert_eq!(
                backend
                    .settle_transaction_for_tests(&b, TerminalIntent::Commit)
                    .await
                    .expect("commit b"),
                TerminalOutcome::Committed
            );
            assert_eq!(
                backend
                    .settle_transaction_for_tests(&a, TerminalIntent::Commit)
                    .await
                    .expect("commit a"),
                TerminalOutcome::Committed
            );
            let probe = backend.autocommit_client();
            for (app, want) in [("app_a", "a"), ("app_b", "b")] {
                let rows = probe
                    .query(&format!("SELECT v FROM \"{app}\".\"t\""), &[])
                    .await
                    .expect("read back");
                assert_eq!(rows.len(), 1, "{app} must hold exactly its own row");
                assert_eq!(rows[0][0].as_deref(), Some(want));
            }
        });
    })
}

/// A transaction connection carries ONE app's file, so a creator transaction
/// cannot address another tenant's tables at all.
///
/// The connection enforces this boundary even though builders also reject
/// qualified creator identifiers.
#[test]
fn a_transaction_lane_cannot_address_another_apps_tables() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_a")
                .await
                .expect("attach app_a");
            backend
                .attach_app_file("app_b")
                .await
                .expect("attach app_b");
            backend
                .execute_fixture(
                    "CREATE TABLE \"app_a\".\"secret\" (id INTEGER PRIMARY KEY, v TEXT)",
                    &[],
                )
                .await
                .expect("create app_a.secret");
            backend
                .execute_fixture(
                    "INSERT INTO \"app_a\".\"secret\" (v) VALUES ('tenant-a')",
                    &[],
                )
                .await
                .expect("seed app_a.secret");

            let b = backend
                .fixture_session("app_b")
                .await
                .expect("acquire app_b's transaction connection");
            backend
                .execute_fixture_on(&b, "BEGIN", &[])
                .await
                .expect("BEGIN b");
            let leaked = backend
                .execute_fixture_on(&b, "DELETE FROM \"app_a\".\"secret\"", &[])
                .await
                .expect_err("app_b's transaction must not reach app_a's tables");
            assert!(
                format!("{leaked}").contains("no such table"),
                "the refusal must be SQLite not knowing the alias, not an \
             application-level check; got {leaked:?}"
            );

            // The row is still there. A refusal that let the DELETE through and
            // reported an error afterwards would be worse than no check.
            let rows = backend
                .autocommit_client()
                .query("SELECT v FROM \"app_a\".\"secret\"", &[])
                .await
                .expect("read app_a.secret back");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0].as_deref(), Some("tenant-a"));
        });
    })
}

/// The refusal that survives, and the message that must name the app.
///
/// A second top-level transaction for the SAME app is still refused
/// immediately. What changed is that this is now the *only* producer of
/// `transaction_connection_busy` on this path, so the message can say whose
/// transaction it is - defect L22b's "reads as your transaction when it is
/// another tenant's" is gone with the cause.
#[test]
fn a_second_transaction_for_the_same_app_is_still_refused_and_names_it() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .attach_app_file("app_a")
                .await
                .expect("attach app_a");
            let first = backend
                .fixture_session("app_a")
                .await
                .expect("first acquire");
            backend
                .execute_fixture_on(&first, "BEGIN", &[])
                .await
                .expect("BEGIN");

            let err = backend
                .fixture_session("app_a")
                .await
                .expect_err("a second transaction for the same app must be refused");
            match &err {
                DbError::ValidationFailed {
                    code,
                    message,
                    hint,
                } => {
                    assert_eq!(*code, "transaction_connection_busy", "got {err:?}");
                    assert!(
                        message.contains("app_a"),
                        "the message must name the app whose transaction it is; got {message:?}"
                    );
                    assert!(hint.is_some(), "the remedy is the creator's, so say it");
                }
                other => panic!("expected a typed refusal, got {other:?}"),
            }

            // Dropping the first lease frees the app's lane immediately - no queue
            // round trip - so the next acquire succeeds.
            drop(first);
            backend
                .fixture_session("app_a")
                .await
                .expect("the app's lane is free once its lease drops");
        });
    })
}

/// An idle transaction connection is evicted to make room for a new app, and
/// only a session whose every lane is mid-transaction refuses - under a code of
/// its own.
///
/// Bounded per-tenant resources are the point: without the cap every app that
/// ever opened a transaction would hold a connection for the session's life.
/// The two halves differ in ONE variable - whether the incumbent lanes are
/// still inside a transaction - so the eviction path cannot be mistaken for the
/// refusal path.
#[test]
fn transaction_lanes_are_capped_and_the_refusal_has_its_own_code() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            let cap = zeroship_data_orm::backend::sqlite::session::MAX_TX_LANES_FOR_TESTS;

            // Half one: `cap` apps that each settle. Every lane is idle, so the
            // next app evicts one and is admitted.
            for i in 0..cap {
                let app = format!("cap_a{i}");
                backend.attach_app_file(&app).await.expect("attach");
                let client = backend
                    .fixture_session(&app)
                    .await
                    .expect("acquire under the cap");
                drop(client);
            }
            let app = format!("cap_a{cap}");
            backend.attach_app_file(&app).await.expect("attach");
            backend
                .fixture_session(&app)
                .await
                .expect("an idle lane must be evicted rather than refusing");

            // Half two: `cap` apps that all HOLD their transactions. Now there is
            // nothing to evict.
            let (backend, _dir) = fresh_backend(host);
            let mut held = Vec::new();
            for i in 0..cap {
                let app = format!("cap_b{i}");
                backend.attach_app_file(&app).await.expect("attach");
                let client = backend
                    .fixture_session(&app)
                    .await
                    .expect("acquire under the cap");
                backend
                    .execute_fixture_on(&client, "BEGIN", &[])
                    .await
                    .expect("BEGIN");
                held.push(client);
            }
            let app = format!("cap_b{cap}");
            backend.attach_app_file(&app).await.expect("attach");
            let err = backend
                .fixture_session(&app)
                .await
                .expect_err("every lane is mid-transaction, so this must be refused");
            match &err {
                DbError::ValidationFailed { code, .. } => assert_eq!(
                    *code, "transaction_lanes_exhausted",
                    "contention with OTHER apps must not arrive under the same code as this \
                 app's own overlapping transaction; got {err:?}"
                ),
                other => panic!("expected a typed refusal, got {other:?}"),
            }
            drop(held);
        });
    })
}

/// `SQLITE_BUSY_SNAPSHOT` on a write upgrade - the last of SC-2's three owed
/// arms.
///
/// SC-2 names it "the real serialization point" and the epoch bullet rests on
/// it. It needs a read snapshot held open ACROSS commands, which the autocommit
/// lane cannot express (one reservation per command, `BEGIN DEFERRED ...
/// COMMIT` around each). **The transaction lane can**: its `BEGIN` and its
/// statements are separate commands on one connection, so another connection
/// can commit in between.
///
/// What this fixture reaches and what it does NOT: the table lives in `main`,
/// the session's own database, which the boot PRAGMAs put in **WAL**. That is
/// what makes `SQLITE_BUSY_SNAPSHOT` (517) possible at all. An app's own file
/// is a different story - see
/// `an_app_files_write_upgrade_is_plain_busy_because_it_is_not_in_wal`, the
/// control that pins why.
#[test]
fn a_write_upgrade_on_a_stale_wal_snapshot_is_refused() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend
                .execute_fixture("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", &[])
                .await
                .expect("create table");
            backend
                .execute_fixture("INSERT INTO t (v) VALUES ('seed')", &[])
                .await
                .expect("seed");

            let tx = backend
                .fixture_session("snapshot_app")
                .await
                .expect("acquire tx client");
            backend
                .execute_fixture_on(&tx, "BEGIN", &[])
                .await
                .expect("BEGIN");
            // The read is what takes the deferred snapshot. Without it `BEGIN`
            // alone has taken no snapshot and the write below simply succeeds -
            // which is the arm's whole difficulty and why it stayed owed.
            let seen = tx
                .query("SELECT COUNT(*) FROM t", &[])
                .await
                .expect("snapshot read");
            assert_eq!(seen[0][0].as_deref(), Some("1"));

            // A different connection commits. `op_conn` is a separate SQLite
            // connection to the same WAL database, so this moves the WAL past the
            // snapshot the transaction is pinned to.
            backend
                .execute_fixture("INSERT INTO t (v) VALUES ('from-op-conn')", &[])
                .await
                .expect("op_conn write commits");

            let started = std::time::Instant::now();
            let err = backend
                .execute_fixture_on(&tx, "INSERT INTO t (v) VALUES ('upgrade')", &[])
                .await
                .expect_err("a write on a stale WAL snapshot must be refused");
            let elapsed = started.elapsed();
            match &err {
                DbError::LockContention { message } => assert!(
                    message.contains("database is locked") || message.contains("busy"),
                    "got {message:?}"
                ),
                other => panic!(
                    "SQLITE_BUSY_SNAPSHOT must map to LockContention, not an opaque \
                 fault; got {other:?}"
                ),
            }
            // The discriminator between 517 and a plain 5. `busy_timeout` is 5000 ms
            // (`BOOT_PRAGMAS`), and SQLite does NOT invoke the busy handler for
            // SQLITE_BUSY_SNAPSHOT because retrying can never succeed - so an
            // ordinary lock conflict would have sat here for five seconds and this
            // one returns at once. Without this the assertion above passes on
            // either code and the arm proves only that something was locked.
            assert!(
                elapsed < std::time::Duration::from_millis(1500),
                "a snapshot conflict must not go through the busy handler; waited {elapsed:?}, \
             which is the shape of a plain SQLITE_BUSY waiting out busy_timeout"
            );

            // And the transaction is still the caller's to end: the refusal is a
            // refusal, not a teardown.
            assert_eq!(
                backend
                    .settle_transaction_for_tests(&tx, TerminalIntent::Rollback)
                    .await
                    .expect("rollback"),
                TerminalOutcome::RolledBack
            );
        });
    })
}

/// The control that names what the arm above cannot reach: an app's own file is
/// **not** in WAL, so the same schedule cannot produce
/// `SQLITE_BUSY_SNAPSHOT` there.
///
/// `PRAGMA journal_mode` is per database and does NOT propagate across `ATTACH`
/// (measured: attaching a fresh file to a WAL connection leaves it `delete`),
/// and the migration engine pins every app file to DELETE outright and refuses
/// to run otherwise -
/// `crates/zeroship-migrate-sqlite/src/backend/actor.rs:719-729`. So the arm
/// above proves the mapping and the lane mechanics; it does not prove anything
/// about app data. This one records which mode app data is actually in, so a
/// change to that fact fails here rather than silently making the arm above
/// describe a world we do not run in.
#[test]
fn an_app_files_write_upgrade_is_plain_busy_because_it_is_not_in_wal() {
    Host::test(|host| {
        host.run(async {
            let (backend, _dir) = fresh_backend(host);
            backend.attach_app_file("jm_app").await.expect("attach");

            let mode = backend
                .autocommit_client()
                .query("PRAGMA \"jm_app\".journal_mode", &[])
                .await
                .expect("read the attached file's journal mode");
            assert_eq!(
                mode[0][0].as_deref(),
                Some("delete"),
                "an ATTACHed app file does not inherit main's WAL mode, and the migration \
             engine pins it to DELETE; SQLITE_BUSY_SNAPSHOT cannot arise on app data \
             while that is true"
            );

            let main_mode = backend
                .autocommit_client()
                .query("PRAGMA main.journal_mode", &[])
                .await
                .expect("read main's journal mode");
            assert_eq!(
                main_mode[0][0].as_deref(),
                Some("wal"),
                "the control: the session's own database IS in WAL, so the two databases \
             on one connection genuinely differ"
            );
        });
    })
}
