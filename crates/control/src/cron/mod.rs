//! In-process cron tasks for the control plane.
//!
//! Mirrors `auth::cron`: each task is a `loop { tick; sleep }` future detached
//! on the compio runtime via [`spawn_all`], living for the lifetime of the
//! process.
//!
//! Tasks:
//!   - [`audit_retention`] — the sanctioned deleter for the append-only
//!     `zeroship.app_audit` / `zeroship.authz_decisions` tables (P12). Peer of
//!     `auth::cron::audit_retention` (which sweeps `zeroship.audit_events`);
//!     shares the same `zeroship.audit_retention` GUC.
//!   - [`orphaned_app_reaper`] — purges apps left owner-less by the ISS-12
//!     account-erase reaper (auth), tearing down their DB rows + blobs. Excludes
//!     `system = true` apps (the platform console). See ISS-12b.

pub mod audit_retention;
pub mod billing_notify;
pub mod billing_reconcile;
pub mod dunning;
pub mod metering_export;
pub mod orphaned_app_reaper;
pub mod spend_reconcile;
pub mod stripe_reconcile;

use std::sync::Arc;

use crate::AppState;

/// Spawn every control-plane cron task onto the compio runtime.
///
/// Detached: tasks live for the lifetime of the process. The caller keeps the
/// `Arc<AppState>` alive for the lifetime of the server, so each cron's per-tick
/// connections / blob-store handles stay live.
pub fn spawn_all(state: Arc<AppState>, retention_months: u32, retention_check_secs: u64) {
    // Audit-retention sweep — needs only the registry (cheap clone of the
    // db-url handle inside `AppState`).
    let registry = Arc::new(state.registry.clone());
    compio::runtime::spawn(async move {
        audit_retention::run(registry, retention_months, retention_check_secs).await;
    })
    .detach();

    // Orphaned-app reaper — needs the full `AppState` (registry + blob VFS +
    // native per-app OAuth rows) to run the shared `api::purge_app` teardown.
    let reaper_state = Arc::clone(&state);
    compio::runtime::spawn(async move {
        orphaned_app_reaper::run(reaper_state, orphaned_app_reaper::DEFAULT_CHECK_SECS).await;
    })
    .detach();

    // Spend-reconcile sweep (billing PR5) — prices each app's period usage,
    // derives + persists its SpendState; the gateway pulls the new state on
    // its next /internal/routes poll (decision D1).
    let spend_state = Arc::clone(&state);
    compio::runtime::spawn(async move {
        spend_reconcile::run(spend_state, spend_reconcile::DEFAULT_TICK_SECS).await;
    })
    .detach();

    // Dunning sweep (billing G2) — suspends each `past_due` creator whose
    // dunning window (`max_dunning_days`, default 7) has elapsed; the gateway
    // 402s their apps on its next /internal/routes poll. Provider-agnostic and
    // ALWAYS spawned (peer of spend_reconcile): payment status is orthogonal to
    // which metering backend is configured.
    let dunning_state = Arc::clone(&state);
    compio::runtime::spawn(async move {
        dunning::run(dunning_state, dunning::DEFAULT_TICK_SECS).await;
    })
    .detach();

    // Billing-notify sweep (billing-ops gap #26, PR-6) — turns the already-written
    // billing transition rows (dunning history, newly-finalized invoices, newly-issued
    // refunds) into creator emails via the `BillingNotifier` seam: claim-before-send
    // under a dedicated advisory lock (multi-node safe), then flip to `sent`.
    // Provider-agnostic and ALWAYS spawned (peer of dunning): notifications are
    // orthogonal to which metering backend is configured. READ-ONLY w.r.t. money.
    let notify_state = Arc::clone(&state);
    compio::runtime::spawn(async move {
        billing_notify::run(notify_state, billing_notify::DEFAULT_TICK_SECS).await;
    })
    .detach();

    let tasks = provider_aware_cron_tasks(&state.billing_stack);
    if tasks.contains(&"billing_reconcile") {
        let billing_state = Arc::clone(&state);
        compio::runtime::spawn(async move {
            billing_reconcile::run(billing_state, billing_reconcile::DEFAULT_TICK_SECS).await;
        })
        .detach();
    }
    if tasks.contains(&"stripe_reconcile") {
        let reconcile_state = Arc::clone(&state);
        compio::runtime::spawn(async move {
            stripe_reconcile::run(reconcile_state, stripe_reconcile::DEFAULT_TICK_SECS).await;
        })
        .detach();
    }
    if tasks.contains(&"metering_export") {
        let export_state = Arc::clone(&state);
        compio::runtime::spawn(async move {
            metering_export::run(export_state, metering_export::DEFAULT_TICK_SECS).await;
        })
        .detach();
    }
}

/// The set of provider-aware cron tasks `spawn_all` would spawn for a given
/// provider kind (the export/invoice sweeps; the always-on sweeps —
/// audit-retention, orphaned-app reaper, spend-reconcile — are not listed).
///
/// This is the single source of truth the `$0-revenue guard` test asserts on
/// (blueprint §M5 table / §M9 risk 3) WITHOUT having to spin up the compio
/// runtime: it makes the "which crons run per provider" decision testable.
#[must_use]
pub fn provider_aware_cron_tasks(
    stack: &crate::metering::provider::BillingStack,
) -> Vec<&'static str> {
    let mut tasks = Vec::new();
    if !stack.metered_by_owned_local_provider() {
        tasks.push("metering_export");
    }
    if !stack.self_invoicing() {
        tasks.push("billing_reconcile");
        if stack.invoicer_id() == "lite" || stack.invoicer_id() == "stripe_invoice" {
            tasks.push("stripe_reconcile");
        }
    }
    tasks
}

#[cfg(test)]
mod tests {
    use super::provider_aware_cron_tasks;
    use std::sync::Arc;
    use crate::metering::provider::{BillingStack, Capabilities, MeteringProvider};

    /// THE $0-revenue guard (blueprint §M9 risk 3): under `stripe`, the
    /// `metering_export` cron — where CU is PUSHED — MUST be in the spawned set.
    /// Forgetting it yields a Stripe deployment that enforces locally but bills
    /// Stripe $0 (a silent revenue black hole). And `billing_reconcile` (whose
    /// `invoice` is a no-op under stripe) must NOT be spawned.
    #[test]
    fn stripe_spawns_metering_export_not_billing_reconcile() {
        let stack = stripe_meters_stack();
        let tasks = provider_aware_cron_tasks(&stack);
        assert!(
            tasks.contains(&"metering_export"),
            "stripe MUST spawn metering_export (else $0 revenue) — got {tasks:?}"
        );
        assert!(
            !tasks.contains(&"billing_reconcile"),
            "stripe must NOT spawn billing_reconcile (invoice is a no-op) — got {tasks:?}"
        );
    }

    /// Native is the mirror: `billing_reconcile` (the Native invoice rail) IS
    /// spawned; `metering_export` (a no-op under native) is NOT (pure waste).
    #[test]
    fn native_spawns_billing_reconcile_not_metering_export() {
        let stack = lite_stack();
        let tasks = provider_aware_cron_tasks(&stack);
        assert!(tasks.contains(&"billing_reconcile"), "native spawns billing_reconcile — got {tasks:?}");
        assert!(
            !tasks.contains(&"metering_export"),
            "native must NOT spawn metering_export (report_usage is a no-op) — got {tasks:?}"
        );
    }

    /// The Stripe state-reconciliation backstop (#28) rides the Native invoice rail (which
    /// MINTS the Stripe objects it re-reads), so it is spawned under `native` and NOT under
    /// the export backends (which never run the invoice rail → nothing platform-minted to
    /// reconcile).
    #[test]
    fn native_spawns_stripe_reconcile_export_backends_do_not() {
        let native_stack = lite_stack();
        let native = provider_aware_cron_tasks(&native_stack);
        assert!(
            native.contains(&"stripe_reconcile"),
            "native MUST spawn stripe_reconcile (the missed-webhook backstop) — got {native:?}"
        );
        let export = stripe_meters_stack();
        let tasks = provider_aware_cron_tasks(&export);
        assert!(
            !tasks.contains(&"stripe_reconcile"),
            "stripe_meters must NOT spawn stripe_reconcile (no owned invoice rail) — got {tasks:?}"
        );
    }

    /// OpenMeter, like Stripe, is an export backend: the `metering_export` cron —
    /// where CU is PUSHED (as CloudEvents) — MUST be spawned (else $0 export), and
    /// `billing_reconcile` (whose `invoice` is a no-op under openmeter) must NOT.
    #[test]
    fn openmeter_spawns_metering_export_not_billing_reconcile() {
        let stack = openmeter_stripe_invoice_stack();
        let tasks = provider_aware_cron_tasks(&stack);
        assert!(
            tasks.contains(&"metering_export"),
            "openmeter MUST spawn metering_export (else $0 export) — got {tasks:?}"
        );
        assert!(
            tasks.contains(&"billing_reconcile"),
            "openmeter+stripe_invoice must spawn billing_reconcile — got {tasks:?}"
        );
    }

    fn stripe_meters_stack() -> BillingStack {
        let p: Arc<dyn MeteringProvider> = Arc::new(CronProvider("stripe_meters"));
        BillingStack {
            meter: p.clone(),
            rater: p.clone(),
            invoicer: p,
            webhooks: Vec::new(),
        }
    }

    fn openmeter_stripe_invoice_stack() -> BillingStack {
        let meter: Arc<dyn MeteringProvider> = Arc::new(CronProvider("openmeter"));
        let invoicer: Arc<dyn MeteringProvider> = Arc::new(CronProvider("stripe_invoice"));
        BillingStack {
            meter,
            rater: invoicer.clone(),
            invoicer,
            webhooks: Vec::new(),
        }
    }

    fn lite_stack() -> BillingStack {
        let p: Arc<dyn MeteringProvider> = Arc::new(CronProvider("lite"));
        BillingStack {
            meter: p.clone(),
            rater: p.clone(),
            invoicer: p,
            webhooks: Vec::new(),
        }
    }

    #[derive(Debug)]
    struct CronProvider(&'static str);

    impl MeteringProvider for CronProvider {
        fn id(&self) -> &str {
            self.0
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::empty()
        }
    }
}
