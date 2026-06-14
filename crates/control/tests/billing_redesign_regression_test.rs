//! Regression tests for the billing-schema redesign (Phase B), changes 5/6/9:
//!
//!   (5a) the invoice balance CHECK rejects a NON-atomic finalize — writing
//!        `subtotal` then `total` in two statements is rejected, proving finalize
//!        MUST be one UPDATE (the under-bill-window guard).
//!   (5b) re-running `charge_cents` over a finalized line's frozen snapshot
//!        reproduces `amount_cents` BIT-FOR-BIT (the reproducibility fix).
//!   (6)  the Native `invoice` verb's `lookup_invoice_id` resolves the finalized
//!        id via `invoices ⋈ billing_provider_refs` after the relocation.
//!   (9)  the immutability triggers: a finalized invoice rejects any non-void
//!        UPDATE; its lines reject UPDATE/DELETE; the only legal invoice
//!        transition is finalized→void.
//!
//! FAITHFUL by construction: every assertion runs against a live, migrated
//! Postgres (the REAL `invoices`/`invoice_lines`/`billing_provider_refs` tables +
//! the REAL immutability triggers + the REAL `charge_cents`). Gated on
//! `CONTROL_TEST_DB`; silent skip otherwise.

use std::collections::HashMap;

use compio_postgres::{connect, NoTls};
use uuid::Uuid;

use zeroship_control::pricing::{charge_cents, MetricWeight, MetricWeights, PlanPrice, FX_SCALE};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

async fn pg(db_url: &str) -> compio_postgres::Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

/// Seed a user → creator_billing → app, returning `(creator_id, app_id)`.
async fn seed_creator_app(client: &compio_postgres::Client) -> (Uuid, Uuid) {
    let email = format!("redesign-{}@test.invalid", Uuid::new_v4().simple());
    let creator: Uuid = client
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, 'redesign') RETURNING id",
            &[&email],
        )
        .await
        .expect("insert user")[0]
        .get("id");
    client
        .execute(
            "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) \
             ON CONFLICT (creator_id) DO NOTHING",
            &[&creator],
        )
        .await
        .expect("insert creator_billing");
    let plan_id = format!("pln_rd_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'rd', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 0)",
            &[&plan_id, &(FX_SCALE as i64)],
        )
        .await
        .expect("seed plan");
    let app: Uuid = client
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[&format!("rd-{}", Uuid::new_v4()), &plan_id, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app")[0]
        .get("id");
    (creator, app)
}

fn first_of_this_month() -> chrono::NaiveDate {
    use chrono::Datelike;
    let now = chrono::Utc::now().date_naive();
    chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap()
}

/// Claim a draft invoice for `(creator, period)`, returning its id.
async fn claim_draft(client: &compio_postgres::Client, creator: Uuid) -> String {
    let id = zeroship_core::typed_id::new_invoice_id();
    client
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&id, &creator, &first_of_this_month()],
        )
        .await
        .expect("claim draft");
    id
}

// ---------------------------------------------------------------------------
// (5a) The balance CHECK rejects a non-atomic finalize (two-statement write).
// ---------------------------------------------------------------------------

#[compio::test]
async fn nonatomic_finalize_two_statement_subtotal_then_total_is_rejected() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let (creator, _app) = seed_creator_app(&client).await;
    let inv = claim_draft(&client, creator).await;

    // A NON-atomic finalize: write the subtotal alone FIRST. The balance CHECK
    // `total = subtotal − credit + tax` is now violated (total=0, subtotal=500),
    // so this single statement is rejected — exactly why finalize must write
    // subtotal/credit/tax/total in ONE statement. (RED before the
    // invoice_total_balances CHECK: a half-written row would persist.)
    let res = client
        .execute(
            "UPDATE zeroship.invoices SET subtotal_cents = 500 WHERE id = $1",
            &[&inv],
        )
        .await;
    assert!(
        res.is_err(),
        "writing subtotal alone must violate the balance CHECK (finalize must be one UPDATE)",
    );

    // The atomic finalize (all money columns in ONE statement) is accepted.
    client
        .execute(
            "UPDATE zeroship.invoices SET subtotal_cents = 500, credit_cents = 0, \
               tax_cents = 0, total_cents = 500, status = 'finalized', finalized_at = NOW() \
             WHERE id = $1",
            &[&inv],
        )
        .await
        .expect("atomic finalize is accepted");
    let status: String = client
        .query("SELECT status FROM zeroship.invoices WHERE id = $1", &[&inv])
        .await
        .unwrap()[0]
        .get("status");
    assert_eq!(status, "finalized");
}

// ---------------------------------------------------------------------------
// (5b) charge_cents replays a finalized line's snapshot bit-for-bit.
// ---------------------------------------------------------------------------

#[compio::test]
async fn finalized_line_snapshot_replays_amount_cents_bit_for_bit() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let (creator, app) = seed_creator_app(&client).await;
    let inv = claim_draft(&client, creator).await;

    // Build a concrete charge: 750 requests @ weight 1 CU/op, fx = 2 cents/CU,
    // included 100 CU, base 50c ⇒ billable 650 CU × 2c + 50c = 1350c.
    let mut usage: HashMap<String, i64> = HashMap::new();
    usage.insert("requests".to_string(), 750);
    let mut weights: MetricWeights = HashMap::new();
    weights.insert("requests".to_string(), MetricWeight { units_per_op: 1, per_units: 1 });
    let fx = 2 * FX_SCALE as u64;
    let price = PlanPrice {
        base_fee_cents: 50,
        included_units: 100,
        fx_pico_cents_per_unit: Some(fx),
        spend_limit_default_cents: 0,
    };
    let breakdown = charge_cents(&price, &usage, &weights).expect("charge");
    assert_eq!(breakdown.total_cents, 650 * 2 + 50, "sanity: 1350c");

    // Freeze the snapshot onto the line (exactly what bill_creator writes).
    let usage_json = serde_json::to_value(&usage).unwrap();
    let weights_json = serde_json::to_value(
        &weights
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect::<std::collections::BTreeMap<_, _>>(),
    )
    .unwrap();
    client
        .execute(
            "INSERT INTO zeroship.invoice_lines \
               (invoice_id, app_id, included_units, fx_pico_cents_per_unit, base_fee_cents, \
                amount_cents, usage_snapshot, weights_snapshot) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &inv,
                &app,
                &(price.included_units as i64),
                &(fx as i64),
                &(price.base_fee_cents as i64),
                &(breakdown.total_cents as i64),
                &usage_json,
                &weights_json,
            ],
        )
        .await
        .expect("insert line");
    // Finalize (one statement) so the line is frozen.
    client
        .execute(
            "UPDATE zeroship.invoices SET subtotal_cents = $2, total_cents = $2, \
               status = 'finalized', finalized_at = NOW() WHERE id = $1",
            &[&inv, &(breakdown.total_cents as i64)],
        )
        .await
        .expect("finalize");

    // REPLAY: read the frozen snapshot back and re-run charge_cents. It must
    // reproduce amount_cents EXACTLY — no live rate-table read participates.
    let row = client
        .query(
            "SELECT included_units, fx_pico_cents_per_unit, base_fee_cents, amount_cents, \
                    usage_snapshot, weights_snapshot \
             FROM zeroship.invoice_lines WHERE invoice_id = $1 AND app_id = $2",
            &[&inv, &app],
        )
        .await
        .expect("read line")
        .into_iter()
        .next()
        .expect("one line");
    let frozen_amount: i64 = row.get("amount_cents");
    let frozen_usage: serde_json::Value = row.get("usage_snapshot");
    let frozen_weights: serde_json::Value = row.get("weights_snapshot");
    let replay_usage: HashMap<String, i64> = serde_json::from_value(frozen_usage).unwrap();
    let replay_weights: MetricWeights = serde_json::from_value(frozen_weights).unwrap();
    let replay_price = PlanPrice {
        base_fee_cents: row.get::<_, i64>("base_fee_cents") as u64,
        included_units: row.get::<_, i64>("included_units") as u64,
        fx_pico_cents_per_unit: Some(row.get::<_, i64>("fx_pico_cents_per_unit") as u64),
        spend_limit_default_cents: 0,
    };
    let replayed = charge_cents(&replay_price, &replay_usage, &replay_weights).expect("replay");
    assert_eq!(
        replayed.total_cents as i64, frozen_amount,
        "re-running charge_cents over the frozen snapshot reproduces amount_cents bit-for-bit",
    );
}

// ---------------------------------------------------------------------------
// (6) The Native `invoice` verb's id resolution after the relocation: the
//     finalized provider id lives on `billing_provider_refs`, resolved via the
//     `invoices ⋈ billing_provider_refs (finalized, provider='stripe',
//     ref_kind='invoice')` join — NOT on a `billing_runs.stripe_invoice_id`
//     column (which no longer exists). This is the exact query
//     `native.rs::lookup_invoice_id` runs.
// ---------------------------------------------------------------------------

#[compio::test]
async fn native_invoice_lookup_resolves_finalized_id_via_provider_refs() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let (creator, _app) = seed_creator_app(&client).await;
    let period = first_of_this_month();
    let inv = claim_draft(&client, creator).await;
    client
        .execute(
            "UPDATE zeroship.invoices SET subtotal_cents = 100, total_cents = 100, \
               status = 'finalized', finalized_at = NOW() WHERE id = $1",
            &[&inv],
        )
        .await
        .expect("finalize");
    let provider_invoice_id = format!("in_native_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.billing_provider_refs \
               (invoice_id, provider, ref_kind, external_id) \
             VALUES ($1, 'stripe', 'invoice', $2)",
            &[&inv, &provider_invoice_id],
        )
        .await
        .expect("insert provider invoice ref");

    // The exact resolution `lookup_invoice_id` performs.
    let resolved: Option<String> = client
        .query(
            "SELECT r.external_id \
             FROM zeroship.invoices i \
             JOIN zeroship.billing_provider_refs r ON r.invoice_id = i.id \
             WHERE i.creator_id = $1 AND i.period = $2::date \
               AND i.status = 'finalized' \
               AND r.provider = 'stripe' AND r.ref_kind = 'invoice'",
            &[&creator, &period],
        )
        .await
        .expect("lookup")
        .first()
        .map(|r| r.get::<_, String>("external_id"));
    assert_eq!(
        resolved.as_deref(),
        Some(provider_invoice_id.as_str()),
        "the Native invoice verb resolves the finalized provider id via the side-table join",
    );

    // A still-DRAFT invoice must NOT resolve (the status='finalized' gate).
    let (creator2, _a2) = seed_creator_app(&client).await;
    let draft = claim_draft(&client, creator2).await;
    client
        .execute(
            "INSERT INTO zeroship.billing_provider_refs \
               (invoice_id, provider, ref_kind, external_id) \
             VALUES ($1, 'stripe', 'draft_invoice', $2)",
            &[&draft, &format!("in_draft_{}", Uuid::new_v4().simple())],
        )
        .await
        .expect("insert draft ref");
    let none: Option<String> = client
        .query(
            "SELECT r.external_id FROM zeroship.invoices i \
             JOIN zeroship.billing_provider_refs r ON r.invoice_id = i.id \
             WHERE i.creator_id = $1 AND i.period = $2::date AND i.status = 'finalized' \
               AND r.provider = 'stripe' AND r.ref_kind = 'invoice'",
            &[&creator2, &period],
        )
        .await
        .expect("lookup draft")
        .first()
        .map(|r| r.get::<_, String>("external_id"));
    assert!(none.is_none(), "a draft invoice does not resolve a finalized id");
}

// ---------------------------------------------------------------------------
// (9) Immutability triggers.
// ---------------------------------------------------------------------------

/// Finalize a fresh invoice with one line; return `inv_id`.
async fn finalized_invoice_with_line(
    client: &compio_postgres::Client,
    creator: Uuid,
    app: Uuid,
) -> String {
    let inv = claim_draft(client, creator).await;
    client
        .execute(
            "INSERT INTO zeroship.invoice_lines \
               (invoice_id, app_id, included_units, fx_pico_cents_per_unit, base_fee_cents, \
                amount_cents, usage_snapshot, weights_snapshot) \
             VALUES ($1, $2, 0, 1000, 0, 100, '{}'::jsonb, '{}'::jsonb)",
            &[&inv, &app],
        )
        .await
        .expect("insert line");
    client
        .execute(
            "UPDATE zeroship.invoices SET subtotal_cents = 100, total_cents = 100, \
               status = 'finalized', finalized_at = NOW() WHERE id = $1",
            &[&inv],
        )
        .await
        .expect("finalize");
    inv
}

#[compio::test]
async fn finalized_invoice_rejects_nonvoid_update() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let (creator, app) = seed_creator_app(&client).await;
    let inv = finalized_invoice_with_line(&client, creator, app).await;

    // Any non-void mutation of a finalized invoice is rejected by the trigger.
    let res = client
        .execute(
            "UPDATE zeroship.invoices SET total_cents = 999, subtotal_cents = 999 WHERE id = $1",
            &[&inv],
        )
        .await;
    assert!(res.is_err(), "a finalized invoice rejects a money UPDATE (immutable)");

    // A bare status change to anything but void is rejected.
    let res2 = client
        .execute("UPDATE zeroship.invoices SET status = 'draft' WHERE id = $1", &[&inv])
        .await;
    assert!(res2.is_err(), "finalized→draft is rejected (only finalized→void is legal)");
}

#[compio::test]
async fn finalized_to_void_is_the_only_legal_transition() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let (creator, app) = seed_creator_app(&client).await;
    let inv = finalized_invoice_with_line(&client, creator, app).await;

    // finalized → void (money columns unchanged) is the ONE permitted transition.
    client
        .execute(
            "UPDATE zeroship.invoices SET status = 'void', voided_at = NOW() WHERE id = $1",
            &[&inv],
        )
        .await
        .expect("finalized→void is permitted");
    let status: String = client
        .query("SELECT status FROM zeroship.invoices WHERE id = $1", &[&inv])
        .await
        .unwrap()[0]
        .get("status");
    assert_eq!(status, "void");
}

#[compio::test]
async fn finalized_invoice_line_amount_update_is_rejected() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let (creator, app) = seed_creator_app(&client).await;
    let inv = finalized_invoice_with_line(&client, creator, app).await;

    // The reproducibility record is frozen: UPDATE of a finalized invoice's line
    // amount is rejected by the line immutability trigger.
    let res = client
        .execute(
            "UPDATE zeroship.invoice_lines SET amount_cents = 1 \
             WHERE invoice_id = $1 AND app_id = $2",
            &[&inv, &app],
        )
        .await;
    assert!(res.is_err(), "UPDATE of a finalized line's amount_cents is rejected");

    // DELETE of a finalized line is likewise rejected.
    let res_del = client
        .execute(
            "DELETE FROM zeroship.invoice_lines WHERE invoice_id = $1 AND app_id = $2",
            &[&inv, &app],
        )
        .await;
    assert!(res_del.is_err(), "DELETE of a finalized line is rejected");
}

#[compio::test]
async fn draft_invoice_lines_stay_mutable_until_finalize() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let (creator, app) = seed_creator_app(&client).await;
    let inv = claim_draft(&client, creator).await;
    client
        .execute(
            "INSERT INTO zeroship.invoice_lines \
               (invoice_id, app_id, included_units, fx_pico_cents_per_unit, base_fee_cents, \
                amount_cents, usage_snapshot, weights_snapshot) \
             VALUES ($1, $2, 0, 1000, 0, 100, '{}'::jsonb, '{}'::jsonb)",
            &[&inv, &app],
        )
        .await
        .expect("insert line");
    // While the parent is DRAFT, the reconciler may refresh the line (UPSERT).
    client
        .execute(
            "UPDATE zeroship.invoice_lines SET amount_cents = 200 \
             WHERE invoice_id = $1 AND app_id = $2",
            &[&inv, &app],
        )
        .await
        .expect("a draft invoice's line stays mutable");
    let amt: i64 = client
        .query(
            "SELECT amount_cents FROM zeroship.invoice_lines WHERE invoice_id = $1 AND app_id = $2",
            &[&inv, &app],
        )
        .await
        .unwrap()[0]
        .get("amount_cents");
    assert_eq!(amt, 200);
}
