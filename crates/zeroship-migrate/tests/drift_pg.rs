//! Faithful drift-detection tests against a REAL Postgres (no shims).
//!
//! B1 checksum/tamper/orphan drift + B2 structural introspection + pure diff.
//! Requires `zeroship_migrate_test` on :5440. Each test uses its own meta +
//! project schema (unique token).

use std::collections::BTreeMap;

use compio_postgres::Client;
use zeroship_migrate::migration::Checksum;
use zeroship_migrate::{
    apply, check_checksum_drift, diff_snapshots, snapshot_schema, Approval, ColumnSnapshot,
    ConstraintSnapshot, ExecutorConfig, IndexSnapshot, Migration, MigrationFlags, MigrationId,
    SchemaSnapshot, TableSnapshot,
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

async fn drop_schemas(conn: &Client, cfg: &ExecutorConfig) {
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
}

fn mig(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(up, None),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// B1 — checksum / tamper / orphan drift
// ---------------------------------------------------------------------------

#[compio::test]
async fn no_drift_when_journal_matches_the_set() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let up = format!("CREATE TABLE \"{}\".\"t1\" (id int)", cfg.project_schema);
    let m1 = mig(MigrationId::generate(), "t1", &up);
    let set = vec![m1];
    apply(&conn, &cfg, &set, Approval::None, "actor")
        .await
        .expect("apply");

    let report = check_checksum_drift(&conn, &cfg, &set)
        .await
        .expect("drift");
    assert!(report.is_clean(), "clean: {report:?}");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn checksum_drift_when_recorded_checksum_is_tampered() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let up = format!("CREATE TABLE \"{}\".\"t1\" (id int)", cfg.project_schema);
    let m1 = mig(MigrationId::generate(), "t1", &up);
    let set = vec![m1.clone()];
    apply(&conn, &cfg, &set, Approval::None, "actor")
        .await
        .expect("apply v1");

    // Present the SAME version with a DIFFERENT up — i.e. the migration SQL was
    // edited after it applied (scenario 36). The journal's recorded checksum no
    // longer matches the set's.
    let tampered_up = format!("CREATE TABLE \"{}\".\"t1\" (id bigint)", cfg.project_schema);
    let tampered = mig(m1.version.clone(), "t1", &tampered_up);
    let tampered_set = vec![tampered.clone()];

    let report = check_checksum_drift(&conn, &cfg, &tampered_set)
        .await
        .expect("drift");
    assert_eq!(report.checksum_drift.len(), 1, "one drift: {report:?}");
    let d = &report.checksum_drift[0];
    assert_eq!(d.version, m1.version.as_str());
    assert_eq!(d.recorded, m1.checksum.as_str(), "recorded = original");
    assert_eq!(d.expected, tampered.checksum.as_str(), "expected = tampered");
    assert!(report.orphan_journal.is_empty());

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn orphan_journal_when_applied_version_absent_from_the_set() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let up = format!("CREATE TABLE \"{}\".\"t1\" (id int)", cfg.project_schema);
    let m1 = mig(MigrationId::generate(), "t1", &up);
    apply(&conn, &cfg, std::slice::from_ref(&m1), Approval::None, "actor")
        .await
        .expect("apply v1");

    // The supplied set NO LONGER contains v1 (a dropped slice / downgrade).
    let report = check_checksum_drift(&conn, &cfg, &[])
        .await
        .expect("drift");
    assert!(report.checksum_drift.is_empty());
    assert_eq!(report.orphan_journal.len(), 1, "one orphan: {report:?}");
    assert_eq!(report.orphan_journal[0].version, m1.version.as_str());
    assert_eq!(report.orphan_journal[0].recorded, m1.checksum.as_str());

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn apply_aborts_on_checksum_drift_using_the_shared_check() {
    // The executor's apply flow uses the SAME shared check_checksum_drift; a
    // tampered checksum makes apply hard-abort (design §2.3 step 3).
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let up = format!("CREATE TABLE \"{}\".\"t1\" (id int)", cfg.project_schema);
    let m1 = mig(MigrationId::generate(), "t1", &up);
    apply(&conn, &cfg, std::slice::from_ref(&m1), Approval::None, "actor")
        .await
        .expect("apply v1");

    // Re-apply with a same-version-different-up migration → ChecksumDrift abort.
    let tampered_up = format!("CREATE TABLE \"{}\".\"t1\" (id bigint)", cfg.project_schema);
    let tampered = mig(m1.version.clone(), "t1", &tampered_up);
    let err = apply(&conn, &cfg, &[tampered], Approval::None, "actor")
        .await
        .expect_err("apply must abort on drift");
    assert!(
        matches!(
            err,
            zeroship_migrate::executor::ApplyError::ChecksumDrift { .. }
        ),
        "got {err:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// B2 — structural introspection + pure diff
// ---------------------------------------------------------------------------

#[compio::test]
async fn snapshot_introspects_tables_columns_indexes_constraints() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"users\" (id int PRIMARY KEY, email text NOT NULL);
         CREATE UNIQUE INDEX users_email_idx ON \"{sch}\".\"users\" (email);
         CREATE TABLE \"{sch}\".\"orders\" (id int PRIMARY KEY);"
    ))
    .await
    .expect("seed schema");

    let snap = snapshot_schema(&conn, sch).await.expect("snapshot");
    assert_eq!(
        snap.tables.keys().collect::<Vec<_>>(),
        vec!["orders", "users"],
        "both tables, name-ordered"
    );

    let users = &snap.tables["users"];
    let cols: Vec<&str> = users.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cols, vec!["email", "id"], "columns name-ordered");
    // email is NOT NULL.
    let email = users.columns.iter().find(|c| c.name == "email").unwrap();
    assert!(!email.nullable);
    // The unique index is captured.
    assert!(
        users.indexes.iter().any(|i| i.name == "users_email_idx" && i.unique),
        "indexes: {:?}",
        users.indexes
    );
    // The primary-key constraint is captured.
    assert!(
        users.constraints.iter().any(|c| c.kind == "PRIMARY KEY"),
        "constraints: {:?}",
        users.constraints
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn diff_reports_missing_table_when_expected_has_an_extra() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!("CREATE TABLE \"{sch}\".\"users\" (id int);"))
        .await
        .expect("seed users");

    let actual = snapshot_schema(&conn, sch).await.expect("actual");

    // Expected = actual + an extra declared table the DB never got.
    let mut tables: BTreeMap<String, TableSnapshot> = actual.tables.clone();
    tables.insert(
        "audit".to_string(),
        TableSnapshot {
            columns: vec![ColumnSnapshot {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: true,
            }],
            indexes: Vec::new(),
            constraints: Vec::new(),
        },
    );
    let expected = SchemaSnapshot { tables };

    let drift = diff_snapshots(&expected, &actual);
    assert_eq!(drift.missing_objects, vec!["audit".to_string()]);
    assert!(drift.unexpected_objects.is_empty(), "{drift:?}");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn diff_reports_unexpected_table_for_out_of_band_creation() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    // DB has TWO tables; one was created out-of-band (scenario 35).
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"users\" (id int);
         CREATE TABLE \"{sch}\".\"shadow_oob\" (id int);"
    ))
    .await
    .expect("seed");
    let actual = snapshot_schema(&conn, sch).await.expect("actual");

    // Expected = only users (shadow_oob is not declared).
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    tables.insert("users".to_string(), actual.tables["users"].clone());
    let expected = SchemaSnapshot { tables };

    let drift = diff_snapshots(&expected, &actual);
    assert!(drift.missing_objects.is_empty(), "{drift:?}");
    assert_eq!(drift.unexpected_objects, vec!["shadow_oob".to_string()]);

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn diff_reports_missing_and_unexpected_columns_within_a_shared_table() {
    // Column-level drift on a table present on both sides.
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"users\" (id int, oob_col text);"
    ))
    .await
    .expect("seed");
    let actual = snapshot_schema(&conn, sch).await.expect("actual");

    // Expected users has id + declared_col (NOT oob_col).
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    tables.insert(
        "users".to_string(),
        TableSnapshot {
            columns: vec![
                ColumnSnapshot { name: "id".into(), data_type: "integer".into(), nullable: true },
                ColumnSnapshot { name: "declared_col".into(), data_type: "text".into(), nullable: true },
            ],
            indexes: Vec::new(),
            constraints: Vec::new(),
        },
    );
    let expected = SchemaSnapshot { tables };

    let drift = diff_snapshots(&expected, &actual);
    assert_eq!(drift.missing_objects, vec!["users.declared_col".to_string()]);
    assert_eq!(drift.unexpected_objects, vec!["users.oob_col".to_string()]);

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn snapshot_is_injection_safe_against_a_hostile_schema_name() {
    // A schema name containing a quote + semicolon must be BOUND, not
    // interpolated: the query selects zero rows (the schema does not exist)
    // rather than executing the injected fragment. Proven by: it neither errors
    // nor drops the real journal/project schema.
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!("CREATE TABLE \"{sch}\".\"keep_me\" (id int);"))
        .await
        .expect("seed");

    let hostile = "x\"; DROP SCHEMA \"public\"; --";
    let snap = snapshot_schema(&conn, hostile)
        .await
        .expect("hostile schema name must not error — it binds, selecting nothing");
    assert!(snap.tables.is_empty(), "hostile schema matched no tables");

    // The REAL project schema + its table are untouched (no injection executed).
    let real = snapshot_schema(&conn, sch).await.expect("real snapshot");
    assert!(real.tables.contains_key("keep_me"), "real table survived");

    // And `public` still exists (the injected DROP never ran).
    let pub_rows = conn
        .query(
            "SELECT 1 FROM information_schema.schemata WHERE schema_name = 'public'",
            &[],
        )
        .await
        .expect("check public");
    assert_eq!(pub_rows.len(), 1, "public schema untouched");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn diff_of_identical_snapshots_is_clean() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"users\" (id int PRIMARY KEY);"
    ))
    .await
    .expect("seed");
    let snap = snapshot_schema(&conn, sch).await.expect("snap");
    let drift = diff_snapshots(&snap, &snap);
    assert!(drift.is_clean(), "{drift:?}");

    // sanity: types referenced so unused-import lints don't fire on a thin test
    let _ = IndexSnapshot { name: "x".into(), unique: false };
    let _ = ConstraintSnapshot { name: "x".into(), kind: "CHECK".into() };

    drop_schemas(&conn, &cfg).await;
}
