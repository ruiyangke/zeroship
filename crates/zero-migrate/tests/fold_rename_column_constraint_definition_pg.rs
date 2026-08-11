//! Live PostgreSQL oracle for the `Op::RenameColumn` constraint-definition rewrite.
//!
//! PostgreSQL renames a column ATTRIBUTE in place. `pg_constraint.conkey` holds
//! attribute NUMBERS, never names, so every constraint over the renamed column
//! follows it automatically: `pg_get_constraintdef` deparses the NEW name the
//! instant the rename commits.
//!
//! `render/fold.rs` used to update `snap.columns`, `ConstraintSnapshot::cascade_columns`
//! and `snap.indexes` in its `Op::RenameColumn` arm while leaving
//! `ConstraintSnapshot::definition` untouched. The differ compares that field for
//! every kind except `EXCLUDE` and `CHECK` (`apply::drift::constraint_definition_is_comparable`),
//! so after `rename a -> b` the fold reported `UNIQUE (a)` / `FOREIGN KEY (a) ...` /
//! `PRIMARY KEY (a)` against live's `UNIQUE (b)` / `FOREIGN KEY (b) ...` /
//! `PRIMARY KEY (b)` - real drift on the very next introspection.
//!
//! Only the LEADING PARENTHESIZED GROUP is re-rendered, and only for UNIQUE /
//! PRIMARY KEY / FOREIGN KEY, whose leading group is a LOCAL COLUMN LIST. A string
//! literal is not legal in that grammar, so the trap that rules out text matching
//! for a CHECK body (`CHECK ((status <> 'qty'::text))` survives dropping `qty` -
//! measured) is structurally unreachable here. The FK tail (`REFERENCES ...`, MATCH,
//! `ON UPDATE`/`ON DELETE`, DEFERRABLE, ` NOT VALID`) is spliced through untouched.
//!
//! The FK tail carries a SECOND column list, the REFERENCED one, and it lives in a
//! DIFFERENT table than the renamed column. `pg_constraint.confkey` is attribute
//! numbers too, so live deparses the new name there as well while the rename arm -
//! which never leaves its own table's snapshot - left it stale. That list is the same
//! CLOSED IDENTIFIER LIST grammar, so the same argument licenses the same rewrite;
//! `rewrite_incoming_fk_column_targets` matches the referenced TABLE first, because a
//! rewrite keyed on the column name alone would corrupt an unrelated FK that
//! references a same-named column in another parent.
//!
//! Quoting is CONDITIONAL, which is why the group is RE-RENDERED through
//! `render::declarative::constraintdef_cols` rather than substring-swapped:
//! `rename a -> order` must produce `UNIQUE ("order")`, and a naive swap gives
//! `UNIQUE (order)` - one drift traded for another.
//!
//! The renames here run as native `ALTER TABLE ... RENAME COLUMN` rather than
//! through the engine's `renameColumn` op - see `drift_between_fold_and_live` in
//! `fold_rename_column_index_cascade_pg.rs` for why.

mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, fold_ops, resolve_create_table_policy, snapshot_schema, Approval,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr,
    PostgresBackend, SqlDialect, StructuralDrift,
};

const OWNER: &str = "app_fold_rename_constraint_pg";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "fold_rename_constraint_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// Apply `applied` through the real engine, run `native_sql` (each statement with
/// `{schema}` replaced by the quoted test schema) on the same real session, then
/// fold `folded` offline and diff it against live introspection. `None` when no
/// live database is configured (the caller then skips its assertion).
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
        .expect("create isolated rename-constraint schema");

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
                "fold-rename-constraint-pg",
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

/// A table-level UNIQUE over the renamed column. `pg_get_constraintdef` reports
/// `UNIQUE (b)` the instant `a` becomes `b`; a fold that keeps `UNIQUE (a)` drifts.
#[compio::test]
async fn rename_column_rewrites_a_unique_constraint_definition() {
    let applied = r#"{
      "ir_version": 1,
      "name": "rename_unique_create",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_uq","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"],"constraints":[
          {"name":"rename_uq_a_key","kind":{"kind":"unique","columns":["a"]}}
        ]}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_unique",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_uq","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"],"constraints":[
          {"name":"rename_uq_a_key","kind":{"kind":"unique","columns":["a"]}}
        ]},
        {"op":"renameColumn","table":"rename_uq","from":"a","to":"b","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"rename_uq\" RENAME COLUMN \"a\" TO \"b\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL deparses a UNIQUE constraint over the NEW column name the instant \
         the rename commits; the fold must re-render the definition's column list: \
         {drift:#?}"
    );
}

/// A FOREIGN KEY whose LOCAL column is renamed. Only the leading group moves - the
/// `REFERENCES ...` tail, the referential actions and the deferrability must stay
/// byte-identical, so this case carries all three.
#[compio::test]
async fn rename_column_rewrites_a_foreign_key_local_column() {
    let applied = r#"{
      "ir_version": 1,
      "name": "rename_fk_create",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_fk_parent","columns":[
          {"name":"id","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"createTable","name":"rename_fk_child","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"rename_fk_child","constraint":{
          "name":"rename_fk_child_a_fkey",
          "kind":{"kind":"fk","columns":["a"],
            "referencesTable":"rename_fk_parent","referencesColumns":["id"],
            "onDelete":"cascade","onUpdate":"restrict",
            "deferrable":true,"initiallyDeferred":true}
        }}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_fk",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_fk_parent","columns":[
          {"name":"id","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"createTable","name":"rename_fk_child","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"rename_fk_child","constraint":{
          "name":"rename_fk_child_a_fkey",
          "kind":{"kind":"fk","columns":["a"],
            "referencesTable":"rename_fk_parent","referencesColumns":["id"],
            "onDelete":"cascade","onUpdate":"restrict",
            "deferrable":true,"initiallyDeferred":true}
        }},
        {"op":"renameColumn","table":"rename_fk_child","from":"a","to":"b","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"rename_fk_child\" RENAME COLUMN \"a\" TO \"b\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a FOREIGN KEY's LOCAL column follows the rename in PostgreSQL; the fold must \
         re-render the leading group and splice the REFERENCES tail through untouched: \
         {drift:#?}"
    );
}

/// A COMPOSITE UNIQUE over `(note, a)` where the rename hits the TRAILING column.
/// The re-render is positional: `UNIQUE (note, b)`, never a re-sort.
#[compio::test]
async fn rename_column_keeps_composite_unique_column_order() {
    let applied = r#"{
      "ir_version": 1,
      "name": "rename_composite_unique_create",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_uq_multi","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":false}
        ],"primaryKey":["id"],"constraints":[
          {"name":"rename_uq_multi_key","kind":{"kind":"unique","columns":["note","a"]}}
        ]}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_composite_unique",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_uq_multi","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":false}
        ],"primaryKey":["id"],"constraints":[
          {"name":"rename_uq_multi_key","kind":{"kind":"unique","columns":["note","a"]}}
        ]},
        {"op":"renameColumn","table":"rename_uq_multi","from":"a","to":"b","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"rename_uq_multi\" RENAME COLUMN \"a\" TO \"b\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a composite UNIQUE keeps its declared column ORDER across a rename; the fold \
         must rewrite the renamed position in place and never re-sort: {drift:#?}"
    );
}

/// The rename target is a RESERVED WORD. PostgreSQL deparses `UNIQUE ("order")`,
/// so the fold has to re-render the group through the conditional-quoting speller
/// instead of swapping the name inside the text - a naive swap yields
/// `UNIQUE (order)` and trades one drift for another.
#[compio::test]
async fn rename_column_into_a_reserved_word_requotes_the_constraint_definition() {
    let applied = r#"{
      "ir_version": 1,
      "name": "rename_reserved_create",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_uq_reserved","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false}
        ],"primaryKey":["id"],"constraints":[
          {"name":"rename_uq_reserved_key","kind":{"kind":"unique","columns":["a"]}}
        ]}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_reserved",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_uq_reserved","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false}
        ],"primaryKey":["id"],"constraints":[
          {"name":"rename_uq_reserved_key","kind":{"kind":"unique","columns":["a"]}}
        ]},
        {"op":"renameColumn","table":"rename_uq_reserved","from":"a","to":"order",
         "type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"rename_uq_reserved\" RENAME COLUMN \"a\" TO \"order\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "renaming into a reserved word makes PostgreSQL deparse UNIQUE (\"order\"); the \
         fold must re-render through the conditional-quoting speller rather than \
         swapping the name inside the rendered text: {drift:#?}"
    );
}

/// A PRIMARY KEY over the renamed column. `constraint_definition_is_comparable`
/// excludes only `EXCLUDE` and `CHECK`, so a stale `PRIMARY KEY (a)` reports too.
#[compio::test]
async fn rename_column_rewrites_a_primary_key_definition() {
    let applied = r#"{
      "ir_version": 1,
      "name": "rename_pk_create",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_pk","columns":[
          {"name":"a","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["a"]}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_pk",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_pk","columns":[
          {"name":"a","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["a"]},
        {"op":"renameColumn","table":"rename_pk","from":"a","to":"b","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"rename_pk\" RENAME COLUMN \"a\" TO \"b\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a PRIMARY KEY definition follows the rename in PostgreSQL and the differ \
         compares it (only EXCLUDE and CHECK are exempt); the fold must re-render it: \
         {drift:#?}"
    );
}

/// The renamed column is the FK TARGET of a DIFFERENT table. `pg_constraint.confkey`
/// holds attribute NUMBERS too, so the referencing table's `REFERENCES ...(id)` tail
/// deparses the NEW name the instant the rename commits, in a table the rename arm
/// never visits.
#[compio::test]
async fn rename_column_rewrites_an_incoming_foreign_key_reference() {
    let applied = r#"{
      "ir_version": 1,
      "name": "xfk_create",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"xfk_parent","columns":[
          {"name":"id","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"createTable","name":"xfk_child","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"parent_id","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"xfk_child","constraint":{
          "name":"xfk_child_parent_fkey",
          "kind":{"kind":"fk","columns":["parent_id"],
            "referencesTable":"xfk_parent","referencesColumns":["id"]}
        }}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "xfk",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"xfk_parent","columns":[
          {"name":"id","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"createTable","name":"xfk_child","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"parent_id","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"xfk_child","constraint":{
          "name":"xfk_child_parent_fkey",
          "kind":{"kind":"fk","columns":["parent_id"],
            "referencesTable":"xfk_parent","referencesColumns":["id"]}
        }},
        {"op":"renameColumn","table":"xfk_parent","from":"id","to":"uid","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"xfk_parent\" RENAME COLUMN \"id\" TO \"uid\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a FK in ANOTHER table names the referenced column in its REFERENCES tail, and \
         PostgreSQL deparses the NEW name there the instant the rename commits; the \
         fold must rewrite the referencing table too: {drift:#?}"
    );
}

/// Two parents carry a column of the SAME name and one child references each. The
/// rename hits ONE parent, so ONLY the FK that targets that parent may move - a
/// rewrite that matched on the column name alone would corrupt the other FK into
/// naming a column its parent does not have.
#[compio::test]
async fn rename_column_leaves_an_incoming_fk_on_another_parent_alone() {
    let applied = r#"{
      "ir_version": 1,
      "name": "xfk_twin_create",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"xfk_p1","columns":[
          {"name":"k","type":"int","nullable":false}
        ],"primaryKey":["k"]},
        {"op":"createTable","name":"xfk_p2","columns":[
          {"name":"k","type":"int","nullable":false}
        ],"primaryKey":["k"]},
        {"op":"createTable","name":"xfk_twin_child","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"p1_k","type":"int","nullable":false},
          {"name":"p2_k","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"xfk_twin_child","constraint":{
          "name":"xfk_twin_child_p1_fkey",
          "kind":{"kind":"fk","columns":["p1_k"],
            "referencesTable":"xfk_p1","referencesColumns":["k"]}
        }},
        {"op":"addConstraint","table":"xfk_twin_child","constraint":{
          "name":"xfk_twin_child_p2_fkey",
          "kind":{"kind":"fk","columns":["p2_k"],
            "referencesTable":"xfk_p2","referencesColumns":["k"]}
        }}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "xfk_twin",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"xfk_p1","columns":[
          {"name":"k","type":"int","nullable":false}
        ],"primaryKey":["k"]},
        {"op":"createTable","name":"xfk_p2","columns":[
          {"name":"k","type":"int","nullable":false}
        ],"primaryKey":["k"]},
        {"op":"createTable","name":"xfk_twin_child","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"p1_k","type":"int","nullable":false},
          {"name":"p2_k","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"xfk_twin_child","constraint":{
          "name":"xfk_twin_child_p1_fkey",
          "kind":{"kind":"fk","columns":["p1_k"],
            "referencesTable":"xfk_p1","referencesColumns":["k"]}
        }},
        {"op":"addConstraint","table":"xfk_twin_child","constraint":{
          "name":"xfk_twin_child_p2_fkey",
          "kind":{"kind":"fk","columns":["p2_k"],
            "referencesTable":"xfk_p2","referencesColumns":["k"]}
        }},
        {"op":"renameColumn","table":"xfk_p1","from":"k","to":"uid","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"xfk_p1\" RENAME COLUMN \"k\" TO \"uid\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "only the FK whose REFERENCES tail names the RENAMED table may follow the \
         rename; a same-named column in a different parent must keep its own FK \
         byte-identical: {drift:#?}"
    );
}

/// A SELF-REFERENCING FK: the renamed column is both the table's own PK and the
/// target of its own FK. The referencing constraint lives in the renamed table's
/// snapshot, so the walk must include that table rather than skipping it.
#[compio::test]
async fn rename_column_rewrites_a_self_referencing_foreign_key() {
    let applied = r#"{
      "ir_version": 1,
      "name": "xfk_self_create",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"xfk_self","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"parent_id","type":"int","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"xfk_self","constraint":{
          "name":"xfk_self_parent_fkey",
          "kind":{"kind":"fk","columns":["parent_id"],
            "referencesTable":"xfk_self","referencesColumns":["id"]}
        }}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "xfk_self",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"xfk_self","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"parent_id","type":"int","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"xfk_self","constraint":{
          "name":"xfk_self_parent_fkey",
          "kind":{"kind":"fk","columns":["parent_id"],
            "referencesTable":"xfk_self","referencesColumns":["id"]}
        }},
        {"op":"renameColumn","table":"xfk_self","from":"id","to":"uid","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"xfk_self\" RENAME COLUMN \"id\" TO \"uid\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a self-referencing FK targets the renamed column from inside the renamed \
         table's own snapshot; the fold must rewrite its REFERENCES tail too: {drift:#?}"
    );
}

/// A COMPOSITE FK where only ONE referenced column is renamed. The referenced list
/// is POSITIONAL and pairs with the local list, so the rewrite must move exactly
/// that position and never re-sort.
#[compio::test]
async fn rename_column_rewrites_one_position_of_a_composite_fk_reference() {
    let applied = r#"{
      "ir_version": 1,
      "name": "xfk_pair_create",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"xfk_pair_parent","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false},
          {"name":"b","type":"int","nullable":false}
        ],"primaryKey":["id"],"constraints":[
          {"name":"xfk_pair_parent_ab_key","kind":{"kind":"unique","columns":["a","b"]}}
        ]},
        {"op":"createTable","name":"xfk_pair_child","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"x","type":"int","nullable":false},
          {"name":"y","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"xfk_pair_child","constraint":{
          "name":"xfk_pair_child_ab_fkey",
          "kind":{"kind":"fk","columns":["x","y"],
            "referencesTable":"xfk_pair_parent","referencesColumns":["a","b"]}
        }}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "xfk_pair",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"xfk_pair_parent","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false},
          {"name":"b","type":"int","nullable":false}
        ],"primaryKey":["id"],"constraints":[
          {"name":"xfk_pair_parent_ab_key","kind":{"kind":"unique","columns":["a","b"]}}
        ]},
        {"op":"createTable","name":"xfk_pair_child","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"x","type":"int","nullable":false},
          {"name":"y","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"xfk_pair_child","constraint":{
          "name":"xfk_pair_child_ab_fkey",
          "kind":{"kind":"fk","columns":["x","y"],
            "referencesTable":"xfk_pair_parent","referencesColumns":["a","b"]}
        }},
        {"op":"renameColumn","table":"xfk_pair_parent","from":"a","to":"c","type":"int"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"xfk_pair_parent\" RENAME COLUMN \"a\" TO \"c\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a composite FK's referenced list is positional and pairs with the local \
         list; renaming one referenced column must move exactly that position: \
         {drift:#?}"
    );
}

/// Control: renaming a column NO constraint covers must leave every constraint
/// definition untouched on both sides. This pins that the rewrite is about the
/// renamed column and not about constraint folding in general.
#[compio::test]
async fn rename_of_an_unrelated_column_leaves_constraint_definitions_alone() {
    let applied = r#"{
      "ir_version": 1,
      "name": "rename_unrelated_constraint_create",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_uq_ctl","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"],"constraints":[
          {"name":"rename_uq_ctl_a_key","kind":{"kind":"unique","columns":["a"]}}
        ]}
      ]
    }"#;
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_unrelated_constraint",
      "owner_app": "app_fold_rename_constraint_pg",
      "ops": [
        {"op":"createTable","name":"rename_uq_ctl","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"],"constraints":[
          {"name":"rename_uq_ctl_a_key","kind":{"kind":"unique","columns":["a"]}}
        ]},
        {"op":"renameColumn","table":"rename_uq_ctl","from":"note","to":"memo",
         "type":"text"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(
        applied,
        &["ALTER TABLE {schema}.\"rename_uq_ctl\" RENAME COLUMN \"note\" TO \"memo\""],
        folded,
    )
    .await
    else {
        return;
    };
    assert!(
        drift.is_clean(),
        "renaming a column no constraint covers must leave every constraint definition \
         alone on both sides: {drift:#?}"
    );
}
