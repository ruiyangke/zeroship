//! **A retype must leave the folded MySQL physical contract describing the NEW type.**
//!
//! On MySQL the drift comparator does not consult `data_type` for a column that
//! carries a physical contract. `apply::drift::column_data_types_eq` says so in its
//! own words: "A MySQL physical contract, when BOTH sides carry one, is the
//! authority. The portable `data_type` cannot be: `mysql_canonical_type` folds every
//! `varchar(n)` to the literal `text`, so a declared 255 and a live 64 are the same
//! string here."
//!
//! So a fold that leaves `ColumnSnapshot::mysql_physical_type` describing a type the
//! column no longer has does not produce a cosmetically stale field: it produces a
//! snapshot the differ then compares against the live column and REPORTS A DIFFERENCE
//! THAT IS NOT THERE. The deploy succeeded, the database is exactly what was asked
//! for, and structural drift says otherwise forever after.
//!
//! Everything here goes through the real pipeline - `load_and_lower_guarded` +
//! `MigrationEngine::apply_plan` over `MysqlBackend`, then the shipped
//! `snapshot_schema` - and the ORACLE is always the server, never a literal written
//! in this file. Three tests, each covering a different half of
//! `render::fold::restamp_mysql_physical_types`:
//!
//! 1. `a_narrowing_retype_folds_the_contract_mysql_reports_for_the_target` - the
//!    retype the field was named for. It also pins the fact that a `setColumnType`
//!    CANNOT be applied on MySQL at all, which is why its oracle is a separately
//!    deployed column rather than a drift report.
//! 2. `folded_column_shapes_describe_the_physical_type_mysql_holds` - the reachable
//!    half. `createTable` and `addColumn` DO deploy on MySQL, and a drift report
//!    against the server is the assertion.
//! 3. `folding_onto_a_live_mysql_base_keeps_the_contracts_the_server_reported` - the
//!    half a from-empty fold cannot reach, where re-deriving too eagerly would
//!    DESTROY information the server supplied.
//!
//! **Width is the case only this field can see.** `string(255) -> string(64)` folds to
//! the same `data_type` on both sides, so a test written against `data_type` would
//! pass against the defect. The family and precision cases are kept as well, because
//! the same code is responsible for them and a fix that special-cased lengths would
//! pass the width case alone.
//!
//! Gated on `ZERO_MIGRATE_MYSQL_URL` through `skip_if_no_mysql!`, which routes into
//! `announce_live_db_skip`: `ZERO_MIGRATE_REQUIRE_LIVE_DB=1` turns a missing DSN into
//! a failure rather than a green run with no coverage.

use crate::support;

use std::collections::BTreeMap;

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::apply::backend::{MigrationBackend, MysqlBackend};
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    diff_snapshots, fold_ops, fold_ops_onto, model::ir::Op, resolve_create_table_policy, Approval,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr,
    SqlDialect,
};

const OWNER: &str = "app_fold_retype_physical_type_mysql";

fn cfg_for(database: &str) -> ExecutorConfig {
    ExecutorConfig::new(
        format!("project_{database}"),
        database,
        support::no_inject(database),
    )
}

fn registry(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(table, owner)| ((*table).to_string(), (*owner).to_string()))
        .collect()
}

/// Apply one IR doc through the REAL MySQL pipeline, returning the RESOLVED ops so
/// the caller accumulates the exact stream the fold replays.
async fn apply_doc(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    source: &str,
    registry: &BTreeMap<String, String>,
    live: &LiveSchema,
) -> Result<Vec<Op>, String> {
    let policy = support::no_inject(&cfg.project_schema);
    let authored: MigrationIr =
        serde_json::from_str(source).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    let resolved_source = serde_json::to_string(&resolved)
        .map_err(|error| format!("serialize resolved test IR: {error}"))?;
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Mysql, &policy);
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Mysql);
    let artifact = author
        .load_and_lower_guarded(&resolved_source, OWNER, registry, live, &guard)
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;

    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &MysqlBackend::new_generic(session),
            cfg,
            "fold-retype-physical-type-mysql",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply IR plan: {error}"))?;

    Ok(resolved.ops)
}

/// Fold everything applied so far and require structural drift against the live
/// server to be CLEAN.
///
/// The failure text carries the live `COLUMN_TYPE` alongside the drift dump, so a RED
/// run states what the database actually holds rather than only that two structs
/// disagree.
async fn assert_no_drift(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    ops: &[Op],
    table: &str,
    stage: &str,
) -> Result<(), String> {
    let expected = fold_ops(
        ops,
        SqlDialect::Mysql,
        &cfg.project_schema,
        &support::no_inject(&cfg.project_schema),
    )
    .map_err(|error| format!("{stage}: fold the op stream offline: {error}"))?;
    let actual = MysqlBackend::new_generic(session)
        .snapshot_schema(cfg)
        .await
        .map_err(|error| format!("{stage}: snapshot the live MySQL schema: {error}"))?;
    let drift = diff_snapshots(&expected, &actual);
    if drift.is_clean() {
        return Ok(());
    }
    let live_types = live_column_types(session, cfg, table).await?;
    let folded = physical_types(&expected, table);
    let introspected = physical_types(&actual, table);
    Err(format!(
        "{stage}: the deploy succeeded and the server holds exactly what was asked \
         for ({live_types}), but structural drift reported a difference: {drift:#?}\n  \
         folded:\n    {folded}\n  introspected:\n    {introspected}"
    ))
}

/// `data_type` / `mysql_physical_type` per column of one table, as readable lines.
fn physical_types(snapshot: &zero_migrate::SchemaSnapshot, table: &str) -> String {
    snapshot
        .tables
        .iter()
        .find(|(name, _)| name.as_str() == table || name.ends_with(&format!(".{table}")))
        .map(|(_, t)| {
            t.columns
                .iter()
                .map(|c| format!("{} {:?} / {:?}", c.name, c.data_type, c.mysql_physical_type))
                .collect::<Vec<_>>()
                .join("\n    ")
        })
        .unwrap_or_default()
}

/// `information_schema.COLUMNS.COLUMN_TYPE` for one table, as a readable line.
async fn live_column_types(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    table: &str,
) -> Result<String, String> {
    let rows = session
        .query(
            "SELECT COLUMN_NAME, COLUMN_TYPE FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            &[
                zero_migrate::driver::Bind::Text(cfg.project_schema.clone()),
                zero_migrate::driver::Bind::Text(table.to_string()),
            ],
        )
        .await
        .map_err(|error| format!("read live COLUMN_TYPE: {error}"))?;
    let mut out = Vec::new();
    for row in &rows {
        let name: String = row.try_get("COLUMN_NAME").unwrap_or_default();
        let ty: String = row.try_get("COLUMN_TYPE").unwrap_or_default();
        out.push(format!("{name} {ty}"));
    }
    Ok(out.join(", "))
}

/// A `varchar(255) -> varchar(64)` retype folds to the contract MySQL ITSELF reports
/// for a `varchar(64)` column - and the retype never reaches a MySQL server.
///
/// Both halves are the finding, and the second one is why the first is written this
/// way. `render::lower`'s `refuse_mysql_alter_column` refuses EVERY alter-column op
/// on MySQL unconditionally, `setColumnType` included, because the renderers emit
/// PostgreSQL `ALTER COLUMN` syntax and MySQL's `MODIFY COLUMN` needs the whole
/// column definition restated. So a retype cannot be APPLIED here, and no phantom
/// drift line can be produced from one: the plan dies at lower time, before anything
/// is deployed. The first assertion below pins that, so a reader does not conclude
/// from the second one that the retype path is live on MySQL.
///
/// The folded contract still has to be right, because `fold_ops` is public API and
/// the fold is the expected side of every structural comparison. The ORACLE is the
/// server rather than a literal: a second table is deployed carrying a `varchar(64)`
/// column outright, introspected through the shipped `snapshot_schema`, and the
/// contract MySQL reports for it is what the retype's fold must produce. A literal
/// `Character { fixed: false, length: 64 }` in this file would be a second spelling
/// of the derivation under test.
///
/// WIDTH is the case chosen deliberately: `mysql_canonical_type` folds every
/// `varchar(n)` to the literal `text`, so 255 and 64 are the same `data_type` string
/// and nothing but the physical contract can tell them apart.
#[compio::test]
async fn a_narrowing_retype_folds_the_contract_mysql_reports_for_the_target() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("retypew");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated retype database");

    let result: Result<(), String> = async {
        // The ORACLE, deployed for real: a column DECLARED `string(64)`, so the
        // contract compared against below is MySQL's own answer and not this file's
        // opinion.
        let oracle = r#"{"ir_version":1,"name":"create_oracle","ops":[
            {"op":"createTable","name":"oracle","columns":[
                {"name":"id","type":"int","nullable":false},
                {"name":"label","type":{"string":{"length":64}},"nullable":false}
            ],
            "primaryKey":["id"]}
        ]}"#;
        let mut all_ops: Vec<Op> = apply_doc(
            &session,
            &cfg,
            oracle,
            &registry(&[]),
            &LiveSchema::default(),
        )
        .await?;
        let live_snapshot = MysqlBackend::new_generic(&session)
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot the oracle table: {error}"))?;
        let target_contract = column_contract(&live_snapshot, "oracle", "label")?;

        let create = r#"{"ir_version":1,"name":"create_widths","ops":[
            {"op":"createTable","name":"widths","columns":[
                {"name":"id","type":"int","nullable":false},
                {"name":"label","type":{"string":{"length":255}},"nullable":false}
            ],
            "primaryKey":["id"]}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                create,
                &registry(&[]),
                &LiveSchema::from_tables(["oracle".to_string()].into_iter().collect()),
            )
            .await?,
        );
        // The CONTROL, and it is doing work rather than decorating: `createTable`
        // must already agree with the server, or a disagreement after the retype
        // would say nothing about the retype.
        assert_no_drift(&session, &cfg, &all_ops, "widths", "create table").await?;

        let live = LiveSchema::from_tables(
            ["oracle".to_string(), "widths".to_string()]
                .into_iter()
                .collect(),
        );
        let narrow = r#"{"ir_version":1,"name":"narrow_label","ops":[
            {"op":"setColumnType","table":"widths","column":"label",
             "toType":{"string":{"length":64}}}
        ]}"#;

        // HALF ONE: the op cannot be deployed on MySQL at all.
        let refused = apply_doc(
            &session,
            &cfg,
            narrow,
            &registry(&[("widths", OWNER)]),
            &live,
        )
        .await;
        match refused {
            Ok(_) => {
                return Err(
                    "setColumnType lowered on MySQL; the alter-column refusal this test \
                     documents is gone, and the retype now needs live drift coverage \
                     rather than the fold-side oracle below"
                        .to_string(),
                )
            }
            Err(error) if error.contains("setColumnType is not supported on MySQL") => {}
            Err(error) => {
                return Err(format!(
                    "setColumnType on MySQL failed for an unexpected reason: {error}"
                ))
            }
        }

        // HALF TWO: the fold still has to describe the target correctly.
        let authored: MigrationIr = serde_json::from_str(narrow)
            .map_err(|error| format!("parse the retype IR: {error}"))?;
        let policy = support::no_inject(&cfg.project_schema);
        all_ops.extend(
            resolve_create_table_policy(&authored, &policy, &cfg.project_schema)
                .map_err(|error| format!("resolve the retype IR: {error}"))?
                .ops,
        );
        let folded = fold_ops(&all_ops, SqlDialect::Mysql, &cfg.project_schema, &policy)
            .map_err(|error| format!("fold the retype offline: {error}"))?;
        let folded_contract = column_contract(&folded, "widths", "label")?;
        if folded_contract != target_contract {
            return Err(format!(
                "after string(255) -> string(64) the fold describes widths.label as \
                 {folded_contract:?}, but MySQL reports {target_contract:?} for a column \
                 declared string(64). `data_type` cannot see this: both sides read {:?}.",
                folded
                    .tables
                    .iter()
                    .find(|(name, _)| name.ends_with("widths"))
                    .and_then(|(_, t)| t.columns.iter().find(|c| c.name == "label"))
                    .map(|c| c.data_type.clone())
                    .unwrap_or_default()
            ));
        }
        Ok(())
    }
    .await;

    result.unwrap_or_else(|error| panic!("{error}"));
}

/// One column's `mysql_physical_type` out of a snapshot, or a stated failure.
fn column_contract(
    snapshot: &zero_migrate::SchemaSnapshot,
    table: &str,
    column: &str,
) -> Result<zero_migrate::model::snapshot::MysqlPhysicalType, String> {
    snapshot
        .tables
        .iter()
        .find(|(name, _)| name.as_str() == table || name.ends_with(&format!(".{table}")))
        .and_then(|(_, t)| t.columns.iter().find(|c| c.name == column))
        .ok_or_else(|| format!("no column {table}.{column} in the snapshot"))?
        .mysql_physical_type
        .clone()
        .ok_or_else(|| format!("column {table}.{column} carries no MySQL physical contract"))
}

/// Every column shape whose folded MySQL type is decided AFTER the shared builder
/// stamped the physical contract, applied for real and drift-compared.
#[compio::test]
async fn folded_column_shapes_describe_the_physical_type_mysql_holds() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("shapes");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated shapes database");

    let result: Result<(), String> = async {
        let mut all_ops: Vec<Op> = Vec::new();

        let create = r#"{"ir_version":1,"name":"create_shapes","ops":[
            {"op":"createTable","name":"shapes","columns":[
                {"name":"id","type":"int","nullable":false},
                {"name":"amount","type":{"decimal":{"precision":12,"scale":2}},"nullable":true},
                {"name":"uid","type":"uuid","nullable":true},
                {"name":"ci","type":"text","nullable":true,"caseSensitive":false},
                {"name":"at","type":"timestamp","nullable":true},
                {"name":"raw","type":"bytes","nullable":true},
                {"name":"fixed","type":{"char":{"length":8}},"nullable":true},
                {"name":"flag","type":"boolean","nullable":true}
            ],
            "primaryKey":["id"]}
        ]}"#;
        all_ops.extend(
            apply_doc(
                &session,
                &cfg,
                create,
                &registry(&[]),
                &LiveSchema::default(),
            )
            .await?,
        );
        assert_no_drift(&session, &cfg, &all_ops, "shapes", "create table").await?;

        let live = LiveSchema::from_tables(["shapes".to_string()].into_iter().collect());
        let owned = registry(&[("shapes", OWNER)]);

        let add = r#"{"ir_version":1,"name":"add_shapes","ops":[
            {"op":"addColumn","table":"shapes","column":"ref_id","type":"uuid","nullable":true},
            {"op":"addColumn","table":"shapes","column":"fee",
             "type":{"decimal":{"precision":30,"scale":10}},"nullable":true}
        ]}"#;
        all_ops.extend(apply_doc(&session, &cfg, add, &owned, &live).await?);
        assert_no_drift(&session, &cfg, &all_ops, "shapes", "add column").await?;
        Ok(())
    }
    .await;

    result.unwrap_or_else(|error| panic!("{error}"));
}
/// Folding ONTO a live MySQL snapshot keeps the server's own contracts on the columns
/// the replay never touched, and derives one for the column it adds.
///
/// This is the half of `restamp_mysql_physical_types` that a from-empty fold cannot
/// reach, and it is the half that can do damage. A base read from the server carries
/// contracts `MysqlPhysicalType::parse` recovered from `COLUMN_TYPE` - `decimal(12,2)`
/// and `varchar(36)` - while the snapshot it rides in stores `data_type` "decimal" and
/// "text" with NO `ddl_type_override`. Re-deriving those from the snapshot answers
/// `Plain { kind: "decimal" }` and `Lob { tier: "text" }` and LOSES what the server
/// said, so a re-stamp that ran over every column would turn a correct base into a
/// wrong one.
///
/// # Why the untouched columns are asserted DIRECTLY and not through drift
///
/// Drift cannot see this one, and the reason is a defect in the REPORT rather than in
/// the comparison. `apply::drift::diff_attrs` runs
/// `if !column_data_types_eq(ec, ac) { push("data_type", &ec.data_type, &ac.data_type) }`,
/// and `push` drops the entry when the two strings are equal. Destroying the base's
/// contract leaves `data_type` reading "decimal" on BOTH sides, so
/// `column_data_types_eq` correctly answers "different" and the report then throws the
/// answer away.
///
/// MEASURED: with the carried-through guard disabled, this test kept passing on the
/// drift check alone while the fold was reporting `Decimal { precision: 65, scale: 30 }`
/// for a `decimal(12,2)` column - `mysql_ddl_type` maps a bare "decimal" to
/// `DECIMAL(65, 30)` and a bare "text" to `VARCHAR(191)`, which is exactly the
/// information the base had and the snapshot does not. So the contract comparison is
/// the instrument here; the drift check stays as the second half, because it is the
/// one that can see the ADDED column.
///
/// That reporting hole is recorded in the review log as its own finding. When it is
/// closed, the direct assertion below can go - not before.
#[compio::test]
async fn folding_onto_a_live_mysql_base_keeps_the_contracts_the_server_reported() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("ontobase");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated fold-onto-base database");

    let result: Result<(), String> = async {
        let create = r#"{"ir_version":1,"name":"create_ledger","ops":[
            {"op":"createTable","name":"ledger","columns":[
                {"name":"id","type":"int","nullable":false},
                {"name":"amount","type":{"decimal":{"precision":12,"scale":2}},"nullable":true},
                {"name":"uid","type":"uuid","nullable":true}
            ],
            "primaryKey":["id"]}
        ]}"#;
        apply_doc(
            &session,
            &cfg,
            create,
            &registry(&[]),
            &LiveSchema::default(),
        )
        .await?;

        // THE BASE, read from the server. Its `amount` and `uid` carry contracts no
        // re-derivation from this snapshot could reproduce.
        let base = MysqlBackend::new_generic(&session)
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot the base: {error}"))?;

        let add = r#"{"ir_version":1,"name":"add_fee","ops":[
            {"op":"addColumn","table":"ledger","column":"fee",
             "type":{"decimal":{"precision":30,"scale":10}},"nullable":true}
        ]}"#;
        let added = apply_doc(
            &session,
            &cfg,
            add,
            &registry(&[("ledger", OWNER)]),
            &LiveSchema::from_tables(["ledger".to_string()].into_iter().collect()),
        )
        .await?;

        let expected = fold_ops_onto(
            &base,
            &added,
            SqlDialect::Mysql,
            &cfg.project_schema,
            &support::no_inject(&cfg.project_schema),
        )
        .map_err(|error| format!("fold the addColumn onto the live base: {error}"))?;
        let actual = MysqlBackend::new_generic(&session)
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot after the addColumn: {error}"))?;

        // HALF ONE: the columns the replay never touched still carry the contracts the
        // SERVER reported, byte for byte.
        for column in ["amount", "uid"] {
            let from_server = column_contract(&base, "ledger", column)?;
            let after_fold = column_contract(&expected, "ledger", column)?;
            if from_server != after_fold {
                return Err(format!(
                    "folding onto a live base overwrote the contract MySQL reported for \
                     the untouched column ledger.{column}: the server said \
                     {from_server:?}, the fold now says {after_fold:?}. Re-deriving a \
                     column the replay never decided cannot recover what the server \
                     supplied - the snapshot carries data_type {:?} with no \
                     ddl_type_override, and mysql_ddl_type maps that to a default width.",
                    base.tables
                        .iter()
                        .find(|(name, _)| name.ends_with("ledger"))
                        .and_then(|(_, t)| t.columns.iter().find(|c| c.name == column))
                        .map(|c| c.data_type.clone())
                        .unwrap_or_default()
                ));
            }
        }

        // HALF TWO: the ADDED column got a contract, and it is the one the server holds.
        let drift = diff_snapshots(&expected, &actual);
        if drift.is_clean() {
            return Ok(());
        }
        Err(format!(
            "folding an addColumn onto the live base drifted from the server it was \
             read from: {drift:#?}\n  folded:\n    {}\n  introspected:\n    {}",
            physical_types(&expected, "ledger"),
            physical_types(&actual, "ledger"),
        ))
    }
    .await;

    result.unwrap_or_else(|error| panic!("{error}"));
}
