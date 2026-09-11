/// Arms binding the SC-1 **driver** to a real server.
///
/// The module above proves the server behaves the way the reducer models. This
/// one proves the driver acts on those behaviours correctly - it drives
/// `transaction::driver` through `transaction::probe` and asserts on the session
/// disposition, the pool, and the savepoint names that actually reached the
/// wire.
///
/// Each test owns the PostgreSQL server used by its sessions.
///
/// Every arm uses a unique `zs_sc1drv_*` app id and drops the role and schema it
/// created, so a shared server is left as it was found.
///
/// Run with:
///
/// ```text
/// cargo test -p zeroship-data-v8 --features test-helpers --test test_helpers \
///   -- --test-threads=1 native_transaction::sc1_driver
/// ```
mod sc1_driver {
    use compio_postgres::{Client, NoTls, Pool};
    use zeroship_data_orm::transaction::probe;
    use zeroship_data_orm::transaction::reducer::{
        CleanupCause, SessionOwnership, TerminalOutcome, TxState,
    };

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        crate::live_tests::host::in_test(|| crate::live_tests::host::run(f))
    }

    /// The backend `probe::begin` takes as a parameter.
    ///
    /// It resolved its own through `tx_scope::ensure_backend` until 2026-09-03,
    /// which made an ENGINE file call the ADAPTER - the one direction the crate
    /// split forbids, and a hard cargo error once `transaction/` became
    /// `zeroship-data-orm`. The lookup lives here now, in the caller that
    /// owns the thread context, and it is the same call the V8 dispatcher makes
    /// on this test's behalf in production.
    /// The physical schema a probe opens its session against.
    ///
    /// Derived from the app id here because these fixtures still mint one
    /// string for both identities - the same thing production does today. The
    /// point of the parameter is that the CALL now states which it means.
    fn app_schema(app_id: &str) -> zeroship_data_sql::SchemaName {
        zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema name")
    }

    async fn probe_backend() -> zeroship_data_orm::backend::BackendHandle {
        crate::live_tests::host::ensure_backend()
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
            .unwrap_or_else(|e| panic!("sc1_driver needs a live PostgreSQL at {url}: {e}"));
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
    async fn provision(app_id: &str) -> (crate::support::postgres::Postgres, Client) {
        let postgres = crate::support::postgres::Postgres::start();
        let url = postgres.url();
        let client = admin(&url).await;
        let role = zeroship_core::database_role::per_app_role_name(app_id)
            .expect("transaction fixture app id must produce a valid PostgreSQL role name");
        client
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE; \
                 CREATE SCHEMA \"{app_id}\"; \
                 DO $$ BEGIN \
                   IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN \
                     CREATE ROLE \"{role}\" NOLOGIN NOREPLICATION; \
                   END IF; \
                 END $$; \
                 GRANT USAGE, CREATE ON SCHEMA \"{app_id}\" TO \"{role}\""
            ))
            .await
            .unwrap_or_else(|e| panic!("provision {app_id}: {e}"));

        let pool = Pool::connect(&url, 1)
            .await
            .expect("a one-connection pool for the driver to check out from");
        crate::support::install_postgres_pool(std::rc::Rc::new(pool), &url);
        (postgres, client)
    }

    /// Drop everything the arm created, and clear this thread's driver state.
    async fn teardown(admin: &Client, app_id: &str) {
        probe::reset(app_id);
        crate::live_tests::host::reset_context_for_tests();
        let role = zeroship_core::database_role::per_app_role_name(app_id)
            .expect("transaction fixture app id must produce a valid PostgreSQL role name");
        let _ = admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS \"{app_id}\" CASCADE; \
                 DROP ROLE IF EXISTS \"{role}\""
            ))
            .await;
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
    struct SessionGuard(&'static str);

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            probe::reset(self.0);
            crate::live_tests::host::reset_context_for_tests();
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
                &format!("SELECT count(*) FROM \"{app_id}\".settlement"),
                &[],
            )
            .await
            .expect("inspect committed rows independently of the ORM")
            .get(0)
    }

    async fn provision_settlement_table(admin: &Client, app_id: &str) {
        let role = zeroship_core::database_role::per_app_role_name(app_id).unwrap();
        admin
            .batch_execute(&format!(
                "CREATE TABLE \"{app_id}\".settlement (id int PRIMARY KEY); \
                 GRANT SELECT, INSERT ON \"{app_id}\".settlement TO \"{role}\""
            ))
            .await
            .expect("provision the transaction's data table");
    }

    #[test]
    fn root_rollback_waits_for_the_active_statement_before_returning() {
        use zeroship_data_orm::transaction::{SettleOutcome, exec_settle};

        const APP: &str = "zs_sc1drv_waitrollback";
        block_on(async {
            let (_postgres, admin) = provision(APP).await;
            let _session_guard = SessionGuard(APP);
            provision_settlement_table(&admin, APP).await;
            admin
                .batch_execute("SELECT pg_advisory_lock(71001)")
                .await
                .unwrap();
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            let pid = probe::session_backend_pid(APP).unwrap();
            let operation = compio::runtime::spawn(async {
                probe::operation(
                    APP,
                    &format!(
                        "INSERT INTO \"{APP}\".settlement SELECT 1 \
                         FROM pg_advisory_xact_lock(71001)"
                    ),
                )
                .await
            });
            wait_for_advisory_block(&admin, pid, "INSERT").await;

            let mut settlement = Box::pin(exec_settle(APP, false, None));
            assert!(
                futures::poll!(&mut settlement).is_pending(),
                "rollback cannot return while the statement still owns its session"
            );
            assert_eq!(probe::state(APP), Some(TxState::Quiescing));
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
            assert_eq!(probe::state(APP), None);
            assert_eq!(
                crate::live_tests::host::pool_counts_for_tests(),
                Some((1, 0, 1)),
                "settlement returns after releasing its session"
            );
            teardown(&admin, APP).await;
        });
    }

    #[test]
    fn root_commit_waits_for_terminal_sql_and_keeps_its_attempt_result() {
        use zeroship_data_orm::transaction::{SettleOutcome, exec_settle};

        const APP: &str = "zs_sc1drv_waitcommit";
        block_on(async {
            let (_postgres, admin) = provision(APP).await;
            let _session_guard = SessionGuard(APP);
            provision_settlement_table(&admin, APP).await;
            admin
                .batch_execute(&format!(
                    "CREATE FUNCTION \"{APP}\".commit_barrier() RETURNS trigger \
                       LANGUAGE plpgsql AS $$ BEGIN \
                         PERFORM pg_advisory_xact_lock(71003); RETURN NEW; END $$; \
                     CREATE CONSTRAINT TRIGGER commit_barrier \
                       AFTER INSERT ON \"{APP}\".settlement \
                       DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
                       EXECUTE FUNCTION \"{APP}\".commit_barrier(); \
                     SELECT pg_advisory_lock(71002); \
                     SELECT pg_advisory_lock(71003)"
                ))
                .await
                .expect("hold distinct barriers for the statement and COMMIT");
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            let pid = probe::session_backend_pid(APP).unwrap();
            let operation = compio::runtime::spawn(async {
                probe::operation(
                    APP,
                    &format!(
                        "INSERT INTO \"{APP}\".settlement SELECT 1 \
                         FROM pg_advisory_xact_lock(71002)"
                    ),
                )
                .await
            });
            wait_for_advisory_block(&admin, pid, "INSERT").await;

            let mut settlement = Box::pin(exec_settle(APP, true, None));
            assert!(
                futures::poll!(&mut settlement).is_pending(),
                "commit must wait for the outstanding statement"
            );
            assert_eq!(probe::state(APP), Some(TxState::Quiescing));
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
            assert_eq!(probe::state(APP), None);
            assert_eq!(settlement_rows(&admin, APP).await, 1);

            // Reuse the lane before polling the old caller's completed wait.
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("the released session admits a replacement transaction");
            assert_eq!(probe::session_backend_pid(APP), Some(pid));
            assert!(matches!(settlement.await, SettleOutcome::Ok));
            assert_eq!(probe::state(APP), Some(TxState::Idle));
            probe::operation(APP, &format!("INSERT INTO \"{APP}\".settlement VALUES (2)"))
                .await
                .unwrap();
            assert!(matches!(
                exec_settle(APP, false, None).await,
                SettleOutcome::Ok
            ));
            assert_eq!(settlement_rows(&admin, APP).await, 1);
            teardown(&admin, APP).await;
        });
    }

    #[test]
    fn root_settlement_observes_deadline_cleanup_of_a_blocked_statement() {
        use zeroship_data_orm::error::DbError;
        use zeroship_data_orm::transaction::reducer::deadline::DeadlineKind;
        use zeroship_data_orm::transaction::{SettleOutcome, exec_settle};

        const APP: &str = "zs_sc1drv_waitdeadline";
        block_on(async {
            let (_postgres, admin) = provision(APP).await;
            let _session_guard = SessionGuard(APP);
            provision_settlement_table(&admin, APP).await;
            admin
                .batch_execute("SELECT pg_advisory_lock(71004)")
                .await
                .unwrap();
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            let pid = probe::session_backend_pid(APP).unwrap();
            let operation = compio::runtime::spawn(async {
                probe::operation(
                    APP,
                    &format!(
                        "INSERT INTO \"{APP}\".settlement SELECT 1 \
                         FROM pg_advisory_xact_lock(71004)"
                    ),
                )
                .await
            });
            wait_for_advisory_block(&admin, pid, "INSERT").await;
            let mut settlement = Box::pin(exec_settle(APP, false, None));
            assert!(futures::poll!(&mut settlement).is_pending());
            let expired = probe::fire_execution_deadline(APP).await;
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
                crate::live_tests::host::pool_counts_for_tests(),
                Some((1, 0, 1)),
                "the healthy connection is reusable after confirmed rollback"
            );
            admin
                .batch_execute("SELECT pg_advisory_unlock(71004)")
                .await
                .unwrap();
            teardown(&admin, APP).await;
        });
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
        const APP: &str = "zs_sc1drv_poisoned";
        block_on(async {
            let (_postgres, admin) = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            probe::operation(APP, &format!("CREATE TABLE \"{APP}\".kept (id int)"))
                .await
                .expect("a statement inside the transaction");

            // Poison the block with a real server-side error. A creator callback
            // can swallow exactly this and carry on, which is what makes a
            // forced cleanup of a poisoned transaction an ordinary case.
            let poisoned = probe::operation(APP, "SELECT 1 / 0").await;
            assert!(poisoned.is_err(), "the block must actually be poisoned");
            assert_eq!(
                probe::state(APP),
                Some(TxState::Poisoned),
                "a statement that errored parks the transaction where PostgreSQL \
                 has already put it"
            );
            let pid_before = probe::session_backend_pid(APP).expect("a pinned session");

            // Force it. Cleanup runs from Cancelling, which is the state whose
            // oracle read is the trap.
            let forced = probe::cancel(APP).await;

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
                !probe::withdrawn(APP),
                "no withdrawal tombstone may be set for a proved cleanup"
            );

            // The connection is back in the pool and reusable: the next checkout
            // is the SAME backend. With max_size = 1 there is nothing else it
            // could be handed.
            let (idle, active, total) =
                crate::live_tests::host::pool_counts_for_tests().expect("a pool is installed");
            assert_eq!(
                (idle, active, total),
                (1, 0, 1),
                "a released session returns to the pool as idle"
            );
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("a second BEGIN reuses it");
            assert_eq!(
                probe::session_backend_pid(APP),
                Some(pid_before),
                "the very same physical connection served the next transaction"
            );
            let settled = probe::settle(APP, false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));

            teardown(&admin, APP).await;
        });
    }

    /// **`WithdrawSession` genuinely withdraws - and this is the arm that says
    /// what still reaches it now that forced cleanup CANCELS.**
    ///
    /// The disposition SC-1 gives unknown backend health is to destroy the
    /// physical connection rather than return it, and on PostgreSQL that is not
    /// what a drop does: `PoolConnection::drop` calls
    /// `pool.return_client(entry)`, which republishes the lease as idle.
    ///
    /// **The route changed and the name did not, so the route is asserted.**
    /// This arm used to reach withdrawal through "the driver cannot reach the
    /// session, so it cannot roll back" - which is precisely the answer
    /// `cancel_and_reclaim` replaced. What it reaches now is the *best-effort*
    /// half of PostgreSQL cancellation, and it is a case that matters more:
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
        const APP: &str = "zs_sc1drv_withdraw";
        block_on(async {
            let (_postgres, admin) = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            let (_, _, total_before) =
                crate::live_tests::host::pool_counts_for_tests().expect("a pool is installed");
            assert_eq!(total_before, 1, "one connection, checked out");

            // Another future owns the session, and NOTHING IS RUNNING ON IT.
            // A cancel delivered to this backend is discarded by the server, so
            // cleanup cannot free the session and backend health stays unknown -
            // which is the case SC-1 answers with a withdrawal.
            let held = probe::HeldSession::take(APP).expect("hold the session");
            let withdrawn_pid = held.backend_pid().expect("a Postgres session");

            let started = std::time::Instant::now();
            let forced = probe::cancel(APP).await;
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
                probe::withdrawn(APP),
                "an indeterminate cleanup withdraws the session"
            );

            // The holder gives it back, exactly as a TxClientSlotGuard's Drop
            // does. THIS is the moment a withdrawal has to survive.
            held.restore();

            let (idle, _, total_after) =
                crate::live_tests::host::pool_counts_for_tests().expect("a pool is installed");
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
            probe::reset(APP);
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("a fresh BEGIN");
            let fresh_pid = probe::session_backend_pid(APP).expect("a pinned session");
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

            let settled = probe::settle(APP, false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));
            teardown(&admin, APP).await;
        });
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

    /// **Forced cleanup CANCELS the running statement; it does not destroy the
    /// connection.**
    ///
    /// This is the common case, not an edge. The execution deadline exists to
    /// bound a slow statement, so it fires *because* a statement is slow - and a
    /// slow statement is one whose future is holding the session out of the
    /// slot. Cleanup could not reach the session, could not roll back, answered
    /// `Indeterminate`, and `Indeterminate` withdraws: the mechanism for
    /// bounding a slow statement responded by killing the backend, every time.
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
    /// instead of `cancel_and_reclaim(..)` - that is the pre-change behaviour
    /// verbatim. The outcome becomes `Indeterminate(Cancelled)`, the session is
    /// withdrawn, `total_count` drops to 0 and the next checkout is a different
    /// backend.
    #[test]
    fn a_forced_cleanup_cancels_the_running_statement_and_keeps_the_connection() {
        const APP: &str = "zs_sc1drv_cancelrun";
        block_on(async {
            let (_postgres, admin) = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            let pid = probe::session_backend_pid(APP).expect("a pinned session");

            // A statement that will not end on its own inside this arm. 60s is
            // deliberately past `DB_STATEMENT_TIMEOUT_MS` (30s) so a failure to
            // cancel shows up as this arm hanging and then failing, never as a
            // server-side timeout that happens to look like a cancellation.
            let running = compio::runtime::spawn(async move {
                probe::operation(APP, "SELECT pg_sleep(60)").await
            });
            wait_until_active(&admin, pid).await;
            assert_eq!(
                probe::session_backend_pid(APP),
                None,
                "the running statement holds the session OUT of the slot - that is \
                 the condition that used to force a withdrawal"
            );

            let started = std::time::Instant::now();
            let forced = probe::cancel(APP).await;
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
                !probe::withdrawn(APP),
                "a cancelled statement's connection is healthy once rolled back; \
                 withdrawing it is what this change exists to stop"
            );
            assert!(
                elapsed < probe::cancel_reclaim_grace(),
                "the session must be reclaimed because the cancel landed, not \
                 because the grace expired; {elapsed:?} is the whole grace"
            );

            let (idle, active, total) =
                crate::live_tests::host::pool_counts_for_tests().expect("a pool is installed");
            assert_eq!(
                (idle, active, total),
                (1, 0, 1),
                "the session went back to the pool as idle"
            );
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("a second BEGIN reuses it");
            assert_eq!(
                probe::session_backend_pid(APP),
                Some(pid),
                "the very same physical connection served the next transaction"
            );
            let settled = probe::settle(APP, false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));

            teardown(&admin, APP).await;
        });
    }

    /// **A cleanup that outlived its transaction must not roll back the session
    /// it finds in the slot.**
    ///
    /// Cancellation put a second actor in a window that used to have one.
    /// `cleanup` was a straight line with no await between reading the slot and
    /// writing it back, so "the session in the slot" could only be the session
    /// being cleaned up. It now waits for a cancelled statement's holder, and
    /// that wait can outlive the transaction: the `CancellationSql` deadline
    /// fires in its own task, settles `Indeterminate`, withdraws, and releases
    /// the admission - after which the next transaction is admitted, clears the
    /// withdrawal tombstone, and installs ITS session in this very slot. A
    /// resumed cleanup that took whatever it found there would issue `ROLLBACK`
    /// on a healthy, unrelated transaction.
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
    /// that the next caller filled it.
    ///
    /// This used to restore the SAME session and say so, on the grounds that a
    /// genuine successor could not be reached deterministically: admitting one
    /// by the ordinary route requires the claim, releasing the claim is what
    /// wakes this waiter, and the waiter resolves before the successor's `BEGIN`
    /// has run. `probe::abandon_reducer` now re-homes the session onto a
    /// successor lane directly, which reaches the real state without the race.
    /// What the arm rules on is unchanged - a filled slot plus a dead identity -
    /// and so is the observable: **the transaction in that slot is still open on
    /// the server afterwards.**
    ///
    /// **Mutation that reddens this arm:** delete the post-wait
    /// `identity.is_current(..)` check in `driver::cancel_and_reclaim`. The
    /// stale cleanup then takes the client and rolls it back, and
    /// `pg_stat_activity` reports the backend `idle` instead of
    /// `idle in transaction`.
    #[test]
    fn a_cleanup_that_outlived_its_transaction_leaves_the_slot_alone() {
        const APP: &str = "zs_sc1drv_stalecleanup";
        block_on(async {
            let (_postgres, admin) = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            let held = probe::HeldSession::take(APP).expect("hold the session");
            let pid = held.backend_pid().expect("a Postgres session");
            assert_eq!(
                backend_state(&admin, pid).await.as_deref(),
                Some("idle in transaction"),
                "precondition: the session is inside its transaction block"
            );

            // Force it. Nothing is running, so the cancel is discarded and the
            // cleanup parks on the slot - which is the state this arm needs.
            let cleanup = compio::runtime::spawn(async move { probe::cancel(APP).await });
            compio::time::sleep(std::time::Duration::from_millis(250)).await;

            // Two steps, with NO await between them, so the woken cleanup task
            // cannot run in the middle: the session comes back, and then the
            // transaction it belonged to is retired out from under the cleanup.
            held.restore();
            probe::abandon_reducer(APP);

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
                probe::session_backend_pid(APP),
                Some(pid),
                "and the session is still parked, not taken"
            );

            teardown(&admin, APP).await;
        });
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
        const APP: &str = "zs_sc1drv_names";
        block_on(async {
            let (_postgres, admin) = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("BEGIN");
            probe::operation(APP, &format!("CREATE TABLE \"{APP}\".rows_ (tag text)"))
                .await
                .expect("create table");

            let first = probe::open_frame(APP).await.expect("first frame");
            probe::operation(APP, &format!("INSERT INTO \"{APP}\".rows_ VALUES ('a')"))
                .await
                .expect("write inside the first frame");
            let closed = probe::close_frame(APP, first, true).await;
            assert_eq!(closed.refused, None, "RELEASE must succeed");

            let second = probe::open_frame(APP).await.expect("second frame");
            assert_ne!(first, second, "frame ids are never reused");

            let names = probe::minted_savepoint_names(APP);
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
                probe::operation(APP, &format!("ROLLBACK TO SAVEPOINT {first_name}")).await;
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
            let settled = probe::settle(APP, false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));
            let _ = second;
            teardown(&admin, APP).await;
        });
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
        const APP: &str = "zs_sc1drv_preparing";
        block_on(async {
            let (_postgres, admin) = provision(APP).await;
            let _session_guard = SessionGuard(APP);

            // Admit, and stop there. `admit_in_preparing_for_tests` performs the
            // admission half of `begin_top_level` and returns before the
            // authority observation that would issue BEGIN.
            probe::admit_only(APP);
            assert_eq!(
                probe::state(APP),
                Some(TxState::Preparing),
                "no BEGIN has been sent"
            );
            assert_eq!(
                probe::session(APP),
                Some(SessionOwnership::None),
                "Preparing holds no session: the client is acquired by IssueBegin"
            );
            let (idle_before, _, _) =
                crate::live_tests::host::pool_counts_for_tests().expect("a pool is installed");

            let fired = probe::fire_execution_deadline(APP).await;

            assert_eq!(
                fired.outcome,
                Some(TerminalOutcome::Cancelled(CleanupCause::DeadlineExpired(
                    zeroship_data_orm::transaction::reducer::deadline::DeadlineKind::Execution
                ))),
                "the goal NoTransaction is proved by construction - BEGIN was \
                 never sent, so nothing can be open"
            );
            assert!(
                !probe::withdrawn(APP),
                "there is no session to withdraw, and setting the tombstone here \
                 would destroy the NEXT transaction's connection"
            );
            assert!(
                probe::state(APP).is_none(),
                "ReleaseAdmission retires the transaction on every path to Settled"
            );

            let (idle_after, active_after, _) =
                crate::live_tests::host::pool_counts_for_tests().expect("a pool is installed");
            assert_eq!(
                (idle_after, active_after),
                (idle_before, 0),
                "a cleanup in Preparing touches no connection at all"
            );

            // The claim was released, so the next transaction is admitted
            // rather than parked forever.
            probe::begin(APP, app_schema(APP), None, probe_backend().await)
                .await
                .expect("the admission claim was released");
            let settled = probe::settle(APP, false).await;
            assert_eq!(settled.outcome, Some(TerminalOutcome::RolledBack));

            teardown(&admin, APP).await;
        });
    }
}
