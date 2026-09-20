use zeroship_data_orm::error::DbError;

use crate::backend::BackendHandle;
use crate::binding::DbRoute;

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

/// Admit a top-level transaction on the supplied backend and drive it to `Idle`.
///
/// # Errors
///
/// Returns the error reported by transaction admission or session setup.
pub async fn begin(
    binding: &crate::binding::DbBinding,
    isolation_level: Option<zeroship_data_orm::error::IsolationLevel>,
    backend: BackendHandle,
) -> Result<(), DbError> {
    let driven = driver::begin_top_level(binding, isolation_level, backend).await?;
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
pub fn admit_only(route: &DbRoute) {
    assert!(
        crate::tx_lanes::with_mut(|l| l.try_claim_tx(route)),
        "admit_only: a transaction is already claimed for {} on {}",
        route.app_id(),
        route.database_text()
    );
    let actions = driver::admit_in_preparing(route);
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
pub async fn fire_execution_deadline(route: &DbRoute) -> ProbeOutcome {
    use super::reducer::deadline::{DeadlineKind, DeadlineState};
    let armed = crate::tx_lanes::with(|l| {
        l.transaction_reducer(route)
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
    let driven = driver::deadline_fired(route, kind, generation).await;
    ProbeOutcome {
        outcome: driven.outcome(),
        session: session_after(route, None),
        refused: driven.refusal().map(|refusal| refusal.code()),
    }
}

/// Run one creator data statement under the reducer's operation guard.
///
/// # Errors
///
/// The statement's own error, verbatim. A statement that errors leaves the
/// transaction in [`TxState::Poisoned`].
pub async fn operation(route: &DbRoute, sql: &str) -> Result<(), DbError> {
    driver::run_operation(route, sql, &[]).await
}

/// Open a nested frame.
///
/// # Errors
///
/// The frame guard's refusal, or the `SAVEPOINT`'s own error.
pub async fn open_frame(route: &DbRoute) -> Result<FrameId, DbError> {
    let driven = driver::open_frame(route).await;
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
pub async fn close_frame(route: &DbRoute, frame: FrameId, released: bool) -> ProbeOutcome {
    let close = if released {
        FrameClose::Released
    } else {
        FrameClose::RolledBackTo
    };
    let session = session(route);
    let driven = driver::close_frame(route, frame, close).await;
    ProbeOutcome {
        outcome: driven.outcome(),
        session: session_after(route, session),
        refused: driven.refusal().map(|refusal| refusal.code()),
    }
}

/// Settle the root.
pub async fn settle(route: &DbRoute, commit: bool) -> ProbeOutcome {
    let intent = if commit {
        SettleIntent::Commit
    } else {
        SettleIntent::Rollback
    };
    let driven = driver::settle_root(route, intent).await;
    ProbeOutcome {
        outcome: driven.outcome(),
        session: session_after(route, None),
        refused: driven.refusal().map(|refusal| refusal.code()),
    }
}

/// Force this transaction to end under `CleanupCause::Cancelled`.
pub async fn cancel(route: &DbRoute) -> ProbeOutcome {
    let driven = driver::cancel(route).await;
    ProbeOutcome {
        outcome: driven.outcome(),
        session: session_after(route, None),
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
fn session_after(route: &DbRoute, before: Option<SessionOwnership>) -> Option<SessionOwnership> {
    if let Some(live) = session(route) {
        return Some(live);
    }
    if withdrawn(route) {
        return Some(SessionOwnership::Withdrawn);
    }
    before.or(Some(SessionOwnership::None))
}

/// The reducer's state, or `None` once it has been retired.
#[must_use]
pub fn state(route: &DbRoute) -> Option<TxState> {
    crate::tx_lanes::with(|l| {
        l.transaction_reducer(route)
            .map(super::reducer::TxReducer::state)
    })
}

/// The reducer's session ownership, or `None` once it has been retired.
#[must_use]
pub fn session(route: &DbRoute) -> Option<SessionOwnership> {
    crate::tx_lanes::with(|l| {
        l.transaction_reducer(route)
            .map(super::reducer::TxReducer::session)
    })
}

/// Every savepoint name this transaction has minted, in order.
#[must_use]
pub fn minted_savepoint_names(route: &DbRoute) -> Vec<String> {
    crate::tx_lanes::with(|l| {
        l.transaction_reducer(route).map_or_else(Vec::new, |r| {
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
pub fn withdrawn(route: &DbRoute) -> bool {
    crate::tx_lanes::with(|l| l.tx_session_withdrawn(route))
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
pub fn session_backend_pid(route: &DbRoute) -> Option<i32> {
    crate::tx_lanes::with_mut(|l| {
        let client = l.take_tx_client_for(route)?;
        let pid = client.server_process_id();
        l.put_tx_client_for(route, client);
        pid
    })
}

/// `(idle, active, total)` for the pool behind the backend the caller holds.
///
/// The backend handle is a parameter, matching [`begin`], because every caller
/// already has it rather than reading it back out of `crate::context`.
#[must_use]
pub fn pool_counts(backend: &BackendHandle) -> Option<(usize, usize, usize)> {
    backend.pool_counts()
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
    guard: Option<crate::tx_lanes::TxClientSlotGuard>,
}

impl std::fmt::Debug for HeldSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeldSession")
            .field("held", &self.guard.is_some())
            .finish()
    }
}

impl HeldSession {
    /// Take the session out of `route`'s slot.
    ///
    /// # Errors
    ///
    /// When no session is parked for that app.
    pub fn take(route: &DbRoute) -> Result<Self, DbError> {
        Ok(Self {
            guard: Some(crate::tx_lanes::TxClientSlotGuard::take(route)?),
        })
    }

    /// The backend PID of the held session.
    #[must_use]
    pub fn backend_pid(&self) -> Option<i32> {
        self.guard.as_ref()?.client().server_process_id()
    }

    /// Give the session back, as the holder's `Drop` would.
    pub fn restore(mut self) {
        drop(self.guard.take());
    }
}

/// Retire `route`'s transaction and hand its parked session to a **successor
/// lane**, as the next caller's admission would.
///
/// This reproduces a *timing*, in the same spirit as [`HeldSession`]: the retire
/// and release below are the two `Action::ReleaseAdmission` makes, so nothing
/// here is a second implementation of anything. What it reproduces is a forced
/// cleanup being retired out from under itself - which production reaches when
/// the `CancellationSql` deadline fires in its own task while cleanup is still
/// waiting for a cancelled statement to hand the session back.
///
/// **The successor is the point.** A stale cleanup must find a filled slot it
/// can no longer prove is its own; in production that session belongs to the
/// NEXT transaction, because a claim is never released while its own session is
/// parked - `settle_now` emits `WithdrawSession` or `ReleaseSession` immediately
/// before every `ReleaseAdmission`, and there is no second emission site.
/// Re-homing the session onto a successor lane models that state; what the arm
/// rules on is a filled slot plus a dead identity.
pub fn abandon_reducer(route: &DbRoute) {
    crate::tx_lanes::with_mut(|l| {
        let session = l.take_tx_client_for(route);
        l.retire_transaction(route);
        l.release_tx_claim(route);
        if let Some(session) = session {
            assert!(
                l.try_claim_tx(route),
                "the successor must win the claim the release just freed"
            );
            l.install_tx_client(route, session);
        }
    });
}

/// How long forced cleanup waits for a cancelled statement to release the
/// session before it gives up and withdraws.
///
/// Exposed so an arm can bind itself to WHICH path through cleanup it took.
/// A cancellation the server acted on frees the session in about a round trip;
/// one it discarded frees nothing and the cleanup sits out this whole grace. The
/// two answers are otherwise identical at the reducer, so an arm that means to
/// exercise the second has to measure the clock or it is not ruling on the path
/// at all.
#[must_use]
pub const fn cancel_reclaim_grace() -> std::time::Duration {
    driver::CANCEL_RECLAIM_GRACE
}

/// Clear every trace of `route`'s transaction, for a test tearing down.
pub fn reset(route: &DbRoute) {
    let client = crate::tx_lanes::with_mut(|l| {
        l.retire_transaction(route);
        l.release_tx_claim(route);
        l.clear_pending_emits_for(route);
        l.take_tx_client_for(route)
    });
    if let Some(client) = client {
        crate::tx_lanes::destroy_tx_connection(client);
    }
}
