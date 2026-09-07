//! Extensible billing-provider registry and capability traits.

use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

pub mod adapters;
pub mod control_store;
pub mod ctx;
pub mod registry;
pub mod types;

pub use control_store::ControlLiteStore;
pub use ctx::{
    Clock, HttpClientFactory, PlatformSecretResolver, ProviderCtx, SecretInput, SecretResolver,
};
pub use registry::{ProviderFactory, ProviderRegistry};
pub use types::{
    AdjustmentNote, AggregateQuery, BillingPeriod, ClosedPeriodPolicy, CorrectionCapability,
    DedupContract, DedupKey, DedupTtl, IngestAck, InvoiceRef, ProviderError, SubjectRef,
    UsageEvent, UsageSubject,
};

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Capabilities: u8 {
        const METER = 0b0001;
        const INVOICE = 0b0010;
    }
}

pub trait MeteringProvider: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> Capabilities;

    fn as_meter(&self) -> Option<&dyn Meter> {
        None
    }
    fn as_invoicer(&self) -> Option<&dyn Invoicer> {
        None
    }
    fn as_backfiller(&self) -> Option<&dyn Backfiller> {
        None
    }

    fn production_ready(&self) -> bool {
        true
    }

    /// True when this provider owns the provider-side invoice close itself.
    ///
    /// A self-invoicing provider must also be the configured meter, because the
    /// platform has no separate local invoice rail to run for it.
    fn self_invoices(&self) -> bool {
        false
    }

    /// True when this provider writes invoices into zeroship's local invoice
    /// tables and backs the Stripe-owned invoice reconciliation cron.
    fn owns_local_invoice(&self) -> bool {
        false
    }

    fn dedup(&self) -> DedupContract {
        DedupContract {
            key: DedupKey::NotApplicable,
            ttl: DedupTtl::NotApplicable,
        }
    }

    fn correction(&self) -> CorrectionCapability {
        CorrectionCapability::None
    }
}

#[async_trait::async_trait(?Send)]
pub trait Meter {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError>;
    async fn read_aggregate(&self, q: &AggregateQuery) -> Result<u64, ProviderError>;

    /// Whether this meter is fed by the stream forwarder (`ingest` of forwarded
    /// `UsageEvent`s). Providers that instead derive usage from the platform's
    /// local recompute snapshot (`usage_aggregates`) — e.g. `lite` — return
    /// `false`, so the control plane does not spawn a forwarder that would only
    /// error on every batch. The recompute rail runs regardless (it drives
    /// enforcement for every provider).
    fn accepts_forwarded_events(&self) -> bool {
        true
    }
}

#[async_trait::async_trait(?Send)]
pub trait Backfiller {
    async fn backfill(
        &self,
        subject: &SubjectRef,
        meter: &str,
        period: BillingPeriod,
        correct_total: u64,
    ) -> Result<(), ProviderError>;
}

#[async_trait::async_trait(?Send)]
pub trait Invoicer {
    async fn close_period(
        &self,
        subject: &SubjectRef,
        period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError>;

    async fn adjustment_note(
        &self,
        subject: &SubjectRef,
        note: &AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError>;
}

/// The local invoice rail a `lite`-family provider drives.
///
/// Every verb is scoped to ONE billing subject, and that subject is an
/// **organization id** (`org_…`, TEXT) rather than a `users.id` uuid. The change
/// is not a rename: `SubjectRef` is already a `String`, so a provider handing a
/// uuid-shaped subject to a store that now keys on `organizations(id)` would
/// find nothing and bill nobody rather than fail. The signature is therefore the
/// enforcement — there is no overload taking the old type, and no `From<Uuid>`
/// that would let one be passed by accident.
#[async_trait::async_trait(?Send)]
pub trait LiteStore: Send + Sync {
    async fn ingest_usage_events(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError>;
    async fn owned_app_ids(&self, organization: &str) -> Result<Vec<Uuid>, ProviderError>;
    async fn period_billable_units(
        &self,
        organization: &str,
        period_start: i64,
    ) -> Result<u64, ProviderError>;
    async fn period_meter_units(
        &self,
        organization: &str,
        period_start: i64,
        meter: &str,
    ) -> Result<u64, ProviderError>;
    async fn close_period_invoice(
        &self,
        organization: &str,
        period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError>;
    async fn adjustment_note_invoice(
        &self,
        organization: &str,
        note: &AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError>;
}

/// The organization one forwarded event names, or a refusal.
///
/// Every provider ingest path is downstream of [`crate::cron::event_forwarder`],
/// which fills the subject in from `apps.organization_id` and dead-letters what
/// it cannot attribute. An unset subject arriving here therefore means an event
/// reached a provider without passing that step, and the honest outcome is to
/// refuse it: the previous shape had no way to say this, because an unresolved
/// subject was `Uuid::nil()` and rendered as a perfectly well-formed customer
/// key that every app in the fleet shared.
pub(crate) fn event_subject<'a>(
    provider: &str,
    event: &'a UsageEvent,
) -> Result<&'a str, ProviderError> {
    event.organization_subject().ok_or_else(|| {
        ProviderError::Config(format!(
            "{provider}: usage event {} carries no organization subject",
            event.event_id
        ))
    })
}

pub fn assert_capability_consistency(p: &dyn MeteringProvider) -> Result<(), ProviderError> {
    let c = p.capabilities();
    let ok = c.contains(Capabilities::METER) == p.as_meter().is_some()
        && c.contains(Capabilities::INVOICE) == p.as_invoicer().is_some()
        && matches!(p.correction(), CorrectionCapability::Backfill { .. })
            == p.as_backfiller().is_some()
        && (!matches!(p.correction(), CorrectionCapability::InvoiceCredit)
            || p.as_invoicer().is_some())
        && (!p.self_invoices() || (p.as_meter().is_some() && p.as_invoicer().is_some()))
        && (!p.owns_local_invoice() || p.as_invoicer().is_some());
    if ok {
        Ok(())
    } else {
        Err(ProviderError::Config(format!(
            "{}: capabilities() disagree with as_*() downcasts",
            p.id()
        )))
    }
}

#[derive(Clone)]
pub struct BillingStack {
    pub meter: Arc<dyn MeteringProvider>,
    pub invoicer: Arc<dyn MeteringProvider>,
}

impl std::fmt::Debug for BillingStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingStack")
            .field("meter", &self.meter_id())
            .field("invoicer", &self.invoicer_id())
            .finish()
    }
}

impl BillingStack {
    #[must_use]
    pub fn meter_id(&self) -> &str {
        self.meter.id()
    }

    #[must_use]
    pub fn invoicer_id(&self) -> &str {
        self.invoicer.id()
    }

    #[must_use]
    pub fn metered_by_owned_local_provider(&self) -> bool {
        self.meter.owns_local_invoice()
    }

    #[must_use]
    pub fn self_invoicing(&self) -> bool {
        self.invoicer.self_invoices()
    }

    /// Whether the stream forwarder should run for this stack: true only when the
    /// meter accepts forwarded `UsageEvent`s. A recompute-fed provider (`lite`)
    /// returns false — its billing is driven by the local recompute snapshot, so
    /// a forwarder would only error on every batch. Enforcement recompute runs
    /// regardless of this flag.
    #[must_use]
    pub fn forwards_usage_events(&self) -> bool {
        self.meter
            .as_meter()
            .is_some_and(|meter| meter.accepts_forwarded_events())
    }

    #[must_use]
    pub fn invoicer_owns_local_invoice(&self) -> bool {
        self.invoicer.owns_local_invoice()
    }

    #[must_use]
    pub fn for_tests() -> Arc<Self> {
        let p: Arc<dyn MeteringProvider> = Arc::new(TestProvider);
        Arc::new(Self {
            meter: Arc::clone(&p),
            invoicer: p,
        })
    }

    #[must_use]
    pub fn with_meter_for_tests(meter: Arc<dyn MeteringProvider>) -> Arc<Self> {
        let invoicer: Arc<dyn MeteringProvider> = Arc::new(TestProvider);
        Arc::new(Self {
            meter,
            invoicer,
        })
    }
}

#[derive(Debug, Clone)]
pub struct BillingStackConfig {
    pub meter_provider: String,
    pub invoicer_provider: String,
    pub production: bool,
    pub allow_unsupported_billing: bool,
}

pub fn build_stack(
    registry: &ProviderRegistry,
    ctx: &ProviderCtx,
    cfg: &BillingStackConfig,
) -> Result<BillingStack, ProviderError> {
    let meter_id = cfg.meter_provider.trim();
    if meter_id.is_empty() {
        return Err(ProviderError::Config("no meter configured".to_string()));
    }
    let invoicer_id = cfg.invoicer_provider.trim();
    if invoicer_id.is_empty() {
        return Err(ProviderError::Config(
            "no invoicer configured: usage metered but never billed".to_string(),
        ));
    }
    let mut built: HashMap<String, Arc<dyn MeteringProvider>> = HashMap::new();
    let mut get = |id: &str| -> Result<Arc<dyn MeteringProvider>, ProviderError> {
        if let Some(p) = built.get(id) {
            return Ok(Arc::clone(p));
        }
        let p = registry.build(id, ctx)?;
        built.insert(id.to_string(), Arc::clone(&p));
        Ok(p)
    };

    let meter = get(meter_id)?;
    let invoicer = get(invoicer_id)?;

    if meter.as_meter().is_none() {
        return Err(ProviderError::Config(format!(
            "no meter configured: provider '{meter_id}' does not expose Meter"
        )));
    }
    if invoicer.as_invoicer().is_none() {
        return Err(ProviderError::Config(format!(
            "no invoicer configured: usage metered but never billed (provider '{invoicer_id}' does not expose Invoice)"
        )));
    }
    if invoicer.self_invoices() && meter.id() != invoicer.id() {
        return Err(ProviderError::Config(format!(
            "self-invoicing provider '{}' has no meter feed; select it as the meter too",
            invoicer.id()
        )));
    }
    if cfg.production && !cfg.allow_unsupported_billing {
        for p in [&meter, &invoicer] {
            if !p.production_ready() {
                return Err(ProviderError::Config(format!(
                    "provider '{}' is evaluation-grade and not production-hardened; pass --allow-unsupported-billing to run it knowingly",
                    p.id()
                )));
            }
        }
    }

    Ok(BillingStack {
        meter,
        invoicer,
    })
}

#[derive(Debug)]
struct TestProvider;

#[async_trait::async_trait(?Send)]
impl Meter for TestProvider {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        Ok(IngestAck {
            accepted: batch.len(),
            deduped: Some(0),
        })
    }

    async fn read_aggregate(&self, _q: &AggregateQuery) -> Result<u64, ProviderError> {
        Ok(0)
    }
}

#[async_trait::async_trait(?Send)]
impl Invoicer for TestProvider {
    async fn close_period(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError> {
        Ok(InvoiceRef(None))
    }

    async fn adjustment_note(
        &self,
        _subject: &SubjectRef,
        _note: &AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError> {
        Ok(InvoiceRef(None))
    }
}

impl MeteringProvider for TestProvider {
    fn id(&self) -> &str {
        "test"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::METER | Capabilities::INVOICE
    }

    fn as_meter(&self) -> Option<&dyn Meter> {
        Some(self)
    }

    fn as_invoicer(&self) -> Option<&dyn Invoicer> {
        Some(self)
    }
}

#[must_use]
pub fn builtin_registry() -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    adapters::register_builtin(&mut registry);
    Arc::new(registry)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ProviderCtx {
        ProviderCtx::new(
            serde_json::json!({}),
            Arc::new(PlatformSecretResolver),
            None,
        )
    }

    fn register_test(registry: &mut ProviderRegistry) {
        fn factory(_ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
            Ok(Arc::new(TestProvider))
        }
        registry.register("test", factory);
    }

    fn expect_provider_err<T>(result: Result<T, ProviderError>) -> ProviderError {
        match result {
            Ok(_) => panic!("expected provider error"),
            Err(err) => err,
        }
    }

    #[test]
    fn registry_register_build_and_known_work() {
        let mut registry = ProviderRegistry::default();
        register_test(&mut registry);
        assert_eq!(registry.known(), vec!["test"]);
        let provider = registry.build("test", &ctx()).expect("provider builds");
        assert_eq!(provider.id(), "test");
    }

    #[test]
    #[should_panic(expected = "duplicate provider id test")]
    fn registry_register_panics_on_duplicate_id() {
        let mut registry = ProviderRegistry::default();
        register_test(&mut registry);
        register_test(&mut registry);
    }

    struct RecomputeFedProvider;
    #[async_trait::async_trait(?Send)]
    impl Meter for RecomputeFedProvider {
        async fn ingest(&self, _batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
            Err(ProviderError::Store("recompute-fed: no forwarded ingest".into()))
        }
        async fn read_aggregate(&self, _q: &AggregateQuery) -> Result<u64, ProviderError> {
            Ok(0)
        }
        fn accepts_forwarded_events(&self) -> bool {
            false
        }
    }
    impl MeteringProvider for RecomputeFedProvider {
        fn id(&self) -> &str {
            "recompute_fed"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::METER
        }
        fn as_meter(&self) -> Option<&dyn Meter> {
            Some(self)
        }
    }

    #[test]
    fn forwards_usage_events_gates_on_meter_ingest_model() {
        // A forwarder-fed meter (the default) => run the forwarder.
        assert!(BillingStack::with_meter_for_tests(Arc::new(TestProvider)).forwards_usage_events());
        // A recompute-fed meter (lite-like) => do NOT run the forwarder (it would
        // perpetually error on ingest).
        assert!(
            !BillingStack::with_meter_for_tests(Arc::new(RecomputeFedProvider))
                .forwards_usage_events()
        );
    }

    #[test]
    fn registry_unknown_id_fails_closed_with_known_list() {
        let mut registry = ProviderRegistry::default();
        register_test(&mut registry);
        let err = expect_provider_err(registry.build("missing", &ctx()));
        assert!(err.to_string().contains("unknown metering provider 'missing'"));
        assert!(err.to_string().contains("known: test"));
    }

    #[test]
    fn build_stack_rejects_no_meter() {
        let mut registry = ProviderRegistry::default();
        register_test(&mut registry);
        let err = expect_provider_err(build_stack(
            &registry,
            &ctx(),
            &BillingStackConfig {
                meter_provider: " ".to_string(),
                invoicer_provider: "test".to_string(),
                production: false,
                allow_unsupported_billing: false,
            },
        ));
        assert!(err.to_string().contains("no meter configured"));
    }

    #[test]
    fn build_stack_rejects_no_invoicer() {
        let mut registry = ProviderRegistry::default();
        register_test(&mut registry);
        let err = expect_provider_err(build_stack(
            &registry,
            &ctx(),
            &BillingStackConfig {
                meter_provider: "test".to_string(),
                invoicer_provider: String::new(),
                production: false,
                allow_unsupported_billing: false,
            },
        ));
        assert!(err.to_string().contains("usage metered but never billed"));
    }

    #[derive(Debug)]
    struct MeterOnly;

    #[async_trait::async_trait(?Send)]
    impl Meter for MeterOnly {
        async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
            Ok(IngestAck {
                accepted: batch.len(),
                deduped: Some(0),
            })
        }
        async fn read_aggregate(&self, _q: &AggregateQuery) -> Result<u64, ProviderError> {
            Ok(0)
        }
    }

    impl MeteringProvider for MeterOnly {
        fn id(&self) -> &str {
            "meter_only"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::METER
        }
        fn as_meter(&self) -> Option<&dyn Meter> {
            Some(self)
        }
    }

    fn register_meter_only(registry: &mut ProviderRegistry) {
        fn factory(_ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
            Ok(Arc::new(MeterOnly))
        }
        registry.register("meter_only", factory);
    }

    #[derive(Debug)]
    struct SelfInvoicing;

    #[async_trait::async_trait(?Send)]
    impl Meter for SelfInvoicing {
        async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
            Ok(IngestAck {
                accepted: batch.len(),
                deduped: Some(0),
            })
        }
        async fn read_aggregate(&self, _q: &AggregateQuery) -> Result<u64, ProviderError> {
            Ok(0)
        }
    }

    #[async_trait::async_trait(?Send)]
    impl Invoicer for SelfInvoicing {
        async fn close_period(
            &self,
            _subject: &SubjectRef,
            _period: BillingPeriod,
        ) -> Result<InvoiceRef, ProviderError> {
            Ok(InvoiceRef(None))
        }

        async fn adjustment_note(
            &self,
            _subject: &SubjectRef,
            _note: &AdjustmentNote,
        ) -> Result<InvoiceRef, ProviderError> {
            Ok(InvoiceRef(None))
        }
    }

    impl MeteringProvider for SelfInvoicing {
        fn id(&self) -> &str {
            "self_invoicing"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::METER | Capabilities::INVOICE
        }
        fn as_meter(&self) -> Option<&dyn Meter> {
            Some(self)
        }
        fn as_invoicer(&self) -> Option<&dyn Invoicer> {
            Some(self)
        }
        fn self_invoices(&self) -> bool {
            true
        }
    }

    fn register_self_invoicing(registry: &mut ProviderRegistry) {
        fn factory(_ctx: &ProviderCtx) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
            Ok(Arc::new(SelfInvoicing))
        }
        registry.register("self_invoicing", factory);
    }

    #[test]
    fn build_stack_rejects_meter_only_without_invoicer() {
        let mut registry = ProviderRegistry::default();
        register_meter_only(&mut registry);
        let err = expect_provider_err(build_stack(
            &registry,
            &ctx(),
            &BillingStackConfig {
                meter_provider: "meter_only".to_string(),
                invoicer_provider: "meter_only".to_string(),
                production: false,
                allow_unsupported_billing: false,
            },
        ));
        assert!(err.to_string().contains("usage metered but never billed"));
    }

    #[test]
    fn build_stack_rejects_any_self_invoicer_without_its_meter_feed() {
        let mut registry = ProviderRegistry::default();
        register_test(&mut registry);
        register_self_invoicing(&mut registry);
        let err = expect_provider_err(build_stack(
            &registry,
            &ctx(),
            &BillingStackConfig {
                meter_provider: "test".to_string(),
                invoicer_provider: "self_invoicing".to_string(),
                production: false,
                allow_unsupported_billing: false,
            },
        ));
        assert!(
            err.to_string()
                .contains("self-invoicing provider 'self_invoicing' has no meter feed"),
            "{err}"
        );
    }

    #[test]
    fn build_stack_rejects_lite_under_production() {
        let registry = builtin_registry();
        let err = expect_provider_err(build_stack(
            &registry,
            &ctx_with_store(),
            &BillingStackConfig {
                meter_provider: "lite".to_string(),
                invoicer_provider: "lite".to_string(),
                production: true,
                allow_unsupported_billing: false,
            },
        ));
        assert!(err.to_string().contains("evaluation-grade"));
    }

    #[test]
    fn capability_consistency_rejects_mismatched_provider() {
        #[derive(Debug)]
        struct Bad;
        impl MeteringProvider for Bad {
            fn id(&self) -> &str {
                "bad"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities::METER
            }
        }

        let err = assert_capability_consistency(&Bad).unwrap_err();
        assert!(err.to_string().contains("capabilities() disagree"));
    }

    #[test]
    fn capability_consistency_rejects_invoice_credit_without_invoicer() {
        #[derive(Debug)]
        struct BadInvoiceCredit;
        impl MeteringProvider for BadInvoiceCredit {
            fn id(&self) -> &str {
                "bad_invoice_credit"
            }
            fn capabilities(&self) -> Capabilities {
                Capabilities::METER
            }
            fn correction(&self) -> CorrectionCapability {
                CorrectionCapability::InvoiceCredit
            }
        }

        let err = assert_capability_consistency(&BadInvoiceCredit).unwrap_err();
        assert!(err.to_string().contains("capabilities() disagree"));
    }

    #[derive(Default)]
    struct DummyLiteStore;

    #[async_trait::async_trait(?Send)]
    impl LiteStore for DummyLiteStore {
        async fn ingest_usage_events(
            &self,
            batch: &[UsageEvent],
        ) -> Result<IngestAck, ProviderError> {
            Ok(IngestAck {
                accepted: batch.len(),
                deduped: Some(0),
            })
        }

        async fn owned_app_ids(&self, _organization: &str) -> Result<Vec<Uuid>, ProviderError> {
            Ok(Vec::new())
        }

        async fn period_billable_units(
            &self,
            _organization: &str,
            _period_start: i64,
        ) -> Result<u64, ProviderError> {
            Ok(0)
        }

        async fn period_meter_units(
            &self,
            _organization: &str,
            _period_start: i64,
            _meter: &str,
        ) -> Result<u64, ProviderError> {
            Ok(0)
        }

        async fn close_period_invoice(
            &self,
            _organization: &str,
            _period: BillingPeriod,
        ) -> Result<InvoiceRef, ProviderError> {
            Ok(InvoiceRef(Some("dummy-invoice".to_string())))
        }

        async fn adjustment_note_invoice(
            &self,
            _organization: &str,
            _note: &AdjustmentNote,
        ) -> Result<InvoiceRef, ProviderError> {
            Ok(InvoiceRef(Some("dummy-adjustment-invoice".to_string())))
        }
    }

    fn ctx_with_store() -> ProviderCtx {
        ProviderCtx::new(
            serde_json::json!({}),
            Arc::new(PlatformSecretResolver),
            Some(Arc::new(DummyLiteStore)),
        )
    }
}
