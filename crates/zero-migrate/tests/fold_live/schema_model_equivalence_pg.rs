//! **The neutral model, measured against live PostgreSQL.**
//!
//! `docs/proposals/single-fold-and-effects.md` section G step 2 adds `SchemaModel`,
//! `VendorFacts` and the named comparators, and is required to change NOTHING. This file
//! is the proof, and it runs through the real path on both sides: the schema is built by
//! applying an IR document through `IrAuthor::load_and_lower_guarded` +
//! `MigrationEngine::apply_plan` against a real server, then read back with the shipped
//! `snapshot_schema`. The same ops are folded offline. Both snapshots go through the
//! model.
//!
//! Three claims, all in `support::model_equivalence`:
//!
//! 1. `TableSnapshot -> (Table, VendorFacts) -> TableSnapshot` is the identity, compared
//!    through `Debug` rather than `==` because `==` is the lossy thing under test.
//! 2. Every named comparator answers identically to the hand-written `eq` it was
//!    extracted from, on EVERY ordered pair of columns, indexes, constraints and tables
//!    drawn from both snapshots.
//! 3. `drift_identity` and `column_shape_identity` are genuinely two questions, shown on
//!    a real generated column put through the one mutation the real event applies.
//!
//! ## Why the fixture is what it is
//!
//! Every facet the comparators can see has to be populated by SOMETHING, or the
//! equivalence is vacuous on that facet. The corpus therefore carries a generated STORED
//! column (`generated_kind`, and the one facet that separates drift from shape), a
//! partial index and an expression index (`predicate`, `elements[].Expr`,
//! `expr_cascade_columns`), an INCLUDE index, a DESC index (canonical sort order), a
//! CHECK, a UNIQUE, a PRIMARY KEY and a FOREIGN KEY (`constraints[].definition` per
//! kind), a TypeID column
//! (`value_format`, `inline_checks`), an identity column, and comments on a table, a
//! column and an index.
//!
//! The coverage counters are asserted rather than logged, so a corpus that stops
//! populating a facet fails here instead of quietly measuring less.

use crate::support;

use std::collections::BTreeMap;

use crate::support::model_equivalence::{
    assert_comparators_match_partial_eq, assert_drift_sees_what_shape_does_not,
    assert_each_field_moves_both_verdicts_together, assert_roundtrip_is_lossless,
};
use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    fold_ops, resolve_create_table_policy, snapshot_schema, Approval, EffectivePolicy,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr,
    PostgresBackend, SchemaSnapshot, SqlDialect,
};

const OWNER: &str = "app_schema_model_equivalence_pg";

/// The corpus. One `createTable` plus a child table for the foreign key.
///
/// `id` is a TypeID column so the PostgreSQL fold emits a format CHECK naming its own
/// column, which is what populates `inline_checks` and `value_format`.
const CORPUS_IR: &str = r#"{
  "ir_version": 1,
  "name": "schema_model_equivalence_create",
  "owner_app": "app_schema_model_equivalence_pg",
  "ops": [
    {"op":"createTable","name":"sm_parent","columns":[
      {"name":"id","type":"int","nullable":false}
    ],"primaryKey":["id"]},

    {"op":"createTable","name":"sm_main","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty","type":"int","nullable":false},
      {"name":"note","type":"text","nullable":true},
      {"name":"label","type":"text","nullable":true},
      {"name":"seq_no","type":"bigInt","nullable":false,"identity":{"always":true}},
      {"name":"qty_plus_one","type":"int","nullable":true,
       "generated":{"expr":{"node":"binOp","op":"add",
         "lhs":{"node":"colRef","name":"qty"},
         "rhs":{"node":"literal","value":1}},"stored":true}},
      {"name":"sku_code","type":"text","nullable":true,
       "valueFormat":{"typeId":{"prefix":"sku"}}}
    ],"primaryKey":["id","qty"],"constraints":[
      {"name":"sm_main_uq","kind":{"kind":"unique","columns":["qty","note"]}},
      {"name":"sm_main_ck","kind":{"kind":"check","expr":{
        "node":"binOp","op":"gt",
        "lhs":{"node":"colRef","name":"qty"},
        "rhs":{"node":"literal","value":0}}}}
    ]},

    {"op":"addConstraint","table":"sm_main","constraint":{
      "name":"sm_main_fk","kind":{"kind":"fk",
        "columns":["qty"],
        "referencesTable":"sm_parent","referencesColumns":["id"]}}},

    {"op":"createIndex","table":"sm_main","name":"sm_main_plain",
     "columns":[{"kind":"column","name":"qty"}],"unique":false},
    {"op":"createIndex","table":"sm_main","name":"sm_main_partial",
     "columns":[{"kind":"column","name":"id"}],"unique":false,
     "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"qty"},
              "rhs":{"node":"literal","value":0}}},
    {"op":"createIndex","table":"sm_main","name":"sm_main_expr",
     "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
       "lhs":{"node":"colRef","name":"qty"},"rhs":{"node":"literal","value":1}}}],
     "unique":false},
    {"op":"createIndex","table":"sm_main","name":"sm_main_incl",
     "columns":[{"kind":"column","name":"note"}],"include":["qty"],"unique":false},
    {"op":"createIndex","table":"sm_main","name":"sm_main_desc",
     "columns":[{"kind":"column","name":"label","order":"desc"}],"unique":false},

    {"op":"comment","target":{"kind":"table","name":"sm_main"},
     "comment":"every facet the comparators can see"},
    {"op":"comment","target":{"kind":"column","table":"sm_main","name":"note"},
     "comment":"free-form note"},
    {"op":"comment","target":{"kind":"index","name":"sm_main_partial"},
     "comment":"a partial index"}
  ]
}"#;

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "schema_model_eq_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn fold(
    ir: &str,
    policy: &EffectivePolicy,
    project_schema: &str,
) -> Result<SchemaSnapshot, String> {
    let authored: MigrationIr =
        serde_json::from_str(ir).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, policy, project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    fold_ops(&resolved.ops, SqlDialect::Postgres, project_schema, policy)
        .map_err(|error| format!("fold the PostgreSQL ops: {error}"))
}

async fn apply_through_engine(
    backend: &PostgresBackend<'_, PgDevSession>,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    ir: &str,
) -> Result<(), String> {
    let authored: MigrationIr =
        serde_json::from_str(ir).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, policy, &cfg.project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    let resolved_source = serde_json::to_string(&resolved)
        .map_err(|error| format!("serialize resolved IR: {error}"))?;
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, policy);
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
    let artifact = author
        .load_and_lower_guarded(
            &resolved_source,
            OWNER,
            &BTreeMap::new(),
            &LiveSchema::default(),
            &guard,
        )
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            cfg,
            "schema-model-equivalence-pg",
            LockMode::Acquire,
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("{error}"))
}

struct Measured {
    folded: SchemaSnapshot,
    live: SchemaSnapshot,
}

async fn measure() -> Option<Measured> {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return None;
    };
    let session = PgDevSession::connect(&url);
    let schema = token();
    let policy = support::no_inject(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated schema-model equivalence schema");

    let work: Result<Measured, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let policy = support::no_inject(&cfg.project_schema);
        apply_through_engine(&backend, &cfg, &policy, CORPUS_IR).await?;

        let folded = fold(CORPUS_IR, &policy, &cfg.project_schema)?;
        let live = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;

        Ok(Measured { folded, live })
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(measured), Ok(())) => Some(measured),
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(_), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}
/// All three claims from ONE measurement.
///
/// Deliberately one test rather than three. Each `measure()` builds an isolated schema,
/// applies the corpus through the real engine and introspects it, so three tests meant
/// three concurrent connections and three schemas for one corpus. The claims are
/// independent assertions inside one run, and each names itself in its failure message.
#[compio::test]
async fn the_neutral_model_preserves_postgresql_behaviour_exactly() {
    let Some(measured) = measure().await else {
        return;
    };

    // ---- Claim 1: the neutral/vendor split loses nothing -------------------
    //
    // Both producers are checked, and both are needed: they populate DIFFERENT fields.
    // Only the catalog read fills `expr_cascade_columns` from `pg_depend`; only the fold
    // fills `inline_checks` and `generated`. A split that lost one producer's fields
    // would pass against the other.
    assert_roundtrip_is_lossless("live PostgreSQL", &measured.live.tables);
    assert_roundtrip_is_lossless("folded PostgreSQL", &measured.folded.tables);

    // ---- Claim 2: every named comparator answers what its `eq` answers -----
    let coverage = assert_comparators_match_partial_eq(
        "PostgreSQL",
        &measured.live.tables,
        &measured.folded.tables,
    );

    // Asserted, not logged. A corpus that stops populating a facet must FAIL here rather
    // than quietly measure less, and a run where every pair is unequal proves nothing
    // about the `true` branch of any comparator.
    assert!(
        coverage.column_pairs >= 200,
        "too few column pairs compared to be meaningful: {coverage:?}"
    );
    assert!(
        coverage.column_pairs_equal >= 8,
        "no column pair was found EQUAL, so only the `false` branch of \
         `column_shape_identity` was exercised: {coverage:?}"
    );
    assert!(
        coverage.column_pairs_equal < coverage.column_pairs,
        "every column pair was found equal, so only the `true` branch was exercised: \
         {coverage:?}"
    );
    assert!(
        coverage.index_pairs >= 49,
        "too few index pairs compared: {coverage:?}"
    );
    assert!(
        coverage.index_pairs_paired >= 7 && coverage.index_pairs_paired < coverage.index_pairs,
        "`index_pairing_identity` was not exercised in both directions: {coverage:?}"
    );
    assert!(
        coverage.constraint_pairs >= 25,
        "too few constraint pairs compared: {coverage:?}"
    );
    assert!(
        coverage.table_pairs >= 9,
        "too few table pairs compared: {coverage:?}"
    );

    // ---- Claim 3: drift and shape are two questions ------------------------
    //
    // The measurement that refutes the proposal's premise that `ColumnSnapshot::eq`
    // should be extracted UNDER the name `drift_identity`. `apply/drift.rs` compares
    // `generated_kind` through `comparable_generated_column` and `ColumnSnapshot::eq`
    // does not, so the two already disagree about what "the same column" means.
    // ---- Claim 2b: every TERM of every comparator, one field at a time -----
    //
    // The pairwise sweep above catches a comparator that ADDED a term. It cannot catch
    // one that DROPPED a term, because any two distinct real columns differ in `name`
    // and the first term already decides. Mutating ONE field of a real object is what
    // turns each later term into something a neuter can move.
    let checked =
        assert_each_field_moves_both_verdicts_together("live PostgreSQL", &measured.live.tables);
    assert!(
        checked >= 200,
        "field-mutation sweep was too small: {checked}"
    );

    assert_drift_sees_what_shape_does_not("live PostgreSQL", &measured.live.tables);
}
