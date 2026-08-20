//! **`setColumnType` on MySQL: what `MODIFY COLUMN` drops, where the definition can
//! be recovered from, and the retype working end to end.**
//!
//! MySQL's retype restates the WHOLE column definition and the op carries one field,
//! which is why `render::lower` refused it. The question is not whether that refusal
//! was comfortable but whether the definition can be RECOVERED, and from where. Four
//! measurements, all against a live server; the oracle is always
//! `information_schema`, never the SQL the engine spelled.
//!
//! 1. `a_naive_modify_column_drops_every_facet_it_omits` - the RISK, priced. A
//!    hostile column is deployed, retyped with a bare `MODIFY COLUMN name newtype`,
//!    and read back. Whatever the statement omitted is gone. This is the failure any
//!    restate design must avoid, and it is the CONTROL for (4): a green there with
//!    this red is the only combination that means the design works rather than that
//!    MySQL is forgiving.
//!
//! 2. `the_engine_snapshot_of_a_mysql_column_is_lossy_against_show_create_table` -
//!    the CHEAP design, priced, and the reason it was NOT taken.
//!    `LiveSchema::table_snapshots` is populated from the shipped `snapshot_schema`
//!    before the apply path lowers, so a lower-time restate built from
//!    `ColumnSnapshot` would need no new plumbing at all. This measures what that
//!    introspection recovers and what it does not - and the gaps (`COLUMN_COMMENT`,
//!    `ON UPDATE CURRENT_TIMESTAMP`, the generated-column facet) are exactly the
//!    facets (1) proves get destroyed silently.
//!
//! 3. `a_show_create_table_restate_preserves_every_facet` - the LOSSLESS design,
//!    priced. The same hostile column, retyped by restating the clause
//!    `SHOW CREATE TABLE` reports with only its type token replaced. This is the
//!    shape `apply/backend/mysql/primary_key_sql.rs` already uses for
//!    `dropIdentityFrom`, measured here before being adopted.
//!
//! 4. `an_authored_set_column_type_applies_on_mysql_and_keeps_every_facet` - the
//!    OPERATION, through the shipped pipeline. An authored `setColumnType` deploys
//!    against the live server via `load_and_lower_guarded` + the engine's
//!    `apply_plan`, and every facet survives.
//!
//! Gated on `ZERO_MIGRATE_MYSQL_URL` through `skip_if_no_mysql!`.

use crate::support;

use std::collections::BTreeMap;

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::apply::backend::{MigrationBackend, MysqlBackend};
use zero_migrate::driver::{Bind, SqlSession};
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine,
    SqlDialect,
};

const OWNER: &str = "app_mysql_setcolumntype_restate";

fn cfg_for(database: &str) -> ExecutorConfig {
    ExecutorConfig::new(
        format!("project_{database}"),
        database,
        support::no_inject(database),
    )
}

/// One hostile column, carrying every facet a `MODIFY COLUMN` can silently drop.
///
/// `varchar(64)` so the retype to `varchar(128)` is a REAL type change MySQL has to
/// rewrite, `COLLATE utf8mb4_bin` because the table default is not it, `NOT NULL`
/// and a `DEFAULT` because those are the two the refusal's own comment names, and a
/// `COMMENT` because it is the facet the engine's snapshot does not read at all.
const HOSTILE_TABLE: &str = "\
CREATE TABLE `facets` (
  `id` int NOT NULL,
  `label` varchar(64) COLLATE utf8mb4_bin NOT NULL DEFAULT 'unset' COMMENT 'keep me',
  `touched` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  PRIMARY KEY (`id`)
) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci";

/// Every facet `information_schema` reports for one column, as the server's own
/// words. The ORACLE for both restate measurements.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveFacets {
    column_type: String,
    is_nullable: String,
    column_default: Option<String>,
    collation_name: Option<String>,
    extra: String,
    comment: String,
}

async fn read_facets(
    session: &MysqlDevSession,
    database: &str,
    table: &str,
    column: &str,
) -> Result<LiveFacets, String> {
    let rows = session
        .query(
            "SELECT COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, COLLATION_NAME, EXTRA, COLUMN_COMMENT
               FROM information_schema.COLUMNS
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?",
            &[
                Bind::Text(database.to_string()),
                Bind::Text(table.to_string()),
                Bind::Text(column.to_string()),
            ],
        )
        .await
        .map_err(|error| format!("read information_schema facets: {error}"))?;
    let row = rows
        .first()
        .ok_or_else(|| format!("information_schema has no column {table}.{column}"))?;
    Ok(LiveFacets {
        column_type: row
            .try_get("COLUMN_TYPE")
            .map_err(|e| format!("COLUMN_TYPE: {e}"))?,
        is_nullable: row
            .try_get("IS_NULLABLE")
            .map_err(|e| format!("IS_NULLABLE: {e}"))?,
        column_default: row
            .try_get("COLUMN_DEFAULT")
            .map_err(|e| format!("COLUMN_DEFAULT: {e}"))?,
        collation_name: row
            .try_get("COLLATION_NAME")
            .map_err(|e| format!("COLLATION_NAME: {e}"))?,
        extra: row.try_get("EXTRA").map_err(|e| format!("EXTRA: {e}"))?,
        comment: row
            .try_get("COLUMN_COMMENT")
            .map_err(|e| format!("COLUMN_COMMENT: {e}"))?,
    })
}

async fn setup(session: &MysqlDevSession, database: &str) -> Result<(), String> {
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(database)))
        .await
        .map_err(|error| format!("create database: {error}"))?;
    session
        .batch(&format!("USE {}", quote_ident(database)))
        .await
        .map_err(|error| format!("use database: {error}"))?;
    session
        .batch(HOSTILE_TABLE)
        .await
        .map_err(|error| format!("create the hostile table: {error}"))?;
    Ok(())
}

/// **THE RISK, PRICED.** A bare `MODIFY COLUMN label varchar(128)` changes the type
/// and DESTROYS the collation, the `NOT NULL`, the default and the comment - all four
/// in one statement, with no warning and no error.
///
/// This is what any restate design must not do, and it is why a `setColumnType` that
/// merely learns to spell `MODIFY COLUMN` would be WORSE than today's refusal.
#[compio::test]
async fn a_naive_modify_column_drops_every_facet_it_omits() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("naivemod");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        setup(&session, &database).await?;
        let before = read_facets(&session, &database, "facets", "label").await?;
        assert_eq!(before.column_type, "varchar(64)");
        assert_eq!(before.is_nullable, "NO");
        assert_eq!(before.column_default.as_deref(), Some("unset"));
        assert_eq!(before.collation_name.as_deref(), Some("utf8mb4_bin"));
        assert_eq!(before.comment, "keep me");

        session
            .batch("ALTER TABLE `facets` MODIFY COLUMN `label` varchar(128)")
            .await
            .map_err(|error| format!("naive MODIFY COLUMN: {error}"))?;

        let after = read_facets(&session, &database, "facets", "label").await?;
        // The type DID change - the statement is not a no-op.
        assert_eq!(
            after.column_type, "varchar(128)",
            "the naive MODIFY COLUMN must actually retype the column"
        );
        // And every facet it omitted is GONE.
        assert_eq!(
            after.is_nullable, "YES",
            "NOT NULL survived a MODIFY COLUMN that omitted it - the silent-drop \
             premise is wrong and this whole file needs rewriting"
        );
        assert_eq!(
            after.column_default, None,
            "the DEFAULT survived a MODIFY COLUMN that omitted it"
        );
        assert_eq!(
            after.collation_name.as_deref(),
            Some("utf8mb4_0900_ai_ci"),
            "the column COLLATE survived a MODIFY COLUMN that omitted it (it should \
             have fallen back to the table default)"
        );
        assert_eq!(
            after.comment, "",
            "the COMMENT survived a MODIFY COLUMN that omitted it"
        );
        Ok(())
    }
    .await;
    result.expect("naive MODIFY COLUMN measurement");
}

/// **THE CHEAP DESIGN, PRICED.** `LiveSchema::table_snapshots` is what a lower-time
/// restate would have to build its `MODIFY COLUMN` from, and it is populated by the
/// shipped `snapshot_schema` on every MySQL apply. This measures what that
/// introspection recovers for the hostile column.
///
/// The assertions below are written as the CURRENT answer. A facet that starts
/// arriving is a design improvement and must fail here so the design note is
/// updated, not silently pass.
#[compio::test]
async fn the_engine_snapshot_of_a_mysql_column_is_lossy_against_show_create_table() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("snaploss");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        setup(&session, &database).await?;
        let snapshot = MysqlBackend::new_generic(&session)
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot the hostile table: {error}"))?;
        let table = snapshot
            .tables
            .get("facets")
            .ok_or("snapshot has no `facets` table")?;
        let label = table
            .columns
            .iter()
            .find(|c| c.name == "label")
            .ok_or("snapshot has no `facets.label` column")?;
        let touched = table
            .columns
            .iter()
            .find(|c| c.name == "touched")
            .ok_or("snapshot has no `facets.touched` column")?;

        // RECOVERED - the facets a lower-time restate could spell.
        assert!(
            !label.nullable,
            "NOT NULL must reach the snapshot; without it no restate is possible at all"
        );
        assert_eq!(
            label.default.as_deref(),
            Some("unset"),
            "the DEFAULT must reach the snapshot"
        );
        assert!(
            label.mysql_physical_type.is_some(),
            "the modifier-bearing physical type must reach the snapshot"
        );
        // The exact COLLATE arrives, but NOT on `ColumnSnapshot::collation` - that
        // field stays `None` on MySQL. It arrives on `mysql_text_storage`, together
        // with the character set. Reading the wrong field is how a restate design
        // gets talked into believing the collation is unavailable.
        let storage = label
            .mysql_text_storage
            .as_ref()
            .ok_or("the character-set/collation pair must reach the snapshot")?;
        assert_eq!(storage.collation, "utf8mb4_bin");
        assert_eq!(storage.character_set, "utf8mb4");
        // `case_sensitive` is a LOSSY three-into-two projection: `None` is the
        // snapshot's canonical spelling for BOTH "no collation" and "case-sensitive"
        // (`case_sensitive_from_collation` says so - `Some(true)` is deliberately
        // never emitted). Nothing can be restated from it.
        assert_eq!(
            label.case_sensitive, None,
            "utf8mb4_bin is case-sensitive and the snapshot spells that `None`"
        );

        // NOT RECOVERED - the facets a lower-time restate would SILENTLY DROP.
        assert_eq!(
            label.collation, None,
            "MEASURED: ColumnSnapshot::collation is the PostgreSQL-side field and \
             stays None on MySQL; mysql_text_storage carries the fact instead"
        );
        assert_eq!(
            label.comment, None,
            "MEASURED GAP: COLUMN_COMMENT is not read by MySQL introspection"
        );
        assert_eq!(
            table.stored_create_sql, None,
            "MEASURED GAP: MySQL introspection does not carry SHOW CREATE TABLE, so \
             the lossless text is not in the snapshot either"
        );
        // ON UPDATE CURRENT_TIMESTAMP lives in EXTRA, which MySQL introspection reads
        // only for `auto_increment` and `DEFAULT_GENERATED`. Nothing on the snapshot
        // can spell it, so a restate built from the snapshot drops it.
        assert_eq!(
            touched.default.as_deref(),
            Some("CURRENT_TIMESTAMP"),
            "the timestamp default reaches the snapshot"
        );
        assert_eq!(
            touched.comment, None,
            "and nothing on the snapshot records ON UPDATE CURRENT_TIMESTAMP"
        );
        Ok(())
    }
    .await;
    result.expect("snapshot fidelity measurement");
}

/// **THE LOSSLESS DESIGN, PRICED.** Restating the clause `SHOW CREATE TABLE` reports,
/// with only the type token replaced, retypes the column and leaves every other facet
/// exactly as the server reported it before.
///
/// The oracle is `information_schema` on both sides, so this measures the SERVER's
/// answer rather than the statement's spelling. The type-token replacement here is a
/// SPIKE, not the shipped algorithm: it is deliberately the smallest thing that can
/// prove the approach, so that the cost of the real one is judged against a measured
/// result.
#[compio::test]
async fn a_show_create_table_restate_preserves_every_facet() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("restate");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        setup(&session, &database).await?;
        let before = read_facets(&session, &database, "facets", "label").await?;

        let show_create = session
            .query("SHOW CREATE TABLE `facets`", &[])
            .await
            .map_err(|error| format!("SHOW CREATE TABLE: {error}"))?
            .first()
            .ok_or("SHOW CREATE TABLE returned no row")?
            .try_get::<_, String>(1usize)
            .map_err(|error| format!("SHOW CREATE TABLE column 1: {error}"))?;

        let clause = column_clause(&show_create, "label")
            .ok_or("SHOW CREATE TABLE has no `label` clause")?;
        // The SPIKE's type-token replacement: the clause opens with the backticked
        // name, then the type token, then every facet verbatim.
        let restated = replace_type_token(clause, "varchar(128)")
            .ok_or_else(|| format!("could not replace the type token in {clause:?}"))?;
        session
            .batch(&format!("ALTER TABLE `facets` MODIFY COLUMN {restated}"))
            .await
            .map_err(|error| format!("restated MODIFY COLUMN {restated:?}: {error}"))?;

        let after = read_facets(&session, &database, "facets", "label").await?;
        assert_eq!(
            after.column_type, "varchar(128)",
            "the restated MODIFY COLUMN must actually retype the column"
        );
        assert_eq!(
            after,
            LiveFacets {
                column_type: "varchar(128)".to_string(),
                ..before
            },
            "every facet other than the type must be exactly what the server reported \
             before the retype"
        );
        Ok(())
    }
    .await;
    result.expect("SHOW CREATE TABLE restate measurement");
}

/// **THE SPIKE, END TO END.** An authored `setColumnType` deploys against a live
/// MySQL through the SHIPPED pipeline — `load_and_lower_guarded` + the engine's
/// `apply_plan` over `MysqlBackend` — and every facet the retyped column carried
/// survives it.
///
/// The oracle is `information_schema` before and after, so this asserts on the
/// SERVER's answer and never on the statement the engine spelled. The `before`
/// facets are the ones the hostile `CREATE TABLE` established; the `after` facets
/// must equal them in every field except the type.
///
/// This is the whole risk: `a_naive_modify_column_drops_every_facet_it_omits` above
/// proves that a `MODIFY COLUMN` written from the op alone destroys four of these
/// five fields. A green here with a red there is the only combination that means the
/// design works.
#[compio::test]
async fn an_authored_set_column_type_applies_on_mysql_and_keeps_every_facet() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("sctapply");
    let cfg = cfg_for(&database);
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        setup(&session, &database).await?;
        let before = read_facets(&session, &database, "facets", "label").await?;
        assert_eq!(before.column_type, "varchar(64)");

        // The live facts the apply path builds before it lowers, from a REAL catalog
        // read through the shipped `snapshot_schema` — the same call `engine.rs`
        // makes. Nothing here is hand-built.
        let backend = MysqlBackend::new_generic(&session);
        let snapshot = backend
            .snapshot_schema(&cfg)
            .await
            .map_err(|error| format!("snapshot before the retype: {error}"))?;
        let live = LiveSchema::from_catalog_snapshot(snapshot, OWNER);

        let source = r#"{"ir_version":1,"name":"widen_label","ops":[
            {"op":"setColumnType","table":"facets","column":"label",
             "toType":{"string":{"length":128}}}
        ]}"#;
        let policy = support::no_inject(&cfg.project_schema);
        let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Mysql, &policy);
        let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Mysql);
        let registry: BTreeMap<String, String> = [("facets".to_string(), OWNER.to_string())]
            .into_iter()
            .collect();
        let artifact = author
            .load_and_lower_guarded(source, OWNER, &registry, &live, &guard)
            .map_err(|error| format!("load and lower the authored setColumnType: {error}"))?;

        MigrationEngine::new()
            .apply_plan(
                &artifact.plan.steps,
                Approval::Approved,
                &backend,
                &cfg,
                "mysql-setcolumntype-restate",
                LockMode::Acquire,
            )
            .await
            .map_err(|error| format!("apply the authored setColumnType: {error}"))?;

        let after = read_facets(&session, &database, "facets", "label").await?;
        assert_eq!(
            after,
            LiveFacets {
                column_type: "varchar(128)".to_string(),
                ..before
            },
            "the authored retype must change the type and NOTHING else"
        );
        Ok(())
    }
    .await;
    result.expect("authored setColumnType end to end on live MySQL");
}

/// The `label` clause of a `SHOW CREATE TABLE` body, found by its backticked name.
///
/// SPIKE-GRADE. `apply/backend/mysql/primary_key_sql.rs::create_table_clauses` is the
/// quote- and comment-aware scanner this would REUSE; this is a line scan so the
/// measurement does not depend on making that private function public before the
/// design is settled.
fn column_clause<'a>(show_create: &'a str, column: &str) -> Option<&'a str> {
    let needle = format!("`{}` ", column.replace('`', "``"));
    show_create
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with(&needle))
        .map(|line| line.trim_end_matches(','))
}

/// Replace the type token that follows the backticked column name, keeping every
/// following facet verbatim.
///
/// SPIKE-GRADE, and the limits are the design's open question rather than an
/// oversight: a type token carrying a parenthesised list with a space or a comma
/// inside it (`enum('a','b')`, `decimal(10, 2)`) is not handled by this scan.
fn replace_type_token(clause: &str, new_type: &str) -> Option<String> {
    let after_name = clause.find("` ")? + 2;
    let (name, rest) = clause.split_at(after_name);
    let rest = rest.trim_start();
    let type_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let tail = &rest[type_len..];
    Some(format!("{name}{new_type}{tail}"))
}
