//! Live PostgreSQL regression coverage for the app archive boundary.
//!
//! Archive must retain every app-attributed fact that hard delete cascaded,
//! including immutable billing history. It must still remove the app from the
//! gateway route projection and it must be reversible without releasing the
//! app's routable name.

use crate::common;

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_bundle::Manifest;
use zeroship_control::registry::RegistryError;
use zeroship_control::Registry;
use zeroship_core::UserId;

fn db_url() -> String {
    common::require_control_db()
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

#[compio::test]
async fn archive_migration_removes_hard_delete_capability_and_keeps_the_worker_out_of_the_catalog() {
    let url = db_url();
    let client = pg(&url).await;
    let row = client
        .query_one(
            "SELECT \
                 has_table_privilege('zeroship_control', 'zeroship.apps', 'DELETE') \
                     AS control_can_delete, \
                 has_table_privilege('zeroship_control', 'zeroship.apps', 'UPDATE') \
                     AS control_can_update, \
                 has_column_privilege( \
                     'zeroship_worker', 'zeroship.apps', 'archived_at', 'SELECT' \
                 ) AS worker_can_read_archive",
            &[],
        )
        .await
        .expect("inspect archive privileges");
    assert!(!row.get::<_, bool>("control_can_delete"));
    assert!(row.get::<_, bool>("control_can_update"));
    // INVERTED by the legacy-path removal, and deliberately so. The worker used
    // to read `archived_at` as its own archive fence, which required reaching
    // the platform catalog. It no longer reaches that catalog at all: the
    // cutover revoked `USAGE ON SCHEMA zeroship` from `zeroship_worker`, and
    // `crates/zeroship-worker/src/db_posture.rs` refuses to boot against a
    // login that can see the platform schema. A worker that could still read
    // this column would mean that revocation had been undone.
    assert!(!row.get::<_, bool>("worker_can_read_archive"));

    drop(client);
    common::drain_pg().await;
}

async fn seed_owner_and_plan(client: &Client) -> (UserId, String) {
    let email = format!("archive-{}@test.invalid", Uuid::new_v4().simple());
    let owner = UserId::mint();
    client
        .execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2, 'archive')",
            &[&owner.as_str(), &email],
        )
        .await
        .expect("insert user");
    let plan_id = format!("pln_archive_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'archive', 0, 0, $2, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', 0)",
            &[&plan_id, &FX_SCALE],
        )
        .await
        .expect("seed plan");
    (owner, plan_id)
}

#[compio::test]
async fn archive_preserves_finalized_invoice_history() {
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let (owner, plan_id) = seed_owner_and_plan(&client).await;
    let app = registry
        .create_app(
            &format!("archive-inv-{}", Uuid::new_v4().simple()),
            &plan_id,
            &owner,
            None,
            None,
        )
        .await
        .expect("create app");

    // The billing subject is the app's OWN organization - the personal one
    // `create_app` provisioned - read back off the row rather than assumed, so
    // the invoice below is attached to the app whose archival is under test.
    let organization: String = client
        .query(
            "SELECT organization_id FROM zeroship.apps WHERE id = $1",
            &[&app.id.as_str()],
        )
        .await
        .expect("read app organization")[0]
        .get("organization_id");
    client
        .execute(
            "INSERT INTO zeroship.organization_billing (organization_id) VALUES ($1) \
             ON CONFLICT (organization_id) DO NOTHING",
            &[&organization],
        )
        .await
        .expect("organization_billing");
    let invoice_id = zeroship_core::typed_id::new_invoice_id();
    let period = first_of_this_month();
    client
        .execute(
            "INSERT INTO zeroship.invoices (id, organization_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&invoice_id, &organization, &period],
        )
        .await
        .expect("seed draft invoice");
    client
        .execute(
            "INSERT INTO zeroship.invoice_lines \
               (invoice_id, app_id, segment_no, plan_id, included_units, \
                fx_pico_cents_per_unit, base_fee_cents, amount_cents, usage_snapshot, weights_snapshot) \
             VALUES ($1, $2, 0, $3, 0, 1000, 0, 100, '{}'::jsonb, '{}'::jsonb)",
            &[&invoice_id, &app.id.as_str(), &plan_id],
        )
        .await
        .expect("seed invoice line");
    client
        .execute(
            "UPDATE zeroship.invoices SET subtotal_cents = 100, total_cents = 100, \
               status = 'finalized', finalized_at = NOW() WHERE id = $1",
            &[&invoice_id],
        )
        .await
        .expect("finalize invoice");

    let archived = registry
        .archive_app(&app.id)
        .await
        .expect("archive billed app")
        .expect("app exists");
    assert!(archived.archived_at.is_some());
    assert_eq!(count(&client, "zeroship.apps", "id", &app.id).await, 1);
    assert_eq!(
        count(&client, "zeroship.invoice_lines", "app_id", &app.id).await,
        1
    );

    drop(client);
    drop(registry);
    common::drain_pg().await;
}

#[compio::test]
async fn archive_preserves_custom_metric_and_usage() {
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let (owner, plan_id) = seed_owner_and_plan(&client).await;
    let app = registry
        .create_app(
            &format!("archive-use-{}", Uuid::new_v4().simple()),
            &plan_id,
            &owner,
            None,
            None,
        )
        .await
        .expect("create app");
    let metric = format!("custom_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.billing_metrics (metric, kind, unit, owner_app) \
             VALUES ($1, 'custom', 'unit', $2)",
            &[&metric, &app.id.as_str()],
        )
        .await
        .expect("seed custom metric");
    client
        .execute(
            "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
             VALUES ($1, $2::date, $3, 42)",
            &[&app.id.as_str(), &first_of_this_month(), &metric],
        )
        .await
        .expect("seed usage aggregate");

    registry
        .archive_app(&app.id)
        .await
        .expect("archive app")
        .expect("app exists");
    assert_eq!(
        count(&client, "zeroship.billing_metrics", "owner_app", &app.id).await,
        1
    );
    assert_eq!(
        count(&client, "zeroship.usage_aggregates", "app_id", &app.id).await,
        1
    );

    drop(client);
    drop(registry);
    common::drain_pg().await;
}

#[compio::test]
async fn archive_with_plan_change_history_is_idempotent_and_reversible() {
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let (owner, plan_id) = seed_owner_and_plan(&client).await;
    let name = format!("archive-pce-{}", Uuid::new_v4().simple());
    let app = registry
        .create_app(&name, &plan_id, &owner, None, None)
        .await
        .expect("create app");
    let event_id = format!("pce_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.plan_change_events \
               (id, app_id, period, from_plan_id, to_plan_id, usage_at_change) \
             VALUES ($1, $2, $3::date, NULL, $4, '{}'::jsonb)",
            &[
                &event_id,
                &app.id.as_str(),
                &first_of_this_month(),
                &plan_id,
            ],
        )
        .await
        .expect("seed immutable plan-change history");

    let first = registry
        .archive_app(&app.id)
        .await
        .expect("archive app")
        .expect("app exists");
    let retry = registry
        .archive_app(&app.id)
        .await
        .expect("retry archive")
        .expect("app exists");
    assert_eq!(
        first.archived_at, retry.archived_at,
        "retry preserves first archive time"
    );
    assert!(!registry
        .get_routes()
        .await
        .expect("routes")
        .contains_key(&app.id));
    assert!(
        registry
            .get_versions()
            .await
            .expect("versions")
            .contains_key(&app.id),
        "archive must not signal database teardown to workers"
    );
    assert_eq!(
        count(&client, "zeroship.plan_change_events", "app_id", &app.id).await,
        1
    );
    assert!(
        matches!(
            registry.create_app(&name, &plan_id, &owner, None, None).await,
            Err(RegistryError::AlreadyExists(_))
        ),
        "an archived app retains its routable name"
    );

    let restored = registry
        .unarchive_app(&app.id)
        .await
        .expect("restore app")
        .expect("app exists");
    assert!(restored.archived_at.is_none());
    assert!(registry
        .get_routes()
        .await
        .expect("routes")
        .contains_key(&app.id));

    drop(client);
    drop(registry);
    common::drain_pg().await;
}

#[compio::test]
async fn archived_app_can_stage_a_deploy_without_becoming_routable() {
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let (owner, plan_id) = seed_owner_and_plan(&client).await;
    let app = registry
        .create_app(
            &format!("archive-stage-{}", Uuid::new_v4().simple()),
            &plan_id,
            &owner,
            None,
            None,
        )
        .await
        .expect("create app");
    let old_hash = common::deployments::deploy(
        &registry,
        &app.id,
        &owner,
        common::deployments::labelled("before-archive"),
    )
    .await
    .expect("set initial deploy")
    .result()
    .deploy_hash
    .clone();
    let initial_route = registry
        .get_routes()
        .await
        .expect("initial routes")
        .remove(&app.id)
        .expect("initial app route");
    assert_eq!(
        initial_route.deploy_hash.as_deref(),
        Some(old_hash.as_str())
    );

    registry
        .archive_app(&app.id)
        .await
        .expect("archive app")
        .expect("app exists");
    assert!(
        !registry
            .get_routes()
            .await
            .expect("archived routes")
            .contains_key(&app.id),
        "archive must remove the app from the gateway projection"
    );

    let staged = common::deployments::deploy(
        &registry,
        &app.id,
        &owner,
        common::deployments::labelled("staged-while-archived"),
    )
    .await
    .expect("stage deploy while archived");
    assert_eq!(
        staged.result().lifecycle_revision,
        None,
        "a deploy while archived stages code without publishing an activation"
    );
    let staged_hash = staged.result().deploy_hash.clone();
    assert!(
        !registry
            .get_routes()
            .await
            .expect("routes after staged deploy")
            .contains_key(&app.id),
        "a staged deploy must not bypass the archive route fence"
    );

    registry
        .unarchive_app(&app.id)
        .await
        .expect("restore app")
        .expect("app exists");
    let restored_route = registry
        .get_routes()
        .await
        .expect("restored routes")
        .remove(&app.id)
        .expect("restored app route");
    assert_eq!(
        restored_route.deploy_hash.as_deref(),
        Some(staged_hash.as_str())
    );
    assert_eq!(
        restored_route.manifest.metadata.compiler.as_deref(),
        Some("staged-while-archived"),
        "restore must publish the staged manifest rather than the pre-archive deploy"
    );

    drop(client);
    drop(registry);
    common::drain_pg().await;
}

#[compio::test]
async fn restore_requires_a_staged_deploy_matching_the_latest_applied_schema() {
    let url = db_url();
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let (owner, plan_id) = seed_owner_and_plan(&client).await;
    let app = registry
        .create_app(
            &format!("archive-schema-{}", Uuid::new_v4().simple()),
            &plan_id,
            &owner,
            None,
            None,
        )
        .await
        .expect("create app");
    common::deployments::deploy(
        &registry,
        &app.id,
        &owner,
        common::deployments::labelled("old"),
    )
    .await
    .expect("set schema-less deploy");
    registry
        .archive_app(&app.id)
        .await
        .expect("archive app")
        .expect("app exists");

    let descriptor_hash = "5".repeat(64);
    client
        .execute(
            "INSERT INTO zeroship.app_schema_applies \
                (app_id, migration_id, status, request_body, effective_profile, \
                 ceiling_id, ceiling_version, applied_versions, submitted_by, \
                 applied_at, descriptor_sha256) \
             VALUES ($1, $2, 'applied', '{}'::jsonb, '{}'::jsonb, 'test-ceiling', 1, \
                     '[]'::jsonb, $3, now(), $4)",
            &[
                &app.id.as_str(),
                &Uuid::now_v7(),
                &owner.as_str(),
                &descriptor_hash,
            ],
        )
        .await
        .expect("record applied schema descriptor");

    // Restore admits the staged artifact on its BINDINGS, not on its schema:
    // an app whose staged deploy declares no database has nothing to verify,
    // and the applied-migration row above is now irrelevant to it. Comparing
    // schemas here coupled every app on a shared database to every other.
    assert!(
        !registry
            .get_routes()
            .await
            .expect("routes before restore")
            .contains_key(&app.id),
        "an archived app is out of the routing projection until it is restored"
    );

    let mut staged_manifest = Manifest::passthrough();
    staged_manifest.runtime_descriptor = vec![zeroship_bundle::RuntimeDescriptorEntry {
        label: "main".into(),
        database_id: zeroship_core::DatabaseId::mint(),
        primary: true,
        hash: descriptor_hash.clone(),
    }];
    let staged_hash = common::deployments::deploy(&registry, &app.id, &owner, staged_manifest)
        .await
        .expect("stage schema-compatible deploy")
        .result()
        .deploy_hash
        .clone();
    assert!(!registry
        .get_routes()
        .await
        .expect("routes after compatible stage")
        .contains_key(&app.id));

    registry
        .unarchive_app(&app.id)
        .await
        .expect("restore compatible app")
        .expect("app exists");
    let route = registry
        .get_routes()
        .await
        .expect("restored routes")
        .remove(&app.id)
        .expect("restored route");
    assert_eq!(route.deploy_hash.as_deref(), Some(staged_hash.as_str()));
    assert_eq!(
        route
            .manifest
            .runtime_descriptor
            .iter()
            .find(|entry| entry.primary)
            .map(|entry| entry.hash.as_str()),
        Some(descriptor_hash.as_str())
    );

    drop(client);
    drop(registry);
    common::drain_pg().await;
}

async fn count(client: &Client, table: &str, column: &str, app_id: &zeroship_core::AppId) -> i64 {
    assert!(table
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.'));
    assert!(column
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_'));
    let sql = format!("SELECT COUNT(*)::bigint AS n FROM {table} WHERE {column} = $1");
    client
        .query(&sql, &[&app_id.as_str()])
        .await
        .expect("count query")[0]
        .get("n")
}

fn first_of_this_month() -> chrono::NaiveDate {
    use chrono::Datelike;
    let now = chrono::Utc::now().date_naive();
    chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap()
}
