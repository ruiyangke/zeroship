//! Extensible billing-provider registry and capability traits.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use uuid::Uuid;

pub mod adapters;
pub mod control_store;
pub mod ctx;
pub mod registry;
pub mod types;

pub use control_store::ControlLiteStore;
pub use ctx::{Clock, HttpClientFactory, ProviderCtx, SecretHandle, SecretResolver, StaticSecretResolver};
pub use registry::{ProviderFactory, ProviderRegistry};
pub use types::{
    AdjustmentNote, AggregateQuery, BillingPeriod, ClosedPeriodPolicy, CorrectionCapability,
    DedupContract, DedupKey, DedupTtl, IngestAck, InvoiceRef, LineItem, ProviderError, RatedInput,
    SubjectRef, UsageEvent, WebhookEvent, UsageSubject, WebhookOutcome,
};

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Capabilities: u8 {
        const METER = 0b0001;
        const RATE = 0b0010;
        const INVOICE = 0b0100;
        const WEBHOOK = 0b1000;
    }
}

pub trait MeteringProvider: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> Capabilities;

    fn as_meter(&self) -> Option<&dyn Meter> {
        None
    }
    fn as_rater(&self) -> Option<&dyn Rater> {
        None
    }
    fn as_invoicer(&self) -> Option<&dyn Invoicer> {
        None
    }
    fn as_webhook(&self) -> Option<&dyn WebhookSink> {
        None
    }
    fn as_backfiller(&self) -> Option<&dyn Backfiller> {
        None
    }

    fn production_ready(&self) -> bool {
        true
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
}

#[async_trait::async_trait(?Send)]
pub trait Backfiller {
    async fn backfill(
        &self,
        subject: &SubjectRef,
        period: BillingPeriod,
        correct_total: u64,
    ) -> Result<(), ProviderError>;
}

#[async_trait::async_trait(?Send)]
pub trait Rater {
    async fn rate(
        &self,
        subject: &SubjectRef,
        period: BillingPeriod,
        input: &RatedInput,
    ) -> Result<Vec<LineItem>, ProviderError>;
}

#[async_trait::async_trait(?Send)]
pub trait Invoicer {
    async fn close_period(
        &self,
        subject: &SubjectRef,
        period: BillingPeriod,
        lines: &[LineItem],
    ) -> Result<InvoiceRef, ProviderError>;

    async fn adjustment_note(
        &self,
        subject: &SubjectRef,
        note: &AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError>;
}

#[async_trait::async_trait(?Send)]
pub trait WebhookSink {
    fn verify(&self, payload: &[u8], sig: &str) -> Result<(), ProviderError>;
    async fn handle(&self, event: WebhookEvent) -> Result<WebhookOutcome, ProviderError>;
}

#[async_trait::async_trait(?Send)]
pub trait LiteStore: Send + Sync {
    async fn ingest_usage_events(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError>;
    async fn owned_app_ids(&self, creator: &Uuid) -> Result<Vec<Uuid>, ProviderError>;
    async fn period_billable_units(
        &self,
        creator: &Uuid,
        period_start: i64,
    ) -> Result<u64, ProviderError>;
    async fn close_period_invoice(
        &self,
        creator: &Uuid,
        period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError>;
    async fn adjustment_note_invoice(
        &self,
        creator: &Uuid,
        note: &AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError>;
}

pub fn assert_capability_consistency(p: &dyn MeteringProvider) -> Result<(), ProviderError> {
    let c = p.capabilities();
    let ok = c.contains(Capabilities::METER) == p.as_meter().is_some()
        && c.contains(Capabilities::RATE) == p.as_rater().is_some()
        && c.contains(Capabilities::INVOICE) == p.as_invoicer().is_some()
        && c.contains(Capabilities::WEBHOOK) == p.as_webhook().is_some()
        && matches!(p.correction(), CorrectionCapability::Backfill { .. })
            == p.as_backfiller().is_some();
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
    pub rater: Arc<dyn MeteringProvider>,
    pub invoicer: Arc<dyn MeteringProvider>,
    pub webhooks: Vec<Arc<dyn MeteringProvider>>,
}

impl std::fmt::Debug for BillingStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BillingStack")
            .field("meter", &self.meter_id())
            .field("rater", &self.rater_id())
            .field("invoicer", &self.invoicer_id())
            .field(
                "webhooks",
                &self
                    .webhooks
                    .iter()
                    .map(|provider| provider.id())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl BillingStack {
    #[must_use]
    pub fn meter_id(&self) -> &str {
        self.meter.id()
    }

    #[must_use]
    pub fn rater_id(&self) -> &str {
        self.rater.id()
    }

    #[must_use]
    pub fn invoicer_id(&self) -> &str {
        self.invoicer.id()
    }

    #[must_use]
    pub fn metered_by_owned_local_provider(&self) -> bool {
        self.meter.id() == "lite"
    }

    #[must_use]
    pub fn self_invoicing(&self) -> bool {
        self.invoicer.id() == "stripe_meters"
    }

    #[must_use]
    pub fn for_tests() -> Arc<Self> {
        let p: Arc<dyn MeteringProvider> = Arc::new(TestProvider);
        Arc::new(Self {
            meter: Arc::clone(&p),
            rater: Arc::clone(&p),
            invoicer: p,
            webhooks: Vec::new(),
        })
    }

    #[must_use]
    pub fn with_meter_for_tests(meter: Arc<dyn MeteringProvider>) -> Arc<Self> {
        let invoicer: Arc<dyn MeteringProvider> = Arc::new(TestProvider);
        Arc::new(Self {
            meter,
            rater: Arc::clone(&invoicer),
            invoicer,
            webhooks: Vec::new(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct BillingStackConfig {
    pub meter_provider: String,
    pub rater_provider: Option<String>,
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
    let rater_id = cfg
        .rater_provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(invoicer_id);

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
    let rater = get(rater_id)?;

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
    if rater.as_rater().is_none() {
        return Err(ProviderError::Config(format!(
            "provider '{rater_id}' does not expose Rate"
        )));
    }
    if invoicer.id() == "stripe_meters" && meter.id() != "stripe_meters" {
        return Err(ProviderError::Config(
            "self-invoicing provider 'stripe_meters' has no meter feed; select it as the meter too"
                .to_string(),
        ));
    }
    if cfg.production && !cfg.allow_unsupported_billing {
        for p in [&meter, &rater, &invoicer] {
            if !p.production_ready() {
                return Err(ProviderError::Config(format!(
                    "provider '{}' is evaluation-grade and not production-hardened; pass --allow-unsupported-billing to run it knowingly",
                    p.id()
                )));
            }
        }
    }

    let mut seen = HashSet::new();
    let mut webhooks = Vec::new();
    for p in [&meter, &rater, &invoicer] {
        if p.as_webhook().is_some() && seen.insert(p.id().to_string()) {
            webhooks.push(Arc::clone(p));
        }
    }

    Ok(BillingStack {
        meter,
        rater,
        invoicer,
        webhooks,
    })
}

#[derive(Debug)]
struct TestProvider;

#[async_trait::async_trait(?Send)]
impl Meter for TestProvider {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        Ok(IngestAck {
            accepted: batch.len(),
            deduped: 0,
        })
    }

    async fn read_aggregate(&self, _q: &AggregateQuery) -> Result<u64, ProviderError> {
        Ok(0)
    }
}

#[async_trait::async_trait(?Send)]
impl Rater for TestProvider {
    async fn rate(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
        _input: &RatedInput,
    ) -> Result<Vec<LineItem>, ProviderError> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait(?Send)]
impl Invoicer for TestProvider {
    async fn close_period(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
        _lines: &[LineItem],
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
        Capabilities::METER | Capabilities::RATE | Capabilities::INVOICE
    }

    fn as_meter(&self) -> Option<&dyn Meter> {
        Some(self)
    }

    fn as_rater(&self) -> Option<&dyn Rater> {
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

pub fn build_registered_provider(
    id: &str,
    raw_config: serde_json::Value,
    secrets: HashMap<String, String>,
) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let registry = builtin_registry();
    let ctx = ProviderCtx::new(
        raw_config,
        Arc::new(StaticSecretResolver::new(secrets)),
        None,
    );
    registry.build(id, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ProviderCtx {
        ProviderCtx::new(
            serde_json::json!({}),
            Arc::new(StaticSecretResolver::default()),
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
                rater_provider: None,
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
                rater_provider: None,
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
                deduped: 0,
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

    #[test]
    fn build_stack_rejects_meter_only_without_invoicer() {
        let mut registry = ProviderRegistry::default();
        register_meter_only(&mut registry);
        let err = expect_provider_err(build_stack(
            &registry,
            &ctx(),
            &BillingStackConfig {
                meter_provider: "meter_only".to_string(),
                rater_provider: None,
                invoicer_provider: "meter_only".to_string(),
                production: false,
                allow_unsupported_billing: false,
            },
        ));
        assert!(err.to_string().contains("usage metered but never billed"));
    }

    #[test]
    fn build_stack_rejects_lite_under_production() {
        let registry = builtin_registry();
        let err = expect_provider_err(build_stack(
            &registry,
            &ctx_with_store(),
            &BillingStackConfig {
                meter_provider: "lite".to_string(),
                rater_provider: None,
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
                deduped: 0,
            })
        }

        async fn owned_app_ids(&self, _creator: &Uuid) -> Result<Vec<Uuid>, ProviderError> {
            Ok(Vec::new())
        }

        async fn period_billable_units(
            &self,
            _creator: &Uuid,
            _period_start: i64,
        ) -> Result<u64, ProviderError> {
            Ok(0)
        }

        async fn close_period_invoice(
            &self,
            _creator: &Uuid,
            _period: BillingPeriod,
        ) -> Result<InvoiceRef, ProviderError> {
            Ok(InvoiceRef(Some("dummy-invoice".to_string())))
        }

        async fn adjustment_note_invoice(
            &self,
            _creator: &Uuid,
            _note: &AdjustmentNote,
        ) -> Result<InvoiceRef, ProviderError> {
            Ok(InvoiceRef(Some("dummy-adjustment-invoice".to_string())))
        }
    }

    fn ctx_with_store() -> ProviderCtx {
        ProviderCtx::new(
            serde_json::json!({}),
            Arc::new(StaticSecretResolver::default()),
            Some(Arc::new(DummyLiteStore)),
        )
    }
}
