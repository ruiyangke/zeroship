//! **PR15 (HIGH fix) — FAITHFUL apply-level e2e for `createTable` TABLE-LEVEL
//! constraints + indexes on real Postgres (`:5440`).**
//!
//! Before this fix, `table().create({ uniques, foreignKeys, indexes })` RECORDED
//! the table-level specs into the IR but the apply path silently DROPPED them
//! (`create_table_descriptor` carried only columns). This suite authors a
//! `createTable` whose IR carries a named UNIQUE + a table-level single-`id`
//! FOREIGN KEY + an extra INDEX, applies it through the REAL load-gate +
//! `IrAuthor::lower_plan` + `MigrationEngine::apply_plan` under the least-priv
//! migrator role, and asserts each object is PRESENT in the live catalog
//! (`pg_constraint` / `pg_indexes`). It would FAIL pre-fix (the constraints/index
//! never reach the DDL).
//!
//! It also pins the FAIL-CLOSED arms (HIGH-finding mandate "never a silent
//! no-op"): a composite/per-column user PRIMARY KEY and a table-level CHECK are
//! HARD validate-time authoring errors, not silent drops.
//!
//! Requires `:5440` (the `*_pg` suite convention); run with `--test-threads=1`.

use compio_postgres::Client;
use zeroship_migrate::model::load::IrLoadError;
use zeroship_migrate::model::validate::{UnsupportedKind, CODE_UNSUPPORTED};
use zeroship_migrate::{
    apply::executor::LockMode,
    provision_migrator, apply::role::deprovision_migrator, Approval, ExecutorConfig, IrAuthor, LiveSchema,
    resolve_create_table_policy, MigrationEngine, MigrationIr, PolicyProfile, SqlDialect,
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

const APP: &str = "app_test";

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.pg.meta_schema = format!("meta_{tok}");
    let role = zeroship_migrate::migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", cfg.project_schema))
        .await
        .expect("create project schema");
    provision_migrator(conn, cfg).await.expect("provision migrator role");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.pg.meta_schema
        ))
        .await;
}

fn registry(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
    pairs.iter().map(|(t, o)| (t.to_string(), o.to_string())).collect()
}

async fn author_and_apply(
    conn: &Client,
    cfg: &ExecutorConfig,
    ir: &str,
    reg: &std::collections::BTreeMap<String, String>,
    approval: Approval,
) {
    let raw: MigrationIr = serde_json::from_str(ir).expect("test IR parses before resolution");
    let resolved = resolve_create_table_policy(&raw, &PolicyProfile::confined())
        .expect("test IR resolves confined table shape");
    let resolved_json = serde_json::to_string(&resolved).expect("resolved IR serializes");
    let author = IrAuthor::new(cfg.project_schema.clone(), APP, SqlDialect::Postgres);
    let document = zeroship_migrate::model::load::load_ir_document(
        &resolved_json,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
        reg,
        None,
        None,
    )
    .expect("load gate");
    let plan = author
        .lower_plan(&document, &LiveSchema::default())
        .expect("lower the createTable plan on PG");
    let engine = MigrationEngine::new();
    engine
        .apply_plan(
            &plan.steps,
            approval,
            &zeroship_migrate::PostgresBackend::new(conn),
            cfg,
            APP,
            LockMode::Acquire,
        )
        .await
        .expect("apply the authored createTable plan on PG");
}

/// `pg_constraint` row count for a named constraint of a given contype on a table.
async fn constraint_kind(conn: &Client, schema: &str, table: &str, name: &str) -> Option<String> {
    let rows = conn
        .query(
            "SELECT c.contype::text FROM pg_constraint c \
             JOIN pg_class t ON t.oid = c.conrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = $1 AND t.relname = $2 AND c.conname = $3",
            &[&schema, &table, &name],
        )
        .await
        .expect("query pg_constraint");
    rows.first().map(|r| r.get::<_, String>(0))
}

async fn index_exists(conn: &Client, schema: &str, table: &str, name: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM pg_indexes WHERE schemaname = $1 AND tablename = $2 AND indexname = $3",
            &[&schema, &table, &name],
        )
        .await
        .expect("query pg_indexes");
    !rows.is_empty()
}

async fn column_occurrences(conn: &Client, schema: &str, table: &str, column: &str) -> i64 {
    let rows = conn
        .query(
            "SELECT COUNT(*)::bigint FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query information_schema.columns");
    rows[0].get::<_, i64>(0)
}

/// Slice 4 regression: a resolved confined `createTable` applies with exactly one
/// copy of every system column. If lower re-injected after record-time resolution,
/// this would fail at DDL time or show duplicate catalog entries.
#[compio::test]
async fn resolved_confined_create_table_applies_without_double_injection_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let schema = cfg.project_schema.clone();

    let raw = r#"{"ir_version":1,"name":"create_widgets","ops":[
        {"op":"createTable","name":"widgets","columns":[
            {"name":"title","type":"text","nullable":false}
        ]}
    ]}"#;
    author_and_apply(&conn, &cfg, raw, &registry(&[]), Approval::None).await;

    for column in [
        "id",
        "created_at",
        "updated_at",
        "created_by",
        "updated_by",
        "version",
        "deleted_at",
        "title",
    ] {
        assert_eq!(
            column_occurrences(&conn, &schema, "widgets", column).await,
            1,
            "{column} should appear exactly once"
        );
    }

    teardown(&conn, &cfg).await;
}

/// HIGH-fix regression: a createTable carrying a table-level UNIQUE + a
/// single-`id` FOREIGN KEY + an extra INDEX lowers them to live DDL.
#[compio::test]
async fn create_table_level_unique_fk_and_index_apply_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let schema = cfg.project_schema.clone();

    // The FK target table first (so the FK inlines against a live table).
    let teams = r#"{"ir_version":1,"name":"create_teams","ops":[
        {"op":"createTable","name":"teams","columns":[
            {"name":"label","type":"text","nullable":false}
        ]}
    ]}"#;
    author_and_apply(&conn, &cfg, teams, &registry(&[]), Approval::None).await;

    // The table whose IR carries a named UNIQUE, a table-level single-`id` FK to
    // `teams`, and an extra index — all of which were silently dropped pre-fix.
    let memberships = r#"{"ir_version":1,"name":"create_memberships","ops":[
        {"op":"createTable","name":"memberships","columns":[
            {"name":"team_id","type":"text","nullable":false},
            {"name":"slot","type":"text","nullable":false}
        ],
        "constraints":[
            {"name":"m_slot_uq","kind":{"kind":"unique","columns":["slot"]}},
            {"name":"m_team_fk","kind":{"kind":"fk","columns":["team_id"],
                "referencesTable":"teams","referencesColumns":["id"]}}
        ],
        "indexes":[
            {"name":"m_team_idx","columns":[{"kind":"column","name":"team_id"}]}
        ]}
    ]}"#;
    author_and_apply(
        &conn,
        &cfg,
        memberships,
        &registry(&[("teams", APP)]),
        Approval::None,
    )
    .await;

    assert_eq!(
        constraint_kind(&conn, &schema, "memberships", "m_slot_uq").await.as_deref(),
        Some("u"),
        "the named UNIQUE constraint must be present in the live catalog (was silently dropped pre-fix)"
    );
    assert_eq!(
        constraint_kind(&conn, &schema, "memberships", "m_team_fk").await.as_deref(),
        Some("f"),
        "the table-level FOREIGN KEY must be present in the live catalog (was silently dropped pre-fix)"
    );
    assert!(
        index_exists(&conn, &schema, "memberships", "m_team_idx").await,
        "the extra table-level index must be present in the live catalog (was silently dropped pre-fix)"
    );

    teardown(&conn, &cfg).await;
}

/// HIGH-fix fail-closed: a table-level PRIMARY KEY constraint is a HARD error,
/// never a silent no-op. Top-level `primaryKey` is policy-gated separately.
#[compio::test]
async fn create_table_user_primary_key_is_hard_error_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let ir = r#"{"ir_version":1,"name":"create_pk","ops":[
        {"op":"createTable","name":"pk_tbl","columns":[
            {"name":"a","type":"text","nullable":false},
            {"name":"b","type":"text","nullable":false}
        ],
        "constraints":[
            {"kind":{"kind":"pk","columns":["a","b"]}}
        ]}
    ]}"#;
    let policy = PolicyProfile::platform();
    let err = zeroship_migrate::model::load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
        &registry(&[]),
        None,
        Some(&policy),
    )
    .expect_err("a table-level PRIMARY KEY must validate-refuse, not silently no-op");
    let IrLoadError::Validate(err) = err else {
        panic!("expected validate error for user PRIMARY KEY, got {err:?}");
    };
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert_eq!(err.kind, Some(UnsupportedKind::Op));
    assert!(
        err.reason.contains("primary key") || err.reason.contains("primary keys"),
        "the error must name the unsupported PRIMARY KEY (got: {err:?})"
    );

    teardown(&conn, &cfg).await;
}

/// HIGH-fix fail-closed: a table-level CHECK is a validate-time HARD error, never
/// a silent drop.
#[compio::test]
async fn create_table_check_is_hard_error_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let ir = r#"{"ir_version":1,"name":"create_chk","ops":[
        {"op":"createTable","name":"chk_tbl","columns":[
            {"name":"age","type":"int","nullable":false}
        ],
        "constraints":[
            {"name":"age_pos","kind":{"kind":"check","expr":{"node":"binOp","op":"gt",
                "lhs":{"node":"colRef","name":"age"},
                "rhs":{"node":"literal","value":0}}}}
        ]}
    ]}"#;
    let policy = PolicyProfile::platform();
    let err = zeroship_migrate::model::load::load_ir_document(
        ir,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
        &registry(&[]),
        None,
        Some(&policy),
    )
    .expect_err("a table-level CHECK must validate-refuse, not be dropped");
    let IrLoadError::Validate(err) = err else {
        panic!("expected validate error for table CHECK, got {err:?}");
    };
    assert_eq!(err.code, CODE_UNSUPPORTED);
    assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    assert!(
        err.reason.to_lowercase().contains("check"),
        "the error must name the deferred CHECK/expr (got: {err:?})"
    );

    teardown(&conn, &cfg).await;
}
