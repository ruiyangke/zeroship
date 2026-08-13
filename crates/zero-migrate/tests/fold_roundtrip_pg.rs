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
//! `IndexSnapshot` equality excludes `opclass`, `nulls_not_distinct` and `only`, so
//! this file does not claim `fold_ops == snapshot_schema`. `only` joined that list
//! because the case below authored it and measured the guaranteed false red this
//! comment used to merely predict.
//!
//! The lifecycle cases cover the index facets equality does compare (a partial
//! predicate, an expression key, an INCLUDE payload, a DESC unique key), catalog
//! comments, a table rename, a structured view, and the constraint facets
//! `pg_get_constraintdef` spells - referential actions, deferrability, and the
//! NOT VALID state a later validateConstraint clears. The last of those found a
//! defect: the fold omitted the ` NOT VALID` tail the catalog renders, so an
//! unvalidated foreign key reported drift for as long as it stayed unvalidated.

mod support;

use std::collections::BTreeMap;

use support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, effective_policy_from_charter_toml, fold_ops, resolve_create_table_policy,
    snapshot_schema, Approval, EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema,
    LockMode, MigrationEngine, MigrationIr, PostgresBackend, SqlDialect, StructuralDrift,
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

fn lifecycle_policy(schema: &str) -> EffectivePolicy {
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
key = "access.role"
value = true
scope = "all"

[[grant]]
key = "schema.partition"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#
    );
    effective_policy_from_charter_toml(&charter_toml)
        .expect("explicit partition lifecycle test charter composes")
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
    // Both schemas, dropped on an unwind that skips the explicit cleanup below.
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
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

/// `policy_for` builds the charter the case runs under, from its schema name.
///
/// Passed per case rather than fixed in the helper so a case that needs a wider
/// grant does not widen it for the others. The partition cases need
/// `schema.partition` and `access.role`, which `attachPartition` refuses without;
/// the column, constraint, named-type and sequence cases run under the narrower
/// `support::no_inject` they were written against.
async fn assert_lifecycle_roundtrip(
    label: &str,
    source: &str,
    checkpoints: &[(&str, usize)],
    policy_for: fn(&str) -> EffectivePolicy,
) {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);
    let schema = token();
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy_for(&schema));
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
        .expect("create isolated fold lifecycle schema");

    let work: Result<(), String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let policy = policy_for(&cfg.project_schema);
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
                    &policy,
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
        support::no_inject,
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
        support::no_inject,
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
        support::no_inject,
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
        support::no_inject,
    )
    .await;
}

#[compio::test]
async fn create_and_drop_partition() {
    // Exercise createPartition before dropPartition removes it. This does not cover
    // existence guards.
    let source = r#"{
      "ir_version": 1,
      "name": "create_and_drop_partition",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"create_drop_parent","columns":[
          {"name":"bucket","type":"int","nullable":false},
          {"name":"payload","type":"text","nullable":false}
        ],"partitionBy":{"kind":"range","columns":["bucket"],"collapse":false}},
        {"op":"createPartition","name":"create_drop_child",
         "of":"create_drop_parent","bounds":{"kind":"range",
           "from":[{"kind":"int","value":0}],
           "to":[{"kind":"int","value":100}]}},
        {"op":"dropPartition","parent":"create_drop_parent",
         "name":"create_drop_child"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "create and drop partition",
        source,
        &[
            ("create partitioned parent", 1),
            ("create partition", 2),
            ("drop partition", 3),
        ],
        lifecycle_policy,
    )
    .await;
}

#[compio::test]
async fn attach_standalone_table_as_partition() {
    // Exercise attachPartition after observing the standalone table. This does not
    // cover validation of rows already stored in the table.
    let source = r#"{
      "ir_version": 1,
      "name": "attach_standalone_table_as_partition",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"attach_parent","columns":[
          {"name":"bucket","type":"int","nullable":false},
          {"name":"payload","type":"text","nullable":false}
        ],"partitionBy":{"kind":"range","columns":["bucket"],"collapse":false}},
        {"op":"createTable","name":"attach_child","columns":[
          {"name":"bucket","type":"int","nullable":false},
          {"name":"payload","type":"text","nullable":false}
        ]},
        {"op":"attachPartition","parent":"attach_parent","name":"attach_child",
         "bound":{"kind":"range",
           "from":[{"kind":"int","value":100}],
           "to":[{"kind":"int","value":200}]}}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "attach standalone table as partition",
        source,
        &[
            ("create partitioned parent", 1),
            ("create standalone table", 2),
            ("attach partition", 3),
        ],
        lifecycle_policy,
    )
    .await;
}

#[compio::test]
async fn detach_partition_lifecycle() {
    // Exercise detachPartition after observing the created partition. This does not
    // cover CONCURRENTLY.
    let source = r#"{
      "ir_version": 1,
      "name": "detach_partition_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"detach_parent","columns":[
          {"name":"bucket","type":"int","nullable":false},
          {"name":"payload","type":"text","nullable":false}
        ],"partitionBy":{"kind":"range","columns":["bucket"],"collapse":false}},
        {"op":"createPartition","name":"detach_child","of":"detach_parent",
         "bounds":{"kind":"range",
           "from":[{"kind":"int","value":200}],
           "to":[{"kind":"int","value":300}]}},
        {"op":"detachPartition","parent":"detach_parent","name":"detach_child"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "detach partition lifecycle",
        source,
        &[
            ("create partitioned parent", 1),
            ("create partition", 2),
            ("detach partition", 3),
        ],
        lifecycle_policy,
    )
    .await;
}

#[compio::test]
async fn index_facet_lifecycle() {
    // Exercise the index facets `diff_snapshots` compares: a partial predicate, an
    // expression key, an INCLUDE payload, and a UNIQUE key with a DESC element. This
    // does not cover `opclass` or `nulls_not_distinct`, which index equality excludes.
    let source = r#"{
      "ir_version": 1,
      "name": "index_facet_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"index_facets","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"a","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true},
          {"name":"tag","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"index_facets","name":"index_facets_partial_key",
         "columns":[{"kind":"column","name":"note"}],
         "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"a"},
                  "rhs":{"node":"literal","value":0}}},
        {"op":"createIndex","table":"index_facets","name":"index_facets_expr_key",
         "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
           "lhs":{"node":"colRef","name":"a"},"rhs":{"node":"literal","value":1}}}]},
        {"op":"createIndex","table":"index_facets","name":"index_facets_include_key",
         "columns":[{"kind":"column","name":"note"}],"include":["tag"]},
        {"op":"createIndex","table":"index_facets","name":"index_facets_desc_key",
         "columns":[{"kind":"column","name":"tag","order":"desc"}],"unique":true},
        {"op":"dropIndex","table":"index_facets","name":"index_facets_partial_key"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "index facet lifecycle",
        source,
        &[
            ("create table", 1),
            ("create partial index", 2),
            ("create expression index", 3),
            ("create covering index", 4),
            ("create descending unique index", 5),
            ("drop partial index", 6),
        ],
        support::no_inject,
    )
    .await;
}

#[compio::test]
async fn comment_lifecycle() {
    // Exercise the catalog comments `TableSnapshot` and `IndexSnapshot` compare, and
    // the clearing form that renders `IS NULL`. This does not cover constraint or
    // view comments, which structural equality does not carry.
    let source = r#"{
      "ir_version": 1,
      "name": "comment_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"commented","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"commented","name":"commented_note_key",
         "columns":[{"kind":"column","name":"note"}]},
        {"op":"comment","target":{"kind":"table","name":"commented"},
         "comment":"rows a reader may annotate"},
        {"op":"comment","target":{"kind":"column","table":"commented","name":"note"},
         "comment":"free-form note"},
        {"op":"comment","target":{"kind":"index","name":"commented_note_key"},
         "comment":"lookup by note"},
        {"op":"comment","target":{"kind":"table","name":"commented"}}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "comment lifecycle",
        source,
        &[
            ("create table", 1),
            ("create index", 2),
            ("comment on table", 3),
            ("comment on column", 4),
            ("comment on index", 5),
            ("clear the table comment", 6),
        ],
        support::no_inject,
    )
    .await;
}

#[compio::test]
async fn rename_lifecycle() {
    // Exercise renameTable, whose folded result must name the relation PostgreSQL
    // renamed and carry the index that followed it. renameColumn is deliberately
    // absent: it is refused unless it is the only operation targeting its table in a
    // migration, so it cannot share this envelope with the createTable it needs.
    let source = r#"{
      "ir_version": 1,
      "name": "rename_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"rename_source","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"rename_source","name":"rename_source_note_key",
         "columns":[{"kind":"column","name":"note"}]},
        {"op":"renameTable","table":"rename_source","to":"rename_target"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "rename lifecycle",
        source,
        &[
            ("create table", 1),
            ("create index", 2),
            ("rename table", 3),
        ],
        support::no_inject,
    )
    .await;
}

#[compio::test]
async fn view_lifecycle() {
    // Exercise a structured view through create and drop. This does not cover a
    // materialized view or CREATE OR REPLACE over a changed projection.
    let source = r#"{
      "ir_version": 1,
      "name": "view_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"view_rows","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"name","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createView","name":"named_rows",
         "query":{"kind":"structured","select":{"from":{"name":"view_rows"},
           "projection":[{"kind":"colRef","name":"id"},{"kind":"colRef","name":"name"}],
           "where":{"node":"unaryOp","op":"isNotNull",
                    "operand":{"node":"colRef","name":"name"}}}}},
        {"op":"dropView","name":"named_rows"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "view lifecycle",
        source,
        &[("create table", 1), ("create view", 2), ("drop view", 3)],
        support::no_inject,
    )
    .await;
}

/// A trigger and function lifecycle APPLIES cleanly - and this oracle cannot see
/// either object.
///
/// Read the name of this test as a warning, not as coverage. It was written to
/// add triggers to the fold==live oracle for the same reason views and constraint
/// facets are here, and it passes - VACUOUSLY.
///
/// `snapshot_schema` builds the live side with
///
///     functions: BTreeMap::new(),
///     policies:  BTreeMap::new(),
///     triggers:  BTreeMap::new(),
///
/// hardcoded (`apply/drift.rs`), and `diff_snapshots` never compares those three
/// maps. The FOLD does record the trigger (`fold.rs` `triggers.insert`), so if the
/// comparison looked at them this test would fail on the difference. It passes,
/// which is the proof that it does not look.
///
/// So what this scenario actually establishes is narrower than its shape suggests,
/// and both halves are worth having:
///
///   * the APPLY path really does create a function, create a trigger with an
///     ordered event set and a `WHEN` predicate, and drop the trigger, against
///     live PostgreSQL, without error - that part is real;
///   * the ORACLE is blind to triggers, functions and policies, so no scenario
///     added here can ever measure them.
///
/// The second half matters beyond this file: the same `diff_snapshots` powers
/// structural DRIFT DETECTION, so an out-of-band change to a trigger, a function,
/// or an RLS policy is invisible to it. Dropping a trigger by hand produces no
/// drift report.
///
/// If those three maps are ever populated and compared, this test starts failing
/// on a genuine fold-vs-live difference, and that is the moment to turn it into
/// the real round-trip scenario it looks like.
#[compio::test]
async fn trigger_lifecycle_applies_but_is_invisible_to_this_oracle() {
    let source = r#"{
      "ir_version": 1,
      "name": "trigger_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"trigger_rows","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"payload","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createFunction","name":"trigger_rows_audit",
         "returns":"trigger","language":"plpgsql",
         "body":"BEGIN RETURN NEW; END;"},
        {"op":"createTrigger","name":"trigger_rows_audit_trg","table":"trigger_rows",
         "timing":"after","events":["insert","update"],"forEach":"row",
         "action":{"kind":"executeFunction","name":"trigger_rows_audit"},
         "when":{"node":"unaryOp","op":"isNotNull",
                 "operand":{"node":"colRef","name":"payload","table":"new"}}},
        {"op":"dropTrigger","name":"trigger_rows_audit_trg","table":"trigger_rows"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "trigger lifecycle",
        source,
        &[
            ("create table", 1),
            ("create function", 2),
            ("create trigger", 3),
            ("drop trigger", 4),
        ],
        // `operator_charter` rather than `no_inject`: a function is a charter-gated
        // vendor primitive, and under the default charter this scenario is refused
        // at policy load with VENDOR_OP_DENIED - above the fold, before anything
        // this file measures runs. That is the F457 trap, and it would have made
        // the scenario look covered while never reaching the oracle.
        support::operator_charter,
    )
    .await;
}

#[compio::test]
async fn constraint_facet_lifecycle() {
    // Exercise the constraint facets `pg_get_constraintdef` spells and
    // `ConstraintSnapshot` compares: referential actions, deferrability, and the
    // NOT VALID state a later validateConstraint clears. A CHECK body is exempted
    // from the comparison by the differ, so the NOT VALID leg here is a foreign key,
    // whose whole definition text is compared.
    let source = r#"{
      "ir_version": 1,
      "name": "constraint_facet_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"facet_parent","columns":[
          {"name":"id","type":"int","nullable":false}
        ],"primaryKey":["id"]},
        {"op":"createTable","name":"facet_child","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"cascade_id","type":"int","nullable":true},
          {"name":"deferred_id","type":"int","nullable":true},
          {"name":"adopted_id","type":"int","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"addConstraint","table":"facet_child","constraint":{
          "name":"facet_child_cascade_fkey",
          "kind":{"kind":"fk","columns":["cascade_id"],
            "referencesTable":"facet_parent","referencesColumns":["id"],
            "onDelete":"cascade","onUpdate":"setNull"}
        }},
        {"op":"addConstraint","table":"facet_child","constraint":{
          "name":"facet_child_deferred_fkey",
          "kind":{"kind":"fk","columns":["deferred_id"],
            "referencesTable":"facet_parent","referencesColumns":["id"],
            "deferrable":true,"initiallyDeferred":true}
        }},
        {"op":"addConstraint","table":"facet_child","constraint":{
          "name":"facet_child_adopted_fkey",
          "kind":{"kind":"fk","columns":["adopted_id"],
            "referencesTable":"facet_parent","referencesColumns":["id"],
            "notValid":true}
        }},
        {"op":"validateConstraint","table":"facet_child",
         "name":"facet_child_adopted_fkey"}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "constraint facet lifecycle",
        source,
        &[
            ("create constraint tables", 2),
            ("add foreign key with referential actions", 3),
            ("add deferrable foreign key", 4),
            ("add NOT VALID foreign key", 5),
            ("validate the adopted foreign key", 6),
        ],
        support::no_inject,
    )
    .await;
}

#[compio::test]
async fn identity_and_primary_key_replacement_lifecycle() {
    // Exercise the two column facets `ColumnSnapshot` equality carries that no other
    // lifecycle case reaches: an IDENTITY column, and the removal of that facet as
    // part of the primary-key replacement that retires it. A plain column default is
    // deliberately not asserted here - equality does not compare one.
    let source = r#"{
      "ir_version": 1,
      "name": "identity_and_primary_key_replacement_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"id_lifecycle","columns":[
          {"name":"legacy_id","type":"int","nullable":false,
           "identity":{"always":false}},
          {"name":"public_id","type":"text","nullable":false}
        ],"primaryKey":["legacy_id"]},
        {"op":"addConstraint","table":"id_lifecycle","constraint":{
          "name":"id_lifecycle_public_id_key",
          "kind":{"kind":"unique","columns":["public_id"]}
        }},
        {"op":"alterPrimaryKey","table":"id_lifecycle","action":{
          "kind":"replace",
          "expectedColumns":["legacy_id"],
          "columns":["public_id"],
          "dropIdentityFrom":["legacy_id"]
        }}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "identity and primary key replacement lifecycle",
        source,
        &[
            ("create table with an identity column", 1),
            ("add the candidate unique key", 2),
            ("replace the primary key and retire the identity", 3),
        ],
        support::no_inject,
    )
    .await;
}

#[compio::test]
async fn partitioned_parent_index_only_lifecycle() {
    // Authoring `only: true` used to be a guaranteed false red: introspection reports
    // `only: false` for every index, and equality compared the field. The module doc
    // predicted that and nothing pinned it. This case authors the shape against the
    // live server, so the exclusion that fixed it cannot be reverted silently.
    let source = r#"{
      "ir_version": 1,
      "name": "partitioned_parent_index_only_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"only_parent","columns":[
          {"name":"bucket","type":"int","nullable":false},
          {"name":"payload","type":"text","nullable":false}
        ],"partitionBy":{"kind":"range","columns":["bucket"],"collapse":false}},
        {"op":"createIndex","table":"only_parent","name":"only_parent_payload_idx",
         "columns":[{"kind":"column","name":"payload"}],"only":true}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "partitioned parent index only lifecycle",
        source,
        &[
            ("create partitioned parent", 1),
            ("create index on only the parent", 2),
        ],
        lifecycle_policy,
    )
    .await;
}

#[compio::test]
async fn index_storage_parameter_lifecycle() {
    // `IndexSnapshot.with` is compared by both the attribute pass and index equality,
    // and PostgreSQL stores it in `pg_class.reloptions` as text that has to parse back
    // into the same typed pair. Nothing authored a storage parameter, so nothing had
    // ever checked that round trip against a live server. BRIN carries
    // `pages_per_range`; the B-tree carries `fillfactor`.
    let source = r#"{
      "ir_version": 1,
      "name": "index_storage_parameter_lifecycle",
      "owner_app": "app_fold_roundtrip_pg",
      "ops": [
        {"op":"createTable","name":"storage_params","columns":[
          {"name":"id","type":"int","nullable":false},
          {"name":"bucket","type":"int","nullable":true},
          {"name":"note","type":"text","nullable":true}
        ],"primaryKey":["id"]},
        {"op":"createIndex","table":"storage_params","name":"storage_params_note_idx",
         "columns":[{"kind":"column","name":"note"}],
         "with":{"fillfactor":70}},
        {"op":"createIndex","table":"storage_params","name":"storage_params_brin_idx",
         "columns":[{"kind":"column","name":"bucket"}],"using":"brin",
         "with":{"pagesPerRange":32}}
      ]
    }"#;

    assert_lifecycle_roundtrip(
        "index storage parameter lifecycle",
        source,
        &[
            ("create table", 1),
            ("create index with a fillfactor", 2),
            ("create a BRIN index with pages_per_range", 3),
        ],
        support::no_inject,
    )
    .await;
}
