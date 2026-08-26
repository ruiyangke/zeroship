// `Pool::connect_with_pool_config` inside a `compio::time::timeout` makes rustc
// compute the layout of one deep async query chain, and the default depth is
// not enough. `tests/pool_transaction_isolation.rs` carries the same attribute
// for the same reason; without it this target fails to compile from a COLD
// cache, which an incremental build hides.

//! Session state DELIBERATELY survives a pool release; transaction state does not.
//!
//! `Pool::return_client` queues a `ROLLBACK`, not a `DISCARD ALL`, and
//! `src/pool.rs` carries a long comment explaining why: `DISCARD ALL` would
//! drop session-scoped advisory locks (`crates/plugin-db`'s `LockGuard` holds
//! one on a pooled client), every prepared statement (this driver's own
//! type-info cache holds those for the life of the `Client`, so the next use of
//! a cached entry would fail), and every session GUC -- and it cannot run
//! inside a transaction block at all, which is the very state the rollback
//! exists to clear.
//!
//! That reasoning was written down and nothing tested it. A decision that lives
//! only in a comment is one refactor away from being undone by someone who
//! reads "the pool should reset sessions" as obviously correct, and the damage
//! would surface far away -- a released advisory lock, or a cached statement
//! that no longer exists -- rather than here.
//!
//! `tests/pool_transaction_isolation.rs` pins the other half: that an open
//! transaction does NOT survive. This file pins the half that must.
//!
//! Every assertion checks `pg_backend_pid()` across the release. Without that,
//! a pool that discarded the connection and opened a fresh one would look
//! identical to a pool that reset the session, and the opposite conclusion
//! would be equally consistent with the evidence.
//!
//! MUTATION, 2026-08-23, and the first two attempts at it were worthless in a
//! way worth recording. Making the release send `DISCARD ALL` turns all three
//! tests below red, so they do discriminate. Getting there took three tries:
//!
//! 1. Changing the release SQL to `ROLLBACK; DISCARD ALL` changed nothing. The
//!    release only queues SQL when the session is NOT idle, and these tests
//!    release an idle one, so nothing was sent at all.
//! 2. Forcing the release to fire unconditionally STILL changed nothing --
//!    because a multi-statement simple query runs in an implicit transaction
//!    block, and `DISCARD ALL` is rejected there with `25001`. The send is
//!    fire-and-forget, so the server's refusal was swallowed and the run
//!    printed exactly what a clean run prints.
//! 3. Only a standalone `DISCARD ALL` reaches the session and clears it.
//!
//! Attempt 2 is the useful one: it is an independent confirmation of the claim
//! in `pool.rs` that `DISCARD ALL` "cannot run inside a transaction block at
//! all". That was written as a reason and is now also a measurement. It is
//! also a reminder that a mutation which never applied prints green, so a
//! green run after a mutation means "check the mutation" before it means
//! "the code is fine".

use compio_postgres::{Pool, PoolConfig};
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const POOL_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn test_url() -> String {
    common::test_url()
}

async fn single_connection_pool(url: &str) -> Pool {
    let mut config = PoolConfig::default();
    // Exactly one connection, kept warm, so the second borrower is guaranteed
    // to be handed the SAME backend the first one used.
    config.max_size(1).min_idle(1);
    match compio::time::timeout(
        POOL_CONNECT_TIMEOUT,
        Pool::connect_with_pool_config(url, config),
    )
    .await
    {
        Ok(Ok(pool)) => pool,
        Ok(Err(error)) => common::postgres_unreachable(url, &error),
        Err(_) => panic!("pool connection exceeded its timeout"),
    }
}

/// A session GUC set by one borrower is still set for the next.
#[compio::test]
async fn a_session_guc_survives_a_release() {
    let url = test_url();
    let pool = single_connection_pool(&url).await;

    let first_pid = {
        let client = pool.get().await.expect("first checkout");
        client
            .batch_execute("SET cpg_carry.marker = 'set-by-first-borrower'")
            .await
            .expect("set a custom GUC");
        client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("pid")
            .get::<_, i32>(0)
    };

    let client = pool.get().await.expect("second checkout");
    let second_pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("pid")
        .get(0);
    assert_eq!(
        first_pid, second_pid,
        "the pool handed out a different backend, so this test cannot say \
         anything about what a release preserves"
    );

    let marker: String = client
        .query_one("SELECT current_setting('cpg_carry.marker', true)", &[])
        .await
        .expect("read the GUC back")
        .get(0);
    assert_eq!(
        marker, "set-by-first-borrower",
        "a session GUC did not survive the release; if a DISCARD ALL was just \
         added to the release path, read the comment in pool.rs first -- it \
         also drops advisory locks and cached prepared statements"
    );
}

/// A session-scoped advisory lock is still held by the next borrower.
///
/// This is the case with teeth: `plugin-db`'s `LockGuard` holds one of these
/// across a pooled client, so a release that dropped it would hand out a lock
/// two callers each believe they own.
#[compio::test]
async fn a_session_advisory_lock_survives_a_release() {
    let url = test_url();
    let pool = single_connection_pool(&url).await;

    // Process-scoped so concurrent runs of this suite cannot collide on it.
    let key: i64 = i64::from(std::process::id()) + 0x5000_0000;

    let first_pid = {
        let client = pool.get().await.expect("first checkout");
        let acquired: bool = client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&key])
            .await
            .expect("take the advisory lock")
            .get(0);
        assert!(acquired, "the lock was already held; key collision");
        client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("pid")
            .get::<_, i32>(0)
    };

    let client = pool.get().await.expect("second checkout");
    let second_pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("pid")
        .get(0);
    assert_eq!(
        first_pid, second_pid,
        "different backend; test says nothing"
    );

    // Asked of pg_locks rather than by re-locking: pg_advisory_lock is
    // re-entrant within a session, so a second successful lock would prove
    // nothing about whether the first survived.
    let held: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_locks \
             WHERE locktype = 'advisory' AND pid = pg_backend_pid() \
               AND ((classid::bigint << 32) | objid::bigint) = $1",
            &[&key],
        )
        .await
        .expect("inspect pg_locks")
        .get(0);
    assert_eq!(
        held, 1,
        "the session advisory lock did not survive the release -- plugin-db's \
         LockGuard relies on this, and losing it hands the same lock to two \
         owners"
    );

    client
        .batch_execute(&format!("SELECT pg_advisory_unlock({key})"))
        .await
        .expect("release the advisory lock");
}

/// The contrast, in one variable: a TRANSACTION does not survive.
///
/// Without this, every assertion above would also be satisfied by a pool that
/// did nothing at all on release. The rollback is what separates "hands
/// session state across" from "hands everything across".
#[compio::test]
async fn an_open_transaction_does_not_survive_a_release() {
    let url = test_url();
    let pool = single_connection_pool(&url).await;

    let first_pid = {
        let client = pool.get().await.expect("first checkout");
        client
            .batch_execute("BEGIN; SET LOCAL cpg_carry.in_txn = 'yes'")
            .await
            .expect("open a transaction");
        client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("pid")
            .get::<_, i32>(0)
    };

    let client = pool.get().await.expect("second checkout");
    let second_pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("pid")
        .get(0);
    assert_eq!(
        first_pid, second_pid,
        "different backend; test says nothing"
    );

    // `SET LOCAL` is scoped to the transaction, so its absence is evidence the
    // transaction ended rather than merely that nobody read it.
    let in_txn: String = client
        .query_one("SELECT current_setting('cpg_carry.in_txn', true)", &[])
        .await
        .expect("read the transaction-local GUC")
        .get(0);
    assert_eq!(
        in_txn, "",
        "a transaction-local setting survived, so the release did not roll the \
         transaction back"
    );
}

/// A LISTEN registration survives a release, like the GUC and the advisory
/// lock above.
///
/// Recorded because the release path documents exactly what it does and does
/// not clear - "ROLLBACK, not DISCARD ALL" - and LISTEN was not on either
/// list. `pg_listening_channels()` is the server's own view, so this asks the
/// backend rather than inferring from behaviour.
///
/// This is a statement of what the pool DOES, not an endorsement. It matters
/// to a caller because the registration is invisible from the pooled side:
/// `notifications()` lives on `Connection`, which the pool owns, so a borrower
/// can leave a subscription behind that no later borrower can read or clear.
/// The server keeps sending on it, and the connection task logs and discards.
///
/// WHAT THIS CAN AND CANNOT CATCH. A clean release sends NOTHING - the
/// rollback is queued only for a session that is not provably `Idle` - so
/// there is no code on this path to perturb, and no mutation short of
/// changing the policy makes it fail. That is its job: it exists so that
/// adding a `DISCARD ALL` or an `UNLISTEN *` to the release path goes red and
/// forces the author to weigh the consequence the surrounding comments
/// describe - plugin-db holds a session advisory lock across a release.
#[compio::test]
async fn a_listen_registration_survives_a_release() {
    let url = test_url();
    let pool = single_connection_pool(&url).await;

    let first_pid = {
        let client = pool.get().await.expect("first checkout");
        client
            .batch_execute("LISTEN cpg_carry_channel")
            .await
            .expect("listen");
        client
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("pid")
            .get::<_, i32>(0)
    };

    let client = pool.get().await.expect("second checkout");
    let second_pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("pid")
        .get(0);
    assert_eq!(
        first_pid, second_pid,
        "the pool handed out a different backend, so this proves nothing about carry-over"
    );

    // Set-returning, so it belongs in FROM rather than in an expression.
    let still_listening: i64 = client
        .query_one(
            "SELECT count(*)::int8 FROM pg_listening_channels() AS c
              WHERE c = 'cpg_carry_channel'",
            &[],
        )
        .await
        .expect("listening channels")
        .get(0);
    assert_eq!(still_listening, 1, "the LISTEN did not survive the release");
}
