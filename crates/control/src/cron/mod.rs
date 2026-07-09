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
pub mod event_forwarder;
pub mod orphaned_app_reaper;
pub mod spend_recompute;
pub mod spend_reconcile;
pub mod stripe_reconcile;

use std::sync::Arc;

use crate::AppState;

/// Spawn every control-plane cron task onto the compio runtime.
///
/// Detached: tasks live for the lifetime of the process. The caller keeps the
/// `Arc<AppState>` alive for the lifetime of the server, so each cron's per-tick
/// connections / blob-store handles stay live.
pub fn spawn_all(
    state: Arc<AppState>,
    retention_months: u32,
    retention_check_secs: u64,
    spend_recompute_interval_secs: u64,
) {
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

    if state.billing_stream.is_none() {
        tracing::warn!(
            "billing stream transport is not configured; usage metering/enforcement is disabled (no old-model fallback)"
        );
    }

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
    if should_spawn_billing_reconcile_safety_net(
        &state.billing_stack,
        state.billing_stream.is_some(),
    ) {
        let safety_net_state = Arc::clone(&state);
        compio::runtime::spawn(async move {
            billing_reconcile::run_safety_net(
                safety_net_state,
                billing_reconcile::DEFAULT_SAFETY_NET_TICK_SECS,
            )
            .await;
        })
        .detach();
    }
    if let Some(streams) = state.billing_stream.as_ref() {
        let forwarder_stream = match streams.build_forwarder() {
            Ok(stream) => stream,
            Err(err) => {
                tracing::error!(error = %err, "billing forwarder stream consumer build failed");
                return;
            }
        };
        let recompute_stream = match streams.build_recompute() {
            Ok(stream) => stream,
            Err(err) => {
                tracing::error!(error = %err, "spend recompute stream consumer build failed");
                return;
            }
        };
        let forwarder_stack = Arc::clone(&state.billing_stack);
        let forwarder_sink = Arc::new(event_forwarder::PgDeadLetterSink::new(Arc::clone(
            &state.control_pg,
        )));
        compio::runtime::spawn(async move {
            event_forwarder::run(
                forwarder_stream,
                forwarder_stack,
                forwarder_sink,
                event_forwarder::EventForwarderConfig::default(),
            )
            .await;
        })
        .detach();

        let recompute_state = Arc::clone(&state);
        compio::runtime::spawn(async move {
            spend_recompute::run(
                recompute_state,
                recompute_stream,
                spend_recompute::SpendRecomputeConfig {
                    interval: std::time::Duration::from_secs(
                        spend_recompute_interval_secs.max(1),
                    ),
                    ..spend_recompute::SpendRecomputeConfig::default()
                },
            )
            .await;
        })
        .detach();
    }
}

/// The set of provider-aware cron tasks `spawn_all` would spawn for a given
/// provider kind (the invoice/reconciliation sweeps; the always-on sweeps —
/// audit-retention, orphaned-app reaper, dunning, billing-notify — are not listed).
///
/// This is the single source of truth the `$0-revenue guard` test asserts on
/// (blueprint §M5 table / §M9 risk 3) WITHOUT having to spin up the compio
/// runtime: it makes the "which crons run per provider" decision testable.
#[must_use]
pub fn provider_aware_cron_tasks(
    stack: &crate::metering::provider::BillingStack,
) -> Vec<&'static str> {
    let mut tasks = Vec::new();
    if billing_reconcile_safety_net_needed(stack) {
        tasks.push("billing_reconcile_safety_net");
    }
    if !stack.self_invoicing() {
        tasks.push("billing_reconcile");
        if stack.invoicer_owns_local_invoice() {
            tasks.push("stripe_reconcile");
        }
    }
    tasks
}

/// Full spawn predicate for the §6.3 billing reconciliation safety-net. It
/// needs the retained stream witness; without a configured stream, spawning the
/// cron only wakes up to read stale/no-op snapshots.
#[must_use]
pub fn should_spawn_billing_reconcile_safety_net(
    stack: &crate::metering::provider::BillingStack,
    stream_configured: bool,
) -> bool {
    stream_configured && billing_reconcile_safety_net_needed(stack)
}

fn billing_reconcile_safety_net_needed(
    stack: &crate::metering::provider::BillingStack,
) -> bool {
    use crate::metering::provider::{Capabilities, CorrectionCapability};

    let has_meter = stack.meter.capabilities().contains(Capabilities::METER);
    let has_correction = !matches!(stack.meter.correction(), CorrectionCapability::None)
        || !matches!(stack.invoicer.correction(), CorrectionCapability::None);
    has_meter && (has_correction || !stack.metered_by_owned_local_provider())
}

#[cfg(test)]
mod tests {
    use super::{provider_aware_cron_tasks, should_spawn_billing_reconcile_safety_net};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use crate::metering::provider::{
        BillingStack, Capabilities, CorrectionCapability, MeteringProvider,
    };
    use zeroship_stream::{adapters, StreamConfig, StreamOffset, StreamRegistry};

    #[test]
    fn stripe_uses_stream_forwarder_not_metering_export_or_billing_reconcile() {
        let stack = stripe_meters_stack();
        let tasks = provider_aware_cron_tasks(&stack);
        assert!(
            !tasks.contains(&"metering_export"),
            "stripe must not spawn the deleted old-model metering_export cron — got {tasks:?}"
        );
        assert!(
            tasks.contains(&"billing_reconcile_safety_net"),
            "stripe MUST be safety-net eligible when a stream is configured — got {tasks:?}"
        );
        assert!(
            !tasks.contains(&"billing_reconcile"),
            "stripe must NOT spawn billing_reconcile (invoice is a no-op) — got {tasks:?}"
        );
        assert!(
            should_spawn_billing_reconcile_safety_net(&stack, true),
            "stripe safety-net still requires a configured stream"
        );
        assert!(
            !should_spawn_billing_reconcile_safety_net(&stack, false),
            "stripe safety-net must not spawn without a stream witness"
        );
    }

    #[test]
    fn native_spawns_billing_reconcile_not_metering_export() {
        let stack = lite_stack();
        let tasks = provider_aware_cron_tasks(&stack);
        assert!(
            tasks.contains(&"billing_reconcile"),
            "native spawns billing_reconcile — got {tasks:?}"
        );
        assert!(
            tasks.contains(&"billing_reconcile_safety_net"),
            "native is safety-net eligible when a stream is configured — got {tasks:?}"
        );
        assert!(
            !tasks.contains(&"metering_export"),
            "native must not spawn the deleted old-model metering_export cron — got {tasks:?}"
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

    #[test]
    fn openmeter_uses_stream_forwarder_not_metering_export() {
        let stack = openmeter_stripe_invoice_stack();
        let tasks = provider_aware_cron_tasks(&stack);
        assert!(
            !tasks.contains(&"metering_export"),
            "openmeter must not spawn the deleted old-model metering_export cron — got {tasks:?}"
        );
        assert!(
            tasks.contains(&"billing_reconcile_safety_net"),
            "openmeter+stripe_invoice needs the safety net for provider drift/late adjustments — got {tasks:?}"
        );
        assert!(
            tasks.contains(&"billing_reconcile"),
            "openmeter+stripe_invoice must spawn billing_reconcile — got {tasks:?}"
        );
        assert!(
            should_spawn_billing_reconcile_safety_net(&stack, true),
            "openmeter+stripe_invoice safety-net still requires a configured stream"
        );
        assert!(
            !should_spawn_billing_reconcile_safety_net(&stack, false),
            "openmeter+stripe_invoice safety-net must not spawn without a stream witness"
        );
    }

    #[test]
    fn safety_net_spawn_requires_stream_and_reconcilable_stack() {
        let stack = openmeter_stripe_invoice_stack();
        assert!(
            should_spawn_billing_reconcile_safety_net(&stack, true),
            "stream-backed openmeter+stripe_invoice needs the safety net"
        );
        assert!(
            !should_spawn_billing_reconcile_safety_net(&stack, false),
            "without a stream there is no retained witness to reconcile"
        );

        let noop = no_correction_local_stack();
        assert!(
            !should_spawn_billing_reconcile_safety_net(&noop, true),
            "a local stack with no provider drift/correction surface should not spawn the safety net"
        );
    }

    #[test]
    fn billing_stream_config_builds_independent_forwarder_and_recompute_consumers() {
        futures::executor::block_on(async {
            let suffix = unique_suffix();
            let topic = format!("zeroship-cron-groups-{suffix}");
            let mut registry = StreamRegistry::default();
            adapters::register_builtin(&mut registry);
            let streams = crate::BillingStreamConfig::new(
                Arc::new(registry),
                "memory",
                StreamConfig::from(serde_json::json!({
                    "topic": topic.clone(),
                    "group.id": "base-group-that-must-be-overridden",
                    "partitions": 1
                })),
                format!("billing-forwarder-{suffix}"),
                format!("spend-recompute-witness-{suffix}"),
            )
            .expect("billing stream config builds");

            let forwarder = streams.build_forwarder().expect("forwarder stream builds");
            let recompute = streams.build_recompute().expect("recompute stream builds");
            forwarder
                .publish(&topic, b"app-1", b"record-1")
                .await
                .expect("publish record");
            let forwarded = forwarder.poll(10).await.expect("forwarder poll");
            assert_eq!(forwarded.len(), 1);
            let offsets: Vec<_> = forwarded.iter().map(StreamOffset::from).collect();
            forwarder.commit(&offsets).await.expect("forwarder commit");

            recompute.rewind().await.expect("recompute rewind");
            let witness = recompute.poll(10).await.expect("recompute poll");
            assert_eq!(witness.len(), 1, "recompute gets its own full witness read");

            let fresh_forwarder = streams
                .build_forwarder()
                .expect("fresh forwarder stream builds");
            let after_recompute = fresh_forwarder
                .poll(10)
                .await
                .expect("fresh forwarder poll");
            assert!(
                after_recompute.is_empty(),
                "recompute rewind must not clobber the forwarder group's committed offset"
            );
        });
    }

    fn unique_suffix() -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_millis();
        format!("{}-{now}", std::process::id())
    }

    fn stripe_meters_stack() -> BillingStack {
        let p: Arc<dyn MeteringProvider> = Arc::new(CronProvider {
            id: "stripe_meters",
            capabilities: Capabilities::METER | Capabilities::INVOICE,
            correction: CorrectionCapability::InvoiceCredit,
            self_invoices: true,
            owns_local_invoice: false,
        });
        BillingStack {
            meter: p.clone(),
            invoicer: p,
        }
    }

    fn openmeter_stripe_invoice_stack() -> BillingStack {
        let meter: Arc<dyn MeteringProvider> = Arc::new(CronProvider {
            id: "openmeter",
            capabilities: Capabilities::METER,
            correction: CorrectionCapability::None,
            self_invoices: false,
            owns_local_invoice: false,
        });
        let invoicer: Arc<dyn MeteringProvider> = Arc::new(CronProvider {
            id: "stripe_invoice",
            capabilities: Capabilities::INVOICE,
            correction: CorrectionCapability::InvoiceCredit,
            self_invoices: false,
            owns_local_invoice: true,
        });
        BillingStack {
            meter,
            invoicer,
        }
    }

    fn lite_stack() -> BillingStack {
        let p: Arc<dyn MeteringProvider> = Arc::new(CronProvider {
            id: "lite",
            capabilities: Capabilities::METER | Capabilities::INVOICE,
            correction: CorrectionCapability::InvoiceCredit,
            self_invoices: false,
            owns_local_invoice: true,
        });
        BillingStack {
            meter: p.clone(),
            invoicer: p,
        }
    }

    fn no_correction_local_stack() -> BillingStack {
        let p: Arc<dyn MeteringProvider> = Arc::new(CronProvider {
            id: "lite",
            capabilities: Capabilities::METER | Capabilities::INVOICE,
            correction: CorrectionCapability::None,
            self_invoices: false,
            owns_local_invoice: true,
        });
        BillingStack {
            meter: p.clone(),
            invoicer: p,
        }
    }

    #[derive(Debug)]
    struct CronProvider {
        id: &'static str,
        capabilities: Capabilities,
        correction: CorrectionCapability,
        self_invoices: bool,
        owns_local_invoice: bool,
    }

    impl MeteringProvider for CronProvider {
        fn id(&self) -> &str {
            self.id
        }

        fn capabilities(&self) -> Capabilities {
            self.capabilities
        }

        fn correction(&self) -> CorrectionCapability {
            self.correction
        }

        fn self_invoices(&self) -> bool {
            self.self_invoices
        }

        fn owns_local_invoice(&self) -> bool {
            self.owns_local_invoice
        }
    }
}
