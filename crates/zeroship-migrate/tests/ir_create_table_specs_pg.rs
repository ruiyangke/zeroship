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
//! no-op"): a composite/per-column user PRIMARY KEY is a HARD validate-time
//! authoring error, not a silent drop. Table-level CHECKs render/apply on PG.
//!
//! Requires `:5440` (the `*_pg` suite convention); run with `--test-threads=1`.

use compio_postgres::Client;
use zeroship_migrate::model::validate::{UnsupportedKind, CODE_UNSUPPORTED};
use zeroship_migrate::{
    apply::executor::LockMode,
    apply::role::deprovision_migrator, provision_migrator, AppliedPlan, Approval, ExecutorConfig,
    IrAuthor, LiveSchema, MigrationEngine, MigrationIr, PolicyProfile, SqlDialect,
    resolve_create_table_policy,
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
) -> AppliedPlan {
    let policy = PolicyProfile::confined();
    author_and_apply_with_policy(conn, cfg, ir, reg, approval, &policy).await
}

async fn author_and_apply_with_policy(
    conn: &Client,
    cfg: &ExecutorConfig,
    ir: &str,
    reg: &std::collections::BTreeMap<String, String>,
    approval: Approval,
    policy: &PolicyProfile,
) -> AppliedPlan {
    let raw: MigrationIr = serde_json::from_str(ir).expect("test IR parses before resolution");
    let resolved =
        resolve_create_table_policy(&raw, policy).expect("test IR resolves table shape");
    let resolved_json = serde_json::to_string(&resolved).expect("resolved IR serializes");
    let author = IrAuthor::new(cfg.project_schema.clone(), APP, SqlDialect::Postgres);
    let document = zeroship_migrate::model::load::load_ir_document(
        &resolved_json,
        APP,
        zeroship_migrate::model::validate::Dialect::Postgres,
        reg,
        None,
        Some(policy),
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
    plan
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

async fn constraint_definition(
    conn: &Client,
    schema: &str,
    table: &str,
    name: &str,
) -> Option<String> {
    let rows = conn
        .query(
            "SELECT pg_get_constraintdef(c.oid) \
             FROM pg_constraint c \
             JOIN pg_class t ON t.oid = c.conrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             WHERE n.nspname = $1 AND t.relname = $2 AND c.conname = $3",
            &[&schema, &table, &name],
        )
        .await
        .expect("query pg_get_constraintdef");
    rows.first().map(|r| r.get::<_, String>(0))
}

async fn domain_constraint_definition(conn: &Client, schema: &str, domain: &str) -> Option<String> {
    let rows = conn
        .query(
            "SELECT pg_get_constraintdef(c.oid) \
             FROM pg_constraint c \
             JOIN pg_type t ON t.oid = c.contypid \
             JOIN pg_namespace n ON n.oid = t.typnamespace \
             WHERE n.nspname = $1 AND t.typname = $2 AND c.contype = 'c'",
            &[&schema, &domain],
        )
        .await
        .expect("query domain pg_get_constraintdef");
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

async fn primary_key_columns(conn: &Client, schema: &str, table: &str) -> Option<Vec<String>> {
    let rows = conn
        .query(
            "SELECT a.attname \
             FROM pg_constraint c \
             JOIN pg_class t ON t.oid = c.conrelid \
             JOIN pg_namespace n ON n.oid = t.relnamespace \
             JOIN unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord) ON true \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
             WHERE n.nspname = $1 AND t.relname = $2 AND c.contype = 'p' \
             ORDER BY k.ord",
            &[&schema, &table],
        )
        .await
        .expect("query primary key columns");
    if rows.is_empty() {
        None
    } else {
        Some(rows.into_iter().map(|r| r.get::<_, String>(0)).collect())
    }
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

#[compio::test]
async fn platform_composite_primary_key_lowers_and_applies_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let schema = cfg.project_schema.clone();
    let policy = PolicyProfile::platform();

    let ir = r#"{"ir_version":1,"name":"create_memberships","ops":[
        {"op":"createTable","name":"memberships","columns":[
            {"name":"account_id","type":"uuid","nullable":false},
            {"name":"team","type":"text","nullable":false}
        ],
        "primaryKey":["account_id","team"],
        "constraints":[],
        "indexes":[]}
    ]}"#;
    let plan = author_and_apply_with_policy(
        &conn,
        &cfg,
        ir,
        &registry(&[]),
        Approval::None,
        &policy,
    )
    .await;
    let create_sql = plan
        .steps
        .iter()
        .find_map(|step| match step {
            zeroship_migrate::PlanStep::Ddl(m) if m.up.contains("CREATE TABLE") => Some(&m.up),
            _ => None,
        })
        .expect("CREATE TABLE step");
    assert!(
        create_sql.contains("PRIMARY KEY (account_id, team)"),
        "platform composite primaryKey must lower to a composite PRIMARY KEY:\n{create_sql}"
    );
    assert_eq!(
        primary_key_columns(&conn, &schema, "memberships").await,
        Some(vec!["account_id".to_string(), "team".to_string()]),
        "live PG must have the composite primary key"
    );

    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn platform_null_primary_key_lowers_and_applies_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let schema = cfg.project_schema.clone();
    let policy = PolicyProfile::platform();

    let ir = r#"{"ir_version":1,"name":"create_events","ops":[
        {"op":"createTable","name":"events","columns":[
            {"name":"stream","type":"text","nullable":false},
            {"name":"payload","type":"json","nullable":false}
        ],
        "primaryKey":null,
        "constraints":[],
        "indexes":[]}
    ]}"#;
    let plan = author_and_apply_with_policy(
        &conn,
        &cfg,
        ir,
        &registry(&[]),
        Approval::None,
        &policy,
    )
    .await;
    let create_sql = plan
        .steps
        .iter()
        .find_map(|step| match step {
            zeroship_migrate::PlanStep::Ddl(m) if m.up.contains("CREATE TABLE") => Some(&m.up),
            _ => None,
        })
        .expect("CREATE TABLE step");
    assert!(
        !create_sql.contains("PRIMARY KEY"),
        "platform primaryKey:null must lower with no PRIMARY KEY:\n{create_sql}"
    );
    assert_eq!(
        primary_key_columns(&conn, &schema, "events").await,
        None,
        "live PG must have no primary key"
    );

    teardown(&conn, &cfg).await;
}

/// Slice A: current-AST table-level CHECK constraints render on PG both in
/// createTable and stand-alone addConstraint, and apply to the live catalog.
#[compio::test]
async fn table_level_checks_render_and_apply_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let schema = cfg.project_schema.clone();

    let ir = r#"{"ir_version":1,"name":"check_cases","ops":[
        {"op":"createTable","name":"check_cases","columns":[
            {"name":"a","type":"int","nullable":false},
            {"name":"b","type":"int","nullable":true},
            {"name":"subtotal","type":"int","nullable":false},
            {"name":"tax","type":"int","nullable":false},
            {"name":"total","type":"int","nullable":false},
            {"name":"score","type":"int","nullable":false}
        ],
        "constraints":[
            {"name":"checks_a_nonnegative","kind":{"kind":"check","expr":{"node":"binOp","op":"ge",
                "lhs":{"node":"colRef","name":"a"},
                "rhs":{"node":"literal","value":0}}}},
            {"name":"checks_b_null_or_nonnegative","kind":{"kind":"check","expr":{"node":"binOp","op":"or",
                "lhs":{"node":"unaryOp","op":"isNull","operand":{"node":"colRef","name":"b"}},
                "rhs":{"node":"binOp","op":"ge",
                    "lhs":{"node":"colRef","name":"b"},
                    "rhs":{"node":"literal","value":0}}}}}
        ]},
        {"op":"addConstraint","table":"check_cases","constraint":
            {"name":"checks_total_matches_parts","kind":{"kind":"check","expr":{"node":"binOp","op":"eq",
                "lhs":{"node":"colRef","name":"total"},
                "rhs":{"node":"binOp","op":"add",
                    "lhs":{"node":"colRef","name":"subtotal"},
                    "rhs":{"node":"colRef","name":"tax"}}}}}},
        {"op":"addConstraint","table":"check_cases","constraint":
            {"name":"checks_score_range","kind":{"kind":"check","expr":{"node":"binOp","op":"and",
                "lhs":{"node":"binOp","op":"ge",
                    "lhs":{"node":"colRef","name":"score"},
                    "rhs":{"node":"literal","value":0}},
                "rhs":{"node":"binOp","op":"le",
                    "lhs":{"node":"colRef","name":"score"},
                    "rhs":{"node":"literal","value":100}}}}}}
    ]}"#;
    let plan = author_and_apply(&conn, &cfg, ir, &registry(&[]), Approval::Approved).await;
    let rendered = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            zeroship_migrate::PlanStep::Ddl(m) => Some(m.up.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        r#"CONSTRAINT "checks_a_nonnegative" CHECK (("a" >= 0))"#,
        r#"CONSTRAINT "checks_b_null_or_nonnegative" CHECK ((("b" IS NULL) OR ("b" >= 0)))"#,
        r#"ADD CONSTRAINT "checks_total_matches_parts" CHECK (("total" = ("subtotal" + "tax")))"#,
        r#"ADD CONSTRAINT "checks_score_range" CHECK ((("score" >= 0) AND ("score" <= 100)))"#,
    ] {
        assert!(rendered.contains(expected), "missing rendered CHECK `{expected}` in:\n{rendered}");
    }

    for (name, expected) in [
        ("checks_a_nonnegative", "CHECK ((a >= 0))"),
        (
            "checks_b_null_or_nonnegative",
            "CHECK (((b IS NULL) OR (b >= 0)))",
        ),
        (
            "checks_total_matches_parts",
            "CHECK ((total = (subtotal + tax)))",
        ),
        (
            "checks_score_range",
            "CHECK (((score >= 0) AND (score <= 100)))",
        ),
    ] {
        assert_eq!(
            constraint_kind(&conn, &schema, "check_cases", name).await.as_deref(),
            Some("c"),
            "{name} should be a live CHECK constraint"
        );
        assert_eq!(
            constraint_definition(&conn, &schema, "check_cases", name)
                .await
                .as_deref(),
            Some(expected),
            "{name} should have the canonical live CHECK definition"
        );
    }
    assert!(
        conn.batch_execute(&format!(
            "INSERT INTO \"{}\".\"check_cases\" \
             (id, created_at, updated_at, version, a, b, subtotal, tax, total, score) \
             VALUES ('ok', now(), now(), 1, 0, NULL, 2, 3, 5, 100)",
            schema
        ))
        .await
        .is_ok(),
        "valid rows should satisfy every rendered CHECK"
    );

    teardown(&conn, &cfg).await;
}

/// Slice B: PG-only expression nodes for text-array membership, regex, and
/// pg_column_size render in the platform pg_dump idiom and apply as live CHECKs.
#[compio::test]
async fn pg_only_expr_nodes_render_and_apply_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let schema = cfg.project_schema.clone();

    let ir = r#"{"ir_version":1,"name":"pg_expr_checks","ops":[
        {"op":"createTable","name":"pg_expr_checks","columns":[
            {"name":"status","type":"text","nullable":false},
            {"name":"name","type":"text","nullable":false},
            {"name":"data","type":"json","nullable":false}
        ],
        "constraints":[
            {"name":"status_any_check","kind":{"kind":"check","expr":{
                "node":"pgArrayMembership",
                "expr":{"node":"colRef","name":"status"},
                "op":"eq",
                "elems":["a","b"]}}},
            {"name":"status_ne_all_check","kind":{"kind":"check","expr":{
                "node":"pgArrayMembership",
                "expr":{"node":"colRef","name":"status"},
                "op":"ne",
                "elems":["x"]}}},
            {"name":"name_regex_check","kind":{"kind":"check","expr":{
                "node":"pgRegexMatch",
                "expr":{"node":"colRef","name":"name"},
                "pattern":"^[a-z]+$"}}},
            {"name":"data_size_check","kind":{"kind":"check","expr":{
                "node":"binOp","op":"le",
                "lhs":{"node":"pgColumnSize","expr":{"node":"colRef","name":"data"}},
                "rhs":{"node":"literal","value":8192}}}}
        ]}
    ]}"#;
    let plan = author_and_apply(&conn, &cfg, ir, &registry(&[]), Approval::Approved).await;
    let rendered = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            zeroship_migrate::PlanStep::Ddl(m) => Some(m.up.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        r#"CONSTRAINT "status_any_check" CHECK (("status" = ANY (ARRAY['a'::text, 'b'::text])))"#,
        r#"CONSTRAINT "status_ne_all_check" CHECK (("status" <> ALL (ARRAY['x'::text])))"#,
        r#"CONSTRAINT "name_regex_check" CHECK (("name" ~ '^[a-z]+$'::text))"#,
        r#"CONSTRAINT "data_size_check" CHECK ((pg_column_size("data") <= 8192))"#,
    ] {
        assert!(
            rendered.contains(expected),
            "missing rendered PG-only CHECK `{expected}` in:\n{rendered}"
        );
    }

    for (name, expected) in [
        (
            "status_any_check",
            "CHECK ((status = ANY (ARRAY['a'::text, 'b'::text])))",
        ),
        (
            "status_ne_all_check",
            "CHECK ((status <> ALL (ARRAY['x'::text])))",
        ),
    ] {
        assert_eq!(
            constraint_kind(&conn, &schema, "pg_expr_checks", name).await.as_deref(),
            Some("c"),
            "{name} should be a live CHECK constraint"
        );
        assert_eq!(
            constraint_definition(&conn, &schema, "pg_expr_checks", name)
                .await
                .as_deref(),
            Some(expected),
            "{name} should have the canonical live CHECK definition"
        );
    }
    for (name, expected) in [
        ("name_regex_check", "CHECK ((name ~ '^[a-z]+$'::text))"),
        (
            "data_size_check",
            "CHECK ((pg_column_size(data) <= 8192))",
        ),
    ] {
        assert_eq!(
            constraint_kind(&conn, &schema, "pg_expr_checks", name).await.as_deref(),
            Some("c"),
            "{name} should be a live CHECK constraint"
        );
        assert_eq!(
            constraint_definition(&conn, &schema, "pg_expr_checks", name)
                .await
                .as_deref(),
            Some(expected),
            "{name} should have the canonical live CHECK definition"
        );
    }

    assert!(
        conn.batch_execute(&format!(
            "INSERT INTO \"{}\".\"pg_expr_checks\" \
             (id, created_at, updated_at, version, status, name, data) \
             VALUES ('ok', now(), now(), 1, 'a', 'abc', '{{}}'::jsonb)",
            schema
        ))
        .await
        .is_ok(),
        "valid rows should satisfy every PG-only CHECK"
    );

    teardown(&conn, &cfg).await;
}

/// Slice C: PG-only EXTRACT(day FROM ...) and strict interval literal nodes render
/// to the platform pg_dump idioms and apply as live CHECK constraints.
#[compio::test]
async fn pg_extract_and_interval_literal_render_and_apply_on_pg() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;
    let schema = cfg.project_schema.clone();

    let ir = r#"{"ir_version":1,"name":"pg_expr_slice_c","ops":[
        {"op":"createDomain","name":"billing_period","as":"date","check":{
            "node":"binOp","op":"eq",
            "lhs":{"node":"extract","field":"day","from":{"node":"colRef","name":"VALUE"}},
            "rhs":{"node":"literal","value":1}}},
        {"op":"createTable","name":"oauth_device_codes","columns":[
            {"name":"issued_at","type":"timestamp","nullable":false},
            {"name":"expires_at","type":"timestamp","nullable":false}
        ],
        "constraints":[
            {"name":"expires_window_check","kind":{"kind":"check","expr":{
                "node":"binOp","op":"le",
                "lhs":{"node":"colRef","name":"expires_at"},
                "rhs":{"node":"binOp","op":"add",
                    "lhs":{"node":"colRef","name":"issued_at"},
                    "rhs":{"node":"pgInterval","duration":"00:01:00"}}}}}
        ]}
    ]}"#;
    let plan = author_and_apply(&conn, &cfg, ir, &registry(&[]), Approval::Approved).await;
    let rendered = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            zeroship_migrate::PlanStep::Ddl(m) => Some(m.up.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains(&format!(
            r#"CREATE DOMAIN "{schema}"."billing_period" AS date CHECK ((EXTRACT(day FROM VALUE) = 1))"#
        )),
        "missing rendered EXTRACT domain CHECK in:\n{rendered}"
    );
    assert!(
        rendered.contains(
            r#"CONSTRAINT "expires_window_check" CHECK (("expires_at" <= ("issued_at" + '00:01:00'::interval)))"#
        ),
        "missing rendered interval CHECK in:\n{rendered}"
    );

    let domain_def = domain_constraint_definition(&conn, &schema, "billing_period").await;
    assert!(
        domain_def
            .as_deref()
            .is_some_and(|def| def.contains("EXTRACT(day FROM VALUE)")),
        "billing_period domain should have a live EXTRACT(day FROM VALUE) CHECK, got {domain_def:?}"
    );
    conn.batch_execute(&format!(
        r#"CREATE TABLE "{schema}".domain_probe (period "{schema}"."billing_period");
           INSERT INTO "{schema}".domain_probe (period) VALUES (DATE '2026-07-01');"#
    ))
    .await
    .expect("first-of-month domain value should satisfy the EXTRACT check");
    assert!(
        conn.batch_execute(&format!(
            r#"INSERT INTO "{schema}".domain_probe (period) VALUES (DATE '2026-07-02')"#
        ))
        .await
        .is_err(),
        "non-first-of-month value should fail the EXTRACT domain check"
    );

    assert_eq!(
        constraint_definition(&conn, &schema, "oauth_device_codes", "expires_window_check")
            .await
            .as_deref(),
        Some("CHECK ((expires_at <= (issued_at + '00:01:00'::interval)))"),
        "interval arithmetic should be present in the live CHECK definition"
    );
    conn.batch_execute(&format!(
        r#"INSERT INTO "{schema}"."oauth_device_codes"
           (id, created_at, updated_at, version, issued_at, expires_at)
           VALUES ('ok', now(), now(), 1,
                   TIMESTAMPTZ '2026-07-01 00:00:00+00',
                   TIMESTAMPTZ '2026-07-01 00:00:30+00')"#
    ))
    .await
    .expect("expires_at within the interval literal should satisfy the CHECK");
    assert!(
        conn.batch_execute(&format!(
            r#"INSERT INTO "{schema}"."oauth_device_codes"
               (id, created_at, updated_at, version, issued_at, expires_at)
               VALUES ('bad', now(), now(), 1,
                       TIMESTAMPTZ '2026-07-01 00:00:00+00',
                       TIMESTAMPTZ '2026-07-01 00:02:00+00')"#
        ))
        .await
        .is_err(),
        "expires_at outside the interval literal should fail the CHECK"
    );

    teardown(&conn, &cfg).await;
}

#[test]
fn pg_extract_and_interval_literal_validate_refuse_non_pg() {
    use zeroship_migrate::model::validate::{validate_ir, Dialect};

    let cases = [
        r#"{"ir_version":1,"name":"extract_refuse","ops":[
            {"op":"update","table":"t","set":{"x":{
                "node":"extract","field":"day","from":{"node":"colRef","name":"x"}}}}
        ]}"#,
        r#"{"ir_version":1,"name":"interval_refuse","ops":[
            {"op":"update","table":"t","set":{"x":{"node":"pgInterval","duration":"00:01:00"}}}
        ]}"#,
    ];

    for raw in cases {
        let ir: MigrationIr = serde_json::from_str(raw).expect("test IR parses");
        for dialect in [Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_ir(&ir, dialect, &[])
                .expect_err("PG-only expression nodes must validate-refuse off PG");
            assert_eq!(err.code, CODE_UNSUPPORTED);
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
    }
}
