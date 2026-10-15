//! Spend-reconcile cron.
//!
//! Every ~60s it runs the [`SpendEngine::evaluate_all`] sweep: price each
//! app's current-period usage, derive the new [`zeroship_core::types::SpendState`] with hysteresis,
//! and persist transitions to `zeroship.app_spend_state` (+ history). The
//! gateway picks up the new state on its next `/internal/routes` pull (the
//! registry JOINs `app_spend_state`); enforcement rides the PULLed
//! `RouteEntry.spend_state`, NOT a pushed event.
//!
//! For each transition this cron constructs a `ControlEvent::SpendState`; the
//! durable `SpendStateChange` audit row is written by the engine that persists the
//! transition (see [`SpendEngine::evaluate_all`]). There is no live `ControlEvent`
//! delivery path today; the event is built for the audit log / future SSE fan-out
//! only. It is logged (and dropped) here so the wire variant has a producer and the
//! transition is observable.

use zeroship_core::types::ControlEvent;

use crate::deadline::CycleDeadline;
use crate::registry::RegistryError;
use crate::spend::{SpendEngine, SpendTransition};
use crate::AppState;

/// Run one reconcile sweep under the compiled default enforcement interval.
///
/// Existing callers (the manual `force_spend_reconcile` endpoint, tests) keep
/// this entry point; the cron drives [`tick_within`] with the configured
/// interval's deadline. Returns the number of apps that transitioned.
///
/// # Errors
/// Returns the sweep's [`RegistryError`], including [`RegistryError::CycleDeadline`]
/// when the compiled default interval's deadline cut it.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<usize, RegistryError> {
    tick_within(
        state,
        crate::cron::spend_recompute::SpendRecomputeConfig::default().cycle_deadline(),
    )
    .await
}

/// Run one reconcile sweep under `deadline`, derived from the enforcement
/// cycle's configured interval.
///
/// Exposed so an integration test can drive a single tick deterministically
/// without sitting on the cron sleep. Returns the number of apps that
/// transitioned.
///
/// The advisory-lock acquire, the sweep and the unlock all run under
/// `deadline`'s client-side backstop, so a network that never answers ends the
/// tick at the deadline, and a server-side cancellation anywhere in it is
/// reported as [`RegistryError::CycleDeadline`]. The advisory-lock connection
/// and the sweep's own connection both carry the server-side
/// `statement_timeout` too, so a stalled database cannot hold the tick past its
/// interval; a cut cycle rolls its transaction back and the next tick retries.
///
/// # Errors
/// Returns the sweep's [`RegistryError`], including [`RegistryError::CycleDeadline`]
/// when `deadline` cut it.
pub async fn tick_within(state: &AppState, deadline: CycleDeadline) -> Result<usize, RegistryError> {
    crate::deadline::within(
        deadline,
        "spend_reconcile",
        Box::pin(tick_bounded(state, deadline)),
    )
    .await
}

/// The lock-held body. Separate so the whole cycle - acquire, sweep, unlock -
/// runs under one [`crate::deadline::within`] backstop without nesting the
/// sweep's future inside the backstop's.
async fn tick_bounded(state: &AppState, deadline: CycleDeadline) -> Result<usize, RegistryError> {
    // #2 - multi-instance safety. Hold a session-scoped advisory lock on a
    // dedicated connection for the whole sweep so only ONE control instance
    // runs `evaluate_all` per tick. A loser skips this tick (returns 0) and
    // retries next cadence; without this, racing instances both append
    // duplicate `spend_state_history` rows. The lock connection is held until
    // the explicit unlock below (and released anyway when the conn drops, since
    // advisory locks are session-scoped).
    let lock_conn = state.registry.conn().await?;
    crate::deadline::set_session_statement_timeout(&lock_conn, deadline).await?;
    let got = lock_conn
        .query(
            "SELECT pg_try_advisory_lock($1) AS locked",
            &[&super::lock_keys::SPEND_SWEEP],
        )
        .await?;
    let acquired = got.first().is_some_and(|r| r.get::<_, bool>("locked"));
    if !acquired {
        tracing::debug!(
            "spend_reconcile: advisory lock held by another instance - skipping tick"
        );
        return Ok(0);
    }

    let engine = SpendEngine::new(state.registry.clone());
    // The sweep body directly, NOT `evaluate_all_within`: this tick is already
    // inside `tick_within`'s boundary, which classifies and logs the cut once.
    // Re-wrapping here would log the same cut a second time under a second
    // operation tag.
    let result = Box::pin(engine.evaluate_all_server_bounded(deadline)).await;

    // Release the advisory lock regardless of sweep outcome (best-effort; the
    // session-scoped lock also frees when `lock_conn` drops).
    if let Err(e) = lock_conn
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&super::lock_keys::SPEND_SWEEP],
        )
        .await
    {
        tracing::warn!(error = %e, "spend_reconcile: advisory unlock failed (lock frees on conn drop)");
    }

    let transitions = result?;
    for t in &transitions {
        emit_transition(t);
    }
    Ok(transitions.len())
}

/// Construct and log the (currently un-delivered) `ControlEvent::SpendState`
/// for one transition.
///
/// The durable `SpendStateChange` audit row is written by the engine that
/// persists the transition (see `SpendEngine::evaluate_all`), so the cron and a
/// direct engine caller do not each audit one transition: the winner of the
/// compare-and-swap writes it at most once. The insert is best-effort on a
/// separate connection after the transition commits, so a crash between the two
/// loses the row rather than duplicating it.
fn emit_transition(t: &SpendTransition) {
    // D1: built for the audit log / future SSE only — there is no live
    // delivery path. Construct it so the wire variant has a real producer and
    // the transition is observable in logs.
    let event = ControlEvent::SpendState {
        app_id: t.app_id.clone(),
        state: t.new,
    };
    match serde_json::to_string(&event) {
        Ok(json) => tracing::info!(target: "control.spend", event = %json, "spend state transition"),
        Err(e) => tracing::warn!(error = %e, "spend: failed to encode ControlEvent::SpendState"),
    }
}
