//! Live-Postgres coverage for what a status read does when a peer's deploy holds
//! the project lock.
//!
//! `pg_advisory_lock` waits without a timeout, so a reader that takes it inherits
//! the wall-clock cost of whatever deploy is running. MySQL bounds the same
//! acquisition at `GET_LOCK(name, 10)` and SQLite bounds it with a try-lock spin;
//! these tests pin the PostgreSQL leg to the same finite behaviour, with a
//! first-class busy outcome instead of an error, because a strict CI gate must not
//! read contention as a dirty migration set.
//!
//! The lock is held from a SECOND LIVE SESSION for the whole reader run. That is
//! the only arrangement that reproduces a peer's deploy: a single session takes
//! `pg_advisory_lock` re-entrantly and would never observe contention at all.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`, like every other live suite here.

use crate::support;

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::support::PgDevSession;

use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{ExecutorConfig, PostgresBackend, StatusSnapshot};

/// How long a reader gets to answer before the test calls it blocked. The bounded
/// retry is three attempts around 200ms apart, so a correct reader answers in well
/// under a second; this leaves room for a loaded machine without letting a genuine
/// unbounded wait pass as slow.
const READER_DEADLINE: Duration = Duration::from_secs(15);

/// A unique token so each test gets isolated meta + project schemas in the shared DB.
fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(
        format!("prj_{tok}"),
        format!("proj_{tok}"),
        support::no_inject(&format!("proj_{tok}")),
    );
    c.pg.meta_schema = format!("meta_{tok}");
    c
}

fn drop_schemas(session: &PgDevSession, cfg: &ExecutorConfig) {
    futures::executor::block_on(async {
        session
            .batch(&format!(
                "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
                cfg.project_schema, cfg.pg.meta_schema
            ))
            .await
            .expect("drop schemas");
    });
}

/// Run `body` against its OWN live session on a worker thread and fail the test if
/// it has not answered within [`READER_DEADLINE`].
///
/// The reader needs a separate connection because the project lock is held per
/// session; it needs a separate thread because a blocking acquisition has no
/// timeout to observe, so the only way to report it as a failure instead of a hung
/// test run is to stop waiting on it.
fn read_within_deadline<T: Send + 'static>(
    dsn: &str,
    what: &str,
    body: impl FnOnce(&PostgresBackend<'_, PgDevSession>) -> T + Send + 'static,
) -> T {
    let (tx, rx) = mpsc::channel();
    let dsn = dsn.to_string();
    thread::spawn(move || {
        let session = PgDevSession::connect(&dsn);
        let backend = PostgresBackend::new_generic(&session);
        let _ = tx.send(body(&backend));
    });
    rx.recv_timeout(READER_DEADLINE).unwrap_or_else(|_| {
        panic!(
            "{what} did not answer within {READER_DEADLINE:?} while a peer held the project lock"
        )
    })
}

/// A reader answers while a peer holds the project lock, and names the holder.
///
/// The holder session stays locked across the whole read, so a pass here cannot be
/// the reader having simply outlived a lock that was released underneath it.
#[test]
fn plan_status_reports_a_busy_project_lock_instead_of_waiting_for_a_peer() {
    let url = skip_if_no_pg!();
    let holder = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&holder, &cfg);

    let holder_backend = PostgresBackend::new_generic(&holder);
    futures::executor::block_on(holder_backend.acquire_project_lock(&cfg)).expect("peer locks");
    let holder_pid = backend_pid(&holder);

    let read_cfg = cfg.clone();
    let outcome = read_within_deadline(&url, "read-only plan status", move |backend| {
        futures::executor::block_on(
            zero_migrate::ops::status::status_plans_via_backend_read_only(backend, &read_cfg, &[]),
        )
    })
    .expect("a contended read is an outcome, not an error");

    // Still held: the reader answered around the peer, not after it. The probe
    // needs its OWN session because an advisory lock is re-entrant within the
    // session that holds it, so the holder would always succeed at retaking it.
    let witness = PgDevSession::connect(&url);
    assert!(
        !futures::executor::block_on(try_lock(&witness, &cfg)),
        "the peer must still hold the lock when the reader's answer is judged"
    );

    match outcome {
        StatusSnapshot::ProjectLockBusy(holders) => assert_eq!(
            holders.iter().map(|entry| entry.pid).collect::<Vec<_>>(),
            vec![holder_pid],
            "the busy outcome names the session actually holding the lock"
        ),
        StatusSnapshot::Ready(status) => {
            panic!("a contended read must not reconcile: {status:?}")
        }
    }

    futures::executor::block_on(holder_backend.release_project_lock(&cfg)).expect("peer unlocks");
    drop_schemas(&holder, &cfg);
}

/// The positive control: with no peer, the same read still takes the lock, still
/// runs the journal reads, and still returns its verdict -- and leaves the lock
/// free afterwards.
///
/// Without this arm the busy test above would also pass if the reader had simply
/// stopped doing any work.
#[test]
fn plan_status_still_locks_reads_and_reconciles_when_no_peer_holds_the_lock() {
    let url = skip_if_no_pg!();
    let observer = PgDevSession::connect(&url);
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&observer, &cfg);

    let read_cfg = cfg.clone();
    let outcome = read_within_deadline(&url, "uncontended plan status", move |backend| {
        futures::executor::block_on(
            zero_migrate::ops::status::status_plans_via_backend_read_only(backend, &read_cfg, &[]),
        )
    })
    .expect("an uncontended read succeeds");

    let status = match outcome {
        StatusSnapshot::Ready(status) => status,
        StatusSnapshot::ProjectLockBusy(holders) => {
            panic!("nothing holds this project's lock, yet the read reported {holders:?}")
        }
    };
    assert!(
        status.applied.is_empty(),
        "a fresh project has applied none"
    );
    assert!(status.pending.is_empty(), "no plan was supplied");
    assert!(status.plans.is_empty(), "no plan was supplied");

    // The reader released what it took: an observer can take the same lock now.
    assert!(
        futures::executor::block_on(try_lock(&observer, &cfg)),
        "an uncontended read must release the project lock it acquired"
    );
    futures::executor::block_on(PostgresBackend::new_generic(&observer).release_project_lock(&cfg))
        .expect("observer unlocks");

    drop_schemas(&observer, &cfg);
}

/// This session's backend pid, for matching against a reported lock holder.
fn backend_pid(session: &PgDevSession) -> i64 {
    futures::executor::block_on(async {
        session
            .query_one("SELECT pg_backend_pid()::int8 AS pid", &[])
            .await
            .expect("backend pid")
            .try_get("pid")
            .expect("decode pid")
    })
}

/// Whether `session` can take the project lock right now.
async fn try_lock(session: &PgDevSession, cfg: &ExecutorConfig) -> bool {
    session
        .query_one(
            "SELECT pg_try_advisory_lock(hashtext($1)::bigint) AS got",
            &[cfg.project_id.as_str().into()],
        )
        .await
        .expect("probe the project lock")
        .try_get("got")
        .expect("decode the probe")
}
