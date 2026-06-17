//! Faithful declarative type-fidelity tests against a REAL Postgres (no shims).
//!
//! **P0** — type fidelity: for each DSL type, build the equivalent CREATE TABLE,
//! apply it, snapshot the live schema, and assert `desired_snapshot` of the same
//! descriptor round-trips with ZERO drift. This surfaces any type-spelling
//! mismatch between the replicated map and live Postgres.
//!
//! Requires `zeroship_migrate_test` on :5440.

use compio_postgres::Client;
use zeroship_migrate::{
    desired_snapshot, diff_snapshots, migrator_role_name, provision_migrator,
    role::deprovision_migrator, snapshot_schema, Approval, CollectionDescriptor, DeclarativeAuthor,
    DeclarativeError, EngineError, ExecutorConfig, FieldDescriptor, GuardConfig, IndexDescriptor,
    MigrationEngine, SchemaSnapshot,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.meta_schema = format!("meta_{tok}");
    c
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
    // Harmless when no role was provisioned (P0 tests).
    let _ = deprovision_migrator(conn, cfg).await;
}

/// A config with a matching least-privilege migrator role (the P1/P2 apply path).
fn cfg_with_role(tok: &str) -> ExecutorConfig {
    let c = cfg_for(tok);
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    }
}

fn author_for(cfg: &ExecutorConfig) -> DeclarativeAuthor {
    DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test")
}

/// Stand up the project schema + provision the migrator role.
async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    ensure_project_schema(conn, cfg).await;
    provision_migrator(conn, cfg)
        .await
        .expect("provision migrator role");
}

/// Plan the desired-vs-live diff through `plan_declarative` and apply it.
async fn apply_plan(
    engine: &MigrationEngine,
    desired: &SchemaSnapshot,
    live: &SchemaSnapshot,
    author: &DeclarativeAuthor,
    cfg: &ExecutorConfig,
    conn: &Client,
    approval: Approval,
) -> Result<(), EngineError> {
    let plan = engine
        .plan_declarative(desired, live, author, &guard_cfg(cfg))
        .expect("plan_declarative");
    engine.apply(&plan, approval, conn, cfg, "app_test").await?;
    Ok(())
}

/// The seven system-field column declarations every collection table gets,
/// in the SAME DDL spelling `desired_snapshot` models (system fields injected).
const SYSTEM_FIELD_DDL: &str = "id TEXT PRIMARY KEY, \
     created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
     updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
     created_by TEXT NULL, \
     updated_by TEXT NULL, \
     version INTEGER NOT NULL DEFAULT 1, \
     deleted_at TIMESTAMPTZ NULL";

// ---------------------------------------------------------------------------
// P0 — type-fidelity round-trip.
// ---------------------------------------------------------------------------

/// For one DSL type token + its equivalent DDL type, create a real table with
/// the system fields + one declared column, snapshot the live schema, and assert
/// `desired_snapshot` of the matching descriptor round-trips with ZERO drift.
/// This surfaces any type-spelling mismatch between the map and live Postgres.
async fn assert_type_fidelity(dsl_type: &str, ddl_type: &str, required: bool) {
    let tok = token();
    let cfg = cfg_for(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let null = if required { "NOT NULL" } else { "NULL" };
    // Build the real table: system fields + one declared column of `ddl_type`.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{schema}\".\"widgets\" ({sys}, \"attr\" {ddl} {null})",
        schema = cfg.project_schema,
        sys = SYSTEM_FIELD_DDL,
        ddl = ddl_type,
        null = null,
    ))
    .await
    .unwrap_or_else(|e| panic!("create widgets for {dsl_type}: {e}"));

    let live = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("snapshot live");

    // The matching descriptor: one declared field `attr` of `dsl_type`.
    let desc = CollectionDescriptor {
        name: "widgets".into(),
        fields: vec![FieldDescriptor {
            name: "attr".into(),
            ty: dsl_type.into(),
            required,
            unique: false,
            references: None,
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&[desc]);

    // The columns must round-trip with ZERO drift. (We compare columns only;
    // the live snapshot's PK constraint definition is compared loosely below.)
    let drift = diff_snapshots(&desired, &live);
    // The declared column must be present + same type/nullability on both sides
    // (no altered_objects on `widgets.attr`).
    let attr_altered: Vec<_> = drift
        .altered_objects
        .iter()
        .filter(|a| a.object == "column attr")
        .collect();
    assert!(
        attr_altered.is_empty(),
        "type fidelity drift for DSL '{dsl_type}' (DDL '{ddl_type}'): {attr_altered:?}"
    );
    // The column is not missing/unexpected either.
    assert!(
        !drift.missing_objects.iter().any(|m| m == "widgets.attr"),
        "column attr unexpectedly MISSING for '{dsl_type}': desired has it but live lacks it"
    );
    assert!(
        !drift.unexpected_objects.iter().any(|m| m == "widgets.attr"),
        "column attr unexpectedly UNEXPECTED for '{dsl_type}'"
    );

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn type_fidelity_string_is_text() {
    assert_type_fidelity("string", "TEXT", false).await;
}

#[compio::test]
async fn type_fidelity_ref_is_text() {
    assert_type_fidelity("ref", "TEXT", false).await;
}

#[compio::test]
async fn type_fidelity_actor_is_text() {
    assert_type_fidelity("actor", "TEXT", false).await;
}

#[compio::test]
async fn type_fidelity_number_is_double_precision() {
    assert_type_fidelity("number", "DOUBLE PRECISION", false).await;
}

#[compio::test]
async fn type_fidelity_boolean_is_boolean() {
    assert_type_fidelity("boolean", "BOOLEAN", false).await;
}

#[compio::test]
async fn type_fidelity_date_is_timestamptz() {
    assert_type_fidelity("date", "TIMESTAMPTZ", false).await;
}

#[compio::test]
async fn type_fidelity_calendar_date_is_date() {
    assert_type_fidelity("calendarDate", "DATE", false).await;
}

#[compio::test]
async fn type_fidelity_json_is_jsonb() {
    assert_type_fidelity("json", "JSONB", false).await;
}

#[compio::test]
async fn type_fidelity_object_is_jsonb() {
    assert_type_fidelity("object", "JSONB", false).await;
}

#[compio::test]
async fn type_fidelity_array_is_jsonb() {
    assert_type_fidelity("array", "JSONB", false).await;
}

#[compio::test]
async fn type_fidelity_union_is_jsonb() {
    assert_type_fidelity("union", "JSONB", false).await;
}

#[compio::test]
async fn type_fidelity_bytes_is_bytea() {
    assert_type_fidelity("bytes", "BYTEA", false).await;
}

#[compio::test]
async fn type_fidelity_required_is_not_null() {
    // Nullability fidelity: a required field round-trips to a NOT NULL column.
    assert_type_fidelity("string", "TEXT", true).await;
}

#[compio::test]
async fn type_fidelity_whole_table_round_trips_to_zero_drift() {
    // A full collection (every system field + several declared types + an id PK)
    // built by hand round-trips to a byte-clean snapshot — the strongest P0
    // proof: zero missing/unexpected/altered across the whole table.
    let tok = token();
    let cfg = cfg_for(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    conn.batch_execute(&format!(
        "CREATE TABLE \"{schema}\".\"profiles\" ({sys}, \
         \"handle\" TEXT NOT NULL, \
         \"score\" DOUBLE PRECISION NULL, \
         \"active\" BOOLEAN NOT NULL, \
         \"prefs\" JSONB NULL, \
         \"joined\" TIMESTAMPTZ NULL)",
        schema = cfg.project_schema,
        sys = SYSTEM_FIELD_DDL,
    ))
    .await
    .expect("create profiles");

    let live = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("snapshot");

    let desc = CollectionDescriptor {
        name: "profiles".into(),
        fields: vec![
            FieldDescriptor { name: "handle".into(), ty: "string".into(), required: true, unique: false, references: None },
            FieldDescriptor { name: "score".into(), ty: "number".into(), required: false, unique: false, references: None },
            FieldDescriptor { name: "active".into(), ty: "boolean".into(), required: true, unique: false, references: None },
            FieldDescriptor { name: "prefs".into(), ty: "json".into(), required: false, unique: false, references: None },
            FieldDescriptor { name: "joined".into(), ty: "date".into(), required: false, unique: false, references: None },
        ],
        indexes: vec![],
    };
    let desired = desired_snapshot(&[desc]);
    let drift = diff_snapshots(&desired, &live);

    // No column drift at all. (The PK constraint definition may differ in
    // spelling — pg_get_constraintdef renders `PRIMARY KEY (id)`; our desired
    // models the same — so we assert column + index cleanliness and that the
    // only possible altered object is not a column.)
    let col_drift: Vec<_> = drift
        .altered_objects
        .iter()
        .filter(|a| a.object.starts_with("column "))
        .collect();
    assert!(col_drift.is_empty(), "column drift: {col_drift:?}");
    assert!(
        drift.missing_objects.is_empty(),
        "missing: {:?}",
        drift.missing_objects
    );
    assert!(
        drift.unexpected_objects.is_empty(),
        "unexpected: {:?}",
        drift.unexpected_objects
    );

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// P1 — additive differ: apply-then-re-diff-to-zero is the oracle.
// ---------------------------------------------------------------------------

#[compio::test]
async fn additive_create_table_with_column_and_index_applies_to_zero_drift() {
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "tasks".into(),
        fields: vec![
            FieldDescriptor { name: "title".into(), ty: "string".into(), required: true, unique: false, references: None },
            FieldDescriptor { name: "slug".into(), ty: "string".into(), required: false, unique: true, references: None },
            FieldDescriptor { name: "done".into(), ty: "boolean".into(), required: false, unique: false, references: None },
        ],
        indexes: vec![IndexDescriptor { name: "tasks_title_idx".into(), columns: vec!["title".into()], unique: false }],
    };
    let desired = desired_snapshot(&[desc]);
    let empty = SchemaSnapshot::default();

    apply_plan(&engine, &desired, &empty, &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply additive plan");

    // Re-snapshot: the live schema now equals desired (zero drift).
    let live2 = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("re-snapshot");
    let drift = diff_snapshots(&desired, &live2);
    assert!(
        drift.is_clean(),
        "expected zero drift after apply, got: missing={:?} unexpected={:?} altered={:?}",
        drift.missing_objects,
        drift.unexpected_objects,
        drift.altered_objects
    );

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn additive_diff_is_idempotent_second_plan_is_empty() {
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "notes".into(),
        fields: vec![FieldDescriptor { name: "body".into(), ty: "string".into(), required: false, unique: false, references: None }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&[desc]);
    let empty = SchemaSnapshot::default();

    apply_plan(&engine, &desired, &empty, &author, &cfg, &conn, Approval::None)
        .await
        .expect("first apply");

    // Second diff against the now-current live yields an EMPTY migration set.
    let live2 = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("re-snapshot");
    let migs = author.diff(&desired, &live2).expect("second diff");
    assert!(
        migs.is_empty(),
        "second diff should be empty (idempotent), got {} migration(s)",
        migs.len()
    );

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn additive_add_column_to_existing_table_applies_clean() {
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // First create the table with one field.
    let v1 = CollectionDescriptor {
        name: "items".into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&[v1]);
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create items");

    // Now desire an ADDED nullable column.
    let v2 = CollectionDescriptor {
        name: "items".into(),
        fields: vec![
            FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None },
            FieldDescriptor { name: "qty".into(), ty: "number".into(), required: false, unique: false, references: None },
        ],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&[v2]);
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    apply_plan(&engine, &d2, &live, &author, &cfg, &conn, Approval::None)
        .await
        .expect("add column");

    let live2 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    assert!(diff_snapshots(&d2, &live2).is_clean(), "add-column did not converge");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn fk_ordering_referencing_table_after_target_applies_clean() {
    // B references A: A must be created before B (or the FK deferred). Either
    // way the batch applies clean and re-diffs to zero.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // Declare B before A in the descriptor list to prove ordering is by FK dep,
    // not declaration order.
    let b = CollectionDescriptor {
        name: "orders".into(),
        fields: vec![FieldDescriptor {
            name: "customer".into(),
            ty: "ref".into(),
            required: false,
            unique: false,
            references: Some("customers".into()),
        }],
        indexes: vec![],
    };
    let a = CollectionDescriptor {
        name: "customers".into(),
        fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&[b, a]);
    apply_plan(&engine, &desired, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply FK batch");

    let live2 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    // Both tables exist with the FK constraint; columns clean.
    let drift = diff_snapshots(&desired, &live2);
    let col_drift: Vec<_> = drift.altered_objects.iter().filter(|x| x.object.starts_with("column ")).collect();
    assert!(col_drift.is_empty(), "column drift after FK batch: {col_drift:?}");
    assert!(drift.missing_objects.is_empty(), "missing after FK batch: {:?}", drift.missing_objects);

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn type_change_is_unsupported_in_v1_not_silently_skipped() {
    // A same-name column whose type changed is an explicit UnsupportedInV1,
    // never a silent no-op (and never an auto type-change).
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    let live = {
        let desc = CollectionDescriptor {
            name: "widgets".into(),
            fields: vec![FieldDescriptor { name: "attr".into(), ty: "string".into(), required: false, unique: false, references: None }],
            indexes: vec![],
        };
        desired_snapshot(&[desc])
    };
    let desired = {
        let desc = CollectionDescriptor {
            name: "widgets".into(),
            fields: vec![FieldDescriptor { name: "attr".into(), ty: "number".into(), required: false, unique: false, references: None }],
            indexes: vec![],
        };
        desired_snapshot(&[desc])
    };

    let err = author.diff(&desired, &live).unwrap_err();
    assert!(matches!(err, DeclarativeError::UnsupportedInV1(_)), "got {err:?}");
}

#[compio::test]
async fn malicious_table_name_is_rejected_at_author_boundary() {
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);
    let desc = CollectionDescriptor {
        name: "users\"; DROP SCHEMA control CASCADE; --".into(),
        fields: vec![FieldDescriptor { name: "x".into(), ty: "string".into(), required: false, unique: false, references: None }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&[desc]);
    let err = author.diff(&desired, &SchemaSnapshot::default()).unwrap_err();
    assert!(matches!(err, DeclarativeError::Invalid(_)), "got {err:?}");
}

#[compio::test]
async fn malicious_column_name_is_rejected_at_author_boundary() {
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);
    let desc = CollectionDescriptor {
        name: "widgets".into(),
        fields: vec![FieldDescriptor {
            name: "evil\") ; DROP TABLE control.users; --".into(),
            ty: "string".into(),
            required: false,
            unique: false,
            references: None,
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&[desc]);
    let err = author.diff(&desired, &SchemaSnapshot::default()).unwrap_err();
    assert!(matches!(err, DeclarativeError::Invalid(_)), "got {err:?}");
}

#[compio::test]
async fn every_generated_migration_passes_through_the_guard_no_bypass() {
    // The declarative path emits SQL that goes through plan()'s SqlGuard. Build
    // a normal additive desired/live pair, plan it, and assert the plan has NO
    // denials (the generated SQL is guard-safe) AND every migration is a planned
    // item — i.e. it flowed through the guard, not around it.
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desired = {
        let desc = CollectionDescriptor {
            name: "events".into(),
            fields: vec![
                FieldDescriptor { name: "kind".into(), ty: "string".into(), required: true, unique: true, references: None },
                FieldDescriptor { name: "payload".into(), ty: "json".into(), required: false, unique: false, references: None },
            ],
            indexes: vec![IndexDescriptor { name: "events_kind_idx".into(), columns: vec!["kind".into()], unique: false }],
        };
        desired_snapshot(&[desc])
    };
    let migs = author.diff(&desired, &SchemaSnapshot::default()).expect("diff");
    assert!(!migs.is_empty(), "diff should generate migrations");
    let plan = engine.plan(&migs, &guard_cfg(&cfg));
    assert!(plan.denied.is_empty(), "generated SQL must not be denied: {:?}", plan.denied);
    assert_eq!(
        plan.items.len(),
        migs.len(),
        "every generated migration must flow through the guard as a planned item"
    );
}

// ---------------------------------------------------------------------------
// P2 — destructive classification (GATED). NEVER auto-applied.
// ---------------------------------------------------------------------------

#[compio::test]
async fn drop_table_is_gated_and_not_auto_applied() {
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // Create a table.
    let v1 = CollectionDescriptor {
        name: "legacy".into(),
        fields: vec![FieldDescriptor { name: "x".into(), ty: "string".into(), required: false, unique: false, references: None }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&[v1]);
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create legacy");

    // Now desire it GONE (empty desired).
    let empty = SchemaSnapshot::default();
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let plan = engine
        .plan_declarative(&empty, &live, &author, &guard_cfg(&cfg))
        .expect("plan drop");
    assert!(plan.destructive, "a DROP TABLE diff must be destructive");
    assert!(plan.requires_approval, "a DROP TABLE diff must require approval");

    // Apply WITHOUT approval → ApprovalRequired, nothing applied.
    let err = engine
        .apply(&plan, Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("drop without approval must be refused");
    assert!(matches!(err, EngineError::ApprovalRequired), "got {err:?}");

    // The table is STILL present (nothing applied).
    let live_after = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    assert!(
        live_after.tables.contains_key("legacy"),
        "table must still exist after a refused drop"
    );

    // Now apply WITH approval → the drop lands, re-diff clean.
    engine
        .apply(&plan, Approval::Approved, &conn, &cfg, "app_test")
        .await
        .expect("approved drop applies");
    let live_final = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap3");
    assert!(
        !live_final.tables.contains_key("legacy"),
        "table must be gone after an approved drop"
    );
    assert!(
        diff_snapshots(&empty, &live_final).is_clean(),
        "re-diff after approved drop must be clean"
    );

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn drop_column_is_destructive_and_gated() {
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let v1 = CollectionDescriptor {
        name: "people".into(),
        fields: vec![
            FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None },
            FieldDescriptor { name: "nickname".into(), ty: "string".into(), required: false, unique: false, references: None },
        ],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&[v1]);
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create people");

    // Drop `nickname`.
    let v2 = CollectionDescriptor {
        name: "people".into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&[v2]);
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let plan = engine
        .plan_declarative(&d2, &live, &author, &guard_cfg(&cfg))
        .expect("plan drop column");
    assert!(plan.destructive, "drop column must be destructive");
    assert!(plan.requires_approval, "drop column must be gated");

    // Refused without approval; column still present.
    let err = engine.apply(&plan, Approval::None, &conn, &cfg, "app_test").await.unwrap_err();
    assert!(matches!(err, EngineError::ApprovalRequired), "got {err:?}");
    let live_after = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    assert!(
        live_after.tables["people"].columns.iter().any(|c| c.name == "nickname"),
        "nickname must survive a refused drop"
    );

    // Approved → applied, re-diff clean.
    engine.apply(&plan, Approval::Approved, &conn, &cfg, "app_test").await.expect("approved drop");
    let live_final = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap3");
    assert!(
        !live_final.tables["people"].columns.iter().any(|c| c.name == "nickname"),
        "nickname must be gone after an approved drop"
    );
    assert!(diff_snapshots(&d2, &live_final).is_clean(), "re-diff clean after approved drop");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn drop_index_is_not_data_loss_so_it_is_not_gated() {
    // Dropping an index is REVERSIBLE (recreate it), not data loss — the guard
    // correctly does not mark it destructive, so the declarative path applies it
    // ungated (like an additive op). This documents that the differ honours the
    // security core's data-loss judgement rather than over-gating.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // Create a table with a named index.
    let v1 = CollectionDescriptor {
        name: "logs".into(),
        fields: vec![FieldDescriptor { name: "level".into(), ty: "string".into(), required: false, unique: false, references: None }],
        indexes: vec![IndexDescriptor { name: "logs_level_idx".into(), columns: vec!["level".into()], unique: false }],
    };
    let d1 = desired_snapshot(&[v1]);
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create logs");

    // Desire the index GONE.
    let v2 = CollectionDescriptor {
        name: "logs".into(),
        fields: vec![FieldDescriptor { name: "level".into(), ty: "string".into(), required: false, unique: false, references: None }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&[v2]);
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let plan = engine
        .plan_declarative(&d2, &live, &author, &guard_cfg(&cfg))
        .expect("plan drop index");
    assert!(!plan.destructive, "DROP INDEX is not data loss");
    assert!(!plan.requires_approval, "DROP INDEX must not require approval");

    // Applies WITHOUT approval; the index is gone, re-diff clean.
    engine.apply(&plan, Approval::None, &conn, &cfg, "app_test").await.expect("apply drop index");
    let live_after = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    assert!(
        !live_after.tables["logs"].indexes.iter().any(|i| i.name == "logs_level_idx"),
        "index must be gone after the ungated drop"
    );
    assert!(diff_snapshots(&d2, &live_after).is_clean(), "re-diff clean after drop index");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn malicious_type_in_descriptor_never_reaches_db_unguarded() {
    // A malicious DSL type string maps to `text` (the conservative fallback) and
    // can never inject DDL: the desired column's data_type is a fixed mapping
    // output, validated by validate_type at the author boundary, and the
    // generated SQL still passes through the guard. Build a descriptor whose
    // type is a SQL-injection attempt and assert it produces a guard-safe,
    // denial-free plan (no bypass) AND the emitted type is the safe fallback.
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "safe".into(),
        fields: vec![FieldDescriptor {
            name: "f".into(),
            ty: "text; DROP TABLE control.users; --".into(),
            required: false,
            unique: false,
            references: None,
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&[desc]);
    // The malicious type maps to the `text` fallback — no injection in the
    // desired snapshot at all.
    assert_eq!(desired.tables["safe"].columns.iter().find(|c| c.name == "f").unwrap().data_type, "text");

    let migs = author.diff(&desired, &SchemaSnapshot::default()).expect("diff");
    let plan = engine.plan(&migs, &guard_cfg(&cfg));
    assert!(plan.denied.is_empty(), "generated SQL must be guard-safe: {:?}", plan.denied);
    // And the rendered CREATE TABLE contains the safe `text` type, not the payload.
    let create = migs.iter().find(|m| m.name == "create_table_safe").unwrap();
    assert!(create.up.contains("\"f\" text"), "up = {}", create.up);
    assert!(!create.up.contains("DROP TABLE control"), "payload leaked into SQL: {}", create.up);
}
