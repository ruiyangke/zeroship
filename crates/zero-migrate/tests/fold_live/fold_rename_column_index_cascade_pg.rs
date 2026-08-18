//! Live PostgreSQL oracle for the `Op::RenameColumn` index cascade.
//!
//! PostgreSQL renames a column ATTRIBUTE in place. `pg_index` references the
//! attribute by `attnum`, never by name, so every index over the renamed column
//! follows it automatically: `pg_get_indexdef` spells the NEW name the instant the
//! rename commits, and a later `DROP COLUMN` of the new name cascades the index
//! away exactly as it would have under the old one.
//!
//! `render/fold.rs` used to update only `snap.columns` and
//! `ConstraintSnapshot::cascade_columns` in its `Op::RenameColumn` arm and never
//! touched `snap.indexes`, so the fold kept naming the OLD column in
//! `IndexSnapshot::columns` / `IndexSnapshot::elements`. That produced drift twice
//! over: the surviving index disagreed with live on its key columns, and a
//! subsequent `dropColumn` of the NEW name found no index to cascade and left a
//! PHANTOM INDEX behind that PostgreSQL had already removed.
//!
//! Measured on PostgreSQL 18.4, `ALTER TABLE ... RENAME COLUMN a TO b`:
//! `CREATE INDEX rename_idx_a ON t (a)` becomes
//! `CREATE INDEX rename_idx_a ON t USING btree (b)` - the KEY follows the rename
//! and the index NAME does NOT. The fold has to do both: rewrite the key columns
//! and leave the name alone.
//!
//! The same holds for the two column-bearing sites that are RENDERED SQL TEXT - a
//! partial index's `WHERE` and an expression key - which follow the rename in
//! `pg_index.indpred` / `pg_index.indexprs` and cascade on a later drop. The fold
//! carries those through `IndexSnapshot::expr_cascade_columns`, the structural
//! column set collected from the closed `Expr` at snapshot time, so
//! `rename a -> b; drop b` cascades the index instead of leaving a phantom.
//!
//! What those arms do NOT pin, because a column list cannot fix it: the rendered
//! TEXT still names the old column after a pure rename. Measured on PostgreSQL 18.4,
//! `rename a -> b` leaves the fold holding `("a" > 0)` / `expr:("a" + 1)` against
//! live's `(b > 0)` / `expr:(b + 1)`. Re-rendering needs the `Expr` the snapshot
//! discarded; see the `Op::RenameColumn` arm of `render/fold.rs`.
//!
//! The renames here run as native `ALTER TABLE ... RENAME COLUMN` rather than
//! through the engine's `renameColumn` op - see `drift_between_fold_and_live` for
//! why.

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

const OWNER: &str = "app_fold_rename_index_pg";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "fold_rename_index_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Apply `applied` through the real engine, then run `native_sql` (each statement
/// with `{schema}` replaced by the quoted test schema) on the same real session,
/// then fold `folded` offline and diff it against live introspection. `None` when
/// no live database is configured (the caller then skips its assertion).
///
/// `native_sql` exists for ONE op: PostgreSQL `renameColumn`. The engine lowers it
/// to an expand-contract plan that ADDS the new column and leaves the old one live
/// until a separate later contract deploy, while the fold collapses the rename to
/// its final name - a divergence `render/fold.rs` documents and deliberately
/// excludes from the fold==live oracle. Driving the rename with PostgreSQL's own
/// `ALTER TABLE ... RENAME COLUMN` is what makes the comparison meaningful: it is
/// exactly the catalog transition the fold models, and it is the transition under
/// test here (PostgreSQL renames the attribute in place and every index's
/// `indkey` keeps pointing at it, so the index follows the new name and a later
/// drop still cascades).
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
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated rename-index schema");

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
                "fold-rename-index-pg",
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

/// The table and single-column index every rename case below starts from.
const CREATE_INDEXED_TABLE: &str = r#"{
  "ir_version": 1,
  "name": "rename_index_create",
  "owner_app": "app_fold_rename_index_pg",
  "ops": [
    {"op":"createTable","name":"rename_idx","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"a","type":"int","nullable":true},
      {"name":"note","type":"text","nullable":true}
    ],"primaryKey":["id"]},
    {"op":"createIndex","table":"rename_idx","name":"rename_idx_a_key",
     "columns":[{"kind":"column","name":"a"}],"unique":false}
  ]
}"#;

/// A rename must carry the index's KEY COLUMNS with it. PostgreSQL reports the
/// index over the NEW name the instant the rename commits, so a fold that still
/// names the old column drifts against live on the very next introspection.
#[compio::test]
async fn rename_column_carries_its_index_key_columns() {
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_index_key_columns",
      "owner_app": "app_fold_rename_index_pg",
      "ops": [
        {"op":"createTable","name":"rename_idx","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_idx","name":"rename_idx_a_key",
         "columns":[{"kind":"column","name":"a"}],"unique":false},
        {"op":"renameColumn","table":"rename_idx","from":"a","to":"b","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        CREATE_INDEXED_TABLE,
        &["ALTER TABLE {schema}.\"rename_idx\" RENAME COLUMN \"a\" TO \"b\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL indexes reference the attribute, not the name, so a rename moves \
         the index to the new column; the fold must rewrite the index key columns the \
         same way: {drift:#?}"
    );
}

/// The phantom index. `createIndex on a; renameColumn a -> b; dropColumn b`:
/// PostgreSQL cascades the index away with the dropped column, but a fold that
/// still believes the index covers `a` matches nothing in the `DropColumn` cascade
/// and keeps an index live introspection does not have.
#[compio::test]
async fn drop_of_a_renamed_column_cascades_its_index() {
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_then_drop_index_cascade",
      "owner_app": "app_fold_rename_index_pg",
      "ops": [
        {"op":"createTable","name":"rename_idx","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_idx","name":"rename_idx_a_key",
         "columns":[{"kind":"column","name":"a"}],"unique":false},
        {"op":"renameColumn","table":"rename_idx","from":"a","to":"b","type":"int"},
        {"op":"dropColumn","table":"rename_idx","column":"b"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        CREATE_INDEXED_TABLE,
        &[
            "ALTER TABLE {schema}.\"rename_idx\" RENAME COLUMN \"a\" TO \"b\"",
            "ALTER TABLE {schema}.\"rename_idx\" DROP COLUMN \"b\"",
        ],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "dropping a renamed column cascades its index in PostgreSQL; a fold that still \
         records the OLD column name finds no index to cascade and keeps a phantom: \
         {drift:#?}"
    );
}

/// The index NAME is NOT part of the rename. PostgreSQL leaves `rename_idx_a_key`
/// named after a column that no longer exists, and so must the fold - a rewrite
/// that reached the name would invent an index live does not have. This case names
/// the index after the OLD column precisely so a name rewrite could not hide.
#[compio::test]
async fn rename_column_leaves_the_index_name_alone() {
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_index_keeps_name",
      "owner_app": "app_fold_rename_index_pg",
      "ops": [
        {"op":"createTable","name":"rename_idx","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_idx","name":"rename_idx_a_key",
         "columns":[{"kind":"column","name":"a"}],"unique":true},
        {"op":"renameColumn","table":"rename_idx","from":"a","to":"a_renamed",
         "type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        r#"{
          "ir_version": 1,
          "name": "rename_index_keeps_name_create",
          "owner_app": "app_fold_rename_index_pg",
          "ops": [
            {"op":"createTable","name":"rename_idx","columns":[
              {"name":"id","type":"int","nullable":false},
              {"name":"a","type":"int","nullable":true},
              {"name":"note","type":"text","nullable":true}
            ],"primaryKey":["id"]},
            {"op":"createIndex","table":"rename_idx","name":"rename_idx_a_key",
             "columns":[{"kind":"column","name":"a"}],"unique":true}
          ]
        }"#,
        &["ALTER TABLE {schema}.\"rename_idx\" RENAME COLUMN \"a\" TO \"a_renamed\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL keeps an index's name when the column it covers is renamed; the \
         fold must rewrite the key columns without touching the name: {drift:#?}"
    );
}

/// A MULTI-COLUMN index where the rename hits one key and leaves the other, and
/// where the renamed key is not the leading one - the rewrite has to be
/// positional, not a wholesale replacement of the key list.
#[compio::test]
async fn rename_column_carries_one_key_of_a_multi_column_index() {
    let applied = r#"{
      "ir_version": 1,
      "name": "rename_multi_index_create",
      "owner_app": "app_fold_rename_index_pg",
      "ops": [
        {"op":"createTable","name":"rename_multi","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_multi","name":"rename_multi_note_a_key",
         "columns":[{"kind":"column","name":"note"},{"kind":"column","name":"a"}],
         "unique":false}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_multi_index",
      "owner_app": "app_fold_rename_index_pg",
      "ops": [
        {"op":"createTable","name":"rename_multi","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_multi","name":"rename_multi_note_a_key",
         "columns":[{"kind":"column","name":"note"},{"kind":"column","name":"a"}],
         "unique":false},
        {"op":"renameColumn","table":"rename_multi","from":"a","to":"b","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"rename_multi\" RENAME COLUMN \"a\" TO \"b\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a rename must rewrite exactly the key it renames and keep index key ORDER: \
         {drift:#?}"
    );
}

/// A non-key `INCLUDE` column follows the rename exactly as a key column does:
/// `INCLUDE` payload attributes are `indkey` entries past `indnkeyatts`, so they
/// reference the attribute by number too.
#[compio::test]
async fn rename_column_carries_an_index_include_column() {
    let applied = r#"{
      "ir_version": 1,
      "name": "rename_include_index_create",
      "owner_app": "app_fold_rename_index_pg",
      "ops": [
        {"op":"createTable","name":"rename_include","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_include","name":"rename_include_note_key",
         "columns":[{"kind":"column","name":"note"}],"include":["a"],"unique":false}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_include_index",
      "owner_app": "app_fold_rename_index_pg",
      "ops": [
        {"op":"createTable","name":"rename_include","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_include","name":"rename_include_note_key",
         "columns":[{"kind":"column","name":"note"}],"include":["a"],"unique":false},
        {"op":"renameColumn","table":"rename_include","from":"a","to":"b","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"rename_include\" RENAME COLUMN \"a\" TO \"b\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "an INCLUDE payload column follows the rename in PostgreSQL; the fold must \
         rewrite it too: {drift:#?}"
    );
}

/// The table every partial-index case below starts from: an index on `note` whose
/// `WHERE` reads `a`.
const CREATE_PARTIAL_INDEXED_TABLE: &str = r#"{
  "ir_version": 1,
  "name": "rename_partial_index_create",
  "owner_app": "app_fold_rename_index_pg",
  "ops": [
    {"op":"createTable","name":"rename_pred","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"a","type":"int","nullable":true},
      {"name":"note","type":"text","nullable":true}
    ],"primaryKey":["id"]},
    {"op":"createIndex","table":"rename_pred","name":"rename_pred_note_key",
     "columns":[{"kind":"column","name":"note"}],"unique":false,
     "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"a"},
              "rhs":{"node":"literal","value":0}}}
  ]
}"#;

/// The table every expression-key case below starts from: an index keyed on
/// `(a + 1)`.
const CREATE_EXPR_INDEXED_TABLE: &str = r#"{
  "ir_version": 1,
  "name": "rename_expr_index_create",
  "owner_app": "app_fold_rename_index_pg",
  "ops": [
    {"op":"createTable","name":"rename_expr","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"a","type":"int","nullable":true},
      {"name":"note","type":"text","nullable":true}
    ],"primaryKey":["id"]},
    {"op":"createIndex","table":"rename_expr","name":"rename_expr_key",
     "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
       "lhs":{"node":"colRef","name":"a"},"rhs":{"node":"literal","value":1}}}],
     "unique":false}
  ]
}"#;

/// `createIndex WHERE a > 0; rename a -> b; drop b`. PostgreSQL keeps `indpred`
/// pointing at the renamed attribute, so the drop cascades the whole index away. A
/// fold whose predicate provenance still names `a` matches nothing and keeps a
/// phantom index live introspection does not have.
#[compio::test]
async fn drop_of_a_renamed_column_cascades_its_partial_index() {
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_then_drop_partial_index",
      "owner_app": "app_fold_rename_index_pg",
      "ops": [
        {"op":"createTable","name":"rename_pred","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_pred","name":"rename_pred_note_key",
         "columns":[{"kind":"column","name":"note"}],"unique":false,
         "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"a"},
                  "rhs":{"node":"literal","value":0}}},
        {"op":"renameColumn","table":"rename_pred","from":"a","to":"b","type":"int"},
        {"op":"dropColumn","table":"rename_pred","column":"b"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        CREATE_PARTIAL_INDEXED_TABLE,
        &[
            "ALTER TABLE {schema}.\"rename_pred\" RENAME COLUMN \"a\" TO \"b\"",
            "ALTER TABLE {schema}.\"rename_pred\" DROP COLUMN \"b\"",
        ],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "dropping a renamed column cascades the partial index whose predicate reads \
         it; a fold whose predicate provenance still names the OLD column finds no \
         index to cascade and keeps a phantom: {drift:#?}"
    );
}

/// The same for an expression KEY: `createIndex ON ((a + 1)); rename a -> b;
/// drop b`. `pg_index.indexprs` follows the attribute, so the drop cascades.
#[compio::test]
async fn drop_of_a_renamed_column_cascades_its_expression_index() {
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_then_drop_expr_index",
      "owner_app": "app_fold_rename_index_pg",
      "ops": [
        {"op":"createTable","name":"rename_expr","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_expr","name":"rename_expr_key",
         "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
           "lhs":{"node":"colRef","name":"a"},"rhs":{"node":"literal","value":1}}}],
         "unique":false},
        {"op":"renameColumn","table":"rename_expr","from":"a","to":"b","type":"int"},
        {"op":"dropColumn","table":"rename_expr","column":"b"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        CREATE_EXPR_INDEXED_TABLE,
        &[
            "ALTER TABLE {schema}.\"rename_expr\" RENAME COLUMN \"a\" TO \"b\"",
            "ALTER TABLE {schema}.\"rename_expr\" DROP COLUMN \"b\"",
        ],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "dropping a renamed column cascades the index keyed on an expression over it; \
         a fold whose expression provenance still names the OLD column keeps a \
         phantom: {drift:#?}"
    );
}

/// Control: renaming an UNRELATED column must leave the index untouched on both
/// sides. This pins that the rewrite is about the renamed column and not about
/// index folding in general.
#[compio::test]
async fn rename_of_an_unrelated_column_leaves_the_index_untouched() {
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_unrelated_index",
      "owner_app": "app_fold_rename_index_pg",
      "ops": [
        {"op":"createTable","name":"rename_idx","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_idx","name":"rename_idx_a_key",
         "columns":[{"kind":"column","name":"a"}],"unique":false},
        {"op":"renameColumn","table":"rename_idx","from":"note","to":"memo",
         "type":"text"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        CREATE_INDEXED_TABLE,
        &["ALTER TABLE {schema}.\"rename_idx\" RENAME COLUMN \"note\" TO \"memo\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "renaming a column no index covers must leave every index alone on both sides: \
         {drift:#?}"
    );
}
