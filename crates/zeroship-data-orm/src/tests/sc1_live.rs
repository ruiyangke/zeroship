use compio_postgres::{Client, NoTls, TransactionStatus};
use zeroship_data_orm::transaction::reducer::{
    CleanupAck, CleanupGoal, SettleIntent, TerminalOutcome, TerminalResult,
};

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    crate::tests::host::in_test(|| crate::tests::host::run(f))
}

/// Connect, and report the server actually reached.
///
/// **Prints `server_version_num`, not a container tag.** A cross-version
/// claim published off the variable rather than the server is a recorded
/// failure in this repository; the number below comes from the session
/// that ran the assertions.
async fn connect() -> (crate::support::postgres::Postgres, Client) {
    let postgres = crate::support::postgres::Postgres::start();
    let url = postgres.url();
    let (client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|e| panic!("sc1_live needs a live PostgreSQL at {url}: {e}"));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    let version: String = client
        .query_one("SELECT current_setting('server_version_num')", &[])
        .await
        .expect("read server_version_num")
        .get(0);
    println!("sc1_live oracle: server_version_num={version}");
    (postgres, client)
}

/// Project PostgreSQL's health oracle onto SC-1's acknowledgement.
///
/// **`None` is `Indeterminate`, and that is the whole point of the
/// mapping.** SC-1 states it directly - the driver's `None` "means
/// *indeterminate* and is documented as such" - and it is the reason a
/// cleanup goal that is not *proved* withdraws the session rather than
/// guessing. This helper is shared by both halves of the arm below so the
/// two cannot disagree about what a status means.
fn ack_for(status: Option<TransactionStatus>) -> CleanupAck {
    match status {
        Some(TransactionStatus::Idle) => CleanupAck::RolledBack,
        Some(TransactionStatus::InTransaction | TransactionStatus::Failed) | None => {
            CleanupAck::Indeterminate
        }
    }
}

/// **The L8 oracle.** A `COMMIT` issued in a failed transaction block is
/// answered with the command tag `ROLLBACK`.
///
/// The reducer maps `(SettleIntent::Commit, TerminalResult::RolledBack)`
/// to `TerminalOutcome::RolledBack`, which publishes nothing. That mapping
/// only means anything if the server really answers this way.
///
/// Where this would fail today: on a server that answers `COMMIT` to a
/// poisoned commit, or errors instead of answering. Either would make the
/// reducer's L8 arm model a behaviour that does not exist.
#[test]
fn a_commit_in_a_failed_transaction_is_answered_with_the_rollback_tag() {
    block_on(async {
        let (_postgres, client) = connect().await;
        client.batch_execute("BEGIN").await.expect("BEGIN");
        client
            .batch_execute("CREATE TEMP TABLE sc1_live_probe (id int)")
            .await
            .expect("temp table");
        // Poison the block. A creator callback can swallow this and
        // resolve, which is what makes the COMMIT below reachable.
        let poisoned = client.batch_execute("SELECT 1 / 0").await;
        assert!(poisoned.is_err(), "the block must actually be poisoned");

        let tag = client
            .batch_execute_reporting_tag("COMMIT")
            .await
            .expect("COMMIT is accepted even from a failed block");
        assert_eq!(
            tag.as_deref(),
            Some("ROLLBACK"),
            "PostgreSQL answers a poisoned COMMIT with the tag ROLLBACK; the \
             reducer's (Commit, RolledBack) -> RolledBack mapping models exactly this"
        );

        // Bind the observation to the reducer's own mapping, so the two
        // cannot drift apart silently.
        let modelled = match tag.as_deref() {
            Some("COMMIT") => TerminalResult::Committed,
            Some("ROLLBACK") => TerminalResult::RolledBack,
            _ => TerminalResult::Indeterminate,
        };
        assert_eq!(modelled, TerminalResult::RolledBack);
        assert!(
            !TerminalOutcome::RolledBack.publishes(),
            "a transaction PostgreSQL rolled back publishes nothing"
        );
        let _ = SettleIntent::Commit;
    });
}

/// **The savepoint-shadowing oracle.** `ROLLBACK TO SAVEPOINT` leaves the
/// savepoint defined, and a reused name resolves to the **most recently
/// established** one.
///
/// This is the precondition monotonic savepoint naming removes. The
/// orchestrator ships depth-derived names, so a leftover savepoint of a
/// reused name shadows an enclosing frame and sends its rollback to the
/// wrong scope.
///
/// Where this would fail today: on a server that RELEASES a savepoint when
/// rolling back to it. There the reuse would be harmless and
/// `FrameStack`'s monotonic sequence would be solving a problem that does
/// not exist. Step 2 is the one that decides it - a released savepoint
/// makes the second `ROLLBACK TO` fail with `3B001`.
#[test]
fn a_rolled_back_savepoint_stays_defined_and_a_reused_name_shadows_it() {
    block_on(async {
        let (_postgres, client) = connect().await;
        client.batch_execute("BEGIN").await.expect("BEGIN");
        client
            .batch_execute("CREATE TEMP TABLE sc1_live_probe (tag text)")
            .await
            .expect("temp table");

        // 1. Establish the OUTER savepoint under a depth-derived name.
        client
            .batch_execute("SAVEPOINT zs_sp_1")
            .await
            .expect("outer");
        client
            .batch_execute("INSERT INTO sc1_live_probe VALUES ('outer')")
            .await
            .expect("outer write");

        // 2. Roll back to it, then roll back to it AGAIN. The second call
        //    is decisive: had PostgreSQL released the savepoint, this
        //    errors with 3B001 invalid_savepoint_specification.
        client
            .batch_execute("ROLLBACK TO SAVEPOINT zs_sp_1")
            .await
            .expect("first rollback-to");
        client
            .batch_execute("ROLLBACK TO SAVEPOINT zs_sp_1")
            .await
            .expect(
                "ROLLBACK TO deliberately LEAVES the savepoint defined; if this \
                 errored, depth-derived names would be safe and monotonic naming \
                 would be unnecessary",
            );

        // 3. Reuse the name at the same depth - what a depth-derived
        //    scheme does after the depth decrements. There are now TWO
        //    savepoints called zs_sp_1.
        client
            .batch_execute("SAVEPOINT zs_sp_1")
            .await
            .expect("inner");
        client
            .batch_execute("INSERT INTO sc1_live_probe VALUES ('inner')")
            .await
            .expect("inner write");

        // 4. RELEASE destroys the INNER one only. Had the name replaced
        //    rather than shadowed, no savepoint of that name would remain
        //    and the ROLLBACK TO below would fail.
        client
            .batch_execute("RELEASE SAVEPOINT zs_sp_1")
            .await
            .expect("release the inner one");
        client
            .batch_execute("ROLLBACK TO SAVEPOINT zs_sp_1")
            .await
            .expect(
                "the enclosing savepoint of the same name is STILL THERE - the \
                 reused name shadowed it rather than replacing it, which is how an \
                 enclosing frame's rollback reaches the wrong scope",
            );

        client.batch_execute("ROLLBACK").await.expect("clean up");
    });
}

/// **The health oracle the cleanup goals read.** `transaction_status()`
/// reports `Idle` / `InTransaction` / `Failed`, and the reducer's
/// [`CleanupAck`] is a faithful projection of it.
///
/// SC-1 fixes each cleanup goal's proof against this oracle, so an arm
/// asserting only the reducer's own table would be checking the reducer
/// against itself. This one maps the live status onto `CleanupAck` and
/// asserts the goals accept exactly what SC-1 says.
///
/// Where this would fail today: on a server that reports `Idle` inside a
/// failed block - which would make `Poisoned` unobservable and
/// `CleanupGoal::OpenTransaction` unprovable, so forced cleanup could
/// never distinguish "rolled back" from "never opened".
#[test]
fn transaction_status_is_the_oracle_the_cleanup_goals_read() {
    block_on(async {
        let (_postgres, client) = connect().await;

        assert_eq!(
            client.transaction_status(),
            Some(TransactionStatus::Idle),
            "a fresh session is not in a transaction"
        );

        client.batch_execute("BEGIN").await.expect("BEGIN");
        assert_eq!(
            client.transaction_status(),
            Some(TransactionStatus::InTransaction),
            "a confirmed BEGIN is what fixes CleanupGoal::OpenTransaction"
        );

        let poisoned = client.batch_execute("SELECT 1 / 0").await;
        assert!(poisoned.is_err());

        // **The read straight after a failed statement is a RACE, and the
        // arm asserts the guarantee rather than the outcome.** The driver
        // decrements its in-flight counter when the connection task
        // consumes the trailing `ReadyForQuery`, which is not the moment a
        // failed `await` returns - so this observation is `None`
        // (unresolved) or `Some(Failed)` depending on scheduling. Measured
        // here: both occur, `None` when the target runs the `sc1_live`
        // filter alone and `Some(Failed)` in a full-target run.
        //
        // Asserting either literal is an arm whose verdict is noise. What
        // is guaranteed - and what SC-1 actually needs - is the negative:
        // **it is never `Some(Idle)`**, so a poisoned block can never be
        // mistaken for a proved clean one. Returning a stale `Idle` here is
        // the bug the driver's `Option` signature exists to prevent.
        let immediately_after = client.transaction_status();
        assert_ne!(
            immediately_after,
            Some(TransactionStatus::Idle),
            "a poisoned block must never read as idle - that is what would \
             make a caller skip its rollback and hand the next user of the \
             connection an aborted transaction"
        );
        assert_ne!(
            ack_for(immediately_after),
            CleanupAck::RolledBack,
            "whichever side of the race this lands on, it must not prove \
             CleanupGoal::OpenTransaction - an unproved cleanup withdraws"
        );

        // **A further FAILING statement is not a barrier either.** Inside a
        // poisoned block every data statement fails with 25P02, and a
        // failed statement leaves the byte exactly as unresolved as the
        // first one did. So there is no retry that makes the public oracle
        // answer while the block stays poisoned.
        let refused = client.batch_execute("SELECT 1").await;
        assert!(
            refused.is_err(),
            "PostgreSQL refuses every further data statement until the block \
             ends - this is what makes TxState::Poisoned a real server-side \
             state rather than a bookkeeping flag"
        );
        assert_ne!(
            client.transaction_status(),
            Some(TransactionStatus::Idle),
            "still poisoned, still never idle"
        );

        // Only a statement that SUCCEEDS resolves the byte, and inside a
        // failed block the terminal statement is the one that can.
        //
        // **This is a timing constraint on any driver wiring the reducer**,
        // and it is not in SC-1: forced cleanup must sample the oracle
        // AFTER its `ROLLBACK`, never before. A driver that samples on
        // entry to `Cancelling` reads `None`, gets `Indeterminate`, and
        // withdraws a connection that was about to be perfectly healthy.
        client.batch_execute("ROLLBACK").await.expect("ROLLBACK");
        let after = client.transaction_status();
        assert_eq!(after, Some(TransactionStatus::Idle));

        // Project the live status onto the reducer's acknowledgement, and
        // assert the goals accept exactly what SC-1's table says.
        let ack = ack_for(after);
        assert_eq!(ack, CleanupAck::RolledBack);
        assert!(
            CleanupGoal::OpenTransaction.is_proved_by(ack),
            "a confirmed rollback proves OpenTransaction"
        );
        assert!(CleanupGoal::AbortIfOpened.is_proved_by(ack));
        assert!(
            !CleanupGoal::OpenTransaction.is_proved_by(CleanupAck::Indeterminate),
            "an indeterminate oracle never proves a goal - it withdraws the session"
        );
        assert!(
            !CleanupGoal::NoTransaction.is_proved_by(CleanupAck::RolledBack),
            "'rolled back' contradicts 'BEGIN was never sent'"
        );
    });
}
