use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::model::ir::{IndexElement, IndexMethod, IrFlagsOverride, Op};
use zero_migrate::model::validate::{validate_ir, Dialect, CODE_OP_INVALID};
use zero_migrate::{
    resolve_create_table_policy, Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema,
    MigrationEngine, MigrationIr, PlanStep, SqlDialect, SqliteBackend, CURRENT_IR_VERSION,
};

const PROJECT: &str = "prj_dialectal";
const APP: &str = "app_dialectal";

fn ir(name: &str, ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: name.to_string(),
        owner_app: APP.to_string(),
        ops,
        flags: IrFlagsOverride::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

fn hnsw_index_op() -> Op {
    Op::CreateIndex {
        table: "docs".into(),
        columns: vec![IndexElement::Column {
            name: "embedding".into(),
            order: None,
            opclass: None,
            collation: None,
        }],
        name: Some("docs_embedding_hnsw_idx".into()),
        unique: None,
        using: Some(IndexMethod::Hnsw),
        r#where: None,
        concurrently: None,
        include: vec![],
        with: None,
        only: None,
        nulls_not_distinct: None,
        schema: None,
        existence_guard: None,
    }
}

fn pg_only_ir() -> MigrationIr {
    ir(
        "pg_only_hnsw",
        vec![Op::Dialectal {
            default: None,
            pg: Some(vec![hnsw_index_op()]),
            sqlite: None,
            mysql: None,
        }],
    )
}

#[test]
fn lower_selects_pg_leg_and_skips_absent_sqlite_mysql_legs() {
    let pg_steps = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Postgres,
        &zero_migrate::zeroship_no_inject_ceiling(),
    )
    .lower_steps(&pg_only_ir(), &LiveSchema::default())
    .expect("PG dialectal leg lowers");
    assert_eq!(pg_steps.len(), 1);
    let PlanStep::Ddl(mig) = &pg_steps[0] else {
        panic!("PG hnsw leg should lower to DDL: {pg_steps:#?}");
    };
    assert!(
        mig.up.contains("USING hnsw"),
        "PG leg should render the HNSW index: {}",
        mig.up
    );

    for dialect in [SqlDialect::Sqlite, SqlDialect::Mysql] {
        let steps = IrAuthor::new(
            PROJECT,
            APP,
            dialect,
            &zero_migrate::zeroship_no_inject_ceiling(),
        )
        .lower_steps(&pg_only_ir(), &LiveSchema::default())
        .unwrap_or_else(|err| panic!("{dialect:?} absent dialectal leg should skip: {err}"));
        assert!(
            steps.is_empty(),
            "{dialect:?} should skip absent pg-only leg"
        );
    }
}

struct Paths {
    _dir: TempDir,
    app: PathBuf,
    journal: PathBuf,
}

fn paths(tag: &str) -> Paths {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join(format!("zs-{tag}.sqlite"));
    let journal = dir.path().join(format!("zs-{tag}.migrations.sqlite"));
    Paths {
        _dir: dir,
        app,
        journal,
    }
}

fn backend(p: &Paths) -> SqliteBackend {
    SqliteBackend::open(&p.app, &p.journal).expect("open hardened sqlite backend")
}

fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(t, o)| (t.to_string(), o.to_string()))
        .collect()
}

fn resolved_envelope_json(raw: &str) -> String {
    let ir: MigrationIr = serde_json::from_str(raw).expect("test IR parses");
    let resolved =
        resolve_create_table_policy(&ir, &zero_migrate::zeroship_confined_ceiling(), PROJECT)
            .expect("test IR resolves");
    serde_json::to_string(&resolved).expect("resolved test IR serializes")
}

#[compio::test]
async fn sqlite_apply_skips_absent_pg_leg_without_column_effect() {
    let p = paths("sqlite_skip");
    let be = backend(&p);
    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"sqlite_skip_pg_leg","ops":[
          {"op":"createTable","name":"docs","columns":[{"name":"title","type":"text"}]},
          {"op":"dialectal","pg":[
            {"op":"addColumn","table":"docs","column":"pg_only","type":"text"}
          ]}
        ]}"#,
    );

    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Sqlite,
        &zero_migrate::zeroship_confined_ceiling(),
    );
    let migrations = author
        .load_and_lower(&ir, APP, &registry(&[]), &LiveSchema::default())
        .expect("SQLite should lower createTable and skip absent dialectal leg");
    assert_eq!(
        migrations.len(),
        1,
        "SQLite lower should emit only createTable; dialectal pg leg is absent"
    );

    let engine = MigrationEngine::new();
    let guard_cfg = GuardConfig::confined_sqlite(PROJECT.to_string());
    let plan = engine.plan(&migrations, &guard_cfg);
    assert!(
        plan.denied.is_empty(),
        "clean SQLite plan should not be denied"
    );
    engine
        .apply(
            &plan,
            Approval::None,
            &be,
            &ExecutorConfig::new(PROJECT, PROJECT),
            "deploy-dialectal",
        )
        .await
        .expect("apply SQLite plan");

    let rows = be
        .actor()
        .query("SELECT name FROM pragma_table_info('docs') WHERE name='pg_only'")
        .await
        .expect("pragma_table_info probe");
    assert!(
        rows.is_empty(),
        "SQLite must not apply the absent pg-only leg"
    );
}

#[test]
fn validate_rejects_empty_and_nested_dialectal_ops() {
    let empty = ir(
        "empty",
        vec![Op::Dialectal {
            default: None,
            pg: None,
            sqlite: None,
            mysql: None,
        }],
    );
    let err = validate_ir(&empty, Dialect::Postgres, &[]).unwrap_err();
    assert_eq!(err.code, CODE_OP_INVALID);

    let nested = ir(
        "nested",
        vec![Op::Dialectal {
            default: None,
            pg: Some(vec![Op::Dialectal {
                default: None,
                pg: Some(Vec::new()),
                sqlite: None,
                mysql: None,
            }]),
            sqlite: None,
            mysql: None,
        }],
    );
    let err = validate_ir(&nested, Dialect::Postgres, &[]).unwrap_err();
    assert_eq!(err.code, CODE_OP_INVALID);
}
