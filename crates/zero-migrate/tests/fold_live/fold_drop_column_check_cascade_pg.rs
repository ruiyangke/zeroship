//! Live PostgreSQL oracle for the `Op::DropColumn` CHECK cascade.
//!
//! PostgreSQL drops a CHECK constraint whenever any column its expression
//! references is dropped. `render/fold.rs` used to claim (at the `DropColumn`
//! cascade and at `constraint_local_columns_contain`) that "CHECK is never
//! folded". It is: both `fold_create_table_specs` and the `Op::AddConstraint` arm
//! push a `ConstraintSnapshot { kind: "CHECK", definition: "CHECK (<rendered>)" }`.
//! `constraint_local_columns_contain` parses the FIRST parenthesized group of that
//! definition as a comma-separated column list, and for a CHECK that group is the
//! EXPRESSION - so a single-column CHECK never matched its own column and the fold
//! kept a constraint PostgreSQL had already cascaded away.
//!
//! Measured on PostgreSQL 18.4: `ALTER TABLE ... DROP COLUMN qty` removes
//! `CHECK ((qty >= 0))` from `pg_constraint`.
//!
//! The fix records the referenced columns STRUCTURALLY on
//! `ConstraintSnapshot::cascade_columns` - from `conkey` on the live side, from the
//! closed AST on the fold side - so neither half has to read them back out of
//! rendered SQL text. These tests pin both directions of that predicate against a
//! real database: the constraint must go when PostgreSQL drops it, and must STAY
//! when PostgreSQL keeps it (a literal that merely spells a column name, and a
//! whole-row predicate that references nothing).

use crate::support;

use std::collections::BTreeMap;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, fold_ops, resolve_create_table_policy, snapshot_schema, Approval,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr,
    PostgresBackend, SqlDialect, StructuralDrift,
};

const OWNER: &str = "app_fold_check_cascade_pg";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "fold_check_cascade_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Apply every op of `source` through the real engine, fold the SAME resolved ops
/// offline, and return the drift between the fold and live introspection. `None`
/// when no live database is configured (the caller then skips its assertion).
async fn drift_after_applying(source: &str) -> Option<StructuralDrift> {
    drift_between_fold_and_live(source, &[], source).await
}

/// The general oracle: apply `applied` through the real engine, then run
/// `native_sql` (each statement with `{schema}` replaced by the quoted test schema)
/// on the same real session, then fold `folded` offline and diff it against live
/// introspection.
///
/// `native_sql` exists for ONE op: PostgreSQL `renameColumn`. The engine lowers it
/// to an expand-contract plan that ADDS the new column and leaves the old one live
/// until a separate later contract deploy, while the fold collapses the rename to
/// its final name - a divergence `render/fold.rs` documents and deliberately
/// excludes from the fold==live oracle. Driving the rename with PostgreSQL's own
/// `ALTER TABLE ... RENAME COLUMN` is what makes the comparison meaningful: it is
/// exactly the catalog transition the fold models, and it is the transition under
/// test here (PostgreSQL renames the attribute in place and every constraint's
/// `conkey` keeps pointing at it, so a later drop still cascades).
async fn drift_between_fold_and_live(
    applied: &str,
    native_sql: &[&str],
    folded: &str,
) -> Option<StructuralDrift> {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return None;
    };
    let session = PgDevSession::connect(&url);
    let schema = token();
    let policy = support::no_inject(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.confinement.meta_schema);
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated check-cascade schema");

    let work: Result<StructuralDrift, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let policy = support::no_inject(&cfg.project_schema);
        let authored: MigrationIr =
            serde_json::from_str(applied).map_err(|error| format!("parse test IR: {error}"))?;
        let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
            .map_err(|error| format!("resolve create-table policy: {error}"))?;
        let resolved_source = serde_json::to_string(&resolved)
            .map_err(|error| format!("serialize resolved test IR: {error}"))?;
        let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, &policy);
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
                &backend,
                &cfg,
                "fold-check-cascade-pg",
                LockMode::Acquire,
            )
            .await
            .map_err(|error| format!("apply IR plan: {error}"))?;

        for statement in native_sql {
            let statement = statement.replace("{schema}", &quoted_schema);
            session
                .batch(&statement)
                .await
                .map_err(|error| format!("run native SQL `{statement}`: {error}"))?;
        }

        let folded_authored: MigrationIr = serde_json::from_str(folded)
            .map_err(|error| format!("parse folded test IR: {error}"))?;
        let folded_resolved =
            resolve_create_table_policy(&folded_authored, &policy, &cfg.project_schema)
                .map_err(|error| format!("resolve folded create-table policy: {error}"))?;
        let expected = fold_ops(
            &folded_resolved.ops,
            SqlDialect::Postgres,
            &cfg.project_schema,
            &policy,
        )
        .map_err(|error| format!("fold the applied PostgreSQL ops: {error}"))?;
        let actual = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;
        Ok(diff_snapshots(&expected, &actual))
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(drift), Ok(())) => Some(drift),
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(_), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}

/// A table-level CHECK on a column that is later dropped.
#[compio::test]
async fn drop_column_cascades_a_table_level_check_the_way_postgresql_does() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_check_cascade",
      "owner_app": "app_fold_check_cascade_pg",
      "ops": [
        {"op":"createTable","name":"check_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"qty","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"],
         "constraints":[
           {"name":"check_cascade_qty_range_check","kind":{"kind":"check","expr":{
             "node":"binOp","op":"ge",
             "lhs":{"node":"colRef","name":"qty"},
             "rhs":{"node":"literal","value":0}}}}
         ]},
        {"op":"dropColumn","table":"check_cascade","column":"qty"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL drops a CHECK when the column it constrains is dropped; the fold \
         must mirror that cascade: {drift:#?}"
    );
}

/// The same divergence via a stand-alone `addConstraint`.
#[compio::test]
async fn drop_column_cascades_a_standalone_check_the_way_postgresql_does() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_standalone_check_cascade",
      "owner_app": "app_fold_check_cascade_pg",
      "ops": [
        {"op":"createTable","name":"standalone_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"qty","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"standalone_cascade","constraint":{
          "name":"standalone_cascade_qty_check",
          "kind":{"kind":"check","expr":{
            "node":"binOp","op":"gt",
            "lhs":{"node":"colRef","name":"qty"},
            "rhs":{"node":"literal","value":0}}}}},
        {"op":"dropColumn","table":"standalone_cascade","column":"qty"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL drops a stand-alone CHECK when the column it constrains is dropped; \
         the fold must mirror that cascade: {drift:#?}"
    );
}

/// Control: dropping an UNRELATED column must NOT cascade the CHECK. This one is
/// expected to pass on current code and pins that the reproduction above is about
/// the cascade, not about CHECK folding in general.
#[compio::test]
async fn drop_of_an_unrelated_column_keeps_the_check() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_unrelated_column_keeps_check",
      "owner_app": "app_fold_check_cascade_pg",
      "ops": [
        {"op":"createTable","name":"unrelated_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"qty","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"],
         "constraints":[
           {"name":"unrelated_cascade_qty_range_check","kind":{"kind":"check","expr":{
             "node":"binOp","op":"ge",
             "lhs":{"node":"colRef","name":"qty"},
             "rhs":{"node":"literal","value":0}}}}
         ]},
        {"op":"dropColumn","table":"unrelated_cascade","column":"note"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "dropping an unrelated column must leave the CHECK in place on both sides: \
         {drift:#?}"
    );
}

/// Rename the constrained column, then drop it under its NEW name. PostgreSQL
/// renames the attribute in place, so `conkey` still points at it and the drop
/// still cascades the CHECK. The fold has to follow the rename through
/// `cascade_columns`; if it kept naming the OLD column, the drop would find no
/// match and leave a phantom CHECK behind.
///
/// The rename runs as native `ALTER TABLE ... RENAME COLUMN` rather than through
/// the engine's `renameColumn` op - see `drift_between_fold_and_live` for why.
#[compio::test]
async fn drop_of_a_renamed_column_cascades_the_check() {
    let applied = r#"{
      "ir_version": 1,
      "name": "rename_then_drop_check_cascade_create",
      "owner_app": "app_fold_check_cascade_pg",
      "ops": [
        {"op":"createTable","name":"renamed_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"qty","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"],
         "constraints":[
           {"name":"renamed_cascade_qty_range_check","kind":{"kind":"check","expr":{
             "node":"binOp","op":"ge",
             "lhs":{"node":"colRef","name":"qty"},
             "rhs":{"node":"literal","value":0}}}}
         ]}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_then_drop_check_cascade",
      "owner_app": "app_fold_check_cascade_pg",
      "ops": [
        {"op":"createTable","name":"renamed_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"qty","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"],
         "constraints":[
           {"name":"renamed_cascade_qty_range_check","kind":{"kind":"check","expr":{
             "node":"binOp","op":"ge",
             "lhs":{"node":"colRef","name":"qty"},
             "rhs":{"node":"literal","value":0}}}}
         ]},
        {"op":"renameColumn","table":"renamed_cascade","from":"qty","to":"amount",
         "type":"int"},
        {"op":"dropColumn","table":"renamed_cascade","column":"amount"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &[
            "ALTER TABLE {schema}.\"renamed_cascade\" RENAME COLUMN \"qty\" TO \"amount\"",
            "ALTER TABLE {schema}.\"renamed_cascade\" DROP COLUMN \"amount\"",
        ],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a rename must carry the CHECK's recorded cascade columns with it, so dropping \
         the renamed column still cascades exactly as PostgreSQL does: {drift:#?}"
    );
}

/// A CHECK spanning TWO columns. PostgreSQL drops it when EITHER is dropped, so a
/// partial match has to cascade the whole constraint.
#[compio::test]
async fn drop_of_one_column_cascades_a_two_column_check() {
    let source = r#"{
      "ir_version": 1,
      "name": "multi_column_check_cascade",
      "owner_app": "app_fold_check_cascade_pg",
      "ops": [
        {"op":"createTable","name":"multi_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"b","type":"int","nullable":true}
        ],"primaryKey":["id"],
         "constraints":[
           {"name":"multi_cascade_a_b_check","kind":{"kind":"check","expr":{
             "node":"binOp","op":"lt",
             "lhs":{"node":"colRef","name":"a"},
             "rhs":{"node":"colRef","name":"b"}}}}
         ]},
        {"op":"dropColumn","table":"multi_cascade","column":"a"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL drops a two-column CHECK when EITHER column goes; the fold must \
         cascade on a partial match too: {drift:#?}"
    );
}

/// The false-positive guard, and the reason the cascade set is collected from the
/// closed AST instead of parsed out of the rendered SQL.
///
/// `CHECK (status <> 'qty')` mentions the token `qty` only as a string LITERAL.
/// Measured on PostgreSQL 18.4: the constraint SURVIVES `DROP COLUMN qty` (its
/// `conkey` holds `status` alone). A text-matching cascade would drop a constraint
/// PostgreSQL kept - a brand-new phantom in the opposite direction.
#[compio::test]
async fn a_literal_that_spells_a_column_name_does_not_cascade() {
    let source = r#"{
      "ir_version": 1,
      "name": "literal_collision_check_cascade",
      "owner_app": "app_fold_check_cascade_pg",
      "ops": [
        {"op":"createTable","name":"literal_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"status","type":"text","nullable":true},
          {"name":"qty","type":"int","nullable":true}
        ],"primaryKey":["id"],
         "constraints":[
           {"name":"literal_cascade_status_check","kind":{"kind":"check","expr":{
             "node":"binOp","op":"ne",
             "lhs":{"node":"colRef","name":"status"},
             "rhs":{"node":"literal","value":"qty"}}}}
         ]},
        {"op":"dropColumn","table":"literal_cascade","column":"qty"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a column name appearing only as a string literal is not a reference; \
         PostgreSQL keeps this CHECK and so must the fold: {drift:#?}"
    );
}

/// A CHECK that references NO column at all. Its `conkey` is NULL, so PostgreSQL
/// never cascades it - the fold records an EMPTY cascade set, which must be read as
/// "references nothing" and not as "provenance unknown, fall back to the text".
#[compio::test]
async fn a_check_referencing_no_column_survives_every_drop() {
    let source = r#"{
      "ir_version": 1,
      "name": "whole_row_check_cascade",
      "owner_app": "app_fold_check_cascade_pg",
      "ops": [
        {"op":"createTable","name":"whole_row_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"qty","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"],
         "constraints":[
           {"name":"whole_row_cascade_always_check","kind":{"kind":"check","expr":{
             "node":"literal","value":true}}}
         ]},
        {"op":"dropColumn","table":"whole_row_cascade","column":"qty"},
        {"op":"dropColumn","table":"whole_row_cascade","column":"note"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a CHECK with no column reference has a NULL conkey and never cascades; the \
         fold's empty cascade set must mean the same: {drift:#?}"
    );
}
