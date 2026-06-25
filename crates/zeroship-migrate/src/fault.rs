//! Test-support **fault-injection seam** (crash simulation).
//!
//! The migration executor is two-phase + idempotent by design: a crash at any
//! point must leave the journal in a state a resume can converge from. To PROVE
//! that at *sub-step* granularity — mid-step, after a DDL/data statement but
//! before/after its journal row, between an online rename's E1/E2/E3 phases,
//! mid-backfill-batch — the executor consults this seam at named boundaries and
//! aborts the in-flight step (returning [`ApplyError`]) when a fault is armed for
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

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::sync::OnceLock;

/// Fast-path gate: `false` whenever no fault is armed, so [`trip`] short-circuits
/// on a single relaxed load on every production boundary.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The armed faults: `point name → remaining hits before it fires`. A fault armed
/// with `skip = k` fires on the `(k+1)`-th [`trip`] of that point (so `skip = 0`
/// fires on the first trip). It fires AT MOST ONCE, then disarms itself.
fn registry() -> &'static Mutex<HashMap<String, u32>> {
    static R: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

/// Arm a one-shot crash at `point`, firing after `skip` prior trips of it (so
/// `skip = 0` ⇒ crash on the first trip of `point`). Test-only.
pub fn arm(point: &str, skip: u32) {
    registry()
        .lock()
        .unwrap()
        .insert(point.to_string(), skip);
    ARMED.store(true, Ordering::SeqCst);
}

/// Clear every armed fault (call between crash-fuzz iterations). Test-only.
pub fn disarm_all() {
    registry().lock().unwrap().clear();
    ARMED.store(false, Ordering::SeqCst);
}

/// The executor's boundary check: returns an injected [`ApplyError`] (a simulated
/// crash) iff a fault is armed for `point` and its countdown has reached zero;
/// otherwise `Ok(())`. The common (unarmed) case is a single relaxed load.
///
/// # Errors
/// [`crate::executor::ApplyError::Backend`] tagged `fault-injection: <point>` when
/// the armed fault fires.
pub fn trip(point: &str) -> Result<(), crate::executor::ApplyError> {
    if !ARMED.load(Ordering::Relaxed) {
        return Ok(());
    }
    let mut reg = registry().lock().unwrap();
    let fire = match reg.get_mut(point) {
        Some(0) => true,
        Some(n) => {
            *n -= 1;
            false
        }
        None => false,
    };
    if fire {
        reg.remove(point);
        if reg.is_empty() {
            ARMED.store(false, Ordering::SeqCst);
        }
        drop(reg);
        return Err(crate::executor::ApplyError::Backend(format!(
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
    /// just-opened obligation OUTSTANDING + its recovery marker `open` — exactly the
    /// state the NEXT same-app deploy's crash-recovery leg must converge from (abort
    /// the half-renamed table, mark the marker reconciled). Tripped by the
    /// deploy-recovery crash-fuzz test only.
    pub const DEPLOY_BEFORE_INPROCESS_ABORT: &str = "deploy.before_inprocess_abort";
    /// In the control IR deploy loop SUCCESS arm (PR9d HIGH): AFTER the
    /// `reached_success` marker is stamped, the phase-2 `reconciled` append FAILS
    /// (DB hiccup). This reproduces the HIGH window — a legitimately-pending go-live
    /// whose `reconciled` append errored. The marker stays net-`reached_success`, so
    /// the NEXT deploy's crash-recovery leg must STILL exclude it (no false-abort of
    /// the live contract). Tripped by the legit-pending-survives-unrelated-deploy
    /// test only.
    pub const DEPLOY_SUCCESS_RECONCILE_FAILS: &str = "deploy.success_reconcile_fails";
    /// In the control IR deploy loop SUCCESS arm (PR9d-crit HIGH): the PHASE-1
    /// `reached_success` stamp itself FAILS (DB unreachable the instant the go-live
    /// reached its success arm). This reproduces the IRREDUCIBLE residual the
    /// `reached_success` discriminator cannot self-heal: the marker stays net-`open`
    /// over a LEGITIMATELY-pending live contract (trigger + shadow column committed,
    /// obligation pending). Unlike a phase-2 (`reconciled`) failure — which is
    /// non-fatal because the marker is already net-`reached_success` — a phase-1
    /// failure leaves a marker INDISTINGUISHABLE from a genuine crash half-state by
    /// any durable signal (the go-live's physical schema state is byte-identical to a
    /// crashed deploy's; see the two recovery tests). It is therefore NOT recoverable
    /// by a re-run (the idempotent re-run finds the EXPAND `already_outstanding`, so it
    /// never re-opens the obligation and never re-reaches the per-obligation stamp).
    /// The ONLY safe clearance is the operator's `resolve-pending --apply` (complete
    /// the rename, discharging the obligation so the next deploy's recovery leg finds
    /// nothing outstanding to abort). Tripped by the phase-1-stamp-failure
    /// characterization test only.
    pub const DEPLOY_SUCCESS_REACHED_SUCCESS_STAMP_FAILS: &str =
        "deploy.success_reached_success_stamp_fails";
}
