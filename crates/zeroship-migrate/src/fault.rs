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
//! **This is inert in production.** When no fault is armed (the only state outside
//! the crash-fuzz test), [`trip`] is a single relaxed atomic load that returns
//! `Ok(())`. Faults are armed only by the in-process crash-fuzz test via [`arm`]
//! and cleared via [`disarm_all`]. Nothing here reads the environment or persists.
//!
//! Hidden from the public docs — it is a test-support surface, not a stable API.

#![doc(hidden)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Fast-path gate: `false` whenever no fault is armed, so [`trip`] short-circuits
/// on a single relaxed load on every production boundary.
static ARMED_THREADS: AtomicUsize = AtomicUsize::new(0);

// The armed faults: `point name -> remaining hits before it fires`. A fault armed
// with `skip = k` fires on the `(k+1)`-th trip of that point (so `skip = 0`
// fires on the first trip). It fires AT MOST ONCE, then disarms itself.
thread_local! {
    static REGISTRY: RefCell<HashMap<String, u32>> = RefCell::new(HashMap::new());
}

/// Arm a one-shot crash at `point`, firing after `skip` prior trips of it (so
/// `skip = 0` ⇒ crash on the first trip of `point`). Test-only.
pub fn arm(point: &str, skip: u32) {
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        if registry.is_empty() {
            ARMED_THREADS.fetch_add(1, Ordering::SeqCst);
        }
        registry.insert(point.to_string(), skip);
    });
}

/// Clear every armed fault (call between crash-fuzz iterations). Test-only.
pub fn disarm_all() {
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        if !registry.is_empty() {
            registry.clear();
            ARMED_THREADS.fetch_sub(1, Ordering::SeqCst);
        }
    });
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
        let fire = match registry.get_mut(point) {
            Some(0) => true,
            Some(n) => {
                *n -= 1;
                false
            }
            None => false,
        };
        if fire {
            registry.remove(point);
            if registry.is_empty() {
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
    /// In the control IR deploy loop (PR9d): a same-deploy later-file failure has
    /// been detected and the deploy's recovery markers are durably written, but the
    /// process dies BEFORE the in-process abort runs. A crash here leaves the
    /// just-opened obligation OUTSTANDING + its recovery marker net-`in_progress` — exactly the
    /// state the NEXT same-app deploy's crash-recovery leg must converge from (abort
    /// the half-renamed table, mark the marker reconciled). Tripped by the
    /// deploy-recovery crash-fuzz test only.
    pub const DEPLOY_BEFORE_INPROCESS_ABORT: &str = "deploy.before_inprocess_abort";
    /// In the control IR deploy loop SUCCESS arm (PR9e): the `committed` recovery-marker
    /// promotion FAILS (DB unreachable the instant the go-live reaches its success arm).
    /// Because the marker was BORN `in_progress` atomically with the obligation
    /// (engine-stamped — PR9e), a promotion failure leaves it net-`in_progress`: the
    /// *recoverable* (fail-safe) state. The deploy surfaces a HARD error, but the NEXT
    /// same-app deploy's crash-recovery leg AUTO-ABORTS the half-rename (safe — a
    /// pending contract has not cut over to the shadow column, so no data is lost), then
    /// the app re-runs the rename cleanly. This is the INVERSE of the pre-PR9e phase-1
    /// stamp-failure residual: there a failure left the marker `open` (protected) and a
    /// later deploy silently reverted a committed contract; here a failure leaves it
    /// recoverable and the later deploy safely auto-aborts. Tripped by the
    /// no-false-abort / fail-safe-auto-recovery characterization test only.
    pub const DEPLOY_SUCCESS_COMMITTED_STAMP_FAILS: &str =
        "deploy.success_committed_stamp_fails";
}
