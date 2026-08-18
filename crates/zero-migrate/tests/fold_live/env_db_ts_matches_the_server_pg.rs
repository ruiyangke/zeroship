//! **The oracle that adjudicates step 4 consumer 2, live.**
//!
//! `docs/proposals/single-fold-and-effects.md` section G step 4 moves `env.db.ts` off
//! `authoring_tables_from_ops` and onto `FoldedSchema::project_authoring_tables`. Two
//! ops answer differently across that move - `alterPrimaryKey`, which the walker had
//! no arm for at all, and `dropPartition`, which the walker also ignored - and a
//! differential gate can only say THAT they differ. Which side is right is a question
//! about a database, so it is asked of one.
//!
//! Each test here APPLIES the migration to a real PostgreSQL through the shipped
//! `MigrationEngine::apply_plan`, reads the answer out of `pg_catalog`, and then
//! asserts that `env.db.ts` - rendered from the SAME resolved ops through the real
//! `render_artifacts` entry point - declares what the server holds. The artifact is
//! what an app is regenerated from, so an artifact that disagrees with the server is
//! an instruction to rebuild a database that does not exist.
//!
//! This is deliberately NOT a comparison against `fold_ops`. `fold_ops` is the
//! offline model whose own agreement with the server these tests would then be
//! inheriting; running the DDL and reading the catalog costs one throwaway schema per
//! case and removes the inheritance.
//!
//! Every case asserts the SERVER's answer first and fails as a setup error if it is
//! not the expected one, so a case whose DDL silently did nothing reports that rather
//! than reporting an artifact defect.
//!
//! Gated on `ZERO_MIGRATE_TEST_PG_URL` and skips cleanly without it - so read the
//! SKIP banner before reading the pass count, because a skipped run of this file
//! proves nothing at all. The offline halves of these same claims live in
//! `tests/gen_types_authoring_tables_from_the_fold.rs` and always run.

use crate::support;

use std::collections::{BTreeMap, BTreeSet};

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    effective_policy_from_charter_toml, render_artifacts, resolve_create_table_policy, Approval,
    EffectivePolicy, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine,
    MigrationIr, PostgresBackend, SqlDialect,
};

const OWNER: &str = "app_env_db_ts_server";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "env_db_ts_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn cfg_for(schema: &str, policy: &EffectivePolicy) -> ExecutorConfig {
    ExecutorConfig::new(format!("project_{schema}"), schema, policy.clone())
}

/// Which charter a case folds and lowers under.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Charter {
    /// The ordinary no-injection charter the primary-key cases use.
    Plain,
    /// The same, plus `schema.partition`. A partition op is a vendor primitive the
    /// plain charter denies at lowering (`VENDOR_OP_DENIED … "partition"`), so the
    /// two partition cases would fail before reaching the server without it.
    Partition,
}

impl Charter {
    fn policy(self, schema: &str) -> EffectivePolicy {
        match self {
            Charter::Plain => support::no_inject(schema),
            Charter::Partition => {
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
                    .expect("the partition test charter composes")
            }
        }
    }
}

/// What the server holds after the migration, plus the artifact rendered from the
/// same ops. Collected inside the throwaway schema's lifetime and asserted after it
/// is dropped, so an assertion failure cannot leak a schema.
struct Measured {
    /// Every ordinary and partitioned relation in the project schema.
    relations: BTreeSet<String>,
    /// The ORDERED primary-key tuple per table that has one. A table with no primary
    /// key is absent rather than present-and-empty.
    primary_keys: BTreeMap<String, Vec<String>>,
    /// `(table, column)` for every column the server still generates values for.
    identity_columns: BTreeSet<(String, String)>,
    /// What an app would be regenerated from.
    env_db_ts: String,
}

impl Measured {
    /// The `env.db.ts` line for one column, which is where a single-column primary
    /// key and the identity facet both render.
    #[track_caller]
    fn column_line(&self, column: &str) -> &str {
        self.env_db_ts
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with(&format!("{column}: t.")))
            .unwrap_or_else(|| {
                panic!(
                    "`env.db.ts` carries no column line for `{column}`:\n{}",
                    self.env_db_ts
                )
            })
    }
}

/// The ORDERED primary-key tuple per table, read from `pg_index.indkey`.
///
/// Not from `information_schema`: the ORDER of the tuple is load-bearing here, and
/// `information_schema.key_column_usage` orders by the column's position in the
/// table rather than by its position in the key.
async fn live_primary_keys(
    session: &PgDevSession,
    schema: &str,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let rows = session
        .query(
            "SELECT rel.relname::text AS table_name, att.attname::text AS column_name \
             FROM pg_catalog.pg_index idx \
             JOIN pg_catalog.pg_class rel ON rel.oid = idx.indrelid \
             JOIN pg_catalog.pg_namespace ns ON ns.oid = rel.relnamespace \
             JOIN LATERAL unnest(idx.indkey) WITH ORDINALITY AS k(attnum, key_position) \
               ON TRUE \
             JOIN pg_catalog.pg_attribute att \
               ON att.attrelid = rel.oid AND att.attnum = k.attnum \
             WHERE idx.indisprimary AND ns.nspname = $1 \
             ORDER BY rel.relname, k.key_position",
            &[schema.into()],
        )
        .await
        .map_err(|error| format!("read the live primary keys: {error}"))?;
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in &rows {
        let table: String = row
            .try_get("table_name")
            .map_err(|error| format!("decode a primary-key table name: {error}"))?;
        let column: String = row
            .try_get("column_name")
            .map_err(|error| format!("decode a primary-key column name: {error}"))?;
        out.entry(table).or_default().push(column);
    }
    Ok(out)
}

/// Every column the server still generates values for.
async fn live_identity_columns(
    session: &PgDevSession,
    schema: &str,
) -> Result<BTreeSet<(String, String)>, String> {
    let rows = session
        .query(
            "SELECT rel.relname::text AS table_name, att.attname::text AS column_name \
             FROM pg_catalog.pg_attribute att \
             JOIN pg_catalog.pg_class rel ON rel.oid = att.attrelid \
             JOIN pg_catalog.pg_namespace ns ON ns.oid = rel.relnamespace \
             WHERE ns.nspname = $1 AND att.attidentity <> '' AND att.attnum > 0 \
             ORDER BY rel.relname, att.attname",
            &[schema.into()],
        )
        .await
        .map_err(|error| format!("read the live identity columns: {error}"))?;
    let mut out = BTreeSet::new();
    for row in &rows {
        let table: String = row
            .try_get("table_name")
            .map_err(|error| format!("decode an identity table name: {error}"))?;
        let column: String = row
            .try_get("column_name")
            .map_err(|error| format!("decode an identity column name: {error}"))?;
        out.insert((table, column));
    }
    Ok(out)
}

/// Every relation name the project schema holds, partition children included.
async fn live_relations(session: &PgDevSession, schema: &str) -> Result<BTreeSet<String>, String> {
    let rows = session
        .query(
            "SELECT rel.relname::text AS name \
             FROM pg_catalog.pg_class rel \
             JOIN pg_catalog.pg_namespace ns ON ns.oid = rel.relnamespace \
             WHERE ns.nspname = $1 AND rel.relkind IN ('r', 'p') \
             ORDER BY rel.relname",
            &[schema.into()],
        )
        .await
        .map_err(|error| format!("read the live relation list: {error}"))?;
    let mut out = BTreeSet::new();
    for row in &rows {
        let name: String = row
            .try_get("name")
            .map_err(|error| format!("decode a relation name: {error}"))?;
        out.insert(name);
    }
    Ok(out)
}

async fn apply_ir(
    session: &PgDevSession,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    source: &str,
) -> Result<MigrationIr, String> {
    let backend = PostgresBackend::new_generic(session);
    backend
        .ensure_journal(cfg)
        .await
        .map_err(|error| format!("ensure migration journal: {error}"))?;

    let policy = policy.clone();
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
            "env-db-ts-pg",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply IR plan: {error}"))?;

    Ok(resolved)
}

/// Apply one IR to a throwaway schema, measure the server, render the artifact, drop
/// the schema, and hand both halves back.
///
/// The schema is dropped on every exit path, including an unwind, and the drop's own
/// failure is reported rather than swallowed.
async fn measure(label: &str, charter: Charter, ops: &str) -> Measured {
    let Some(session_url) = support::pg_url() else {
        unreachable!("callers gate on `skip_if_no_pg!` before reaching here")
    };
    let session = PgDevSession::connect(&session_url);
    let schema = token();
    let policy = charter.policy(&schema);
    let cfg = cfg_for(&schema, &policy);
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.pg.meta_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated env.db.ts test schema");

    let work: Result<Measured, String> = async {
        let ir = apply_ir(&session, &cfg, &policy, ops).await?;
        let relations = live_relations(&session, &cfg.project_schema).await?;
        let primary_keys = live_primary_keys(&session, &cfg.project_schema).await?;
        let identity_columns = live_identity_columns(&session, &cfg.project_schema).await?;
        // The SAME resolved ops and the SAME charter the server just applied, through
        // the real artifact entry point.
        let env_db_ts =
            render_artifacts(&ir.ops, SqlDialect::Postgres, &cfg.project_schema, &policy)
                .map_err(|error| format!("render artifacts for the applied ops: {error}"))?
                .env_db_ts;
        Ok(Measured {
            relations,
            primary_keys,
            identity_columns,
            env_db_ts,
        })
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(measured), Ok(())) => measured,
        (Err(work), Ok(())) => panic!("{label}: {work}"),
        (Ok(_), Err(cleanup)) => panic!("{label}: drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{label}: {work}; cleanup failed: {cleanup}"),
    }
}

const REPLACE_SINGLE: &str = r#"{"ir_version":1,"name":"pk_replace_single","ops":[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["legacy_id"]}}
]}"#;

/// **The adjudication.** After the replace the server holds the key on `legacy_id`,
/// so that is the key `env.db.ts` must declare.
#[compio::test]
async fn a_replaced_primary_key_is_the_key_env_db_ts_declares() {
    let _url = skip_if_no_pg!();
    let measured = measure(
        "replace a single-column primary key",
        Charter::Plain,
        REPLACE_SINGLE,
    )
    .await;

    // THE ORACLE. Asserted first, so a case whose DDL silently did nothing reports a
    // setup failure rather than an artifact defect.
    assert_eq!(
        measured.primary_keys.get("orders").map(Vec::as_slice),
        Some(["legacy_id".to_string()].as_slice()),
        "the SERVER must hold the replaced key, or this test adjudicates nothing"
    );

    assert!(
        measured.column_line("legacy_id").contains(".primaryKey()"),
        "`env.db.ts` must declare the key the server holds:\n{}",
        measured.env_db_ts
    );
    assert!(
        !measured.column_line("id").contains(".primaryKey()"),
        "`env.db.ts` still declares the REPLACED key on `id`, so regenerating an app \
         from it reinstates a key the migration removed:\n{}",
        measured.env_db_ts
    );
}

const REPLACE_COMPOSITE: &str = r#"{"ir_version":1,"name":"pk_replace_composite","ops":[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"tenant","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false}],"primaryKey":["id"],"constraints":[{"name":"orders_pair_uq","kind":{"kind":"unique","columns":["tenant","legacy_id"]}}]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["tenant","legacy_id"]}}
]}"#;

/// A composite key renders through a different branch of `render_table`, and its
/// ORDER is load-bearing, so the server's ordered tuple is compared to the artifact's
/// ordered array rather than to a set.
#[compio::test]
async fn a_replaced_composite_primary_key_keeps_the_servers_column_order() {
    let _url = skip_if_no_pg!();
    let measured = measure(
        "replace with a composite primary key",
        Charter::Plain,
        REPLACE_COMPOSITE,
    )
    .await;

    assert_eq!(
        measured.primary_keys.get("orders").map(Vec::as_slice),
        Some(["tenant".to_string(), "legacy_id".to_string()].as_slice()),
        "the SERVER must hold the composite key in this order"
    );
    assert!(
        measured
            .env_db_ts
            .contains("primaryKey: [\"tenant\", \"legacy_id\"]"),
        "`env.db.ts` must declare the composite key in the server's order:\n{}",
        measured.env_db_ts
    );
}

const DROP_KEY: &str = r#"{"ir_version":1,"name":"pk_drop","ops":[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"drop","expectedColumns":["id"]}}
]}"#;

/// After a `drop` the server holds NO primary key, and the artifact's spelling for
/// that is `primaryKey: null`.
#[compio::test]
async fn a_dropped_primary_key_leaves_env_db_ts_declaring_none() {
    let _url = skip_if_no_pg!();
    let measured = measure("drop the primary key", Charter::Plain, DROP_KEY).await;

    assert_eq!(
        measured.primary_keys.get("orders"),
        None,
        "the SERVER must have no primary key after the drop"
    );
    assert!(
        measured.env_db_ts.contains("primaryKey: null"),
        "`env.db.ts` must declare no key:\n{}",
        measured.env_db_ts
    );
    assert!(
        !measured.column_line("id").contains(".primaryKey()"),
        "`env.db.ts` still marks `id` as the key the drop removed:\n{}",
        measured.env_db_ts
    );
}

const ADD_KEY: &str = r#"{"ir_version":1,"name":"pk_add","ops":[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"text","nullable":false},{"name":"legacy_id","type":"text","nullable":false,"unique":true}]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"add","columns":["legacy_id"]}}
]}"#;

/// The mirror direction: a table that had no key gets one, and an artifact still
/// saying `primaryKey: null` would drop a constraint the migration installed.
#[compio::test]
async fn an_added_primary_key_reaches_env_db_ts() {
    let _url = skip_if_no_pg!();
    let measured = measure("add a primary key", Charter::Plain, ADD_KEY).await;

    assert_eq!(
        measured.primary_keys.get("orders").map(Vec::as_slice),
        Some(["legacy_id".to_string()].as_slice()),
        "the SERVER must hold the added key"
    );
    assert!(
        !measured.env_db_ts.contains("primaryKey: null"),
        "`env.db.ts` still says the table has no key:\n{}",
        measured.env_db_ts
    );
    assert!(
        measured.column_line("legacy_id").contains(".primaryKey()"),
        "`env.db.ts` must mark the column the server made the key:\n{}",
        measured.env_db_ts
    );
}

const REPLACE_DROPS_IDENTITY: &str = r#"{"ir_version":1,"name":"pk_replace_identity","ops":[
  {"op":"createTable","name":"orders","columns":[{"name":"id","type":"int","nullable":false,"identity":{"always":false}},{"name":"legacy_id","type":"text","nullable":false,"unique":true}],"primaryKey":["id"]},
  {"op":"alterPrimaryKey","table":"orders","action":{"kind":"replace","expectedColumns":["id"],"columns":["legacy_id"],"dropIdentityFrom":["id"]}}
]}"#;

/// The same op clears an identity facet, and `env.db.ts` spells identity
/// `.autoIncrement()`. Both halves are read off the server: the key it holds, and
/// whether `id` still generates values.
#[compio::test]
async fn the_identity_a_primary_key_replace_drops_leaves_env_db_ts_too() {
    let _url = skip_if_no_pg!();
    let measured = measure(
        "replace a primary key and drop the old identity",
        Charter::Plain,
        REPLACE_DROPS_IDENTITY,
    )
    .await;

    assert_eq!(
        measured.primary_keys.get("orders").map(Vec::as_slice),
        Some(["legacy_id".to_string()].as_slice()),
        "the SERVER must hold the replaced key"
    );
    assert!(
        !measured
            .identity_columns
            .contains(&("orders".to_string(), "id".to_string())),
        "the SERVER must have dropped the identity, or this case adjudicates nothing \
         about it: {:?}",
        measured.identity_columns
    );
    assert!(
        !measured.column_line("id").contains(".autoIncrement()"),
        "`env.db.ts` still generates values for a column the server no longer does:\n{}",
        measured.env_db_ts
    );
    assert!(
        measured.column_line("legacy_id").contains(".primaryKey()"),
        "and the key moved in the same op:\n{}",
        measured.env_db_ts
    );
}

const DROP_ATTACHED_PARTITION: &str = r#"{"ir_version":1,"name":"partition_drop","ops":[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}],"partitionBy":{"kind":"range","columns":["bucket"]}},
  {"op":"createTable","name":"p1","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}]},
  {"op":"attachPartition","parent":"par","name":"p1","bound":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":100}]}},
  {"op":"dropPartition","parent":"par","name":"p1"}
]}"#;

const DETACH_ATTACHED_PARTITION: &str = r#"{"ir_version":1,"name":"partition_detach","ops":[
  {"op":"createTable","name":"par","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}],"partitionBy":{"kind":"range","columns":["bucket"]}},
  {"op":"createTable","name":"p1","columns":[{"name":"bucket","type":"int","nullable":false},{"name":"payload","type":"text"}]},
  {"op":"attachPartition","parent":"par","name":"p1","bound":{"kind":"range","from":[{"kind":"int","value":0}],"to":[{"kind":"int","value":100}]}},
  {"op":"detachPartition","parent":"par","name":"p1"}
]}"#;

/// **The second adjudication.** `AuthoredState::advance` removes a table on
/// `dropPartition` and the deleted walker did not. The server decides: after the
/// drop, is there still a relation called `p1`?
#[compio::test]
async fn a_dropped_partition_is_gone_from_the_server_and_from_env_db_ts() {
    let _url = skip_if_no_pg!();
    let measured = measure(
        "drop an attached partition",
        Charter::Partition,
        DROP_ATTACHED_PARTITION,
    )
    .await;

    assert!(
        !measured.relations.contains("p1"),
        "the SERVER must no longer hold `p1`, or this case adjudicates nothing: {:?}",
        measured.relations
    );
    assert!(
        measured.relations.contains("par"),
        "the parent must survive its child, or the case proves only that everything \
         went away: {:?}",
        measured.relations
    );
    assert!(
        !measured.env_db_ts.contains("p1: {"),
        "`env.db.ts` still describes a relation the server does not have:\n{}",
        measured.env_db_ts
    );
    assert!(
        measured.env_db_ts.contains("par: {"),
        "`env.db.ts` lost the parent too:\n{}",
        measured.env_db_ts
    );
}

/// **The control that shapes the arm above.** A DETACHED partition survives as a
/// standalone table under the same name - the rule
/// `tests/partition_claims_the_relation_namespace_pg.rs` enforces offline - so the
/// artifact must keep it. Without this, "a partition op removes the table" would look
/// equally justified and would be wrong for the op next door.
#[compio::test]
async fn a_detached_partition_survives_on_the_server_and_in_env_db_ts() {
    let _url = skip_if_no_pg!();
    let measured = measure(
        "detach an attached partition",
        Charter::Partition,
        DETACH_ATTACHED_PARTITION,
    )
    .await;

    assert!(
        measured.relations.contains("p1"),
        "the SERVER keeps a detached partition as a standalone table: {:?}",
        measured.relations
    );
    assert!(
        measured.env_db_ts.contains("p1: {"),
        "`env.db.ts` must keep the detached table:\n{}",
        measured.env_db_ts
    );
}
