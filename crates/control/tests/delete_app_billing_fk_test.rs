//! Schema MAJOR-1 regression: `Registry::delete_app` vs the new billing FKs.
//!
//!   (a) An app with billing history (an `invoice_lines.app_id → apps RESTRICT`
//!       row) is NOT hard-deletable: `delete_app` returns a TYPED
//!       `RegistryError::Conflict`, not an opaque raw DB error — consistent with
//!       the anonymize-don't-delete financial-history posture.
//!   (b) An app with a CUSTOM metric (`billing_metrics.owner_app → apps CASCADE`)
//!       + `usage_aggregates` rows (no invoice) is deletable: the CASCADE×RESTRICT
//!       ordering hazard is resolved by `usage_aggregates.metric → billing_metrics
//!       NO ACTION DEFERRABLE INITIALLY DEFERRED` (end-of-statement check), so the
//!       concurrent `usage_aggregates.app_id` CASCADE removes the rows FIRST.
//!
//! FAITHFUL: drives the REAL `Registry::create_app` / `Registry::delete_app`
//! against a live, migrated Postgres. Gated on `CONTROL_TEST_DB`; silent skip.

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_control::registry::RegistryError;
use zeroship_control::Registry;

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

async fn pg(db_url: &str) -> Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

const FX_SCALE: i64 = 1_000_000_000_000;

/// Seed a user (owner) + a plan, returning `(owner_id, plan_id)`.
async fn seed_owner_and_plan(client: &Client) -> (Uuid, String) {
    let email = format!("del-fk-{}@test.invalid", Uuid::new_v4().simple());
    let owner: Uuid = client
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, 'del-fk') RETURNING id",
            &[&email],
        )
        .await
        .expect("insert user")[0]
        .get("id");
    let plan_id = format!("pln_delfk_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'delfk', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 0)",
            &[&plan_id, &FX_SCALE],
        )
        .await
        .expect("seed plan");
    (owner, plan_id)
}

// ---------------------------------------------------------------------------
// (a) A billed app is NOT hard-deletable — typed Conflict, not a raw DB error.
// ---------------------------------------------------------------------------

#[compio::test]
async fn delete_app_with_invoice_history_returns_typed_conflict() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let (owner, plan_id) = seed_owner_and_plan(&client).await;

    let app = registry
        .create_app(&format!("delfka-{}", Uuid::new_v4().simple()), &plan_id, &owner)
        .await
        .expect("create app");

    // Give the owner financial history: a finalized invoice with a line for `app`.
    client
        .execute(
            "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) \
             ON CONFLICT (creator_id) DO NOTHING",
            &[&owner],
        )
        .await
        .expect("creator_billing");
    let inv = zeroship_core::typed_id::new_invoice_id();
    let period = first_of_this_month();
    // Seed as DRAFT first so the line INSERT is permitted (the immutability trigger
    // — Schema MAJOR-2 — blocks INSERTs into a finalized invoice), then finalize.
    client
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv, &owner, &period],
        )
        .await
        .expect("seed draft invoice");
    client
        .execute(
            // PR-4 reshape: segment_no + plan_id (NOT NULL). A single full-period
            // line is segment_no=0 with the app's current plan (subquery FK).
            "INSERT INTO zeroship.invoice_lines \
               (invoice_id, app_id, segment_no, plan_id, included_units, \
                fx_pico_cents_per_unit, base_fee_cents, amount_cents, usage_snapshot, weights_snapshot) \
             VALUES ($1, $2, 0, (SELECT plan_id FROM zeroship.apps WHERE id = $2), \
                     0, 1000, 0, 100, '{}'::jsonb, '{}'::jsonb)",
            &[&inv, &app.id],
        )
        .await
        .expect("seed invoice line (financial history)");
    client
        .execute(
            "UPDATE zeroship.invoices SET subtotal_cents = 100, total_cents = 100, \
               status = 'finalized', finalized_at = NOW() WHERE id = $1",
            &[&inv],
        )
        .await
        .expect("finalize invoice");

    // delete_app must return a TYPED Conflict (not a raw Database error).
    let err = registry.delete_app(&app.id).await.expect_err("billed app is not hard-deletable");
    match err {
        RegistryError::Conflict(msg) => {
            assert!(msg.contains("billing history"), "conflict message names the cause: {msg}");
        }
        other => panic!("expected RegistryError::Conflict, got {other:?}"),
    }

    // The app row still exists (the delete was refused cleanly, not half-applied).
    let still_there: i64 = client
        .query("SELECT COUNT(*)::bigint AS n FROM zeroship.apps WHERE id = $1", &[&app.id])
        .await
        .unwrap()[0]
        .get("n");
    assert_eq!(still_there, 1, "the billed app is preserved (anonymize-don't-delete)");
}

// ---------------------------------------------------------------------------
// (b) A custom-metric app with usage_aggregates (no invoice) IS deletable —
//     the deferred metric FK lets the app_id CASCADE win the ordering race.
//
// NOTE on RED reproducibility: the "immediate-RESTRICT abort" the critic feared
// is ORDER-DEPENDENT. On PostgreSQL 17.7 the `usage_aggregates.app_id` CASCADE is
// evaluated before the `usage_aggregates.metric` RESTRICT, so this same-app delete
// ALSO succeeds under the old immediate RESTRICT — the hazard does not reproduce
// here. The `ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED` change makes the
// outcome order-INDEPENDENT (future-proof against a PG version that orders the
// cascade the other way) while preserving the "no dangling aggregate metric"
// guarantee (the FK still rejects an uncataloged metric, just at statement end).
// This test is the GREEN guard that the delete + cascade stays clean.
// ---------------------------------------------------------------------------

#[compio::test]
async fn delete_app_with_custom_metric_and_aggregates_succeeds() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let (owner, plan_id) = seed_owner_and_plan(&client).await;

    let app = registry
        .create_app(&format!("delfkb-{}", Uuid::new_v4().simple()), &plan_id, &owner)
        .await
        .expect("create app");

    // A CUSTOM metric owned by this app (billing_metrics.owner_app → apps CASCADE).
    let metric = format!("custom_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.billing_metrics (metric, kind, unit, owner_app) \
             VALUES ($1, 'custom', 'unit', $2)",
            &[&metric, &app.id],
        )
        .await
        .expect("seed custom metric");
    // A usage_aggregates row referencing BOTH the app (CASCADE) and the custom
    // metric (deferred NO ACTION). On delete_app the app_id CASCADE must remove
    // this row before the deferred metric FK is checked at statement end.
    client
        .execute(
            "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
             VALUES ($1, $2::date, $3, 42)",
            &[&app.id, &first_of_this_month(), &metric],
        )
        .await
        .expect("seed usage aggregate");

    // The delete must SUCCEED (no immediate-RESTRICT abort from the metric FK).
    let deleted = registry
        .delete_app(&app.id)
        .await
        .expect("delete_app succeeds despite the custom-metric CASCADE×deferred-FK ordering");
    assert!(deleted, "the app row was deleted");

    // Both the app's custom metric and its aggregate rows are gone (CASCADE).
    let metric_left: i64 = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.billing_metrics WHERE owner_app = $1",
            &[&app.id],
        )
        .await
        .unwrap()[0]
        .get("n");
    assert_eq!(metric_left, 0, "the custom metric cascaded away with the app");
    let agg_left: i64 = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.usage_aggregates WHERE app_id = $1",
            &[&app.id],
        )
        .await
        .unwrap()[0]
        .get("n");
    assert_eq!(agg_left, 0, "the aggregate rows cascaded away with the app");
}

fn first_of_this_month() -> chrono::NaiveDate {
    use chrono::Datelike;
    let now = chrono::Utc::now().date_naive();
    chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap()
}
