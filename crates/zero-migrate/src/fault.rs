//! Test-support **fault-injection seam** (crash simulation).
//!
//! The migration executor is two-phase + idempotent by design: a crash at any
//! point must leave the journal in a state a resume can converge from. To PROVE
//! that at *sub-step* granularity — mid-step, after a DDL/data statement but
//! before/after its journal row, between an online rename's E1/E2/E3 phases,
//! mid-backfill-batch — the executor consults this seam at named boundaries and
//! aborts the in-flight step (returning [`crate::apply::executor::ApplyError`]) when a fault is armed for
//! that boundary. Aborting mid-transaction is behaviorally identical to a process
//! crash there: the open transaction rolls back, exactly as it would on a real
//! crash, and the resume runs the same recovery path.
//!
//! **Inert until something arms it.** With no fault armed, [`trip`] is a single
//! relaxed atomic load that returns `Ok(())`. Nothing here reads the environment
//! or persists, so the seam cannot be switched on from outside the process.
//!
//! That is a RUNTIME default, not a compile-time absence. This module carries no
//! `cfg` gate and ships in release builds, and the boundaries it guards are the
//! real executor's. Which consumers can and cannot reach it is a property of how
//! each one drives the engine future, spelled out on [`arm`] - the shipping Node
//! addon cannot, a Rust embedder driving the future on its own thread can. Do not
//! read this paragraph as a guarantee that nothing arms it; read [`arm`].
//!
//! Hidden from the public docs by `doc(hidden)`, which removes it from rustdoc
//! output and not from the build - it is a test-support surface, not a stable API.

#![doc(hidden)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Fast-path gate: `false` whenever no fault is armed, so [`trip`] short-circuits
/// on a single relaxed load on every production boundary.
static ARMED_THREADS: AtomicUsize = AtomicUsize::new(0);

/// One thread's armed faults: `point name -> remaining hits before it fires`. A
/// fault armed with `skip = k` fires on the `(k+1)`-th trip of that point (so
/// `skip = 0` fires on the first trip). It fires AT MOST ONCE, then disarms itself.
///
/// A newtype rather than a bare `HashMap` so the thread's claim on
/// `ARMED_THREADS` can be released by `Drop` at thread exit. Without that, a
/// thread that arms a fault, never trips it, and exits without calling
/// [`disarm_all`] leaves the counter permanently incremented, which disables
/// [`trip`]'s single-atomic-load fast path for every thread for the life of the
/// process. This does NOT cover threads whose destructors never run (a
/// `process::exit`, an abort, or the platforms where the main thread's TLS
/// destructors are skipped); those cases end the process anyway.
#[derive(Default)]
struct Registry(HashMap<String, u32>);

impl Drop for Registry {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            ARMED_THREADS.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

thread_local! {
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());
}

/// Arm a one-shot crash at `point`, firing after `skip` prior trips of it (so
/// `skip = 0` means crash on the first trip of `point`).
///
/// Armed faults live in a `thread_local!`, so this arms `point` for the CALLING
/// THREAD ONLY; a [`trip`] of the same point on any other thread is unaffected.
///
/// The shipping Node consumer runs every apply on a freshly spawned worker
/// thread: `run_engine_blocking` in the `zero-migrate-node` runtime spawns the
/// thread that drives the engine future, and that worker never calls `arm`.
/// An external caller that arms a fault on its own thread therefore cannot reach
/// a production apply through that consumer.
///
/// That inertness is a property of the zero-migrate-node worker spawn, NOT of
/// this crate. This module carries no `cfg` gate and ships in release builds, and
/// `arm` checks nothing about the build profile, the environment, or the caller.
/// A Rust embedder that drives the engine future on a thread it also controls
/// inherits a LIVE crash-injection seam: arm and apply on one thread and the
/// ten executor trip points fire for real.
pub fn arm(point: &str, skip: u32) {
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        if registry.0.is_empty() {
            ARMED_THREADS.fetch_add(1, Ordering::SeqCst);
        }
        registry.0.insert(point.to_string(), skip);
    });
}

/// Clear the CALLING THREAD's armed faults (call between crash-fuzz iterations).
/// The registry is a `thread_local!`, so faults armed on other threads survive
/// this call untouched.
///
/// Not a production-safety mechanism on its own: see [`arm`] for which consumer
/// the seam is inert under and which one inherits it live.
pub fn disarm_all() {
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        if !registry.0.is_empty() {
            registry.0.clear();
            ARMED_THREADS.fetch_sub(1, Ordering::SeqCst);
        }
    });
}

/// Number of threads that currently hold at least one armed fault.
///
/// TEST-ONLY read accessor, exposed for one reason: the integration suite pins
/// that a thread which arms a fault and then exits without disarming releases its
/// claim on `ARMED_THREADS` (the `Registry` `Drop` guard), which is otherwise
/// unobservable from outside the module. The engine never reads this; [`trip`]
/// reads the atomic directly.
#[must_use]
pub fn armed_thread_count() -> usize {
    ARMED_THREADS.load(Ordering::SeqCst)
}

/// The executor's boundary check: returns an injected [`crate::apply::executor::ApplyError`] (a simulated
/// crash) iff a fault is armed for `point` and its countdown has reached zero;
/// otherwise `Ok(())`. The common (unarmed) case is a single relaxed load.
///
/// # Errors
/// [`crate::apply::executor::ApplyError::Backend`] tagged `fault-injection: <point>` when
/// the armed fault fires.
pub fn trip(point: &str) -> Result<(), crate::apply::executor::ApplyError> {
    if ARMED_THREADS.load(Ordering::Relaxed) == 0 {
        return Ok(());
    }
    let fire = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let fire = match registry.0.get_mut(point) {
            Some(0) => true,
            Some(n) => {
                *n -= 1;
                false
            }
            None => false,
        };
        if fire {
            registry.0.remove(point);
            if registry.0.is_empty() {
                ARMED_THREADS.fetch_sub(1, Ordering::SeqCst);
            }
        }
        fire
    });
    if fire {
        return Err(crate::apply::executor::ApplyError::Backend(format!(
            "fault-injection: simulated crash at boundary '{point}'"
        )));
    }
    Ok(())
}

/// The named sub-step boundaries the executor trips at. Centralised so the test
/// and the executor agree on the exact set (the journal-state-machine model).
pub mod points {
    /// In `apply_dml_transactional`: AFTER the DML statement ran, BEFORE the
    /// journal INSERT (a crash here must leave NO journal row — the txn rolls
    /// back the data write too).
    pub const DML_AFTER_STMT_BEFORE_JOURNAL: &str = "dml.after_stmt.before_journal";
    /// In `apply_dml_transactional`: AFTER the journal INSERT, BEFORE COMMIT (a
    /// crash here must ALSO leave no row — the INSERT is inside the uncommitted
    /// txn).
    pub const DML_AFTER_JOURNAL_BEFORE_COMMIT: &str = "dml.after_journal.before_commit";
    /// In the backfill loop: AFTER a batch's UPDATE COMMITted, before the next
    /// batch (a crash here leaves the cursor partway; resume continues the
    /// remaining rows — the WHERE-filter idempotency).
    pub const BACKFILL_MID_BATCHES: &str = "backfill.mid_batches";
    /// In the online expand: BETWEEN the E1+E2 apply and the E3 backfill (a crash
    /// here leaves E1/E2 journaled but E3 not — resume re-runs the backfill).
    pub const EXPAND_BETWEEN_E2_AND_BACKFILL: &str = "expand.between_e2_and_backfill";
    /// In the apply of ONE migration: AFTER its `up` ran, BEFORE the `completed`
    /// journal row lands.
    ///
    /// BOTH apply shapes trip it, so a crash test can compare them at the same
    /// boundary. The transactional apply is still inside its `BEGIN`, so the abort
    /// rolls the `up` back along with the journal row it never wrote. The two-phase
    /// apply has already committed the `up` (that is what `transaction:false`
    /// means) and leaves the armed inflight marker behind - the state the next
    /// apply's recovery has to converge from. Before this point existed the second
    /// shape could only be reached by killing the process or writing the marker
    /// database-side, so no in-repo test could measure what recovery does with it.
    pub const APPLY_AFTER_UP_BEFORE_COMPLETED: &str = "apply.after_up.before_completed";
    // Two deploy-scoped points were declared here and never wired: nothing tripped
    // them and no test ever armed them, while their docs claimed a crash-fuzz test
    // did. They described a deploy-recovery loop this crate does not drive - see
    // the marker-log design in `crate::apply::journal`, which now says so and keeps
    // the state-machine rationale a driver would need.
}
