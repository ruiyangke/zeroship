//! Faithful drift-detection tests against a REAL Postgres (no shims).
//!
//! B1 checksum/tamper/orphan drift + B2 structural introspection + pure diff.
//! Requires `zeroship_migrate_test` on :5440. Each test uses its own meta +
//! project schema (unique token).

use std::collections::BTreeMap;

use compio_postgres::Client;
use zeroship_migrate::migration::Checksum;
use zeroship_migrate::{
    apply, check_checksum_drift, diff_snapshots, rollback, snapshot_schema, Approval, ColumnSnapshot,
    ConstraintSnapshot, ExecutorConfig, IndexSnapshot, Migration, MigrationFlags, MigrationId,
    RollbackRequest, RollbackTarget, SchemaSnapshot, TableSnapshot,
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
    c.pg.meta_schema = format!("meta_{tok}");
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
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn mig(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(&zeroship_migrate::ChecksumInput {
            up,
            down: None,
            flags: &MigrationFlags::default(),
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        }),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
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

#[compio::test]
async fn checksum_drift_reads_latest_completed_checksum_across_reapply() {
    // #2 — locks that `applied()` (and thus check_checksum_drift) reads the LATEST
    // completed event's checksum, not a stale earlier one, across a
    // apply(upA) → rollback → re-apply(upB) cycle.
    //
    //   apply v1 with upA (csA) → rollback v1 → re-apply v1 with a DIFFERENT
    //   up upB (csB).
    // The journal's net checksum for v1 must be csB (the newest incarnation).
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    let ver = MigrationId::generate();

    // upA: create t1(id int); reversible.
    let up_a = format!("CREATE TABLE \"{sch}\".\"t1\" (id int)");
    let down_a = format!("DROP TABLE \"{sch}\".\"t1\"");
    let mig_a = { let mut __mig = Migration {
        version: ver.clone(),
        name: "t1".into(),
        up: up_a.clone(),
        down: Some(down_a.clone()),
        checksum: Checksum::of(&zeroship_migrate::ChecksumInput { up: "", down: None, flags: &MigrationFlags::default(), owner_app: "", depends_on: &[], supersedes: &[], preconditions: &[] }),
        flags: MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
    }; __mig.recompute_checksum(); __mig };
    let cs_a = mig_a.checksum.as_str().to_string();

    // upB: SAME version, DIFFERENT body (t1 with an extra column) → different cs.
    let up_b = format!("CREATE TABLE \"{sch}\".\"t1\" (id int, label text)");
    let down_b = format!("DROP TABLE \"{sch}\".\"t1\"");
    let mig_b = { let mut __mig = Migration {
        version: ver.clone(),
        name: "t1".into(),
        up: up_b.clone(),
        down: Some(down_b.clone()),
        checksum: Checksum::of(&zeroship_migrate::ChecksumInput { up: "", down: None, flags: &MigrationFlags::default(), owner_app: "", depends_on: &[], supersedes: &[], preconditions: &[] }),
        flags: MigrationFlags::default(),
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
    }; __mig.recompute_checksum(); __mig };
    let cs_b = mig_b.checksum.as_str().to_string();
    assert_ne!(cs_a, cs_b, "upA and upB must have distinct checksums");

    // apply upA.
    apply(&conn, &cfg, std::slice::from_ref(&mig_a), Approval::None, "actor")
        .await
        .expect("apply upA");
    // rollback v1 (so it's re-appliable).
    rollback(
        &conn,
        &cfg,
        std::slice::from_ref(&mig_a),
        RollbackRequest::new(RollbackTarget::Steps(1)),
        Approval::Approved,
        "rollbacker",
    )
    .await
    .expect("rollback v1");
    // re-apply upB (a DIFFERENT incarnation of the same version).
    apply(&conn, &cfg, std::slice::from_ref(&mig_b), Approval::None, "actor")
        .await
        .expect("re-apply upB");

    // Against a set shipping upB: CLEAN — recorded (csB) == set checksum (csB).
    let report_b = check_checksum_drift(&conn, &cfg, std::slice::from_ref(&mig_b))
        .await
        .expect("drift upB");
    assert!(
        report_b.is_clean(),
        "set shipping upB must be clean (recorded==csB): {report_b:?}"
    );

    // Against a set still shipping upA: DRIFT — recorded is csB (the LATEST
    // completed event), not the stale csA, and it disagrees with the set's csA.
    let report_a = check_checksum_drift(&conn, &cfg, std::slice::from_ref(&mig_a))
        .await
        .expect("drift upA");
    assert_eq!(report_a.checksum_drift.len(), 1, "one drift: {report_a:?}");
    let d = &report_a.checksum_drift[0];
    assert_eq!(d.version, ver.as_str());
    assert_eq!(d.recorded, cs_b, "recorded is the LATEST completed checksum (csB)");
    assert_eq!(d.expected, cs_a, "expected is the set's stale upA checksum (csA)");
    assert!(report_a.orphan_journal.is_empty(), "{report_a:?}");

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
        "CREATE TABLE \"{sch}\".\"users\" (id int PRIMARY KEY, email text NOT NULL, a text, b text);
         CREATE UNIQUE INDEX users_email_idx ON \"{sch}\".\"users\" (email);
         CREATE INDEX users_a_b_idx ON \"{sch}\".\"users\" (a, b);
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
    assert_eq!(cols, vec!["a", "b", "email", "id"], "columns name-ordered");
    // email is NOT NULL.
    let email = users.columns.iter().find(|c| c.name == "email").unwrap();
    assert!(!email.nullable);
    // The unique index is captured WITH its single key column (1a: columns are
    // introspected from pg_index, not recovered from the name).
    let email_idx = users
        .indexes
        .iter()
        .find(|i| i.name == "users_email_idx")
        .unwrap_or_else(|| panic!("indexes: {:?}", users.indexes));
    assert!(email_idx.unique);
    assert_eq!(email_idx.columns, vec!["email".to_string()], "single key col");
    // The COMPOSITE index carries BOTH key columns IN ORDER — the case the old
    // name-heuristic could never recover (`users_a_b_idx` → `a_b`).
    let ab_idx = users
        .indexes
        .iter()
        .find(|i| i.name == "users_a_b_idx")
        .unwrap_or_else(|| panic!("indexes: {:?}", users.indexes));
    assert!(!ab_idx.unique);
    assert_eq!(
        ab_idx.columns,
        vec!["a".to_string(), "b".to_string()],
        "composite index key columns in order"
    );
    // The PK's implicit index carries its key column too.
    let pk_idx = users
        .indexes
        .iter()
        .find(|i| i.name == "users_pkey")
        .unwrap_or_else(|| panic!("indexes: {:?}", users.indexes));
    assert_eq!(pk_idx.columns, vec!["id".to_string()], "pk index key col");
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
                ..Default::default()
            }],
            indexes: Vec::new(),
            constraints: Vec::new(),
            stored_create_sql: None,
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
                ColumnSnapshot { name: "id".into(), data_type: "integer".into(), nullable: true, ..Default::default() },
                ColumnSnapshot { name: "declared_col".into(), data_type: "text".into(), nullable: true, ..Default::default() },
            ],
            indexes: Vec::new(),
            constraints: Vec::new(),
            stored_create_sql: None,
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
    let _ = IndexSnapshot::btree("x", false, vec!["c".into()]);
    let _ = ConstraintSnapshot {
        name: "x".into(),
        kind: "CHECK".into(),
        definition: "CHECK (true)".into(),
    };

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// #1 — attribute-aware structural drift (out-of-band ALTER blind spot)
// ---------------------------------------------------------------------------

#[compio::test]
async fn diff_reports_altered_column_data_type() {
    // (a) An out-of-band `ALTER COLUMN … TYPE` keeps the column NAME but changes
    // data_type. Name-only diffing reports CLEAN; attribute diff surfaces it.
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    // LIVE: id is bigint (the out-of-band ALTERed shape).
    conn.batch_execute(&format!("CREATE TABLE \"{sch}\".\"users\" (id bigint);"))
        .await
        .expect("seed");
    let actual = snapshot_schema(&conn, sch).await.expect("actual");

    // EXPECTED: same table+column NAME, but data_type = integer.
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    tables.insert(
        "users".to_string(),
        TableSnapshot {
            columns: vec![ColumnSnapshot {
                name: "id".into(),
                data_type: "integer".into(),
                nullable: true,
                ..Default::default()
            }],
            indexes: Vec::new(),
            constraints: Vec::new(),
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables };

    let drift = diff_snapshots(&expected, &actual);
    assert!(drift.missing_objects.is_empty(), "no missing: {drift:?}");
    assert!(drift.unexpected_objects.is_empty(), "no unexpected: {drift:?}");
    assert_eq!(drift.altered_objects.len(), 1, "one altered: {drift:?}");
    let a = &drift.altered_objects[0];
    assert_eq!(a.table, "users");
    assert_eq!(a.object, "column id");
    assert_eq!(a.field, "data_type");
    assert_eq!(a.expected, "integer");
    assert_eq!(a.actual, "bigint");
    assert!(!drift.is_clean());

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn diff_reports_altered_column_nullability() {
    // (b) An out-of-band `DROP NOT NULL` flips nullable while the name is stable.
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    // LIVE: email is nullable (NOT NULL was dropped out-of-band).
    conn.batch_execute(&format!("CREATE TABLE \"{sch}\".\"users\" (email text);"))
        .await
        .expect("seed");
    let actual = snapshot_schema(&conn, sch).await.expect("actual");

    // EXPECTED: email NOT NULL.
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    tables.insert(
        "users".to_string(),
        TableSnapshot {
            columns: vec![ColumnSnapshot {
                name: "email".into(),
                data_type: "text".into(),
                nullable: false,
                ..Default::default()
            }],
            indexes: Vec::new(),
            constraints: Vec::new(),
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables };

    let drift = diff_snapshots(&expected, &actual);
    assert!(drift.missing_objects.is_empty(), "{drift:?}");
    assert!(drift.unexpected_objects.is_empty(), "{drift:?}");
    assert_eq!(drift.altered_objects.len(), 1, "one altered: {drift:?}");
    let a = &drift.altered_objects[0];
    assert_eq!(a.object, "column email");
    assert_eq!(a.field, "nullable");
    assert_eq!(a.expected, "false");
    assert_eq!(a.actual, "true");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn diff_reports_altered_index_uniqueness() {
    // (c) An index that lost UNIQUE out-of-band — same name, unique true→false.
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    // LIVE: a NON-unique index named users_email_idx.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"users\" (email text);
         CREATE INDEX users_email_idx ON \"{sch}\".\"users\" (email);"
    ))
    .await
    .expect("seed");
    let actual = snapshot_schema(&conn, sch).await.expect("actual");

    // EXPECTED: same index name, but UNIQUE.
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    tables.insert(
        "users".to_string(),
        TableSnapshot {
            columns: actual.tables["users"].columns.clone(),
            indexes: vec![IndexSnapshot {
                name: "users_email_idx".into(),
                unique: true,
                // Same column set as live (email) — so the ONLY altered field is
                // `unique`, keeping this test's single-altered assertion exact.
                columns: vec!["email".into()],
                // Same access method as live (btree) so `access_method` is NOT a
                // second altered field; isolates the `unique` flip.
                access_method: "btree".into(),
                expression: None,
                opclass: None,
            }],
            constraints: Vec::new(),
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables };

    let drift = diff_snapshots(&expected, &actual);
    assert!(drift.missing_objects.is_empty(), "{drift:?}");
    assert!(drift.unexpected_objects.is_empty(), "{drift:?}");
    assert_eq!(drift.altered_objects.len(), 1, "one altered: {drift:?}");
    let a = &drift.altered_objects[0];
    assert_eq!(a.object, "index users_email_idx");
    assert_eq!(a.field, "unique");
    assert_eq!(a.expected, "true");
    assert_eq!(a.actual, "false");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn diff_reports_altered_check_constraint_definition() {
    // (d) A CHECK constraint whose PREDICATE was rewritten out-of-band — same
    // name + same kind (CHECK), different body. The definition field surfaces it
    // (the CHECK-body hole #1 closes).
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    // LIVE: CHECK (age > 18) under a stable constraint name.
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"users\" (age int, CONSTRAINT users_age_chk CHECK (age > 18));"
    ))
    .await
    .expect("seed");
    let actual = snapshot_schema(&conn, sch).await.expect("actual");

    // The live definition (whatever pg_get_constraintdef renders) is captured.
    let live = actual.tables["users"]
        .constraints
        .iter()
        .find(|c| c.name == "users_age_chk")
        .expect("live constraint captured");
    assert_eq!(live.kind, "CHECK");
    assert!(
        live.definition.contains("18"),
        "live def reflects body: {:?}",
        live.definition
    );

    // EXPECTED: same name, same kind, but the body says age > 0.
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    tables.insert(
        "users".to_string(),
        TableSnapshot {
            columns: actual.tables["users"].columns.clone(),
            indexes: Vec::new(),
            constraints: vec![ConstraintSnapshot {
                name: "users_age_chk".into(),
                kind: "CHECK".into(),
                definition: "CHECK ((age > 0))".into(),
            }],
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables };

    let drift = diff_snapshots(&expected, &actual);
    assert!(drift.missing_objects.is_empty(), "{drift:?}");
    assert!(drift.unexpected_objects.is_empty(), "{drift:?}");
    // kind matches (CHECK==CHECK); only the definition diverges.
    assert_eq!(drift.altered_objects.len(), 1, "one altered: {drift:?}");
    let a = &drift.altered_objects[0];
    assert_eq!(a.object, "constraint users_age_chk");
    assert_eq!(a.field, "definition");
    assert_eq!(a.expected, "CHECK ((age > 0))");
    assert!(a.actual.contains("18"), "actual def: {:?}", a.actual);

    drop_schemas(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// T12 — access-method / expression drift (the index-method blind spot).
// ---------------------------------------------------------------------------

#[compio::test]
async fn t12_out_of_band_gin_index_surfaces_as_unexpected() {
    // (a) An out-of-band GIN index the migration journal never declared must
    // surface in `unexpected_objects`. Pre-T12 the introspection joined no `pg_am`
    // and read no expression, so a wholly-expression GIN index round-tripped with
    // an empty column list and could not be distinguished/modeled at all.
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"docs\" (body text);
         CREATE INDEX docs_fts_gin ON \"{sch}\".\"docs\"
             USING gin (to_tsvector('english', body));"
    ))
    .await
    .expect("seed");
    let actual = snapshot_schema(&conn, sch).await.expect("actual");

    // The GIN index introspects with access_method = gin and a recovered
    // expression (it has no plain key columns).
    let live_idx = actual.tables["docs"]
        .indexes
        .iter()
        .find(|i| i.name == "docs_fts_gin")
        .expect("the GIN index is introspected (pre-T12 it joined no pg_am)");
    assert_eq!(live_idx.access_method, "gin");
    assert!(
        live_idx
            .expression
            .as_deref()
            .is_some_and(|e| e.contains("to_tsvector")),
        "the expression index must recover its expr: {:?}",
        live_idx.expression
    );

    // EXPECTED: the table with NO indexes declared → the GIN index is unexpected.
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    tables.insert(
        "docs".to_string(),
        TableSnapshot {
            columns: actual.tables["docs"].columns.clone(),
            indexes: Vec::new(),
            constraints: Vec::new(),
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables };

    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift
            .unexpected_objects
            .iter()
            .any(|u| u == "docs index docs_fts_gin"),
        "the out-of-band GIN index must be unexpected: {drift:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn t12_btree_to_ivfflat_access_method_flip_is_reported() {
    // (b) A same-name index whose ACCESS METHOD changed (btree → ivfflat — the
    // vector-ANN flip) must surface as an altered_object with field
    // `access_method`. Name + columns match, so ONLY the method-aware compare
    // catches it. We seed a plain btree index live (no pgvector needed) and assert
    // the expected ivfflat method diverges.
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"items\" (embedding text);
         CREATE INDEX items_embedding_idx ON \"{sch}\".\"items\" (embedding);"
    ))
    .await
    .expect("seed");
    let actual = snapshot_schema(&conn, sch).await.expect("actual");
    assert_eq!(
        actual.tables["items"].indexes[0].access_method, "btree",
        "the live index is btree"
    );

    // EXPECTED: same index name + columns, but access_method = ivfflat (the ANN
    // shape the desired schema intends).
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    tables.insert(
        "items".to_string(),
        TableSnapshot {
            columns: actual.tables["items"].columns.clone(),
            indexes: vec![IndexSnapshot {
                name: "items_embedding_idx".into(),
                unique: false,
                columns: vec!["embedding".into()],
                access_method: "ivfflat".into(),
                expression: None,
                opclass: Some("vector_cosine_ops".into()),
            }],
            constraints: Vec::new(),
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables };

    let drift = diff_snapshots(&expected, &actual);
    assert!(drift.missing_objects.is_empty(), "{drift:?}");
    assert!(drift.unexpected_objects.is_empty(), "{drift:?}");
    let method_alter = drift
        .altered_objects
        .iter()
        .find(|a| a.object == "index items_embedding_idx" && a.field == "access_method")
        .expect("the btree→ivfflat method flip must be reported");
    assert_eq!(method_alter.expected, "ivfflat");
    assert_eq!(method_alter.actual, "btree");
    // The emission-only `opclass` must NOT itself produce drift noise.
    assert!(
        !drift
            .altered_objects
            .iter()
            .any(|a| a.field == "opclass"),
        "opclass is emission-only, not a drift attribute: {drift:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn t12_fts_expression_index_re_diffs_clean() {
    // (c) An FTS expression GIN index, snapshotted and diffed against itself, is
    // CLEAN — the recovered `pg_get_expr` spelling is re-parse-stable, so an
    // expression index does NOT phantom-drift (pre-T12 it had an empty column list
    // and no expression, so it both phantom-DROPped and could collide).
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"pages\" (title text, body text);
         CREATE INDEX pages_fts ON \"{sch}\".\"pages\"
             USING gin (to_tsvector('english', coalesce(title,'') || ' ' || coalesce(body,'')));"
    ))
    .await
    .expect("seed");
    let snap = snapshot_schema(&conn, sch).await.expect("snap");

    // The expression index round-trips with its expr captured.
    let idx = snap.tables["pages"]
        .indexes
        .iter()
        .find(|i| i.name == "pages_fts")
        .expect("the FTS expression index is introspected");
    assert_eq!(idx.access_method, "gin");
    assert!(idx.expression.is_some(), "expr captured: {idx:?}");

    // Self-diff is clean — the expression is not a phantom drift.
    let drift = diff_snapshots(&snap, &snap);
    assert!(drift.is_clean(), "expression index phantom-drifted: {drift:?}");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn diff_of_self_snapshot_with_attributes_is_clean() {
    // A snapshot diffed against ITSELF — including constraints+indexes+columns
    // with full attributes — reports no altered_objects (regression guard that
    // attribute compare doesn't false-positive on equal values).
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"users\" (id int PRIMARY KEY, email text NOT NULL,
            CONSTRAINT users_email_len CHECK (length(email) > 0));
         CREATE UNIQUE INDEX users_email_idx ON \"{sch}\".\"users\" (email);"
    ))
    .await
    .expect("seed");
    let snap = snapshot_schema(&conn, sch).await.expect("snap");
    let drift = diff_snapshots(&snap, &snap);
    assert!(drift.altered_objects.is_empty(), "no false alters: {drift:?}");
    assert!(drift.is_clean(), "{drift:?}");

    drop_schemas(&conn, &cfg).await;
}
