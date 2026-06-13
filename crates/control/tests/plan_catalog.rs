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
use zeroship_control::pricing::{charge_cents, PlanPrice, PricingRule};
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
    let mut included = HashMap::new();
    included.insert("requests".to_string(), 1_000_000u64);
    let mut overage = HashMap::new();
    overage.insert(
        "requests".to_string(),
        PricingRule::Flat { rate_cents: 30, per_units: 1_000_000 },
    );
    let plan = Plan {
        id: zeroship_core::typed_id::new_plan_id(),
        name: name.to_string(),
        price: PlanPrice {
            base_fee_cents: 500,
            included,
            overage,
            spend_limit_default_cents: 5_000,
        },
        runtime: AppRuntimeLimits {
            cpu_limit_ms: Some(30_000),
            wall_timeout_ms: Some(30_000),
            heap_limit_mb: Some(256),
        },
        archived: false,
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
async fn list_skips_poison_row_but_get_is_strict() {
    // #5: a row with un-parseable JSONB is skipped by list() (warn) but a
    // direct get() of that id hard-errors. Inject a poison row via raw SQL.
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry);

    let good = seed_plan(&catalog, "good-row").await;
    // A poison plan: runtime_limits_json is a STRING, not an AppRuntimeLimits
    // object, so row_to_plan's from_value fails.
    let poison_id = zeroship_core::typed_id::new_plan_id();
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, price_model_json, included_quota_json, \
                runtime_limits_json, spend_limit_default_cents, archived) \
             VALUES ($1, 'poison', 0, '{}'::jsonb, '{}'::jsonb, '\"not-an-object\"'::jsonb, 0, false)",
            &[&poison_id],
        )
        .await
        .expect("insert poison row");

    let plans = catalog.list().await.expect("list tolerates poison row");
    assert!(plans.iter().any(|p| p.id == good.id), "good row is listed");
    assert!(!plans.iter().any(|p| p.id == poison_id), "poison row is skipped, not listed");

    // get() of the poison id is strict — hard error.
    assert!(catalog.get(&poison_id).await.is_err(), "get is strict on a poison row");
}

#[compio::test]
async fn charge_from_real_aggregates() {
    // End-to-end of the pricing path against REAL usage_aggregates rows: write
    // usage for an app, fetch the period totals via the real Metering reader,
    // price them against a real catalog plan, and assert the charge matches the
    // overage math (base + over-quota units × rate).
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let catalog = PlanCatalog::new(registry.clone());
    let owner = make_user(&client).await;

    // Plan: base 500c, 1M requests included, 30c/1M overage.
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

    // Read back the real aggregates and price them.
    let totals = metering.period_totals(&app.id, period).await.expect("totals");
    let fetched_plan = catalog.get(&plan.id).await.expect("get").expect("present");
    let breakdown = charge_cents(&fetched_plan.price, &totals);

    // 500 base + (1.5M − 1M) × 30 / 1M = 500 + 15 = 515 cents.
    assert_eq!(breakdown.base_cents, 500);
    assert_eq!(breakdown.lines.len(), 1);
    assert_eq!(breakdown.lines[0].metric, "requests");
    assert_eq!(breakdown.lines[0].billable_units, 500_000);
    assert_eq!(breakdown.lines[0].cents, 15);
    assert_eq!(breakdown.total_cents, 515);
}
