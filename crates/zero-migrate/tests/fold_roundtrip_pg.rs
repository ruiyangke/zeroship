//! Live PostgreSQL round-trip checks for `fold_ops` primary-key and lifecycle cases.
//!
//! The primary-key tests apply a validated IR plan through
//! `MigrationEngine::apply_plan`,
//! verifies the resulting primary-key catalog state, folds the same resolved ops
//! offline, and checks the folded snapshot against live introspection with
//! `diff_snapshots(...).is_clean()`. The first two checks cover PostgreSQL's
//! implicit-name uniquification. The third covers adopting a standalone candidate
//! index. The lifecycle tests apply and compare at every listed stable point.
//!
//! A clean drift result is strictly narrower than saying that the snapshots agree.
//! `IndexSnapshot` equality excludes `opclass` and `nulls_not_distinct`. It does
//! compare `only`, while PostgreSQL introspection hardcodes `only: false`, so a case
//! that authors `only: true` would be a guaranteed false red. This file does not
//! claim `fold_ops == snapshot_schema`, and it does not cover index facets outside
//! the three primary-key naming cases below.

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

const OWNER: &str = "app_fold_roundtrip_pg";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "fold_roundtrip_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn cfg_for(schema: &str) -> ExecutorConfig {
    ExecutorConfig::new(
        format!("project_{schema}"),
        schema,
        support::no_inject(schema),
    )
}

#[derive(Debug)]
struct PrimaryKeyCatalogRow {
    table: String,
    constraint: String,
    index: String,
}

async fn primary_key_catalog(
    session: &PgDevSession,
    schema: &str,
) -> Result<Vec<PrimaryKeyCatalogRow>, String> {
    let rows = session
        .query(
            "SELECT tbl.relname::text AS table_name, \
                    con.conname::text AS constraint_name, \
                    idx_rel.relname::text AS index_name \
             FROM pg_catalog.pg_constraint con \
             JOIN pg_catalog.pg_class tbl ON tbl.oid = con.conrelid \
             JOIN pg_catalog.pg_namespace ns ON ns.oid = tbl.relnamespace \
             JOIN pg_catalog.pg_index idx \
               ON idx.indrelid = con.conrelid AND idx.indexrelid = con.conindid \
             JOIN pg_catalog.pg_class idx_rel ON idx_rel.oid = idx.indexrelid \
             WHERE ns.nspname = $1 AND con.contype = 'p' AND idx.indisprimary \
             ORDER BY tbl.relname, con.conname",
            &[schema.into()],
        )
        .await
        .map_err(|error| format!("query live primary-key catalog: {error}"))?;

    rows.iter()
        .map(|row| {
            Ok(PrimaryKeyCatalogRow {
                table: row
                    .try_get("table_name")
                    .map_err(|error| format!("decode primary-key table name: {error}"))?,
                constraint: row
                    .try_get("constraint_name")
                    .map_err(|error| format!("decode primary-key constraint name: {error}"))?,
                index: row
                    .try_get("index_name")
                    .map_err(|error| format!("decode primary-key index name: {error}"))?,
            })
        })
        .collect()
}

fn has_primary_key(catalog: &[PrimaryKeyCatalogRow], table_name: &str, object_name: &str) -> bool {
    catalog.iter().any(|row| {
        row.table == table_name && row.constraint == object_name && row.index == object_name
    })
}

async fn apply_ir(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    source: &str,
) -> Result<MigrationIr, String> {
    let backend = PostgresBackend::new_generic(session);
    backend
        .ensure_journal(cfg)
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
            cfg,
            "fold-roundtrip-pg",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply IR plan: {error}"))?;

    Ok(resolved)
}

async fn assert_roundtrip(
    label: &str,
    source: &str,
    precondition: impl FnOnce(&[PrimaryKeyCatalogRow]) -> Result<(), String>,
) {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = cfg_for(&schema);
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated fold round-trip schema");

    let work: Result<StructuralDrift, String> = async {
        let ir = apply_ir(&session, &cfg, source).await?;
        let catalog = primary_key_catalog(&session, &cfg.project_schema).await?;
        precondition(&catalog)?;

        let expected = fold_ops(
            &ir.ops,
            SqlDialect::Postgres,
            &cfg.project_schema,
            &support::no_inject(&cfg.project_schema),
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
    let drift = match (work, cleanup) {
        (Ok(drift), Ok(())) => drift,
        (Err(work), Ok(())) => panic!("{label}: {work}"),
        (Ok(_), Err(cleanup)) => panic!("{label}: drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => {
            panic!("{label}: {work}; cleanup failed: {cleanup}")
        }
    };

    assert!(
        drift.is_clean(),
        "{label}: folded and live PostgreSQL schemas must have clean drift: {drift:#?}"
    );
}

#[compio::test]
async fn rename_and_recreate_exposes_implicit_primary_key_name_collision() {
    let source = r#"{
      "ir_version": 1,
      "name": "rename_and_recreate_primary_key",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"old","columns":[
          {"name":"id","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"renameTable","table":"old","to":"new"},
        {"op":"createTable","name":"old","columns":[
          {"name":"id","type":"int","nullable":false}
        ],"primaryKey":["id"]}
      ]
    }"#;

    assert_roundtrip(
        "rename and recreate primary-key name collision",
        source,
        |catalog| {
            if has_primary_key(catalog, "new", "old_pkey")
                && has_primary_key(catalog, "old", "old_pkey1")
            {
                Ok(())
            } else {
                Err(format!(
                    "SCENARIO did not occur: expected new.old_pkey and old.old_pkey1 \
                     as both constraint and backing-index names; catalog: {catalog:#?}"
                ))
            }
        },
    )
    .await;
}

#[compio::test]
async fn unrelated_index_exposes_implicit_primary_key_name_uniquification() {
    let source = r#"{
      "ir_version": 1,
      "name": "unrelated_index_primary_key_collision",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"t4","columns":[
          {"name":"id","type":"int","nullable":false}
        ],"constraints":[
          {"name":"t4_id_key","kind":{"kind":"unique","columns":["id"]}}
        ]},
        {"op":"createIndex","table":"t4","name":"t4_pkey","columns":[
          {"kind":"column","name":"id"}
        ],"unique":false},
        {"op":"alterPrimaryKey","table":"t4","action":{
          "kind":"add","columns":["id"]
        }}
      ]
    }"#;

    assert_roundtrip(
        "unrelated index primary-key name collision",
        source,
        |catalog| {
            if has_primary_key(catalog, "t4", "t4_pkey1") {
                Ok(())
            } else {
                Err(format!(
                    "SCENARIO did not occur: expected constraint and backing index \
                     t4_pkey1 on t4; catalog: {catalog:#?}"
                ))
            }
        },
    )
    .await;
}

#[compio::test]
async fn using_candidate_index_keeps_fold_and_live_primary_key_names_aligned() {
    let source = r#"{
      "ir_version": 1,
      "name": "adopt_candidate_primary_key_index",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"t5","columns":[
          {"name":"id","type":"int","nullable":false}
        ]},
        {"op":"createIndex","table":"t5","name":"t5_id_candidate","columns":[
          {"kind":"column","name":"id"}
        ],"unique":true},
        {"op":"alterPrimaryKey","table":"t5","action":{
          "kind":"add","columns":["id"]
        }}
      ]
    }"#;

    assert_roundtrip("candidate index primary-key adoption", source, |catalog| {
        if has_primary_key(catalog, "t5", "t5_id_candidate") {
            Ok(())
        } else {
            Err(format!(
                "SCENARIO did not occur: expected PostgreSQL to adopt \
                 t5_id_candidate as both the constraint and backing-index name; \
                 catalog: {catalog:#?}"
            ))
        }
    })
    .await;
}

async fn assert_lifecycle_roundtrip(label: &str, source: &str, checkpoints: &[(&str, usize)]) {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = cfg_for(&schema);
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated fold lifecycle schema");

    let work: Result<(), String> = async {
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

        if artifact.op_spans.len() != resolved.ops.len() {
            return Err(format!(
                "lowered {} operation spans for {} resolved ops",
                artifact.op_spans.len(),
                resolved.ops.len()
            ));
        }

        let engine = MigrationEngine::new();
        let mut applied_ops = Vec::new();
        let mut next_checkpoint = 0;
        for (op_index, span) in artifact.op_spans.iter().enumerate() {
            if span.op != resolved.ops[op_index] {
                return Err(format!(
                    "lowered operation span {op_index} does not match the resolved operation"
                ));
            }

            let mut ranges = vec![span.step_range.clone()];
            ranges.extend(span.additional_step_ranges.iter().cloned());
            ranges.sort_by_key(|range| range.start);
            for range in ranges {
                engine
                    .apply_plan(
                        &artifact.plan.steps[range],
                        Approval::Approved,
                        &backend,
                        &cfg,
                        "fold-roundtrip-pg",
                        LockMode::Acquire,
                    )
                    .await
                    .map_err(|error| {
                        format!("apply IR plan operation {}: {error}", op_index + 1)
                    })?;
            }
            applied_ops.push(resolved.ops[op_index].clone());

            if checkpoints
                .get(next_checkpoint)
                .is_some_and(|(_, op_count)| *op_count == applied_ops.len())
            {
                let (checkpoint, _) = checkpoints[next_checkpoint];
                let expected = fold_ops(
                    &applied_ops,
                    SqlDialect::Postgres,
                    &cfg.project_schema,
                    &support::no_inject(&cfg.project_schema),
                )
                .map_err(|error| {
                    format!("{checkpoint}: fold the applied PostgreSQL ops: {error}")
                })?;
                let actual = snapshot_schema(&session, &cfg.project_schema)
                    .await
                    .map_err(|error| {
                        format!("{checkpoint}: snapshot the live PostgreSQL schema: {error}")
                    })?;
                let drift = diff_snapshots(&expected, &actual);
                if !drift.is_clean() {
                    return Err(format!(
                        "{checkpoint}: folded and live PostgreSQL schemas must have clean drift: \
                         {drift:#?}"
                    ));
                }

                // A CHECK body is deliberately not compared, so a clean drift result
                // says nothing about whether the live text was read at all. The guard's
                // fail-closed refusal prints this field, so an empty one degrades that
                // message to `<present>`. Assert it survives introspection here,
                // because nothing in the comparison would notice if it stopped.
                for (table, snapshot) in &actual.tables {
                    for constraint in &snapshot.constraints {
                        if constraint.kind == "CHECK" && constraint.definition.is_empty() {
                            return Err(format!(
                                "{checkpoint}: live CHECK constraint {table}.{} lost its \
                                 catalog definition text",
                                constraint.name
                            ));
                        }
                    }
                }
                next_checkpoint += 1;
            }
        }

        if next_checkpoint != checkpoints.len() {
            return Err(format!(
                "reached {next_checkpoint} of {} lifecycle checkpoints",
                checkpoints.len()
            ));
        }
        Ok(())
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(()), Ok(())) => {}
        (Err(work), Ok(())) => panic!("{label}: {work}"),
        (Ok(()), Err(cleanup)) => panic!("{label}: drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => {
            panic!("{label}: {work}; cleanup failed: {cleanup}")
        }
    }
}

#[compio::test]
async fn add_and_alter_columns() {
    // Exercise plain column alteration stages. This does not cover setColumnType.using.
    let source = r#"{
      "ir_version": 1,
      "name": "add_and_alter_columns",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"column_lifecycle","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"amount","type":"int","nullable":true}
        ]},
        {"op":"addColumn","table":"column_lifecycle","column":"note",
         "type":"text","nullable":true},
        {"op":"setColumnNotNull","table":"column_lifecycle","column":"note"},
        {"op":"dropColumnNotNull","table":"column_lifecycle","column":"note"},
        {"op":"setColumnDefault","table":"column_lifecycle","column":"note",
         "value":{"literal":{"value":"memo"}}},
        {"op":"dropColumnDefault","table":"column_lifecycle","column":"note"},
        {"op":"setColumnType","table":"column_lifecycle","column":"amount",
         "toType":"bigInt"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "add and alter columns",
        source,
        &[
            ("create table", 1),
            ("add column", 2),
            ("set column not null", 3),
            ("drop column not null", 4),
            ("set column default", 5),
            ("drop column default", 6),
            ("set column type", 7),
        ],
    )
    .await;
}

#[compio::test]
async fn standalone_constraint_lifecycle() {
    // Exercise standalone constraint stages. This does not cover NOT VALID state.
    let source = r#"{
      "ir_version": 1,
      "name": "standalone_constraint_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"constraint_parent","columns":[
          {"name":"id","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"createTable","name":"constraint_child","columns":[
          {"name":"code","type":"text","nullable":false},
          {"name":"quantity","type":"int","nullable":false},
          {"name":"parent_id","type":"int","nullable":false}
        ]},
        {"op":"addConstraint","table":"constraint_child","constraint":{
          "name":"constraint_child_code_key",
          "kind":{"kind":"unique","columns":["code"]}
        }},
        {"op":"addConstraint","table":"constraint_child","constraint":{
          "name":"constraint_child_quantity_check",
          "kind":{"kind":"check","expr":{
            "node":"binOp","op":"gt",
            "lhs":{"node":"colRef","name":"quantity"},
            "rhs":{"node":"literal","value":0}
          }}
        }},
        {"op":"addConstraint","table":"constraint_child","constraint":{
          "name":"constraint_child_parent_fkey",
          "kind":{"kind":"fk","columns":["parent_id"],
            "referencesTable":"constraint_parent","referencesColumns":["id"]}
        }},
        {"op":"dropConstraint","table":"constraint_child",
         "name":"constraint_child_code_key"},
        {"op":"dropConstraint","table":"constraint_child",
         "name":"constraint_child_quantity_check"},
        {"op":"dropConstraint","table":"constraint_child",
         "name":"constraint_child_parent_fkey"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "standalone constraint lifecycle",
        source,
        &[
            ("create constraint tables", 2),
            ("add unique constraint", 3),
            ("add check constraint", 4),
            ("add foreign-key constraint", 5),
            ("drop unique constraint", 6),
            ("drop check constraint", 7),
            ("drop foreign-key constraint", 8),
        ],
    )
    .await;
}

#[compio::test]
async fn named_type_lifecycle() {
    // Exercise named types and their column uses. This does not cover type attributes.
    let source = r#"{
      "ir_version": 1,
      "name": "named_type_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createEnum","name":"account_state",
         "values":["active","disabled"]},
        {"op":"createDomain","name":"positive_number","as":"int"},
        {"op":"createTable","name":"typed_rows","columns":[
          {"name":"state","type":{"enum":{"name":"account_state"}},
           "nullable":false},
          {"name":"amount","type":{"domain":{"name":"positive_number"}},
           "nullable":true}
        ]},
        {"op":"dropTable","table":"typed_rows"},
        {"op":"dropDomain","name":"positive_number"},
        {"op":"dropEnum","name":"account_state"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "named type lifecycle",
        source,
        &[
            ("create enum and domain", 2),
            ("create table using named types", 3),
            ("drop table using named types", 4),
            ("drop domain", 5),
            ("drop enum", 6),
        ],
    )
    .await;
}

#[compio::test]
async fn sequence_lifecycle() {
    // Exercise catalog-visible sequence attributes. This does not cover restart state.
    let source = r#"{
      "ir_version": 1,
      "name": "sequence_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createSequence","name":"roundtrip_seq","as":"int",
         "increment":5,"start":100,"minValue":10,"maxValue":1000,
         "cache":7,"cycle":true},
        {"op":"alterSequence","name":"roundtrip_seq","increment":9,
         "minValue":20,"maxValue":2000,"cache":11,"cycle":false},
        {"op":"dropSequence","name":"roundtrip_seq"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "sequence lifecycle",
        source,
        &[
            ("create sequence", 1),
            ("alter sequence", 2),
            ("drop sequence", 3),
        ],
    )
    .await;
}
