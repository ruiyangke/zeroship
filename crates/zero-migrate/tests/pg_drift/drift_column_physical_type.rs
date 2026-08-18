//! **A MySQL physical-type difference must reach the drift REPORT, not only the
//! comparator.**
//!
//! `apply::drift::column_data_types_eq` already treats the MySQL physical contract as
//! the authority when BOTH sides carry one, and it has to: `mysql_canonical_type`
//! folds every `varchar(n)` and every `text` tier to the literal `text`, so a live
//! `TEXT` and a live `VARCHAR(64)` are the SAME portable `data_type` string. The
//! comparator therefore answers "different" correctly.
//!
//! The report then threw that answer away. `diff_attrs` reported the difference as
//! `push("data_type", &ec.data_type, &ac.data_type)`, and `push` drops an entry whose
//! two sides are equal strings. So the report had two failure modes, both measured on
//! this tree:
//!
//! - **The line disappears.** A live `TEXT -> VARCHAR(64)` narrowing reads `"text"` on
//!   both sides, so nothing at all was reported.
//! - **The line says nothing.** A live `decimal(12,2) -> decimal(30,10)` widening was
//!   reported as `expected: "numeric", actual: "decimal"` - one type spelled two ways,
//!   because `fold_ops` emits the PostgreSQL `information_schema` spelling regardless
//!   of dialect while the MySQL catalog side is canonicalized.
//!
//! # What each test measures
//!
//! 1. `live_mysql_reports_a_physical_type_change_the_portable_type_cannot_see` - the
//!    RED. A table is deployed for real through `load_and_lower_guarded` +
//!    `MigrationEngine::apply_plan` over `MysqlBackend`, two live columns are changed
//!    OUT OF BAND, and the shipped drift path must name both.
//! 2. `an_untouched_mysql_table_reports_clean` - the OVER-REFUSAL control for the
//!    dialect being made more sensitive. Every column shape whose MySQL spelling the
//!    engine renders differently from the way MySQL stores it (`DECIMAL(65, 30)` vs
//!    `decimal(65,30)`, `TIMESTAMP` vs `datetime(6)`, `BOOLEAN` vs `tinyint(1)`) is
//!    deployed and left alone; drift must stay silent.
//! 3. `an_untouched_postgres_table_reports_clean` /
//!    `an_untouched_sqlite_table_reports_clean` - the same control for the two dialects
//!    that leave `mysql_physical_type` as `None`. The contract is consulted only when
//!    BOTH sides carry one, and these prove that rule still holds after the report
//!    learned to print the contract.
//!
//! # The instrument is asserted DIRECTLY
//!
//! A previous neuter on this code went green because the instrument could not see the
//! difference it was asserting about. So test 1 does not only ask whether the drift
//! report is non-empty - it cannot, because a bare `MODIFY COLUMN` also moves the
//! collation MySQL reports, so the report is non-empty either way. It asserts the two
//! CONTRACTS differ, that the portable `data_type` strings agree or disagree exactly as
//! the fixture intends, and that each reported side PARSES BACK to the contract it came
//! from. Any of those failing is a broken instrument and says so.
//!
//! Gated on `ZERO_MIGRATE_MYSQL_URL` / `ZERO_MIGRATE_TEST_PG_URL` through
//! `skip_if_no_mysql!` / `skip_if_no_pg!`, which route into `announce_live_db_skip`:
//! `ZERO_MIGRATE_REQUIRE_LIVE_DB=1` turns a missing DSN into a failure rather than a
//! green run with no coverage. The SQLite leg needs no server.

use crate::support;

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use tempfile::TempDir;
use zero_migrate::apply::backend::{MigrationBackend, MysqlBackend};
use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::snapshot::MysqlPhysicalType;
use zero_migrate::{
    diff_snapshots, fold_ops, model::ir::Op, resolve_create_table_policy, snapshot_schema,
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine,
    SchemaSnapshot, SqlDialect, SqliteBackend, StructuralDrift,
};

const OWNER: &str = "app_drift_column_physical_type";

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

/// Apply one IR doc through the REAL MySQL pipeline, returning the RESOLVED ops so the
/// caller accumulates the exact stream the fold replays.
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
            "drift-column-physical-type",
            LockMode::Acquire,
        )
        .await
        .map_err(|error| format!("apply IR plan: {error}"))?;

    Ok(resolved.ops)
}

/// `data_type` / `mysql_physical_type` per column of one table, as readable lines.
fn physical_types(snapshot: &SchemaSnapshot, table: &str) -> String {
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

/// One column's `(data_type, mysql_physical_type)` out of a snapshot.
fn column_facts(
    snapshot: &SchemaSnapshot,
    table: &str,
    column: &str,
) -> Result<(String, Option<MysqlPhysicalType>), String> {
    let c = snapshot
        .tables
        .iter()
        .find(|(name, _)| name.as_str() == table || name.ends_with(&format!(".{table}")))
        .and_then(|(_, t)| t.columns.iter().find(|c| c.name == column))
        .ok_or_else(|| format!("no column {table}.{column} in the snapshot"))?;
    Ok((c.data_type.clone(), c.mysql_physical_type.clone()))
}

/// The one `data_type` drift line for a column, or `None`.
fn data_type_line<'d>(
    drift: &'d StructuralDrift,
    table: &str,
    column: &str,
) -> Option<&'d zero_migrate::apply::drift::AlteredObject> {
    let object = format!("column {column}");
    drift
        .altered_objects
        .iter()
        .find(|a| a.table.ends_with(table) && a.object == object && a.field == "data_type")
}

/// **THE DEFECT.** A live MySQL column whose PHYSICAL type changed out of band must be
/// reported by structural drift, and the line must NAME the change.
///
/// Two columns, because the reporting hole has two faces and one fixture would only
/// show one of them. Both were measured on this tree before the fix:
///
/// - `label` is authored `text`, which MySQL stores as `TEXT`; narrowing it to
///   `VARCHAR(64)` leaves `data_type` reading `"text"` on BOTH sides, because the fold
///   emits the portable spelling and `mysql_canonical_type` folds every `varchar(n)`
///   to the same literal. `push` drops an entry whose two sides are equal strings, so
///   THE DIFFERENCE WAS NOT REPORTED AT ALL.
/// - `amount` is `decimal(12,2)` widened to `decimal(30,10)`. Here the strings differ,
///   so a line survived - but it read `expected: "numeric", actual: "decimal"`, which
///   is one type spelled two ways and names nothing a reader can act on.
///
/// # The oracle is the shipped parser, not a literal in this file
///
/// Each reported side is fed back through `MysqlPhysicalType::parse` and must recover
/// the contract it came from. That checks the line is FAITHFUL without this file
/// carrying a second spelling of the formatter under test.
///
/// # Why `is_clean` is not the assertion
///
/// A bare `MODIFY COLUMN` also changes what MySQL reports for the column's collation,
/// so the drift report is non-empty here for reasons that have nothing to do with the
/// type. Asserting `!drift.is_clean()` would have gone green against the defect. The
/// assertion is the `data_type` LINE for the named column.
#[compio::test]
async fn live_mysql_reports_a_physical_type_change_the_portable_type_cannot_see() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("widthdrift");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated width-drift database");

    let result: Result<(), String> = async {
        let create = r#"{"ir_version":1,"name":"create_widths","ops":[
            {"op":"createTable","name":"widths","columns":[
                {"name":"id","type":"int","nullable":false},
                {"name":"label","type":"text","nullable":true},
                {"name":"amount","type":{"decimal":{"precision":12,"scale":2}},"nullable":true}
            ],
            "primaryKey":["id"]}
        ]}"#;
        let ops = apply_doc(
            &session,
            &cfg,
            create,
            &registry(&[]),
            &LiveSchema::default(),
        )
        .await?;

        let expected = fold_ops(
            &ops,
            SqlDialect::Mysql,
            &cfg.project_schema,
            &support::no_inject(&cfg.project_schema),
        )
        .map_err(|error| format!("fold the op stream offline: {error}"))?;

        // THE CONTROL, and it is doing work: if the untouched deploy already drifted,
        // a drift line after the change would say nothing about the change.
        let untouched = MysqlBackend::new_generic(&session)
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot the untouched schema: {error}"))?;
        let clean = diff_snapshots(&expected, &untouched);
        if !clean.is_clean() {
            return Err(format!(
                "the table was deployed by the engine and left alone, yet drift reported \
                 a difference: {clean:#?}\n  folded:\n    {}\n  introspected:\n    {}",
                physical_types(&expected, "widths"),
                physical_types(&untouched, "widths"),
            ));
        }

        // OUT OF BAND: what a DBA does behind the engine's back.
        for change in [
            "MODIFY COLUMN `label` VARCHAR(64)",
            "MODIFY COLUMN `amount` DECIMAL(30,10)",
        ] {
            session
                .batch(&format!(
                    "ALTER TABLE {}.{} {change}",
                    quote_ident(&database),
                    quote_ident("widths")
                ))
                .await
                .map_err(|error| format!("apply `{change}` out of band: {error}"))?;
        }

        let actual = MysqlBackend::new_generic(&session)
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot the changed schema: {error}"))?;
        let drift = diff_snapshots(&expected, &actual);

        // `label` is the face that was DROPPED; `amount` is the face that survived as
        // a line naming nothing. `same_portable_type` records which is which so the
        // instrument check below fails loudly if a fixture ever stops isolating it.
        for (column, same_portable_type) in [("label", true), ("amount", false)] {
            // THE INSTRUMENT, asserted before the report is asked anything. A neuter
            // that makes the two contracts equal, or a fixture that no longer isolates
            // the case, must fail HERE and say so - not go quietly green below.
            let (expected_data_type, expected_contract) =
                column_facts(&expected, "widths", column)?;
            let (actual_data_type, actual_contract) = column_facts(&actual, "widths", column)?;
            let (Some(expected_contract), Some(actual_contract)) =
                (expected_contract.clone(), actual_contract.clone())
            else {
                return Err(format!(
                    "BROKEN INSTRUMENT: widths.{column} carries no MySQL physical contract \
                     on one side, so this test cannot measure the reporting hole. folded \
                     {expected_contract:?}, introspected {actual_contract:?}"
                ));
            };
            if expected_contract == actual_contract {
                return Err(format!(
                    "BROKEN INSTRUMENT: widths.{column} was changed on the live server but \
                     both sides report the same physical contract {expected_contract:?}, \
                     so there is no difference here for the report to print"
                ));
            }
            if (expected_data_type == actual_data_type) != same_portable_type {
                return Err(format!(
                    "BROKEN INSTRUMENT: widths.{column} was chosen because its portable \
                     data_type strings {} on both sides, and they now read \
                     {expected_data_type:?} vs {actual_data_type:?}. The fixture no longer \
                     isolates the case it was written for.",
                    if same_portable_type {
                        "AGREE"
                    } else {
                        "DISAGREE"
                    }
                ));
            }

            // THE ASSERTION.
            let Some(line) = data_type_line(&drift, "widths", column) else {
                return Err(format!(
                    "a live physical-type change on widths.{column} was NOT reported. The \
                     comparator can see it - folded {expected_contract:?} vs introspected \
                     {actual_contract:?} - but the report carries no data_type line. Both \
                     sides read data_type {expected_data_type:?}, which is why printing \
                     that string drops the entry. Drift: {drift:#?}"
                ));
            };
            // Faithfulness, checked with the SHIPPED parser rather than a literal.
            let reported_expected = MysqlPhysicalType::parse(&line.expected);
            let reported_actual = MysqlPhysicalType::parse(&line.actual);
            if reported_expected != expected_contract || reported_actual != actual_contract {
                return Err(format!(
                    "the data_type line for widths.{column} does not name the difference it \
                     was reported for. It reads expected {:?} / actual {:?}, which parse \
                     back to {reported_expected:?} / {reported_actual:?}, while the \
                     contracts that established the difference are {expected_contract:?} / \
                     {actual_contract:?}. Drift: {drift:#?}",
                    line.expected, line.actual
                ));
            }
        }
        Ok(())
    }
    .await;

    result.unwrap_or_else(|error| panic!("{error}"));
}

/// **THE OVER-REFUSAL CONTROL for MySQL.** A table the engine deployed and nobody
/// touched must report clean, across every column shape whose rendered DDL spelling
/// differs from the way MySQL stores it.
///
/// This is the risk the fix carries: making drift more sensitive can invent phantom
/// drift on a correct database. `DECIMAL(65, 30)` is stored as `decimal(65,30)`, a
/// `char(36)` UUID as `char(36)`, a boolean as `tinyint(1)`, and a `timestamp` with no
/// fractional seconds as bare `datetime` - every one of them a chance for a report to
/// print two spellings of the same type.
#[compio::test]
async fn an_untouched_mysql_table_reports_clean() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("mysqlclean");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(&database)))
        .await
        .expect("create the isolated mysql-clean database");

    let result: Result<(), String> = async {
        let create = r#"{"ir_version":1,"name":"create_corpus","ops":[
            {"op":"createTable","name":"corpus","columns":[
                {"name":"id","type":"int","nullable":false},
                {"name":"big","type":"bigInt","nullable":true},
                {"name":"small","type":"smallInt","nullable":true},
                {"name":"amount","type":{"decimal":{"precision":12,"scale":2}},"nullable":true},
                {"name":"wide","type":{"decimal":{"precision":30,"scale":10}},"nullable":true},
                {"name":"ratio","type":"double","nullable":true},
                {"name":"uid","type":"uuid","nullable":true},
                {"name":"label","type":{"string":{"length":255}},"nullable":true},
                {"name":"fixed","type":{"char":{"length":8}},"nullable":true},
                {"name":"body","type":"text","nullable":true},
                {"name":"ci","type":"text","nullable":true,"caseSensitive":false},
                {"name":"doc","type":"json","nullable":true},
                {"name":"raw","type":"bytes","nullable":true},
                {"name":"at","type":"timestamp","nullable":true},
                {"name":"day","type":"date","nullable":true},
                {"name":"flag","type":"boolean","nullable":true}
            ],
            "primaryKey":["id"]}
        ]}"#;
        let mut ops = apply_doc(
            &session,
            &cfg,
            create,
            &registry(&[]),
            &LiveSchema::default(),
        )
        .await?;
        assert_mysql_clean(&session, &cfg, &ops, "create table").await?;

        let add = r#"{"ir_version":1,"name":"add_corpus","ops":[
            {"op":"addColumn","table":"corpus","column":"ref_id","type":"uuid","nullable":true},
            {"op":"addColumn","table":"corpus","column":"note",
             "type":{"string":{"length":64}},"nullable":true},
            {"op":"addColumn","table":"corpus","column":"fee",
             "type":{"decimal":{"precision":30,"scale":10}},"nullable":true}
        ]}"#;
        ops.extend(
            apply_doc(
                &session,
                &cfg,
                add,
                &registry(&[("corpus", OWNER)]),
                &LiveSchema::from_tables(["corpus".to_string()].into_iter().collect()),
            )
            .await?,
        );
        assert_mysql_clean(&session, &cfg, &ops, "add column").await?;
        Ok(())
    }
    .await;

    result.unwrap_or_else(|error| panic!("{error}"));
}

/// Fold everything applied so far and require structural drift against the live server
/// to be CLEAN, printing both sides' contracts when it is not.
async fn assert_mysql_clean(
    session: &MysqlDevSession,
    cfg: &ExecutorConfig,
    ops: &[Op],
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
    Err(format!(
        "{stage}: the deploy succeeded and the server holds exactly what was asked for, \
         but structural drift reported a difference: {drift:#?}\n  folded:\n    {}\n  \
         introspected:\n    {}",
        physical_types(&expected, "corpus"),
        physical_types(&actual, "corpus"),
    ))
}

/// One column corpus, authored portably so the same JSON deploys on both dialects
/// that carry NO MySQL physical contract.
///
/// `columns` is a caller-supplied list rather than a constant because the two dialects
/// do not agree on what round-trips today. `fold_ops` always emits the PostgreSQL
/// `information_schema` spelling regardless of dialect, and the SQLite catalog reports
/// storage affinities (`integer`, `real`, `blob`), so a corpus with `bigInt`/`double`/
/// `bytes`/`json`/`timestamp` in it drifts on SQLite BEFORE any change of mine - the
/// `fold_roundtrip_sqlite` oracle exists precisely because it has to fold both sides
/// through `sqlite_canonical_type` first. That divergence is out of scope here; a
/// control has to be clean on the baseline or it measures nothing, so the SQLite leg
/// takes the subset that already is.
///
/// `primary_key` is caller-supplied for the same reason. SQLite reports a table's
/// PRIMARY KEY constraint under a DIFFERENT NAME from the fold (`pk_corpus` vs
/// `corpus_pkey`) and materializes no separate index for it - the same divergence
/// `fold_roundtrip_sqlite::canonicalize` strips before comparing. That is a
/// constraint-naming difference, not a column one, and it would drown the signal this
/// control exists for, so the SQLite leg authors no primary key at all.
fn portable_corpus(
    name: &str,
    columns: serde_json::Value,
    primary_key: serde_json::Value,
) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": 1,
        "name": name,
        "owner_app": OWNER,
        "ops": [{
            "op": "createTable",
            "name": "corpus",
            "columns": columns,
            "primaryKey": primary_key,
            "constraints": [],
            "indexes": []
        }]
    }))
    .expect("portable corpus fixture must deserialize")
}

/// The PostgreSQL leg's columns: every portable type the authoring layer offers.
fn postgres_corpus_columns() -> serde_json::Value {
    serde_json::json!([
        { "name": "id", "type": "int", "nullable": false },
        { "name": "big", "type": "bigInt", "nullable": true },
        { "name": "small", "type": "smallInt", "nullable": true },
        { "name": "amount", "type": { "decimal": { "precision": 12, "scale": 2 } },
          "nullable": true },
        { "name": "ratio", "type": "double", "nullable": true },
        { "name": "uid", "type": "uuid", "nullable": true },
        { "name": "label", "type": { "string": { "length": 255 } }, "nullable": true },
        { "name": "fixed", "type": { "char": { "length": 8 } }, "nullable": true },
        { "name": "body", "type": "text", "nullable": true },
        { "name": "doc", "type": "json", "nullable": true },
        { "name": "raw", "type": "bytes", "nullable": true },
        { "name": "at", "type": "timestamp", "nullable": true },
        { "name": "day", "type": "date", "nullable": true },
        { "name": "flag", "type": "boolean", "nullable": true }
    ])
}

/// The SQLite leg's columns: the subset whose folded spelling and SQLite affinity
/// already agree, so the control is clean on the baseline.
fn sqlite_corpus_columns() -> serde_json::Value {
    serde_json::json!([
        { "name": "id", "type": "int", "nullable": false },
        { "name": "count", "type": "int", "nullable": true },
        { "name": "amount", "type": { "decimal": { "precision": 12, "scale": 2 } },
          "nullable": true },
        { "name": "fee", "type": { "decimal": { "precision": 30, "scale": 10 } },
          "nullable": true },
        { "name": "uid", "type": "uuid", "nullable": true },
        { "name": "body", "type": "text", "nullable": true },
        { "name": "note", "type": "text", "nullable": true }
    ])
}

/// **THE OVER-REFUSAL CONTROL for PostgreSQL.** PostgreSQL leaves
/// `mysql_physical_type` as `None`, and the contract is consulted only when BOTH sides
/// carry one. A deployed-and-untouched table must stay clean.
#[compio::test]
async fn an_untouched_postgres_table_reports_clean() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("physdrift_{}_pg", std::process::id());
    let _schema_guard = support::SchemaGuard::arm(&session, [schema.clone()]);
    session
        .batch(&format!("CREATE SCHEMA \"{schema}\""))
        .await
        .expect("create the isolated postgres control schema");

    let result: Result<(), String> = async {
        let ir = portable_corpus(
            "drift_column_physical_type_pg",
            postgres_corpus_columns(),
            serde_json::json!(["id"]),
        );
        let expected = fold_ops(
            &ir.ops,
            SqlDialect::Postgres,
            &schema,
            &support::no_inject(&schema),
        )
        .map_err(|error| format!("fold the portable corpus: {error}"))?;
        let migrations = IrAuthor::new(
            &schema,
            OWNER,
            SqlDialect::Postgres,
            &support::no_inject(&schema),
        )
        .lower(&ir, &LiveSchema::default())
        .map_err(|error| format!("lower the portable corpus: {error}"))?;
        for migration in &migrations {
            session
                .batch(&migration.up)
                .await
                .map_err(|error| format!("apply {}: {error}", migration.name))?;
        }
        let actual = snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect the postgres control schema: {error}"))?;
        let drift = diff_snapshots(&expected, &actual);
        if drift.is_clean() {
            return Ok(());
        }
        Err(format!(
            "a PostgreSQL table the engine deployed and nobody touched reported drift: \
             {drift:#?}\n  folded:\n    {}\n  introspected:\n    {}",
            physical_types(&expected, "corpus"),
            physical_types(&actual, "corpus"),
        ))
    }
    .await;

    result.unwrap_or_else(|error| panic!("{error}"));
}

/// **THE OVER-REFUSAL CONTROL for SQLite.** Same rule, same reason - SQLite carries no
/// physical contract either, and a deployed-and-untouched database must stay clean.
#[compio::test]
async fn an_untouched_sqlite_table_reports_clean() {
    let dir: TempDir = tempfile::tempdir().expect("tempdir");
    let app: PathBuf = dir.path().join("zs-physdrift.sqlite");
    let journal: PathBuf = dir.path().join("zs-physdrift.migrations.sqlite");
    let backend = SqliteBackend::open(&app, &journal).expect("open hardened sqlite backend");

    let ir = portable_corpus(
        "drift_column_physical_type_sqlite",
        sqlite_corpus_columns(),
        serde_json::Value::Null,
    );
    let expected = fold_ops(
        &ir.ops,
        SqlDialect::Sqlite,
        "main",
        &support::no_inject("main"),
    )
    .expect("portable corpus must fold on SQLite");
    let migrations = IrAuthor::new(
        "main",
        OWNER,
        SqlDialect::Sqlite,
        &support::no_inject("main"),
    )
    .lower(&ir, &LiveSchema::default())
    .expect("portable corpus must lower on SQLite");
    for migration in &migrations {
        backend
            .apply_one_additive(migration, "d")
            .await
            .unwrap_or_else(|error| panic!("apply {}: {error}", migration.name));
    }
    let actual = backend
        .snapshot_schema_sqlite()
        .await
        .expect("introspect the sqlite control database");

    let drift = diff_snapshots(&expected, &actual);
    assert!(
        drift.is_clean(),
        "a SQLite database the engine deployed and nobody touched reported drift: \
         {drift:#?}\n  folded:\n    {}\n  introspected:\n    {}",
        physical_types(&expected, "corpus"),
        physical_types(&actual, "corpus"),
    );
}
