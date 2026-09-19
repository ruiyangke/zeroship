use crate::tests::fixtures::Host;
use compio_postgres::{Client, NoTls, Pool};
use zeroship_data_orm::transaction::probe;
use zeroship_data_orm::transaction::reducer::{
    CleanupCause, SessionOwnership, TerminalOutcome, TxState,
};

/// The harness binding this arm narrows to.
fn app_route(app_id: &str) -> zeroship_data_orm::binding::DbRoute {
    crate::tests::fixtures::harness_route(app_id)
}

/// The harness binding this arm narrows to.
fn app_binding(app_id: &str) -> zeroship_data_orm::binding::DbBinding {
    crate::tests::fixtures::harness_binding(app_id)
}

/// The quoted physical schema that binding addresses.
fn app_schema_ident(app_id: &str) -> String {
    crate::sql::mapping::quote_ident(app_binding(app_id).schema().as_str())
}

async fn probe_backend(host: &Host) -> zeroship_data_orm::backend::BackendHandle {
    host.backend()
        .await
        .expect("the adapter funnel must open a backend before BEGIN")
}

/// Connect an out-of-band admin session, and report the server reached.
///
/// **Prints `server_version_num`, not a container tag.** A cross-version
/// claim published off the variable rather than the server is a recorded
/// failure in this repository; the number below comes from the session that
/// ran the assertions.
async fn admin(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("sc1_driver requires PostgreSQL at {url}: {e}"));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    let version: String = client
        .query_one("SELECT current_setting('server_version_num')", &[])
        .await
        .expect("read server_version_num")
        .get(0);
    println!("sc1_driver oracle: server_version_num={version}");
    client
}

/// Provision the schema and per-app role the transaction session's
/// `SET LOCAL ROLE` needs, and install the pool the driver checks out from.
///
/// The pool is sized to **one** connection deliberately: with a single slot,
/// "did the session come back" is answerable by taking the next checkout and
/// comparing its backend PID, with no chance of being handed a different
/// idle entry.
async fn provision(
    host: &Host,
    app_id: &str,
) -> (crate::tests::fixtures::postgres::Postgres, Client) {
    let postgres = crate::tests::fixtures::postgres::Postgres::start();
    let url = postgres.url();
    let client = admin(&url).await;
    let binding = app_binding(app_id);
    let schema = app_schema_ident(app_id);
    client
        .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .await
        .unwrap_or_else(|e| panic!("reset {app_id}: {e}"));
    let pool_for_roles = Pool::connect(&url, 1)
        .await
        .expect("a pool for the ladder provisioning");
    crate::tests::fixtures::roles::ensure_binding_ladder(&pool_for_roles, &binding)
        .await
        .unwrap_or_else(|e| panic!("provision the ladder for {app_id}: {e}"));
    let capability = crate::sql::mapping::quote_ident(
        &crate::tests::fixtures::harness_capability_role(&binding),
    );
    client
        .batch_execute(&format!(
            "GRANT USAGE, CREATE ON SCHEMA {schema} TO {capability}"
        ))
        .await
        .unwrap_or_else(|e| panic!("provision {app_id}: {e}"));

    let pool = Pool::connect(&url, 1)
        .await
        .expect("a one-connection pool for the driver to check out from");
    host.install_postgres_pool(std::rc::Rc::new(pool), &url);
    (postgres, client)
}

/// Drop everything the arm created, and clear this thread's driver state.
async fn teardown(host: &Host, admin: &Client, app_id: &str) {
    probe::reset(&crate::tests::fixtures::harness_route(app_id));
    host.reset();
    let _ = admin
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            app_schema_ident(app_id)
        ))
        .await;
    // The roles outlive the schema, and that is the fixture's shape: each arm
    // owns a throwaway container, so nothing survives the test to collide.
}

/// Destroy this arm's transaction session even if the arm PANICS.
///
/// Not belt-and-braces. An assertion that fires mid-transaction skips
/// `teardown`, and the session then stays checked out with an open
/// transaction for the rest of the binary - holding locks in its schema and
/// a slot in a pool sized to one. Under mutation that is exactly what
/// happens, and it turned a mutation of the SAVEPOINT NAMING into a second,
/// unrelated failure in `transaction_rolls_back_on_async_reject`: a count of
/// "how many tests did this mutation redden" that includes collateral from
/// the harness measures the harness, not the code.
struct SessionGuard<'a>(&'a Host, &'static str);

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        probe::reset(&crate::tests::fixtures::harness_route(self.1));
        self.0.reset();
    }
}

async fn wait_for_advisory_block(admin: &Client, pid: i32, statement: &str) {
    compio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let row = admin
                .query_one(
                    "SELECT wait_event, query FROM pg_stat_activity WHERE pid = $1",
                    &[&pid],
                )
                .await
                .expect("observe the blocked transaction statement");
            let wait: Option<String> = row.get(0);
            let query: String = row.get(1);
            if wait.as_deref() == Some("advisory") && query.starts_with(statement) {
                break;
            }
        }
    })
    .await
    .expect("the transaction must reach its advisory-lock barrier");
}

async fn settlement_rows(admin: &Client, app_id: &str) -> i64 {
    admin
        .query_one(
            &format!(
                "SELECT count(*) FROM {}.settlement",
                app_schema_ident(app_id)
            ),
            &[],
        )
        .await
        .expect("inspect committed rows independently of the ORM")
        .get(0)
}

async fn provision_settlement_table(admin: &Client, app_id: &str) {
    let role = crate::sql::mapping::quote_ident(&crate::tests::fixtures::harness_capability_role(
        &app_binding(app_id),
    ));
    let schema = app_schema_ident(app_id);
    admin
        .batch_execute(&format!(
            "CREATE TABLE {schema}.settlement (id int PRIMARY KEY); \
             GRANT SELECT, INSERT ON {schema}.settlement TO {role}"
        ))
        .await
        .expect("provision the transaction's data table");
}

#[test]
fn root_rollback_waits_for_the_active_statement_before_returning() {
    Host::test(|host| {
        use zeroship_data_orm::transaction::{SettleOutcome, exec_settle};

        const APP: &str = "zs_sc1drv_waitrollback";
        host.run(async {
            let (_postgres, admin) = provision(host, APP).await;
            let _session_guard = SessionGuard(host, APP);
            provision_settlement_table(&admin, APP).await;
            admin
                .batch_execute("SELECT pg_advisory_lock(71001)")
                .await
                .unwrap();
            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("BEGIN");
            let pid = probe::session_backend_pid(&app_route(APP)).unwrap();
            let operation = compio::runtime::spawn(async {
                probe::operation(
                    &app_route(APP),
                    &format!(
                        "INSERT INTO {s}.settlement SELECT 1 \
                         FROM pg_advisory_xact_lock(71001)",
                        s = app_schema_ident(APP)
                    ),
                )
                .await
            });
            wait_for_advisory_block(&admin, pid, "INSERT").await;

            let settle_route = app_route(APP);
            let mut settlement = Box::pin(exec_settle(&settle_route, false, None));
            assert!(
                futures::poll!(&mut settlement).is_pending(),
                "rollback cannot return while the statement still owns its session"
            );
            assert_eq!(probe::state(&app_route(APP)), Some(TxState::Quiescing));
            assert_eq!(settlement_rows(&admin, APP).await, 0);

            admin
                .batch_execute("SELECT pg_advisory_unlock(71001)")
                .await
                .unwrap();
            operation
                .await
                .unwrap()
                .expect("the blocked insert finishes");
            assert!(matches!(settlement.await, SettleOutcome::Ok));
            assert_eq!(settlement_rows(&admin, APP).await, 0);
            assert_eq!(probe::state(&app_route(APP)), None);
            assert_eq!(
                host.pool_counts(),
                Some((1, 0, 1)),
                "settlement returns after releasing its session"
            );
            teardown(host, &admin, APP).await;
        });
    })
}

#[test]
fn root_commit_waits_for_terminal_sql_and_keeps_its_attempt_result() {
    Host::test(|host| {
        use zeroship_data_orm::transaction::{SettleOutcome, exec_settle};

        const APP: &str = "zs_sc1drv_waitcommit";
        host.run(async {
            let (_postgres, admin) = provision(host, APP).await;
            let _session_guard = SessionGuard(host, APP);
            provision_settlement_table(&admin, APP).await;
            admin
                .batch_execute(&format!(
                    "CREATE FUNCTION {s}.commit_barrier() RETURNS trigger \
                   LANGUAGE plpgsql AS $$ BEGIN \
                     PERFORM pg_advisory_xact_lock(71003); RETURN NEW; END $$; \
                 CREATE CONSTRAINT TRIGGER commit_barrier \
                   AFTER INSERT ON {s}.settlement \
                   DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
                   EXECUTE FUNCTION {s}.commit_barrier(); \
                 SELECT pg_advisory_lock(71002); \
                 SELECT pg_advisory_lock(71003)"
                    , s = app_schema_ident(APP)
                ))
                .await
                .expect("hold distinct barriers for the statement and COMMIT");
            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("BEGIN");
            let pid = probe::session_backend_pid(&app_route(APP)).unwrap();
            let operation = compio::runtime::spawn(async {
                probe::operation(
                    &app_route(APP),
                    &format!(
                        "INSERT INTO {s}.settlement SELECT 1 \
                         FROM pg_advisory_xact_lock(71002)",
                        s = app_schema_ident(APP)
                    ),
                )
                .await
            });
            wait_for_advisory_block(&admin, pid, "INSERT").await;

            let settle_route = app_route(APP);
            let mut settlement = Box::pin(exec_settle(&settle_route, true, None));
            assert!(
                futures::poll!(&mut settlement).is_pending(),
                "commit must wait for the outstanding statement"
            );
            assert_eq!(probe::state(&app_route(APP)), Some(TxState::Quiescing));
            admin
                .batch_execute("SELECT pg_advisory_unlock(71002)")
                .await
                .unwrap();
            wait_for_advisory_block(&admin, pid, "COMMIT").await;
            assert!(
                futures::poll!(&mut settlement).is_pending(),
                "statement completion alone does not prove COMMIT completed"
            );
            assert_eq!(settlement_rows(&admin, APP).await, 0);
            admin
                .batch_execute("SELECT pg_advisory_unlock(71003)")
                .await
                .unwrap();
            operation
                .await
                .unwrap()
                .expect("the insert and its terminal action finish");
            assert_eq!(probe::state(&app_route(APP)), None);
            assert_eq!(settlement_rows(&admin, APP).await, 1);

            // Reuse the lane before polling the old caller's completed wait.
            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("the released session admits a replacement transaction");
            assert_eq!(probe::session_backend_pid(&app_route(APP)), Some(pid));
            assert!(matches!(settlement.await, SettleOutcome::Ok));
            assert_eq!(probe::state(&app_route(APP)), Some(TxState::Idle));
            probe::operation(&app_route(APP), &format!("INSERT INTO {s}.settlement VALUES (2)", s = app_schema_ident(APP)))
                .await
                .unwrap();
            assert!(matches!(
                exec_settle(&app_route(APP), false, None).await,
                SettleOutcome::Ok
            ));
            assert_eq!(settlement_rows(&admin, APP).await, 1);
            teardown(host, &admin, APP).await;
        });
    })
}

#[test]
fn root_settlement_observes_deadline_cleanup_of_a_blocked_statement() {
    Host::test(|host| {
        use zeroship_data_orm::error::DbError;
        use zeroship_data_orm::transaction::reducer::deadline::DeadlineKind;
        use zeroship_data_orm::transaction::{SettleOutcome, exec_settle};

        const APP: &str = "zs_sc1drv_waitdeadline";
        host.run(async {
            let (_postgres, admin) = provision(host, APP).await;
            let _session_guard = SessionGuard(host, APP);
            provision_settlement_table(&admin, APP).await;
            admin
                .batch_execute("SELECT pg_advisory_lock(71004)")
                .await
                .unwrap();
            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("BEGIN");
            let pid = probe::session_backend_pid(&app_route(APP)).unwrap();
            let operation = compio::runtime::spawn(async {
                probe::operation(
                    &app_route(APP),
                    &format!(
                        "INSERT INTO {s}.settlement SELECT 1 \
                         FROM pg_advisory_xact_lock(71004)",
                        s = app_schema_ident(APP)
                    ),
                )
                .await
            });
            wait_for_advisory_block(&admin, pid, "INSERT").await;
            let settle_route = app_route(APP);
            let mut settlement = Box::pin(exec_settle(&settle_route, false, None));
            assert!(futures::poll!(&mut settlement).is_pending());
            let expired = probe::fire_execution_deadline(&app_route(APP)).await;
            assert_eq!(
                expired.outcome,
                Some(TerminalOutcome::Cancelled(CleanupCause::DeadlineExpired(
                    DeadlineKind::Execution
                )))
            );
            let error = operation
                .await
                .unwrap()
                .expect_err("PostgreSQL cancels the insert");
            assert!(
                error
                    .message_str()
                    .contains("canceling statement due to user request")
            );
            match settlement.await {
                SettleOutcome::SettleErr(DbError::Coded { code, .. }) => {
                    assert_eq!(code, "transaction_deadline_expired");
                }
                outcome => {
                    panic!("the waiter must receive the confirmed deadline outcome: {outcome:?}")
                }
            }
            assert_eq!(settlement_rows(&admin, APP).await, 0);
            assert_eq!(
                host.pool_counts(),
                Some((1, 0, 1)),
                "the healthy connection is reusable after confirmed rollback"
            );
            admin
                .batch_execute("SELECT pg_advisory_unlock(71004)")
                .await
                .unwrap();
            teardown(host, &admin, APP).await;
        });
    })
}

/// **The oracle-sampling trap, as an assertion.**
///
/// A forced cleanup of a POISONED transaction must not withdraw the
/// connection. Inside a poisoned block `transaction_status()` answers
/// `None`, because the failed statement's trailing `ReadyForQuery` has not
/// been consumed, and no retry changes that while the block stays poisoned.
/// SC-1 defines `None` as indeterminate and indeterminate withdraws, so a
/// driver that samples the oracle on entry to `Cancelling` destroys a
/// healthy connection on **every** forced cleanup.
///
/// `driver::cleanup_postgres` therefore issues the cleanup `ROLLBACK` first
/// and samples after it.
///
/// **Mutation that reddens this arm:** swap the two statements in
/// `cleanup_postgres` so the `transaction_status()` read precedes the
/// `batch_execute("ROLLBACK")`. The ack becomes `Indeterminate`, the outcome
/// becomes `Indeterminate(Cancelled)` instead of `Cancelled(Cancelled)`, the
/// session reads `Withdrawn`, and the pool loses its only connection.
#[test]
fn a_forced_cleanup_on_a_poisoned_block_keeps_a_healthy_connection() {
    Host::test(|host| {
        const APP: &str = "zs_sc1drv_poisoned";
        host.run(async {
            let (_postgres, admin) = provision(host, APP).await;
            let _session_guard = SessionGuard(host, APP);

            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("BEGIN");
            probe::operation(&app_route(APP), &format!("CREATE TABLE {s}.kept (id int)", s = app_schema_ident(APP)))
                .await
                .expect("a statement inside the transaction");

            // Poison the block with a real server-side error. A creator callback
            // can swallow exactly this and carry on, which is what makes a
            // forced cleanup of a poisoned transaction an ordinary case.
            let poisoned = probe::operation(&app_route(APP), "SELECT 1 / 0").await;
            assert!(poisoned.is_err(), "the block must actually be poisoned");
            assert_eq!(
                probe::state(&app_route(APP)),
                Some(TxState::Poisoned),
                "a statement that errored parks the transaction where PostgreSQL \
             has already put it"
            );
            let pid_before = probe::session_backend_pid(&app_route(APP)).expect("a pinned session");

            // Force it. Cleanup runs from Cancelling, which is the state whose
            // oracle read is the trap.
            let forced = probe::cancel(&app_route(APP)).await;

            assert_eq!(
                forced.outcome,
                Some(TerminalOutcome::Cancelled(CleanupCause::Cancelled)),
                "the cleanup ROLLBACK succeeds from a poisoned block and PROVES \
             CleanupGoal::OpenTransaction; sampling the oracle before it \
             reads None, which is indeterminate and withdraws"
            );
            assert_ne!(
                forced.session,
                Some(SessionOwnership::Withdrawn),
                "a poisoned block is a HEALTHY connection once it is rolled back - \
             withdrawing it destroys a connection on every forced cleanup"
            );
            assert!(
                !probe::withdrawn(&app_route(APP)),
                "no withdrawal tombstone may be set for a proved cleanup"
            );

            // The connection is back in the pool and reusable: the next checkout
            // is the SAME backend. With max_size = 1 there is nothing else it
            // could be handed.
            let (idle, active, total) = host.pool_counts().expect("a pool is installed");
            assert_eq!(
                (idle, active, total),
                (1, 0, 1),
                "a released session returns to the pool as idle"
            );
            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("a second BEGIN reuses it");
            assert_eq!(
                probe::session_backend_pid(&app_route(APP)),
                Some(pid_before),
                "the very same physical connection served the next transaction"
            );
            let settled = probe::settle(&app_route(APP), false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));

            teardown(host, &admin, APP).await;
        });
    })
}

/// **`WithdrawSession` genuinely withdraws - and this is the arm that says
/// what reaches it when forced cleanup CANCELS.**
///
/// The disposition SC-1 gives unknown backend health is to destroy the
/// physical connection rather than return it, and on PostgreSQL that is not
/// what a drop does: `PoolConnection::drop` calls
/// `pool.return_client(entry)`, which republishes the lease as idle.
///
/// **The route is what the name does not say, so the route is asserted.**
/// Forced cleanup reaches this withdrawal through the *best-effort* half of
/// PostgreSQL cancellation, and it is a case that matters:
///
/// 1. `HeldSession` takes the session out of the slot with **no statement
///    running on it**. The backend is idle inside its transaction block.
/// 2. Forced cleanup delivers a real `CancelRequest` to that backend, and
///    the postmaster confirms it consumed the packet.
/// 3. **PostgreSQL discards it.** A cancel that arrives while a backend is
///    reading its next command clears `QueryCancelPending` and raises
///    nothing. The session is not freed, because there was nothing to free.
/// 4. Nothing returns the session within `probe::cancel_reclaim_grace()`,
///    the goal is unproved, and the session is withdrawn.
///
/// That is exactly "what happens when cancellation silently does nothing",
/// and the answer is: the fallback, unchanged.
///
/// The elapsed-time assertion is what binds the arm to that route. A
/// cancellation the server acted on frees the session in about a round trip,
/// so a cleanup that returned quickly took the OTHER path and this arm would
/// be reporting on cancellation while claiming to report on withdrawal.
///
/// **Mutation that reddens this arm:** make
/// `context::destroy_tx_connection`'s Postgres arm a plain `drop(client)`
/// instead of `client.discard()`. The lease returns to the pool and retains
/// its capacity slot; the next checkout reports the same backend PID the
/// protocol withdrew.
#[test]
fn a_withdrawn_session_never_comes_back_from_the_pool() {
    Host::test(|host| {
        const APP: &str = "zs_sc1drv_withdraw";
        host.run(async {
            let (_postgres, admin) = provision(host, APP).await;
            let _session_guard = SessionGuard(host, APP);

            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("BEGIN");
            let (_, _, total_before) = host.pool_counts().expect("a pool is installed");
            assert_eq!(total_before, 1, "one connection, checked out");

            // Another future owns the session, and NOTHING IS RUNNING ON IT.
            // A cancel delivered to this backend is discarded by the server, so
            // cleanup cannot free the session and backend health stays unknown -
            // which is the case SC-1 answers with a withdrawal.
            let held = probe::HeldSession::take(&app_route(APP)).expect("hold the session");
            let withdrawn_pid = held.backend_pid().expect("a Postgres session");

            let started = std::time::Instant::now();
            let forced = probe::cancel(&app_route(APP)).await;
            let elapsed = started.elapsed();
            assert_eq!(
                forced.outcome,
                Some(TerminalOutcome::Indeterminate(CleanupCause::Cancelled)),
                "cleanup that could not free the session proves no goal"
            );
            assert!(
                elapsed >= probe::cancel_reclaim_grace(),
                "this arm must reach withdrawal through a cancellation the server \
             DISCARDED, which is the path that sits out the whole reclaim \
             grace. Returning in {elapsed:?} means the session came back and \
             the arm is now measuring cancellation, not withdrawal"
            );
            assert!(
                probe::withdrawn(&app_route(APP)),
                "an indeterminate cleanup withdraws the session"
            );

            // The holder gives it back, exactly as a TxClientSlotGuard's Drop
            // does. THIS is the moment a withdrawal has to survive.
            held.restore();

            let (idle, _, total_after) = host.pool_counts().expect("a pool is installed");
            assert_eq!(
                idle, 0,
                "a withdrawn session must not be published as idle - a plain \
             drop() would republish it and the next borrower would inherit \
             the connection the protocol withdrew"
            );
            assert_eq!(
                total_after, 0,
                "the capacity slot is released by eviction, not by redeposit"
            );

            // And the strongest form: whatever the pool opens next is a
            // DIFFERENT backend.
            probe::reset(&app_route(APP));
            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("a fresh BEGIN");
            let fresh_pid = probe::session_backend_pid(&app_route(APP)).expect("a pinned session");
            assert_ne!(
                fresh_pid, withdrawn_pid,
                "the withdrawn backend must be gone; the pool opened a new one"
            );

            // The withdrawn backend is really gone from the server, not merely
            // unreachable from the pool.
            let still_there: i64 = admin
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE pid = $1",
                    &[&withdrawn_pid],
                )
                .await
                .expect("count the withdrawn backend")
                .get(0);
            assert_eq!(
                still_there, 0,
                "closing the client's request channel terminates the backend; a \
             session that is still on the server is one a later user could \
             still be handed"
            );

            let settled = probe::settle(&app_route(APP), false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));
            teardown(host, &admin, APP).await;
        });
    })
}

/// Wait until `pid` is actually executing a statement on the server.
///
/// A `sleep` would be a guess about scheduling; this is the server's own
/// answer, so the arm that uses it cannot cancel a statement that never
/// started and then report that cancellation worked.
async fn wait_until_active(admin: &Client, pid: i32) {
    for _ in 0..200u32 {
        let state: Option<String> = admin
            .query_one("SELECT state FROM pg_stat_activity WHERE pid = $1", &[&pid])
            .await
            .expect("read the backend's state")
            .get(0);
        if state.as_deref() == Some("active") {
            return;
        }
        compio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("backend {pid} never reached state=active");
}

/// The state PostgreSQL reports for `pid`, or `None` once it is gone.
async fn backend_state(admin: &Client, pid: i32) -> Option<String> {
    admin
        .query_one("SELECT state FROM pg_stat_activity WHERE pid = $1", &[&pid])
        .await
        .expect("read the backend's state")
        .get(0)
}

/// **A forced cleanup CANCELS the running statement; it does not destroy the
/// connection.**
///
/// This is the common case, not an edge. The execution deadline exists to
/// bound a slow statement, so it fires *because* a statement is slow - and a
/// slow statement is one whose future is holding the session out of the slot.
/// Answering `Indeterminate` there would withdraw the session and kill the
/// backend, which is the opposite of bounding a slow statement.
///
/// A `CancelRequest` needs no session. It travels on its own connection and
/// names the backend by process id, so the canceller captured at install
/// time reaches a session no one can take out of the slot.
///
/// The arm asserts the whole chain on the server rather than on the
/// reducer's bookkeeping: the statement really was cancelled (`57014`), the
/// transaction really was rolled back (the goal `OpenTransaction` is proved,
/// which only "rolled back" proves), the connection really did come back
/// (the pool is idle at 1 and the next checkout is the SAME backend pid),
/// and it happened promptly rather than by sitting out the reclaim grace.
///
/// **Mutation that reddens this arm:** in `driver::cleanup`, answer the
/// empty-slot `Registry`/`Command` case with `CleanupAck::Indeterminate`
/// instead of `cancel_and_reclaim(..)`. The outcome becomes
/// `Indeterminate(Cancelled)`, the session is withdrawn, `total_count` drops
/// to 0 and the next checkout is a different backend.
#[test]
fn a_forced_cleanup_cancels_the_running_statement_and_keeps_the_connection() {
    Host::test(|host| {
        const APP: &str = "zs_sc1drv_cancelrun";
        host.run(async {
            let (_postgres, admin) = provision(host, APP).await;
            let _session_guard = SessionGuard(host, APP);

            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("BEGIN");
            let pid = probe::session_backend_pid(&app_route(APP)).expect("a pinned session");

            // A statement that will not end on its own inside this arm. 60s is
            // deliberately past `DB_STATEMENT_TIMEOUT_MS` (30s) so a failure to
            // cancel shows up as this arm hanging and then failing, never as a
            // server-side timeout that happens to look like a cancellation.
            let running = compio::runtime::spawn(async move {
                probe::operation(&app_route(APP), "SELECT pg_sleep(60)").await
            });
            wait_until_active(&admin, pid).await;
            assert_eq!(
                probe::session_backend_pid(&app_route(APP)),
                None,
                "the running statement holds the session OUT of the slot - that is \
             the condition that used to force a withdrawal"
            );

            let started = std::time::Instant::now();
            let forced = probe::cancel(&app_route(APP)).await;
            let elapsed = started.elapsed();

            let statement = running.await.expect("the cancelled statement's task");
            let failure = statement.expect_err("a cancelled statement must not report success");
            assert!(
                format!("{failure:?}").contains("canceling statement due to user request"),
                "the statement must end with PostgreSQL's own 57014, not with some \
             other failure that would pass this arm for the wrong reason: \
             {failure:?}"
            );

            assert_eq!(
                forced.outcome,
                Some(TerminalOutcome::Cancelled(CleanupCause::Cancelled)),
                "cancelling frees the session, the cleanup ROLLBACK then PROVES \
             CleanupGoal::OpenTransaction, and the transaction ends as \
             cancelled rather than indeterminate"
            );
            assert!(
                !probe::withdrawn(&app_route(APP)),
                "a cancelled statement's connection is healthy once rolled back; \
             withdrawing it is what this change exists to stop"
            );
            assert!(
                elapsed < probe::cancel_reclaim_grace(),
                "the session must be reclaimed because the cancel landed, not \
             because the grace expired; {elapsed:?} is the whole grace"
            );

            let (idle, active, total) = host.pool_counts().expect("a pool is installed");
            assert_eq!(
                (idle, active, total),
                (1, 0, 1),
                "the session went back to the pool as idle"
            );
            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("a second BEGIN reuses it");
            assert_eq!(
                probe::session_backend_pid(&app_route(APP)),
                Some(pid),
                "the very same physical connection served the next transaction"
            );
            let settled = probe::settle(&app_route(APP), false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));

            teardown(host, &admin, APP).await;
        });
    })
}

/// **A cleanup that outlived its transaction must not roll back the session
/// it finds in the slot.**
///
/// `cleanup` waits for a cancelled statement's holder, and that wait can
/// outlive the transaction: the `CancellationSql` deadline fires in its own
/// task, settles `Indeterminate`, withdraws, and releases the admission -
/// after which the next transaction is admitted, clears the withdrawal
/// tombstone, and installs ITS session in this very slot. A resumed cleanup
/// that took whatever it found there would issue `ROLLBACK` on a healthy,
/// unrelated transaction.
///
/// Two things stop it, and they are not the same thing.
/// `retire_transaction` wakes the slot waiters, so a cleanup whose
/// transaction was retired is released immediately rather than sitting out
/// its grace - that is the first line, and it depends on scheduling.
/// `CleanupIdentity` is the second, and does not: the cleanup re-reads the
/// reducer's `(cancellation token, backend generation)` after the wait and
/// refuses to act on a slot it can no longer prove is its own.
///
/// **The session in the slot belongs to a SUCCESSOR, which is what
/// production reaches.** A claim is never released while its own session is
/// parked - `settle_now` emits `WithdrawSession` or `ReleaseSession`
/// immediately before every `ReleaseAdmission`, and there is no second
/// emission site - so the only way a stale cleanup finds a filled slot is
/// that the next caller filled it. `probe::abandon_reducer` re-homes the
/// session onto a successor lane directly, which reaches the real state
/// without the race.
///
/// The arm rules on a filled slot plus a dead identity, and the observable is
/// **the transaction in that slot is still open on the server afterwards.**
///
/// **Mutation that reddens this arm:** delete the post-wait
/// `identity.is_current(..)` check in `driver::cancel_and_reclaim`. The
/// stale cleanup then takes the client and rolls it back, and
/// `pg_stat_activity` reports the backend `idle` instead of
/// `idle in transaction`.
#[test]
fn a_cleanup_that_outlived_its_transaction_leaves_the_slot_alone() {
    Host::test(|host| {
        const APP: &str = "zs_sc1drv_stalecleanup";
        host.run(async {
            let (_postgres, admin) = provision(host, APP).await;
            let _session_guard = SessionGuard(host, APP);

            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("BEGIN");
            let held = probe::HeldSession::take(&app_route(APP)).expect("hold the session");
            let pid = held.backend_pid().expect("a Postgres session");
            assert_eq!(
                backend_state(&admin, pid).await.as_deref(),
                Some("idle in transaction"),
                "precondition: the session is inside its transaction block"
            );

            // Force it. Nothing is running, so the cancel is discarded and the
            // cleanup parks on the slot - which is the state this arm needs.
            let cleanup = compio::runtime::spawn(async move { probe::cancel(&app_route(APP)).await });
            compio::time::sleep(std::time::Duration::from_millis(250)).await;

            // Two steps, with NO await between them, so the woken cleanup task
            // cannot run in the middle: the session comes back, and then the
            // transaction it belonged to is retired out from under the cleanup.
            held.restore();
            probe::abandon_reducer(&app_route(APP));

            let forced = cleanup.await.expect("the cleanup task");
            assert_eq!(
                forced.outcome, None,
                "the acknowledgement lands on a retired reducer and changes nothing"
            );

            assert_eq!(
                backend_state(&admin, pid).await.as_deref(),
                Some("idle in transaction"),
                "a cleanup that is no longer the current one must not send \
             ROLLBACK to whatever session it finds in the slot - the \
             transaction there is still open, and in production it would \
             belong to the NEXT caller"
            );
            assert_eq!(
                probe::session_backend_pid(&app_route(APP)),
                Some(pid),
                "and the session is still parked, not taken"
            );

            teardown(host, &admin, APP).await;
        });
    })
}

/// **The savepoint names dispatch emits are the reducer's.**
///
/// Two frames opened at the same depth get DIFFERENT names. Under the
/// depth-derived scheme this replaced they got the same one, and the arm
/// proves the difference is observable on the server: after the first frame
/// is released, rolling back to its name must fail with `3B001`. Under a
/// reused name that rollback would SUCCEED - silently unwinding to the
/// second frame's scope under the first frame's name.
///
/// **Mutation that reddens this arm:** change `FrameStack::open_child`'s
/// name to `format!("zs_sp_{}", self.frames.len())`. Both frames are then
/// `zs_sp_1`, `minted_names()` collapses to one entry, and the
/// `ROLLBACK TO SAVEPOINT zs_sp_1` at the end succeeds instead of raising
/// `3B001`.
#[test]
fn dispatch_emits_the_reducers_monotonic_savepoint_names() {
    Host::test(|host| {
        const APP: &str = "zs_sc1drv_names";
        host.run(async {
            let (_postgres, admin) = provision(host, APP).await;
            let _session_guard = SessionGuard(host, APP);

            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("BEGIN");
            probe::operation(&app_route(APP), &format!("CREATE TABLE {s}.rows_ (tag text)", s = app_schema_ident(APP)))
                .await
                .expect("create table");

            let first = probe::open_frame(&app_route(APP)).await.expect("first frame");
            probe::operation(&app_route(APP), &format!("INSERT INTO {s}.rows_ VALUES ('a')", s = app_schema_ident(APP)))
                .await
                .expect("write inside the first frame");
            let closed = probe::close_frame(&app_route(APP), first, true).await;
            assert_eq!(closed.refused, None, "RELEASE must succeed");

            let second = probe::open_frame(&app_route(APP)).await.expect("second frame");
            assert_ne!(first, second, "frame ids are never reused");

            let names = probe::minted_savepoint_names(&app_route(APP));
            assert_eq!(
                names.len(),
                2,
                "two frames at the SAME depth must mint two distinct names; a \
             depth-derived scheme mints one name twice and this set collapses \
             to a single entry. got {names:?}"
            );
            // The names are `zs_sp_<frame sequence>`, NOT `zs_sp_<depth>`, so the
            // first frame's number is whichever is lower - the root frame takes
            // sequence 1, which is why neither of these is `zs_sp_1`.
            let mut ordered = names.clone();
            ordered.sort_by_key(|name| {
                name.rsplit('_')
                    .next()
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(u64::MAX)
            });
            let first_name = ordered[0].clone();

            // The decisive server-side check: the first frame's savepoint is
            // GONE, because it was released under its own name and no later
            // frame reused it. A reused name would still be established here and
            // this rollback would SUCCEED, unwinding to the second frame's scope
            // under the first frame's name.
            let shadowed =
                probe::operation(&app_route(APP), &format!("ROLLBACK TO SAVEPOINT {first_name}")).await;
            let message = shadowed
                .as_ref()
                .err()
                .map(|error| format!("{error:?}"))
                .unwrap_or_default();
            assert!(
                shadowed.is_err(),
                "rolling back to a RELEASED savepoint must be refused; if it \
             succeeded, a later frame had re-established the same name and \
             this rollback reached the WRONG scope. names={names:?}"
            );
            assert!(
                message.contains(&format!("savepoint \\\"{first_name}\\\" does not exist"))
                    || message.contains(&format!("savepoint \"{first_name}\" does not exist")),
                "the refusal must be PostgreSQL's 3B001 naming THIS savepoint, not \
             some other failure that would pass this arm for the wrong reason: \
             {message}"
            );

            // That statement poisoned the block, so the settle is a rollback.
            let settled = probe::settle(&app_route(APP), false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));
            let _ = second;
            teardown(host, &admin, APP).await;
        });
    })
}

/// **A fired execution deadline in `Preparing`, where no `BEGIN` was ever
/// sent.**
///
/// The goal `NoTransaction` is fixed only from `Preparing`, and it is proved
/// by "the session reports no open transaction". There is no session at all
/// there - `Preparing` is admission plus the authority read, and the client
/// is acquired by `IssueBegin` - so the driver must prove the goal from the
/// protocol's own session ownership rather than from an empty slot, and must
/// send no SQL.
///
/// The transaction still settles, still releases its admission claim, and
/// still does NOT withdraw: nothing was opened, so there is nothing whose
/// health is unknown.
///
/// **Mutation that reddens this arm:** in `driver::cleanup`, answer the
/// empty-slot case with `CleanupAck::Indeterminate` unconditionally (delete
/// the `SessionOwnership::None` arm). The goal is then unproved, the outcome
/// becomes `Indeterminate` instead of `Cancelled`, and the tombstone is set
/// for a session that never existed - which would destroy the NEXT
/// transaction's connection, because `put_tx_client_for` consults it.
#[test]
fn a_deadline_that_fires_in_preparing_settles_without_a_begin() {
    Host::test(|host| {
        const APP: &str = "zs_sc1drv_preparing";
        host.run(async {
            let (_postgres, admin) = provision(host, APP).await;
            let _session_guard = SessionGuard(host, APP);

            // Admit, and stop there. `admit_in_preparing_for_tests` performs the
            // admission half of `begin_top_level` and returns before the
            // authority observation that would issue BEGIN.
            probe::admit_only(&app_route(APP));
            assert_eq!(
                probe::state(&app_route(APP)),
                Some(TxState::Preparing),
                "no BEGIN has been sent"
            );
            assert_eq!(
                probe::session(&app_route(APP)),
                Some(SessionOwnership::None),
                "Preparing holds no session: the client is acquired by IssueBegin"
            );
            let (idle_before, _, _) = host.pool_counts().expect("a pool is installed");

            let fired = probe::fire_execution_deadline(&app_route(APP)).await;

            assert_eq!(
                fired.outcome,
                Some(TerminalOutcome::Cancelled(CleanupCause::DeadlineExpired(
                    zeroship_data_orm::transaction::reducer::deadline::DeadlineKind::Execution
                ))),
                "the goal NoTransaction is proved by construction - BEGIN was \
             never sent, so nothing can be open"
            );
            assert!(
                !probe::withdrawn(&app_route(APP)),
                "there is no session to withdraw, and setting the tombstone here \
             would destroy the NEXT transaction's connection"
            );
            assert!(
                probe::state(&app_route(APP)).is_none(),
                "ReleaseAdmission retires the transaction on every path to Settled"
            );

            let (idle_after, active_after, _) = host.pool_counts().expect("a pool is installed");
            assert_eq!(
                (idle_after, active_after),
                (idle_before, 0),
                "a cleanup in Preparing touches no connection at all"
            );

            // The claim was released, so the next transaction is admitted
            // rather than parked forever.
            probe::begin(&app_binding(APP), None, probe_backend(host).await)
                .await
                .expect("the admission claim was released");
            let settled = probe::settle(&app_route(APP), false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));

            teardown(host, &admin, APP).await;
        });
    })
}
