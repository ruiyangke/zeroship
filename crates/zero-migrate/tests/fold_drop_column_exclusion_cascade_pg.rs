//! Live PostgreSQL oracle for the `Op::DropColumn` EXCLUDE-constraint cascade.
//!
//! An `EXCLUDE` records its plain-column elements in `pg_constraint.conkey`, which
//! IS PostgreSQL's cascade predicate: `ALTER TABLE ... DROP COLUMN` removes every
//! constraint whose `conkey` names the dropped attribute. Measured on PostgreSQL
//! 18.4:
//!
//! ```text
//! ALTER TABLE t ADD CONSTRAINT x EXCLUDE USING btree (a WITH =);
//! ALTER TABLE t DROP COLUMN a;                 -- x is GONE from pg_constraint
//! ALTER TABLE t2 ADD CONSTRAINT x2 EXCLUDE USING btree (a WITH =, b WITH =);
//! ALTER TABLE t2 DROP COLUMN a;                -- x2 is GONE (whole constraint)
//! ```
//!
//! Both `render/fold.rs` EXCLUDE producers recorded `cascade_columns: None` with an
//! empty `definition`, and the `DropColumn` cascade falls back to parsing the leading
//! parenthesized group of `definition` when the provenance is absent. That parse
//! finds nothing in an empty string, so the constraint never matched and never
//! cascaded, leaving a PHANTOM EXCLUDE the live catalog does not have.
//!
//! The empty `definition` is correct and stays: PostgreSQL canonicalizes exclusion
//! bodies differently from the authored render, so
//! `apply::drift::constraint_definition_is_comparable` tracks EXCLUDE by presence and
//! name. Only the provenance is added.
//!
//! The provenance records PLAIN COLUMN ELEMENTS ONLY, because that is exactly the set
//! PostgreSQL cascades on. An `EXCLUDE` that reaches a column through an EXPRESSION
//! element or through its `WHERE` predicate carries a NORMAL dependency rather than an
//! auto one, and PostgreSQL REFUSES the plain `DROP COLUMN` outright rather than
//! cascading - measured on PostgreSQL 18.4:
//!
//! ```text
//! ALTER TABLE t3 ADD CONSTRAINT x3 EXCLUDE USING btree (((a + 1)) WITH =);
//! ALTER TABLE t3 DROP COLUMN a;
//! -- ERROR:  cannot drop column a of table t3 because other objects depend on it
//! -- DETAIL:  constraint x3 on table t3 depends on column a of table t3
//! ```
//!
//! This is where an EXCLUDE constraint DIVERGES from the index precedent next door:
//! an expression index and a partial index are both silently cascaded by the same
//! plain `DROP COLUMN`, which is why `IndexSnapshot::expr_cascade_columns` folds them
//! in. Folding an EXCLUDE's expression or predicate columns the same way would drop a
//! constraint PostgreSQL did NOT drop - and could not, because the statement aborted.
//! The engine only ever emits a plain `ALTER TABLE ... DROP COLUMN` (no `CASCADE`), so
//! the refusal is the real behaviour and the fold must not model a cascade past it.
//!
//! The live half agrees by construction: `snapshot_schema` fills `cascade_columns`
//! from `conkey`, and `conkey` holds attnum `0` for an expression element and nothing
//! at all for the predicate - measured on PostgreSQL 18.4:
//!
//! ```text
//! conname  | conkey | local_columns | def
//! ---------+--------+---------------+------------------------------------------
//! k1_plain | {1}    | {a}           | EXCLUDE USING gist (a WITH =)
//! k1_comp  | {1,2}  | {a,b}         | EXCLUDE USING gist (a WITH =, b WITH =)
//! k1_expr  | {0}    | {}            | EXCLUDE USING gist (((a + 1)) WITH =)
//! k1_pred  | {2}    | {b}           | EXCLUDE USING gist (b WITH =) WHERE ((a > 0))
//! ```
//!
//! `USING btree` is used throughout instead of `gist` so the shapes need no
//! `btree_gist` extension: gist has no `=` strategy for a scalar type on its own, and
//! the cascade being measured is a catalog dependency that does not depend on the
//! access method.

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

const OWNER: &str = "app_fold_drop_exclusion_pg";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "fold_drop_exclusion_pg_{}_{}",
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

/// Apply `applied` through the real engine, then run `native_sql` (each statement with
/// `{schema}` replaced by the quoted project schema), then fold `folded` offline and
/// return the drift against live introspection.
///
/// The split exists for the ops the engine cannot apply as the fold models them - see
/// the rename case below. `drift_after_applying` is the degenerate form where the
/// applied and folded histories are the same and no native SQL is needed.
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
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated drop-column exclusion-cascade schema");

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
                "fold-drop-exclusion-pg",
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

/// The phantom EXCLUDE, via the stand-alone `addConstraint` producer. PostgreSQL
/// cascades the constraint away with the column; the fold kept it.
#[compio::test]
async fn drop_column_cascades_an_added_exclusion_over_it() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_added_exclusion_cascade",
      "owner_app": "app_fold_drop_exclusion_pg",
      "ops": [
        {"op":"createTable","name":"excl_added","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"excl_added","constraint":{
          "name":"excl_added_a_excl",
          "kind":{"kind":"exclusion","usingMethod":"btree","elements":[
            {"target":{"kind":"column","name":"a"},"operator":"="}
          ]}
        }},
        {"op":"dropColumn","table":"excl_added","column":"a"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL cascades an EXCLUDE away when a column its `conkey` names is \
         dropped; a fold that records no cascade provenance keeps a phantom \
         constraint: {drift:#?}"
    );
}

/// The same phantom through the OTHER producer: an EXCLUDE declared inline on
/// `createTable`. Both producers wrote the same empty `definition` + absent
/// provenance, so both had to be fixed.
#[compio::test]
async fn drop_column_cascades_a_create_table_exclusion_over_it() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_create_table_exclusion_cascade",
      "owner_app": "app_fold_drop_exclusion_pg",
      "ops": [
        {"op":"createTable","name":"excl_inline","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true}
        ],"primaryKey":["id"],"constraints":[
          {"name":"excl_inline_a_excl",
           "kind":{"kind":"exclusion","usingMethod":"btree","elements":[
             {"target":{"kind":"column","name":"a"},"operator":"="}
           ]}}
        ]},
        {"op":"dropColumn","table":"excl_inline","column":"a"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a `createTable` EXCLUDE cascades in PostgreSQL exactly as an added one does; \
         the inline producer needs the same provenance: {drift:#?}"
    );
}

/// A COMPOSITE EXCLUDE, dropping only ONE of the columns it names. PostgreSQL removes
/// the WHOLE constraint - there is no partial exclusion - so the fold must too.
#[compio::test]
async fn drop_column_cascades_a_composite_exclusion_naming_it() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_composite_exclusion_cascade",
      "owner_app": "app_fold_drop_exclusion_pg",
      "ops": [
        {"op":"createTable","name":"excl_composite","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"b","type":"int","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"excl_composite","constraint":{
          "name":"excl_composite_ab_excl",
          "kind":{"kind":"exclusion","usingMethod":"btree","elements":[
            {"target":{"kind":"column","name":"a"},"operator":"="},
            {"target":{"kind":"column","name":"b"},"operator":"="}
          ]}
        }},
        {"op":"dropColumn","table":"excl_composite","column":"a"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL drops a composite EXCLUDE whole when any column it names is \
         dropped: {drift:#?}"
    );
}

/// Control: dropping a column the EXCLUDE does not name must leave the constraint
/// standing on both sides. Without this arm the cascade above could pass by dropping
/// every EXCLUDE on the table.
#[compio::test]
async fn drop_of_an_unrelated_column_keeps_an_exclusion() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_unrelated_column_keeps_exclusion",
      "owner_app": "app_fold_drop_exclusion_pg",
      "ops": [
        {"op":"createTable","name":"excl_unrelated","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"spare","type":"int","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"excl_unrelated","constraint":{
          "name":"excl_unrelated_a_excl",
          "kind":{"kind":"exclusion","usingMethod":"btree","elements":[
            {"target":{"kind":"column","name":"a"},"operator":"="}
          ]}
        }},
        {"op":"dropColumn","table":"excl_unrelated","column":"spare"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "an EXCLUDE that does not name the dropped column survives in PostgreSQL and \
         must survive the fold: {drift:#?}"
    );
}

/// The GUARD. The dropped column `a` is spelled only inside a STRING LITERAL in the
/// exclusion's `WHERE` predicate, never referenced. PostgreSQL keeps the constraint,
/// so a cascade that matched the rendered text would drop a constraint the catalog
/// KEPT - drift worse than the phantom the provenance fixes.
#[compio::test]
async fn drop_column_keeps_an_exclusion_that_only_spells_it_in_a_literal() {
    let source = r#"{
      "ir_version": 1,
      "name": "drop_column_keeps_literal_spelling_exclusion",
      "owner_app": "app_fold_drop_exclusion_pg",
      "ops": [
        {"op":"createTable","name":"excl_literal","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"excl_literal","constraint":{
          "name":"excl_literal_note_excl",
          "kind":{"kind":"exclusion","usingMethod":"btree","elements":[
            {"target":{"kind":"column","name":"note"},"operator":"="}
          ],
          "wherePredicate":{"node":"binOp","op":"ne",
            "lhs":{"node":"colRef","name":"note"},
            "rhs":{"node":"literal","value":"a"}}}
        }},
        {"op":"dropColumn","table":"excl_literal","column":"a"}
      ]
    }"#;

    let Some(drift) = drift_after_applying(source).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "a column name spelled inside a string LITERAL is not a column reference; \
         PostgreSQL keeps this EXCLUDE and the fold must keep it too: {drift:#?}"
    );
}

/// A rename followed by a drop of the NEW name. PostgreSQL renames the attribute in
/// place and leaves `conkey` pointing at it, so the later drop still cascades. The
/// provenance names columns, so the `RenameColumn` arm has to follow it - without
/// that it would still name the OLD column and the drop would find no match.
///
/// The rename + drop run as NATIVE SQL rather than through the engine, for the reason
/// the rename oracle next door does the same: the engine lowers `renameColumn` to an
/// online expand-contract whose contract is a separate later deploy, and it refuses to
/// share a migration with another op on the table. Native `RENAME COLUMN` is exactly
/// what the fold models, so it is the honest live half here.
#[compio::test]
async fn rename_then_drop_cascades_an_exclusion_through_the_new_name() {
    let applied = r#"{
      "ir_version": 1,
      "name": "rename_then_drop_exclusion_create",
      "owner_app": "app_fold_drop_exclusion_pg",
      "ops": [
        {"op":"createTable","name":"excl_renamed","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"qty","type":"int","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"excl_renamed","constraint":{
          "name":"excl_renamed_qty_excl",
          "kind":{"kind":"exclusion","usingMethod":"btree","elements":[
            {"target":{"kind":"column","name":"qty"},"operator":"="}
          ]}
        }}
      ]
    }"#;
    let native_sql = [
        "ALTER TABLE {schema}.\"excl_renamed\" RENAME COLUMN \"qty\" TO \"amount\"",
        "ALTER TABLE {schema}.\"excl_renamed\" DROP COLUMN \"amount\"",
    ];
    let folded = r#"{
      "ir_version": 1,
      "name": "rename_then_drop_exclusion_folded",
      "owner_app": "app_fold_drop_exclusion_pg",
      "ops": [
        {"op":"createTable","name":"excl_renamed","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"qty","type":"int","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"excl_renamed","constraint":{
          "name":"excl_renamed_qty_excl",
          "kind":{"kind":"exclusion","usingMethod":"btree","elements":[
            {"target":{"kind":"column","name":"qty"},"operator":"="}
          ]}
        }},
        {"op":"renameColumn","table":"excl_renamed","from":"qty","to":"amount",
         "type":"int"},
        {"op":"dropColumn","table":"excl_renamed","column":"amount"}
      ]
    }"#;

    let Some(drift) = drift_between_fold_and_live(applied, &native_sql, folded).await else {
        return;
    };
    assert!(
        drift.is_clean(),
        "PostgreSQL keeps `conkey` pointing at the renamed attribute, so dropping the \
         NEW name still cascades the EXCLUDE; the fold's provenance has to follow the \
         rename: {drift:#?}"
    );
}
