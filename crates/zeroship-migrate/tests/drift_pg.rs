//! Faithful drift-detection tests against a REAL Postgres (no shims).
//!
//! B1 checksum/tamper/orphan drift + B2 structural introspection + pure diff.
//! Requires `zeroship_migrate_test` on :5440. Each test uses its own meta +
//! project schema (unique token).

use std::collections::BTreeMap;

use compio_postgres::Client;
use zeroship_migrate::model::migration::Checksum;
use zeroship_migrate::model::ir::{SelectAst, SelectItem, TableRef, ViewQuery};
use zeroship_migrate::{
    apply, check_checksum_drift, diff_snapshots, fold_ops, rollback, snapshot_schema, Approval,
    ColType, ColumnSnapshot, CommentTarget, ConstraintSnapshot, ExecutorConfig, Expr,
    IndexElement, IndexElementSnapshot, IndexSnapshot, IrAuthor, IrColumn, IrFlagsOverride,
    IrDefault, LiveSchema, Migration, MigrationFlags, MigrationId, MigrationIr, Op,
    RollbackRequest, RollbackTarget, SafeI64, SafeU64, ScalarFn, SchemaScope, SchemaSnapshot,
    SequenceOwnedBy, SequenceRef, SqlDialect, StructuralDrift, TableSnapshot, UnaryOp,
    CURRENT_IR_VERSION,
    PolicyProfile, resolve_create_table_policy,
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

async fn cleanup_vendor_attribute_objects(
    conn: &Client,
    cfg: &ExecutorConfig,
    ext_schema: &str,
    roles: &[&str],
) {
    let mut sql = format!(
        "DROP EXTENSION IF EXISTS hstore CASCADE; \
         DROP SCHEMA IF EXISTS \"{}\" CASCADE; \
         DROP SCHEMA IF EXISTS \"{}\" CASCADE; \
         DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
        ext_schema, cfg.project_schema, cfg.pg.meta_schema
    );
    for role in roles {
        sql.push_str(&format!(" DROP ROLE IF EXISTS \"{role}\";"));
    }
    let _ = conn.batch_execute(&sql).await;
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
        existence_guard: None,
    }
}

fn ir_col(name: &str, ty: ColType, nullable: bool) -> IrColumn {
    IrColumn {
        name: name.to_string(),
        ty,
        nullable: Some(nullable),
        default: None,
        unique: None,
        id_prefix: None,
        case_sensitive: None,
        vector_metric: None,
        mask: None,
        generated: None,
        identity: None,
    }
}

fn si(n: i64) -> SafeI64 {
    SafeI64::new(n).expect("test sequence value is JS-safe")
}

fn su(n: u64) -> SafeU64 {
    SafeU64::new(n).expect("test sequence cache is JS-safe")
}

fn ir_doc(name: &str, ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: name.to_string(),
        owner_app: "app_test".to_string(),
        ops,
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn lower_ir_migrations(schema: &str, name: &str, ops: &[Op]) -> Vec<Migration> {
    let author = IrAuthor::new(schema, "app_test", SqlDialect::Postgres)
        .with_schema_scope(SchemaScope::Allowlist(vec![schema.to_string()]));
    author
        .lower(&ir_doc(name, ops.to_vec()), &LiveSchema::default())
        .expect("lower authored IR to migrations")
}

fn idx_col(name: &str) -> IndexElement {
    IndexElement::Column {
        name: name.to_string(),
        order: None,
        opclass: None,
        collation: None,
    }
}

fn lower_email_expr() -> Expr {
    Expr::FnCall {
        r#fn: ScalarFn::Lower,
        args: vec![Expr::col("email")],
    }
}

fn active_true_expr() -> Expr {
    Expr::UnaryOp {
        op: UnaryOp::IsTrue,
        operand: Box::new(Expr::col("active")),
    }
}

fn altered_contains(drift: &StructuralDrift, object: &str, field: &str) -> bool {
    drift
        .altered_objects
        .iter()
        .any(|a| a.object == object && a.field == field)
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
            zeroship_migrate::apply::executor::ApplyError::ChecksumDrift { .. }
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
        existence_guard: None,
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
        existence_guard: None,
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
async fn citext_column_round_trips_as_case_insensitive_text_without_drift() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS citext WITH SCHEMA public")
        .await
        .expect("install citext extension");

    let sch = &cfg.project_schema;
    let mut email = ir_col("email", ColType::Text, false);
    email.case_sensitive = Some(false);
    let ops = vec![Op::CreateTable {
        name: "contacts".into(),
        columns: vec![email],
        primary_key: None,
        constraints: vec![],
        indexes: vec![],
        partition_by: None,
        runtime_options: None,
        schema: None,
        existence_guard: None,
    }];
    let migrations = lower_ir_migrations(sch, "citext_case_insensitive_text", &ops);
    apply(&conn, &cfg, &migrations, Approval::None, "actor")
        .await
        .expect("apply citext table");

    let expected = fold_ops(&ops, SqlDialect::Postgres, sch).expect("fold citext expected");
    let actual = snapshot_schema(&conn, sch).await.expect("snapshot citext table");
    let live_col = actual.tables["contacts"]
        .columns
        .iter()
        .find(|c| c.name == "email")
        .expect("email column introspected");
    assert_eq!(live_col.data_type, "text");
    assert_eq!(live_col.case_sensitive, Some(false));

    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift.is_clean(),
        "live citext must recover as text + caseSensitive:false: {drift:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn nextval_default_recovers_from_live_pg_and_diffs_clean() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!(
        "CREATE SEQUENCE \"{sch}\".\"audit_events_id_seq\"; \
         CREATE TABLE \"{sch}\".\"audit_events\" (\
             \"id\" bigint DEFAULT nextval('{sch}.audit_events_id_seq'::regclass)\
         );"
    ))
    .await
    .expect("seed nextval default");

    let mut id = ir_col("id", ColType::BigInt, true);
    id.default = Some(IrDefault::Nextval {
        sequence: SequenceRef {
            name: "audit_events_id_seq".into(),
            schema: Some(sch.to_string()),
        },
    });
    let ops = vec![
        Op::CreateSequence {
            name: "audit_events_id_seq".into(),
            schema: None,
            as_type: Some(ColType::BigInt),
            increment: None,
            start: None,
            min_value: None,
            max_value: None,
            cache: None,
            cycle: None,
            owned_by: None,
        },
        Op::CreateTable {
            name: "audit_events".into(),
            columns: vec![id],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        },
    ];

    let expected = fold_ops(&ops, SqlDialect::Postgres, sch).expect("fold nextval expected");
    let actual = snapshot_schema(&conn, sch).await.expect("snapshot nextval table");
    let want_default = format!("nextval('{sch}.audit_events_id_seq'::regclass)");
    let live_default = actual.tables["audit_events"]
        .columns
        .iter()
        .find(|c| c.name == "id")
        .and_then(|c| c.default.as_deref());
    assert_eq!(
        live_default,
        Some(want_default.as_str()),
        "live nextval default should recover into normalized pg_dump-style metadata"
    );

    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift.is_clean(),
        "live nextval default must recover and diff cleanly: {drift:?}"
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
            runtime_options: Default::default(),

        partition_by: None,

        comment: None,
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables, ..Default::default() };

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
    let expected = SchemaSnapshot { tables, ..Default::default() };

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
            runtime_options: Default::default(),

        partition_by: None,

        comment: None,
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables, ..Default::default() };

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
        comment: None,
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
            runtime_options: Default::default(),

        partition_by: None,

        comment: None,
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables, ..Default::default() };

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
            runtime_options: Default::default(),

        partition_by: None,

        comment: None,
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables, ..Default::default() };

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
                elements: vec![IndexElementSnapshot::column("email")],
                // Same access method as live (btree) so `access_method` is NOT a
                // second altered field; isolates the `unique` flip.
                access_method: "btree".into(),
                predicate: None,
                include: Vec::new(),
                with: None,
                only: false,
                opclass: None,
                comment: None,
                nulls_not_distinct: false,
            }],
            constraints: Vec::new(),
            runtime_options: Default::default(),

        partition_by: None,

        comment: None,
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables, ..Default::default() };

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
                comment: None,
            }],
            runtime_options: Default::default(),

        partition_by: None,

        comment: None,
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables, ..Default::default() };

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

#[compio::test]
async fn exclusion_constraint_reintrospection_does_not_false_drift_or_tamper() {
    // Apply an EXCLUDE constraint through the journal, then compare the authored
    // body spelling against fresh `pg_get_constraintdef` introspection. PG
    // canonicalizes exclusion definitions (notably identifier quoting), so this
    // must compare on presence/name + kind without treating the body as tamper.
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    let up = format!(
        "CREATE TABLE \"{sch}\".\"bookings\" (period tsrange NOT NULL); \
         ALTER TABLE \"{sch}\".\"bookings\" ADD CONSTRAINT bookings_no_overlap \
         EXCLUDE USING gist (\"period\" WITH &&);"
    );
    let m = mig(MigrationId::generate(), "add_exclusion_constraint", &up);
    let set = vec![m];
    apply(&conn, &cfg, &set, Approval::Approved, "actor")
        .await
        .expect("apply exclusion constraint");

    let checksum = check_checksum_drift(&conn, &cfg, &set)
        .await
        .expect("checksum drift");
    assert!(checksum.is_clean(), "same migration set must not report tamper: {checksum:?}");

    let actual = snapshot_schema(&conn, sch).await.expect("actual");
    let live_table = actual.tables.get("bookings").expect("bookings live");
    let live_constraint = live_table
        .constraints
        .iter()
        .find(|c| c.name == "bookings_no_overlap")
        .expect("exclusion constraint live");
    assert_eq!(live_constraint.kind, "EXCLUDE");

    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    tables.insert(
        "bookings".to_string(),
        TableSnapshot {
            columns: live_table.columns.clone(),
            indexes: live_table.indexes.clone(),
            constraints: vec![ConstraintSnapshot {
                name: "bookings_no_overlap".into(),
                kind: "EXCLUDE".into(),
                definition: r#"EXCLUDE USING gist ("period" WITH &&)"#.into(),
                comment: None,
            }],
            runtime_options: Default::default(),

        partition_by: None,

        comment: None,
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables, ..Default::default() };

    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift.is_clean(),
        "exclusion body canonicalization must not false-report structural drift: {drift:?}"
    );

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
            .elements
            .iter()
            .any(|element| matches!(element, IndexElementSnapshot::Expr(e) if e.contains("to_tsvector"))),
        "the expression index must recover its expr: {:?}",
        live_idx.elements
    );

    // EXPECTED: the table with NO indexes declared → the GIN index is unexpected.
    let mut tables: BTreeMap<String, TableSnapshot> = BTreeMap::new();
    tables.insert(
        "docs".to_string(),
        TableSnapshot {
            columns: actual.tables["docs"].columns.clone(),
            indexes: Vec::new(),
            constraints: Vec::new(),
            runtime_options: Default::default(),

        partition_by: None,

        comment: None,
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables, ..Default::default() };

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
                elements: vec![IndexElementSnapshot::column("embedding")],
                access_method: "ivfflat".into(),
                predicate: None,
                include: Vec::new(),
                with: None,
                only: false,
                opclass: Some("vector_cosine_ops".into()),
                comment: None,
                nulls_not_distinct: false,
            }],
            constraints: Vec::new(),
            runtime_options: Default::default(),

        partition_by: None,

        comment: None,
            stored_create_sql: None,
        },
    );
    let expected = SchemaSnapshot { tables, ..Default::default() };

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
    assert!(
        idx.elements.iter().any(|element| matches!(element, IndexElementSnapshot::Expr(_))),
        "expr captured: {idx:?}"
    );

    // Self-diff is clean — the expression is not a phantom drift.
    let drift = diff_snapshots(&snap, &snap);
    assert!(drift.is_clean(), "expression index phantom-drifted: {drift:?}");

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn partial_and_expression_indexes_reintrospect_without_false_drift() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    let ops = vec![
        Op::CreateTable {
            name: "users".into(),
            columns: vec![
                ir_col("email", ColType::Text, true),
                ir_col("active", ColType::Boolean, true),
            ],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: None,
        },
        Op::CreateIndex {
            table: "users".into(),
            columns: vec![idx_col("active")],
            name: Some("users_active_idx".into()),
            unique: None,
            using: None,
            r#where: Some(active_true_expr()),

        include: Vec::new(),
        with: None,
        only: None,
        concurrently: None,
            schema: None,
            existence_guard: None,
            nulls_not_distinct: None,
        },
        Op::CreateIndex {
            table: "users".into(),
            columns: vec![
                idx_col("email"),
                IndexElement::Expr {
                    expr: lower_email_expr(),
                },
            ],
            name: Some("users_email_lower_idx".into()),
            unique: None,
            using: None,
            r#where: Some(active_true_expr()),

        include: Vec::new(),
        with: None,
        only: None,
        concurrently: None,
            schema: None,
            existence_guard: None,
            nulls_not_distinct: None,
        },
    ];
    let migrations = lower_ir_migrations(sch, "partial_expression_indexes", &ops);
    apply(&conn, &cfg, &migrations, Approval::None, "actor")
        .await
        .expect("apply authored partial/expression indexes");

    let expected =
        fold_ops(&ops, SqlDialect::Postgres, sch).expect("fold authored partial/expression ops");
    let actual = snapshot_schema(&conn, sch).await.expect("snap");
    let users = actual.tables.get("users").expect("users table");
    let partial = users
        .indexes
        .iter()
        .find(|i| i.name == "users_active_idx")
        .expect("partial index introspected");
    assert!(
        partial
            .predicate
            .as_deref()
            .is_some_and(|p| p.contains("active") && p.contains("TRUE")),
        "partial predicate recovered: {partial:?}"
    );
    assert_eq!(partial.elements, vec![IndexElementSnapshot::column("active")]);

    let expr = users
        .indexes
        .iter()
        .find(|i| i.name == "users_email_lower_idx")
        .expect("expression index introspected");
    assert!(
        expr.elements
            .iter()
            .any(|element| matches!(element, IndexElementSnapshot::Expr(e) if e.contains("lower"))),
        "expression element recovered: {expr:?}"
    );
    assert!(
        expr.predicate
            .as_deref()
            .is_some_and(|p| p.contains("active") && p.contains("TRUE")),
        "expression index predicate recovered: {expr:?}"
    );

    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift.is_clean(),
        "authored fold and catalog should agree on canonical partial/expression indexes: {drift:?}"
    );

    conn.batch_execute(&format!(
        "DROP INDEX \"{sch}\".\"users_active_idx\";
         CREATE INDEX users_active_idx ON \"{sch}\".\"users\" (active) WHERE active IS FALSE;"
    ))
    .await
    .expect("change partial index predicate out of band");
    let changed = snapshot_schema(&conn, sch)
        .await
        .expect("snap changed predicate");
    let changed_drift = diff_snapshots(&expected, &changed);
    assert!(
        altered_contains(&changed_drift, "index users_active_idx", "predicate"),
        "a genuinely changed partial-index predicate must drift: {changed_drift:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn comment_metadata_reintrospects_and_clears_without_false_drift() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    conn.batch_execute(&format!(
        "CREATE TABLE \"{sch}\".\"users\" (
             email text NOT NULL,
             active boolean,
             CONSTRAINT users_email_uq UNIQUE (email)
         );
         CREATE INDEX users_active_idx ON \"{sch}\".\"users\" (active);
         COMMENT ON TABLE \"{sch}\".\"users\" IS 'User accounts';
         COMMENT ON COLUMN \"{sch}\".\"users\".\"email\" IS 'Login email';
         COMMENT ON INDEX \"{sch}\".\"users_active_idx\" IS 'Active lookup';
         COMMENT ON CONSTRAINT \"users_email_uq\" ON \"{sch}\".\"users\" IS 'Email uniqueness';"
    ))
    .await
    .expect("seed comments");

    let snap = snapshot_schema(&conn, sch).await.expect("snap with comments");
    let users = snap.tables.get("users").expect("users table");
    assert_eq!(users.comment.as_deref(), Some("User accounts"));
    assert_eq!(
        users
            .columns
            .iter()
            .find(|c| c.name == "email")
            .and_then(|c| c.comment.as_deref()),
        Some("Login email")
    );
    assert_eq!(
        users
            .indexes
            .iter()
            .find(|i| i.name == "users_active_idx")
            .and_then(|i| i.comment.as_deref()),
        Some("Active lookup")
    );
    assert_eq!(
        users
            .constraints
            .iter()
            .find(|c| c.name == "users_email_uq")
            .and_then(|c| c.comment.as_deref()),
        Some("Email uniqueness")
    );
    let drift = diff_snapshots(&snap, &snap);
    assert!(drift.is_clean(), "comment metadata phantom-drifted: {drift:?}");

    conn.batch_execute(&format!(
        "COMMENT ON TABLE \"{sch}\".\"users\" IS NULL;
         COMMENT ON COLUMN \"{sch}\".\"users\".\"email\" IS NULL;"
    ))
    .await
    .expect("clear comments");
    let cleared = snapshot_schema(&conn, sch).await.expect("snap after clear");
    let users = cleared.tables.get("users").expect("users table");
    assert_eq!(users.comment, None);
    assert_eq!(
        users
            .columns
            .iter()
            .find(|c| c.name == "email")
            .and_then(|c| c.comment.as_deref()),
        None
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn top_level_comment_metadata_reintrospects_and_drifts() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;

    let sch = &cfg.project_schema;
    let ops = vec![
        Op::CreateTable {
            name: "users".into(),
            columns: vec![
                ir_col("email", ColType::Text, true),
                ir_col("active", ColType::Boolean, true),
            ],
            primary_key: None,
            constraints: Vec::new(),
            indexes: Vec::new(),

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: None,
        },
        Op::CreateView {
            name: "active_users".into(),
            schema: None,
            columns: None,
            query: ViewQuery::Structured {
                select: SelectAst {
                    from: TableRef {
                        name: "users".into(),
                        schema: None,
                        alias: None,
                    },
                    projection: vec![SelectItem::ColRef {
                        table: None,
                        name: "email".into(),
                        alias: None,
                    }],
                    joins: Vec::new(),
                    r#where: Some(active_true_expr()),
                    order_by: None,
                    limit: None,
                },
            },
            replace: None,
            materialized: None,
        },
        Op::CreateEnum {
            name: "mood".into(),
            schema: None,
            values: vec!["happy".into(), "sad".into()],
        },
        Op::CreateSequence {
            name: "event_seq".into(),
            schema: None,
            as_type: None,
            increment: None,
            start: None,
            min_value: None,
            max_value: None,
            cache: None,
            cycle: None,
            owned_by: None,
        },
        Op::Comment {
            target: CommentTarget::View {
                schema: None,
                name: "active_users".into(),
            },
            comment: Some("Active user projection".into()),
        },
        Op::Comment {
            target: CommentTarget::Type {
                schema: None,
                name: "mood".into(),
            },
            comment: Some("Mood enum".into()),
        },
        Op::Comment {
            target: CommentTarget::Sequence {
                schema: None,
                name: "event_seq".into(),
            },
            comment: Some("Event ids".into()),
        },
    ];
    let migrations = lower_ir_migrations(sch, "top_level_comments", &ops);
    apply(&conn, &cfg, &migrations, Approval::None, "actor")
        .await
        .expect("apply top-level comments");

    let expected = fold_ops(&ops, SqlDialect::Postgres, sch).expect("fold top-level comments");
    let actual = snapshot_schema(&conn, sch)
        .await
        .expect("snap top-level comments");
    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift.is_clean(),
        "authored fold and catalog should agree on view/type/sequence comments: {drift:?}"
    );

    conn.batch_execute(&format!(
        "COMMENT ON VIEW \"{sch}\".\"active_users\" IS NULL;
         COMMENT ON TYPE \"{sch}\".\"mood\" IS 'Changed mood enum';
         COMMENT ON SEQUENCE \"{sch}\".\"event_seq\" IS NULL;"
    ))
    .await
    .expect("change top-level comments out of band");
    let changed = snapshot_schema(&conn, sch)
        .await
        .expect("snap changed top-level comments");
    let changed_drift = diff_snapshots(&expected, &changed);
    assert!(
        altered_contains(&changed_drift, "view active_users", "comment"),
        "view comment drift must be detected: {changed_drift:?}"
    );
    assert!(
        altered_contains(&changed_drift, "type mood", "comment"),
        "type comment drift must be detected: {changed_drift:?}"
    );
    assert!(
        altered_contains(&changed_drift, "sequence event_seq", "comment"),
        "sequence comment drift must be detected: {changed_drift:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn sequence_options_round_trip_and_out_of_band_option_drift_is_reported() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let sch = &cfg.project_schema;

    let ops = vec![
        Op::CreateTable {
            name: "invoices".into(),
            columns: vec![ir_col("amount", ColType::Int, false)],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: None,
        },
        Op::CreateSequence {
            name: "invoice_seq".into(),
            schema: None,
            as_type: Some(ColType::Int),
            increment: Some(si(3)),
            start: Some(si(30)),
            min_value: Some(Some(si(3))),
            max_value: Some(Some(si(300))),
            cache: Some(su(5)),
            cycle: Some(false),
            owned_by: Some(Some(SequenceOwnedBy {
                table: "invoices".into(),
                column: "id".into(),
            })),
        },
        Op::AlterSequence {
            name: "invoice_seq".into(),
            schema: None,
            increment: Some(si(-3)),
            restart: None,
            min_value: Some(Some(si(-300))),
            max_value: Some(Some(si(300))),
            cache: Some(su(7)),
            cycle: Some(true),
            owned_by: Some(None),
        },
    ];
    let ops = resolve_create_table_policy(
        &ir_doc("sequence_options_roundtrip", ops),
        &PolicyProfile::confined(),
    )
    .expect("sequence options test IR resolves")
    .ops;
    let migrations = lower_ir_migrations(sch, "sequence_options_roundtrip", &ops);
    apply(&conn, &cfg, &migrations, Approval::None, "actor")
        .await
        .expect("apply sequence option migration");

    let expected = fold_ops(&ops, SqlDialect::Postgres, sch).expect("fold sequence options");
    let actual = snapshot_schema(&conn, sch)
        .await
        .expect("snap sequence options");
    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift.is_clean(),
        "authored sequence options must round-trip through live PG catalog: {drift:?}"
    );

    conn.batch_execute(&format!(
        "ALTER SEQUENCE \"{sch}\".\"invoice_seq\" INCREMENT BY -4;"
    ))
    .await
    .expect("mutate sequence increment out of band");
    let changed = snapshot_schema(&conn, sch)
        .await
        .expect("snap changed sequence options");
    let changed_drift = diff_snapshots(&expected, &changed);
    assert!(
        altered_contains(&changed_drift, "sequence invoice_seq", "increment"),
        "sequence option drift must be detected: {changed_drift:?}"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn vendor_privileged_object_attributes_round_trip_and_drift() {
    let conn = pg().await;
    // Extensions are database-global by name. Serialize this regression's
    // hstore install/drop sequence without touching the shared :5440 server.
    conn.batch_execute("SELECT pg_advisory_lock(216005)")
        .await
        .expect("take vendor attribute advisory lock");

    let tok = token();
    let cfg = cfg_for(&tok);
    let role = format!("zsr_{tok}");
    let parent_role = format!("zsr_parent_{tok}");
    let other_owner = format!("zsr_owner_{tok}");
    let ext_schema = format!("ext_{tok}");
    cleanup_vendor_attribute_objects(
        &conn,
        &cfg,
        &ext_schema,
        &[&role, &parent_role, &other_owner],
    )
    .await;

    let sch = &cfg.project_schema;
    let ops = vec![
        Op::CreateRole {
            name: parent_role.clone(),
            login: None,
            password: None,
            bypass_rls: None,
            create_role: None,
            create_db: None,
            superuser: None,
            in_role: None,
            set_search_path: None,
            if_not_exists: Some(true),
        },
        Op::CreateRole {
            name: role.clone(),
            login: Some(true),
            password: None,
            bypass_rls: Some(false),
            create_role: Some(false),
            create_db: Some(false),
            superuser: Some(false),
            in_role: Some(vec![parent_role.clone()]),
            set_search_path: None,
            if_not_exists: Some(true),
        },
        Op::CreateRole {
            name: other_owner.clone(),
            login: None,
            password: None,
            bypass_rls: None,
            create_role: None,
            create_db: None,
            superuser: None,
            in_role: None,
            set_search_path: None,
            if_not_exists: Some(true),
        },
        Op::CreateSchema {
            name: sch.clone(),
            if_not_exists: Some(true),
            authorization: Some(role.clone()),
        },
        Op::CreateExtension {
            name: "hstore".into(),
            if_not_exists: Some(true),
            schema: Some(sch.clone()),
        },
    ];
    let migrations = lower_ir_migrations(sch, "vendor_privileged_attributes", &ops);
    for migration in &migrations {
        conn.batch_execute(&migration.up)
            .await
            .unwrap_or_else(|e| panic!("apply {}: {e}", migration.name));
    }

    let expected = fold_ops(&ops, SqlDialect::Postgres, sch).expect("fold vendor attrs");
    let actual = snapshot_schema(&conn, sch)
        .await
        .expect("snapshot vendor attrs");
    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift.is_clean(),
        "authored privileged object attributes must round-trip through live PG catalog: {drift:?}"
    );

    conn.batch_execute(&format!(
        "CREATE SCHEMA \"{ext_schema}\"; \
         ALTER ROLE \"{role}\" BYPASSRLS; \
         ALTER SCHEMA \"{sch}\" OWNER TO \"{other_owner}\"; \
         ALTER EXTENSION hstore SET SCHEMA \"{ext_schema}\";"
    ))
    .await
    .expect("mutate privileged object attributes out of band");
    let changed = snapshot_schema(&conn, sch)
        .await
        .expect("snapshot changed vendor attrs");
    let changed_drift = diff_snapshots(&expected, &changed);
    assert!(
        altered_contains(&changed_drift, &format!("role {role}"), "bypass_rls"),
        "role BYPASSRLS drift must be detected: {changed_drift:?}"
    );
    assert!(
        altered_contains(&changed_drift, &format!("schema {sch}"), "owner"),
        "schema owner drift must be detected: {changed_drift:?}"
    );
    assert!(
        altered_contains(&changed_drift, "extension hstore", "schema"),
        "extension placement drift must be detected: {changed_drift:?}"
    );

    cleanup_vendor_attribute_objects(
        &conn,
        &cfg,
        &ext_schema,
        &[&role, &parent_role, &other_owner],
    )
    .await;
    conn.batch_execute("SELECT pg_advisory_unlock(216005)")
        .await
        .expect("release vendor attribute advisory lock");
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
