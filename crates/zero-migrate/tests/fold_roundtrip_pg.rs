//! Live PostgreSQL round-trip checks for three `fold_ops` primary-key name cases.
//!
//! Each test applies a validated IR plan through `MigrationEngine::apply_plan`,
//! verifies the resulting primary-key catalog state, folds the same resolved ops
//! offline, and checks the folded snapshot against live introspection with
//! `diff_snapshots(...).is_clean()`. The first two checks intentionally stay red
//! until the fold models PostgreSQL's implicit-name uniquification. The third is
//! the positive control for adopting a standalone candidate index.
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
