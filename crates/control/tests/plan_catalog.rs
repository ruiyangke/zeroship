//! Integration tests for the plan catalog + plan-validated app CRUD against a
//! live, migrated Postgres (changeset 0038: `zeroship.plans` + the
//! `apps.plan_id → plans.id` FK).
//!
//! These drive the REAL `PlanCatalog` / `Registry` paths — not stubs — so they
//! prove the server-side plan gate (CT-A1: free-text `plan_id` self-escalation
//! is rejected) and that `get_versions` derives runtime limits from the catalog
//! row rather than a hardcoded plan-name table.
//!
//! Set `CONTROL_TEST_DB` to run; tests silently skip otherwise (CI without a DB
//! stays green). The pure tier math is unit-tested DB-free in `src/pricing.rs`.

use std::collections::HashMap;

use compio_postgres::{connect, NoTls};
use uuid::Uuid;

use zeroship_control::plan_catalog::{Plan, PlanCatalog};
use zeroship_control::pricing::{charge_cents, MetricWeight, MetricWeights, PlanPrice, FX_SCALE};
use zeroship_control::Registry;
use zeroship_core::types::AppRuntimeLimits;

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

/// Seed a unique unarchived plan into the catalog; return it. Each test mints a
/// fresh `pln_…` id so parallel runs never collide.
async fn seed_plan(catalog: &PlanCatalog, name: &str) -> Plan {
    let plan = Plan {
        id: zeroship_core::typed_id::new_plan_id(),
        name: name.to_string(),
        price: PlanPrice {
            base_fee_cents: 500,
            included_units: 1_000_000,
            // 1 cent/CU (explicit so the round-trip pins a concrete fx, not the
            // global default-inheriting None).
            fx_pico_cents_per_unit: Some(FX_SCALE as u64),
            spend_limit_default_cents: 5_000,
        },
        runtime: AppRuntimeLimits {
            cpu_limit_ms: Some(30_000),
            wall_timeout_ms: Some(30_000),
            heap_limit_mb: Some(256),
        },
        archived: false,
        assignable_by_creator: false,
    };
    catalog.upsert(&plan, Some(plan.archived)).await.expect("upsert plan")
}

/// Mint a user row so `create_app`'s owner-membership insert has a valid FK.
async fn make_user(client: &compio_postgres::Client) -> Uuid {
    let id = Uuid::now_v7();
    let email = format!("plan-test-{id}@example.com");
    client
        .execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2, $3)",
            &[&id, &email, &"Plan Test"],
        )
        .await
        .expect("insert user");
    id
}

#[compio::test]
async fn upsert_and_get_round_trips_pure_types() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let _client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry);

    let plan = seed_plan(&catalog, "round-trip").await;
    let fetched = catalog.get(&plan.id).await.expect("get").expect("present");
    assert_eq!(fetched, plan, "JSONB columns round-trip the pure types verbatim");

    // upsert again (same id) updates in place — idempotent.
    let mut updated = plan.clone();
    updated.name = "round-trip-2".to_string();
    catalog.upsert(&updated, Some(updated.archived)).await.expect("re-upsert");
    let again = catalog.get(&plan.id).await.expect("get2").expect("present2");
    assert_eq!(again.name, "round-trip-2");
}

#[compio::test]
async fn create_app_with_unknown_plan_id_is_rejected() {
    // CT-A1: an app can no longer pick a plan that is not in the catalog. The
    // server-side gate returns a clean InvalidInput (NOT a raw FK violation),
    // and NO app row is left behind.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let owner = make_user(&client).await;

    let name = format!("ct-a1-{}", Uuid::new_v4().simple());
    let bogus = "enterprise"; // free-text id that is NOT a catalog plan
    let err = registry
        .create_app(&name, bogus, &owner)
        .await
        .expect_err("unknown plan must be rejected");
    match err {
        zeroship_control::registry::RegistryError::InvalidInput(msg) => {
            assert!(msg.contains("unknown plan"), "clean InvalidInput, got: {msg}");
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }

    // No app row was created (the txn rolled back / never committed).
    let rows = client
        .query("SELECT 1 FROM zeroship.apps WHERE name = $1", &[&name])
        .await
        .expect("query apps");
    assert!(rows.is_empty(), "rejected create must leave no app row");
}

#[compio::test]
async fn create_app_with_real_plan_id_succeeds() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry.clone());
    let owner = make_user(&client).await;
    let plan = seed_plan(&catalog, "real-plan").await;

    let name = format!("ok-{}", Uuid::new_v4().simple());
    let record = registry
        .create_app(&name, &plan.id, &owner)
        .await
        .expect("create with a real plan succeeds");
    assert_eq!(record.plan_id, plan.id);
}

#[compio::test]
async fn set_plan_to_archived_plan_is_rejected() {
    // An archived plan stays resolvable (historical FKs) but cannot be ASSIGNED
    // to an app via set_plan.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry.clone());
    let owner = make_user(&client).await;

    let live = seed_plan(&catalog, "live").await;
    let archived = seed_plan(&catalog, "to-archive").await;
    assert!(catalog.archive(&archived.id).await.expect("archive"));

    let name = format!("setplan-{}", Uuid::new_v4().simple());
    let app = registry
        .create_app(&name, &live.id, &owner)
        .await
        .expect("create on live plan");

    let err = registry
        .set_plan(&app.id, &archived.id)
        .await
        .expect_err("set_plan to an archived plan must be rejected");
    match err {
        zeroship_control::registry::RegistryError::InvalidInput(msg) => {
            assert!(msg.contains("archived"), "clean InvalidInput, got: {msg}");
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }

    // The app's plan is unchanged.
    let still = registry.get_app(&app.id).await.expect("get").expect("present");
    assert_eq!(still.plan_id, live.id, "rejected set_plan must not change the plan");

    // set_plan to the live plan still works.
    assert!(registry.set_plan(&app.id, &live.id).await.expect("set live"));
}

#[compio::test]
async fn get_versions_derives_limits_from_catalog_not_hardcode() {
    // The deleted `runtime_limits_for_plan` hardcoded limits by plan NAME. Now
    // limits come from the plan ROW's runtime_limits_json. Seed a plan with a
    // bespoke limit matrix and assert get_versions reflects it (a hardcoded
    // table keyed on a plan name would never produce these exact values for a
    // random pln_ id).
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry.clone());
    let owner = make_user(&client).await;

    let mut plan = seed_plan(&catalog, "bespoke-limits").await;
    plan.runtime = AppRuntimeLimits {
        cpu_limit_ms: Some(12_345),
        wall_timeout_ms: Some(23_456),
        heap_limit_mb: Some(177),
    };
    catalog.upsert(&plan, Some(plan.archived)).await.expect("upsert bespoke limits");

    let name = format!("limits-{}", Uuid::new_v4().simple());
    let app = registry
        .create_app(&name, &plan.id, &owner)
        .await
        .expect("create");

    let versions = registry.get_versions().await.expect("get_versions");
    let info = versions.get(&app.id).expect("app in versions");
    assert_eq!(info.plan_id, plan.id);
    assert_eq!(info.runtime.cpu_limit_ms, Some(12_345), "from the catalog row, not a name table");
    assert_eq!(info.runtime.wall_timeout_ms, Some(23_456));
    assert_eq!(info.runtime.heap_limit_mb, Some(177));
}

#[compio::test]
async fn upsert_with_none_archived_preserves_existing_archived() {
    // REGRESSION (#11): an archived plan, re-upserted with `archived = None`
    // (the PUT-without-archived case), MUST STAY archived. The old code set
    // `archived = EXCLUDED.archived` from a `#[serde(default)] -> false`, so a
    // name edit silently UN-archived the plan. Now `None` ⇒ COALESCE-preserve.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let _client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry);

    let plan = seed_plan(&catalog, "to-stay-archived").await;
    assert!(catalog.archive(&plan.id).await.expect("archive"));
    assert!(
        catalog.get(&plan.id).await.expect("get").expect("present").archived,
        "precondition: archived"
    );

    // PUT a name change with NO archived field (archived = None) — must NOT
    // resurrect the plan.
    let mut renamed = plan.clone();
    renamed.name = "renamed-while-archived".to_string();
    let written = catalog.upsert(&renamed, None).await.expect("upsert none-archived");
    assert!(written.archived, "name edit with archived=None must NOT un-archive");
    assert_eq!(written.name, "renamed-while-archived", "the name DID change");

    let fetched = catalog.get(&plan.id).await.expect("get").expect("present");
    assert!(fetched.archived, "still archived after the read-back");

    // Explicit Some(false) is the deliberate un-archive path.
    let unarchived = catalog.upsert(&renamed, Some(false)).await.expect("explicit un-archive");
    assert!(!unarchived.archived, "Some(false) explicitly un-archives");
}

#[compio::test]
async fn set_plan_guards_archive_in_one_statement() {
    // #8: set_plan is race-free in a single guarded UPDATE. Assigning a plan
    // archived just before the call is rejected with a typed InvalidInput, and
    // the app's plan is unchanged. (A direct test of the single-statement guard;
    // the true concurrent race can't be deterministically forced in a unit test,
    // but the guard is what closes the window.)
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry.clone());
    let owner = make_user(&client).await;

    let live = seed_plan(&catalog, "live-8").await;
    let target = seed_plan(&catalog, "target-8").await;
    let name = format!("toctou-{}", Uuid::new_v4().simple());
    let app = registry.create_app(&name, &live.id, &owner).await.expect("create");

    // Archive the target, then attempt to assign it: the guarded UPDATE matches
    // 0 rows and the disambiguation returns InvalidInput("archived").
    assert!(catalog.archive(&target.id).await.expect("archive"));
    let err = registry
        .set_plan(&app.id, &target.id)
        .await
        .expect_err("assigning an archived plan must be rejected");
    match err {
        zeroship_control::registry::RegistryError::InvalidInput(msg) => {
            assert!(msg.contains("archived"), "got: {msg}");
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
    let still = registry.get_app(&app.id).await.expect("get").expect("present");
    assert_eq!(still.plan_id, live.id, "rejected set_plan must not change the plan");

    // set_plan to a non-existent app returns Ok(false), NOT an error.
    let ghost = Uuid::now_v7();
    assert!(
        !registry.set_plan(&ghost, &live.id).await.expect("no such app -> Ok(false)"),
        "no such app yields Ok(false)"
    );
}

#[compio::test]
async fn poison_runtime_limits_still_prices_via_both_list_and_get() {
    // MAJOR-2 REGRESSION: a plan row with un-parseable `runtime_limits_json` must
    // STILL be priced by BOTH paths — `list()` (spend enforcement) AND `get()`
    // (billing reconcile). Pricing needs only the scalar price columns, not the
    // runtime limits, so a poison row falls back to free-tier runtime limits and
    // is RETURNED with its real price intact.
    //
    // Pre-fix: `list()` SKIPPED the poison row (app ran UNCAPPED) and `get()`
    // HARD-ERRORED (creator's whole bill failed) — so a poison plan made an app
    // both uncapped AND unbilled. The two paths now AGREE: both return it.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry);

    let good = seed_plan(&catalog, "good-row").await;
    // A poison plan: runtime_limits_json is a STRING, not an AppRuntimeLimits
    // object, so row_to_plan's from_value fails — but its PRICE columns are real
    // (base 700c, FX = 1 cent/CU) and must survive.
    let poison_id = zeroship_core::typed_id::new_plan_id();
    let fx = FX_SCALE as i64; // 1 cent/CU
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents, archived, \
                assignable_by_creator) \
             VALUES ($1, 'poison', 700, 0, $2, '\"not-an-object\"'::jsonb, 0, false, false)",
            &[&poison_id, &fx],
        )
        .await
        .expect("insert poison row");

    // list() RETURNS the poison row (priced), not skipped.
    let plans = catalog.list().await.expect("list tolerates poison row");
    assert!(plans.iter().any(|p| p.id == good.id), "good row is listed");
    let listed = plans
        .iter()
        .find(|p| p.id == poison_id)
        .expect("poison row is STILL listed (priceable), not skipped");
    assert_eq!(listed.price.base_fee_cents, 700, "poison row keeps its real price (list)");

    // get() of the poison id is ALSO tolerant now — returns the priced plan
    // (free-tier runtime fallback), no hard error.
    let got = catalog
        .get(&poison_id)
        .await
        .expect("get tolerates poison row (prices it)")
        .expect("poison plan is present");
    assert_eq!(got.price.base_fee_cents, 700, "poison row keeps its real price (get)");
    assert_eq!(
        got.runtime,
        zeroship_core::types::FREE_TIER_RUNTIME_LIMITS,
        "poison runtime_limits_json falls back to the conservative free-tier limits",
    );
}

#[compio::test]
async fn charge_from_real_aggregates_uses_weight_table() {
    // End-to-end of the CU pricing path against REAL usage_aggregates rows +
    // the REAL global `metric_weights` table: write usage for an app, fetch the
    // period totals via the real Metering reader, load the global weight table
    // via the real PricingStore, price the plan, and assert `total_cents`.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry.clone());
    let owner = make_user(&client).await;

    // Make the weight for `requests` deterministic for this assertion (1 CU per
    // request), independent of any future seed re-tuning.
    client
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('requests', 1, 1) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
            &[],
        )
        .await
        .expect("upsert requests weight");

    // Plan: base 500c, 1M CU included, FX = 1 cent/CU (explicit).
    let plan = seed_plan(&catalog, "charge").await;
    let name = format!("charge-{}", Uuid::new_v4().simple());
    let app = registry.create_app(&name, &plan.id, &owner).await.expect("create");

    // Write 1.5M requests into the current period via the real Metering ingest.
    let metering = zeroship_control::metering::Metering::new(registry.clone());
    let period = zeroship_control::metering::current_period_start_unix();
    let mut counters = HashMap::new();
    counters.insert(
        app.id,
        zeroship_core::types::AppUsage { requests: 1_500_000, ..Default::default() },
    );
    let report = zeroship_core::types::UsageReport {
        worker_id: format!("w-{}", Uuid::new_v4()),
        report_id: Uuid::now_v7(),
        sequence: 1,
        counters,
    };
    metering.ingest_at(&report, period).await.expect("ingest");

    // Read back the real aggregates, the real global weight table, and price.
    let totals = metering.period_totals(&app.id, period).await.expect("totals");
    let fetched_plan = catalog.get(&plan.id).await.expect("get").expect("present");
    let pricing = zeroship_control::pricing_store::PricingStore::new(registry.clone());
    let weights = pricing.weights().await.expect("weights");
    let default_fx = pricing.default_fx_pico_cents_per_unit().await.expect("default fx");
    let price = fetched_plan.price.with_effective_fx(default_fx);
    let breakdown = charge_cents(&price, &totals, &weights).expect("charge");

    // 1.5M requests × 1 CU = 1.5M CU; included 1M ⇒ 0.5M billable CU.
    // 0.5M CU × 1 cent = 500_000c overage + 500c base = 500_500c.
    assert_eq!(breakdown.base_cents, 500);
    assert_eq!(breakdown.total_units, 1_500_000);
    assert_eq!(breakdown.billable_units, 500_000);
    assert_eq!(breakdown.total_cents, 500_500);
}

#[compio::test]
async fn charge_uses_only_db_weight_table_and_default_fx() {
    // Prove the global weight table + default FX are actually loaded from the DB
    // (not a hardcoded const): a metric with NO weight row contributes 0 CU, and
    // a plan with `fx = None` prices at the seeded global default.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let _client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let pricing = zeroship_control::pricing_store::PricingStore::new(registry.clone());
    let weights = pricing.weights().await.expect("weights");
    let default_fx = pricing.default_fx_pico_cents_per_unit().await.expect("fx");
    assert!(default_fx.is_some(), "the global default FX is seeded");
    let mut t = MetricWeights::new();
    t.insert("requests".to_string(), MetricWeight { units_per_op: 1, per_units: 1 });
    // sanity: the loaded table is non-empty (seeded platform counters)
    assert!(weights.contains_key("requests"), "platform-counter weight is seeded");
}

#[compio::test]
async fn upsert_hard_errors_on_out_of_range_price_not_silent_clamp() {
    // MINOR (write-path i64 clamps) REGRESSION: a plan write with an
    // out-of-range price (here included_units = u64::MAX, above the i64 BIGINT
    // ceiling) must be a HARD ERROR at `upsert` — not a silent clamp to i64::MAX.
    // `upsert` calls `plan.price.validate()` as defense in depth so even a direct
    // (non-HTTP) caller cannot land a clamped plan.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let _client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry);

    let mut plan = seed_plan(&catalog, "range-ok").await;
    // Now mutate to an out-of-range included_units and re-upsert: must error.
    plan.price.included_units = u64::MAX;
    let res = catalog.upsert(&plan, Some(false)).await;
    assert!(
        res.is_err(),
        "an out-of-range included_units must be a hard error at upsert, not a silent i64 clamp",
    );
}

#[compio::test]
async fn below_floor_global_fx_rejected_by_check_and_loader_fails_closed() {
    // MAJOR-3 REGRESSION. Two arms:
    //   (a) the DB CHECK on `pricing_config.fx_pico_cents_per_unit >= 1000`
    //       (the MIN_FX floor) rejects a fat-fingered near-zero global FX at the
    //       source.
    //   (b) defense in depth: even if a below-floor value somehow lands (here we
    //       drop the CHECK to inject one), the loader treats it as UNRESOLVED
    //       (returns None) so the sweep fails CLOSED — it does NOT coerce a
    //       near-zero/zero global FX to Some(0) and silently price ALL overage to
    //       ~$0 platform-wide.
    // Single-threaded (the harness runs --test-threads=1), and we RESTORE the
    // seeded global row + CHECK before returning so other tests see a sane FX.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let pricing = zeroship_control::pricing_store::PricingStore::new(registry.clone());

    // Snapshot the seeded global FX so we can restore it.
    let seeded: i64 = client
        .query_one(
            "SELECT fx_pico_cents_per_unit FROM zeroship.pricing_config WHERE id = 'global'",
            &[],
        )
        .await
        .expect("read seeded fx")
        .get("fx_pico_cents_per_unit");

    // (a) The CHECK rejects a below-floor UPDATE.
    let blocked = client
        .execute(
            "UPDATE zeroship.pricing_config SET fx_pico_cents_per_unit = 0 WHERE id = 'global'",
            &[],
        )
        .await;
    assert!(blocked.is_err(), "below-floor global FX (0) must be rejected by the CHECK (MAJOR-3)");

    // (b) Bypass the CHECK to inject a below-floor value, then prove the loader
    // fails closed (None), then restore the CHECK + seeded value.
    client
        .execute(
            "ALTER TABLE zeroship.pricing_config DROP CONSTRAINT \
             pricing_config_fx_pico_cents_per_unit_check",
            &[],
        )
        .await
        .expect("drop check");
    client
        .execute(
            "UPDATE zeroship.pricing_config SET fx_pico_cents_per_unit = 0 WHERE id = 'global'",
            &[],
        )
        .await
        .expect("inject below-floor fx");

    let fx = pricing
        .default_fx_pico_cents_per_unit()
        .await
        .expect("loader runs");
    // RESTORE before asserting so a panic can't leave the global row poisoned.
    client
        .execute(
            "UPDATE zeroship.pricing_config SET fx_pico_cents_per_unit = $1 WHERE id = 'global'",
            &[&seeded],
        )
        .await
        .expect("restore seeded fx");
    client
        .execute(
            "ALTER TABLE zeroship.pricing_config ADD CONSTRAINT \
             pricing_config_fx_pico_cents_per_unit_check CHECK (fx_pico_cents_per_unit >= 1000)",
            &[],
        )
        .await
        .expect("restore check");

    assert_eq!(
        fx, None,
        "a below-floor global FX must resolve to None (unresolved → sweep fails closed), \
         NOT Some(0) which would silently bill all overage at $0",
    );
}

#[compio::test]
async fn metric_weights_rejects_negative_units_per_op() {
    // MINOR-2 REGRESSION: the 0041 CHECK (units_per_op >= 0) makes a negative
    // weight unrepresentable at the source — a negative weight would credit CU
    // (nonsensical). Without the CHECK this INSERT would succeed.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let metric = format!("neg_w_{}", Uuid::now_v7().simple());
    let res = client
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ($1, -1, 1)",
            &[&metric],
        )
        .await;
    assert!(
        res.is_err(),
        "a negative units_per_op must be rejected by the CHECK constraint (MINOR-2)"
    );
    // Clean up any row that somehow landed (it shouldn't have).
    let _ = client
        .execute("DELETE FROM zeroship.metric_weights WHERE metric = $1", &[&metric])
        .await;
}
