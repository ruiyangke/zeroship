use crate::support;

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::model::ir::{IndexElement, IndexMethod, IrFlagsOverride, Op};
use zero_migrate::model::validate::{validate_ir, CODE_OP_INVALID};
use zero_migrate::{
    effective_policy_from_charter_toml, resolve_create_table_policy, Approval, EffectivePolicy,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, MigrationEngine, MigrationIr, PlanStep,
    CURRENT_IR_VERSION,
};
use zero_migrate_postgres::DIALECT as POSTGRES;
use zero_migrate_sqlite::SqliteBackend;

const PROJECT: &str = "prj_dialectal";
const APP: &str = "app_dialectal";

fn ir(name: &str, ops: Vec<Op>) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
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
            legs: BTreeMap::from([(POSTGRES, vec![hnsw_index_op()])]),
        }],
    )
}

#[test]
fn lower_selects_postgres_leg_and_emits_nothing_for_absent_sqlite_mysql_legs() {
    let pg_steps = IrAuthor::new(
        zero_migrate::shipping_vendors(),
        PROJECT,
        APP,
        &zero_migrate_postgres::DIALECT,
        &support::no_inject("app"),
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

    // An absent op leg CONTRIBUTES NOTHING rather than refusing. Refusing would
    // make shipping a fourth backend retroactively break every migration authored
    // before that backend existed, and the legs record into checksummed history
    // that cannot be edited forward without tripping ChecksumDrift.
    //
    // Asserting the step list is EXACTLY EMPTY, not merely "no HNSW step": the
    // whole claim is that the op vanished, and a length check is what catches a
    // future change that emits some other step in its place.
    for dialect in [&zero_migrate_sqlite::DIALECT, &zero_migrate_mysql::DIALECT] {
        let steps = IrAuthor::new(
            zero_migrate::shipping_vendors(),
            PROJECT,
            APP,
            dialect,
            &support::no_inject("app"),
        )
        .lower_steps(&pg_only_ir(), &LiveSchema::default())
        .expect("an absent dialectal leg contributes nothing, it does not refuse");
        assert!(
            steps.is_empty(),
            "{dialect:?} has no leg in this op, so it must emit no steps: {steps:#?}"
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
    let resolved = resolve_create_table_policy(&ir, &support::confined_charter(), PROJECT)
        .expect("test IR resolves");
    serde_json::to_string(&resolved).expect("resolved test IR serializes")
}

/// The confined charter re-scoped onto `schema`, so the SAME policy both shapes the
/// table (its `[[inject]]`) and guards the rendered SQL (its `schema.*` grants).
fn inject_charter(schema: &str) -> EffectivePolicy {
    let charter_toml = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "schema.create_table"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "schema.rename"
value = true
scope = {{ include = [{schema:?}] }}

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  {{ name = "id",         type = "text",        nullable = false }},
  {{ name = "created_at", type = "timestamptz", nullable = false }},
  {{ name = "deleted_at", type = "timestamptz", nullable = true  }},
]
"#
    );
    effective_policy_from_charter_toml(&charter_toml).expect("inject test charter composes")
}

#[test]
fn authored_create_table_lowers_under_the_charter_that_shaped_it() {
    // The engine hands the guard the same policy it hands the shape resolver. The
    // resolver renders the injected columns into the CREATE TABLE text, so the guard
    // must admit that text rather than refuse every create in an inject scope.
    let policy = inject_charter(PROJECT);
    let authored: MigrationIr = serde_json::from_str(
        r#"{"ir_version":1,"name":"authored_inject_create","ops":[
          {"op":"createTable","name":"notes","columns":[
            {"name":"title","type":"text","nullable":false}
          ]}
        ]}"#,
    )
    .expect("authored IR parses");
    let resolved =
        resolve_create_table_policy(&authored, &policy, PROJECT).expect("table shape resolves");
    let author = IrAuthor::new(
        zero_migrate::shipping_vendors(),
        PROJECT,
        APP,
        &zero_migrate_postgres::DIALECT,
        &policy,
    );
    let guard_cfg = GuardConfig::from_policy(policy, zero_migrate_postgres::DIALECT);
    let (steps, _fragments) = author
        .lower_guarded(&resolved, &guard_cfg, &LiveSchema::default())
        .expect("an authored createTable lowers under the charter that shaped it");
    assert!(
        !steps.is_empty(),
        "an authored createTable should lower to at least one step"
    );
}

#[compio::test]
async fn sqlite_apply_selects_explicit_empty_leg_without_column_effect() {
    let p = paths("sqlite_skip");
    let be = backend(&p);
    let ir = resolved_envelope_json(
        r#"{"ir_version":1,"name":"sqlite_skip_postgres_leg","ops":[
          {"op":"createTable","name":"docs","columns":[{"name":"title","type":"text"}]},
          {"op":"dialectal","legs":{"postgres":[
            {"op":"addColumn","table":"docs","column":"pg_only","type":"text"}
          ],"sqlite":[]}}
        ]}"#,
    );

    let author = IrAuthor::new(
        zero_migrate::shipping_vendors(),
        PROJECT,
        APP,
        &zero_migrate_sqlite::DIALECT,
        &support::confined_charter(),
    );
    let migrations = author
        .load_and_lower(&ir, APP, &registry(&[]), &LiveSchema::default())
        .expect("SQLite should lower createTable and select its explicit empty leg");
    assert_eq!(
        migrations.len(),
        1,
        "SQLite lower should emit only createTable; its explicit dialectal leg is empty"
    );

    let engine = MigrationEngine::new(zero_migrate::shipping_vendors());
    let guard_cfg =
        GuardConfig::from_policy(support::no_inject(PROJECT), zero_migrate_sqlite::DIALECT);
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
            &ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT)),
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
        "SQLite must not apply the PostgreSQL-only column op"
    );
}

#[test]
fn validate_rejects_empty_and_nested_dialectal_ops() {
    let empty = ir(
        "empty",
        vec![Op::Dialectal {
            legs: BTreeMap::new(),
        }],
    );
    let err = validate_ir(
        zero_migrate::shipping_vendors(),
        &empty,
        &zero_migrate_postgres::DIALECT,
    )
    .unwrap_err();
    assert_eq!(err.code, CODE_OP_INVALID);

    let nested = ir(
        "nested",
        vec![Op::Dialectal {
            legs: BTreeMap::from([(
                POSTGRES,
                vec![Op::Dialectal {
                    legs: BTreeMap::from([(POSTGRES, Vec::new())]),
                }],
            )]),
        }],
    );
    let err = validate_ir(
        zero_migrate::shipping_vendors(),
        &nested,
        &zero_migrate_postgres::DIALECT,
    )
    .unwrap_err();
    assert_eq!(err.code, CODE_OP_INVALID);
}

#[test]
fn validate_accepts_absent_and_misspelled_target_dialectal_legs() {
    // Validate must agree with the fold: an absent target leg contributes nothing,
    // so there is nothing to refuse. If validate refused while the fold skipped,
    // every migration would be rejected for ops the target was never going to run.
    let absent = pg_only_ir();
    validate_ir(
        zero_migrate::shipping_vendors(),
        &absent,
        &zero_migrate_sqlite::DIALECT,
    )
    .expect("an absent exact target leg contributes nothing, it does not refuse");

    // A misspelled key is INDISTINGUISHABLE from a deliberate skip. That is the
    // accepted cost of emit-nothing: refusing an unrecognised key would also refuse
    // every already-checksummed migration the moment a new backend ships.
    let misspelled = ir(
        "misspelled_postgres",
        vec![Op::Dialectal {
            legs: BTreeMap::from([(
                zero_migrate_ir::dialect::DialectId::new("postgre"),
                vec![hnsw_index_op()],
            )]),
        }],
    );
    validate_ir(
        zero_migrate::shipping_vendors(),
        &misspelled,
        &zero_migrate_postgres::DIALECT,
    )
    .expect("a misspelled key leaves postgres uncovered, which emits nothing");
}
