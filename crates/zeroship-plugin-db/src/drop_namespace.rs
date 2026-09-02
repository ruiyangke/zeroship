//! Section 17.7 drop-namespace sequencing - the privileged PostgreSQL teardown
//! order for retiring a database namespace.
//!
//! App archive does not call this module. Control has no provisioning DSN and
//! must never issue privileged DDL. A future database teardown coordinator
//! belongs behind zeroship-migrate-server and must first re-home this app-keyed
//! shape onto the database binding lifecycle. This module remains the single
//! place the low-level PG step order is recorded so it cannot drift.
//!
//! ## Ordering
//!
//! 1. **Subscription gate.** If any subscription is active on the app,
//!    defer with `subscriptions_active` + a count. Under `--force`, fire
//!    `subscription_app_dropped` to every active subscriber (the broker
//!    `Closed` message — the SDK surfaces it as a terminal error on the
//!    subscription iterator for graceful shutdown), then proceed. No
//!    drop step runs while a subscription is observable.
//! 2. **Drain broker** via `subscription_app_dropped` (broker `drop_app`).
//! 3. **Consumer cancel + slot teardown.** Clear the in-process consumer
//!    mark (courtesy: the slot survives consumer death, so this
//!    is best-effort), then `ChangeStream::deprovision`, which runs the
//!    PG slot order: `pg_terminate_backend` against the slot's backend
//!    after the grace, then `pg_drop_replication_slot`. Retry from here
//!    on partial failure.
//! 4. `DROP SCHEMA "<app_id>" CASCADE`.
//! 5. `DROP ROLE "app_<id>_role"` - the per-app role. Dropped
//!    AFTER the schema so no objects depend on it.
//!
//! ## Publication ownership
//!
//! The worker does not own or drop the migration-owned publication.
//! Dropping the schema removes its table memberships and leaves an empty
//! publication for a privileged app-deletion reconciler to remove. That
//! privileged deletion hook is not wired in this module.
//!
//! ## Idempotency + retry
//!
//! Steps 3-5 are individually idempotent (`pg_drop_replication_slot`
//! tolerates a concurrent dropper via undefined_object; `DROP SCHEMA IF
//! EXISTS ... CASCADE`, `DROP ROLE IF EXISTS`). A `LockContention` from the slot drop (backend still
//! detaching) is surfaced so the caller retries from step 3. Every other
//! step is a no-op when its precondition is already met.
//!
//! ## This module is compiled only into test builds
//!
//! [`drop_namespace`] has no production caller and is not a creator-facing or
//! control-plane app lifecycle entry point. It is fully exercised by the
//! `tests/integration.rs` PG suite. Un-gate the `mod` declaration in `lib.rs`
//! when a database-keyed migrate-server coordinator owns the call.
//!
//! Until 2026-09-02 that fact was carried by a module-wide
//! `#![allow(dead_code)]` and this paragraph. The gate says the same thing and
//! checks it: a production build now contains no `DROP SCHEMA` / `DROP ROLE`
//! path at all, and the allow proved unnecessary once the gate was in place
//! (removed and re-checked under `--features test-helpers` - the tests use
//! every item, so nothing goes unused in the configuration that compiles it).
//!
//! **The tier census still counts this file.** It tiers by path and reads each
//! file on its own, so a `mod` declaration gated in `lib.rs` is invisible to
//! it; the one `compio_postgres` row here is real code in a build nobody ships.
//! Item-level cfgs ARE understood (census defect 10); module-level ones, from
//! the declaring file, are not.

use compio_postgres::Pool;

use crate::backend::BackendHandle;
use crate::backend::pg_error;
use zeroship_data_core::error::{DbError, prefix_message};

/// Options for [`drop_namespace`].
#[derive(Debug, Clone)]
pub struct DropNamespaceOpts {
    /// Operator override. When `false`, an active subscription defers the
    /// drop. When `true`, active subscribers are sent
    /// `subscription_app_dropped` and the drop proceeds.
    pub force: bool,
    /// Count of active subscriptions on the app, as seen by the CALLER.
    /// A future privileged coordinator must aggregate this across workers; the
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
    /// Every teardown step ran (or was already a no-op). The app's slots,
    /// schema, and per-app role are gone; its migration-owned publication is
    /// retained empty for privileged cleanup.
    Completed,
}

/// Run the §17.7 PG drop-namespace teardown for `app_id`.
///
/// `backend` supplies the worker's replication-slot capability. `pool` is a
/// separate provisioning connection supplied by the future privileged
/// control/migrated deletion hook for `DROP SCHEMA` / `DROP ROLE`; it must not
/// be the worker login.
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

    // ---- Step 3: consumer cancel (courtesy) + slot teardown ----
    // Refuse new readiness handshakes and signal this process's consumer.
    // Full deprovision below terminates every worker slot, so app deletion is
    // correct even when another container has not observed deletion yet.
    crate::cdc_lifecycle::shutdown_app(app_id).await;

    // Terminate each slot's backend after the grace and drop the slot.
    // Routed through the `ChangeStream::deprovision` adapter (PG arm
    // routes to `replication::drop_worker_slots`). Publication membership
    // remains migration-owned. On the SQLite arm this is the file-unlink
    // path (no slot/publication); `deprovision`
    // there is the SQLite teardown. A `LockContention` here means "retry
    // from step 3" per §17.7.
    deprovision_change_stream(backend, app_id).await?;

    // ---- Step 4: DROP SCHEMA CASCADE ----
    // Slots are gone before schema teardown. Postgres removes dropped-table
    // membership from the retained publication. Idempotent.
    let schema = crate::query::quote_ident(app_id);
    pool.query_text_params(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), &[])
        .await
        .map_err(|e| {
            let mut err = pg_error::classify(&e);
            prefix_message(&mut err, &format!("drop_namespace: DROP SCHEMA {app_id}: "));
            err
        })?;

    // ---- Step 5: DROP ROLE (per-app role) ----
    // Dropped LAST: the schema CASCADE removed the role's objects +
    // grants, so the role no longer owns anything and can be dropped.
    // Idempotent (`DROP ROLE IF EXISTS`).
    crate::auth::bootstrap::drop_per_app_role(pool, app_id).await?;

    Ok(DropNamespaceOutcome::Completed)
}

/// Invoke `ChangeStream::deprovision` for whichever backend arm is live.
/// Split out so the arm-matching lives in one place; the PG arm routes
/// to `replication::drop_worker_slots`, the SQLite arm to its
/// file-unlink teardown.
async fn deprovision_change_stream(backend: &BackendHandle, app_id: &str) -> Result<(), DbError> {
    use crate::backend::ChangeStream;
    if let Some(pg) = backend.as_change_stream_pg() {
        return pg.deprovision(app_id).await;
    }
    if let Some(sq) = backend.as_change_stream_sqlite() {
        return sq.deprovision(app_id).await;
    }
    // No CDC arm (should not happen for a configured backend); nothing
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
            DropNamespaceOutcome::Deferred {
                active_subscriptions,
            } => {
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
