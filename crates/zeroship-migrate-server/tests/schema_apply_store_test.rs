mod fixture;

use compio_postgres::{Client, NoTls};
use serde_json::Value;
use uuid::Uuid;
use zeroship_id::{AppId, OrganizationId, ProjectId, UserId};
use zeroship_migrate_server::schema_apply_store::{SchemaApplyStore, TerminalTransition};

// Mandatory PostgreSQL tests. The fixture owns its server and applies the
// platform corpus before these cases connect.

/// The migrated database owned by this integration target.
fn test_dsn() -> String {
    fixture::migrated_url()
}

#[allow(
    clippy::future_not_send,
    reason = "compio clients and their runtime are intentionally thread-local"
)]
async fn test_client() -> Client {
    let (client, conn) = compio_postgres::connect(&test_dsn(), NoTls)
        .await
        .expect("connect to migrate-server test database");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    assert_platform_schema_present(&client).await;
    client
}

/// Tables the fixtures need, ALL of which come from the platform migrations.
///
/// `app_schema_applies` is on this list and the earlier version of this suite
/// created its own copy of the equivalent table instead. That was the defect:
/// a hand-rolled `CREATE TABLE IF NOT EXISTS` in the fixture meant the suite
/// passed against a shape the corpus does not produce, so a column the corpus
/// declares `NOT NULL` could be nullable here and nothing would notice. The
/// suite now REQUIRES a migrated database for the table under test as well.
const REQUIRED_PLATFORM_TABLES: [&str; 4] = ["plans", "users", "apps", "app_schema_applies"];

/// Fail with an actionable message when the target database has no platform schema.
async fn assert_platform_schema_present(client: &Client) {
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables \
              WHERE table_schema = 'zeroship'",
            &[],
        )
        .await
        .expect("read information_schema for platform tables");
    let present: std::collections::HashSet<String> = rows
        .iter()
        .map(|row| row.get::<_, String>("table_name"))
        .collect();
    let missing: Vec<&str> = REQUIRED_PLATFORM_TABLES
        .iter()
        .copied()
        .filter(|table| !present.contains(*table))
        .collect();
    assert!(
        missing.is_empty(),
        "the owned migration fixture is missing zeroship.{}",
        missing.join(", zeroship."),
    );
}

async fn insert_transition_row(
    client: &Client,
    app_id: &AppId,
    migration_id: Uuid,
    principal_id: &UserId,
    status: &str,
) {
    client
        .execute(
            "INSERT INTO zeroship.app_schema_applies \
                (app_id, migration_id, status, request_body, effective_profile, \
                 ceiling_id, ceiling_version, descriptor_sha256, applied_versions, \
                 submitted_by, submitted_at, applied_at, last_error) \
             VALUES ($1::text, $2, $3, '{\"marker\":\"original\"}'::jsonb, \
                     '{\"require_rls\":true}'::jsonb, 'test-ceiling', 7, \
                     repeat('a', 64), '[\"mig_original\"]'::jsonb, $4, \
                     TIMESTAMPTZ '2026-08-01 01:02:03+00', \
                     CASE WHEN $3 = 'applied' \
                          THEN TIMESTAMPTZ '2026-08-01 03:04:05+00' END, \
                     CASE WHEN $3 = 'failed' THEN 'original failure' END)",
            &[
                &app_id.as_str(),
                &migration_id,
                &status,
                &principal_id.as_str(),
            ],
        )
        .await
        .expect("insert transition test row");
}

async fn seed_transition_dependencies(client: &Client, app_id: &AppId, principal_id: &UserId) {
    let plan_id = "pln_schema_apply_transition_test";
    client
        .execute(
            "INSERT INTO zeroship.plans \
                (id, name, base_fee_cents, included_units, spend_limit_default_cents, \
                 runtime_limits_json) \
             VALUES ($1, 'Schema Apply Transition Test', 0, 0, 0, '{}'::jsonb) \
             ON CONFLICT (id) DO NOTHING",
            &[&plan_id],
        )
        .await
        .expect("seed transition test plan");
    let email = format!("schema-apply-{}@zeroship.test", principal_id.as_str());
    client
        .execute(
            "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, 'Schema Apply Test User', NOW())",
            &[&principal_id.as_str(), &email],
        )
        .await
        .expect("seed transition test user");
    let app_name = format!("schema-apply-{}", app_id.as_str());
    // An app needs a project and a project needs an organization:
    // `apps.project_id` is NOT NULL against a RESTRICT foreign key. This
    // fixture is about the apply-record state machine, not about who may
    // apply, so the organization is left member-less.
    let organization_id = OrganizationId::mint();
    let project_id = ProjectId::mint();
    client
        .execute(
            "INSERT INTO zeroship.organizations (id, slug, name, billing_email) \
             VALUES ($1, $2, 'Schema Apply Fixture', 'fixture@zeroship.test')",
            &[
                &organization_id.as_str(),
                &format!("schema-apply-org-{}", Uuid::new_v4().simple()),
            ],
        )
        .await
        .expect("seed transition test organization");
    client
        .execute(
            "INSERT INTO zeroship.projects (id, organization_id, slug, name) \
             VALUES ($1, $2, 'default', 'Default')",
            &[&project_id.as_str(), &organization_id.as_str()],
        )
        .await
        .expect("seed transition test project");
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
             SELECT $1::text, $2, $3, p.id, p.organization_id \
               FROM zeroship.projects p WHERE p.id = $4",
            &[&app_id.as_str(), &app_name, &plan_id, &project_id.as_str()],
        )
        .await
        .expect("seed transition test app");
}

/// Terminal states are terminal: neither marking may overwrite the other, and
/// the loser SAYS SO.
///
/// The guard is what stops a late error path flipping a row the engine already
/// applied: the DDL is committed and cannot be taken back, so an operator
/// reading `status = 'failed'` would believe nothing changed while the app's
/// schema had in fact moved.
///
/// What this does NOT cover: the callers in `apply.rs` acting on `Lost`. They
/// log and continue, and nothing here observes a log.
#[ntex::test]
async fn terminal_status_transitions_do_not_clobber_each_other_pg() {
    let client = test_client().await;
    let store = SchemaApplyStore::new(test_dsn());
    let app_id = AppId::mint();
    let principal_id = UserId::mint();
    let applied_id = Uuid::now_v7();
    let failed_id = Uuid::now_v7();

    seed_transition_dependencies(&client, &app_id, &principal_id).await;
    insert_transition_row(&client, &app_id, applied_id, &principal_id, "applied").await;
    insert_transition_row(&client, &app_id, failed_id, &principal_id, "failed").await;

    // An error path firing after a successful apply must not erase it.
    let outcome = store
        .mark_failed(&app_id, applied_id, "late error on an applied migration")
        .await
        .expect("call must not error");
    assert_eq!(
        outcome,
        TerminalTransition::Lost,
        "a failure that matched no row must report the loss, not success"
    );
    let row = client
        .query_one(
            "SELECT status FROM zeroship.app_schema_applies \
              WHERE app_id = $1::text AND migration_id = $2",
            &[&app_id.as_str(), &applied_id],
        )
        .await
        .expect("read the applied row");
    assert_eq!(
        row.get::<_, String>("status"),
        "applied",
        "a failed marking must not overwrite an applied migration"
    );

    // And the converse: a failed migration must not silently become applied.
    let outcome = store
        .mark_applied(&app_id, failed_id, &["mig_late".to_string()])
        .await
        .expect("call must not error");
    assert_eq!(
        outcome,
        TerminalTransition::Lost,
        "an apply that matched no row must report the loss, not success"
    );
    let row = client
        .query_one(
            "SELECT status FROM zeroship.app_schema_applies \
              WHERE app_id = $1::text AND migration_id = $2",
            &[&app_id.as_str(), &failed_id],
        )
        .await
        .expect("read the failed row");
    assert_eq!(
        row.get::<_, String>("status"),
        "failed",
        "an applied marking must not overwrite a failed migration"
    );

    // POSITIVE CONTROL. Both assertions above are satisfied by a method that
    // returns `Lost` unconditionally, which is the same shape of mistake the
    // discarded row count was. A transition that WINS must say `Recorded`, or
    // `Lost` carries no information.
    let winning_id = Uuid::now_v7();
    insert_transition_row(&client, &app_id, winning_id, &principal_id, "submitted").await;
    let outcome = store
        .mark_applied(&app_id, winning_id, &["mig_winner".to_string()])
        .await
        .expect("call must not error");
    assert_eq!(
        outcome,
        TerminalTransition::Recorded,
        "a transition that matched its row must report success"
    );
    let row = client
        .query_one(
            "SELECT status, applied_versions FROM zeroship.app_schema_applies \
              WHERE app_id = $1::text AND migration_id = $2",
            &[&app_id.as_str(), &winning_id],
        )
        .await
        .expect("read the winning row");
    assert_eq!(
        row.get::<_, String>("status"),
        "applied",
        "a submitted migration must reach applied"
    );
    assert_eq!(
        row.get::<_, Value>("applied_versions"),
        serde_json::json!(["mig_winner"]),
        "the engine's applied set must be recorded, not the row's prior value"
    );
}

/// A request that applied NOTHING still lands a row, and the row says so.
///
/// This is the case the deploy precondition depends on: an engine upgrade that
/// changes descriptor bytes without changing any schema must be repairable by a
/// migrate that applies nothing. If `mark_applied` skipped the write for an
/// empty set - a natural-looking optimisation - every such app's next deploy
/// would be refused forever.
#[ntex::test]
async fn an_apply_that_advanced_nothing_still_records_an_applied_row_pg() {
    let client = test_client().await;
    let store = SchemaApplyStore::new(test_dsn());
    let app_id = AppId::mint();
    let principal_id = UserId::mint();
    let migration_id = Uuid::now_v7();

    seed_transition_dependencies(&client, &app_id, &principal_id).await;
    insert_transition_row(&client, &app_id, migration_id, &principal_id, "submitted").await;

    let outcome = store
        .mark_applied(&app_id, migration_id, &[])
        .await
        .expect("call must not error");
    assert_eq!(outcome, TerminalTransition::Recorded);
    let row = client
        .query_one(
            "SELECT status, applied_versions, applied_at IS NOT NULL AS stamped \
               FROM zeroship.app_schema_applies \
              WHERE app_id = $1::text AND migration_id = $2",
            &[&app_id.as_str(), &migration_id],
        )
        .await
        .expect("read the row");
    assert_eq!(row.get::<_, String>("status"), "applied");
    assert!(row.get::<_, bool>("stamped"), "applied_at must be stamped");
    assert_eq!(
        row.get::<_, Value>("applied_versions"),
        serde_json::json!([]),
        "an apply that advanced nothing must be visible as an empty applied set"
    );
}
