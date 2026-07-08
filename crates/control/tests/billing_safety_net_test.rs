//! DB-gated coverage for the S7 billing reconciliation/correction safety net.
//!
//! Uses a tiny provider fake for the external meter/invoicer surface but writes
//! through the real `ControlLiteStore` into `invoices`/`invoice_lines` and the
//! real `billing_reconciliation_findings` table.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::billing_reconcile;
use zeroship_control::metering::provider::{
    AdjustmentNote, AggregateQuery, BillingPeriod, BillingStack, Capabilities,
    ControlLiteStore, CorrectionCapability, IngestAck, InvoiceRef, LineItem, LiteStore, Meter,
    MeteringProvider, ProviderError, Rater, RatedInput, Subject, SubjectRef, UsageEvent,
};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const METER: &str = "compute_units";

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-safety-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
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
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
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
        store,
        provider_quantity: 125,
    });
    let billing_stack = Arc::new(BillingStack {
        meter: Arc::clone(&provider),
        rater: Arc::clone(&provider),
        invoicer: provider,
        webhooks: Vec::new(),
    });

    let state = Arc::new(AppState {
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
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: false,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg,
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        pat_issuer: Arc::new(zeroship_authn::PatIssuer::dev_insecure()),
        auth_provider: zeroship_control::platform_auth_provider(
            "https://auth.zeroship.test/oauth2",
            Some("http://127.0.0.1:9/oauth2/.well-known/jwks.json".to_string()),
        ),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
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
    store: Arc<ControlLiteStore>,
    provider_quantity: u64,
}

#[async_trait::async_trait(?Send)]
impl Meter for DbAdjustmentProvider {
    async fn ingest(&self, batch: &[UsageEvent]) -> Result<IngestAck, ProviderError> {
        Ok(IngestAck {
            accepted: batch.len(),
            deduped: 0,
        })
    }

    async fn read_aggregate(&self, _q: &AggregateQuery) -> Result<u64, ProviderError> {
        Ok(self.provider_quantity)
    }

    async fn ensure_subject(&self, subject: &Subject) -> Result<SubjectRef, ProviderError> {
        Ok(SubjectRef(subject.creator_id.to_string()))
    }
}

#[async_trait::async_trait(?Send)]
impl Rater for DbAdjustmentProvider {
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
impl zeroship_control::metering::provider::Invoicer for DbAdjustmentProvider {
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
        "db_adjustment"
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

    fn as_invoicer(&self) -> Option<&dyn zeroship_control::metering::provider::Invoicer> {
        Some(self)
    }

    fn correction(&self) -> CorrectionCapability {
        CorrectionCapability::InvoiceCredit
    }
}

#[compio::test]
async fn reconcile_pass_writes_invoice_credit_adjustment_idempotently() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "invoice-credit").await;
    let period_start = billing_reconcile::previous_period_start_unix(common::isolated_closed_period_now());
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
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.billing_metrics (metric, kind, unit) \
             VALUES ($1, 'platform', 'op') \
             ON CONFLICT (metric) DO UPDATE SET unit = EXCLUDED.unit",
            &[&METER],
        )
        .await
        .expect("seed metric");
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
    let entity_id = format!("billing-correction:{app}:{}", period.start);
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
