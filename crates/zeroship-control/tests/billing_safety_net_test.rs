//! DB-gated coverage for the S7 billing reconciliation/correction safety net.
//!
//! Uses a tiny provider fake for the external meter/invoicer surface but writes
//! through the real `ControlLiteStore` into `invoices`/`invoice_lines` and the
//! real `billing_reconciliation_findings` table.

use crate::common;

use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::billing_reconcile;
use zeroship_control::metering::provider::{
    AdjustmentNote, AggregateQuery, BillingPeriod, BillingStack, Capabilities,
    ControlLiteStore, CorrectionCapability, IngestAck, InvoiceRef, LiteStore, Meter,
    MeteringProvider, ProviderError, SubjectRef, UsageEvent,
};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const METER: &str = "compute_units";
const SECOND_METER: &str = "db_reads";

fn db_url() -> String {
    common::require_control_db()
}

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-safety-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

/// A period this test OWNS, which is what its `subjects_checked` assertions
/// need and what the local random draw this replaced never actually gave them.
///
/// `reconcile_pass` counts every subject in the period, summed over every meter
/// in it, so `assert_eq!(subjects_checked, 1)` is a claim about the whole
/// period, not about this test's app. The old `unique_closed_period_now()` drew
/// a fresh month out of 2400 per call and was unique only by luck; measured on
/// 2026-08-20 it landed on a month another module had seeded and returned 121.
/// `common::next_isolated_period()` reserves a private window instead. See its
/// header for why a lock is the wrong repair and why this only works now that
/// these files share one process.
fn unique_closed_period_now() -> i64 {
    common::next_isolated_period()
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn build_fixture(db_url: &str, label: &str) -> Fixture {
    build_fixture_with_provider(db_url, label, "db_adjustment", 125).await
}

async fn build_fixture_with_provider(
    db_url: &str,
    label: &str,
    provider_id: &'static str,
    provider_quantity: u64,
) -> Fixture {
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );

    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls)
            .await
            .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();
    let control_pg = Arc::new(control_pg_client);

    let tax_provider = zeroship_control::tax::build_tax_provider(
        &zeroship_control::tax::TaxProviderConfig::native(),
    )
    .expect("native tax provider builds");
    let store = Arc::new(ControlLiteStore::new(
        registry.clone(),
        StripeStore::new(registry.clone()),
        SecretString::new("sk_test_unused".to_string()),
        "http://127.0.0.1:9".to_string(),
        tax_provider.clone(),
    ));
    let provider: Arc<dyn MeteringProvider> = Arc::new(DbAdjustmentProvider {
        id: provider_id,
        store,
        provider_quantity,
    });
    let billing_stack = Arc::new(BillingStack {
        meter: Arc::clone(&provider),
        invoicer: provider,
    });

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        workflow_blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new("sk_test_unused".to_string()),
        stripe_base_url: "http://127.0.0.1:9".to_string(),
        gateway_url: "http://127.0.0.1:9".to_string(),
        migrate_server_url: "http://127.0.0.1:9".to_string(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        origin_scheme: zeroship_core::config::OriginScheme::Https,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg,
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        auth_provider: zeroship_control::platform_auth_provider(
            "https://auth.zeroship.test/oauth2",
            Some(common::platform_jwks_url()),
        ),
        // No platform deploy-token mint here: that is control's OUTBOUND
        // destination for the device flow, and no fixture below drives one.
        provider_registry: zeroship_control::metering::provider::builtin_registry(),
        billing_stack,
        billing_stream: None,
        tax_provider,
        notifier: Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    });

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
    }
}

struct DbAdjustmentProvider {
    id: &'static str,
    store: Arc<ControlLiteStore>,
    provider_quantity: u64,
}

#[async_trait::async_trait(?Send)]
impl Meter for DbAdjustmentProvider {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        Ok(IngestAck {
            accepted: batch.len(),
            deduped: Some(0),
        })
    }

    async fn read_aggregate(&self, q: &AggregateQuery) -> Result<u64, ProviderError> {
        if q.meter == SECOND_METER {
            Ok(60)
        } else {
            Ok(self.provider_quantity)
        }
    }
}

#[async_trait::async_trait(?Send)]
impl zeroship_control::metering::provider::Invoicer for DbAdjustmentProvider {
    async fn close_period(
        &self,
        _subject: &SubjectRef,
        _period: BillingPeriod,
    ) -> Result<InvoiceRef, ProviderError> {
        Ok(InvoiceRef(None))
    }

    async fn adjustment_note(
        &self,
        subject: &SubjectRef,
        note: &AdjustmentNote,
    ) -> Result<InvoiceRef, ProviderError> {
        let creator = Uuid::parse_str(subject.as_str()).map_err(|e| {
            ProviderError::Config(format!("test provider subject is not a UUID: {e}"))
        })?;
        self.store.adjustment_note_invoice(&creator, note).await
    }
}

impl MeteringProvider for DbAdjustmentProvider {
    fn id(&self) -> &str {
        self.id
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::METER | Capabilities::INVOICE
    }

    fn as_meter(&self) -> Option<&dyn Meter> {
        Some(self)
    }

    fn as_invoicer(&self) -> Option<&dyn zeroship_control::metering::provider::Invoicer> {
        Some(self)
    }

    fn correction(&self) -> CorrectionCapability {
        CorrectionCapability::InvoiceCredit
    }
}

#[compio::test]
async fn reconcile_pass_writes_invoice_credit_adjustment_idempotently() {
    let url = db_url();
    let fx = build_fixture(&url, "invoice-credit").await;
    let period_start = billing_reconcile::previous_period_start_unix(unique_closed_period_now());
    let period = BillingPeriod {
        start: period_start,
        end: billing_reconcile::period_end_unix(period_start),
    };
    let creator = make_creator(&fx.state).await;
    let plan_id = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan_id, creator).await;
    seed_witness_and_invoice(&fx.state, creator, app, &plan_id, period).await;

    let first = billing_reconcile::reconcile_pass(&fx.state, period)
        .await
        .expect("first reconcile pass");
    assert_eq!(first.subjects_checked, 1);
    assert_eq!(first.corrections_issued, 1);
    assert_eq!(first.findings_recorded, 1);

    let (line_count, amount_sum) = correction_lines(&fx.state, app).await;
    assert_eq!(line_count, 1);
    assert_eq!(amount_sum, 25);
    assert_eq!(finding_count(&fx.state, app, period).await, 1);

    let second = billing_reconcile::reconcile_pass(&fx.state, period)
        .await
        .expect("second reconcile pass");
    assert_eq!(second.corrections_issued, 0);
    assert_eq!(second.findings_recorded, 0);
    let (line_count, amount_sum) = correction_lines(&fx.state, app).await;
    assert_eq!(line_count, 1);
    assert_eq!(amount_sum, 25);
    assert_eq!(finding_count(&fx.state, app, period).await, 1);

    // Teardown: the fixture holds a Postgres connection, and locals are dropped
    // only after the body returns - by which point the runtime is gone and the
    // socket can no longer be closed. Drop it explicitly, then wait for the
    // close to land.
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn stripe_meters_self_invoicing_drift_issues_invoice_credit_not_provider_reject() {
    let url = db_url();
    let fx = build_fixture_with_provider(&url, "stripe-meters-self-invoice", "stripe_meters", 100)
        .await;
    let period_start = billing_reconcile::previous_period_start_unix(unique_closed_period_now());
    let period = BillingPeriod {
        start: period_start,
        end: billing_reconcile::period_end_unix(period_start),
    };
    let creator = make_creator(&fx.state).await;
    let plan_id = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan_id, creator).await;
    seed_witness_and_invoice(&fx.state, creator, app, &plan_id, period).await;

    let first = billing_reconcile::reconcile_pass(&fx.state, period)
        .await
        .expect("stripe_meters self-invoicing reconcile pass");
    assert_eq!(first.subjects_checked, 1);
    assert_eq!(first.corrections_issued, 1);
    assert_eq!(first.provider_rejects, 0);
    assert_eq!(first.findings_recorded, 2);

    let (line_count, amount_sum) = correction_lines(&fx.state, app).await;
    assert_eq!(line_count, 1);
    assert_eq!(amount_sum, 25);
    assert_eq!(finding_count(&fx.state, app, period).await, 1);
    assert_eq!(
        provider_reject_count(&fx.state, app, period).await,
        0,
        "stripe_meters InvoiceCredit drift must not be recorded as provider_reject"
    );

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn stripe_meters_self_invoicing_unpriceable_drift_flags_not_credits() {
    // A self-invoicing provider (stripe_meters) prices the meter at the provider
    // and writes NO local invoice lines, so there is no monetary basis. A drift
    // must NOT be "corrected" with a credit priced at the old ~1¢/unit fallback:
    // it is flagged `correction_unpriceable` for provider-authoritative repricing.
    let url = db_url();
    let fx = build_fixture_with_provider(&url, "stripe-meters-unpriceable", "stripe_meters", 100)
        .await;
    let period_start = billing_reconcile::previous_period_start_unix(unique_closed_period_now());
    let period = BillingPeriod {
        start: period_start,
        end: billing_reconcile::period_end_unix(period_start),
    };
    let creator = make_creator(&fx.state).await;
    let plan_id = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan_id, creator).await;
    // Witness only — deliberately NO invoice / invoice_lines seeded.
    seed_witness_only(&fx.state, app, period).await;

    let summary = billing_reconcile::reconcile_pass(&fx.state, period)
        .await
        .expect("stripe_meters unpriceable reconcile pass");
    assert_eq!(summary.subjects_checked, 1);
    assert_eq!(
        summary.corrections_issued, 0,
        "an unpriceable drift must not issue a (mispriced) correction"
    );
    assert_eq!(summary.provider_rejects, 0);

    let (line_count, _) = correction_lines(&fx.state, app).await;
    assert_eq!(line_count, 0, "no credit/debit line may be written");
    assert_eq!(
        unpriceable_finding_count(&fx.state, app, period).await,
        1,
        "the drift must be flagged correction_unpriceable for operator repricing"
    );

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn reconcile_pass_corrects_multi_metric_app_per_metric() {
    let url = db_url();
    let fx = build_fixture(&url, "multi-metric").await;
    let period_start = billing_reconcile::previous_period_start_unix(unique_closed_period_now());
    let period = BillingPeriod {
        start: period_start,
        end: billing_reconcile::period_end_unix(period_start),
    };
    let creator = make_creator(&fx.state).await;
    let plan_id = make_plan(&fx.state).await;
    seed_metric(&fx.state, SECOND_METER).await;
    let app = make_owned_app(&fx.state, &plan_id, creator).await;
    seed_multi_metric_witness_and_invoice(&fx.state, creator, app, &plan_id, period).await;

    let first = billing_reconcile::reconcile_pass(&fx.state, period)
        .await
        .expect("first multi-metric reconcile pass");
    assert_eq!(first.subjects_checked, 2);
    assert_eq!(first.corrections_issued, 2);
    assert_eq!(first.findings_recorded, 2);

    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT usage_snapshot \
             FROM zeroship.invoice_lines \
             WHERE app_id = $1 AND line_kind = 'debit_note' \
             ORDER BY correction_dedup_key",
            &[&app],
        )
        .await
        .expect("read correction lines");
    assert_eq!(rows.len(), 2);
    let deltas: std::collections::BTreeMap<String, i64> = rows
        .iter()
        .map(|row| {
            let usage: serde_json::Value = row.get("usage_snapshot");
            let meter = usage
                .get("meter")
                .and_then(serde_json::Value::as_str)
                .expect("meter recorded")
                .to_string();
            let delta = usage
                .get("quantity_delta")
                .and_then(serde_json::Value::as_i64)
                .expect("delta recorded");
            (meter, delta)
        })
        .collect();
    assert_eq!(deltas.get(METER), Some(&25));
    assert_eq!(deltas.get(SECOND_METER), Some(&10));
    assert_eq!(finding_count_for_meter(&fx.state, app, METER, period).await, 1);
    assert_eq!(
        finding_count_for_meter(&fx.state, app, SECOND_METER, period).await,
        1
    );

    let second = billing_reconcile::reconcile_pass(&fx.state, period)
        .await
        .expect("second multi-metric reconcile pass");
    assert_eq!(second.corrections_issued, 0);
    assert_eq!(second.findings_recorded, 0);

    drop(fx);
    common::drain_pg().await;
}

async fn make_creator(state: &AppState) -> Uuid {
    let email = format!("safety-{}@example.test", Uuid::new_v4().simple());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, 'Safety Test') RETURNING id",
            &[&email],
        )
        .await
        .expect("insert user");
    let creator: Uuid = rows[0].get("id");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.creator_billing (creator_id, default_pm_set) \
             VALUES ($1, true) ON CONFLICT (creator_id) DO NOTHING",
            &[&creator],
        )
        .await
        .expect("insert creator_billing");
    creator
}

async fn make_plan(state: &AppState) -> String {
    seed_metric(state, METER).await;
    let plan_id = format!("pln_safety_{}", Uuid::new_v4().simple());
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, net_policy_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'safety-test', 0, 0, 1000, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', \
                     '{\"max_sockets\":4,\"egress_ceiling_bytes\":10485760}', 100000)",
            &[&plan_id],
        )
        .await
        .expect("seed plan");
    plan_id
}

async fn seed_metric(state: &AppState, metric: &str) {
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.billing_metrics (metric, kind, unit) \
             VALUES ($1, 'platform', 'op') \
             ON CONFLICT (metric) DO UPDATE SET unit = EXCLUDED.unit",
            &[&metric],
        )
        .await
        .expect("seed metric");
}

async fn make_owned_app(state: &AppState, plan_id: &str, owner: Uuid) -> Uuid {
    let name = format!("safety-{}", Uuid::new_v4());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[&name, &plan_id, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app");
    let app_id: Uuid = rows[0].get("id");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.app_members (app_id, user_id, role) VALUES ($1, $2, 'owner')",
            &[&app_id, &owner],
        )
        .await
        .expect("insert owner membership");
    app_id
}

/// Seed only the local witness (usage_aggregates) with no invoice/invoice_lines —
/// the shape a self-invoicing provider produces (it bills at the provider).
async fn seed_witness_only(state: &AppState, app: Uuid, period: BillingPeriod) {
    let period_date = common::period_date(period.start);
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
             VALUES ($1, $2::date, $3, 125)",
            &[&app, &period_date, &METER],
        )
        .await
        .expect("seed witness only");
}

async fn seed_witness_and_invoice(
    state: &AppState,
    creator: Uuid,
    app: Uuid,
    plan_id: &str,
    period: BillingPeriod,
) {
    let period_date = common::period_date(period.start);
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
             VALUES ($1, $2::date, $3, 125)",
            &[&app, &period_date, &METER],
        )
        .await
        .expect("seed local witness");
    let invoice_id = zeroship_core::typed_id::new_invoice_id();
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices \
               (id, creator_id, period, status, subtotal_cents, total_cents) \
             VALUES ($1, $2, $3::date, 'draft', 100, 100)",
            &[&invoice_id, &creator, &period_date],
        )
        .await
        .expect("seed draft invoice");
    let usage = serde_json::json!({ "compute_units": 100 });
    let weights = serde_json::json!({});
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoice_lines \
               (invoice_id, app_id, segment_no, plan_id, included_units, \
                fx_pico_cents_per_unit, base_fee_cents, amount_cents, usage_snapshot, weights_snapshot) \
             VALUES ($1, $2, 0, $3, 0, 1000, 0, 100, $4, $5)",
            &[&invoice_id, &app, &plan_id, &usage, &weights],
        )
        .await
        .expect("seed invoiced usage line");
    state
        .control_pg
        .execute(
            "UPDATE zeroship.invoices \
             SET status = 'finalized', finalized_at = NOW(), updated_at = NOW() \
             WHERE id = $1",
            &[&invoice_id],
        )
        .await
        .expect("finalize seeded invoice");
}

async fn seed_multi_metric_witness_and_invoice(
    state: &AppState,
    creator: Uuid,
    app: Uuid,
    plan_id: &str,
    period: BillingPeriod,
) {
    let period_date = common::period_date(period.start);
    for (metric, total) in [(METER, 125_i64), (SECOND_METER, 60_i64)] {
        state
            .control_pg
            .execute(
                "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
                 VALUES ($1, $2::date, $3, $4)",
                &[&app, &period_date, &metric, &total],
            )
            .await
            .expect("seed local multi-metric witness");
    }
    let invoice_id = zeroship_core::typed_id::new_invoice_id();
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices \
               (id, creator_id, period, status, subtotal_cents, total_cents) \
             VALUES ($1, $2, $3::date, 'draft', 150, 150)",
            &[&invoice_id, &creator, &period_date],
        )
        .await
        .expect("seed draft invoice");
    let mut usage_map = serde_json::Map::new();
    usage_map.insert(METER.to_string(), serde_json::json!(100));
    usage_map.insert(SECOND_METER.to_string(), serde_json::json!(50));
    let usage = serde_json::Value::Object(usage_map);
    let weights = serde_json::json!({});
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoice_lines \
               (invoice_id, app_id, segment_no, plan_id, included_units, \
                fx_pico_cents_per_unit, base_fee_cents, amount_cents, usage_snapshot, weights_snapshot) \
             VALUES ($1, $2, 0, $3, 0, 1000, 0, 150, $4, $5)",
            &[&invoice_id, &app, &plan_id, &usage, &weights],
        )
        .await
        .expect("seed multi-metric invoiced usage line");
    state
        .control_pg
        .execute(
            "UPDATE zeroship.invoices \
             SET status = 'finalized', finalized_at = NOW(), updated_at = NOW() \
             WHERE id = $1",
            &[&invoice_id],
        )
        .await
        .expect("finalize seeded invoice");
}

async fn correction_lines(state: &AppState, app: Uuid) -> (i64, i64) {
    let rows = state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n, COALESCE(SUM(amount_cents), 0)::bigint AS amount \
             FROM zeroship.invoice_lines \
             WHERE app_id = $1 AND line_kind = 'debit_note'",
            &[&app],
        )
        .await
        .expect("count correction lines");
    (rows[0].get("n"), rows[0].get("amount"))
}

async fn finding_count(state: &AppState, app: Uuid, period: BillingPeriod) -> i64 {
    finding_count_for_meter(state, app, METER, period).await
}

async fn provider_reject_count(state: &AppState, app: Uuid, period: BillingPeriod) -> i64 {
    let entity_id = format!("billing-correction:{app}:{METER}:{}", period.start);
    let rows = state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n \
             FROM zeroship.billing_reconciliation_findings \
             WHERE kind = 'provider_reject'::text::zeroship.reconciliation_finding_kind \
               AND entity_id = $1",
            &[&entity_id],
        )
        .await
        .expect("count provider reject findings");
    rows[0].get("n")
}

async fn unpriceable_finding_count(state: &AppState, app: Uuid, period: BillingPeriod) -> i64 {
    let entity_id = format!("billing-correction:{app}:{METER}:{}", period.start);
    let rows = state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n \
             FROM zeroship.billing_reconciliation_findings \
             WHERE kind = 'correction_unpriceable'::text::zeroship.reconciliation_finding_kind \
               AND entity_id = $1",
            &[&entity_id],
        )
        .await
        .expect("count correction_unpriceable findings");
    rows[0].get("n")
}

async fn finding_count_for_meter(
    state: &AppState,
    app: Uuid,
    meter: &str,
    period: BillingPeriod,
) -> i64 {
    let entity_id = format!("billing-correction:{app}:{meter}:{}", period.start);
    let rows = state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n \
             FROM zeroship.billing_reconciliation_findings \
             WHERE kind = 'late_period_adjustment'::text::zeroship.reconciliation_finding_kind \
               AND entity_id = $1",
            &[&entity_id],
        )
        .await
        .expect("count findings");
    rows[0].get("n")
}

/// The allocator's band must belong to the allocator ALONE.
///
/// `common::next_isolated_period()` makes its own callers disjoint, and that is
/// all it can do: it is a process-global counter, and two other writers put rows
/// into far-future periods without going through it - hardcoded literals, and
/// the LIB test binary's `#[cfg(test)]` modules, which are a different PROCESS
/// sharing the same database. Neither can be handed a window.
///
/// So the band is reserved by ADDRESS instead. Everything else in this crate
/// pins periods below `common::ISOLATED_PERIOD_BASE_YEAR`; this fails if a new
/// literal moves into the band, which is the only way the disjointness the
/// billing assertions rely on can silently come apart.
///
/// Measured 2026-08-20 with the band at 2030: `src/cron/spend_recompute.rs`'s
/// hardcoded 2036-08 sat inside a window handed to a proration test, so two
/// modules' apps shared a period. This test is what keeps that from recurring.
#[test]
fn period_band_is_reserved_for_the_allocator() {
    fn collect_rs(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    // Years appearing where a PERIOD is being built: the two chrono
    // constructors, and `YYYY-MM` inside a quoted date. A bare integer is not
    // matched - `ISOLATED_PERIOD_BASE_YEAR` itself must not trip this.
    fn period_years(line: &str) -> Vec<i32> {
        let mut found = Vec::new();
        for marker in ["with_ymd_and_hms(", "from_ymd_opt("] {
            let mut rest = line;
            while let Some(at) = rest.find(marker) {
                rest = &rest[at + marker.len()..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if digits.len() == 4 {
                    if let Ok(y) = digits.parse::<i32>() {
                        found.push(y);
                    }
                }
            }
        }
        // BYTES ONLY below. Slicing `line` by byte index panics the moment a
        // source line holds a multi-byte character, and this crate's doc
        // comments do - the first run of this test died on an em-dash in
        // `src/internal.rs` rather than reporting anything.
        let b = line.as_bytes();
        for i in 0..b.len().saturating_sub(6) {
            let is_year = b[i..i + 4].iter().all(u8::is_ascii_digit);
            let looks_like_date =
                is_year && b[i + 4] == b'-' && b[i + 5].is_ascii_digit() && b[i + 6].is_ascii_digit();
            if looks_like_date && (i == 0 || !b[i - 1].is_ascii_digit()) {
                let year = (0..4).fold(0i32, |acc, k| acc * 10 + i32::from(b[i + k] - b'0'));
                found.push(year);
            }
        }
        found
    }

    let crate_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // The allocator names its own band; this file quotes the year that proved
    // the hazard. Both are documentation of the rule, not violations of it.
    let exempt = [
        crate_root.join("tests/common/mod.rs"),
        crate_root.join("tests/billing_safety_net_test.rs"),
    ];

    let mut files = Vec::new();
    for sub in ["src", "tests"] {
        collect_rs(&crate_root.join(sub), &mut files);
    }

    let mut offenders: Vec<String> = Vec::new();
    for file in &files {
        if exempt.contains(file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            for year in period_years(line) {
                if year >= common::ISOLATED_PERIOD_BASE_YEAR {
                    offenders.push(format!(
                        "{}:{}: {}",
                        file.strip_prefix(crate_root).unwrap_or(file).display(),
                        i + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these pin a period inside the band reserved for \
         common::next_isolated_period() (>= {}). Move them below it, or take a \
         window from the allocator - a literal in the band lands on top of \
         whichever test was handed that month:\n{}",
        common::ISOLATED_PERIOD_BASE_YEAR,
        offenders.join("\n")
    );
}
