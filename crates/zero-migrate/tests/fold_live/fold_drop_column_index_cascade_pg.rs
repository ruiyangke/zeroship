//! Live PostgreSQL oracle for the `Op::DropColumn` index cascade beyond key columns.
//!
//! PostgreSQL drops an index when ANY column it references is dropped, and an
//! `INCLUDE` payload column counts: `INCLUDE` attributes are `indkey` entries past
//! `indnkeyatts`, so `ALTER TABLE ... DROP COLUMN` finds the same dependency it
//! finds for a key column and removes the whole index.
//!
//! Measured on PostgreSQL 18.4:
//!
//! ```text
//! CREATE TABLE t_incl (id int primary key, a int, b int);
//! CREATE INDEX i_incl ON t_incl (b) INCLUDE (a);
//! ALTER TABLE t_incl DROP COLUMN a;
//! -- pg_indexes afterwards holds only t_incl_pkey; i_incl is gone.
//! ```
//!
//! `render/fold.rs` matched the drop against `IndexSnapshot::columns` alone - the
//! KEY column list - so an index that covered the dropped column only through
//! `IndexSnapshot::include` survived the fold and left a PHANTOM INDEX that live
//! introspection does not have, making `fold_ops != snapshot_schema(live)`.
//!
//! The other two column-bearing fields, `IndexSnapshot::predicate` and the
//! `IndexElementSnapshot::Expr` key, cascade in PostgreSQL for the same reason but
//! are RENDERED SQL TEXT in the snapshot rather than structured names. They cascade
//! off `IndexSnapshot::expr_cascade_columns` instead - the column set
//! `create_index_snapshot` collects from the closed `Expr` AST - and the guards
//! below pin why the text itself must never be matched:
//!
//! ```text
//! CREATE INDEX i_lit  ON t_lit  (note) WHERE (note <> 'a');  DROP COLUMN a -> SURVIVES
//! CREATE INDEX i_lit2 ON t_lit2 ((note || 'a'));             DROP COLUMN a -> SURVIVES
//! ```
//!
//! A name-shaped token inside rendered SQL is not a column reference, so a text
//! match would drop indexes PostgreSQL KEPT - worse drift than the phantom it fixes.

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

const OWNER: &str = "app_fold_drop_index_pg";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "fold_drop_index_pg_{}_{}",
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
///
/// Unlike the rename oracle next door, no native SQL is needed: the engine lowers
/// `dropColumn` to PostgreSQL's own `ALTER TABLE ... DROP COLUMN`, so the applied
/// history and the folded history are the same ops.
async fn drift_after_applying(source: &str) -> Option<StructuralDrift> {
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
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated drop-column index-cascade schema");

    let work: Result<StructuralDrift, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let policy = support::no_inject(&cfg.project_schema);
        let authored: MigrationIr =
            serde_json::from_str(source).map_err(|error| format!("parse test IR: {error}"))?;
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
                "fold-drop-index-pg",
                LockMode::Acquire,
            )
            .await
            .map_err(|error| format!("apply IR plan: {error}"))?;

        let expected = fold_ops(
            &resolved.ops,
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

/// The phantom index. The dropped column is NOT a key of the index - it is only an
/// `INCLUDE` payload - so a cascade that reads `IndexSnapshot::columns` alone finds
/// nothing to drop while PostgreSQL has already removed the index.
#[compio::test]
async fn drop_column_cascades_an_index_that_only_includes_it() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_include_index_cascade",
      "owner_app": "app_fold_drop_index_pg",
      "ops": [
        {"op":"createTable","name":"include_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"include_cascade","name":"include_cascade_note_key",
         "columns":[{"kind":"column","name":"note"}],"include":["a"],"unique":false},
        {"op":"dropColumn","table":"include_cascade","column":"a"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL drops an index when its INCLUDE payload column is dropped; a fold \
         that matches only the KEY columns keeps a phantom index: {drift:#?}"
    );
}

/// Control: dropping a column the index does not name at all - neither as a key nor
/// in its `INCLUDE` list - must leave the index standing on both sides. Without this
/// arm the cascade above could pass by dropping every index on the table.
#[compio::test]
async fn drop_of_an_unrelated_column_keeps_an_index_with_an_include_list() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_unrelated_column_keeps_include_index",
      "owner_app": "app_fold_drop_index_pg",
      "ops": [
        {"op":"createTable","name":"include_unrelated","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true},
          {"name":"spare","type":"int","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"include_unrelated","name":"include_unrelated_note_key",
         "columns":[{"kind":"column","name":"note"}],"include":["a"],"unique":false},
        {"op":"dropColumn","table":"include_unrelated","column":"spare"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "an index that names neither the dropped column as a key nor in its INCLUDE \
         list survives in PostgreSQL and must survive the fold: {drift:#?}"
    );
}

/// A partial index whose `WHERE` reads the dropped column. PostgreSQL records the
/// predicate as `pg_index.indpred`, a parse tree over the same attributes, so
/// `DROP COLUMN` finds the dependency and removes the whole index. The predicate is
/// rendered SQL TEXT in the snapshot, so the cascade has to read the structural
/// column set the snapshot recorded instead.
#[compio::test]
async fn drop_column_cascades_a_partial_index_whose_predicate_reads_it() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_predicate_index_cascade",
      "owner_app": "app_fold_drop_index_pg",
      "ops": [
        {"op":"createTable","name":"pred_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"pred_cascade","name":"pred_cascade_note_key",
         "columns":[{"kind":"column","name":"note"}],"unique":false,
         "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"a"},
                  "rhs":{"node":"literal","value":0}}},
        {"op":"dropColumn","table":"pred_cascade","column":"a"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL drops a partial index when its WHERE predicate reads the dropped \
         column; a fold that matches only the structured name lists keeps a phantom \
         index: {drift:#?}"
    );
}

/// An index whose KEY is an expression over the dropped column. The expression is
/// `pg_index.indexprs`, another parse tree over the table's attributes, so the drop
/// cascades exactly as a plain key column would.
#[compio::test]
async fn drop_column_cascades_an_index_keyed_on_an_expression_over_it() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_expr_index_cascade",
      "owner_app": "app_fold_drop_index_pg",
      "ops": [
        {"op":"createTable","name":"expr_cascade","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"expr_cascade","name":"expr_cascade_key",
         "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
           "lhs":{"node":"colRef","name":"a"},"rhs":{"node":"literal","value":1}}}],
         "unique":false},
        {"op":"dropColumn","table":"expr_cascade","column":"a"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL drops an index whose expression key reads the dropped column; a \
         fold that matches only the structured name lists keeps a phantom index: \
         {drift:#?}"
    );
}

/// The false-positive guard for the predicate. The dropped column is `a`; the
/// predicate contains the STRING LITERAL `'a'` and reads no column but `note`.
/// Measured on PostgreSQL 18.4, `DROP COLUMN a` leaves the index standing - so a
/// cascade that greps the rendered text would delete an index the database KEPT.
///
/// Asserts full drift cleanliness. It used to assert index SURVIVAL alone, because
/// PostgreSQL deparses a bare string literal with its inferred cast (`'a'` comes back
/// as `'a'::text`) and the differ byte-compared that against the offline rendering, so
/// every partial index carrying a literal reported drift with no `dropColumn` in the
/// history at all. The differ no longer compares two present predicate BODIES
/// (`apply::drift::index_expression_bodies_are_comparable`) and compares the
/// referenced-column set from `pg_depend` instead, so the unrelated divergence is gone
/// and this arm can demand what it always meant to.
#[compio::test]
async fn drop_column_keeps_a_partial_index_whose_predicate_only_spells_it_in_a_literal() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_predicate_literal_guard",
      "owner_app": "app_fold_drop_index_pg",
      "ops": [
        {"op":"createTable","name":"pred_literal","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"pred_literal","name":"pred_literal_note_key",
         "columns":[{"kind":"column","name":"note"}],"unique":false,
         "where":{"node":"binOp","op":"ne","lhs":{"node":"colRef","name":"note"},
                  "rhs":{"node":"literal","value":"a"}}},
        {"op":"dropColumn","table":"pred_literal","column":"a"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a predicate that merely SPELLS the dropped column inside a string literal \
         references no such column; PostgreSQL keeps the index and so must the fold: \
         {drift:#?}"
    );
}

/// The same guard for the expression key: `(note || 'a')` reads `note` only. The
/// index survives `DROP COLUMN a` in PostgreSQL and must survive the fold. Asserts
/// full drift cleanliness for the same reason as the predicate guard above: the
/// differ no longer compares two present expression-key BODIES, so the `'a'::text`
/// deparse divergence that forced a survival-only assertion is gone.
#[compio::test]
async fn drop_column_keeps_an_expression_index_whose_literal_spells_it() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_expr_literal_guard",
      "owner_app": "app_fold_drop_index_pg",
      "ops": [
        {"op":"createTable","name":"expr_literal","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"expr_literal","name":"expr_literal_key",
         "columns":[{"kind":"expr","expr":{"node":"binOp","op":"concat",
           "lhs":{"node":"colRef","name":"note"},
           "rhs":{"node":"literal","value":"a"}}}],
         "unique":false},
        {"op":"dropColumn","table":"expr_literal","column":"a"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "an expression key that merely SPELLS the dropped column inside a string \
         literal references no such column; PostgreSQL keeps the index and so must \
         the fold: {drift:#?}"
    );
}

/// The dialect-leg guard. The predicate is a `dialect()` node whose PostgreSQL leg
/// reads `a` and whose SQLite leg reads `b`; only the PostgreSQL leg is rendered
/// into the database, so `DROP COLUMN b` leaves the index standing. The provenance
/// walk has to descend the SELECTED leg only - unioning every leg reintroduces the
/// false positive `gen_types_drop_column_dialect_legs.rs` pins offline.
#[compio::test]
async fn drop_column_keeps_a_partial_index_whose_predicate_leg_never_reads_it() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_predicate_leg_guard",
      "owner_app": "app_fold_drop_index_pg",
      "ops": [
        {"op":"createTable","name":"pred_legs","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"b","type":"int","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"pred_legs","name":"pred_legs_id_key",
         "columns":[{"kind":"column","name":"id"}],"unique":false,
         "where":{"node":"dialect",
           "pg":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"a"},
                 "rhs":{"node":"literal","value":0}},
           "sqlite":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"b"},
                     "rhs":{"node":"literal","value":0}}}},
        {"op":"dropColumn","table":"pred_legs","column":"b"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "only the PostgreSQL leg of a dialectal predicate reaches the database, so a \
         column named solely by the inactive leg never cascades; the fold must read \
         the selected leg alone: {drift:#?}"
    );
}
