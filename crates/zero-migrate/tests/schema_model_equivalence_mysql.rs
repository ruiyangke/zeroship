//! **The neutral model, measured against live MySQL.**
//!
//! The MySQL leg of `schema_model_equivalence_pg.rs`, and it is not a duplicate: it is
//! the ONLY leg that exercises the vendor side table with real values.
//!
//! On PostgreSQL every family in `VendorFacts` is at its default - `sqlite_rowid` false,
//! `catalog_uuid_format_check` false, `pg_index_only` false (PostgreSQL introspection
//! hardcodes it), `mysql_*` absent - so a round trip that silently dropped a vendor
//! family would still pass there. MySQL populates `mysql_physical_type`,
//! `mysql_text_storage` and `mysql_default_generated` from
//! `information_schema.COLUMNS` on every column, so the lossless claim is only actually
//! TESTED here.
//!
//! It also exercises the one column family that participates in a comparator:
//! `mysql_physical_type` is compared by `VendorFacts::column_drift_identity` and NOT by
//! `column_shape_identity`, mirroring `apply::drift::column_data_types_eq` reading it
//! while `ColumnSnapshot::eq` does not.
//!
//! Same harness as `fold_roundtrip_mysql.rs`: apply through
//! `IrAuthor::load_and_lower_guarded` + `MigrationEngine::apply_plan` over
//! `MysqlBackend`, introspect with the shipped `snapshot_schema`, fold the same resolved
//! ops offline. A MySQL "schema" is a DATABASE, so the isolation unit is a database and
//! [`support::mysql::DatabaseGuard`] guards it.

mod support;

use std::collections::BTreeMap;

use support::model_equivalence::{
    assert_comparators_match_partial_eq, assert_each_field_moves_both_verdicts_together,
    assert_roundtrip_is_lossless,
};
use support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::apply::backend::{MigrationBackend, MysqlBackend};
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    fold_ops, resolve_create_table_policy, Approval, ExecutorConfig, GuardConfig, IrAuthor,
    LiveSchema, LockMode, MigrationEngine, MigrationIr, SchemaSnapshot, SqlDialect,
};

const OWNER: &str = "app_schema_model_equivalence_mysql";

/// The corpus. The TYPE SPREAD is the point: `mysql_physical_type` parses
/// `information_schema.COLUMNS.COLUMN_TYPE` into a family-gated value, so a corpus of
/// one family would leave most of the parse unexercised and the round trip would only
/// prove that one arm survives.
///
/// `body` is a bare `text` and is deliberately never keyed - a key over a bare TEXT
/// column is MySQL error 1170, the trap `fold_roundtrip_mysql.rs` records.
const CORPUS_IR: &str = r#"{
  "ir_version": 1,
  "name": "schema_model_equivalence_mysql_create",
  "owner_app": "app_schema_model_equivalence_mysql",
  "ops": [
    {"op":"createTable","name":"sm_mysql_main","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"title","type":{"string":{"length":200}},"nullable":false},
      {"name":"body","type":"text","nullable":true},
      {"name":"email","type":"text","nullable":true,"caseSensitive":false},
      {"name":"amount","type":{"decimal":{"precision":12,"scale":2}},"nullable":true},
      {"name":"score","type":"double","nullable":true},
      {"name":"done","type":"boolean","nullable":false},
      {"name":"sku_code","type":"text","nullable":true,
       "valueFormat":{"typeId":{"prefix":"sku"}}}
    ],"primaryKey":["id"],"indexes":[
      {"name":"sm_mysql_main_title_idx","columns":[{"kind":"column","name":"title"}]}
    ]}
  ]
}"#;

fn cfg_for(database: &str) -> ExecutorConfig {
    ExecutorConfig::new(
        format!("project_{database}"),
        database,
        support::no_inject(database),
    )
}

struct Measured {
    folded: SchemaSnapshot,
    live: SchemaSnapshot,
}

async fn measure(session: &MysqlDevSession, cfg: &ExecutorConfig) -> Result<Measured, String> {
    let policy = support::no_inject(&cfg.project_schema);
    let authored: MigrationIr =
        serde_json::from_str(CORPUS_IR).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    let resolved_source = serde_json::to_string(&resolved)
        .map_err(|error| format!("serialize resolved IR: {error}"))?;
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Mysql, &policy);
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Mysql);
    let artifact = author
        .load_and_lower_guarded(
            &resolved_source,
            OWNER,
            &BTreeMap::new(),
            &LiveSchema::default(),
            &guard,
        )
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;

    let backend = MysqlBackend::new_generic(session);
    backend
        .ensure_journal(cfg)
        .await
        .map_err(|error| format!("ensure migration journal: {error}"))?;
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &backend,
            cfg,
            "schema-model-equivalence-mysql",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply IR plan: {error}"))?;

    let folded = fold_ops(
        &resolved.ops,
        SqlDialect::Mysql,
        &cfg.project_schema,
        &policy,
    )
    .map_err(|error| format!("fold the MySQL ops: {error}"))?;
    let live = backend
        .snapshot_schema(cfg)
        .await
        .map_err(|error| format!("snapshot the live MySQL schema: {error}"))?;

    Ok(Measured { folded, live })
}

/// The three claims from ONE measurement, plus the vendor-population assertion that
/// makes this leg different from the PostgreSQL one.
#[compio::test]
async fn the_neutral_model_preserves_mysql_behaviour_exactly() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("smeq");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated schema-model equivalence database");

    let measured = measure(&session, &cfg).await.expect("measure live MySQL");

    // The reason this leg exists. If MySQL introspection stopped populating the physical
    // type, every assertion below would still pass while proving nothing about the
    // vendor side table, so the precondition is asserted rather than assumed.
    let vendor = zero_migrate::SchemaModel::from_tables(&measured.live.tables).vendor;
    assert!(
        !vendor.mysql_physical_type.is_empty(),
        "the live MySQL snapshot populated NO `mysql_physical_type`, so this leg is not \
         exercising the vendor side table at all"
    );
    assert!(
        !vendor.mysql_text_storage.is_empty(),
        "the live MySQL snapshot populated NO `mysql_text_storage`"
    );

    // ---- Claim 1: the split is lossless, with vendor facts actually present ----
    assert_roundtrip_is_lossless("live MySQL", &measured.live.tables);
    assert_roundtrip_is_lossless("folded MySQL", &measured.folded.tables);

    // ---- Claim 2: comparators answer what their `eq` answers -----------------
    let coverage = assert_comparators_match_partial_eq(
        "MySQL",
        &measured.live.tables,
        &measured.folded.tables,
    );
    assert!(
        coverage.column_pairs >= 200,
        "too few column pairs compared: {coverage:?}"
    );
    assert!(
        coverage.column_pairs_equal >= 4 && coverage.column_pairs_equal < coverage.column_pairs,
        "`column_shape_identity` was not exercised in both directions: {coverage:?}"
    );

    // ---- Claim 2b: every TERM, one field at a time --------------------------
    //
    // This is where `mysql_physical_type` earns its place. Mutating it must move
    // NEITHER verdict, because `ColumnSnapshot::eq` excludes it and
    // `column_shape_identity` cannot see it - and mutating `sqlite_rowid` must move
    // BOTH, because `ColumnSnapshot::eq` compares it and
    // `VendorFacts::column_shape_identity` is the half that recombines it.
    let checked =
        assert_each_field_moves_both_verdicts_together("live MySQL", &measured.live.tables);
    assert!(
        checked >= 150,
        "field-mutation sweep was too small: {checked}"
    );
}
