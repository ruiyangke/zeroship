//! Faithful declarative type-fidelity tests against a REAL Postgres (no shims).
//!
//! **P0** — type fidelity: for each DSL type, build the equivalent CREATE TABLE,
//! apply it, snapshot the live schema, and assert `desired_snapshot` of the same
//! descriptor round-trips with ZERO drift. This surfaces any type-spelling
//! mismatch between the replicated map and live Postgres.
//!
//! Requires `zeroship_migrate_test` on :5440.

use std::collections::HashMap;

use compio_postgres::Client;
use zeroship_migrate::{
    apply as executor_apply, desired_snapshot, diff_snapshots, migrator_role_name,
    provision_migrator, role::deprovision_migrator, snapshot_schema, Approval, CollectionDescriptor,
    DeclarativeAuthor, DeclarativeError, DesiredSchema, EngineError, ExecutorConfig, FieldDescriptor,
    GuardConfig,
    IndexDescriptor, Migration, MigrationEngine, OnlinePhase, RenameHint, SchemaSnapshot,
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
    GuardConfig::confined(cfg.project_schema.clone())
}

fn author_for(cfg: &ExecutorConfig) -> DeclarativeAuthor {
    DeclarativeAuthor::new(cfg.project_schema.clone(), "app_test")
}

/// A declarative author whose **deploying** app is `app` (the P4 ownership
/// subject). Used by the multi-app ownership-enforcement tests.
fn author_app(cfg: &ExecutorConfig, app: &str) -> DeclarativeAuthor {
    DeclarativeAuthor::new(cfg.project_schema.clone(), app)
}

/// Stand up the project schema + provision the migrator role.
async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    ensure_project_schema(conn, cfg).await;
    provision_migrator(conn, cfg)
        .await
        .expect("provision migrator role");
}

/// Plan the desired-vs-live diff through `plan_declarative` (no rename hints) and
/// apply it. This helper drives only ADDITIVE flows (create-table / add-column /
/// add-index), where live ⊆ desired so NO `DROP TABLE` is ever emitted — hence
/// an empty `live_ownership` map is correct (the fail-closed drop guard is never
/// consulted). Drop flows call `plan_declarative` directly with an explicit map.
async fn apply_plan(
    engine: &MigrationEngine,
    desired: &DesiredSchema,
    live: &SchemaSnapshot,
    author: &DeclarativeAuthor,
    cfg: &ExecutorConfig,
    conn: &Client,
    approval: Approval,
) -> Result<(), EngineError> {
    let plan = engine
        .plan_declarative(desired, live, &HashMap::new(), author, &[], &guard_cfg(cfg))
        .expect("plan_declarative");
    // This helper drives only NO-rename diffs (it passes `&[]` hints), so the
    // plain plan is the whole deploy; apply it through the gate directly.
    debug_assert!(plan.renames.is_empty(), "apply_plan is for hint-free diffs");
    engine.apply(&plan.plain, approval, conn, cfg, "app_test").await?;
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
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "attr".into(),
            ty: dsl_type.into(),
            required,
            unique: false,
            references: None,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot");

    // The columns must round-trip with ZERO drift. (We compare columns only;
    // the live snapshot's PK constraint definition is compared loosely below.)
    let drift = diff_snapshots(&desired.snapshot, &live);
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

    // The platform auto-indexes deleted_at / updated_at / created_by on every
    // table (#6); `desired_snapshot` models these three implicit B-tree indexes,
    // so the hand-built live table must carry them too or they phantom-MISS.
    // Names mirror plugin-db's `index_name(table, &[col], false)` = `<table>_<col>_idx`.
    for col in ["deleted_at", "updated_at", "created_by"] {
        conn.batch_execute(&format!(
            "CREATE INDEX \"profiles_{col}_idx\" ON \"{schema}\".\"profiles\" (\"{col}\")",
            schema = cfg.project_schema,
        ))
        .await
        .unwrap_or_else(|e| panic!("create system index profiles_{col}_idx: {e}"));
    }

    let live = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("snapshot");

    let desc = CollectionDescriptor {
        name: "profiles".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor { name: "handle".into(), ty: "string".into(), required: true, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "score".into(), ty: "number".into(), required: false, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "active".into(), ty: "boolean".into(), required: true, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "prefs".into(), ty: "json".into(), required: false, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "joined".into(), ty: "date".into(), required: false, unique: false, references: None, ..Default::default() },
        ],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot");
    let drift = diff_snapshots(&desired.snapshot, &live);

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
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor { name: "title".into(), ty: "string".into(), required: true, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "slug".into(), ty: "string".into(), required: false, unique: true, references: None, ..Default::default() },
            FieldDescriptor { name: "done".into(), ty: "boolean".into(), required: false, unique: false, references: None, ..Default::default() },
        ],
        indexes: vec![IndexDescriptor { name: "tasks_title_idx".into(), columns: vec!["title".into()], unique: false }],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot");
    let empty = SchemaSnapshot::default();

    apply_plan(&engine, &desired, &empty, &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply additive plan");

    // Re-snapshot: the live schema now equals desired (zero drift).
    let live2 = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("re-snapshot");
    let drift = diff_snapshots(&desired.snapshot, &live2);
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
async fn full_shape_round_trips_to_zero_drift_the_canonical_idempotency_oracle() {
    // THE canonical idempotency oracle (strengthened): generate + apply a batch
    // covering EVERY shape that broke the weak (column-filtered) oracle —
    //   * an FK to another table (1b: FK definition spelling),
    //   * a multi-column index (1a: composite columns dropped from the snapshot),
    //   * a custom-named index whose name does NOT encode its columns (1a),
    //   * a per-field unique index (1c: name cap + columns),
    // then assert the FULL diff is clean (NOT column-filtered) AND the differ
    // re-diffs to an EMPTY migration set. This must hold across the whole shape:
    // columns + indexes + constraints. Pre-fix this fails two ways — the
    // composite/custom indexes emit `column "a_b" does not exist` (42703) at
    // apply, and (had apply succeeded) every FK shows permanent phantom drift.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let authors_tbl = CollectionDescriptor {
        name: "authors".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "email".into(),
            ty: "string".into(),
            required: false,
            unique: true, // per-field unique index (1c)
            references: None,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let posts_tbl = CollectionDescriptor {
        name: "posts".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor {
                name: "author".into(),
                ty: "ref".into(),
                required: false,
                unique: false,
                references: Some("authors".into()), // FK (1b)
                ..Default::default()
            },
            FieldDescriptor { name: "a".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "b".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
        ],
        indexes: vec![
            // Composite index over (a, b) whose name encodes neither column set
            // recoverably (1a): `posts_a_b_idx` → broken heuristic recovers `a_b`.
            IndexDescriptor { name: "posts_a_b_idx".into(), columns: vec!["a".into(), "b".into()], unique: false },
            // Custom-named index whose name has NO relation to its column (1a).
            IndexDescriptor { name: "weird_custom_name".into(), columns: vec!["a".into()], unique: true },
        ],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[posts_tbl, authors_tbl]).expect("desired_snapshot");
    let empty = SchemaSnapshot::default();

    apply_plan(&engine, &desired, &empty, &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply full-shape plan (composite + custom + unique idx + FK)");

    // Re-snapshot the live schema. The FULL diff must be clean — no column,
    // index, OR constraint drift (this is what the weak oracle missed).
    let live2 = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("re-snapshot");
    let drift = diff_snapshots(&desired.snapshot, &live2);
    assert!(
        drift.is_clean(),
        "FULL re-diff must be clean: missing={:?} unexpected={:?} altered={:?}",
        drift.missing_objects,
        drift.unexpected_objects,
        drift.altered_objects
    );

    // And the differ itself re-diffs to ZERO migrations (true idempotency).
    let migs = author.diff(&desired, &live2, &HashMap::new(), &[]).expect("re-diff").migrations;
    assert!(
        migs.is_empty(),
        "re-diff must be empty, got {} migration(s): {:?}",
        migs.len(),
        migs.iter().map(|m| &m.name).collect::<Vec<_>>()
    );

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn changed_fk_target_is_unsupported_in_v1_not_silently_skipped() {
    // 5-fk: a same-name FK whose referenced target changed is an in-place
    // constraint redefinition — explicit UnsupportedInV1, never a silent no-op
    // (the old differ never compared FK bodies on existing tables, so a re-pointed
    // FK emitted 0 migrations and left the wrong constraint in place). With the FK
    // definition spelling now matching live exactly (1b), this compare is real,
    // not phantom-drift noise.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // Two possible targets + a referencing table whose FK points at `alpha`.
    let alpha = CollectionDescriptor { name: "alpha".into(), owner_app: "app_test".into(), fields: vec![], indexes: vec![] };
    let beta = CollectionDescriptor { name: "beta".into(), owner_app: "app_test".into(), fields: vec![], indexes: vec![] };
    let child_v1 = CollectionDescriptor {
        name: "child".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "parent".into(),
            ty: "ref".into(),
            required: false,
            unique: false,
            references: Some("alpha".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[alpha.clone(), beta.clone(), child_v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create alpha/beta/child");

    // Now re-point `child.parent` at `beta` — SAME constraint name (parent_fkey),
    // different REFERENCES target.
    let child_v2 = CollectionDescriptor {
        name: "child".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "parent".into(),
            ty: "ref".into(),
            required: false,
            unique: false,
            references: Some("beta".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[alpha, beta, child_v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let err = author.diff(&d2, &live, &HashMap::new(), &[]).unwrap_err();
    assert!(matches!(err, DeclarativeError::UnsupportedInV1(_)), "got {err:?}");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn long_table_and_field_unique_index_name_is_capped_and_re_diffs_clean() {
    // 1c: a unique index name `<table>_<field>_key` over a long table+field would
    // overflow Postgres's 63-byte limit and be truncated server-side, so the
    // desired (full) name would never match the live (truncated) name → CREATE/
    // DROP churn on every re-diff. The name must be hash-capped ≤63 bytes,
    // deterministic, and re-diff to ZERO.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // A table + field whose `<table>_<field>_key` natural name is > 63 bytes.
    let long_table = "a_very_long_collection_name_for_overflow_testing"; // 48
    let long_field = "an_equally_long_field_name_here"; // 31 → natural 48+1+31+4 = 84
    let desc = CollectionDescriptor {
        name: long_table.into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: long_field.into(),
            ty: "string".into(),
            required: false,
            unique: true,
            references: None,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot");

    // The generated unique-index name is ≤63 bytes (and deterministic across two
    // builds of the same desired snapshot).
    let idx = desired.snapshot.tables[long_table]
        .indexes
        .iter()
        .find(|i| i.unique && i.name != format!("{long_table}_pkey"))
        .expect("a per-field unique index exists");
    assert!(idx.name.len() <= 63, "index name {} bytes (>63)", idx.name.len());

    // Apply and re-diff to zero — no churn (the capped name round-trips to live).
    apply_plan(&engine, &desired, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply long-name table");
    let live2 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let migs = author.diff(&desired, &live2, &HashMap::new(), &[]).expect("re-diff").migrations;
    assert!(
        migs.is_empty(),
        "long-name index churned (re-diff not empty): {:?}",
        migs.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    assert!(diff_snapshots(&desired.snapshot, &live2).is_clean(), "full re-diff clean");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn index_uniqueness_flip_on_same_name_is_unsupported_in_v1_not_silent() {
    // 5-idx: a same-name index whose `unique` flag flipped is an in-place
    // redefinition (DROP+CREATE) — explicit UnsupportedInV1, never silent (the old
    // loop only checked name presence, so a uniqueness flip emitted 0 migrations
    // and left the wrong index in place).
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // Create a table with a NON-unique named index over `level`.
    let v1 = CollectionDescriptor {
        name: "logs".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "level".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![IndexDescriptor { name: "logs_level_idx".into(), columns: vec!["level".into()], unique: false }],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create logs");

    // Desire the SAME-name index but UNIQUE now.
    let v2 = CollectionDescriptor {
        name: "logs".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "level".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![IndexDescriptor { name: "logs_level_idx".into(), columns: vec!["level".into()], unique: true }],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let err = author.diff(&d2, &live, &HashMap::new(), &[]).unwrap_err();
    assert!(matches!(err, DeclarativeError::UnsupportedInV1(_)), "got {err:?}");

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
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "body".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot");
    let empty = SchemaSnapshot::default();

    apply_plan(&engine, &desired, &empty, &author, &cfg, &conn, Approval::None)
        .await
        .expect("first apply");

    // Second diff against the now-current live yields an EMPTY migration set.
    let live2 = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("re-snapshot");
    let migs = author.diff(&desired, &live2, &HashMap::new(), &[]).expect("second diff").migrations;
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
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create items");

    // Now desire an ADDED nullable column.
    let v2 = CollectionDescriptor {
        name: "items".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "qty".into(), ty: "number".into(), required: false, unique: false, references: None, ..Default::default() },
        ],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    apply_plan(&engine, &d2, &live, &author, &cfg, &conn, Approval::None)
        .await
        .expect("add column");

    let live2 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    assert!(diff_snapshots(&d2.snapshot, &live2).is_clean(), "add-column did not converge");

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
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "customer".into(),
            ty: "ref".into(),
            required: false,
            unique: false,
            references: Some("customers".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let a = CollectionDescriptor {
        name: "customers".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[b, a]).expect("desired_snapshot");
    apply_plan(&engine, &desired, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply FK batch");

    let live2 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    // Both tables exist with the FK constraint; columns clean.
    let drift = diff_snapshots(&desired.snapshot, &live2);
    let col_drift: Vec<_> = drift.altered_objects.iter().filter(|x| x.object.starts_with("column ")).collect();
    assert!(col_drift.is_empty(), "column drift after FK batch: {col_drift:?}");
    assert!(drift.missing_objects.is_empty(), "missing after FK batch: {:?}", drift.missing_objects);

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn type_change_emits_a_gated_alter_not_an_error_and_never_auto_applies() {
    // P3 Feature 2: a same-name column whose type changed is NO LONGER
    // UnsupportedInV1 — it emits a GATED `ALTER COLUMN … TYPE …` (destructive +
    // requires_approval; no auto type-change). It is never a silent no-op.
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    let live = {
        let desc = CollectionDescriptor {
            name: "widgets".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "attr".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let desired = {
        let desc = CollectionDescriptor {
            name: "widgets".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "attr".into(), ty: "number".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };

    let migs = author.diff(&desired, &live.snapshot, &HashMap::new(), &[]).expect("type change now diffs to a gated ALTER").migrations;
    let alter = migs.iter().find(|m| m.up.contains("ALTER COLUMN") && m.up.contains("TYPE"))
        .expect("a gated ALTER COLUMN TYPE migration");
    assert!(alter.flags.destructive, "type change is destructive (lossy/rewrite)");
    assert!(alter.flags.requires_approval, "type change is gated — no auto type-change");
    assert!(alter.up.contains("\"attr\""), "alters the attr column: {}", alter.up);
}

#[compio::test]
async fn malicious_table_name_is_rejected_at_author_boundary() {
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);
    let desc = CollectionDescriptor {
        name: "users\"; DROP SCHEMA control CASCADE; --".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "x".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot");
    let err = author.diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[]).unwrap_err();
    assert!(matches!(err, DeclarativeError::Invalid(_)), "got {err:?}");
}

#[compio::test]
async fn malicious_column_name_is_rejected_at_author_boundary() {
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);
    let desc = CollectionDescriptor {
        name: "widgets".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "evil\") ; DROP TABLE control.users; --".into(),
            ty: "string".into(),
            required: false,
            unique: false,
            references: None,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot");
    let err = author.diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[]).unwrap_err();
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
            owner_app: "app_test".into(),
            fields: vec![
                FieldDescriptor { name: "kind".into(), ty: "string".into(), required: true, unique: true, references: None, ..Default::default() },
                FieldDescriptor { name: "payload".into(), ty: "json".into(), required: false, unique: false, references: None, ..Default::default() },
            ],
            indexes: vec![IndexDescriptor { name: "events_kind_idx".into(), columns: vec!["kind".into()], unique: false }],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let migs = author.diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[]).expect("diff").migrations;
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
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "x".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create legacy");

    // Now desire it GONE (empty desired). The owner (app_test) is dropping its
    // OWN table, so supply live_ownership{legacy: app_test} — the fail-closed drop
    // guard allows an owner to drop its own table (2b).
    let empty = DesiredSchema::default();
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let live_ownership: HashMap<String, String> =
        std::iter::once(("legacy".to_string(), "app_test".to_string())).collect();
    let plan = engine
        .plan_declarative(&empty, &live, &live_ownership, &author, &[], &guard_cfg(&cfg))
        .expect("plan drop");
    assert!(plan.plain.destructive, "a DROP TABLE diff must be destructive");
    assert!(plan.plain.requires_approval, "a DROP TABLE diff must require approval");

    // Apply WITHOUT approval → ApprovalRequired, nothing applied.
    let err = engine
        .apply(&plan.plain, Approval::None, &conn, &cfg, "app_test")
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
        .apply(&plan.plain, Approval::Approved, &conn, &cfg, "app_test")
        .await
        .expect("approved drop applies");
    let live_final = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap3");
    assert!(
        !live_final.tables.contains_key("legacy"),
        "table must be gone after an approved drop"
    );
    assert!(
        diff_snapshots(&empty.snapshot, &live_final).is_clean(),
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
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "nickname".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
        ],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create people");

    // Drop `nickname`.
    let v2 = CollectionDescriptor {
        name: "people".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let plan = engine
        .plan_declarative(&d2, &live, &HashMap::new(), &author, &[], &guard_cfg(&cfg))
        .expect("plan drop column");
    assert!(plan.plain.destructive, "drop column must be destructive");
    assert!(plan.plain.requires_approval, "drop column must be gated");

    // Refused without approval; column still present.
    let err = engine.apply(&plan.plain, Approval::None, &conn, &cfg, "app_test").await.unwrap_err();
    assert!(matches!(err, EngineError::ApprovalRequired), "got {err:?}");
    let live_after = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    assert!(
        live_after.tables["people"].columns.iter().any(|c| c.name == "nickname"),
        "nickname must survive a refused drop"
    );

    // Approved → applied, re-diff clean.
    engine.apply(&plan.plain, Approval::Approved, &conn, &cfg, "app_test").await.expect("approved drop");
    let live_final = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap3");
    assert!(
        !live_final.tables["people"].columns.iter().any(|c| c.name == "nickname"),
        "nickname must be gone after an approved drop"
    );
    assert!(diff_snapshots(&d2.snapshot, &live_final).is_clean(), "re-diff clean after approved drop");

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
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "level".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![IndexDescriptor { name: "logs_level_idx".into(), columns: vec!["level".into()], unique: false }],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create logs");

    // Desire the index GONE.
    let v2 = CollectionDescriptor {
        name: "logs".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "level".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let plan = engine
        .plan_declarative(&d2, &live, &HashMap::new(), &author, &[], &guard_cfg(&cfg))
        .expect("plan drop index");
    assert!(!plan.plain.destructive, "DROP INDEX is not data loss");
    assert!(!plan.plain.requires_approval, "DROP INDEX must not require approval");

    // Applies WITHOUT approval; the index is gone, re-diff clean.
    engine.apply(&plan.plain, Approval::None, &conn, &cfg, "app_test").await.expect("apply drop index");
    let live_after = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    assert!(
        !live_after.tables["logs"].indexes.iter().any(|i| i.name == "logs_level_idx"),
        "index must be gone after the ungated drop"
    );
    assert!(diff_snapshots(&d2.snapshot, &live_after).is_clean(), "re-diff clean after drop index");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn malicious_type_in_descriptor_is_rejected_not_silently_mapped_to_text() {
    // #2: a malicious / unknown DSL type token must be REJECTED at the author
    // boundary, NOT silently degraded to a `text` column (the old `_ => text`
    // fallback let a SQL-injection payload — and any typo — become a `text`
    // column the creator never declared). The injection can never reach DDL
    // because `desired_snapshot` errors out before any column is emitted.
    let cfg = cfg_for(&token());

    let desc = CollectionDescriptor {
        name: "safe".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "f".into(),
            ty: "text; DROP TABLE control.users; --".into(),
            required: false,
            unique: false,
            references: None,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let err = desired_snapshot(&cfg.project_schema, &[desc]).unwrap_err();
    assert!(
        matches!(err, DeclarativeError::UnsupportedType { .. }),
        "got {err:?}"
    );
}

#[compio::test]
async fn vector_type_is_accepted_and_maps_to_vector_column() {
    // Schema-authority P2: `vector` was REJECTED by the v1 subset differ; now the
    // engine adopts the shared kernel's FULL type map, so a `t.vector(dims)` field
    // is ACCEPTED and DDLs to a `vector(N)` column — the capability gained by
    // reuse. (It is never silently degraded to `text`.)
    let cfg = cfg_for(&token());
    let desc = CollectionDescriptor {
        name: "embeddings".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "vec".into(),
            ty: "vector".into(),
            vector_dims: Some(384),
            vector_metric: Some("cosine".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("vector accepted");
    let col = desired.snapshot.tables["embeddings"]
        .columns
        .iter()
        .find(|c| c.name == "vec")
        .expect("vec column modelled");
    assert_eq!(col.data_type, "vector(384)", "vector dims carried into the column type");
}

// ---------------------------------------------------------------------------
// T12 — vector-ANN + FTS indexes routed through the access-method dimension.
// ---------------------------------------------------------------------------

#[compio::test]
async fn t12_vector_field_models_ivfflat_ann_index_and_renders_using_ivfflat() {
    // A `t.vector(dims, { metric })` field must model an ANN index with
    // `access_method = "ivfflat"` and the metric opclass — and the differ must
    // render `USING ivfflat ("vec" vector_cosine_ops) WITH (lists = 100)`.
    // Pre-T12 the differ modeled NO vector index (the ivfflat the data plane
    // built was unknown to it → phantom-DROP / vector search falls back to a flat
    // scan). No pgvector extension needed — this asserts the modeled snapshot +
    // generated DDL only (the real ivfflat apply round-trip is the capstone's job
    // on the pgvector :5434 instance).
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);
    let desc = CollectionDescriptor {
        name: "embeddings".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "vec".into(),
            ty: "vector".into(),
            vector_dims: Some(384),
            vector_metric: Some("cosine".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired");

    // (a) the modeled ANN index.
    let ann = desired.snapshot.tables["embeddings"]
        .indexes
        .iter()
        .find(|i| i.access_method == "ivfflat")
        .expect("an ivfflat ANN index must be modeled for the vector field");
    assert_eq!(ann.name, "embeddings_vec_idx", "matches plugin-db index_name");
    assert_eq!(ann.columns, vec!["vec".to_string()]);
    assert_eq!(ann.opclass.as_deref(), Some("vector_cosine_ops"));

    // (b) the rendered CREATE INDEX DDL uses ivfflat + opclass + lists param.
    let plan = author
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff");
    let idx_sql = plan
        .migrations
        .iter()
        .find(|m| m.name == "create_index_embeddings_vec_idx")
        .map(|m| m.up.clone())
        .expect("vector index migration");
    assert!(
        idx_sql.contains("USING ivfflat (\"vec\" vector_cosine_ops)")
            && idx_sql.contains("WITH (lists = 100)"),
        "vector index DDL must be ivfflat with the cosine opclass + lists: {idx_sql}"
    );
}

#[compio::test]
async fn t12_l2_and_inner_product_metrics_pick_the_right_opclass() {
    let cfg = cfg_for(&token());
    for (metric, opclass) in [
        ("l2", "vector_l2_ops"),
        ("innerProduct", "vector_ip_ops"),
        ("cosine", "vector_cosine_ops"),
    ] {
        let desc = CollectionDescriptor {
            name: "e".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor {
                name: "v".into(),
                ty: "vector".into(),
                vector_dims: Some(8),
                vector_metric: Some(metric.into()),
                ..Default::default()
            }],
            indexes: vec![],
        };
        let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired");
        let ann = desired.snapshot.tables["e"]
            .indexes
            .iter()
            .find(|i| i.access_method == "ivfflat")
            .expect("ann index");
        assert_eq!(
            ann.opclass.as_deref(),
            Some(opclass),
            "metric {metric} must map to {opclass}"
        );
    }
}

#[compio::test]
async fn t12_fts_field_round_trips_to_zero_drift_via_gin_index() {
    // THE FTS oracle: a `.fts()`-marked text column folds into a `__fts` GENERATED
    // tsvector column + a `<coll>__fts_idx` GIN index. Apply the generated
    // migration on REAL Postgres (tsvector + GIN are core — no extension), then
    // re-snapshot and assert ZERO drift. Pre-T12 the FTS facet was stripped at the
    // IR boundary → no `__fts` column / no GIN index, and any out-of-band FTS
    // index phantom-DROPped because the differ could not model `access_method`.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "articles".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor {
                name: "title".into(),
                ty: "string".into(),
                required: true,
                fts: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: "body".into(),
                ty: "string".into(),
                fts: true,
                ..Default::default()
            },
        ],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired");

    // The generated table DDL must carry the GENERATED `__fts` column and the
    // create-index DDL must be `USING gin`.
    let plan = author
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff");
    let create_sql: String = plan
        .migrations
        .iter()
        .find(|m| m.name == "create_table_articles")
        .map(|m| m.up.clone())
        .expect("create_table_articles");
    assert!(
        create_sql.contains("\"__fts\" tsvector GENERATED ALWAYS AS (to_tsvector("),
        "the __fts column must be a STORED generated tsvector: {create_sql}"
    );
    let idx_sql: String = plan
        .migrations
        .iter()
        .find(|m| m.name == "create_index_articles__fts_idx")
        .map(|m| m.up.clone())
        .expect("fts index migration");
    assert!(
        idx_sql.contains("USING gin (\"__fts\")"),
        "the FTS index must be a GIN index over __fts: {idx_sql}"
    );

    // Apply on real PG, then re-snapshot and assert ZERO drift (the round-trip).
    apply_plan(&engine, &desired, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply FTS plan");
    let live2 = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("re-snapshot");
    let drift = diff_snapshots(&desired.snapshot, &live2);
    assert!(
        drift.is_clean(),
        "FTS schema must re-diff clean: missing={:?} unexpected={:?} altered={:?}",
        drift.missing_objects,
        drift.unexpected_objects,
        drift.altered_objects
    );

    // And the live GIN index is really there with the gin access method.
    let fts_idx = live2.tables["articles"]
        .indexes
        .iter()
        .find(|i| i.name == "articles__fts_idx")
        .expect("the GIN __fts index exists in live");
    assert_eq!(fts_idx.access_method, "gin", "the FTS index is a GIN index");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn t12_fts_added_to_existing_table_applies_in_order_and_re_diffs_clean() {
    // Adding a `.fts()` field to an EXISTING table must add the `__fts` GENERATED
    // column AND its GIN index in dependency order (the column migration's
    // UUIDv7 version precedes the index's, so the engine applies the column
    // first). Then re-diff to ZERO.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // v1: a plain table, no FTS.
    let v1 = CollectionDescriptor {
        name: "notes".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "title".into(),
            ty: "string".into(),
            required: true,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired1");
    apply_plan(&engine, &desired1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply v1");
    let live1 = snapshot_schema(&conn, &cfg.project_schema).await.expect("live1");

    // v2: mark `title` as fts → __fts column + GIN index appear on the live table.
    let v2 = CollectionDescriptor {
        name: "notes".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "title".into(),
            ty: "string".into(),
            required: true,
            fts: true,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired2");
    apply_plan(&engine, &desired2, &live1, &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply v2 (add fts to existing table)");

    let live2 = snapshot_schema(&conn, &cfg.project_schema).await.expect("live2");
    let drift = diff_snapshots(&desired2.snapshot, &live2);
    assert!(
        drift.is_clean(),
        "adding fts to an existing table must re-diff clean: {drift:?}"
    );
    assert!(
        live2.tables["notes"].indexes.iter().any(|i| i.name == "notes__fts_idx" && i.access_method == "gin"),
        "the GIN __fts index landed on the existing table"
    );

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// T13 — geoPoint live round-trip (PostGIS-backed).
//
// `geoPoint` was only ever asserted at the type-mapping level
// (`p2_goodies_are_accepted_not_rejected`): the descriptor maps to
// `geography(POINT, 4326)`. But the runtime data plane
// (`plugin-db::SpatialIndex::ensure_spatial_index`) builds a `USING GIST`
// spatial index over that column — and the engine (the sole schema authority)
// must MODEL that same GiST index in the desired snapshot or the live index
// phantom-DROPs (and spatial `ST_DWithin` search degrades to a full scan), the
// SAME class of bug T12 fixed for vector/FTS. This test proves the engine emits
// the geography column + the `<table>_<col>_idx` GiST index, applies them on a
// REAL PostGIS Postgres, and re-diffs to ZERO drift.
//
// PostGIS is not on the default :5440 image, so this connects to a separate
// PostGIS-capable instance via `MIGRATE_POSTGIS_DB` and SKIPS (does not fail)
// when none is reachable / the extension is absent — the same skip discipline
// the pgvector capstone uses.
// ---------------------------------------------------------------------------

const DEFAULT_POSTGIS_DSN: &str =
    "host=localhost port=5435 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn postgis_dsn() -> String {
    std::env::var("MIGRATE_POSTGIS_DB").unwrap_or_else(|_| DEFAULT_POSTGIS_DSN.to_string())
}

/// Connect to the PostGIS-capable instance and ensure the `postgis` extension is
/// present, or `None` (⇒ skip the test) when the instance is unreachable or
/// PostGIS cannot be installed.
async fn postgis_pg() -> Option<Client> {
    let (client, conn) = match compio_postgres::connect(&postgis_dsn(), compio_postgres::NoTls).await
    {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("SKIP: PostGIS instance unreachable ({e})");
            return None;
        }
    };
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    if client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS postgis")
        .await
        .is_err()
    {
        eprintln!("SKIP: PostGIS extension not available on the target instance");
        return None;
    }
    Some(client)
}

#[compio::test]
async fn t13_geopoint_field_round_trips_to_zero_drift_via_gist_index() {
    // THE geoPoint oracle: a `t.geoPoint()` field maps to a `geography(POINT,
    // 4326)` column AND emits a `<table>_<col>_idx` GiST index (mirroring
    // plugin-db's runtime `ensure_spatial_index`). Apply the generated migration
    // on REAL PostGIS, then re-snapshot and assert ZERO drift. Pre-T13 the engine
    // modeled NO geo index → the runtime-built GiST index phantom-DROPped.
    let Some(conn) = postgis_pg().await else {
        return; // skip — no PostGIS
    };
    let tok = token();
    let cfg = cfg_with_role(&tok);
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "places".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "loc".into(),
            ty: "geoPoint".into(),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("geoPoint accepted");

    // (a) the desired snapshot models the geography column…
    assert_eq!(
        desired.snapshot.tables["places"]
            .columns
            .iter()
            .find(|c| c.name == "loc")
            .expect("loc column")
            .data_type,
        "geography(POINT, 4326)",
        "geoPoint maps to a geography(POINT, 4326) column"
    );
    // …and a GiST index over it, named like plugin-db's runtime index.
    let geo_idx = desired.snapshot.tables["places"]
        .indexes
        .iter()
        .find(|i| i.access_method == "gist")
        .expect("a GiST spatial index must be modeled for the geoPoint field");
    assert_eq!(geo_idx.name, "places_loc_idx", "GiST index name matches plugin-db");
    assert_eq!(geo_idx.columns, vec!["loc".to_string()]);

    // (b) the generated CREATE INDEX DDL is `USING gist ("loc")`.
    let plan = author
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff");
    let idx_sql: String = plan
        .migrations
        .iter()
        .find(|m| m.name == "create_index_places_loc_idx")
        .map(|m| m.up.clone())
        .expect("geo index migration");
    assert!(
        idx_sql.contains("USING gist (\"loc\")"),
        "the geo index DDL must be a GiST index over loc: {idx_sql}"
    );

    // (c) apply on real PostGIS, re-snapshot, assert ZERO drift (the round-trip).
    apply_plan(&engine, &desired, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply geoPoint plan on PostGIS");
    let live2 = snapshot_schema(&conn, &cfg.project_schema)
        .await
        .expect("re-snapshot");
    let drift = diff_snapshots(&desired.snapshot, &live2);
    assert!(
        drift.is_clean(),
        "geoPoint schema must re-diff clean: missing={:?} unexpected={:?} altered={:?}",
        drift.missing_objects,
        drift.unexpected_objects,
        drift.altered_objects
    );

    // (d) the live GiST index is really present with the gist access method.
    let live_idx = live2.tables["places"]
        .indexes
        .iter()
        .find(|i| i.name == "places_loc_idx")
        .expect("the GiST spatial index exists in live");
    assert_eq!(live_idx.access_method, "gist", "the geo index is a GiST index");

    // (e) the geography column accepts a real point (PostGIS is functional, not
    //     just a type alias) and a `ST_DWithin` lookup uses the column.
    conn.execute(
        &format!(
            "INSERT INTO \"{}\".\"places\" (id, loc, created_at, updated_at, version) \
             VALUES ($1, ST_MakePoint($2, $3)::geography, now(), now(), 1)",
            cfg.project_schema
        ),
        &[&"plc_sf", &(-122.4194f64), &37.7749f64],
    )
    .await
    .expect("insert a geography point");
    let near = conn
        .query(
            &format!(
                "SELECT id FROM \"{}\".\"places\" \
                 WHERE ST_DWithin(loc, ST_MakePoint($1, $2)::geography, $3)",
                cfg.project_schema
            ),
            &[&(-122.42f64), &37.77f64, &5000f64],
        )
        .await
        .expect("ST_DWithin spatial query runs against the geography column");
    assert_eq!(near.len(), 1, "the point is within 5km of the query origin");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn typo_type_token_is_rejected_not_silently_text() {
    // #2: a typo / wrong spelling (`bigint` is a Postgres type, not a DSL token)
    // is rejected, not silently degraded to `text`.
    let cfg = cfg_for(&token());
    let desc = CollectionDescriptor {
        name: "t".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "n".into(),
            ty: "bigint".into(),
            required: false,
            unique: false,
            references: None,
            ..Default::default()
        }],
        indexes: vec![],
    };
    let err = desired_snapshot(&cfg.project_schema, &[desc]).unwrap_err();
    assert!(
        matches!(err, DeclarativeError::UnsupportedType { ref ty } if ty == "bigint"),
        "got {err:?}"
    );
}

#[compio::test]
async fn all_twelve_supported_types_still_map() {
    // #2 regression: the twelve in-scope tokens must all still map (the fix
    // removed the `_ => text` fallback, not any real mapping).
    use zeroship_migrate::dsl_to_pg_data_type;
    for (tok, expected) in [
        ("string", "text"),
        ("ref", "text"),
        ("actor", "text"),
        ("id", "text"),
        ("number", "double precision"),
        ("boolean", "boolean"),
        ("date", "timestamp with time zone"),
        ("calendarDate", "date"),
        ("json", "jsonb"),
        ("object", "jsonb"),
        ("array", "jsonb"),
        ("union", "jsonb"),
        ("bytes", "bytea"),
    ] {
        assert_eq!(
            dsl_to_pg_data_type(tok).expect("supported type maps"),
            expected,
            "type {tok}"
        );
    }
}

#[compio::test]
async fn conflicting_declaration_across_apps_is_rejected_not_silently_merged() {
    // P4 (refines #6): two apps declaring the SAME table with DIFFERENT shapes is
    // a conflict — never a silent last-writer-wins merge that would drop one app's
    // column with no signal. app_a declares `a`; app_b declares `b`.
    let cfg = cfg_for(&token());
    let first = CollectionDescriptor {
        name: "dup".into(),
        owner_app: "app_a".into(),
        fields: vec![FieldDescriptor { name: "a".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let second = CollectionDescriptor {
        name: "dup".into(),
        owner_app: "app_b".into(),
        fields: vec![FieldDescriptor { name: "b".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let err = desired_snapshot(&cfg.project_schema, &[first, second]).unwrap_err();
    assert!(
        matches!(
            err,
            DeclarativeError::ConflictingDeclaration { ref table, ref apps }
                if table == "dup" && apps == &["app_a".to_string(), "app_b".to_string()]
        ),
        "got {err:?}"
    );
}

#[compio::test]
async fn conflict_app_pair_is_deterministic_regardless_of_descriptor_order() {
    // P4 determinism: the ConflictingDeclaration `apps` set is reported sorted
    // REGARDLESS of which descriptor came first. A critic could otherwise see a
    // different error per input order; lock order-independence.
    let cfg = cfg_for(&token());
    let mk = |app: &str, field: &str| CollectionDescriptor {
        name: "dup".into(),
        owner_app: app.into(),
        fields: vec![FieldDescriptor { name: field.into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    // Order A: app_z first, then app_a.
    let err_a = desired_snapshot(&cfg.project_schema, &[mk("app_z", "z"), mk("app_a", "a")]).unwrap_err();
    // Order B: app_a first, then app_z.
    let err_b = desired_snapshot(&cfg.project_schema, &[mk("app_a", "a"), mk("app_z", "z")]).unwrap_err();
    for err in [&err_a, &err_b] {
        assert!(
            matches!(
                err,
                DeclarativeError::ConflictingDeclaration { table, apps }
                    if table == "dup" && apps == &["app_a".to_string(), "app_z".to_string()]
            ),
            "conflict must report sorted [app_a, app_z] regardless of order, got {err:?}"
        );
    }
}

#[compio::test]
async fn conflict_with_three_declarers_reports_deterministic_set_across_permutations() {
    // 1b: with 3 apps declaring the same table — two IDENTICAL (A,B share shapeX)
    // and one DIFFERENT (C shapeY) — the reported conflict must be IDENTICAL across
    // every descriptor permutation. The old code reported `order_pair(slot_owner,
    // latecomer)` on the first mismatch, so the pair flapped with which identical
    // twin (A or B) held the slot when C arrived. Now the full sorted declarer set
    // is reported, which is order-invariant.
    let cfg = cfg_for(&token());
    // A and B are byte-identical (shapeX: field `x`); C differs (shapeY: field `y`).
    let mk = |app: &str, field: &str| CollectionDescriptor {
        name: "tbl".into(),
        owner_app: app.into(),
        fields: vec![FieldDescriptor { name: field.into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let a = || mk("app_a", "x");
    let b = || mk("app_b", "x"); // identical shape to A
    let c = || mk("app_c", "y"); // different shape

    let permutations: [[CollectionDescriptor; 3]; 3] = [
        [a(), b(), c()],
        [b(), c(), a()],
        [c(), a(), b()],
    ];
    let expected = vec!["app_a".to_string(), "app_b".to_string(), "app_c".to_string()];
    for perm in permutations {
        let err = desired_snapshot(&cfg.project_schema, &perm).unwrap_err();
        match err {
            DeclarativeError::ConflictingDeclaration { table, apps } => {
                assert_eq!(table, "tbl", "table name");
                assert_eq!(
                    apps, expected,
                    "the conflict must report the SAME sorted declarer set for every permutation"
                );
            }
            other => panic!("expected ConflictingDeclaration, got {other:?}"),
        }
    }
}

#[compio::test]
async fn identical_redeclaration_by_two_apps_is_idempotent_one_table_no_error() {
    // P4 union (design §4): two apps declaring the SAME table with the SAME shape
    // is IDEMPOTENT — it merges to ONE table in the union, no error. Ownership is
    // the lexicographically-smallest declaring app (order-independent).
    let cfg = cfg_for(&token());
    let mk = |app: &str| CollectionDescriptor {
        name: "shared".into(),
        owner_app: app.into(),
        fields: vec![
            FieldDescriptor { name: "title".into(), ty: "string".into(), required: true, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "count".into(), ty: "number".into(), required: false, unique: false, references: None, ..Default::default() },
        ],
        indexes: vec![IndexDescriptor { name: "shared_title_idx".into(), columns: vec!["title".into()], unique: false }],
    };
    // app_b declared first, app_a second — identical shape.
    let d = desired_snapshot(&cfg.project_schema, &[mk("app_b"), mk("app_a")])
        .expect("identical re-declaration is idempotent, not an error");
    // Exactly ONE table in the union (merged, not duplicated).
    assert_eq!(d.snapshot.tables.len(), 1, "identical re-decl must merge to one table");
    assert!(d.snapshot.tables.contains_key("shared"));
    // Owner is the smallest declaring app, regardless of descriptor order.
    assert_eq!(d.owner_of("shared"), Some("app_a"), "owner is the smallest declaring app");

    // And the reverse order yields the SAME union + owner (determinism).
    let d_rev = desired_snapshot(&cfg.project_schema, &[mk("app_a"), mk("app_b")])
        .expect("idempotent");
    assert_eq!(d.snapshot, d_rev.snapshot, "union is order-independent");
    assert_eq!(d.ownership, d_rev.ownership, "ownership is order-independent");
}

#[compio::test]
async fn single_app_declaration_owns_its_table() {
    // P4: the existing single-app path is unchanged — one descriptor → one table,
    // owned by its declaring app.
    let cfg = cfg_for(&token());
    let desc = CollectionDescriptor {
        name: "widgets".into(),
        owner_app: "app_solo".into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot");
    assert_eq!(d.snapshot.tables.len(), 1);
    assert_eq!(d.owner_of("widgets"), Some("app_solo"));
}

// ---------------------------------------------------------------------------
// P4 Feature 2 — ownership enforcement on a deploy.
// ---------------------------------------------------------------------------

#[compio::test]
async fn non_owner_deploy_changing_only_own_tables_is_fine() {
    // P4 ownership: app_b's deploy that adds/changes ONLY a b-owned table is
    // allowed, even though the union also carries an a-owned table. The diff is
    // over the full union, so a_table is in `desired`; because it equals live
    // (a_table already exists, unchanged) it produces NO op and the owner check
    // never fires for it.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();

    // a_app creates its table (deploy 1, as a_app).
    let a_tbl = || CollectionDescriptor {
        name: "a_table".into(),
        owner_app: "app_a".into(),
        fields: vec![FieldDescriptor { name: "x".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let union_v1 = desired_snapshot(&cfg.project_schema, &[a_tbl()]).expect("union v1");
    apply_plan(&engine, &union_v1, &SchemaSnapshot::default(), &author_app(&cfg, "app_a"), &cfg, &conn, Approval::None)
        .await
        .expect("a_app creates a_table");

    // Now b_app deploys: the union is { a_table (a_app), b_table (b_app) }. Diffed
    // against live (which has a_table), only b_table is new.
    let b_tbl = CollectionDescriptor {
        name: "b_table".into(),
        owner_app: "app_b".into(),
        fields: vec![FieldDescriptor { name: "y".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let union_v2 = desired_snapshot(&cfg.project_schema, &[a_tbl(), b_tbl]).expect("union v2");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");

    // app_b is the deploying app. The diff must succeed (only b_table changes) and
    // touch a_table with NO op.
    let plan = engine
        .plan_declarative(&union_v2, &live, &HashMap::new(), &author_app(&cfg, "app_b"), &[], &guard_cfg(&cfg))
        .expect("app_b deploy adding only its own table is allowed");
    assert!(
        plan.plain.items.iter().all(|m| m.migration.up.contains("b_table")),
        "every op must target b_table, got {:?}",
        plan.plain.items.iter().map(|m| &m.migration.name).collect::<Vec<_>>()
    );
    engine.apply(&plan.plain, Approval::None, &conn, &cfg, "app_b").await.expect("apply b_table");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn non_owner_deploy_altering_a_foreign_owned_table_is_refused() {
    // P4 ownership: a deploy whose union/desired would STRUCTURALLY change a table
    // owned by another app is refused with NotTableOwner. Here the union has an
    // a-owned `authors` table that is not yet live; app_b's deploy would CREATE it
    // → refused (a non-owner may use a table but not migrate it).
    let cfg = cfg_for(&token());

    let authors = CollectionDescriptor {
        name: "authors".into(),
        owner_app: "app_a".into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let union = desired_snapshot(&cfg.project_schema, &[authors]).expect("union");
    // Live is empty → the union would CREATE authors, a structural change.
    let live = SchemaSnapshot::default();

    // Deploying as app_b (a non-owner of authors) → refused.
    let err = author_app(&cfg, "app_b")
        .diff(&union, &live, &HashMap::new(), &[])
        .unwrap_err();
    assert!(
        matches!(
            err,
            DeclarativeError::NotTableOwner { ref table, ref owner, ref deploying_app }
                if table == "authors" && owner == "app_a" && deploying_app == "app_b"
        ),
        "got {err:?}"
    );

    // Deploying as app_a (the owner) → fine.
    let migs = author_app(&cfg, "app_a")
        .diff(&union, &live, &HashMap::new(), &[])
        .expect("the owner may create its own table")
        .migrations;
    assert!(migs.iter().any(|m| m.up.contains("authors")), "owner's diff creates authors");
}

#[compio::test]
async fn non_owner_using_a_foreign_table_unchanged_is_a_noop_not_refused() {
    // P4 ownership: a non-owner that merely USES an a-owned table (the table in the
    // union EQUALS live, no structural delta) produces NO op and is NOT refused —
    // the owner check only fires on an actual structural CHANGE. This is the
    // "identical re-declaration by a non-owner is a no-op" guarantee.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();

    // a_app creates `authors`.
    let authors = || CollectionDescriptor {
        name: "authors".into(),
        owner_app: "app_a".into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let union_v1 = desired_snapshot(&cfg.project_schema, &[authors()]).expect("union v1");
    apply_plan(&engine, &union_v1, &SchemaSnapshot::default(), &author_app(&cfg, "app_a"), &cfg, &conn, Approval::None)
        .await
        .expect("a_app creates authors");

    // b_app re-declares the SAME authors shape (it uses the table) — identical, so
    // the union owner stays app_a (smallest). Deploy as app_b: authors == live ⇒
    // no op ⇒ no NotTableOwner.
    let union_v2 = desired_snapshot(&cfg.project_schema, &[authors()]).expect("union v2");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let plan = author_app(&cfg, "app_b")
        .diff(&union_v2, &live, &HashMap::new(), &[])
        .expect("a non-owner merely using an unchanged foreign table is a no-op");
    assert!(plan.is_empty(), "no structural change ⇒ empty plan, got {:?}",
        plan.migrations.iter().map(|m| &m.name).collect::<Vec<_>>());

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// P4 Feature 2b — fail-closed drop ownership (a partial union must not
// mass-drop other tenants' tables).
// ---------------------------------------------------------------------------

#[compio::test]
async fn partial_union_deploy_refuses_to_drop_a_foreign_owned_live_table() {
    // 2b (THE priority): if the caller passes only ONE app's descriptors (a PARTIAL
    // union) while live carries another app's table, the foreign table looks
    // "absent from desired" → the OLD code emitted a gated DROP of it, authored by
    // the deploying app. That is cross-tenant data loss bounded only by the
    // destructive-approval gate. The differ must now REFUSE the drop fail-closed:
    // live_ownership says `a_table` is owned by app_a, the deploying app is app_b →
    // NotTableOwner, and NO foreign DROP is emitted.
    let cfg = cfg_for(&token());
    // Live: a single a-owned table.
    let a_tbl = CollectionDescriptor {
        name: "a_table".into(),
        owner_app: "app_a".into(),
        fields: vec![FieldDescriptor { name: "x".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let live = desired_snapshot(&cfg.project_schema, &[a_tbl]).expect("live snapshot").snapshot;

    // app_b deploys a PARTIAL union: only its OWN table (omitting a_table).
    let b_tbl = CollectionDescriptor {
        name: "b_table".into(),
        owner_app: "app_b".into(),
        fields: vec![FieldDescriptor { name: "y".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let partial = desired_snapshot(&cfg.project_schema, &[b_tbl]).expect("partial union");
    // The caller's live_ownership: a_table is a_app's. (b_table is not live yet.)
    let live_ownership: HashMap<String, String> =
        std::iter::once(("a_table".to_string(), "app_a".to_string())).collect();

    let err = author_app(&cfg, "app_b")
        .diff(&partial, &live, &live_ownership, &[])
        .unwrap_err();
    assert!(
        matches!(
            err,
            DeclarativeError::NotTableOwner { ref table, ref owner, ref deploying_app }
                if table == "a_table" && owner == "app_a" && deploying_app == "app_b"
        ),
        "a partial-union deploy must REFUSE dropping a foreign table, not author a gated DROP; got {err:?}"
    );
}

#[compio::test]
async fn owner_dropping_its_own_table_is_allowed_when_live_ownership_confirms_it() {
    // 2b: the owner legitimately removing its OWN table is still allowed — the
    // fail-closed guard only refuses drops it cannot confirm belong to the
    // deploying app. live_ownership{posts: app_a} + deploying_app=app_a ⇒ the gated
    // DROP is authored (not refused).
    let cfg = cfg_for(&token());
    let posts = CollectionDescriptor {
        name: "posts".into(),
        owner_app: "app_a".into(),
        fields: vec![FieldDescriptor { name: "body".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let live = desired_snapshot(&cfg.project_schema, &[posts]).expect("live").snapshot;
    // app_a now declares nothing → it removes its own posts table.
    let empty = DesiredSchema::default();
    let live_ownership: HashMap<String, String> =
        std::iter::once(("posts".to_string(), "app_a".to_string())).collect();

    let plan = author_app(&cfg, "app_a")
        .diff(&empty, &live, &live_ownership, &[])
        .expect("the owner may drop its own table");
    assert!(
        plan.migrations.iter().any(|m| m.up.contains("DROP TABLE") && m.up.contains("posts")),
        "owner's diff must author the gated DROP TABLE posts, got {:?}",
        plan.migrations.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    // And the drop is the gated/destructive kind.
    assert!(
        plan.migrations.iter().any(|m| m.flags.destructive && m.flags.requires_approval),
        "the owner's own-table drop is still gated"
    );
}

#[compio::test]
async fn dropping_a_live_table_with_unknown_ownership_fails_closed() {
    // 2b fail-closed default: if live_ownership has NO entry for a live table being
    // dropped, the differ must REFUSE (DropOfUnownedTable) — it will not author a
    // destructive drop of a table whose ownership it cannot confirm belongs to the
    // deploying app. (Even though the descriptor was stamped app_a and the deployer
    // is app_a, the AUTHORITATIVE ownership signal is live_ownership, not the
    // omitted descriptor — and here it is silent.)
    let cfg = cfg_for(&token());
    let orphan = CollectionDescriptor {
        name: "orphan".into(),
        owner_app: "app_a".into(),
        fields: vec![FieldDescriptor { name: "v".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let live = desired_snapshot(&cfg.project_schema, &[orphan]).expect("live").snapshot;
    let empty = DesiredSchema::default();
    // Empty live_ownership → ownership of `orphan` is UNKNOWN to the diff.
    let unknown: HashMap<String, String> = HashMap::new();

    let err = author_app(&cfg, "app_a")
        .diff(&empty, &live, &unknown, &[])
        .unwrap_err();
    assert!(
        matches!(err, DeclarativeError::DropOfUnownedTable { ref table } if table == "orphan"),
        "a drop of an ownership-unknown live table must fail closed; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// P4 Feature 3 — cross-app FK ordering.
// ---------------------------------------------------------------------------

#[compio::test]
async fn cross_app_fk_to_a_union_table_orders_and_applies_clean() {
    // P4 cross-app FK: app_a owns `authors`; app_b declares
    // `books(author_id REFERENCES authors)`. With authors already live, app_b's
    // deploy creates books with the FK pointing at a_app's table — the deferred-FK
    // + depends_on (P1) machinery orders it and it applies clean.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let engine = MigrationEngine::new();

    // app_a creates authors (deploy 1).
    let authors = || CollectionDescriptor {
        name: "authors".into(),
        owner_app: "app_a".into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let union_v1 = desired_snapshot(&cfg.project_schema, &[authors()]).expect("union v1");
    apply_plan(&engine, &union_v1, &SchemaSnapshot::default(), &author_app(&cfg, "app_a"), &cfg, &conn, Approval::None)
        .await
        .expect("app_a creates authors");

    // app_b declares books with a cross-app FK → authors (owned by app_a, live).
    let books = CollectionDescriptor {
        name: "books".into(),
        owner_app: "app_b".into(),
        fields: vec![
            FieldDescriptor { name: "title".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "author".into(), ty: "ref".into(), required: false, unique: false, references: Some("authors".into()), ..Default::default() },
        ],
        indexes: vec![],
    };
    let union_v2 = desired_snapshot(&cfg.project_schema, &[authors(), books]).expect("union v2");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");

    // app_b's deploy: authors is live+unchanged (no op, no owner violation), books
    // is new with a cross-app FK. Plan + apply must succeed.
    let plan = engine
        .plan_declarative(&union_v2, &live, &HashMap::new(), &author_app(&cfg, "app_b"), &[], &guard_cfg(&cfg))
        .expect("cross-app FK to a union/live table plans clean");
    engine.apply(&plan.plain, Approval::None, &conn, &cfg, "app_b").await.expect("apply books with cross-app FK");

    // The whole union re-diffs clean (FK constraint materialised, byte-equal).
    let live2 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    let drift = diff_snapshots(&union_v2.snapshot, &live2);
    assert!(drift.missing_objects.is_empty(), "missing after cross-app FK: {:?}", drift.missing_objects);
    let fk_present = live2.tables["books"].constraints.iter().any(|c| c.kind == "FOREIGN KEY");
    assert!(fk_present, "books must carry the cross-app FK to authors");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn cross_app_fk_to_a_table_no_app_declares_is_a_clear_error() {
    // P4 cross-app FK: a `ref` whose target table is declared by NO member app and
    // is not live is a CLEAR error (CrossAppFkTargetMissing), surfaced before any
    // SQL renders — not a bad-SQL failure at apply.
    let cfg = cfg_for(&token());
    let books = CollectionDescriptor {
        name: "books".into(),
        owner_app: "app_b".into(),
        fields: vec![FieldDescriptor {
            name: "author".into(),
            ty: "ref".into(),
            required: false,
            unique: false,
            references: Some("ghosts".into()), // no app declares `ghosts`
            ..Default::default()
        }],
        indexes: vec![],
    };
    let union = desired_snapshot(&cfg.project_schema, &[books]).expect("union builds");
    let live = SchemaSnapshot::default();
    let err = author_app(&cfg, "app_b").diff(&union, &live, &HashMap::new(), &[]).unwrap_err();
    assert!(
        matches!(
            err,
            DeclarativeError::CrossAppFkTargetMissing { ref table, ref target }
                if table == "books" && target == "ghosts"
        ),
        "got {err:?}"
    );
}


#[compio::test]
async fn malicious_ref_target_is_rejected_at_author_boundary() {
    // #3-ref: a `ref` field's target table is interpolated into the FK's
    // REFERENCES clause. A schema-qualified or injecting target must be rejected
    // as an invalid identifier up-front, before any SQL renders.
    let cfg = cfg_for(&token());
    for bad in ["control.users", "x\"; DROP TABLE control.users; --", ";", ""] {
        let desc = CollectionDescriptor {
            name: "child".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor {
                name: "parent".into(),
                ty: "ref".into(),
                required: false,
                unique: false,
                references: Some(bad.into()),
                ..Default::default()
            }],
            indexes: vec![],
        };
        let err = desired_snapshot(&cfg.project_schema, &[desc]).unwrap_err();
        // Fail-closed either way: a dot-qualified target trips the cross-app FK
        // guard (#2, the more specific rejection — it crosses a schema boundary),
        // a non-dotted malformed target trips the bare-ident validation. Both are
        // rejected at the author boundary before any SQL renders.
        assert!(
            matches!(
                err,
                DeclarativeError::Invalid(_) | DeclarativeError::CrossAppFkForbidden { .. }
            ),
            "ref target {bad:?} must be rejected at the author boundary, got {err:?}"
        );
    }
}


#[compio::test]
async fn dropping_a_unique_index_is_gated_dropping_a_plain_index_is_not() {
    // #4: dropping a UNIQUE index silently removes a data-integrity guarantee
    // (duplicate rows become possible; a later re-add fails on dirty data), so
    // it must be classified destructive + requires_approval (gated, like DROP
    // COLUMN). A PLAIN index DROP stays ungated (reversible, perf-only).
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // Create a table carrying BOTH a plain named index and a unique named index.
    let v1 = CollectionDescriptor {
        name: "members".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "tier".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
        ],
        indexes: vec![
            IndexDescriptor { name: "members_email_uq".into(), columns: vec!["email".into()], unique: true },
            IndexDescriptor { name: "members_tier_idx".into(), columns: vec!["tier".into()], unique: false },
        ],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create members");

    // --- Drop the UNIQUE index: must be gated. ---
    let v2 = CollectionDescriptor {
        name: "members".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "tier".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
        ],
        indexes: vec![
            // unique index dropped; plain index kept.
            IndexDescriptor { name: "members_tier_idx".into(), columns: vec!["tier".into()], unique: false },
        ],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let plan = engine
        .plan_declarative(&d2, &live, &HashMap::new(), &author, &[], &guard_cfg(&cfg))
        .expect("plan drop unique index");
    assert!(plan.plain.destructive, "DROP of a UNIQUE index must be destructive");
    assert!(plan.plain.requires_approval, "DROP of a UNIQUE index must require approval");

    // Refused without approval; the unique index still present.
    let err = engine.apply(&plan.plain, Approval::None, &conn, &cfg, "app_test").await.unwrap_err();
    assert!(matches!(err, EngineError::ApprovalRequired), "got {err:?}");
    let live_after = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    assert!(
        live_after.tables["members"].indexes.iter().any(|i| i.name == "members_email_uq"),
        "unique index must survive a refused drop"
    );

    // Approved → applied, the unique index is gone, re-diff clean.
    engine.apply(&plan.plain, Approval::Approved, &conn, &cfg, "app_test").await.expect("approved drop");
    let live_final = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap3");
    assert!(
        !live_final.tables["members"].indexes.iter().any(|i| i.name == "members_email_uq"),
        "unique index must be gone after an approved drop"
    );
    assert!(diff_snapshots(&d2.snapshot, &live_final).is_clean(), "re-diff clean after approved unique-index drop");

    // --- Now drop the PLAIN index: must stay UNGATED. ---
    let v3 = CollectionDescriptor {
        name: "members".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            FieldDescriptor { name: "tier".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
        ],
        indexes: vec![],
    };
    let d3 = desired_snapshot(&cfg.project_schema, &[v3]).expect("desired_snapshot");
    let live3 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap4");
    let plan3 = engine
        .plan_declarative(&d3, &live3, &HashMap::new(), &author, &[], &guard_cfg(&cfg))
        .expect("plan drop plain index");
    assert!(!plan3.plain.destructive, "DROP of a PLAIN index must NOT be destructive");
    assert!(!plan3.plain.requires_approval, "DROP of a PLAIN index must NOT require approval");
    engine.apply(&plan3.plain, Approval::None, &conn, &cfg, "app_test").await.expect("ungated plain drop applies");
    let live5 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap5");
    assert!(
        !live5.tables["members"].indexes.iter().any(|i| i.name == "members_tier_idx"),
        "plain index must be gone after the ungated drop"
    );

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// P3 Feature 1 — rename via explicit hints → expand-contract (NEVER heuristic).
// ---------------------------------------------------------------------------

#[compio::test]
async fn rename_hint_routes_drop_add_through_expand_contract_not_drop_add() {
    // P3 Feature 1: a desired schema that renames `email` → `email_address`, WITH
    // a matching RenameHint, must emit the zero-downtime EXPAND-CONTRACT sequence
    // (online E1/E2/E3 + gated C1/C2) — NOT a bare gated-drop + additive-add. The
    // expand structural steps apply, the dual-write trigger mirrors writes, and
    // the pre-existing row's data is PRESERVED (never dropped).
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // Create `users` with an `email` column (table owned by the migrator role, so
    // the dual-write trigger DDL works under SET ROLE).
    let v1 = CollectionDescriptor {
        name: "users".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create users");

    // Seed a pre-existing row whose `email` value MUST survive the rename. The
    // platform system fields are NOT NULL (no DB-side default — the SDK runtime
    // fills them), so the test supplies them explicitly.
    let schema = &cfg.project_schema;
    conn.batch_execute(&format!(
        "INSERT INTO \"{schema}\".\"users\" (id, created_at, updated_at, version, email) \
         VALUES ('usr_1', NOW(), NOW(), 1, 'keep@x.test')"
    ))
    .await
    .expect("seed row");

    // Desire `email_address` instead of `email`.
    let v2 = CollectionDescriptor {
        name: "users".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "email_address".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");

    let hints = vec![RenameHint {
        table: "users".into(),
        from: "email".into(),
        to: "email_address".into(),
    }];

    // The diff WITH the hint carries the rename STRUCTURED (in `.renames`, NOT
    // flattened into the plain `.migrations`) — C1. A pure rename produces zero
    // plain migrations (its from/to are excluded from the plain drop/add passes).
    // `all_migrations()` flattens it back out for SHAPE inspection only — this is
    // NOT the apply path (the real apply is `apply_declarative` → `run_expand`,
    // exercised by the zero-data-loss e2e below).
    let diff = author.diff(&d2, &live, &HashMap::new(), &hints).expect("diff with rename hint");
    assert_eq!(diff.renames.len(), 1, "exactly one structured rename");
    assert!(
        diff.migrations.is_empty(),
        "a pure rename produces NO plain migrations: {:?}",
        diff.migrations.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
    let migs = diff.all_migrations();
    // No bare DROP COLUMN email / ADD COLUMN email_address as INDEPENDENT ops:
    // the only column-drop is the gated CONTRACT C2 (online, requires_approval),
    // and the only column-add is the EXPAND E1 (online). Assert by flags + names.
    let online: Vec<&Migration> = migs.iter().filter(|m| m.flags.online).collect();
    assert!(
        online.len() == 5,
        "rename must emit the 5-step expand-contract sequence, got {} online of {}: {:?}",
        online.len(),
        migs.len(),
        migs.iter().map(|m| (&m.name, m.flags.online)).collect::<Vec<_>>()
    );
    // Every column-touching migration is online (no bare drop/add).
    for m in &migs {
        if m.up.contains("DROP COLUMN") || m.up.contains("ADD COLUMN") {
            assert!(m.flags.online, "a bare (non-online) DROP/ADD COLUMN leaked: {}", m.name);
        }
    }
    // Expand has E1(add)/E2(trigger)/E3(backfill); contract has C1/C2 (gated).
    let expand: Vec<&Migration> = migs.iter().filter(|m| m.flags.phase == Some(OnlinePhase::Expand)).collect();
    let contract: Vec<&Migration> = migs.iter().filter(|m| m.flags.phase == Some(OnlinePhase::Contract)).collect();
    assert_eq!(expand.len(), 3, "E1/E2/E3");
    assert_eq!(contract.len(), 2, "C1/C2");
    assert!(contract.iter().all(|m| m.flags.requires_approval), "contract steps gated");
    assert!(contract.iter().any(|m| m.flags.destructive), "contract DROP COLUMN destructive");
    // E1 adds the NEW column (nullable, transactional); not destructive/gated.
    let e1 = expand.iter().find(|m| m.up.contains("ADD COLUMN")).expect("E1 add column");
    assert!(e1.up.contains("\"email_address\""), "E1 adds email_address: {}", e1.up);
    assert!(!e1.flags.destructive && !e1.flags.requires_approval, "E1 is additive-safe");

    // Apply ONLY the EXPAND structural steps (E1 add-column + E2 dual-write
    // trigger) through the real executor (guard + migrator role). E3 is the no-op
    // backfill marker; the real backfill is run_expand's job — here we prove the
    // trigger mirrors + the pre-existing data is preserved.
    let structural: Vec<Migration> = expand
        .iter()
        .filter(|m| !m.up.contains("backfill marker"))
        .map(|m| (*m).clone())
        .collect();
    executor_apply(&conn, &cfg, &structural, Approval::Approved, "app_test")
        .await
        .expect("apply expand structural (E1+E2)");

    // The pre-existing row's data is PRESERVED: `email` still holds its value
    // (the rename ADDED a column, it did not drop the old one yet).
    let row = conn
        .query_one(
            &format!("SELECT email, email_address FROM \"{schema}\".\"users\" WHERE id='usr_1'"),
            &[],
        )
        .await
        .expect("read seeded row");
    let email: Option<String> = row.get("email");
    assert_eq!(email.as_deref(), Some("keep@x.test"), "pre-existing email preserved");

    // The dual-write trigger mirrors a NEW write through BOTH columns: writing the
    // OLD name fills the NEW name (coexistence model).
    conn.batch_execute(&format!(
        "INSERT INTO \"{schema}\".\"users\" (id, created_at, updated_at, version, email) \
         VALUES ('usr_2', NOW(), NOW(), 1, 'dual@x.test')"
    ))
    .await
    .expect("insert via old column");
    let row2 = conn
        .query_one(
            &format!("SELECT email, email_address FROM \"{schema}\".\"users\" WHERE id='usr_2'"),
            &[],
        )
        .await
        .expect("read dual-written row");
    let e2_old: Option<String> = row2.get("email");
    let e2_new: Option<String> = row2.get("email_address");
    assert_eq!(e2_old.as_deref(), Some("dual@x.test"), "old column written");
    assert_eq!(
        e2_new.as_deref(),
        Some("dual@x.test"),
        "dual-write trigger mirrored old → new (data preserved through rename)"
    );

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn declarative_rename_preserves_preexisting_rows_through_expand_then_contract() {
    // C1 (CRITICAL — data loss) regression. A declarative rename `<from>`→`<to>`
    // is an ONLINE, multi-deploy op: the pre-existing-row mirror is the REAL
    // backfill (run via `run_expand`), NOT E3's `SELECT 1` marker. The bug was
    // that `diff` flattened the rename's ExpandContractPlan and DISCARDED the
    // BackfillSpec, so the plain `plan`→`apply` path never copied pre-existing
    // rows, then the contract `DROP COLUMN <from>` destroyed them.
    //
    // This test drives the rename through `apply_declarative` (deploy N: plain +
    // EXPAND with the real backfill) and then applies the deferred contract
    // (deploy N+1) — and asserts EVERY pre-existing row's value lands in `<to>`
    // and survives the drop of `<from>`. ZERO data loss.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();
    let schema = &cfg.project_schema;

    // Create `users` with an `email` column (owned by the migrator role so the
    // dual-write trigger DDL works under SET ROLE).
    let v1 = CollectionDescriptor {
        name: "users".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create users");

    // Seed THREE pre-existing rows whose `email` value MUST survive the rename
    // (these are exactly the rows the backfill — not the dual-write trigger — is
    // responsible for: they predate the trigger).
    for (id, email) in [("usr_1", "a@x.test"), ("usr_2", "b@x.test"), ("usr_3", "c@x.test")] {
        conn.batch_execute(&format!(
            "INSERT INTO \"{schema}\".\"users\" (id, created_at, updated_at, version, email) \
             VALUES ('{id}', NOW(), NOW(), 1, '{email}')"
        ))
        .await
        .expect("seed pre-existing row");
    }

    // Desire `email_address` instead of `email`, WITH a matching rename hint.
    let v2 = CollectionDescriptor {
        name: "users".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "email_address".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let hints = vec![RenameHint {
        table: "users".into(),
        from: "email".into(),
        to: "email_address".into(),
    }];

    // Plan the declarative deploy: the rename is carried STRUCTURED (in
    // `.renames`), the plain set is empty (pure rename).
    let plan = engine
        .plan_declarative(&d2, &live, &HashMap::new(), &author, &hints, &guard_cfg(&cfg))
        .expect("plan_declarative");
    assert_eq!(plan.renames.len(), 1, "one structured rename");
    assert!(plan.plain.items.is_empty(), "a pure rename has no plain migrations");

    // === Deploy N: apply_declarative drives the EXPAND through run_expand, which
    // runs the REAL backfill. Approval is required (the backfill mutates data). ===
    let outcome = engine
        .apply_declarative(&plan, Approval::Approved, &conn, &cfg, "app_test")
        .await
        .expect("apply_declarative (expand + real backfill)");

    // The contract is DEFERRED, not applied this deploy (multi-deploy partition).
    assert_eq!(outcome.pending_contract.len(), 2, "contract C1+C2 deferred");
    assert!(
        outcome.pending_contract.iter().all(|m| m.flags.requires_approval),
        "deferred contract steps are gated"
    );
    assert!(
        outcome.pending_contract.iter().any(|m| m.flags.destructive),
        "the deferred DROP COLUMN is destructive"
    );

    // After the EXPAND: `<from>` is STILL present and EVERY pre-existing row's
    // value has been mirrored into `<to>` by the real backfill — the whole point.
    let rows = conn
        .query(
            &format!("SELECT id, email, email_address FROM \"{schema}\".\"users\" ORDER BY id"),
            &[],
        )
        .await
        .expect("read rows after expand");
    assert_eq!(rows.len(), 3, "all three pre-existing rows present");
    for row in &rows {
        let id: String = row.get("id");
        let from: Option<String> = row.get("email");
        let to: Option<String> = row.get("email_address");
        assert!(from.is_some(), "{id}: <from> still present after expand");
        assert_eq!(
            to, from,
            "{id}: backfill mirrored pre-existing <from> into <to> (the C1 bug: this was NULL)"
        );
    }

    // The dual-write trigger mirrors a NEW write both ways (coexistence model):
    // write via the OLD name, read it back on BOTH columns.
    conn.batch_execute(&format!(
        "INSERT INTO \"{schema}\".\"users\" (id, created_at, updated_at, version, email) \
         VALUES ('usr_4', NOW(), NOW(), 1, 'd@x.test')"
    ))
    .await
    .expect("insert via old column during transition");
    let r4 = conn
        .query_one(
            &format!("SELECT email, email_address FROM \"{schema}\".\"users\" WHERE id='usr_4'"),
            &[],
        )
        .await
        .expect("read dual-written row");
    let r4_from: Option<String> = r4.get("email");
    let r4_to: Option<String> = r4.get("email_address");
    assert_eq!(r4_from.as_deref(), Some("d@x.test"), "old column written");
    assert_eq!(r4_to.as_deref(), Some("d@x.test"), "dual-write mirrored old → new");

    // === Deploy N+1: app code has switched to `<to>`; apply the DEFERRED contract
    // (DROP TRIGGER C1 + DROP COLUMN <from> C2) via the normal gated apply. ===
    let contract_plan = engine.plan(&outcome.pending_contract, &guard_cfg(&cfg));
    engine
        .apply(&contract_plan, Approval::Approved, &conn, &cfg, "app_test")
        .await
        .expect("apply deferred contract (drop trigger + drop <from>)");

    // `<from>` is GONE; `<to>` retains ALL original data — ZERO loss.
    let after = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap final");
    let cols = &after.tables["users"].columns;
    assert!(!cols.iter().any(|c| c.name == "email"), "<from> dropped by the contract");
    assert!(cols.iter().any(|c| c.name == "email_address"), "<to> remains");

    let final_rows = conn
        .query(
            &format!("SELECT id, email_address FROM \"{schema}\".\"users\" ORDER BY id"),
            &[],
        )
        .await
        .expect("read final rows");
    let got: Vec<(String, Option<String>)> = final_rows
        .iter()
        .map(|r| (r.get::<_, String>("id"), r.get::<_, Option<String>>("email_address")))
        .collect();
    assert_eq!(
        got,
        vec![
            ("usr_1".into(), Some("a@x.test".into())),
            ("usr_2".into(), Some("b@x.test".into())),
            ("usr_3".into(), Some("c@x.test".into())),
            ("usr_4".into(), Some("d@x.test".into())),
        ],
        "every row's data survived rename expand→contract in <to> (ZERO data loss)"
    );

    // The deploy converged: re-diffing desired vs live is clean.
    assert!(diff_snapshots(&d2.snapshot, &after).is_clean(), "re-diff clean after rename completes");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn flattening_a_rename_into_the_plain_plan_loses_data_or_is_gate_blocked() {
    // C1 RED witness: the OLD behaviour — flatten the rename's ExpandContractPlan
    // into the plain set and push it through `plan`→`apply` (discarding the
    // BackfillSpec) — is BROKEN. This reproduces what the bug did. It asserts the
    // flat path either (a) is refused by the executor's expand/contract gate
    // (contract pending alongside its own expand), or (b) "succeeds" but DESTROYS
    // the pre-existing row's data (the backfill never ran). Either outcome proves
    // the flat path must NOT be the apply path — `apply_declarative` is.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();
    let schema = &cfg.project_schema;

    let v1 = CollectionDescriptor {
        name: "users".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create users");

    // A pre-existing row whose data the flat path is supposed to (but cannot) keep.
    conn.batch_execute(&format!(
        "INSERT INTO \"{schema}\".\"users\" (id, created_at, updated_at, version, email) \
         VALUES ('usr_1', NOW(), NOW(), 1, 'keep@x.test')"
    ))
    .await
    .expect("seed row");

    let v2 = CollectionDescriptor {
        name: "users".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "email_address".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let hints = vec![RenameHint {
        table: "users".into(),
        from: "email".into(),
        to: "email_address".into(),
    }];

    // Reconstruct the OLD flatten: `all_migrations()` is exactly the flat
    // `out.extend(plan.all())` the buggy `diff` produced (E1,E2,E3,C1,C2 inlined).
    let diff = author.diff(&d2, &live, &HashMap::new(), &hints).expect("diff");
    let flat = diff.all_migrations();
    let flat_plan = engine.plan(&flat, &guard_cfg(&cfg));

    // Push the flat batch through the gated apply with FULL approval (the kindest
    // case for the old path). It must NOT cleanly preserve the row's data.
    let result = engine.apply(&flat_plan, Approval::Approved, &conn, &cfg, "app_test").await;

    if result.is_err() {
        // (a) The executor's expand/contract gate refuses the contract while its
        // own expand is still pending in the same batch → dead-on-arrival, nothing
        // applied; the pre-existing row is untouched.
        let row = conn
            .query_one(
                &format!("SELECT email FROM \"{schema}\".\"users\" WHERE id='usr_1'"),
                &[],
            )
            .await
            .expect("row still readable (nothing applied)");
        let email: Option<String> = row.get("email");
        assert_eq!(email.as_deref(), Some("keep@x.test"), "gate-blocked: nothing applied");
    } else {
        // (b) It "applied" — then the pre-existing row's data is GONE: <from> was
        // dropped by C2 and the backfill never copied the value to <to> (the
        // marker E3 ran as a no-op SELECT). THIS is the data loss the C1 fix
        // closes by NOT flattening.
        let after = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap final");
        let cols = &after.tables["users"].columns;
        assert!(!cols.iter().any(|c| c.name == "email"), "flat path dropped <from>");
        let row = conn
            .query_one(
                &format!("SELECT email_address FROM \"{schema}\".\"users\" WHERE id='usr_1'"),
                &[],
            )
            .await
            .expect("read row");
        let to: Option<String> = row.get("email_address");
        assert_eq!(
            to, None,
            "flat path LOST the pre-existing value (backfill never ran) — proves the C1 bug"
        );
    }

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn without_a_hint_the_same_desired_is_two_independent_ops_not_a_rename() {
    // P3 Feature 1 (the no-heuristic guarantee): the EXACT desired/live pair that
    // a hint would turn into a rename, WITHOUT the hint, stays two INDEPENDENT ops
    // — a gated DROP of `email` + an additive ADD of `email_address`. The differ
    // must NEVER infer a rename from a drop+add pair (that risks silent data
    // loss). No online/expand-contract migration is emitted.
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    let live = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let desired = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "email_address".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };

    // No hints → two independent ops, NO rename (the structured `.renames` is
    // empty; everything is in the plain `.migrations`).
    let diff = author.diff(&desired, &live.snapshot, &HashMap::new(), &[]).expect("diff without hint");
    assert!(diff.renames.is_empty(), "no hint must produce NO structured rename");
    let migs = diff.migrations;
    assert!(
        migs.iter().all(|m| !m.flags.online),
        "no hint must NOT produce any online/expand-contract migration: {:?}",
        migs.iter().map(|m| (&m.name, m.flags.online)).collect::<Vec<_>>()
    );
    // Exactly: a gated DROP COLUMN email + an additive ADD COLUMN email_address.
    let drop = migs.iter().find(|m| m.up.contains("DROP COLUMN") && m.up.contains("\"email\""))
        .expect("a bare DROP COLUMN email");
    assert!(drop.flags.destructive && drop.flags.requires_approval, "DROP COLUMN is gated");
    let add = migs.iter().find(|m| m.up.contains("ADD COLUMN") && m.up.contains("\"email_address\""))
        .expect("a bare ADD COLUMN email_address");
    assert!(!add.flags.destructive && !add.flags.requires_approval, "ADD COLUMN is additive");
}

#[compio::test]
async fn rename_hint_naming_a_nonexistent_column_is_an_error() {
    // P3 Feature 1: a hint that does NOT match an actual drop+add pair is a hard
    // error (RenameHintUnmatched) — never silently ignored (a swallowed hint would
    // fall back to an unintended gated-drop + additive-add, losing the column's
    // data).
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    let live = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let desired = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "email_address".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };

    // `from` names a column that does not exist in live → unmatched.
    let bad_from = vec![RenameHint { table: "users".into(), from: "nope".into(), to: "email_address".into() }];
    let err = author.diff(&desired, &live.snapshot, &HashMap::new(), &bad_from).unwrap_err();
    assert!(matches!(err, DeclarativeError::RenameHintUnmatched { .. }), "got {err:?}");

    // `to` names a column that does not exist in desired → unmatched.
    let bad_to = vec![RenameHint { table: "users".into(), from: "email".into(), to: "ghost".into() }];
    let err2 = author.diff(&desired, &live.snapshot, &HashMap::new(), &bad_to).unwrap_err();
    assert!(matches!(err2, DeclarativeError::RenameHintUnmatched { .. }), "got {err2:?}");

    // A hint on a table that is identical on both sides (no drop+add) → unmatched.
    let same = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let hint = vec![RenameHint { table: "users".into(), from: "email".into(), to: "email_address".into() }];
    let err3 = author.diff(&same, &live.snapshot, &HashMap::new(), &hint).unwrap_err();
    assert!(matches!(err3, DeclarativeError::RenameHintUnmatched { .. }), "got {err3:?}");
}

#[compio::test]
async fn rename_hint_with_a_type_mismatch_is_an_error() {
    // P3 Feature 1: a hint that matches a drop+add pair whose TYPES differ is
    // refused (RenameHintTypeMismatch) — a pure online rename requires type
    // identity; rename + type change is two separate intents.
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    let live = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "score".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let desired = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            // renamed AND retyped (string → number).
            fields: vec![FieldDescriptor { name: "rating".into(), ty: "number".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let hint = vec![RenameHint { table: "users".into(), from: "score".into(), to: "rating".into() }];
    let err = author.diff(&desired, &live.snapshot, &HashMap::new(), &hint).unwrap_err();
    assert!(matches!(err, DeclarativeError::RenameHintTypeMismatch { .. }), "got {err:?}");
}

// ---------------------------------------------------------------------------
// P3 Feature 1 — cross-hint rename-hint validation (H1/H2/M1/M2).
//
// `resolve_rename_hints` validates each hint independently (from live-only, to
// desired-only, type identity). These tests lock the CROSS-hint guards: no
// duplicate from/to per table (H1), no rename chains (H2), no no-op from==to
// (M1), and the (already-correct) CREATE'd/DROP'd-table rejection (M2).
// ---------------------------------------------------------------------------

#[compio::test]
async fn two_hints_sharing_a_to_are_rejected_as_duplicate() {
    // H1: `[a→c, b→c]` — two hints targeting the SAME new column `c`. Resolved
    // independently they'd both emit `ADD COLUMN c` (the second fails
    // `already exists`). The cross-hint pass rejects it as a duplicate `to`.
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    let live = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![
                FieldDescriptor { name: "a".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
                FieldDescriptor { name: "b".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            ],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let desired = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            // Both old columns map onto a single new column `c`.
            fields: vec![FieldDescriptor { name: "c".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let hints = vec![
        RenameHint { table: "users".into(), from: "a".into(), to: "c".into() },
        RenameHint { table: "users".into(), from: "b".into(), to: "c".into() },
    ];
    let err = author.diff(&desired, &live.snapshot, &HashMap::new(), &hints).unwrap_err();
    assert!(
        matches!(err, DeclarativeError::DuplicateRenameHint { side: "to", .. }),
        "got {err:?}"
    );
}

#[compio::test]
async fn two_hints_sharing_a_from_are_rejected_as_duplicate() {
    // H1: `[a→c, a→d]` — two hints renaming the SAME old column `a`. Resolved
    // independently they'd both drop `a` (double `DROP COLUMN a`). The cross-hint
    // pass rejects it as a duplicate `from`.
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    let live = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "a".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let desired = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![
                FieldDescriptor { name: "c".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
                FieldDescriptor { name: "d".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            ],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let hints = vec![
        RenameHint { table: "users".into(), from: "a".into(), to: "c".into() },
        RenameHint { table: "users".into(), from: "a".into(), to: "d".into() },
    ];
    let err = author.diff(&desired, &live.snapshot, &HashMap::new(), &hints).unwrap_err();
    assert!(
        matches!(err, DeclarativeError::DuplicateRenameHint { side: "from", .. }),
        "got {err:?}"
    );
}

#[compio::test]
async fn a_rename_chain_is_rejected_explicitly_not_incidentally() {
    // H2: `[a→b, b→c]` — `b` is both the target of one hint and the source of
    // another. This is a CHAIN; reject it with the explicit RenameHintChained
    // error (NOT an incidental RenameHintUnmatched).
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    let live = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![
                FieldDescriptor { name: "a".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
                FieldDescriptor { name: "b".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            ],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let desired = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![
                FieldDescriptor { name: "b".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
                FieldDescriptor { name: "c".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() },
            ],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let hints = vec![
        RenameHint { table: "users".into(), from: "a".into(), to: "b".into() },
        RenameHint { table: "users".into(), from: "b".into(), to: "c".into() },
    ];
    let err = author.diff(&desired, &live.snapshot, &HashMap::new(), &hints).unwrap_err();
    assert!(
        matches!(err, DeclarativeError::RenameHintChained { ref column, .. } if column == "b"),
        "got {err:?}"
    );
}

#[compio::test]
async fn a_noop_rename_hint_from_equals_to_is_a_precise_error() {
    // M1: `from == to` is a no-op rename. It must produce the PRECISE
    // RenameHintNoop error, not the misleading RenameHintUnmatched.
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    let live = {
        let desc = CollectionDescriptor {
            name: "users".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    // Same shape on both sides; the only "change" is the no-op hint.
    let desired = live.clone();
    let hints = vec![RenameHint { table: "users".into(), from: "email".into(), to: "email".into() }];
    let err = author.diff(&desired, &live.snapshot, &HashMap::new(), &hints).unwrap_err();
    assert!(
        matches!(err, DeclarativeError::RenameHintNoop { ref column, .. } if column == "email"),
        "got {err:?}"
    );
}

#[compio::test]
async fn a_hint_on_a_created_table_is_unmatched() {
    // M2: a hint on a table absent from LIVE (being CREATE'd) cannot be a rename
    // (a rename is in-place on an existing table) → RenameHintUnmatched. Locks the
    // already-correct `live.tables.get` guard.
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    // Live has NO `posts` table; desired CREATEs it.
    let live = SchemaSnapshot::default();
    let desired = {
        let desc = CollectionDescriptor {
            name: "posts".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "body".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let hints = vec![RenameHint { table: "posts".into(), from: "old".into(), to: "body".into() }];
    let err = author.diff(&desired, &live, &HashMap::new(), &hints).unwrap_err();
    assert!(matches!(err, DeclarativeError::RenameHintUnmatched { .. }), "got {err:?}");
}

#[compio::test]
async fn a_hint_on_a_dropped_table_is_unmatched() {
    // M2: a hint on a table absent from DESIRED (being DROP'd) cannot be a rename
    // → RenameHintUnmatched. Locks the already-correct `desired.tables.get` guard.
    let cfg = cfg_for(&token());
    let author = author_for(&cfg);

    // Live has `posts`; desired DROPs it (empty desired).
    let live = {
        let desc = CollectionDescriptor {
            name: "posts".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "body".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
            indexes: vec![],
        };
        desired_snapshot(&cfg.project_schema, &[desc]).expect("desired_snapshot")
    };
    let desired = DesiredSchema::default();
    let hints = vec![RenameHint { table: "posts".into(), from: "body".into(), to: "renamed".into() }];
    let err = author.diff(&desired, &live.snapshot, &HashMap::new(), &hints).unwrap_err();
    assert!(matches!(err, DeclarativeError::RenameHintUnmatched { .. }), "got {err:?}");
}

// ---------------------------------------------------------------------------
// P3 Feature 2 — type + nullability changes (gated type / SET NOT NULL, ungated
// DROP NOT NULL).
// ---------------------------------------------------------------------------

#[compio::test]
async fn type_change_is_gated_refused_without_approval_applied_with_then_re_diffs_clean() {
    // P3 Feature 2: a column type change (string/text → number/double precision)
    // emits a GATED ALTER COLUMN TYPE. It is refused without approval (nothing
    // applied), applied with Approval::Approved, then re-diffs clean + idempotent.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // Create `metrics` with a `score` text column (empty table → the cast applies
    // cleanly regardless of data).
    let v1 = CollectionDescriptor {
        name: "metrics".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "score".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create metrics");

    // Desire `score` as a number (double precision) — a gated type change.
    let v2 = CollectionDescriptor {
        name: "metrics".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "score".into(), ty: "number".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let plan = engine
        .plan_declarative(&d2, &live, &HashMap::new(), &author, &[], &guard_cfg(&cfg))
        .expect("plan type change");
    assert!(plan.plain.destructive, "a type change must be destructive");
    assert!(plan.plain.requires_approval, "a type change must require approval");

    // Refused without approval; the column type is UNCHANGED (text).
    let err = engine.apply(&plan.plain, Approval::None, &conn, &cfg, "app_test").await.unwrap_err();
    assert!(matches!(err, EngineError::ApprovalRequired), "got {err:?}");
    let live_after = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    let attr = live_after.tables["metrics"].columns.iter().find(|c| c.name == "score").expect("score col");
    assert_eq!(attr.data_type, "text", "type must be unchanged after a refused type change");

    // Approved → applied; the type is now double precision, re-diff clean.
    engine.apply(&plan.plain, Approval::Approved, &conn, &cfg, "app_test").await.expect("approved type change");
    let live_final = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap3");
    let attr2 = live_final.tables["metrics"].columns.iter().find(|c| c.name == "score").expect("score col");
    assert_eq!(attr2.data_type, "double precision", "type changed after approval");
    assert!(diff_snapshots(&d2.snapshot, &live_final).is_clean(), "re-diff clean after approved type change");
    // Idempotent: re-diffing the same desired against the converged live is empty.
    let migs = author.diff(&d2, &live_final, &HashMap::new(), &[]).expect("re-diff after type change").migrations;
    assert!(migs.is_empty(), "type change must be idempotent, got {} migs", migs.len());

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn set_not_null_is_gated_drop_not_null_is_ungated_and_both_re_diff_clean() {
    // P3 Feature 2: tightening required false→true (SET NOT NULL) is lock-heavy +
    // can fail on existing NULLs → GATED; relaxing true→false (DROP NOT NULL) is
    // safe → UNGATED. Both converge to a clean re-diff.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    // Create `accounts` with a NULLABLE `email` (required:false).
    let v1 = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create accounts");

    // --- SET NOT NULL (required false→true): GATED. ---
    let v2 = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "email".into(), ty: "string".into(), required: true, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let plan = engine
        .plan_declarative(&d2, &live, &HashMap::new(), &author, &[], &guard_cfg(&cfg))
        .expect("plan set not null");
    // SET NOT NULL is gated (requires_approval) but NOT destructive (no data lost).
    assert!(plan.plain.requires_approval, "SET NOT NULL must require approval");
    assert!(!plan.plain.destructive, "SET NOT NULL is not data loss");

    // Refused without approval; the column is still nullable.
    let err = engine.apply(&plan.plain, Approval::None, &conn, &cfg, "app_test").await.unwrap_err();
    assert!(matches!(err, EngineError::ApprovalRequired), "got {err:?}");
    let live_after = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    assert!(live_after.tables["accounts"].columns.iter().find(|c| c.name == "email").unwrap().nullable,
        "email must stay nullable after a refused SET NOT NULL");

    // Approved → applied; column NOT NULL now, re-diff clean.
    engine.apply(&plan.plain, Approval::Approved, &conn, &cfg, "app_test").await.expect("approved set not null");
    let live_nn = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap3");
    assert!(!live_nn.tables["accounts"].columns.iter().find(|c| c.name == "email").unwrap().nullable,
        "email must be NOT NULL after approval");
    assert!(diff_snapshots(&d2.snapshot, &live_nn).is_clean(), "re-diff clean after SET NOT NULL");
    assert!(author.diff(&d2, &live_nn, &HashMap::new(), &[]).expect("re-diff").is_empty(), "SET NOT NULL idempotent");

    // --- DROP NOT NULL (required true→false): UNGATED, applies without approval. ---
    let live3 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap4");
    let plan3 = engine
        .plan_declarative(&d1, &live3, &HashMap::new(), &author, &[], &guard_cfg(&cfg))
        .expect("plan drop not null");
    assert!(!plan3.plain.requires_approval, "DROP NOT NULL must NOT require approval");
    assert!(!plan3.plain.destructive, "DROP NOT NULL is not data loss");
    engine.apply(&plan3.plain, Approval::None, &conn, &cfg, "app_test").await.expect("ungated drop not null applies");
    let live_dn = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap5");
    assert!(live_dn.tables["accounts"].columns.iter().find(|c| c.name == "email").unwrap().nullable,
        "email must be nullable again after the ungated DROP NOT NULL");
    assert!(diff_snapshots(&d1.snapshot, &live_dn).is_clean(), "re-diff clean after DROP NOT NULL");
    assert!(author.diff(&d1, &live_dn, &HashMap::new(), &[]).expect("re-diff").is_empty(), "DROP NOT NULL idempotent");

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn unapplied_gated_type_change_keeps_re_diffing_nonempty_documented_semantics() {
    // P3 Feature 2 idempotency semantics: a GATED-but-UNAPPLIED type change leaves
    // the plan non-empty until it is approved + applied. This is correct (and
    // documents the data-integrity note): the diff reflects DESIRED-vs-LIVE, so an
    // un-applied change re-diffs to the same gated migration every time — it is
    // NEVER silently dropped, NEVER auto-applied. The plan converges to empty only
    // AFTER an approved apply makes live match desired.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let v1 = CollectionDescriptor {
        name: "gauge".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "v".into(), ty: "string".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("desired_snapshot");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create gauge");

    let v2 = CollectionDescriptor {
        name: "gauge".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "v".into(), ty: "number".into(), required: false, unique: false, references: None, ..Default::default() }],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("desired_snapshot");

    // Re-diff TWICE without applying: each time yields the SAME non-empty gated
    // type change (never silently dropped, never auto-applied).
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let migs1 = author.diff(&d2, &live, &HashMap::new(), &[]).expect("diff 1").migrations;
    let migs2 = author.diff(&d2, &live, &HashMap::new(), &[]).expect("diff 2").migrations;
    assert_eq!(migs1.len(), 1, "exactly one gated type change");
    assert_eq!(migs2.len(), 1, "still one — un-applied change is not dropped");
    assert!(migs1[0].flags.requires_approval && migs1[0].flags.destructive, "stays gated");

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Phase-1 parity #1 — FK on-delete / on-update / deferrable round-trip.
// ---------------------------------------------------------------------------

/// A `ref` field carrying an explicit FK policy.
fn ref_field(
    name: &str,
    target: &str,
    on_delete: Option<&str>,
    on_update: Option<&str>,
    deferrable: Option<bool>,
) -> FieldDescriptor {
    FieldDescriptor {
        name: name.into(),
        ty: "ref".into(),
        references: Some(target.into()),
        on_delete: on_delete.map(str::to_string),
        on_update: on_update.map(str::to_string),
        deferrable,
        ..Default::default()
    }
}

/// Apply a desired schema from empty, then assert the FULL re-diff is clean AND
/// the differ re-diffs to ZERO migrations — the lossless round-trip oracle.
async fn apply_and_assert_clean_roundtrip(
    engine: &MigrationEngine,
    author: &DeclarativeAuthor,
    cfg: &ExecutorConfig,
    conn: &Client,
    desired: &DesiredSchema,
) {
    apply_plan(engine, desired, &SchemaSnapshot::default(), author, cfg, conn, Approval::None)
        .await
        .expect("apply plan");
    let live2 = snapshot_schema(conn, &cfg.project_schema).await.expect("re-snapshot");
    let drift = diff_snapshots(&desired.snapshot, &live2);
    assert!(
        drift.is_clean(),
        "re-diff after apply must be clean: missing={:?} unexpected={:?} altered={:?}",
        drift.missing_objects,
        drift.unexpected_objects,
        drift.altered_objects
    );
    let migs = author.diff(desired, &live2, &HashMap::new(), &[]).expect("re-diff").migrations;
    assert!(
        migs.is_empty(),
        "re-diff must be EMPTY (lossless round-trip), got: {:?}",
        migs.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}

#[compio::test]
async fn fk_with_cascade_policy_round_trips_clean() {
    // #1: a CASCADE/CASCADE deferrable FK must apply and re-diff to ZERO. RED
    // pre-fix: the engine emitted a bare FK (no policy), so the live constraint
    // carried ON UPDATE/ON DELETE CASCADE that the desired snapshot lacked → a
    // phantom DROP+ADD on every re-diff (the verify-gate-bricking bug).
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let parent = CollectionDescriptor { name: "parent".into(), owner_app: "app_test".into(), fields: vec![], indexes: vec![] };
    let child = CollectionDescriptor {
        name: "child".into(),
        owner_app: "app_test".into(),
        fields: vec![ref_field("p", "parent", Some("cascade"), Some("cascade"), Some(true))],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[parent, child]).expect("desired_snapshot");
    apply_and_assert_clean_roundtrip(&engine, &author, &cfg, &conn, &desired).await;

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn fk_with_set_null_policy_round_trips_clean() {
    // #1: ON DELETE SET NULL (deferrable) round-trips clean.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let parent = CollectionDescriptor { name: "parent".into(), owner_app: "app_test".into(), fields: vec![], indexes: vec![] };
    let child = CollectionDescriptor {
        name: "child".into(),
        owner_app: "app_test".into(),
        // SET NULL on delete; default (restrict) on update; deferrable default true.
        fields: vec![ref_field("p", "parent", Some("set null"), None, None)],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[parent, child]).expect("desired_snapshot");
    apply_and_assert_clean_roundtrip(&engine, &author, &cfg, &conn, &desired).await;

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn fk_sdk_default_restrict_deferrable_round_trips_clean() {
    // #1: the SDK default for a bare t.ref() is restrict/restrict/deferrable=true.
    // A `references` with no explicit policy must round-trip clean (the most
    // common shape). RED pre-fix: the engine emitted a bare FK (no DEFERRABLE),
    // so live's DEFERRABLE INITIALLY DEFERRED phantom-drifted forever.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let parent = CollectionDescriptor { name: "authors".into(), owner_app: "app_test".into(), fields: vec![], indexes: vec![] };
    let child = CollectionDescriptor {
        name: "posts".into(),
        owner_app: "app_test".into(),
        // Bare references: every policy defaulted (restrict/restrict/deferrable).
        fields: vec![FieldDescriptor {
            name: "author".into(),
            ty: "ref".into(),
            references: Some("authors".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[parent, child]).expect("desired_snapshot");
    apply_and_assert_clean_roundtrip(&engine, &author, &cfg, &conn, &desired).await;

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn fk_no_action_non_deferrable_round_trips_clean() {
    // #1: an explicit NO ACTION / NO ACTION, deferrable=false FK renders with NO
    // policy clauses at all in pg_get_constraintdef — the desired snapshot must
    // omit them too, else phantom drift.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let parent = CollectionDescriptor { name: "parent".into(), owner_app: "app_test".into(), fields: vec![], indexes: vec![] };
    let child = CollectionDescriptor {
        name: "child".into(),
        owner_app: "app_test".into(),
        fields: vec![ref_field("p", "parent", Some("no action"), Some("no action"), Some(false))],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[parent, child]).expect("desired_snapshot");
    apply_and_assert_clean_roundtrip(&engine, &author, &cfg, &conn, &desired).await;

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Phase-1 parity #2 — cross-app FK rejection at the author boundary.
// ---------------------------------------------------------------------------

#[compio::test]
async fn cross_app_ref_target_is_rejected_fail_closed() {
    // #2: a `ref` to `<otherApp>.<table>` is a cross-app/cross-schema FK —
    // forbidden fail-closed (mirrors plugin-db's cross_app_fk.rs). Rejected at
    // the author boundary BEFORE any SQL is rendered.
    let cfg = cfg_for(&token());
    let desc = CollectionDescriptor {
        name: "posts".into(),
        owner_app: "app_test".into(),
        fields: vec![ref_field("author", "other_app.users", None, None, None)],
        indexes: vec![],
    };
    let err = desired_snapshot(&cfg.project_schema, &[desc]).unwrap_err();
    assert!(
        matches!(
            err,
            DeclarativeError::CrossAppFkForbidden { ref table, ref target, ref other_app }
                if table == "posts" && target == "other_app.users" && other_app == "other_app"
        ),
        "got {err:?}"
    );
}

#[compio::test]
async fn own_schema_bare_ref_target_is_allowed() {
    // #2 (counterpart): a bare same-project reference is allowed and applies clean.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let users = CollectionDescriptor { name: "users".into(), owner_app: "app_test".into(), fields: vec![], indexes: vec![] };
    let posts = CollectionDescriptor {
        name: "posts".into(),
        owner_app: "app_test".into(),
        fields: vec![ref_field("author", "users", None, None, None)],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[users, posts]).expect("bare ref allowed");
    apply_and_assert_clean_roundtrip(&engine, &author, &cfg, &conn, &desired).await;

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Phase-1 parity #3 — literal type + CHECK round-trip.
// ---------------------------------------------------------------------------

/// Assert a desired schema applies clean AND the differ re-diffs to ZERO
/// migrations, AND `diff_snapshots` shows no missing/unexpected and no NON-CHECK
/// altered objects. A CHECK constraint's pg_get_constraintdef-normalised BODY is
/// not byte-compared (PG rewrites it; plugin-db never re-diffs a CHECK), so a
/// `constraint <name>` `definition` altered_object is the only tolerated drift —
/// the constraint's PRESENCE (name + kind) round-trips, which is what matters.
async fn assert_clean_modulo_check_bodies(
    engine: &MigrationEngine,
    author: &DeclarativeAuthor,
    cfg: &ExecutorConfig,
    conn: &Client,
    desired: &DesiredSchema,
) {
    apply_plan(engine, desired, &SchemaSnapshot::default(), author, cfg, conn, Approval::None)
        .await
        .expect("apply plan");
    let live2 = snapshot_schema(conn, &cfg.project_schema).await.expect("re-snapshot");

    // The differ re-diffs to ZERO migrations (it never re-diffs CHECK bodies).
    let migs = author.diff(desired, &live2, &HashMap::new(), &[]).expect("re-diff").migrations;
    assert!(
        migs.is_empty(),
        "re-diff must be EMPTY (lossless round-trip), got: {:?}",
        migs.iter().map(|m| &m.name).collect::<Vec<_>>()
    );

    let drift = diff_snapshots(&desired.snapshot, &live2);
    assert!(drift.missing_objects.is_empty(), "missing: {:?}", drift.missing_objects);
    assert!(drift.unexpected_objects.is_empty(), "unexpected: {:?}", drift.unexpected_objects);
    // Only CHECK-constraint `definition` divergence is tolerated (PG normalises
    // the body); nothing else may have altered.
    let non_check: Vec<_> = drift
        .altered_objects
        .iter()
        .filter(|a| !(a.object.starts_with("constraint ") && a.field == "definition"))
        .collect();
    assert!(non_check.is_empty(), "non-CHECK-body drift: {non_check:?}");
    // The CHECK constraints must EXIST live (name + kind round-tripped).
    let live_checks: Vec<&str> = live2
        .tables
        .values()
        .flat_map(|t| t.constraints.iter())
        .filter(|c| c.kind == "CHECK")
        .map(|c| c.name.as_str())
        .collect();
    let want_checks: Vec<&str> = desired
        .snapshot
        .tables
        .values()
        .flat_map(|t| t.constraints.iter())
        .filter(|c| c.kind == "CHECK")
        .map(|c| c.name.as_str())
        .collect();
    for w in &want_checks {
        assert!(live_checks.contains(w), "CHECK {w} must exist live, got {live_checks:?}");
    }
}

#[compio::test]
async fn literal_field_maps_and_check_pins_value_round_trips() {
    // #3: a `literal` field maps to its primitive (text here) + a
    // CHECK (col = value). RED pre-fix: `literal` was UnsupportedType (rejected),
    // so no such column could be modelled at all.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "events".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor {
                name: "kind".into(),
                ty: "literal".into(),
                literal_value: Some(serde_json::json!("login")),
                ..Default::default()
            },
            FieldDescriptor {
                name: "count".into(),
                ty: "literal".into(),
                literal_value: Some(serde_json::json!(7)),
                ..Default::default()
            },
            FieldDescriptor {
                name: "flag".into(),
                ty: "literal".into(),
                literal_value: Some(serde_json::json!(true)),
                ..Default::default()
            },
        ],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("literal maps");
    // Type fidelity: kind→text, count→numeric, flag→boolean.
    let cols = &desired.snapshot.tables["events"].columns;
    assert_eq!(cols.iter().find(|c| c.name == "kind").unwrap().data_type, "text");
    assert_eq!(cols.iter().find(|c| c.name == "count").unwrap().data_type, "numeric");
    assert_eq!(cols.iter().find(|c| c.name == "flag").unwrap().data_type, "boolean");

    assert_clean_modulo_check_bodies(&engine, &author, &cfg, &conn, &desired).await;

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn bare_literal_with_no_value_is_rejected_not_silently_text() {
    // #3: a `literal` field carrying NO value is malformed — rejected as
    // UnsupportedType, never silently a `text` column.
    let cfg = cfg_for(&token());
    let desc = CollectionDescriptor {
        name: "t".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "x".into(), ty: "literal".into(), ..Default::default() }],
        indexes: vec![],
    };
    let err = desired_snapshot(&cfg.project_schema, &[desc]).unwrap_err();
    assert!(
        matches!(err, DeclarativeError::UnsupportedType { ref ty } if ty == "literal"),
        "got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Phase-1 parity #4 — enum / min-max / default (CHECK & DEFAULT) round-trip.
// ---------------------------------------------------------------------------

#[compio::test]
async fn column_with_default_and_enum_check_round_trips() {
    // #4: a column with a DEFAULT + an enum CHECK applies and re-diffs to ZERO.
    // The DEFAULT is emission-only (not drift-tracked, so it round-trips
    // vacuously — matching plugin-db, which never re-diffs defaults); the enum
    // CHECK round-trips at the name+kind level.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "tickets".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor {
                name: "status".into(),
                ty: "string".into(),
                default: Some(serde_json::json!("open")),
                enum_values: Some(vec![
                    serde_json::json!("open"),
                    serde_json::json!("closed"),
                    serde_json::json!("pending"),
                ]),
                ..Default::default()
            },
            FieldDescriptor {
                name: "priority".into(),
                ty: "number".into(),
                min: Some(1.0),
                max: Some(5.0),
                ..Default::default()
            },
        ],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired");
    // The DEFAULT is carried on the column (emission metadata).
    let status = desired.snapshot.tables["tickets"].columns.iter().find(|c| c.name == "status").unwrap();
    assert_eq!(status.default.as_deref(), Some("'open'"));
    // Two CHECKs: the status enum + the priority range.
    let checks: Vec<&str> = desired.snapshot.tables["tickets"]
        .constraints
        .iter()
        .filter(|c| c.kind == "CHECK")
        .map(|c| c.definition.as_str())
        .collect();
    assert!(checks.iter().any(|d| d.contains("IN (")), "enum CHECK present: {checks:?}");
    assert!(checks.iter().any(|d| d.contains(">= 1") && d.contains("<= 5")), "range CHECK present: {checks:?}");

    assert_clean_modulo_check_bodies(&engine, &author, &cfg, &conn, &desired).await;

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn add_column_with_immutable_default_is_additive_not_destructive() {
    // #4 volatile-default trap: ADD COLUMN with an IMMUTABLE literal default takes
    // PG's metadata-only fast path — it must be classified ADDITIVE (not
    // destructive/gated), since the engine never emits a volatile default
    // (plugin-db diff.rs:15-26). Add a defaulted column to an existing table and
    // assert the plan is ungated and applies clean.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let v1 = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), ..Default::default() }],
        indexes: vec![],
    };
    let d1 = desired_snapshot(&cfg.project_schema, &[v1]).expect("d1");
    apply_plan(&engine, &d1, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("create accounts");

    // Add a NOT NULL column WITH a literal default (the only way a NOT NULL
    // add-to-populated is safe — the default backfills existing rows on the fast
    // path).
    let v2 = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor { name: "name".into(), ty: "string".into(), ..Default::default() },
            FieldDescriptor {
                name: "tier".into(),
                ty: "string".into(),
                required: true,
                default: Some(serde_json::json!("free")),
                ..Default::default()
            },
        ],
        indexes: vec![],
    };
    let d2 = desired_snapshot(&cfg.project_schema, &[v2]).expect("d2");
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let plan = engine
        .plan_declarative(&d2, &live, &HashMap::new(), &author, &[], &guard_cfg(&cfg))
        .expect("plan add defaulted column");
    assert!(!plan.plain.destructive, "ADD COLUMN with immutable default is additive, not destructive");
    assert!(!plan.plain.requires_approval, "must not be gated");
    // Applies ungated; re-diff clean (modulo nothing — no CHECK here).
    engine.apply(&plan.plain, Approval::None, &conn, &cfg, "app_test").await.expect("apply add defaulted col");
    let live2 = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap2");
    assert!(diff_snapshots(&d2.snapshot, &live2).is_clean(), "add defaulted column converged");

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Phase-1 parity #5 — system-field id-fold + idPrefix validation.
// ---------------------------------------------------------------------------

#[compio::test]
async fn re_declaring_id_with_prefix_folds_into_the_system_pk_no_second_column() {
    // #5: a `{ type: "id", idPrefix }` field is a PREFIX declaration for the
    // system `id` PK, NOT a second column. It must FOLD — the table has exactly
    // ONE `id` column (the system TEXT PRIMARY KEY) and round-trips clean. RED
    // pre-fix: the field loop pushed a SECOND `id` column (duplicate column +
    // bogus PK), which fails to apply / phantom-drifts.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "posts".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor { name: "id".into(), ty: "id".into(), id_prefix: Some("post".into()), ..Default::default() },
            FieldDescriptor { name: "title".into(), ty: "string".into(), ..Default::default() },
        ],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("id-fold");
    // Exactly ONE `id` column (the system PK), not two.
    let id_cols = desired.snapshot.tables["posts"].columns.iter().filter(|c| c.name == "id").count();
    assert_eq!(id_cols, 1, "id must fold into the single system PK column, not duplicate");
    let id_col = desired.snapshot.tables["posts"].columns.iter().find(|c| c.name == "id").unwrap();
    assert_eq!(id_col.data_type, "text", "the system id is TEXT");
    assert!(!id_col.nullable, "the system id is NOT NULL (PK)");

    apply_and_assert_clean_roundtrip(&engine, &author, &cfg, &conn, &desired).await;

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn reserved_id_prefix_usr_is_rejected() {
    // #5: the platform-reserved `usr` prefix is rejected (it would mint ids
    // colliding with platform user ids).
    let cfg = cfg_for(&token());
    let desc = CollectionDescriptor {
        name: "t".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "id".into(), ty: "id".into(), id_prefix: Some("usr".into()), ..Default::default() }],
        indexes: vec![],
    };
    let err = desired_snapshot(&cfg.project_schema, &[desc]).unwrap_err();
    assert!(
        matches!(err, DeclarativeError::Invalid(ref m) if m.contains("reserved")),
        "got {err:?}"
    );
}

#[compio::test]
async fn malformed_id_prefix_is_rejected() {
    // #5: an idPrefix not matching ^[a-z][a-z0-9_]*$ is rejected at the boundary.
    let cfg = cfg_for(&token());
    for bad in ["Post", "1post", "po-st", "post!"] {
        let desc = CollectionDescriptor {
            name: "t".into(),
            owner_app: "app_test".into(),
            fields: vec![FieldDescriptor { name: "id".into(), ty: "id".into(), id_prefix: Some(bad.into()), ..Default::default() }],
            indexes: vec![],
        };
        let err = desired_snapshot(&cfg.project_schema, &[desc]).unwrap_err();
        assert!(matches!(err, DeclarativeError::Invalid(_)), "prefix {bad:?} must be rejected, got {err:?}");
    }
}

#[compio::test]
async fn field_named_id_with_non_id_type_is_rejected() {
    // #5: a field NAMED `id` but typed something other than `id` is rejected —
    // `id` is reserved for the platform PK, never a creator-typed column.
    let cfg = cfg_for(&token());
    let desc = CollectionDescriptor {
        name: "t".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "id".into(), ty: "number".into(), ..Default::default() }],
        indexes: vec![],
    };
    let err = desired_snapshot(&cfg.project_schema, &[desc]).unwrap_err();
    assert!(matches!(err, DeclarativeError::Invalid(_)), "got {err:?}");
}

// ---------------------------------------------------------------------------
// Phase-1 parity #6 — three implicit system-field indexes round-trip.
// ---------------------------------------------------------------------------

#[compio::test]
async fn system_field_indexes_are_modelled_and_a_fresh_table_re_diffs_empty() {
    // #6: the platform auto-indexes deleted_at / updated_at / created_by on every
    // table. The desired snapshot must model these three B-tree indexes so a
    // freshly-created table re-diffs to ZERO (the engine creates them; they exist
    // live; desired accounts for them). RED pre-fix: desired carried none, so
    // either the engine never created them (diverging from plugin-db) or — once a
    // plugin-db-created table is diffed — they'd phantom-DROP.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "widgets".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), ..Default::default() }],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired");

    // The three system indexes are modelled in desired.
    let idx_names: Vec<&str> = desired.snapshot.tables["widgets"].indexes.iter().map(|i| i.name.as_str()).collect();
    for col in ["deleted_at", "updated_at", "created_by"] {
        let want = format!("widgets_{col}_idx");
        assert!(idx_names.contains(&want.as_str()), "system index {want} must be modelled, got {idx_names:?}");
    }

    // Apply, then both oracles: full diff clean AND differ re-diffs to ZERO.
    apply_and_assert_clean_roundtrip(&engine, &author, &cfg, &conn, &desired).await;

    // And the three indexes actually exist live.
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let live_idx: Vec<&str> = live.tables["widgets"].indexes.iter().map(|i| i.name.as_str()).collect();
    for col in ["deleted_at", "updated_at", "created_by"] {
        let want = format!("widgets_{col}_idx");
        assert!(live_idx.contains(&want.as_str()), "system index {want} must exist live, got {live_idx:?}");
    }

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Schema-authority P2 — FULL type capability gained by adopting zeroship-schema.
//
// The v1-subset differ REJECTED vector / geoPoint / encrypted / mask with
// UnsupportedType. After P2 the engine's declarative path routes column types
// through the shared kernel, so these are first-class. These tests prove the
// gained capability:
//   - vector / geoPoint are ACCEPTED + correctly DDL'd (their parameterised
//     types need an extension the platform catalog lacks, so they are not
//     applied live here — the spec's "at minimum no longer rejects them");
//   - encrypted (→ BYTEA) and mask (→ <col>_masked sibling) need NO extension,
//     so they apply to a real PG and round-trip to ZERO drift.
// ---------------------------------------------------------------------------

#[compio::test]
async fn p2_goodies_are_accepted_not_rejected() {
    // vector / geoPoint / encrypted / mask each ACCEPT (no UnsupportedType) and
    // map to the correct column type — the capability the shared kernel provides.
    let cfg = cfg_for(&token());

    // vector(N).
    let v = CollectionDescriptor {
        name: "emb".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "vec".into(),
            ty: "vector".into(),
            vector_dims: Some(768),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let dv = desired_snapshot(&cfg.project_schema, &[v]).expect("vector accepted");
    assert_eq!(
        dv.snapshot.tables["emb"].columns.iter().find(|c| c.name == "vec").unwrap().data_type,
        "vector(768)"
    );

    // geoPoint → geography(POINT, 4326).
    let g = CollectionDescriptor {
        name: "places".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor { name: "loc".into(), ty: "geoPoint".into(), ..Default::default() }],
        indexes: vec![],
    };
    let dg = desired_snapshot(&cfg.project_schema, &[g]).expect("geoPoint accepted");
    assert_eq!(
        dg.snapshot.tables["places"].columns.iter().find(|c| c.name == "loc").unwrap().data_type,
        "geography(POINT, 4326)"
    );

    // encrypted → bytea.
    let e = CollectionDescriptor {
        name: "vault".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "ssn".into(),
            ty: "string".into(),
            encrypted: Some(serde_json::json!({ "mode": "randomised", "keyId": "default", "wraps": "string" })),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let de = desired_snapshot(&cfg.project_schema, &[e]).expect("encrypted accepted");
    assert_eq!(
        de.snapshot.tables["vault"].columns.iter().find(|c| c.name == "ssn").unwrap().data_type,
        "bytea"
    );

    // mask → a <col>_masked sibling column is modelled.
    let m = CollectionDescriptor {
        name: "people".into(),
        owner_app: "app_test".into(),
        fields: vec![FieldDescriptor {
            name: "email".into(),
            ty: "string".into(),
            mask: Some(serde_json::json!({ "kind": "email", "classification": "pii" })),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let dm = desired_snapshot(&cfg.project_schema, &[m]).expect("mask accepted");
    let cols: Vec<&str> = dm.snapshot.tables["people"].columns.iter().map(|c| c.name.as_str()).collect();
    assert!(cols.contains(&"email"), "parent column present");
    assert!(cols.contains(&"email_masked"), "mask sibling modelled, got {cols:?}");
}

#[compio::test]
async fn p2_encrypted_and_mask_columns_apply_and_round_trip_to_zero_drift() {
    // The strongest capability proof that needs no extension: a table with an
    // ENCRYPTED column (→ BYTEA) and a MASKED column (→ <col>_masked TEXT sibling)
    // applies to a REAL Postgres and re-diffs to ZERO — the engine now DDLs both
    // goodies correctly via the shared kernel, where before it would have refused
    // them outright.
    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "accounts".into(),
        owner_app: "app_test".into(),
        fields: vec![
            FieldDescriptor { name: "label".into(), ty: "string".into(), required: true, ..Default::default() },
            // encrypted → bytea
            FieldDescriptor {
                name: "secret".into(),
                ty: "string".into(),
                encrypted: Some(serde_json::json!({ "mode": "randomised", "keyId": "default", "wraps": "string" })),
                ..Default::default()
            },
            // masked → adds an `phone_masked` sibling column
            FieldDescriptor {
                name: "phone".into(),
                ty: "string".into(),
                mask: Some(serde_json::json!({ "kind": "last4", "classification": "pci" })),
                ..Default::default()
            },
        ],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired");

    // Apply from empty, then assert the round-trip is byte-clean (the sibling
    // column + the bytea column both round-trip).
    apply_and_assert_clean_roundtrip(&engine, &author, &cfg, &conn, &desired).await;

    // The live table actually carries the bytea + the masked sibling.
    let live = snapshot_schema(&conn, &cfg.project_schema).await.expect("snap");
    let cols = &live.tables["accounts"].columns;
    let secret = cols.iter().find(|c| c.name == "secret").expect("secret column");
    assert_eq!(secret.data_type, "bytea", "encrypted column is BYTEA live");
    assert!(
        cols.iter().any(|c| c.name == "phone_masked" && c.data_type == "text"),
        "masked sibling phone_masked TEXT exists live, got {:?}",
        cols.iter().map(|c| (c.name.as_str(), c.data_type.as_str())).collect::<Vec<_>>()
    );

    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// P4 HALF A — goodies-via-sentinels in the GENERATED DDL.
//
// The engine's generated migration DDL must carry the schema-shape sentinels
// the data plane (plugin-db) reads at runtime: an `encrypted` column's BYTEA
// gets the inline `/* zsenc:… */` sentinel AND a `COMMENT ON COLUMN … 'zsenc:…'`
// (the PG-recoverable form), and a masked field's `<col>_masked` sibling gets a
// `COMMENT ON COLUMN … '__zsmask:…'`. The verify-bricking guard: after the
// generated SQL applies on a REAL Postgres, `read_live_schema` (the SHARED
// introspector both the engine and plugin-db use) must recover the SAME
// `EncryptionMeta` / `MaskMeta` the descriptor declared — byte-identical.
// ---------------------------------------------------------------------------
#[compio::test]
async fn p4_half_a_generated_ddl_carries_sentinels_and_round_trips_through_introspection() {
    use zeroship_schema::descriptors::EncryptionMode;
    use zeroship_schema::diff::{
        read_live_schema, Classification, EncryptionMeta, MaskKind, MaskMeta, WrappedType,
    };

    let tok = token();
    let cfg = cfg_with_role(&tok);
    let conn = pg().await;
    teardown(&conn, &cfg).await;
    setup(&conn, &cfg).await;
    let author = author_for(&cfg);
    let engine = MigrationEngine::new();

    let desc = CollectionDescriptor {
        name: "vault".into(),
        owner_app: "app_test".into(),
        fields: vec![
            // encrypted (deterministic, non-default key) → BYTEA + zsenc sentinel.
            FieldDescriptor {
                name: "secret".into(),
                ty: "string".into(),
                encrypted: Some(serde_json::json!({
                    "mode": "deterministic", "keyId": "k7", "wraps": "string"
                })),
                ..Default::default()
            },
            // masked → `phone_masked` sibling + __zsmask sentinel.
            FieldDescriptor {
                name: "phone".into(),
                ty: "string".into(),
                mask: Some(serde_json::json!({ "kind": "last4", "classification": "pci" })),
                ..Default::default()
            },
        ],
        indexes: vec![],
    };
    let desired = desired_snapshot(&cfg.project_schema, &[desc]).expect("desired");

    // (1) The GENERATED DDL must contain the EXACT sentinels — the contract.
    let plan = author
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff");
    let create_sql: String = plan
        .migrations
        .iter()
        .find(|m| m.name == "create_table_vault")
        .map(|m| m.up.clone())
        .expect("create_table_vault migration");

    // Inline zsenc on the BYTEA column (the SQLite-surviving form).
    assert!(
        create_sql.contains("/* zsenc:deterministic:k7:string */"),
        "generated CREATE TABLE must carry the inline zsenc sentinel: {create_sql}"
    );
    // COMMENT ON COLUMN with the zsenc body on the encrypted column (PG-recoverable).
    assert!(
        create_sql.contains("\"secret\" IS 'zsenc:deterministic:k7:string'"),
        "generated DDL must COMMENT the encrypted column with the zsenc body: {create_sql}"
    );
    // COMMENT ON COLUMN with the __zsmask body on the masked sibling.
    assert!(
        create_sql.contains("\"phone_masked\" IS '__zsmask:kind=last4,classification=pci'"),
        "generated DDL must COMMENT the masked sibling with the __zsmask body: {create_sql}"
    );

    // (2) Apply the generated migration on REAL Postgres.
    apply_plan(&engine, &desired, &SchemaSnapshot::default(), &author, &cfg, &conn, Approval::None)
        .await
        .expect("apply generated plan");

    // (3) Introspect via the SHARED `read_live_schema` and assert byte-identical
    //     recovery of the goodies — the verify-bricking guard.
    let pool = compio_postgres::Pool::connect(&dsn(), 2)
        .await
        .expect("pool for introspection");
    let live = read_live_schema(&pool, &cfg.project_schema)
        .await
        .expect("read_live_schema");
    let vault = live.tables.get("vault").expect("vault table introspected");

    let secret = vault.get("secret").expect("secret column introspected");
    assert_eq!(
        secret.encryption,
        Some(EncryptionMeta {
            mode: EncryptionMode::Deterministic,
            key_id: "k7".into(),
            wraps: WrappedType::String,
        }),
        "EncryptionMeta must round-trip byte-identical through introspection"
    );

    let phone = vault.get("phone").expect("phone column introspected");
    assert_eq!(
        phone.mask,
        Some(MaskMeta {
            kind: MaskKind::Last4,
            classification: Classification::Pci,
            sibling_column: "phone_masked".into(),
        }),
        "MaskMeta must round-trip byte-identical through introspection"
    );

    teardown(&conn, &cfg).await;
}
