//! The `test-helpers` seam onto the SC-1 driver.
//!
//! The driver's entry points are `pub(crate)` and its state lives in a
//! thread-local an integration target cannot reach. This module is the only way
//! in, and it is gated on `test-helpers` so a production build does not carry
//! it.
//!
//! **It performs no logic of its own.** Every function forwards to the driver or
//! reads the reducer; anything that made a judgement here would be a second
//! implementation for a test to agree with, which is how a test starts checking
//! itself. The one exception is [`HeldSession`], which exists to reproduce a
//! *timing* the production paths reach by scheduling rather than by request.

use zeroship_data_core::error::DbError;

use super::driver;
use super::reducer::frames::{FrameClose, FrameId};
use super::reducer::{SessionOwnership, SettleIntent, TerminalOutcome, TxState};

/// What a driven step ended on, in the shape a test asserts against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// The recorded terminal outcome, when the step reached `Settled`.
    pub outcome: Option<TerminalOutcome>,
    /// The session disposition SC-1 applied, read off the reducer **before** it
    /// was retired.
    pub session: Option<SessionOwnership>,
    /// The refusal code, when the step was refused.
    pub refused: Option<&'static str>,
}

/// Admit a top-level transaction and drive it to `Idle`.
///
/// # Errors
///
/// The backend error behind a `BEGIN` that did not open.
pub async fn begin(app_id: &str, isolation_level: Option<&str>) -> Result<(), DbError> {
    let driven = driver::begin_top_level(app_id, isolation_level).await?;
    if let Some(outcome) = driven.outcome() {
        return Err(driven.error.unwrap_or_else(|| {
            DbError::internal(format!("db: BEGIN settled immediately as {outcome:?}"))
        }));
    }
    Ok(())
}

/// Admit a transaction and **stop in `Preparing`**, before the authority
/// observation that issues `BEGIN`.
///
/// The only entry point that can reach the `NoTransaction` cleanup goal, which
/// is fixed from `Preparing` and nowhere else. It takes the admission claim the
/// same way [`begin`] does, so a test can also check the claim is released.
///
/// # Panics
///
/// If a transaction is already admitted for this app.
pub fn admit_only(app_id: &str) {
    assert!(
        crate::context::with_mut(|c| c.try_claim_tx(app_id)),
        "admit_only: a transaction is already claimed for {app_id}"
    );
    let actions = driver::admit_in_preparing(app_id);
    assert!(
        actions
            .iter()
            .all(|action| matches!(action, super::reducer::Action::ScheduleTimer(_))),
        "admission emits only ScheduleTimer; got {actions:?}"
    );
}

/// Deliver the execution deadline this transaction is armed with.
///
/// The generation comes from the reducer's own slot, so the delivery is the one
/// the timer task would make rather than a guessed pair - a `(kind, generation)`
/// the slot is not holding is a pure diagnostic and would prove nothing.
///
/// # Panics
///
/// If no execution deadline is armed.
pub async fn fire_execution_deadline(app_id: &str) -> ProbeOutcome {
    use super::reducer::deadline::{DeadlineKind, DeadlineState};
    let armed = crate::context::with(|c| {
        c.transaction_reducer(app_id)
            .map(|reducer| reducer.deadline().state())
    });
    let (kind, generation) = match armed {
        Some(DeadlineState::Armed {
            kind, generation, ..
        }) => (kind, generation),
        other => panic!("fire_execution_deadline: no armed deadline, slot is {other:?}"),
    };
    assert_eq!(
        kind,
        DeadlineKind::Execution,
        "the first deadline of a transaction's life is always Execution"
    );
    let driven = driver::deadline_fired(app_id, kind, generation).await;
    ProbeOutcome {
        outcome: driven.outcome(),
        session: session_after(app_id, None),
        refused: driven.refusal().map(|refusal| refusal.code()),
    }
}

/// Run one creator data statement under the reducer's operation guard.
///
/// # Errors
///
/// The statement's own error, verbatim. A statement that errors leaves the
/// transaction in [`TxState::Poisoned`].
pub async fn operation(app_id: &str, sql: &str) -> Result<(), DbError> {
    driver::run_operation(app_id, sql, &[]).await
}

/// Open a nested frame.
///
/// # Errors
///
/// The frame guard's refusal, or the `SAVEPOINT`'s own error.
pub async fn open_frame(app_id: &str) -> Result<FrameId, DbError> {
    let driven = driver::open_frame(app_id).await;
    if let Some(frame) = driven.frame() {
        return Ok(frame);
    }
    let refusal = driven.refusal();
    Err(driven
        .error
        .or_else(|| refusal.map(|refusal| driver::protocol_error(refusal, None)))
        .unwrap_or_else(|| DbError::internal("db: the frame did not open")))
}

/// Close a nested frame.
pub async fn close_frame(app_id: &str, frame: FrameId, released: bool) -> ProbeOutcome {
    let close = if released {
        FrameClose::Released
    } else {
        FrameClose::RolledBackTo
    };
    let session = session(app_id);
    let driven = driver::close_frame(app_id, frame, close).await;
    ProbeOutcome {
        outcome: driven.outcome(),
        session: session_after(app_id, session),
        refused: driven.refusal().map(|refusal| refusal.code()),
    }
}

/// Settle the root.
pub async fn settle(app_id: &str, commit: bool) -> ProbeOutcome {
    let intent = if commit {
        SettleIntent::Commit
    } else {
        SettleIntent::Rollback
    };
    let driven = driver::settle_root(app_id, intent).await;
    ProbeOutcome {
        outcome: driven.outcome(),
        session: session_after(app_id, None),
        refused: driven.refusal().map(|refusal| refusal.code()),
    }
}

/// Force this transaction to end under `CleanupCause::Cancelled`.
pub async fn cancel(app_id: &str) -> ProbeOutcome {
    let driven = driver::cancel(app_id).await;
    ProbeOutcome {
        outcome: driven.outcome(),
        session: session_after(app_id, None),
        refused: driven.refusal().map(|refusal| refusal.code()),
    }
}

/// The session disposition, read off the reducer if it is still there and
/// inferred from the withdrawal tombstone once it has been retired.
///
/// The reducer is dropped by `Action::ReleaseAdmission`, which the reducer emits
/// on the same settlement that disposed of the session - so a caller asking
/// afterwards has to ask the tombstone instead. Both answers come from state the
/// driver wrote, never from a second judgement made here.
fn session_after(app_id: &str, before: Option<SessionOwnership>) -> Option<SessionOwnership> {
    if let Some(live) = session(app_id) {
        return Some(live);
    }
    if withdrawn(app_id) {
        return Some(SessionOwnership::Withdrawn);
    }
    before.or(Some(SessionOwnership::None))
}

/// The reducer's state, or `None` once it has been retired.
#[must_use]
pub fn state(app_id: &str) -> Option<TxState> {
    crate::context::with(|c| {
        c.transaction_reducer(app_id)
            .map(super::reducer::TxReducer::state)
    })
}

/// The reducer's session ownership, or `None` once it has been retired.
#[must_use]
pub fn session(app_id: &str) -> Option<SessionOwnership> {
    crate::context::with(|c| {
        c.transaction_reducer(app_id)
            .map(super::reducer::TxReducer::session)
    })
}

/// Every savepoint name this transaction has minted, in order.
#[must_use]
pub fn minted_savepoint_names(app_id: &str) -> Vec<String> {
    crate::context::with(|c| {
        c.transaction_reducer(app_id).map_or_else(Vec::new, |r| {
            r.frames()
                .minted_names()
                .iter()
                .map(|name| name.to_string())
                .collect()
        })
    })
}

/// Has this app's transaction session been withdrawn?
#[must_use]
pub fn withdrawn(app_id: &str) -> bool {
    crate::context::with(|c| c.tx_session_withdrawn(app_id))
}

/// The PostgreSQL backend PID of the session currently in this app's slot.
///
/// The strongest available identity for "is this the same physical connection":
/// a withdrawal must make the next checkout report a different one.
///
/// **Do not call this while a withdrawal tombstone is set for the app.** It puts
/// the session back through `put_tx_client_for`, which destroys anything
/// returning under a tombstone - so a read would itself become the withdrawal.
/// The tombstone is cleared by the next `admit_transaction`, which is why
/// [`begin`] comes before every use of this in the arms that withdraw.
#[must_use]
pub fn session_backend_pid(app_id: &str) -> Option<i32> {
    crate::context::with_mut(|c| {
        let client = c.take_tx_client_for(app_id)?;
        let pid = match &client {
            crate::context::TxConnection::Postgres(pg) => Some(pg.process_id()),
            crate::context::TxConnection::Sqlite(_) => None,
        };
        c.put_tx_client_for(app_id, client);
        pid
    })
}

/// `(idle, active, total)` for this thread's data pool.
#[must_use]
pub fn pool_counts() -> Option<(usize, usize, usize)> {
    crate::context::with(|c| {
        c.pool()
            .map(|pool| (pool.idle_count(), pool.active_count(), pool.total_count()))
    })
}

/// Hold this app's transaction session out of the slot, exactly as an in-flight
/// operation does.
///
/// This reproduces a **timing**, not a policy: a forced cleanup can land while
/// some other future owns the session, and the driver cannot roll back a session
/// it cannot reach. The guard restores the session on drop, which is the moment
/// the withdrawal tombstone has to bite.
#[must_use = "the session is restored when this guard drops"]
pub struct HeldSession {
    guard: Option<crate::context::TxClientSlotGuard>,
}

impl std::fmt::Debug for HeldSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldSession")
            .field("held", &self.guard.is_some())
            .finish()
    }
}

impl HeldSession {
    /// Take the session out of `app_id`'s slot.
    ///
    /// # Errors
    ///
    /// When no session is parked for that app.
    pub fn take(app_id: &str) -> Result<Self, DbError> {
        Ok(Self {
            guard: Some(crate::context::TxClientSlotGuard::take(app_id)?),
        })
    }

    /// The backend PID of the held session.
    #[must_use]
    pub fn backend_pid(&self) -> Option<i32> {
        match self.guard.as_ref()?.client() {
            crate::context::TxConnection::Postgres(pg) => Some(pg.process_id()),
            crate::context::TxConnection::Sqlite(_) => None,
        }
    }

    /// Give the session back, as the holder's `Drop` would.
    pub fn restore(mut self) {
        drop(self.guard.take());
    }
}

/// Retire `app_id`'s transaction and hand its parked session to a **successor
/// lane**, as the next caller's admission would.
///
/// This reproduces a *timing*, in the same spirit as [`HeldSession`]: the retire
/// and release below are the two `Action::ReleaseAdmission` makes, so nothing
/// here is a second implementation of anything. What it reproduces is a forced
/// cleanup being retired out from under itself - which production reaches when
/// the `CancellationSql` deadline fires in its own task while cleanup is still
/// waiting for a cancelled statement to hand the session back.
///
/// **The successor is the point, and it used to be faked.** A stale cleanup must
/// find a filled slot it can no longer prove is its own; in production that
/// session belongs to the NEXT transaction, because a claim is never released
/// while its own session is parked - `settle_now` emits `WithdrawSession` or
/// `ReleaseSession` immediately before every `ReleaseAdmission`, and there is no
/// second emission site. This used to stand the retired transaction's own
/// session in the slot, which is a state production cannot reach; re-homing it
/// onto a successor lane models what actually happens and costs the arm nothing,
/// since what it rules on is a filled slot plus a dead identity.
pub fn abandon_reducer(app_id: &str) {
    crate::context::with_mut(|c| {
        let session = c.take_tx_client_for(app_id);
        c.retire_transaction(app_id);
        c.release_tx_claim(app_id);
        if let Some(session) = session {
            assert!(
                c.try_claim_tx(app_id),
                "the successor must win the claim the release just freed"
            );
            c.install_tx_client(app_id, session);
        }
    });
}

/// How long forced cleanup waits for a cancelled statement to release the
/// session before it gives up and withdraws.
///
/// Exposed so an arm can bind itself to WHICH route through cleanup it took.
/// A cancellation the server acted on frees the session in about a round trip;
/// one it discarded frees nothing and the cleanup sits out this whole grace. The
/// two answers are otherwise identical at the reducer, so an arm that means to
/// exercise the second has to measure the clock or it is not ruling on the route
/// at all.
#[must_use]
pub const fn cancel_reclaim_grace() -> std::time::Duration {
    driver::CANCEL_RECLAIM_GRACE
}

/// Clear every trace of `app_id`'s transaction, for a test tearing down.
pub fn reset(app_id: &str) {
    let client = crate::context::with_mut(|c| {
        c.retire_transaction(app_id);
        c.release_tx_claim(app_id);
        c.clear_pending_emits_for(app_id);
        c.take_tx_client_for(app_id)
    });
    if let Some(client) = client {
        crate::context::destroy_tx_connection(client);
    }
}
