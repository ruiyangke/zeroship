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
//! the millisecond band where the acquirer's `lock_timeout` fires, and often
//! enough the release grants the lock to a waiter that is already on its way out.
//! The budget is a `lock_timeout` because that timeout can only end a lock wait:
//! the harness's own statements between attempts never wait, so a busy server
//! slows the race down without cancelling the harness itself, which a
//! `statement_timeout` on the acquiring session would.
//! It carries its own control arm, because a race is worth nothing as coverage
//! unless the run can show it actually happened: the control arm sends the same
//! bare `pg_advisory_lock` the engine sends, with no compensation around it, and
//! the test fails if that arm never leaks a lock. Only once the harness has proven
//! it can provoke the window on this server does the engine arm's clean result
//! mean anything, and the engine arm then gets a dose of the same race calibrated
//! from how hard the control arm had to work.
//!
//! The budget tests cover a caller whose session carries a `statement_timeout`.
//! The server applies it to every statement, so the compensation for a failed
//! acquisition has to run outside it, while the wait itself stays bounded by it.
//! A server too slow to finish the compensation inside that budget is injected at
//! the driver seam as a delay inside the compensating statement.
//!
//! The remaining tests cover the shapes the server-side race cannot reach: a reply
//! lost on the way back from an acquisition the server ran, and a reply that
//! arrives but will not decode. `pg_try_advisory_lock` never waits, so it has no
//! grant-then-cancel window; its exposure is entirely a granted lock whose result
//! the caller never learned. Those are injected at the driver seam, and everything
//! around the injection is still live: a real grant in a real session that stays
//! open for the whole assertion, judged by a `pg_locks` read from a second session.
//!
//! Runs in a database of its own on the PostgreSQL server this binary owns
//! (`crate::support::pg_database`).


use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::support::PgDevSession;

use zeroship_migrate::apply::backend::MigrationBackend;
use zeroship_migrate::driver::SqlSession;
use zeroship_migrate::model::migration::Checksum;
use zeroship_migrate::{
    ApplyError, Approval, DiscardStep, EngineError, ExecutorConfig, Migration, MigrationEngine,
    MigrationFlags, MigrationId, ProjectLockAcquisition, SessionDiscard, SessionResidue,
};
use zeroship_migrate_postgres::PostgresBackend;

/// The engine's own acquisition statement, sent verbatim by the control arm so it
/// races the identical server-side path with no compensation wrapped around it.
const ACQUIRE_SQL: &str = "SELECT pg_advisory_lock( \
        (h >> 32)::int4, ((h << 32) >> 32)::int4 \
   ) FROM (SELECT hashtextextended($1, 0) AS h) AS project_lock_key";

/// How long the acquirer gives the lock wait before `lock_timeout` cancels
/// it. Small, because the whole harness is a peer release aimed at that instant.
const WAIT_BUDGET: Duration = Duration::from_millis(3);

/// Where an arm's first attempt aims the peer's release, measured from the moment
/// the release is armed: the wait budget, as if the acquirer's wait began at once.
///
/// Each arm then steers its own aim (see [`race`]), because how long an arm takes
/// to start waiting depends on the statements it sends before it asks for the
/// lock, and the engine arm sends more of them than the control arm.
const RELEASE_START: Duration = WAIT_BUDGET;

/// How far one attempt moves an arm's aim toward its cancellation instant.
const RELEASE_STEP: Duration = Duration::from_micros(50);

/// How wide a band around the aim the release is jittered across, so successive
/// attempts sample both sides of the cancellation instant instead of landing on
/// one side of it.
const RELEASE_BAND: Duration = Duration::from_millis(1);

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
        crate::support::no_inject(&format!("proj_{tok}")),
    );
    c.confinement.meta_schema = format!("meta_{tok}");
    c
}

/// How many sessions in the cluster hold this project's advisory lock right now.
///
/// Read from `pg_locks` through a session that is NOT the acquiring one, so the
/// answer is the server's own record of the grant rather than the engine's
/// bookkeeping about it. A two-argument `pg_advisory_lock(int4, int4)` is
/// recorded with `objsubid = 2`, with its signed key halves stored as the raw
/// 32 bits in `classid` and `objid`.
fn sessions_holding(witness: &PgDevSession, cfg: &ExecutorConfig) -> i64 {
    futures::executor::block_on(async {
        witness
            .query_one(
                "SELECT count(*)::int8 AS held \
                   FROM pg_locks \
                   CROSS JOIN (SELECT hashtextextended($1, 0) AS h) AS project_lock_key \
                  WHERE locktype = 'advisory' AND granted AND objsubid = 2 \
                    AND database = (SELECT oid FROM pg_database WHERE datname = current_database()) \
                    AND classid = ((h >> 32)::int4)::oid \
                    AND objid = (((h << 32) >> 32)::int4)::oid",
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
    /// acquirer's `lock_timeout` fires.
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
///
/// The release is aimed at the acquirer's cancellation instant by feedback: an
/// acquisition that was granted means the release came before that instant, so
/// the next release is aimed later, and one that failed means it came after, so
/// the next is aimed earlier. The aim settles where the two outcomes balance,
/// which is where a release can land inside the grant-then-cancel window.
#[allow(clippy::too_many_arguments)]
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
    let mut aim = RELEASE_START;
    for _ in 0..attempts {
        if started.elapsed() > budget {
            break;
        }
        tally.attempts += 1;
        peer.send(PeerCmd::Hold);
        peer.wait();

        let band = RELEASE_BAND.as_nanos();
        let jitter = Duration::from_nanos(u64::try_from(nanos() % band).expect("inside the band"));
        let release = (aim + jitter).saturating_sub(RELEASE_BAND / 2);
        peer.send(PeerCmd::ReleaseAt(Instant::now() + release));
        let acquired = acquire(acquirer, cfg);
        peer.wait();
        aim = if acquired {
            aim + RELEASE_STEP
        } else {
            aim.saturating_sub(RELEASE_STEP)
        };

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
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let peer = Peer::start(&url, &cfg.project_id);

    futures::executor::block_on(acquirer.batch(&format!(
        "SET lock_timeout = {}",
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

    futures::executor::block_on(acquirer.batch("SET lock_timeout = 0"))
        .expect("restore the acquirer's timeout");
}

/// The statement budget the caller's session carries in the budget tests below.
const CALLER_STATEMENT_BUDGET: Duration = Duration::from_millis(100);

/// How long the server spends on the compensating unlock in the budget tests: far
/// past [`CALLER_STATEMENT_BUDGET`], so a compensation running under that budget is
/// cancelled before it unlocks anything.
const SLOW_COMPENSATION: Duration = Duration::from_secs(1);

/// Give `session` the caller's statement budget of the tests below.
fn carry_statement_budget(session: &PgDevSession, budget: Duration) {
    futures::executor::block_on(session.batch(&format!(
        "SET statement_timeout = {}",
        budget.as_millis()
    )))
    .expect("give the caller's session a statement budget");
}

/// The blocking acquisition, granted and then failed, with a server too slow to
/// finish the compensating unlock inside the caller's statement budget.
///
/// The server applies `statement_timeout` to every statement, the compensation
/// included, so a compensation under the caller's budget is cancelled exactly when
/// the server is slow, which is also when acquisitions time out. The grant must be
/// dropped anyway.
#[test]
fn a_failed_acquire_drops_its_grant_past_the_callers_statement_budget() {
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    carry_statement_budget(&acquirer, CALLER_STATEMENT_BUDGET);

    acquirer.fail_reply_after_running("pg_advisory_lock");
    acquirer.slow_next_statement("pg_advisory_unlock", SLOW_COMPENSATION);
    let backend = PostgresBackend::new_generic(&acquirer);
    let error = futures::executor::block_on(backend.acquire_project_lock(&cfg))
        .expect_err("the acquisition reports the lost reply as a failure");

    assert_eq!(
        acquirer.unfired_faults(),
        Vec::<String>::new(),
        "the server must have granted the lock and then run the slow compensation, \
         or this test proved nothing: {error}"
    );
    assert_eq!(
        sessions_holding(&witness, &cfg),
        0,
        "a compensation slower than the caller's statement budget left the grant \
         held: {error}"
    );
}

/// The non-blocking acquisition under the same conditions: granted, reply lost,
/// and a compensation slower than the caller's statement budget.
#[test]
fn a_failed_try_acquire_drops_its_grant_past_the_callers_statement_budget() {
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    carry_statement_budget(&acquirer, CALLER_STATEMENT_BUDGET);

    acquirer.fail_reply_after_running("pg_try_advisory_lock");
    acquirer.slow_next_statement("pg_advisory_unlock", SLOW_COMPENSATION);
    let backend = PostgresBackend::new_generic(&acquirer);
    let error = futures::executor::block_on(backend.try_acquire_project_lock(&cfg))
        .expect_err("the acquisition reports the lost reply as a failure");

    assert_eq!(
        acquirer.unfired_faults(),
        Vec::<String>::new(),
        "the server must have granted the lock and then run the slow compensation, \
         or this test proved nothing: {error}"
    );
    assert_eq!(
        sessions_holding(&witness, &cfg),
        0,
        "a compensation slower than the caller's statement budget left the grant \
         held: {error}"
    );
}

/// This session's `statement_timeout` and `lock_timeout`, as `current_setting`
/// reports them.
fn timeouts(session: &PgDevSession) -> (String, String) {
    let row = futures::executor::block_on(session.query_one(
        "SELECT current_setting('statement_timeout') AS st, \
                current_setting('lock_timeout') AS lt",
        &[],
    ))
    .expect("read the session's timeouts");
    (
        row.try_get("st").expect("decode statement_timeout"),
        row.try_get("lt").expect("decode lock_timeout"),
    )
}

/// Every acquisition leaves the caller's `statement_timeout` and `lock_timeout` as
/// it found them, whether it succeeded, failed, or did not wait.
#[test]
fn an_acquisition_leaves_the_callers_timeouts_as_it_found_them() {
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    futures::executor::block_on(
        acquirer.batch("SET statement_timeout = '1234ms'; SET lock_timeout = '4321ms'"),
    )
    .expect("give the caller's session its own timeouts");
    let caller = timeouts(&acquirer);
    assert_eq!(
        caller,
        ("1234ms".to_string(), "4321ms".to_string()),
        "the session must carry the timeouts this test gave it"
    );
    let backend = PostgresBackend::new_generic(&acquirer);

    futures::executor::block_on(backend.acquire_project_lock(&cfg)).expect("acquire");
    assert_eq!(timeouts(&acquirer), caller, "after a successful acquisition");
    futures::executor::block_on(backend.release_project_lock(&cfg)).expect("release");

    acquirer.fail_reply_after_running("pg_advisory_lock");
    futures::executor::block_on(backend.acquire_project_lock(&cfg))
        .expect_err("the acquisition reports the lost reply as a failure");
    assert_eq!(acquirer.unfired_faults(), Vec::<String>::new());
    assert_eq!(timeouts(&acquirer), caller, "after a failed acquisition");

    match futures::executor::block_on(backend.try_acquire_project_lock(&cfg))
        .expect("an uncontended try-acquire succeeds")
    {
        ProjectLockAcquisition::Acquired => {}
        ProjectLockAcquisition::Busy(holders) => {
            panic!("nothing holds this project's lock, yet the acquisition reported {holders:?}")
        }
    }
    assert_eq!(timeouts(&acquirer), caller, "after a try-acquisition");
    futures::executor::block_on(backend.release_project_lock(&cfg)).expect("release");
}

/// The tighter of the caller's two timeouts in each case of the bound test below.
const WAIT_BOUND: Duration = Duration::from_millis(200);

/// How long a peer holding the lock in the bound test stays connected while
/// idle. An acquisition its caller's timeouts fail to bound outlives this, and is
/// then granted the lock and reported as a success, which fails the test instead
/// of hanging it.
const PEER_IDLE_LIMIT: Duration = Duration::from_secs(10);

/// The caller's timeouts still bound the wait for a contended lock: whichever of
/// `statement_timeout` and `lock_timeout` is tighter ends it.
#[test]
fn the_callers_tighter_timeout_bounds_the_lock_wait() {
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let bound = WAIT_BOUND.as_millis();
    let slack = 60_000;
    let cases = [
        format!("SET statement_timeout = {bound}; SET lock_timeout = 0"),
        format!("SET statement_timeout = 0; SET lock_timeout = {bound}"),
        format!("SET statement_timeout = {bound}; SET lock_timeout = {slack}"),
        format!("SET statement_timeout = {slack}; SET lock_timeout = {bound}"),
    ];
    for setting in &cases {
        let peer = PgDevSession::connect(&url);
        futures::executor::block_on(peer.batch(&format!(
            "SET idle_session_timeout = {}",
            PEER_IDLE_LIMIT.as_millis()
        )))
        .expect("bound how long the peer stays connected");
        futures::executor::block_on(peer.exec(ACQUIRE_SQL, &[cfg.project_id.as_str().into()]))
            .expect("the peer takes the project lock");
        futures::executor::block_on(acquirer.batch(setting)).expect("set the caller's timeouts");

        let started = Instant::now();
        let outcome = futures::executor::block_on(
            PostgresBackend::new_generic(&acquirer).acquire_project_lock(&cfg),
        );
        let waited = started.elapsed();

        assert!(
            outcome.is_err(),
            "with {setting}, the acquisition was granted after {waited:?}: the caller's \
             timeouts did not bound the wait"
        );
        assert!(
            waited >= WAIT_BOUND,
            "with {setting}, the acquisition failed after {waited:?}, before its wait \
             could have reached the bound: {outcome:?}"
        );
        drop(peer);
    }
}

/// The discard outcome `error` carries, or a panic naming what came back instead.
fn discard_of(error: &ApplyError) -> &SessionDiscard {
    error
        .session_discard()
        .unwrap_or_else(|| panic!("the session must be reported for discard, got: {error}"))
}

/// An acquisition whose closing restore of the caller's timeouts goes
/// unconfirmed reports the session to discard, naming the lock it holds and the
/// timeouts it may still carry.
///
/// The restore's reply is lost after the server ran it, so the caller's budget is
/// back in force; a compensating unlock sent now would run under that budget, and
/// one slower than it would be cancelled with the grant still held. The slow
/// unlock armed here is that server. The acquisition must not hide the held lock
/// behind a plain restore error whichever way it settles.
#[test]
fn a_lost_restore_reply_reports_the_held_lock_and_the_timeouts() {
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    carry_statement_budget(&acquirer, CALLER_STATEMENT_BUDGET);

    acquirer.fail_reply_after_running("set_config('statement_timeout', $1");
    acquirer.slow_next_statement("pg_advisory_unlock", SLOW_COMPENSATION);
    let backend = PostgresBackend::new_generic(&acquirer);
    let error = futures::executor::block_on(backend.acquire_project_lock(&cfg))
        .expect_err("a lost restore reply is not a clean acquisition");

    assert_eq!(
        acquirer.unfired_faults(),
        vec!["pg_advisory_unlock".to_string()],
        "the restore must have run, and nothing more may be sent on a session \
         whose restore failed: {error}"
    );
    let discard = discard_of(&error);
    assert_eq!(discard.step(), DiscardStep::TimeoutRestore, "{error}");
    assert_eq!(
        discard.residue(),
        SessionResidue::PROJECT_LOCK.and(SessionResidue::TIMEOUTS),
        "{error}"
    );
    assert_eq!(
        sessions_holding(&witness, &cfg),
        1,
        "the report must be true: the acquisition did take the lock"
    );
}

/// A lift whose `set_config` reply is lost may have taken effect, so the
/// acquisition reports the session's timeouts as unconfirmed, and asks for no
/// lock on a session it cannot vouch for.
#[test]
fn a_lost_lift_reply_reports_the_timeouts_and_takes_no_lock() {
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    carry_statement_budget(&acquirer, CALLER_STATEMENT_BUDGET);

    acquirer.fail_reply_after_running("set_config('statement_timeout', '0'");
    let backend = PostgresBackend::new_generic(&acquirer);
    let error = futures::executor::block_on(backend.acquire_project_lock(&cfg))
        .expect_err("an unconfirmed lift is not a clean acquisition");

    assert_eq!(
        acquirer.unfired_faults(),
        Vec::<String>::new(),
        "the lift must have run, or this test proved nothing: {error}"
    );
    let discard = discard_of(&error);
    assert_eq!(discard.step(), DiscardStep::TimeoutLift, "{error}");
    assert_eq!(discard.residue(), SessionResidue::TIMEOUTS, "{error}");
    assert_eq!(
        sessions_holding(&witness, &cfg),
        0,
        "no lock may be requested after an unconfirmed lift"
    );
}

/// A session inside the caller's transaction block is refused before any lock is
/// requested, by the blocking and the non-blocking acquisition alike.
///
/// An acquisition that failed inside a block could not drop the grant it may
/// hold until the caller ended the block. The lock is free here, so without the
/// refusal both acquisitions would take it inside the block.
#[test]
fn an_acquisition_inside_a_transaction_block_is_refused_before_it_asks_for_the_lock() {
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let backend = PostgresBackend::new_generic(&acquirer);
    futures::executor::block_on(acquirer.batch("BEGIN")).expect("open the caller's block");

    let blocking = futures::executor::block_on(backend.acquire_project_lock(&cfg))
        .expect_err("a session inside a transaction block is refused");
    assert!(
        blocking.to_string().contains("outside a transaction block"),
        "the refusal names the transaction block: {blocking}"
    );
    futures::executor::block_on(acquirer.batch("ROLLBACK")).expect("end the caller's block");
    futures::executor::block_on(acquirer.batch("BEGIN")).expect("open the caller's block");
    let non_blocking = futures::executor::block_on(backend.try_acquire_project_lock(&cfg))
        .expect_err("a session inside a transaction block is refused");
    assert!(
        non_blocking.to_string().contains("outside a transaction block"),
        "the refusal names the transaction block: {non_blocking}"
    );
    assert_eq!(
        sessions_holding(&witness, &cfg),
        0,
        "no lock may be requested inside the caller's block"
    );
    futures::executor::block_on(acquirer.batch("ROLLBACK")).expect("end the caller's block");

    futures::executor::block_on(backend.acquire_project_lock(&cfg))
        .expect("the same session outside a block acquires");
    assert_eq!(sessions_holding(&witness, &cfg), 1, "the acquisition holds the lock");
}

/// A lift whose `set_config` the server answered with an error changed nothing,
/// so the acquisition reports that plain failure, not a session to discard, and
/// the caller's timeouts are as they were.
#[test]
fn a_lift_the_server_refused_is_a_plain_failure_on_an_untouched_session() {
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    carry_statement_budget(&acquirer, CALLER_STATEMENT_BUDGET);
    let caller = timeouts(&acquirer);

    acquirer.slow_next_statement("set_config('statement_timeout', '0'", SLOW_COMPENSATION);
    let backend = PostgresBackend::new_generic(&acquirer);
    let error = futures::executor::block_on(backend.acquire_project_lock(&cfg))
        .expect_err("the caller's own budget cancels the slow lift");

    assert_eq!(
        acquirer.unfired_faults(),
        Vec::<String>::new(),
        "the lift must have run slow, or this test proved nothing: {error}"
    );
    assert!(
        error.session_discard().is_none(),
        "a lift the server refused leaves nothing to discard: {error}"
    );
    assert_eq!(timeouts(&acquirer), caller, "the caller's timeouts are untouched");
}

/// The holder probe after a contended try-acquisition is an ordinary read, and
/// runs under the caller's own statement budget, not the lifted one.
#[test]
fn the_holder_probe_runs_under_the_callers_statement_budget() {
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let peer = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    futures::executor::block_on(peer.exec(ACQUIRE_SQL, &[cfg.project_id.as_str().into()]))
        .expect("the peer takes the project lock");
    carry_statement_budget(&acquirer, CALLER_STATEMENT_BUDGET);

    acquirer.slow_next_statement("pg_stat_activity", SLOW_COMPENSATION);
    let backend = PostgresBackend::new_generic(&acquirer);
    let outcome = futures::executor::block_on(backend.try_acquire_project_lock(&cfg));

    assert_eq!(
        acquirer.unfired_faults(),
        Vec::<String>::new(),
        "the holder probe must have run slow, or this test proved nothing"
    );
    match outcome {
        Err(error) => assert!(
            error.to_string().contains("statement timeout"),
            "the caller's budget is what ends the slow probe: {error}"
        ),
        Ok(acquisition) => panic!(
            "a probe slower than the caller's budget ran to the end, so it ran \
             unbudgeted: {acquisition:?}"
        ),
    }
}

/// A rollback whose acquisition loses its restore reply reports the session to
/// discard on the rollback surface, as an apply does.
#[test]
fn a_rollback_whose_acquisition_cannot_settle_reports_the_session_to_discard() {
    let url = crate::support::pg_database();
    let acquirer = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    carry_statement_budget(&acquirer, CALLER_STATEMENT_BUDGET);

    acquirer.fail_reply_after_running("set_config('statement_timeout', $1");
    let backend = PostgresBackend::new_generic(&acquirer);
    let guard = zeroship_migrate::guard_for(
        zeroship_migrate::shipping_vendors(),
        &cfg.guard_config_for(&zeroship_migrate_postgres::DIALECT),
    );
    let error = futures::executor::block_on(zeroship_migrate::rollback(
        &backend,
        &cfg,
        &zeroship_migrate::RollbackRequest::new(zeroship_migrate::RollbackTarget::All),
        &[],
        Approval::Approved,
        "lock-release-test",
        &*guard,
    ))
    .expect_err("an acquisition that cannot settle is not a clean rollback");

    assert_eq!(
        acquirer.unfired_faults(),
        Vec::<String>::new(),
        "the acquisition's restore must have run, or this test proved nothing: {error}"
    );
    let discard = error
        .session_discard()
        .unwrap_or_else(|| panic!("the rollback must report the session to discard: {error}"));
    assert!(
        discard.residue().contains(SessionResidue::PROJECT_LOCK),
        "the session holds the lock the acquisition took: {error}"
    );
    assert_eq!(sessions_holding(&witness, &cfg), 1, "the report must be true");
}

/// The statement budget of the release tests below: generous for every
/// statement an apply sends, and far short of the slow release.
const RELEASE_TEST_BUDGET: Duration = Duration::from_secs(1);

/// How long the server spends on the closing release in the release tests.
const SLOW_RELEASE: Duration = Duration::from_secs(2);

/// A transactional migration with a correct checksum.
fn mig(name: &str, up: &str) -> Migration {
    let flags = MigrationFlags::default();
    Migration {
        version: MigrationId::generate(),
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(
            &zeroship_migrate::ChecksumInput {
                up,
                down: None,
                flags: &flags,
                owner_app: "app_test",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            },
        ),
        flags,
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
        effect: None,
    }
}

/// Apply `migration` on a session whose closing release the server is too slow
/// to finish inside the session's statement budget, and report what the apply
/// returned and how many sessions still hold the lock afterwards.
fn apply_with_a_cancelled_release(migration: Migration) -> (ApplyError, i64) {
    let url = crate::support::pg_database();
    let session = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schemas = crate::support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    futures::executor::block_on(session.batch(&format!(
        "CREATE SCHEMA \"{}\"",
        cfg.project_schema
    )))
    .expect("create the project schema");
    carry_statement_budget(&session, RELEASE_TEST_BUDGET);

    session.slow_next_statement("pg_advisory_unlock", SLOW_RELEASE);
    let error = futures::executor::block_on(crate::support::apply_pg(
        &session,
        &cfg,
        &[migration],
        Approval::Approved,
        "lock-release-test",
    ))
    .expect_err("a cancelled release is not a clean apply");
    assert_eq!(
        session.unfired_faults(),
        Vec::<String>::new(),
        "the release must have run slow, or this test proved nothing: {error}"
    );
    let held = sessions_holding(&witness, &cfg);
    futures::executor::block_on(session.batch("SET statement_timeout = 0"))
        .expect("lift the budget so the schema guard can drop the schemas");
    (error, held)
}

/// An apply that failed, whose closing release the session's own budget then
/// cancelled, reports the session to discard and carries the apply's failure.
///
/// The lock is still held, so returning the apply's error alone would leave a
/// caller that retries on the same session holding the lock for good.
#[test]
fn a_failed_apply_whose_release_is_cancelled_reports_the_session_to_discard() {
    let (error, held) = apply_with_a_cancelled_release(mig(
        "fails_to_apply",
        "CREATE TABLE never_created (id no_such_type)",
    ));

    let discard = discard_of(&error);
    assert_eq!(discard.step(), DiscardStep::ProjectLockRelease, "{error}");
    assert_eq!(discard.residue(), SessionResidue::PROJECT_LOCK, "{error}");
    let operation = discard
        .operation()
        .unwrap_or_else(|| panic!("the apply's own failure must travel inside: {error}"));
    assert!(
        operation.to_string().contains("no_such_type"),
        "the apply's failure is the migration's: {operation}"
    );
    assert_eq!(held, 1, "the report must be true: the release did not happen");
}

/// An apply that succeeded, whose closing release the session's own budget then
/// cancelled, reports the session to discard with no failure of its own.
#[test]
fn a_successful_apply_whose_release_is_cancelled_reports_the_session_to_discard() {
    let (error, held) = apply_with_a_cancelled_release(mig(
        "applies",
        "CREATE TABLE created_before_the_release (id int)",
    ));

    let discard = discard_of(&error);
    assert_eq!(discard.step(), DiscardStep::ProjectLockRelease, "{error}");
    assert!(discard.operation().is_none(), "the apply succeeded: {error}");
    assert_eq!(held, 1, "the report must be true: the release did not happen");
}

/// An envelope deploy whose closing release the session's own budget cancels
/// reports the session to discard through the engine's own error.
#[test]
fn an_envelope_deploy_whose_release_is_cancelled_reports_the_session_to_discard() {
    let url = crate::support::pg_database();
    let session = PgDevSession::connect(&url);
    let witness = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schemas = crate::support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    futures::executor::block_on(session.batch(&format!(
        "CREATE SCHEMA \"{}\"",
        cfg.project_schema
    )))
    .expect("create the project schema");
    carry_statement_budget(&session, RELEASE_TEST_BUDGET);

    session.slow_next_statement("pg_advisory_unlock", SLOW_RELEASE);
    let backend = PostgresBackend::new_generic(&session);
    let error = futures::executor::block_on(
        MigrationEngine::new(zeroship_migrate::shipping_vendors()).deploy_envelopes(
            &[],
            &backend,
            cfg.effective(),
            &zeroship_migrate_postgres::DIALECT,
            &cfg.project_schema,
            "app_test",
            &std::collections::BTreeMap::new(),
            Approval::Approved,
            &cfg,
        ),
    )
    .expect_err("a cancelled release is not a clean deploy");
    assert_eq!(
        session.unfired_faults(),
        Vec::<String>::new(),
        "the release must have run slow, or this test proved nothing: {error}"
    );

    let discard = discard_of(match &error {
        EngineError::Apply(apply) => apply,
        other => panic!("the deploy must report the session to discard, got: {other}"),
    });
    assert_eq!(discard.step(), DiscardStep::ProjectLockRelease, "{error}");
    assert_eq!(sessions_holding(&witness, &cfg), 1, "the release did not happen");
    futures::executor::block_on(session.batch("SET statement_timeout = 0"))
        .expect("lift the budget so the schema guard can drop the schemas");
}

/// The blocking acquisition with its reply lost on the way back: a failure must
/// not leave a grant behind.
///
/// The acquiring session stays open across the assertion, so a pass cannot be
/// session exit dropping the lock on its way out.
#[test]
fn a_failed_blocking_acquire_leaves_no_advisory_lock_held() {
    let url = crate::support::pg_database();
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
    let url = crate::support::pg_database();
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
    let url = crate::support::pg_database();
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
    let url = crate::support::pg_database();
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
    let url = crate::support::pg_database();
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

/// An apply that failed and whose restore of the caller's settings then went
/// unconfirmed reports the session to discard with the settings residue, carrying
/// the apply's failure, instead of returning that failure alone.
///
/// The settings in question are the role, `search_path` and timeouts the apply
/// set for its migrations; a pooled caller that reused the session unawares
/// would run its own statements on the project schema's `search_path`.
#[test]
fn a_failed_apply_whose_settings_restore_is_lost_reports_the_session_to_discard() {
    let url = crate::support::pg_database();
    let session = PgDevSession::connect(&url);
    let cfg = cfg_for(&token());
    let _schemas = crate::support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    futures::executor::block_on(session.batch(&format!(
        "CREATE SCHEMA \"{}\"",
        cfg.project_schema
    )))
    .expect("create the project schema");

    session.fail_reply_after_running("set_config('search_path', $3");
    let error = futures::executor::block_on(crate::support::apply_pg(
        &session,
        &cfg,
        &[mig("fails_to_apply", "CREATE TABLE never_created (id no_such_type)")],
        Approval::Approved,
        "settings-restore-test",
    ))
    .expect_err("the migration fails");

    assert_eq!(
        session.unfired_faults(),
        Vec::<String>::new(),
        "the settings restore must have run, or this test proved nothing: {error}"
    );
    let discard = discard_of(&error);
    assert_eq!(discard.step(), DiscardStep::SettingsRestore, "{error}");
    assert!(
        discard.residue().contains(SessionResidue::SETTINGS),
        "the report names the settings: {error}"
    );
    assert!(
        discard
            .operation()
            .is_some_and(|operation| operation.to_string().contains("no_such_type")),
        "the apply's own failure travels inside the report: {error}"
    );
}
