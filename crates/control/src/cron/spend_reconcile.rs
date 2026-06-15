//! Spend-reconcile cron (billing PR5, ISS-31).
//!
//! Every ~60s it runs the [`SpendEngine::evaluate_all`] sweep: price each
//! app's current-period usage, derive the new [`SpendState`] with hysteresis,
//! and persist transitions to `zeroship.app_spend_state` (+ history). The
//! gateway picks up the new state on its next `/internal/routes` pull (the
//! registry JOINs `app_spend_state`) — decision D1: enforcement rides the
//! PULLed `RouteEntry.spend_state`, NOT a pushed event.
//!
//! For each transition this cron ALSO writes an audit row and constructs a
//! `ControlEvent::SpendState`. Per D1 there is no live `ControlEvent` delivery
//! path today; the event is built for the audit log / future SSE fan-out only.
//! It is logged (and dropped) here so the wire variant has a producer and the
//! transition is observable.

use std::sync::Arc;
use std::time::Duration;

use zeroship_core::types::ControlEvent;

use crate::audit::{self, Action, AuditEntry};
use crate::registry::RegistryError;
use crate::spend::{spend_state_str, SpendEngine, SpendTransition};
use crate::AppState;

/// Default tick cadence in seconds (~1 min). Spend is a soft, minute-scale
/// money bound — a 60s tick bounds new over-limit work without thrashing PG.
pub const DEFAULT_TICK_SECS: u64 = 60;

/// Stable `pg_advisory_lock` key for the spend-reconcile sweep (#2).
///
/// Multiple control instances run this cron concurrently. Without a lock, two
/// instances racing the same sweep would BOTH derive a transition and BOTH
/// append a `spend_state_history` row (duplicate audit rows, possibly
/// mis-recorded flaps). A session-scoped `pg_try_advisory_lock(<key>)` makes
/// the sweep single-flight fleet-wide: the instance that wins runs it; the
/// others skip this tick and retry next cadence. The key is an arbitrary but
/// FIXED 64-bit constant unique to this sweep (derived from "zsspend1" — must
/// never collide with another advisory-lock user).
const SPEND_SWEEP_ADVISORY_LOCK_KEY: i64 = 0x7a73_7370_6e64_0001;

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then
/// sleeps `tick_secs`. A transient PG error is logged and swallowed so the
/// cron task survives (mirrors `audit_retention` / `orphaned_app_reaper`).
//
// `AppState`/`Registry` hold `!Send` handles; the lint is structural.
#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control spend_reconcile cron starting");
    loop {
        match tick(&state).await {
            Ok(n) if n > 0 => {
                tracing::info!(transitions = n, "control spend_reconcile sweep completed");
            }
            Ok(_) => { /* steady state; stay quiet */ }
            Err(e) => {
                tracing::error!(error = %e, "control spend_reconcile tick failed");
            }
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

/// Run one reconcile sweep. Exposed so an integration test can drive a single
/// tick deterministically without sitting on the cron sleep. Returns the
/// number of apps that transitioned.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<usize, RegistryError> {
    // #2 — multi-instance safety. Hold a session-scoped advisory lock on a
    // dedicated connection for the whole sweep so only ONE control instance
    // runs `evaluate_all` per tick. A loser skips this tick (returns 0) and
    // retries next cadence; without this, racing instances both append
    // duplicate `spend_state_history` rows. The lock connection is held until
    // the explicit unlock below (and released anyway when the conn drops, since
    // advisory locks are session-scoped).
    let lock_conn = state.registry.conn().await?;
    let got = lock_conn
        .query(
            "SELECT pg_try_advisory_lock($1) AS locked",
            &[&SPEND_SWEEP_ADVISORY_LOCK_KEY],
        )
        .await?;
    let acquired = got.first().is_some_and(|r| r.get::<_, bool>("locked"));
    if !acquired {
        tracing::debug!("spend_reconcile: advisory lock held by another instance — skipping tick");
        return Ok(0);
    }

    let engine = SpendEngine::new(state.registry.clone());
    let result = engine.evaluate_all().await;

    // Release the advisory lock regardless of sweep outcome (best-effort; the
    // session-scoped lock also frees when `lock_conn` drops).
    if let Err(e) = lock_conn
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&SPEND_SWEEP_ADVISORY_LOCK_KEY],
        )
        .await
    {
        tracing::warn!(error = %e, "spend_reconcile: advisory unlock failed (lock frees on conn drop)");
    }

    let transitions = result?;
    for t in &transitions {
        emit_transition(state, t).await;
    }
    Ok(transitions.len())
}

/// Audit + construct the (currently un-delivered) `ControlEvent::SpendState`
/// for one transition.
#[allow(clippy::future_not_send)]
async fn emit_transition(state: &AppState, t: &SpendTransition) {
    // #8 — enrich the audit detail with the money context, matching the
    // `{from,to,spend_cents,limit_cents}` shape documented on
    // `audit::Action::SpendStateChange`.
    let detail = serde_json::json!({
        "from": spend_state_str(t.old),
        "to": spend_state_str(t.new),
        "spend_cents": t.spend_cents,
        "limit_cents": t.limit_cents,
    });
    audit::log_with_detail(
        &state.registry,
        AuditEntry {
            app_id: Some(t.app_id),
            creator_id: None,
            actor_user_id: None,
            actor_token_id: None,
            action: Action::SpendStateChange,
            resource: Some("spend_state"),
            source_ip: None,
        },
        &detail,
    )
    .await;

    // D1: built for the audit log / future SSE only — there is no live
    // delivery path. Construct it so the wire variant has a real producer and
    // the transition is observable in logs.
    let event = ControlEvent::SpendState {
        app_id: t.app_id,
        state: t.new,
    };
    match serde_json::to_string(&event) {
        Ok(json) => tracing::info!(target: "control.spend", event = %json, "spend state transition"),
        Err(e) => tracing::warn!(error = %e, "spend: failed to encode ControlEvent::SpendState"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tick_is_one_minute() {
        assert_eq!(DEFAULT_TICK_SECS, 60);
    }
}
