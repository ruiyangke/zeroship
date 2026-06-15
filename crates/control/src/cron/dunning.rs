//! Dunning-timeout cron (billing G2).
//!
//! Every ~hour it runs the dunning sweep: each creator in `past_due` whose
//! dunning window has elapsed (`NOW() - past_due_since > max_dunning_days`) is
//! transitioned to `suspended`. The gateway picks up the new state on its next
//! `/internal/routes` pull (the registry JOINs `creator_billing_status` via the
//! owner membership) and 402s the creator's apps. We do NOT touch Stripe —
//! Stripe's own retry/dunning schedule keeps running; this is purely the
//! platform's stop-eating-infra-cost deadline.
//!
//! ## Why a cron (not lazy evaluation at route-pull)
//!
//! Suspension is a TIME-based transition. Evaluating it lazily in
//! `registry.get_routes` would push wall-clock logic into the gateway's pull
//! SELECT (racy across control instances, no single audit edge, and the gateway
//! would compute state rather than read pre-derived state). The codebase already
//! establishes the opposite discipline for spend: the spend cron DERIVES state
//! and persists it; the gateway only READS the pulled value (decision D1). The
//! dunning cron mirrors that exactly — same advisory-lock single-flight pattern,
//! same "derive-then-persist, gateway reads" shape — so account state and spend
//! state are surfaced identically. That is the simpler CORRECT option.
//!
//! Each transition writes a `creator_billing_status_history` row (in the store)
//! + an `AccountStateChange` audit row (here).

use std::sync::Arc;
use std::time::Duration;

use crate::account_status::{account_state_str, AccountStatusStore, DEFAULT_MAX_DUNNING_DAYS};
use crate::audit::{self, Action, AuditEntry};
use crate::registry::RegistryError;
use crate::AppState;

/// Default tick cadence in seconds (~1 hour). Dunning is a day-scale deadline
/// (`max_dunning_days`), so an hourly tick is ample — suspension fires within an
/// hour of the window elapsing without thrashing PG.
pub const DEFAULT_TICK_SECS: u64 = 3600;

/// Stable `pg_advisory_lock` key for the dunning sweep. Distinct from the
/// spend-sweep key (`0x7a73_7370_6e64_0001`) so the two sweeps never block each
/// other. Derived from "zsdunng1" — a fixed 64-bit constant unique to this
/// sweep; must never collide with another advisory-lock user.
const DUNNING_SWEEP_ADVISORY_LOCK_KEY: i64 = 0x7a73_6475_6e6e_0001;

/// Cron entry point. Loops forever; each iteration runs one [`tick`] then sleeps
/// `tick_secs`. A transient PG error is logged and swallowed so the cron task
/// survives (mirrors `spend_reconcile`).
//
// `AppState`/`Registry` hold `!Send` handles; the lint is structural.
#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control dunning cron starting");
    loop {
        match tick(&state, DEFAULT_MAX_DUNNING_DAYS).await {
            Ok(n) if n > 0 => {
                tracing::info!(suspensions = n, "control dunning sweep suspended past_due creators");
            }
            Ok(_) => { /* steady state; stay quiet */ }
            Err(e) => {
                tracing::error!(error = %e, "control dunning tick failed");
            }
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

/// Run one dunning sweep. Exposed so an integration test can drive a single tick
/// deterministically without sitting on the cron sleep. Returns the number of
/// creators suspended this tick.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState, max_dunning_days: i64) -> Result<usize, RegistryError> {
    // Multi-instance safety: hold a session-scoped advisory lock on a dedicated
    // connection for the whole sweep so only ONE control instance suspends per
    // tick (a loser skips and retries next cadence). Without it, racing
    // instances both append duplicate history rows. Mirrors `spend_reconcile`.
    let lock_conn = state.registry.conn().await?;
    let got = lock_conn
        .query(
            "SELECT pg_try_advisory_lock($1) AS locked",
            &[&DUNNING_SWEEP_ADVISORY_LOCK_KEY],
        )
        .await?;
    let acquired = got.first().is_some_and(|r| r.get::<_, bool>("locked"));
    if !acquired {
        tracing::debug!("dunning: advisory lock held by another instance — skipping tick");
        return Ok(0);
    }

    let store = AccountStatusStore::new(state.registry.clone());
    let result = store.suspend_exhausted(max_dunning_days).await;

    // Release the advisory lock regardless of sweep outcome (best-effort; the
    // session-scoped lock also frees when `lock_conn` drops).
    if let Err(e) = lock_conn
        .execute(
            "SELECT pg_advisory_unlock($1)",
            &[&DUNNING_SWEEP_ADVISORY_LOCK_KEY],
        )
        .await
    {
        tracing::warn!(error = %e, "dunning: advisory unlock failed (lock frees on conn drop)");
    }

    let transitions = result.map_err(|e| RegistryError::Database(e.to_string()))?;
    for t in &transitions {
        audit::log_with_detail(
            &state.registry,
            AuditEntry {
                app_id: None,
                creator_id: Some(t.creator_id),
                actor_user_id: None,
                actor_token_id: None,
                action: Action::AccountStateChange,
                resource: Some("dunning"),
                source_ip: None,
            },
            &serde_json::json!({
                "from": account_state_str(t.from),
                "to": account_state_str(t.to),
                "reason": t.reason,
                "creator_id": t.creator_id.to_string(),
            }),
        )
        .await;
    }
    Ok(transitions.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tick_is_one_hour() {
        assert_eq!(DEFAULT_TICK_SECS, 3600);
    }

    #[test]
    fn dunning_lock_key_differs_from_spend() {
        // The two sweeps must never share an advisory-lock key or one would
        // starve the other.
        assert_ne!(DUNNING_SWEEP_ADVISORY_LOCK_KEY, 0x7a73_7370_6e64_0001_i64);
    }
}
