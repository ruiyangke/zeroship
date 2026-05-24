//! §17.7 drop-namespace sequencing — the PG teardown order for deleting
//! an app.
//!
//! Called by the control plane under a control-plane-held lock (§17.7).
//! plugin-db owns the *ordering* of the PG-side steps; the control plane
//! owns the cross-worker fan-out (signalling every worker to deprovision
//! its consumer) and the lock. This module is the single place the
//! ordering lives so it can't drift.
//!
//! ## Ordering (§17.7, with the per-app role from P6a-2 dropped last)
//!
//! 1. **Subscription gate.** If any subscription is active on the app,
//!    defer with `subscriptions_active` + a count. Under `--force`, fire
//!    `subscription_app_dropped` to every active subscriber (the broker
//!    `Closed` message — the SDK surfaces it as a terminal error on the
//!    subscription iterator for graceful shutdown), then proceed. No
//!    drop step runs while a subscription is observable.
//! 2. **Drain broker** via `subscription_app_dropped` (broker `drop_app`).
//! 3. **Consumer cancel + slot teardown.** Clear the in-process consumer
//!    mark (courtesy — §17.6: the slot survives consumer death, so this
//!    is best-effort), then `ChangeStream::deprovision`, which runs the
//!    §17.7 PG slot order: `pg_terminate_backend` against the slot's
//!    backend after the grace → `pg_drop_replication_slot` →
//!    `DROP PUBLICATION`. Retry from here on partial failure.
//! 4. `pg_drop_replication_slot(slot)` — folded into step 3's
//!    `deprovision`; requires inactive, which the terminate guarantees.
//! 5. `DROP PUBLICATION <pub>` — folded into step 3's `deprovision`.
//! 6. `DROP SCHEMA "<app_id>" CASCADE`.
//! 7. `DROP ROLE "app_<id>_role"` — the per-app role (P6a-2). Dropped
//!    AFTER the schema so no objects depend on it.
//!
//! ## CRITICAL #4 — DDL ordering while the writer is alive
//!
//! §17.7's CRITICAL #4 fix is "reorder so DDL runs while the writer is
//! alive." For PG that means the **publication** (which references the
//! schema via `FOR TABLES IN SCHEMA "<app_id>"`) must be dropped BEFORE
//! the schema — otherwise `DROP SCHEMA CASCADE` would tear out tables a
//! live publication still tracks, and Postgres would either error or
//! leave the publication dangling. The order here drops slot →
//! publication (step 3's `deprovision`) and only THEN `DROP SCHEMA`
//! (step 6), so the publication DDL always runs before the schema it
//! depends on disappears. The per-app role (step 7) is dropped last
//! because grants on the schema's objects vanish with the CASCADE; a
//! role still owning objects cannot be dropped.
//!
//! ## Idempotency + retry
//!
//! Steps 4–7 are individually idempotent (`pg_drop_replication_slot`
//! tolerates a concurrent dropper via undefined_object; `DROP
//! PUBLICATION IF EXISTS`, `DROP SCHEMA IF EXISTS … CASCADE`, `DROP ROLE
//! IF EXISTS`). A `LockContention` from the slot drop (backend still
//! detaching) is surfaced so the caller retries from step 3. Every other
//! step is a no-op when its precondition is already met.
//!
//! ## Dead-code posture (control-plane wiring pending)
//!
//! [`drop_namespace`] is the control-plane entry point for app deletion
//! — there is no V8 / creator-facing dispatch for it (deletion is a
//! control-plane operation under a control-plane-held lock, §17.7). Until
//! the control plane wires the call (cross-worker fan-out + lock), the
//! orchestrator surface is unused in a default build; it is fully
//! exercised by the `tests/integration.rs` PG suite (reachable via
//! `test-helpers`). Same posture as the `migration_sweeper` (P6a-1) and
//! `auth/*` — built + tested, zero production callers until the wire-up.
//! Remove the allow in the PR that wires the control-plane call.
#![allow(dead_code)]

use compio_postgres::Pool;

use crate::backend::BackendHandle;
use crate::error::{prefix_message, DbError};

/// Options for [`drop_namespace`].
#[derive(Debug, Clone)]
pub struct DropNamespaceOpts {
    /// Operator override. When `false`, an active subscription defers the
    /// drop. When `true`, active subscribers are sent
    /// `subscription_app_dropped` and the drop proceeds.
    pub force: bool,
    /// Count of active subscriptions on the app, as seen by the CALLER.
    /// The control plane aggregates this across workers via the
    /// `/internal/subscriptions/:app_id` admin endpoint; the
    /// single-worker / dev path can pass
    /// [`crate::broker::app_subscription_count`]. The orchestrator does
    /// NOT read the in-process broker itself for the gate, because the
    /// broker is per-isolate-thread and would undercount in a multi-worker
    /// cluster — the count must reflect the whole cluster.
    pub subscription_count: usize,
}

/// Outcome of a [`drop_namespace`] attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropNamespaceOutcome {
    /// The drop was deferred: a subscription is active and `--force` was
    /// not set. Carries the active-subscription count for the
    /// `subscriptions_active` conflict the SDK surfaces.
    Deferred { active_subscriptions: usize },
    /// Every teardown step ran (or was already a no-op). The app's slot,
    /// publication, schema, and per-app role are gone.
    Completed,
}

/// Run the §17.7 PG drop-namespace teardown for `app_id`.
///
/// `backend` is the platform-role-backed [`BackendHandle`] (§17.5 — the
/// slot teardown + DDL run under the platform role). `pool` is the same
/// backend's platform-role pool, used for the `DROP SCHEMA` / `DROP ROLE`
/// DDL.
///
/// Returns [`DropNamespaceOutcome::Deferred`] when the subscription gate
/// holds; otherwise runs every step and returns
/// [`DropNamespaceOutcome::Completed`]. Errors propagate the typed
/// `DbError`; a `LockContention` from the slot drop means "retry from
/// step 3" (the slot's backend is still detaching).
pub async fn drop_namespace(
    backend: &BackendHandle,
    pool: &Pool,
    app_id: &str,
    opts: &DropNamespaceOpts,
) -> Result<DropNamespaceOutcome, DbError> {
    // ---- Step 1: subscription gate ----
    if opts.subscription_count > 0 {
        if !opts.force {
            // Defer — the SDK surfaces `subscriptions_active` with the
            // count. No drop step runs while a subscription is observable.
            return Ok(DropNamespaceOutcome::Deferred {
                active_subscriptions: opts.subscription_count,
            });
        }
        // --force: fire `subscription_app_dropped` to every active
        // subscriber. The broker `drop_app` sends each subscription a
        // terminal `Closed` (the iterator surfaces it as the
        // `subscription_app_dropped` terminal code for graceful
        // shutdown). This is steps 1's force-arm AND step 2's drain in
        // one call — `drop_app` both notifies and removes.
        tracing::warn!(
            app_id = %app_id,
            active_subscriptions = opts.subscription_count,
            force = true,
            "drop_namespace: --force closing active subscriptions \
             (subscription_app_dropped) before teardown"
        );
    }

    // ---- Step 2: drain broker ----
    // Always drain (idempotent no-op when there are no subscribers on
    // this thread). Under --force this delivers the terminal close to
    // any subscriber the gate counted; with no subscriptions it is a
    // cheap miss.
    crate::broker::drop_app(Some(app_id));

    // ---- Step 3: consumer cancel (courtesy) + slot/publication teardown ----
    // Clear the in-process consumer mark so a racing reload doesn't
    // re-spawn a consumer mid-drop. §17.6: the slot survives consumer
    // death, so this is best-effort — the real force is the
    // `pg_terminate_backend` inside `deprovision`.
    crate::context::with_mut(|c| c.unmark_consumer_running(app_id));

    // §17.7 steps 3–5: terminate the slot's backend after the grace,
    // drop the slot, drop the publication. Routed through the
    // `ChangeStream::deprovision` adapter (PG arm →
    // `replication::drop_publication_and_slot`). On the SQLite arm this
    // is the file-unlink path (no slot/publication) — `deprovision`
    // there is the SQLite teardown. A `LockContention` here means "retry
    // from step 3" per §17.7.
    deprovision_change_stream(backend, app_id).await?;

    // ---- Step 6: DROP SCHEMA CASCADE ----
    // After the publication is gone (step 3) so the CASCADE never tears
    // out tables a live publication tracks (CRITICAL #4). Idempotent.
    let schema = crate::query::quote_ident(app_id);
    pool.query_text_params(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), &[])
        .await
        .map_err(|e| {
            let mut err = DbError::from_pg(&e);
            prefix_message(&mut err, &format!("drop_namespace: DROP SCHEMA {app_id}: "));
            err
        })?;

    // ---- Step 7: DROP ROLE (per-app role from P6a-2) ----
    // Dropped LAST: the schema CASCADE removed the role's objects +
    // grants, so the role no longer owns anything and can be dropped.
    // Idempotent (`DROP ROLE IF EXISTS`). Production-only — the role only
    // exists under `hardening`; without it this is a no-op (the role was
    // never created).
    #[cfg(feature = "hardening")]
    crate::auth::bootstrap::drop_per_app_role(pool, app_id).await?;

    Ok(DropNamespaceOutcome::Completed)
}

/// Invoke `ChangeStream::deprovision` for whichever backend arm is live.
/// Split out so the arm-matching lives in one place; the PG arm routes
/// to `replication::drop_publication_and_slot`, the SQLite arm to its
/// file-unlink teardown.
async fn deprovision_change_stream(
    backend: &BackendHandle,
    app_id: &str,
) -> Result<(), DbError> {
    use crate::backend::ChangeStream;
    if let Some(pg) = backend.as_change_stream_pg() {
        return pg.deprovision(app_id).await;
    }
    #[cfg(feature = "sqlite")]
    if let Some(sq) = backend.as_change_stream_sqlite() {
        return sq.deprovision(app_id).await;
    }
    // No CDC arm (shouldn't happen for a configured backend) — nothing
    // to deprovision.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_outcome_carries_count() {
        let o = DropNamespaceOutcome::Deferred {
            active_subscriptions: 3,
        };
        match o {
            DropNamespaceOutcome::Deferred { active_subscriptions } => {
                assert_eq!(active_subscriptions, 3);
            }
            _ => panic!("expected Deferred"),
        }
    }

    #[test]
    fn opts_round_trip() {
        let o = DropNamespaceOpts {
            force: true,
            subscription_count: 5,
        };
        assert!(o.force);
        assert_eq!(o.subscription_count, 5);
    }
}
