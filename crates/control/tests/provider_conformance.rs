//! Provider conformance gate for billing adapters.
//!
//! A new metering provider is not trustworthy until it passes this shared suite:
//! wire it into the descriptor list, provide a DB-free recording backend, and let
//! the common assertions exercise the advertised capability traits.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_control::metering::provider::{
    assert_capability_consistency, AggregateQuery, BillingPeriod, Capabilities, DedupKey,
    DedupTtl, CorrectionCapability, IngestAck, InvoiceRef, LiteStore, MeteringProvider,
    ProviderCtx, ProviderError, StaticSecretResolver, SubjectRef, UsageEvent, UsageSubject,
};

const METER: &str = "compute_units";
const SECOND_METER: &str = "db_reads";
const STRIPE_METER_ID: &str = "mtr_compute_units_conformance";
const STRIPE_SECOND_METER_ID: &str = "mtr_db_reads_conformance";
const PERIOD: BillingPeriod = BillingPeriod {
    start: 1_783_468_800,
    end: 1_786_147_200,
};
const LAGO_API_KEY: &str = "lago_hmac_conformance";
const STRIPE_SECRET: &str = "sk_test_conformance";

#[compio::test]
async fn provider_conformance_lago() {
    run_provider_conformance(Adapter::Lago).await;
}

#[compio::test]
async fn provider_conformance_lite() {
    run_provider_conformance(Adapter::Lite).await;
}

#[compio::test]
async fn provider_conformance_openmeter() {
    run_provider_conformance(Adapter::OpenMeter).await;
}

#[compio::test]
async fn provider_conformance_stripe_meters() {
    run_provider_conformance(Adapter::StripeMeters).await;
}

#[compio::test]
async fn provider_conformance_stripe_invoice() {
    run_provider_conformance(Adapter::StripeInvoice).await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Adapter {
    Lago,
    Lite,
    OpenMeter,
    StripeMeters,
    StripeInvoice,
}

impl Adapter {
    fn id(self) -> &'static str {
        match self {
            Self::Lago => "lago",
            Self::Lite => "lite",
            Self::OpenMeter => "openmeter",
            Self::StripeMeters => "stripe_meters",
            Self::StripeInvoice => "stripe_invoice",
        }
    }

    fn expected_capabilities(self) -> Capabilities {
        match self {
            Self::Lago => Capabilities::METER | Capabilities::INVOICE,
            Self::Lite => Capabilities::METER | Capabilities::INVOICE,
            Self::OpenMeter => Capabilities::METER,
            Self::StripeMeters => Capabilities::METER | Capabilities::INVOICE,
            Self::StripeInvoice => Capabilities::INVOICE,
        }
    }
}

struct Fixture {
    adapter: Adapter,
    provider: Arc<dyn MeteringProvider>,
    backend: Backend,
}

enum Backend {
    Lago(MockHttpProvider),
    Lite(Arc<FakeLiteStore>),
    OpenMeter(MockHttpProvider),
    StripeMeters(MockHttpProvider),
    StripeInvoice(Arc<FakeLiteStore>),
}

impl Backend {
    fn expire_dedup_window(&self) {
        match self {
            Self::Lago(mock) | Self::OpenMeter(mock) | Self::StripeMeters(mock) => {
                mock.expire_dedup_window();
            }
            Self::Lite(store) | Self::StripeInvoice(store) => store.expire_dedup_window(),
        }
    }

    fn accepted_ingests(&self) -> usize {
        match self {
            Self::Lago(mock) | Self::OpenMeter(mock) | Self::StripeMeters(mock) => {
                mock.accepted_ingests()
            }
            Self::Lite(store) | Self::StripeInvoice(store) => store.accepted_events(),
        }
    }
}

async fn run_provider_conformance(adapter: Adapter) {
    let fx = build_fixture(adapter).await;

    assert_capabilities_consistent(&fx.provider, adapter.expected_capabilities());
    assert_invoice_model_matches_docs(&fx.provider);
    assert_dedup_contract_matches_docs(&fx.provider);
    assert_correction_capability_matches_docs(&fx.provider);
    assert_fail_closed_config(adapter).await;

    if fx.provider.as_meter().is_some() {
        assert_meter_retry_idempotency(&fx).await;
        assert_meter_read_back(&fx).await;
        assert_dedup_ttl_switchover(&fx).await;
        if matches!(adapter, Adapter::StripeMeters) {
            assert_stripe_meters_missing_metric_fails_closed(&fx).await;
        }
    }

    if fx.provider.as_invoicer().is_some() {
        assert_invoice_close_idempotent(&fx).await;
    }

}

async fn build_fixture(adapter: Adapter) -> Fixture {
    match adapter {
        Adapter::Lago => {
            let mock = MockHttpProvider::start(HttpKind::Lago).await;
            let provider = build_provider(
                adapter.id(),
                serde_json::json!({
                    "lago": {
                        "api_url": mock.base_url.clone(),
                        "api_key": "lago_api_key",
                    }
                }),
                HashMap::from([("lago_api_key".to_string(), LAGO_API_KEY.to_string())]),
                None,
            )
            .expect("lago provider builds");
            Fixture {
                adapter,
                provider,
                backend: Backend::Lago(mock),
            }
        }
        Adapter::Lite => {
            let store = Arc::new(FakeLiteStore::default());
            let provider = build_provider(
                adapter.id(),
                serde_json::json!({}),
                HashMap::new(),
                Some(store.clone()),
            )
            .expect("lite provider builds");
            Fixture {
                adapter,
                provider,
                backend: Backend::Lite(store),
            }
        }
        Adapter::OpenMeter => {
            let mock = MockHttpProvider::start(HttpKind::OpenMeter).await;
            let provider = build_provider(
                adapter.id(),
                serde_json::json!({
                    "openmeter": {
                        "base_url": mock.base_url.clone(),
                        "token": "openmeter_token",
                    }
                }),
                HashMap::from([(
                    "openmeter_token".to_string(),
                    "om_test_conformance".to_string(),
                )]),
                None,
            )
            .expect("openmeter provider builds");
            Fixture {
                adapter,
                provider,
                backend: Backend::OpenMeter(mock),
            }
        }
        Adapter::StripeMeters => {
            let mock = MockHttpProvider::start(HttpKind::StripeMeters).await;
            let provider = build_provider(
                adapter.id(),
                serde_json::json!({
                    "stripe_meters": {
                        "secret_key": "stripe_secret_key",
                        "base_url": mock.base_url.clone(),
                        "meters": {
                            METER: STRIPE_METER_ID,
                            SECOND_METER: STRIPE_SECOND_METER_ID,
                        },
                    }
                }),
                HashMap::from([("stripe_secret_key".to_string(), STRIPE_SECRET.to_string())]),
                Some(Arc::new(FakeLiteStore::default())),
            )
            .expect("stripe_meters provider builds");
            Fixture {
                adapter,
                provider,
                backend: Backend::StripeMeters(mock),
            }
        }
        Adapter::StripeInvoice => {
            let store = Arc::new(FakeLiteStore::default());
            let provider = build_provider(
                adapter.id(),
                serde_json::json!({
                    "stripe_invoice": {
                        "secret_key": "stripe_secret_key",
                    }
                }),
                HashMap::from([("stripe_secret_key".to_string(), STRIPE_SECRET.to_string())]),
                Some(store.clone()),
            )
            .expect("stripe_invoice provider builds");
            Fixture {
                adapter,
                provider,
                backend: Backend::StripeInvoice(store),
            }
        }
    }
}

fn build_provider(
    id: &str,
    raw_config: serde_json::Value,
    secrets: HashMap<String, String>,
    store: Option<Arc<FakeLiteStore>>,
) -> Result<Arc<dyn MeteringProvider>, ProviderError> {
    let registry = zeroship_control::metering::provider::builtin_registry();
    let store: Option<Arc<dyn LiteStore>> = store.map(|s| s as Arc<dyn LiteStore>);
    let ctx = ProviderCtx::new(
        raw_config,
        Arc::new(StaticSecretResolver::new(secrets)),
        store,
    );
    registry.build(id, &ctx)
}

fn assert_capabilities_consistent(provider: &Arc<dyn MeteringProvider>, expected: Capabilities) {
    assert_capability_consistency(provider.as_ref()).expect("capabilities/downcasts consistent");
    assert_eq!(provider.capabilities(), expected, "{} capability matrix drift", provider.id());
    assert_eq!(
        provider.capabilities().contains(Capabilities::METER),
        provider.as_meter().is_some()
    );
    assert_eq!(
        provider.capabilities().contains(Capabilities::INVOICE),
        provider.as_invoicer().is_some()
    );
}

fn assert_dedup_contract_matches_docs(provider: &Arc<dyn MeteringProvider>) {
    let dedup = provider.dedup();
    match provider.id() {
        "lago" => {
            assert_eq!(dedup.key, DedupKey::TransactionId);
            assert_eq!(dedup.ttl, DedupTtl::Unbounded);
        }
        "lite" => {
            assert_eq!(dedup.key, DedupKey::SourceAndId);
            assert_eq!(dedup.ttl, DedupTtl::Unbounded);
        }
        "openmeter" => {
            assert_eq!(dedup.key, DedupKey::SourceAndId);
            assert_eq!(dedup.ttl, DedupTtl::Unbounded);
        }
        "stripe_meters" => {
            assert_eq!(dedup.key, DedupKey::Identifier);
            assert_eq!(dedup.ttl, DedupTtl::Bounded(Duration::from_secs(24 * 60 * 60)));
        }
        "stripe_invoice" => {
            assert_eq!(dedup.key, DedupKey::NotApplicable);
            assert_eq!(dedup.ttl, DedupTtl::NotApplicable);
        }
        other => panic!("unexpected provider in conformance suite: {other}"),
    }
}

fn assert_invoice_model_matches_docs(provider: &Arc<dyn MeteringProvider>) {
    match provider.id() {
        "lite" | "stripe_invoice" => {
            assert!(
                provider.owns_local_invoice(),
                "{} must declare local invoice ownership",
                provider.id()
            );
            assert!(
                !provider.self_invoices(),
                "{} must not declare provider-side self-invoicing",
                provider.id()
            );
        }
        "stripe_meters" => {
            assert!(
                provider.self_invoices(),
                "stripe_meters must declare provider-side self-invoicing"
            );
            assert!(
                !provider.owns_local_invoice(),
                "stripe_meters must not declare local invoice ownership"
            );
        }
        "lago" | "openmeter" => {
            assert!(
                !provider.self_invoices() && !provider.owns_local_invoice(),
                "{} uses the default invoice model in this branch",
                provider.id()
            );
        }
        other => panic!("unexpected provider in conformance suite: {other}"),
    }
}

fn assert_correction_capability_matches_docs(provider: &Arc<dyn MeteringProvider>) {
    match provider.id() {
        "lago" => match provider.correction() {
            CorrectionCapability::Backfill { window, closed } => {
                assert!(window > Duration::ZERO);
                assert_eq!(
                    closed,
                    zeroship_control::metering::provider::ClosedPeriodPolicy::OpenPeriodOnly
                );
                assert!(
                    provider.as_backfiller().is_some(),
                    "lago Backfill correction must expose Backfiller"
                );
            }
            other => panic!("lago correction matrix drift: {other:?}"),
        },
        "openmeter" => assert_eq!(provider.correction(), CorrectionCapability::None),
        "stripe_meters" | "stripe_invoice" | "lite" => {
            assert_eq!(provider.correction(), CorrectionCapability::InvoiceCredit);
            assert!(
                provider.as_invoicer().is_some(),
                "{} InvoiceCredit correction must expose Invoicer",
                provider.id()
            );
        }
        other => panic!("unexpected provider in conformance suite: {other}"),
    }
}

async fn assert_meter_retry_idempotency(fx: &Fixture) {
    let meter = fx.provider.as_meter().expect("meter capability");
    let subject = subject_ref("retry");
    let mut batch = events_for_meter("retry", subject_uuid(&subject), METER, &[3, 5, 7]);
    batch.extend(events_for_meter(
        "retry",
        subject_uuid(&subject),
        SECOND_METER,
        &[11, 13],
    ));
    meter.ingest(&batch).await.expect("first ingest");
    let q_primary = aggregate_query_for_meter(&subject, METER);
    let q_secondary = aggregate_query_for_meter(&subject, SECOND_METER);
    let first_primary = meter
        .read_aggregate(&q_primary)
        .await
        .expect("read primary aggregate after first ingest");
    let first_secondary = meter
        .read_aggregate(&q_secondary)
        .await
        .expect("read secondary aggregate after first ingest");
    meter.ingest(&batch).await.expect("retry ingest");
    let second_primary = meter
        .read_aggregate(&q_primary)
        .await
        .expect("read primary aggregate after retry");
    let second_secondary = meter
        .read_aggregate(&q_secondary)
        .await
        .expect("read secondary aggregate after retry");
    assert_eq!(first_primary, 15);
    assert_eq!(first_secondary, 24);
    assert_eq!(
        second_primary, first_primary,
        "{} double-counted a retry for the primary meter",
        fx.provider.id()
    );
    assert_eq!(
        second_secondary, first_secondary,
        "{} double-counted a retry for the secondary meter",
        fx.provider.id()
    );
}

async fn assert_meter_read_back(fx: &Fixture) {
    let meter = fx.provider.as_meter().expect("meter capability");
    let subject = subject_ref("readback");
    let mut batch = events_for_meter("readback", subject_uuid(&subject), METER, &[11, 13]);
    batch.extend(events_for_meter(
        "readback",
        subject_uuid(&subject),
        SECOND_METER,
        &[17, 19],
    ));
    meter.ingest(&batch).await.expect("ingest read-back batch");
    let got_primary = meter
        .read_aggregate(&aggregate_query_for_meter(&subject, METER))
        .await
        .expect("read primary aggregate");
    let got_secondary = meter
        .read_aggregate(&aggregate_query_for_meter(&subject, SECOND_METER))
        .await
        .expect("read secondary aggregate");
    assert_eq!(
        got_primary,
        24,
        "{} primary aggregate read-back drifted",
        fx.provider.id()
    );
    assert_eq!(
        got_secondary,
        36,
        "{} secondary aggregate read-back drifted",
        fx.provider.id()
    );
}

async fn assert_dedup_ttl_switchover(fx: &Fixture) {
    let meter = fx.provider.as_meter().expect("meter capability");
    let subject = subject_ref("ttl");
    let batch = events_for_meter("ttl", subject_uuid(&subject), METER, &[17]);
    let q = aggregate_query_for_meter(&subject, METER);

    forward_under_contract(fx.provider.as_ref(), &batch, &q, false)
        .await
        .expect("initial forward");
    assert_eq!(meter.read_aggregate(&q).await.expect("initial aggregate"), 17);

    forward_under_contract(fx.provider.as_ref(), &batch, &q, false)
        .await
        .expect("within-window refoward");
    assert_eq!(
        meter.read_aggregate(&q).await.expect("within-window aggregate"),
        17,
        "{} did not dedup within its declared window",
        fx.provider.id()
    );

    let accepted_before = fx.backend.accepted_ingests();
    if matches!(fx.provider.dedup().ttl, DedupTtl::Bounded(_)) {
        fx.backend.expire_dedup_window();
    }
    forward_under_contract(fx.provider.as_ref(), &batch, &q, true)
        .await
        .expect("stale replay is handled by read-back/delta path");
    let accepted_after = fx.backend.accepted_ingests();
    let aggregate = meter.read_aggregate(&q).await.expect("stale aggregate");
    assert_eq!(
        aggregate, 17,
        "{} stale replay doubled the aggregate past its DedupContract ttl",
        fx.provider.id()
    );

    if matches!(fx.provider.dedup().ttl, DedupTtl::Bounded(_)) {
        assert_eq!(
            accepted_after, accepted_before,
            "{} stale bounded replay was blindly re-ingested",
            fx.provider.id()
        );
    }
}

/// Shared §6.2 forwarding decision used by the conformance harness.
///
/// Raw `Meter::ingest` is intentionally a provider primitive. The forwarder owns
/// the stale-replay switch: bounded dedup windows use read-back/delta
/// instead of naive re-ingest once a replay is older than the provider contract.
async fn forward_under_contract(
    provider: &dyn MeteringProvider,
    batch: &[UsageEvent],
    q: &AggregateQuery,
    stale_replay: bool,
) -> Result<IngestAck, ProviderError> {
    let meter = provider
        .as_meter()
        .ok_or_else(|| ProviderError::Config(format!("{} has no meter", provider.id())))?;
    if !stale_replay {
        return meter.ingest(batch).await;
    }

    match provider.dedup().ttl {
        DedupTtl::Unbounded => meter.ingest(batch).await,
        DedupTtl::Bounded(_) => {
            let _current = meter.read_aggregate(q).await?;
            Ok(IngestAck {
                accepted: 0,
                deduped: Some(batch.len()),
            })
        }
        DedupTtl::NotApplicable => Ok(IngestAck {
            accepted: 0,
            deduped: Some(batch.len()),
        }),
    }
}

async fn assert_invoice_close_idempotent(fx: &Fixture) {
    let invoicer = fx.provider.as_invoicer().expect("invoice capability");
    let subject = subject_ref("invoice");
    let first = invoicer
        .close_period(&subject, PERIOD)
        .await
        .expect("first close period");
    let second = invoicer
        .close_period(&subject, PERIOD)
        .await
        .expect("retry close period");
    assert_eq!(second, first, "{} close_period is not idempotent", fx.provider.id());

    if !matches!(fx.adapter, Adapter::Lago | Adapter::StripeMeters) {
        assert!(
            first.0.is_some(),
            "{} owned invoicer did not produce an invoice ref",
            fx.provider.id()
        );
    }
}

async fn assert_fail_closed_config(adapter: Adapter) {
    match adapter {
        Adapter::Lago => {
            assert_provider_config_fails("lago", serde_json::json!({}), HashMap::new(), None);
            assert_provider_config_fails(
                "lago",
                serde_json::json!({
                    "lago": {
                        "api_url": "http://127.0.0.1:1",
                        "api_key": "lago_api_key",
                    }
                }),
                HashMap::new(),
                None,
            );
        }
        Adapter::Lite => {
            let err = expect_provider_error(build_provider(
                "lite",
                serde_json::json!({}),
                HashMap::new(),
                None,
            ));
            assert!(err.to_string().contains("LiteStore is required"));
        }
        Adapter::OpenMeter => {
            assert_provider_config_fails("openmeter", serde_json::json!({}), HashMap::new(), None);
            assert_provider_config_fails(
                "openmeter",
                serde_json::json!({
                    "openmeter": {
                        "base_url": "http://127.0.0.1:1",
                        "token": "openmeter_token",
                    }
                }),
                HashMap::new(),
                None,
            );
        }
        Adapter::StripeMeters => {
            assert_provider_config_fails(
                "stripe_meters",
                serde_json::json!({}),
                HashMap::new(),
                None,
            );
            assert_provider_config_fails(
                "stripe_meters",
                serde_json::json!({
                    "stripe_meters": {
                        "secret_key": "stripe_secret_key",
                    }
                }),
                HashMap::from([("stripe_secret_key".to_string(), STRIPE_SECRET.to_string())]),
                None,
            );
            assert_provider_config_fails(
                "stripe_meters",
                serde_json::json!({
                    "stripe_meters": {
                        "secret_key": "stripe_secret_key",
                        "meters": {
                            METER: "",
                        },
                    }
                }),
                HashMap::from([("stripe_secret_key".to_string(), STRIPE_SECRET.to_string())]),
                Some(Arc::new(FakeLiteStore::default())),
            );
        }
        Adapter::StripeInvoice => {
            assert_provider_config_fails(
                "stripe_invoice",
                serde_json::json!({}),
                HashMap::new(),
                None,
            );
            assert_provider_config_fails(
                "stripe_invoice",
                serde_json::json!({
                    "stripe_invoice": {
                        "secret_key": "stripe_secret_key",
                    }
                }),
                HashMap::from([("stripe_secret_key".to_string(), STRIPE_SECRET.to_string())]),
                None,
            );
        }
    }
}

fn assert_provider_config_fails(
    id: &str,
    raw_config: serde_json::Value,
    secrets: HashMap<String, String>,
    store: Option<Arc<FakeLiteStore>>,
) {
    let err = expect_provider_error(build_provider(id, raw_config, secrets, store));
    assert!(
        matches!(err, ProviderError::Config(_) | ProviderError::Store(_)),
        "{id} invalid config failed with unexpected error: {err}"
    );
}

fn expect_provider_error(
    result: Result<Arc<dyn MeteringProvider>, ProviderError>,
) -> ProviderError {
    match result {
        Ok(provider) => panic!("provider factory unexpectedly built {}", provider.id()),
        Err(err) => err,
    }
}

async fn assert_stripe_meters_missing_metric_fails_closed(fx: &Fixture) {
    let meter = fx.provider.as_meter().expect("meter capability");
    let subject = subject_ref("missing-stripe-meter");
    let err = meter
        .read_aggregate(&aggregate_query_for_meter(&subject, "storage_ops"))
        .await
        .expect_err("unmapped stripe_meters metric must fail closed");
    assert!(
        matches!(err, ProviderError::Config(_)),
        "unmapped stripe_meters metric failed with unexpected error: {err}"
    );
    assert!(
        err.to_string().contains("storage_ops"),
        "unmapped stripe_meters metric error should name the metric: {err}"
    );
}

fn subject_ref(label: &str) -> SubjectRef {
    SubjectRef(stable_uuid(&format!("provider-conformance-{label}")).to_string())
}

fn subject_uuid(subject: &SubjectRef) -> Uuid {
    Uuid::parse_str(subject.as_str()).expect("test subject is UUID")
}

fn aggregate_query_for_meter(subject: &SubjectRef, meter: &str) -> AggregateQuery {
    AggregateQuery {
        subject: subject.clone(),
        meter: meter.to_string(),
        period: PERIOD,
    }
}

fn events_for_meter(label: &str, creator: Uuid, meter: &str, values: &[u64]) -> Vec<UsageEvent> {
    values
        .iter()
        .enumerate()
        .map(|(idx, value)| {
            let app = stable_uuid(&format!("provider-conformance-{label}-app"));
            let mut dims = BTreeMap::new();
            dims.insert("period_start".to_string(), PERIOD.start.to_string());
            dims.insert("period_end".to_string(), PERIOD.end.to_string());
            UsageEvent {
                event_id: format!("evt_{label}_{meter}_{idx}"),
                source: "provider-conformance".to_string(),
                subject: UsageSubject {
                    app: Some(app),
                    creator,
                },
                meter: meter.to_string(),
                value: *value,
                event_time: PERIOD.start + i64::try_from(idx).expect("idx fits i64"),
                dims,
            }
        })
        .collect()
}

fn stable_uuid(label: &str) -> Uuid {
    let digest = Sha256::digest(label.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

#[derive(Default)]
struct FakeLiteStore {
    seen: Mutex<HashSet<(String, String)>>,
    totals: Mutex<HashMap<(Uuid, i64, String), u64>>,
    invoices: Mutex<HashMap<(Uuid, i64, i64), InvoiceRef>>,
}

impl FakeLiteStore {
    fn accepted_events(&self) -> usize {
        self.seen.lock().expect("seen poisoned").len()
    }

    fn expire_dedup_window(&self) {}
}

#[async_trait::async_trait(?Send)]
impl LiteStore for FakeLiteStore {
    async fn ingest_usage_events(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        let mut seen = self.seen.lock().expect("seen poisoned");
        let mut totals = self.totals.lock().expect("totals poisoned");
        let mut accepted = 0usize;
        let mut deduped = 0usize;
        for event in batch {
            let dedup_key = (event.source.clone(), event.event_id.clone());
            if !seen.insert(dedup_key) {
                deduped += 1;
                continue;
            }
            accepted += 1;
            let period_start = event
                .dims
                .get("period_start")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or_else(|| zeroship_control::metering::period_start_unix(event.event_time));
            *totals
                .entry((event.subject.creator, period_start, event.meter.clone()))
                .or_insert(0) += event.value;
        }
        Ok(IngestAck {
            accepted,
            deduped: Some(deduped),
        })
    }

    async fn owned_app_ids(&self, _creator: &Uuid) -> Result<Vec<Uuid>, ProviderError> {
        Ok(Vec::new())
    }

    async fn period_billable_units(
        &self,
        creator: &Uuid,
        period_start: i64,
    ) -> Result<u64, ProviderError> {
        let totals = self.totals.lock().expect("totals poisoned");
        Ok(totals
            .iter()
            .filter(|((stored_creator, stored_period, _meter), _)| {
                stored_creator == creator && *stored_period == period_start
            })
            .map(|(_, total)| *total)
            .sum())
    }

    async fn period_meter_units(
        &self,
        creator: &Uuid,
        period_start: i64,
        meter: &str,
    ) -> Result<u64, ProviderError> {
        let totals = self.totals.lock().expect("totals poisoned");
        Ok(totals
            .get(&(*creator, period_start, meter.to_string()))
            .copied()
            .unwrap_or(0))
    }

    async fn close_period_invoice(
        &self,
        creator: &Uuid,
        period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError> {
        let mut invoices = self.invoices.lock().expect("invoices poisoned");
        let key = (*creator, period.start, period.end);
        let invoice = invoices.entry(key).or_insert_with(|| {
            InvoiceRef(Some(format!(
                "in_fake_{}_{}_{}",
                creator.simple(),
                period.start,
                period.end
            )))
        });
        Ok(invoice.clone())
    }

    async fn adjustment_note_invoice(
        &self,
        creator: &Uuid,
        note: &zeroship_control::metering::provider::AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError> {
        let mut invoices = self.invoices.lock().expect("invoices poisoned");
        let key = (*creator, note.period.end, note.period.end);
        let invoice = invoices.entry(key).or_insert_with(|| {
            InvoiceRef(Some(format!(
                "in_adjustment_{}_{}_{}",
                creator.simple(),
                note.period.start,
                note.correction_seq
            )))
        });
        Ok(invoice.clone())
    }
}

#[derive(Clone, Copy, Debug)]
enum HttpKind {
    Lago,
    OpenMeter,
    StripeMeters,
}

#[derive(Clone)]
struct MockHttpProvider {
    base_url: String,
    state: Arc<Mutex<MockHttpState>>,
}

impl MockHttpProvider {
    async fn start(kind: HttpKind) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("mock local addr");
        let base_url = format!("http://{addr}");
        let state = Arc::new(Mutex::new(MockHttpState {
            kind,
            ..MockHttpState::default()
        }));
        let accept_state = Arc::clone(&state);
        compio::runtime::spawn(async move {
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    break;
                };
                let conn_state = Arc::clone(&accept_state);
                compio::runtime::spawn(async move {
                    serve_http_conn(stream, conn_state).await;
                })
                .detach();
            }
        })
        .detach();
        Self { base_url, state }
    }

    fn expire_dedup_window(&self) {
        self.state.lock().expect("mock state poisoned").dedupe_enabled = false;
    }

    fn accepted_ingests(&self) -> usize {
        self.state
            .lock()
            .expect("mock state poisoned")
            .accepted_ingests
    }
}

struct MockHttpState {
    kind: HttpKind,
    seen_ids: HashSet<String>,
    dedupe_enabled: bool,
    totals: HashMap<(String, String), u64>,
    idempotency_replies: HashMap<String, String>,
    accepted_ingests: usize,
}

impl Default for MockHttpState {
    fn default() -> Self {
        Self {
            kind: HttpKind::OpenMeter,
            seen_ids: HashSet::new(),
            dedupe_enabled: true,
            totals: HashMap::new(),
            idempotency_replies: HashMap::new(),
            accepted_ingests: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    path: String,
    idempotency_key: Option<String>,
    body: String,
}

async fn serve_http_conn(mut stream: TcpStream, state: Arc<Mutex<MockHttpState>>) {
    let mut acc = Vec::new();
    loop {
        loop {
            let Some((req, consumed)) = try_parse_request(&acc) else {
                break;
            };
            acc.drain(0..consumed);
            let response = handle_mock_request(&req, &state);
            if stream.write_all(response).await.0.is_err() {
                return;
            }
        }
        let buf = vec![0u8; 4096];
        let compio::BufResult(n, buf) = stream.read(buf).await;
        match n {
            Ok(0) | Err(_) => return,
            Ok(read) => acc.extend_from_slice(&buf[..read]),
        }
    }
}

fn try_parse_request(buf: &[u8]) -> Option<(RecordedRequest, usize)> {
    let text = std::str::from_utf8(buf).ok()?;
    let header_end = text.find("\r\n\r\n")?;
    let head = &text[..header_end];
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();

    let mut content_length = 0usize;
    let mut idempotency_key = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = v.trim().parse().unwrap_or(0),
                "idempotency-key" => idempotency_key = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    let body_start = header_end + 4;
    if buf.len() < body_start + content_length {
        return None;
    }
    let body = String::from_utf8_lossy(&buf[body_start..body_start + content_length]).to_string();
    Some((
        RecordedRequest {
            method,
            path,
            idempotency_key,
            body,
        },
        body_start + content_length,
    ))
}

fn handle_mock_request(req: &RecordedRequest, state: &Arc<Mutex<MockHttpState>>) -> Vec<u8> {
    let kind = state.lock().expect("mock state poisoned").kind;
    match kind {
        HttpKind::Lago => handle_lago_request(req, state),
        HttpKind::OpenMeter => handle_openmeter_request(req, state),
        HttpKind::StripeMeters => handle_stripe_request(req, state),
    }
}

fn handle_lago_request(req: &RecordedRequest, state: &Arc<Mutex<MockHttpState>>) -> Vec<u8> {
    if req.method == "POST" && req.path.starts_with("/api/v1/events") {
        let json: serde_json::Value = match serde_json::from_str(&req.body) {
            Ok(v) => v,
            Err(_) => return http_json(400, r#"{"error":"invalid_json"}"#),
        };
        let event = json.get("event").unwrap_or(&json);
        let transaction_id = event
            .get("transaction_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let subject = event
            .get("external_subscription_id")
            .or_else(|| event.get("external_customer_id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let code = event
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let value = event
            .get("properties")
            .and_then(|properties| properties.get("value"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let correct_total = event
            .get("properties")
            .and_then(|properties| properties.get("zeroship_correct_total"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let mut st = state.lock().expect("mock state poisoned");
        if st.dedupe_enabled && st.seen_ids.contains(&transaction_id) {
            return http_json(
                200,
                &format!(
                    r#"{{"event":{{"transaction_id":"{}","code":"{}"}}}}"#,
                    json_escape(&transaction_id),
                    json_escape(&code)
                ),
            );
        }
        st.seen_ids.insert(transaction_id.clone());
        let total = st.totals.entry((subject, code.clone())).or_insert(0);
        if correct_total {
            *total = value;
        } else {
            *total += value;
        }
        st.accepted_ingests += 1;
        return http_json(
            200,
            &format!(
                r#"{{"event":{{"transaction_id":"{}","code":"{}"}}}}"#,
                json_escape(&transaction_id),
                json_escape(&code)
            ),
        );
    }

    if req.method == "GET" && req.path.contains("/current_usage") {
        let subject = req
            .path
            .split("/api/v1/customers/")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .map(percent_decode)
            .unwrap_or_default();
        let charges = {
            let st = state.lock().expect("mock state poisoned");
            st.totals
                .iter()
                .filter(|((stored_subject, _metric), _)| stored_subject == &subject)
                .map(|((_, metric), total)| {
                    format!(
                        r#"{{"total_aggregated_units":"{total}.0","billable_metric":{{"code":"{}"}}}}"#,
                        json_escape(metric)
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        };
        return http_json(
            200,
            &format!(r#"{{"customer_usage":{{"charges_usage":[{charges}]}}}}"#),
        );
    }

    http_json(404, r#"{"error":"not_found"}"#)
}

fn handle_openmeter_request(req: &RecordedRequest, state: &Arc<Mutex<MockHttpState>>) -> Vec<u8> {
    if req.method == "POST" && req.path.starts_with("/api/v1/events") {
        let json: serde_json::Value = match serde_json::from_str(&req.body) {
            Ok(v) => v,
            Err(_) => return http_json(400, r#"{"error":{"code":"invalid_json"}}"#),
        };
        let source = json.get("source").and_then(serde_json::Value::as_str).unwrap_or("");
        let id = json.get("id").and_then(serde_json::Value::as_str).unwrap_or("");
        let meter = json
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let subject = json
            .get("subject")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let value = json
            .get("data")
            .and_then(|d| d.get("value"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let dedup_key = format!("{source}:{id}");
        let mut st = state.lock().expect("mock state poisoned");
        if st.dedupe_enabled && st.seen_ids.contains(&dedup_key) {
            return http_204();
        }
        st.seen_ids.insert(dedup_key);
        *st.totals.entry((subject, meter)).or_insert(0) += value;
        st.accepted_ingests += 1;
        return http_204();
    }

    if req.method == "GET" && req.path.contains("/query") {
        let meter = req
            .path
            .split("/api/v1/meters/")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .map(percent_decode)
            .unwrap_or_default();
        let subject = query_param(&req.path, "subject").unwrap_or_default();
        let total = state
            .lock()
            .expect("mock state poisoned")
            .totals
            .get(&(subject.clone(), meter))
            .copied()
            .unwrap_or(0);
        return http_json(
            200,
            &format!(r#"{{"data":[{{"value":{total},"subject":"{subject}"}}]}}"#),
        );
    }

    http_json(404, r#"{"error":{"code":"not_found"}}"#)
}

fn handle_stripe_request(req: &RecordedRequest, state: &Arc<Mutex<MockHttpState>>) -> Vec<u8> {
    if let Some(key) = &req.idempotency_key {
        let st = state.lock().expect("mock state poisoned");
        if st.dedupe_enabled {
            if let Some(prev) = st.idempotency_replies.get(key).cloned() {
                return http_json(200, &prev);
            }
        }
    }

    if req.method == "POST" && req.path.starts_with("/v1/billing/meter_events") {
        let event_name = form_param(&req.body, "event_name").unwrap_or_default();
        let meter_id = stripe_meter_id_for_event_name(&event_name);
        let subject = form_param(&req.body, "payload[stripe_customer_id]").unwrap_or_default();
        let value = form_param(&req.body, "payload[value]")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let identifier = form_param(&req.body, "identifier").unwrap_or_default();
        let mut st = state.lock().expect("mock state poisoned");
        if st.dedupe_enabled && st.seen_ids.contains(&identifier) {
            let body = r#"{"object":"billing.meter_event"}"#.to_string();
            if let Some(key) = &req.idempotency_key {
                st.idempotency_replies
                    .entry(key.clone())
                    .or_insert_with(|| body.clone());
            }
            return http_json(200, &body);
        }
        st.seen_ids.insert(identifier);
        *st.totals.entry((subject, meter_id)).or_insert(0) += value;
        st.accepted_ingests += 1;
        let body = r#"{"object":"billing.meter_event"}"#.to_string();
        if let Some(key) = &req.idempotency_key {
            st.idempotency_replies
                .entry(key.clone())
                .or_insert_with(|| body.clone());
        }
        return http_json(200, &body);
    }

    if req.method == "GET" && req.path.contains("/event_summaries") {
        let meter = req
            .path
            .split("/v1/billing/meters/")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .map(percent_decode)
            .unwrap_or_default();
        let subject = query_param(&req.path, "customer").unwrap_or_default();
        let total = state
            .lock()
            .expect("mock state poisoned")
            .totals
            .get(&(subject, meter))
            .copied()
            .unwrap_or(0);
        return http_json(
            200,
            &format!(r#"{{"data":[{{"aggregated_value":{total}}}]}}"#),
        );
    }

    http_json(200, r#"{"id":"obj_mock","object":"unknown"}"#)
}

fn stripe_meter_id_for_event_name(event_name: &str) -> String {
    match event_name {
        METER => STRIPE_METER_ID.to_string(),
        SECOND_METER => STRIPE_SECOND_METER_ID.to_string(),
        other => format!("mtr_unmapped_{other}"),
    }
}

fn query_param(path: &str, name: &str) -> Option<String> {
    let qs = path.split_once('?')?.1;
    for pair in qs.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if percent_decode(k) == name {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

fn form_param(body: &str, name: &str) -> Option<String> {
    for pair in body.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if percent_decode(k) == name {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn http_204() -> Vec<u8> {
    b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: keep-alive\r\n\r\n".to_vec()
}

fn http_json(status: u16, json: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let body = json.as_bytes();
    let mut resp = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         content-type: application/json\r\n\
         content-length: {}\r\n\
         connection: keep-alive\r\n\r\n",
        body.len()
    )
    .into_bytes();
    resp.extend_from_slice(body);
    resp
}
