//! Live-Postgres coverage for a project-lock acquisition that fails AFTER the
//! server already granted the lock.
//!
//! PostgreSQL can grant a session advisory lock and still fail the acquiring
//! statement. `LockErrorCleanup` re-grants a waiter that was kicked off the lock
//! queue ("If they did grant us the lock, we'd better remember it in our local
//! table"), and `pg_advisory_lock` acquires with `sessionLock = true`, so the
//! grant survives the transaction abort that follows. The caller is told the
//! acquisition failed and is left holding nothing to release with, while the
//! session carries a lock that only session exit will drop.
//!
//! The first test provokes that on a REAL server: a peer releases the lock inside
//! the millisecond band where the acquirer's `statement_timeout` fires, and often
//! enough the release grants the lock to a waiter that is already on its way out.
//! It carries its own control arm, because a race is worth nothing as coverage
//! unless the run can show it actually happened: the control arm sends the same
//! bare `pg_advisory_lock` the engine sends, with no compensation around it, and
//! the test fails if that arm never leaks a lock. Only once the harness has proven
//! it can provoke the window on this server does the engine arm's clean result
//! mean anything, and the engine arm then gets a dose of the same race calibrated
//! from how hard the control arm had to work.
//!
//! The remaining tests cover the shapes the server-side race cannot reach: a reply
//! lost on the way back from an acquisition the server ran, and a reply that
//! arrives but will not decode. `pg_try_advisory_lock` never waits, so it has no
//! grant-then-cancel window; its exposure is entirely a granted lock whose result
//! the caller never learned. Those are injected at the driver seam, and everything
//! around the injection is still live: a real grant in a real session that stays
//! open for the whole assertion, judged by a `pg_locks` read from a second session.
//!
//! GATED behind `ZERO_MIGRATE_TEST_PG_URL`, like every other live suite here.

use crate::support;

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::support::PgDevSession;

use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{ExecutorConfig, PostgresBackend, ProjectLockAcquisition};

/// The engine's own acquisition statement, sent verbatim by the control arm so it
/// races the identical server-side path with no compensation wrapped around it.
const ACQUIRE_SQL: &str = "SELECT pg_advisory_lock(hashtext($1)::bigint)";

/// How long the acquirer gives the lock wait before `statement_timeout` cancels
/// it. Small, because the whole harness is a peer release aimed at that instant.
const WAIT_BUDGET: Duration = Duration::from_millis(3);

/// The earliest the peer releases: half a millisecond before the acquirer's wait
/// budget expires.
const RELEASE_FLOOR: Duration = Duration::from_micros(2_500);

/// How far past [`RELEASE_FLOOR`] the release is jittered, so successive attempts
/// walk it across the cancellation instant instead of landing on one side of it.
const RELEASE_BAND_NANOS: u64 = 1_000_000;

/// How long the control arm may keep trying to provoke the grant-then-cancel
/// window before the test gives up and reports that it could not.
const CONTROL_BUDGET: Duration = Duration::from_secs(30);

/// A unique token so each test gets its own advisory-lock key in the shared DB.
fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("{pid}_{}_{n}", nanos())
}

/// The wall clock in nanoseconds, used both for unique tokens and for the
/// sub-millisecond jitter that walks the peer's release across the timeout
/// instant.
fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
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

/// How many sessions in the cluster hold this project's advisory lock right now.
///
/// Read from `pg_locks` through a session that is NOT the acquiring one, so the
/// answer is the server's own record of the grant rather than the engine's
/// bookkeeping about it. A single-argument `pg_advisory_lock(int8)` is recorded
/// with `objsubid = 1` and the key split across `classid`/`objid`, so reassembling
/// them reconstructs the exact `hashtext(project_id)` the engine locked on.
fn sessions_holding(witness: &PgDevSession, cfg: &ExecutorConfig) -> i64 {
    futures::executor::block_on(async {
        witness
            .query_one(
                "SELECT count(*)::int8 AS held \
                   FROM pg_locks \
                  WHERE locktype = 'advisory' AND granted AND objsubid = 1 \
                    AND ((classid::bigint << 32) | objid::bigint) = hashtext($1)::bigint",
                &[cfg.project_id.as_str().into()],
            )
            .await
            .expect("read pg_locks")
            .try_get("held")
            .expect("decode the advisory-lock holder count")
    })
}

/// Drop every advisory lock this session holds, so the next attempt starts from a
/// session that genuinely has to wait. A session that still holds the key would
/// re-take it instantly and never enter the window the race needs.
fn unlock_all(session: &PgDevSession) {
    futures::executor::block_on(session.batch("SELECT pg_advisory_unlock_all()"))
        .expect("drop this session's advisory locks");
}

/// What a peer session is asked to do for one attempt.
enum PeerCmd {
    /// Take the project lock, so the acquirer has to wait for it.
    Hold,
    /// Spin until `deadline`, then release, aiming the grant at the instant the
    /// acquirer's `statement_timeout` fires.
    ReleaseAt(Instant),
    Stop,
}

/// A peer's session, driven from its own thread.
///
/// The acquirer sits inside a BLOCKING lock wait for the whole window, so the
/// release cannot come from the acquiring thread, and an advisory lock is held per
/// session, so it cannot come from the acquiring session either.
struct Peer {
    cmd: mpsc::Sender<PeerCmd>,
    ack: mpsc::Receiver<()>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Peer {
    fn start(dsn: &str, project_id: &str) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<PeerCmd>();
        let (ack_tx, ack_rx) = mpsc::channel::<()>();
        let dsn = dsn.to_string();
        let project_id = project_id.to_string();
        let handle = thread::spawn(move || {
            let session = PgDevSession::connect(&dsn);
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    PeerCmd::Hold => {
                        futures::executor::block_on(
                            session.exec(ACQUIRE_SQL, &[project_id.as_str().into()]),
                        )
                        .expect("the peer takes the project lock");
                    }
                    PeerCmd::ReleaseAt(deadline) => {
                        // Busy wait: the band that matters is under a millisecond
                        // wide, which no sleep can land inside.
                        while Instant::now() < deadline {
                            std::hint::spin_loop();
                        }
                        futures::executor::block_on(
                            session.batch("SELECT pg_advisory_unlock_all()"),
                        )
                        .expect("the peer releases the project lock");
                    }
                    PeerCmd::Stop => break,
                }
                let _ = ack_tx.send(());
            }
        });
        Self {
            cmd: cmd_tx,
            ack: ack_rx,
            handle: Some(handle),
        }
    }

    fn send(&self, cmd: PeerCmd) {
        self.cmd.send(cmd).expect("the peer thread is alive");
    }

    fn wait(&self) {
        self.ack
            .recv_timeout(Duration::from_secs(30))
            .expect("the peer thread answered");
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.cmd.send(PeerCmd::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// What one arm of the race saw.
#[derive(Debug, Default)]
struct Tally {
    attempts: u32,
    failed: u32,
    leaked: u32,
}

/// Run `attempts` rounds of the grant-then-cancel race, tallying how many
/// acquisitions failed and how many of those left a lock held afterwards.
///
/// `acquire` reports whether the acquisition succeeded. Every round ends with the
/// acquirer's locks dropped, so the next round starts from a session that has to
/// wait for real.
fn race<F>(
    peer: &Peer,
    acquirer: &PgDevSession,
    witness: &PgDevSession,
    cfg: &ExecutorConfig,
    attempts: u32,
    stop_at_first_leak: bool,
    budget: Duration,
    mut acquire: F,
) -> Tally
where
    F: FnMut(&PgDevSession, &ExecutorConfig) -> bool,
{
    let started = Instant::now();
    let mut tally = Tally::default();
    for _ in 0..attempts {
        if started.elapsed() > budget {
            break;
        }
        tally.attempts += 1;
        peer.send(PeerCmd::Hold);
        peer.wait();

        // Walk the release across a one millisecond band centred on the timeout,
        // so successive attempts sample both sides of the grant instant.
        let jitter = Duration::from_nanos((nanos() % u128::from(RELEASE_BAND_NANOS)) as u64);
        peer.send(PeerCmd::ReleaseAt(Instant::now() + RELEASE_FLOOR + jitter));
        let acquired = acquire(acquirer, cfg);
        peer.wait();

        if !acquired {
            tally.failed += 1;
            if sessions_holding(witness, cfg) > 0 {
                tally.leaked += 1;
            }
        }
        unlock_all(acquirer);
        if stop_at_first_leak && tally.leaked > 0 {
            break;
        }
    }
    tally
}

/// A cancelled lock wait that the server nonetheless granted must not leave the
/// lock held.
///
/// The control arm proves the harness reaches PostgreSQL's grant-then-cancel
/// window on this server, and the engine arm then runs the same race through the
/// shipped acquisition and must never leak.
#[test]
fn a_cancelled_lock_wait_leaves_no_advisory_lock_held() {
    let url = skip_if_no_pg!();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let peer = Peer::start(&url, &cfg.project_id);

    futures::executor::block_on(acquirer.batch(&format!(
        "SET statement_timeout = {}",
        WAIT_BUDGET.as_millis()
    )))
    .expect("bound the acquirer's lock wait");

    let control = race(
        &peer,
        &acquirer,
        &witness,
        &cfg,
        4_000,
        true,
        CONTROL_BUDGET,
        |session, cfg| {
            futures::executor::block_on(
                session.exec(ACQUIRE_SQL, &[cfg.project_id.as_str().into()]),
            )
            .is_ok()
        },
    );
    assert!(
        control.leaked > 0,
        "the harness never provoked a grant-then-cancel on this server in \
         {attempts} attempts ({failed} of them cancelled), so nothing below would \
         prove anything; the window is real but timing dependent, so a slower \
         server may need a wider budget",
        attempts = control.attempts,
        failed = control.failed,
    );

    // Calibrate the engine arm from how hard the control arm had to work: the
    // control leaked once in `control.attempts`, so this many attempts buys a
    // generous multiple of that dose.
    let engine_attempts = (control.attempts * 20).clamp(200, 4_000);
    let engine = race(
        &peer,
        &acquirer,
        &witness,
        &cfg,
        engine_attempts,
        false,
        CONTROL_BUDGET,
        |session, cfg| {
            futures::executor::block_on(
                PostgresBackend::new_generic(session).acquire_project_lock(cfg),
            )
            .is_ok()
        },
    );
    assert!(
        engine.failed > 0,
        "the engine arm never had an acquisition cancelled in {attempts} attempts, \
         so it never reached the path under test",
        attempts = engine.attempts,
    );
    assert_eq!(
        engine.leaked, 0,
        "a cancelled acquisition left a session advisory lock held that no caller \
         can release ({engine:?}); the control arm needed {control:?} to show the \
         same window is reachable here"
    );

    futures::executor::block_on(acquirer.batch("SET statement_timeout = 0"))
        .expect("restore the acquirer's timeout");
}

/// The blocking acquisition with its reply lost on the way back: a failure must
/// not leave a grant behind.
///
/// The acquiring session stays open across the assertion, so a pass cannot be
/// session exit dropping the lock on its way out.
#[test]
fn a_failed_blocking_acquire_leaves_no_advisory_lock_held() {
    let url = skip_if_no_pg!();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());

    acquirer.fail_reply_after_running("pg_advisory_lock");
    let backend = PostgresBackend::new_generic(&acquirer);
    let error = futures::executor::block_on(backend.acquire_project_lock(&cfg))
        .expect_err("the acquisition reports the lost reply as a failure");

    assert_eq!(
        sessions_holding(&witness, &cfg),
        0,
        "a failed acquire left a session advisory lock held that no caller can \
         release: {error}"
    );
}

/// The non-blocking acquisition, transport branch: `pg_try_advisory_lock` runs and
/// grants, and the reply is lost on the way back.
#[test]
fn a_failed_try_acquire_leaves_no_advisory_lock_held() {
    let url = skip_if_no_pg!();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());

    acquirer.fail_reply_after_running("pg_try_advisory_lock");
    let backend = PostgresBackend::new_generic(&acquirer);
    let error = futures::executor::block_on(backend.try_acquire_project_lock(&cfg))
        .expect_err("the acquisition reports the lost reply as a failure");

    assert_eq!(
        sessions_holding(&witness, &cfg),
        0,
        "a failed try-acquire left a session advisory lock held that no caller can \
         release: {error}"
    );
}

/// The non-blocking acquisition, decode branch: the reply arrives but the `got`
/// column will not read back as a boolean, so the caller never learns it holds the
/// lock the server granted.
#[test]
fn an_undecodable_try_acquire_reply_leaves_no_advisory_lock_held() {
    let url = skip_if_no_pg!();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());

    acquirer.undecodable_bool_reply_after_running("pg_try_advisory_lock", "got");
    let backend = PostgresBackend::new_generic(&acquirer);
    let error = futures::executor::block_on(backend.try_acquire_project_lock(&cfg))
        .expect_err("an undecodable acquisition result is a failure");

    assert_eq!(
        sessions_holding(&witness, &cfg),
        0,
        "a try-acquire whose result could not be decoded left a session advisory \
         lock held that no caller can release: {error}"
    );
}

/// The positive control for the blocking path: a successful acquire still HOLDS
/// the lock afterwards, and one release drops exactly one level of the hold.
///
/// Without this arm the compensation tests above would also pass on an engine that
/// released the lock unconditionally, which is a far worse defect than the one
/// they are guarding.
#[test]
fn a_successful_acquire_holds_the_lock_and_one_release_drops_one_hold() {
    let url = skip_if_no_pg!();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let backend = PostgresBackend::new_generic(&acquirer);

    futures::executor::block_on(backend.acquire_project_lock(&cfg)).expect("acquire");
    assert_eq!(
        sessions_holding(&witness, &cfg),
        1,
        "a successful acquire must leave the lock held"
    );

    // Session advisory locks stack by depth, so a second acquire followed by ONE
    // release must still leave the lock held. That is what distinguishes a release
    // that drops exactly one hold from one that drops every hold.
    futures::executor::block_on(backend.acquire_project_lock(&cfg)).expect("re-acquire");
    futures::executor::block_on(backend.release_project_lock(&cfg)).expect("release once");
    assert_eq!(
        sessions_holding(&witness, &cfg),
        1,
        "one release must drop one hold, not the whole stack"
    );

    futures::executor::block_on(backend.release_project_lock(&cfg)).expect("release again");
    assert_eq!(
        sessions_holding(&witness, &cfg),
        0,
        "the matching release must free the lock"
    );
}

/// The positive control for the non-blocking path: a successful try-acquire
/// reports `Acquired` and still holds the lock the caller is about to rely on.
#[test]
fn a_successful_try_acquire_holds_the_lock_it_reports() {
    let url = skip_if_no_pg!();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let backend = PostgresBackend::new_generic(&acquirer);

    match futures::executor::block_on(backend.try_acquire_project_lock(&cfg))
        .expect("an uncontended try-acquire succeeds")
    {
        ProjectLockAcquisition::Acquired => {}
        ProjectLockAcquisition::Busy(holders) => {
            panic!("nothing holds this project's lock, yet the acquisition reported {holders:?}")
        }
    }
    assert_eq!(
        sessions_holding(&witness, &cfg),
        1,
        "a successful try-acquire must leave the lock held"
    );

    futures::executor::block_on(backend.release_project_lock(&cfg)).expect("release");
    assert_eq!(
        sessions_holding(&witness, &cfg),
        0,
        "the release must free the lock"
    );
}
