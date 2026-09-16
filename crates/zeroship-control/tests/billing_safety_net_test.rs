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
    AdjustmentNote, AggregateQuery, BillingPeriod, BillingStack, Capabilities, ControlLiteStore,
    CorrectionCapability, IngestAck, InvoiceRef, LiteStore, Meter, MeteringProvider, ProviderError,
    SubjectRef, UsageEvent,
};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::AppId;

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
/// a fresh month from a fixed range per call and was unique only by luck;
/// measured on 2026-08-20 it landed on a month another module had seeded, and
/// the count came back as the whole fleet's.
///
/// `common::next_isolated_period()` reserves a private window instead - private
/// to this call within the run, AND to this run against the database, which is
/// what makes these four assertions survive a second run without a reset. See
/// its header for why a lock is the wrong repair.
async fn unique_closed_period_now() -> i64 {
    common::next_isolated_period().await
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
        service_auth: std::sync::Arc::new(zeroship_core::service_peers::ServiceAuth::unconfigured()),
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new("sk_test_unused".to_string()),
        stripe_base_url: "http://127.0.0.1:9".to_string(),
        worker_urls: Vec::new(),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        origin_scheme: zeroship_core::config::OriginScheme::Https,
        trust_proxy: false,
        worker_enrolment: zeroship_control::worker_join::EnrolmentEnvelope::closed(),
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
        mailer: std::sync::Arc::new(zeroship_mailer::RecordingMailer::new()),
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
        self.store
            .adjustment_note_invoice(subject.as_str(), note)
            .await
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
    let period_start =
        billing_reconcile::previous_period_start_unix(unique_closed_period_now().await);
    let period = BillingPeriod {
        start: period_start,
        end: billing_reconcile::period_end_unix(period_start),
    };
    let organization = make_organization(&fx.state).await;
    let organization = organization.as_str();
    let plan_id = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan_id, organization).await;
    seed_witness_and_invoice(&fx.state, organization, &app, &plan_id, period).await;

    let first = billing_reconcile::reconcile_pass(&fx.state, period)
        .await
        .expect("first reconcile pass");
    assert_eq!(first.subjects_checked, 1);
    assert_eq!(first.corrections_issued, 1);
    assert_eq!(first.findings_recorded, 1);

    let (line_count, amount_sum) = correction_lines(&fx.state, &app).await;
    assert_eq!(line_count, 1);
    assert_eq!(amount_sum, 25);
    assert_eq!(finding_count(&fx.state, &app, period).await, 1);

    let second = billing_reconcile::reconcile_pass(&fx.state, period)
        .await
        .expect("second reconcile pass");
    assert_eq!(second.corrections_issued, 0);
    assert_eq!(second.findings_recorded, 0);
    let (line_count, amount_sum) = correction_lines(&fx.state, &app).await;
    assert_eq!(line_count, 1);
    assert_eq!(amount_sum, 25);
    assert_eq!(finding_count(&fx.state, &app, period).await, 1);

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
    let fx =
        build_fixture_with_provider(&url, "stripe-meters-self-invoice", "stripe_meters", 100).await;
    let period_start =
        billing_reconcile::previous_period_start_unix(unique_closed_period_now().await);
    let period = BillingPeriod {
        start: period_start,
        end: billing_reconcile::period_end_unix(period_start),
    };
    let organization = make_organization(&fx.state).await;
    let organization = organization.as_str();
    let plan_id = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan_id, organization).await;
    seed_witness_and_invoice(&fx.state, organization, &app, &plan_id, period).await;

    let first = billing_reconcile::reconcile_pass(&fx.state, period)
        .await
        .expect("stripe_meters self-invoicing reconcile pass");
    assert_eq!(first.subjects_checked, 1);
    assert_eq!(first.corrections_issued, 1);
    assert_eq!(first.provider_rejects, 0);
    assert_eq!(first.findings_recorded, 2);

    let (line_count, amount_sum) = correction_lines(&fx.state, &app).await;
    assert_eq!(line_count, 1);
    assert_eq!(amount_sum, 25);
    assert_eq!(finding_count(&fx.state, &app, period).await, 1);
    assert_eq!(
        provider_reject_count(&fx.state, &app, period).await,
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
    let fx =
        build_fixture_with_provider(&url, "stripe-meters-unpriceable", "stripe_meters", 100).await;
    let period_start =
        billing_reconcile::previous_period_start_unix(unique_closed_period_now().await);
    let period = BillingPeriod {
        start: period_start,
        end: billing_reconcile::period_end_unix(period_start),
    };
    let organization = make_organization(&fx.state).await;
    let organization = organization.as_str();
    let plan_id = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan_id, organization).await;
    // Witness only — deliberately NO invoice / invoice_lines seeded.
    seed_witness_only(&fx.state, &app, period).await;

    let summary = billing_reconcile::reconcile_pass(&fx.state, period)
        .await
        .expect("stripe_meters unpriceable reconcile pass");
    assert_eq!(summary.subjects_checked, 1);
    assert_eq!(
        summary.corrections_issued, 0,
        "an unpriceable drift must not issue a (mispriced) correction"
    );
    assert_eq!(summary.provider_rejects, 0);

    let (line_count, _) = correction_lines(&fx.state, &app).await;
    assert_eq!(line_count, 0, "no credit/debit line may be written");
    assert_eq!(
        unpriceable_finding_count(&fx.state, &app, period).await,
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
    let period_start =
        billing_reconcile::previous_period_start_unix(unique_closed_period_now().await);
    let period = BillingPeriod {
        start: period_start,
        end: billing_reconcile::period_end_unix(period_start),
    };
    let organization = make_organization(&fx.state).await;
    let organization = organization.as_str();
    let plan_id = make_plan(&fx.state).await;
    seed_metric(&fx.state, SECOND_METER).await;
    let app = make_owned_app(&fx.state, &plan_id, organization).await;
    seed_multi_metric_witness_and_invoice(&fx.state, organization, &app, &plan_id, period).await;

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
            &[&app.as_str()],
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
    assert_eq!(
        finding_count_for_meter(&fx.state, &app, METER, period).await,
        1
    );
    assert_eq!(
        finding_count_for_meter(&fx.state, &app, SECOND_METER, period).await,
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

async fn make_organization(state: &AppState) -> String {
    let slug = format!("safety-{}", Uuid::new_v4().simple());
    let organization = zeroship_core::typed_id::generate("org");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
             VALUES ($1, $2, 'Safety Test', $3)",
            &[&organization, &slug, &format!("{slug}@example.test")],
        )
        .await
        .expect("insert organization");
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.organization_billing (organization_id, default_pm_set) \
             VALUES ($1, true) ON CONFLICT (organization_id) DO NOTHING",
            &[&organization],
        )
        .await
        .expect("insert organization_billing");
    organization
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

/// An app the given ORGANIZATION bills. See the equivalent in the reconcile
/// tests: an app seeded into a different organization than the one asserted on
/// is never billed, so the pass would go green over an empty set.
async fn make_owned_app(state: &AppState, plan_id: &str, organization: &str) -> AppId {
    let name = format!("safety-{}", Uuid::new_v4());
    common::seed_app_in_organization(&state.control_pg, &name, plan_id, organization).await
}

/// Seed only the local witness (usage_aggregates) with no invoice/invoice_lines —
/// the shape a self-invoicing provider produces (it bills at the provider).
async fn seed_witness_only(state: &AppState, app: &AppId, period: BillingPeriod) {
    let period_date = common::period_date(period.start);
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
             VALUES ($1, $2::date, $3, 125)",
            &[&app.as_str(), &period_date, &METER],
        )
        .await
        .expect("seed witness only");
}

async fn seed_witness_and_invoice(
    state: &AppState,
    organization: &str,
    app: &AppId,
    plan_id: &str,
    period: BillingPeriod,
) {
    let period_date = common::period_date(period.start);
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
             VALUES ($1, $2::date, $3, 125)",
            &[&app.as_str(), &period_date, &METER],
        )
        .await
        .expect("seed local witness");
    let invoice_id = zeroship_core::typed_id::new_invoice_id();
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices \
               (id, organization_id, period, status, subtotal_cents, total_cents) \
             VALUES ($1, $2, $3::date, 'draft', 100, 100)",
            &[&invoice_id, &organization, &period_date],
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
            &[&invoice_id, &app.as_str(), &plan_id, &usage, &weights],
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
    organization: &str,
    app: &AppId,
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
                &[&app.as_str(), &period_date, &metric, &total],
            )
            .await
            .expect("seed local multi-metric witness");
    }
    let invoice_id = zeroship_core::typed_id::new_invoice_id();
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.invoices \
               (id, organization_id, period, status, subtotal_cents, total_cents) \
             VALUES ($1, $2, $3::date, 'draft', 150, 150)",
            &[&invoice_id, &organization, &period_date],
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
            &[&invoice_id, &app.as_str(), &plan_id, &usage, &weights],
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

async fn correction_lines(state: &AppState, app: &AppId) -> (i64, i64) {
    let rows = state
        .control_pg
        .query(
            "SELECT COUNT(*)::bigint AS n, COALESCE(SUM(amount_cents), 0)::bigint AS amount \
             FROM zeroship.invoice_lines \
             WHERE app_id = $1 AND line_kind = 'debit_note'",
            &[&app.as_str()],
        )
        .await
        .expect("count correction lines");
    (rows[0].get("n"), rows[0].get("amount"))
}

async fn finding_count(state: &AppState, app: &AppId, period: BillingPeriod) -> i64 {
    finding_count_for_meter(state, app, METER, period).await
}

async fn provider_reject_count(state: &AppState, app: &AppId, period: BillingPeriod) -> i64 {
    let entity_id = format!(
        "billing-correction:{}:{METER}:{}",
        app.as_str(),
        period.start
    );
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

async fn unpriceable_finding_count(state: &AppState, app: &AppId, period: BillingPeriod) -> i64 {
    let entity_id = format!(
        "billing-correction:{}:{METER}:{}",
        app.as_str(),
        period.start
    );
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
    app: &AppId,
    meter: &str,
    period: BillingPeriod,
) -> i64 {
    let entity_id = format!(
        "billing-correction:{}:{meter}:{}",
        app.as_str(),
        period.start
    );
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

/// A LATER run against this database must start above every period THIS run
/// seeded. The four assertions above depend on it and nothing else checks it.
///
/// WHAT WENT WRONG WITHOUT IT. `next_isolated_period()` isolated its callers
/// from each other with a process-local counter over FIXED calendar months, so
/// every run handed out the same window sequence. `reconcile_pass` sweeps a
/// period FLEET-WIDE, so the second run against one database counted the first
/// run's subjects too and `subjects_checked` came back an exact multiple of the
/// run count. The suite was green only when a database reset had happened
/// immediately beforehand - and nothing said so, which is why the number it
/// produced was quoted as if it were a property of the code.
///
/// HOW THIS BINDS THE REPAIR. `resolve_run_band_base` is what a fresh process
/// would call before handing out its first window. Seeding a period and asking
/// it again must move it, and must move it PAST what was just written:
///
///   * a base that ignored the database (the defect) answers the same value
///     before and after, and answers below the seeded month;
///   * a base that answered some constant above the band would satisfy the
///     second check and fail the first.
///
/// Both directions are needed, which is why this seeds BETWEEN two readings
/// rather than taking one.
#[compio::test]
async fn a_later_run_starts_above_every_period_this_run_seeded() {
    let url = db_url();
    let fx = build_fixture(&url, "band-base").await;
    let organization = make_organization(&fx.state).await;
    let plan_id = make_plan(&fx.state).await;
    let app = make_owned_app(&fx.state, &plan_id, organization.as_str()).await;

    let before = common::resolve_run_band_base().await;

    // A window of this test's own, used the way every other caller uses one.
    let now = unique_closed_period_now().await;
    common::seed_usage_total(&fx.state.control_pg, &app, now, METER, 125).await;
    let seeded = common::months_since_band_start(common::period_date(now));

    let after = common::resolve_run_band_base().await;

    assert!(
        after > before,
        "the run band base did not move after a period was written into the \
         band ({before} -> {after}). It is not being read from the database, so \
         the next run against it will be handed the months this one just seeded."
    );
    assert!(
        after > seeded,
        "the run band base ({after}) is not above the month this run just \
         seeded ({seeded} months into the band). A later run would start on top \
         of these rows, and a fleet-wide period sweep would count both runs' \
         subjects."
    );

    // Teardown, as every case in this module does it - and this one owns MORE
    // connections than its neighbours, because each `resolve_run_band_base`
    // opens its own. They are dropped when that function returns; the drain is
    // what waits for the sockets to actually close, before the runtime that
    // owns them goes away.
    drop(fx);
    common::drain_pg().await;
}

/// The resolved base must reach the WINDOW ADDRESS, not just be resolved.
///
/// The peer of the test above, and it exists because that one cannot see this:
/// a run that resolved a base correctly and then left it out of the arithmetic
/// is the original defect exactly, and on a FRESH database it is invisible -
/// the base is zero there, so both formulas return the same months. That is why
/// the collision only ever showed up on a second run, and why a check written
/// against a freshly migrated database would have called the broken allocator
/// correct.
///
/// Two bases through the one function, differing in one variable.
#[test]
fn the_run_base_is_part_of_the_window_address() {
    let at_origin = common::isolated_period_offset_months(0, 0);
    let shifted = common::isolated_period_offset_months(7, 0);
    assert_eq!(
        shifted,
        at_origin + 7,
        "a run based 7 months up the band was handed the same window address as \
         a run based at its start. The base is being resolved and then dropped, \
         which is the defect this replaced: every run gets the same months, and \
         a fleet-wide period sweep counts every earlier run's subjects."
    );
    assert_eq!(
        common::isolated_period_offset_months(0, 1) - at_origin,
        common::isolated_period_offset_months(7, 1) - shifted,
        "the stride between windows changed with the run base, so the base is \
         being multiplied into the window rather than added to it."
    );
}

/// The allocator's band must belong to the allocator ALONE.
///
/// `common::next_isolated_period()` makes its own callers disjoint by a
/// process-global counter, and a run disjoint from earlier runs by starting
/// above what they left. Neither reaches the two writers that put rows into
/// far-future periods without going through it - hardcoded literals, and the
/// LIB test binary's `#[cfg(test)]` modules, which are a different PROCESS
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
            let looks_like_date = is_year
                && b[i + 4] == b'-'
                && b[i + 5].is_ascii_digit()
                && b[i + 6].is_ascii_digit();
            if !looks_like_date || (i > 0 && b[i - 1].is_ascii_digit()) {
                continue;
            }
            // THE MONTH MUST BE A MONTH, and without this the shape `dddd-dd`
            // matches any hyphenated pair of numbers in the crate. A port range
            // in `src/worker_join.rs` tripped it and was reported as pinning a
            // billing period in the reserved band. The
            // narrowing loses nothing the guard was built for: a quoted period
            // literal always carries a real month, and a chrono constructor is
            // caught by the marker scan above whatever its arguments are.
            let month = i32::from(b[i + 5] - b'0') * 10 + i32::from(b[i + 6] - b'0');
            if !(1..=12).contains(&month) {
                continue;
            }
            let year = (0..4).fold(0i32, |acc, k| acc * 10 + i32::from(b[i + k] - b'0'));
            found.push(year);
        }
        found
    }

    // THE SCANNER'S OWN CONTROL, because a narrowed detector that matches
    // nothing prints exactly what a clean crate prints. The first row is the
    // shape this guard exists to catch; the rest are shapes it must not.
    assert_eq!(period_years("let p = \"2136-08\";"), vec![2136]);
    assert_eq!(
        period_years("Utc.with_ymd_and_hms(2136, 8, 1, 0, 0, 0)"),
        vec![2136]
    );
    assert!(period_years("const PORTS: &str = \"8080-8090\";").is_empty());
    assert!(period_years("let range = \"9090-8080\";").is_empty());

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

    // The positive control above binds the MATCHER; this binds the SWEEP.
    // `collect_rs` returns on an unreadable directory, so a wrong root or a
    // renamed subtree leaves `files` empty and `offenders` empty with it -
    // green, having ruled on nothing. Both exempt paths are files this test
    // names by hand, so the sweep must have reached both of them.
    for known in &exempt {
        assert!(
            files.contains(known),
            "the sweep never reached {}, so it ruled on {} file(s) and possibly none of the right ones",
            known.display(),
            files.len()
        );
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
