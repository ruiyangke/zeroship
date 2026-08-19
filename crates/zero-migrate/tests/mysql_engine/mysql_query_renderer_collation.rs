//! The SECOND MySQL renderer - `schema::query`'s - pins the same explicit collation
//! the first one pins, so its character columns compare case-SENSITIVELY.
//!
//! # Two renderers, one question
//!
//! `render::declarative::mysql_type_override_with_collation` states the engine's
//! promise: *"every character type pins an explicit collation so string comparison is
//! case-SENSITIVE by default (matching Postgres/SQLite)"*. That renderer answers from
//! a [`FieldDescriptor`](zero_migrate::render::declarative::FieldDescriptor) and a
//! PostgreSQL-spelled `data_type`, and it keeps the promise.
//!
//! `schema::query::renderer(SqlDialect::Mysql).column_type` answers the SAME question
//! from a raw SDK field def, and pinned NOTHING - not on `VARCHAR(n)`, not on
//! `CHAR(n)`, not on `LONGTEXT`, not on the `VARCHAR(191)` a `ref` or an unknown token
//! falls back to, not on the native `ENUM(...)`. Every one of those inherited the
//! table default.
//!
//! # WHAT REACHES A SERVER, MEASURED BEFORE ANYTHING WAS CHANGED
//!
//! **Nothing in production does.** That is not an inference from reading the call
//! graph; it was measured. `MysqlSchemaRenderer::column_type` was given a tripwire
//! that panics on entry, and the whole Rust suite was run against live PostgreSQL,
//! live MySQL and live SQLite (37 sections, 3333 tests). Exactly EIGHT tests tripped
//! it and all eight are `#[cfg(test)]` unit tests inside `schema/query.rs` itself; not
//! one integration test and not one live-server leg reached it. The static reason
//! agrees: the only production caller of the dialect-generic emitter is
//! `render::declarative`'s SQLite 12-step rebuild, which passes a hardcoded
//! `SqlDialect::Sqlite`, and the three call sites of
//! `def_to_column_type_for_dialect` in `render::declarative` and `schema::diff` all
//! pass a hardcoded `SqlDialect::Postgres`. MySQL column DDL is produced by
//! `render::declarative::column_type_for_render` instead.
//!
//! So this file does NOT claim to fix a defect a deployment can hit today. It claims
//! that the two renderers now answer alike, and it is the only thing standing between
//! this arm and a regression - `schema::query` is a `pub mod` of the library crate,
//! and `def_to_column_type_for_dialect` is `pub` and takes the dialect as a
//! parameter, so the first caller that passes `Mysql` gets whatever this arm says.
//! The routing below (a test handing MySQL the statements this arm renders) is
//! therefore the ONLY route by which this DDL reaches a server anywhere.
//!
//! # Why these assertions and not a grep
//!
//! Grepping the rendered statement for `COLLATE utf8mb4_0900_as_cs` would prove only
//! that the renderer agrees with itself. So every claim here is decided by a server:
//! whether `'Active' = 'active'` in that column, and whether a UNIQUE index over it
//! accepts both spellings. The catalog is read too, from
//! `information_schema.COLUMNS` - the relation the SHIPPED drift path reads
//! (`apply::backend::mysql::drift_sql`) - with a missing row treated as an ERROR
//! rather than as "no collation", because that view is privilege-filtered and an
//! absent row and an invisible one are the same thing. `SHOW CREATE TABLE` is the
//! corroborating witness: only its text distinguishes a collation the engine PINNED
//! from one the column INHERITED.
//!
//! [`the_probe_server_is_case_insensitive_by_default`] is the instrument check and it
//! is load-bearing: on a server whose default collation is already `_cs` or `_bin`, a
//! pinned and an unpinned column behave identically and every assertion below passes
//! while proving nothing. It FAILS rather than skips.

use crate::support;

use crate::support::mysql::{quote_ident, DatabaseGuard, MysqlDevSession};
use zero_migrate::driver::{Bind, SqlSession};
use zero_migrate::schema::query::{
    build_create_table_with_fks_for_dialect_scoped_statements, FkEmission, SqliteEmitScope,
};
use zero_migrate::SqlDialect;

/// The table this file probes: one column per CHARACTER spelling the MySQL arm can
/// produce, plus the non-character spellings that must stay bare.
///
/// `bounded`/`unbounded`/`long` cover the three `string` outcomes (`VARCHAR(n)`,
/// `VARCHAR(191)`, `LONGTEXT`), `fixed` covers `char`, `pointer` covers `ref`,
/// `addr` covers `inet`, and `mystery` covers the unknown-token fallback that every
/// unrecognised type lands on.
fn probe_schema() -> serde_json::Value {
    serde_json::json!({
        "id":        { "type": "integer" },
        "bounded":   { "type": "string", "maxLength": 64 },
        "unbounded": { "type": "string" },
        "long":      { "type": "string", "maxLength": 40000 },
        "fixed":     { "type": "char", "charLen": 8 },
        "pointer":   { "type": "ref" },
        "addr":      { "type": "inet" },
        "payload":   { "type": "json" },
        "ratio":     { "type": "number" },
    })
}

/// The CHARACTER columns of [`probe_schema`], with the spelling each one renders as.
const CHARACTER_COLUMNS: [(&str, &str); 7] = [
    ("bounded", "VARCHAR(64)"),
    ("unbounded", "VARCHAR(191)"),
    ("long", "LONGTEXT"),
    ("fixed", "CHAR(8)"),
    ("pointer", "VARCHAR(191)"),
    ("addr", "VARCHAR(43)"),
    ("mystery", "VARCHAR(191)"),
];

/// The NON-character columns, which must carry no collation at all. `JSON COLLATE ...`
/// is a parse error rather than a harmless redundancy, so a wrong pin here would fail
/// the `CREATE TABLE` outright.
const BARE_COLUMNS: [&str; 3] = ["id", "payload", "ratio"];

/// Render the probe table through the `schema::query` emitter for one dialect.
///
/// This is the arm under test. Nothing in production calls it with `Mysql` (see the
/// module header); this function IS the route.
fn render_create(dialect: SqlDialect, schema_name: &str, table: &str) -> Result<String, String> {
    let mut schema = probe_schema();
    let object = schema
        .as_object_mut()
        .expect("the probe schema is an object");
    object.insert(
        "mystery".into(),
        serde_json::json!({ "type": "notAThingTheEngineKnows" }),
    );
    let policy = support::no_inject(schema_name);
    build_create_table_with_fks_for_dialect_scoped_statements(
        schema_name,
        table,
        &schema,
        &FkEmission::Inline,
        dialect,
        SqliteEmitScope::AttachAlias,
        &policy,
    )
    .map_err(|error| format!("render the probe CREATE TABLE: {error}"))?
    .into_iter()
    .next()
    .ok_or_else(|| "the emitter produced no CREATE TABLE".to_string())
}

async fn fresh_database(session: &MysqlDevSession, database: &str) -> Result<(), String> {
    session
        .batch(&format!("CREATE DATABASE {}", quote_ident(database)))
        .await
        .map_err(|e| format!("create the probe database: {e}"))
}

/// The collation MySQL would give an uncollated character column in `database`.
async fn database_collation(session: &MysqlDevSession, database: &str) -> Result<String, String> {
    let row = session
        .query_one(
            "SELECT DEFAULT_COLLATION_NAME AS c FROM information_schema.schemata \
             WHERE SCHEMA_NAME = ?",
            &[Bind::Text(database.to_string())],
        )
        .await
        .map_err(|e| format!("read the database default collation: {e}"))?;
    row.try_get::<_, String>("c")
        .map_err(|e| format!("DEFAULT_COLLATION_NAME did not decode as text: {e}"))
}

/// The collation the SERVER gave `column`, read from the relation the shipped drift
/// path reads (`apply::backend::mysql::drift_sql` reads
/// `information_schema.COLUMNS`), so this measures the same surface the engine
/// measures.
///
/// That view is privilege-filtered - MySQL shows a row only for a column the connected
/// user holds some privilege on - so a missing row is an ERROR here, never "the column
/// has no collation". `Ok(None)` means the row EXISTS and its `COLLATION_NAME` is SQL
/// NULL, which is MySQL's answer for a non-character column.
async fn catalog_collation(
    session: &MysqlDevSession,
    database: &str,
    table: &str,
    column: &str,
) -> Result<Option<String>, String> {
    let rows = session
        .query(
            "SELECT COLLATION_NAME AS c FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND COLUMN_NAME = ?",
            &[
                Bind::Text(database.to_string()),
                Bind::Text(table.to_string()),
                Bind::Text(column.to_string()),
            ],
        )
        .await
        .map_err(|e| format!("read the column collation: {e}"))?;
    match rows.len() {
        1 => Ok(rows[0].try_get::<_, String>("c").ok()),
        n => Err(format!(
            "information_schema.COLUMNS returned {n} rows for {database}.{table}.{column}; \
             an absent row and a row this user cannot see are the same thing here"
        )),
    }
}

/// `SHOW CREATE TABLE`, the corroborating witness: its text carries the column's
/// EXPLICIT collation clause, so it separates a collation the engine PINNED from one
/// the column merely INHERITED from the table default.
async fn show_create_table(
    session: &MysqlDevSession,
    database: &str,
    table: &str,
) -> Result<String, String> {
    let row = session
        .query_one(
            &format!(
                "SHOW CREATE TABLE {}.{}",
                quote_ident(database),
                quote_ident(table)
            ),
            &[],
        )
        .await
        .map_err(|e| format!("SHOW CREATE TABLE: {e}"))?;
    row.try_get::<_, String>("Create Table")
        .map_err(|e| format!("SHOW CREATE TABLE did not decode as text: {e}"))
}

/// Create the probe database and the probe table in it.
async fn deploy_probe(session: &MysqlDevSession, database: &str) -> Result<(), String> {
    fresh_database(session, database).await?;
    let create = render_create(SqlDialect::Mysql, database, "probe")?;
    session
        .batch(&create)
        .await
        .map_err(|e| format!("apply the probe CREATE TABLE: {e}\n{create}"))
}

/// THE INSTRUMENT CHECK. Every claim in this file is void on a server that is already
/// case-sensitive by default, because then a pinned and an unpinned column agree.
///
/// It FAILS rather than skips: a green suite on a mis-provisioned server would be the
/// exact false reassurance this file exists to prevent.
#[compio::test]
async fn the_probe_server_is_case_insensitive_by_default() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("qrycollinstr");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        fresh_database(&session, &database).await?;
        let collation = database_collation(&session, &database).await?;
        if !collation.contains("_ci") {
            return Err(format!(
                "a fresh database on this server defaults to {collation}, which is not \
                 case-INSENSITIVE, so nothing in this file distinguishes a pinned \
                 collation from an absent one (server {})",
                session.server_version()
            ));
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// THE BEHAVIOURAL CLAIM. For every character spelling this renderer can produce, the
/// server must say `'Active'` and `'active'` are two different strings.
///
/// Before the fix all seven compared EQUAL, because each inherited the `_ai_ci`
/// database default.
#[compio::test]
async fn every_character_column_compares_two_cases_as_different_values() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("qrycollcmp");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        deploy_probe(&session, &database).await?;
        let default_collation = database_collation(&session, &database).await?;
        if !default_collation.contains("_ci") {
            return Err(format!(
                "this database defaults to {default_collation}; the assertions below \
                 cannot distinguish the fix from its absence"
            ));
        }

        let columns = CHARACTER_COLUMNS
            .iter()
            .map(|(name, _)| quote_ident(name))
            .collect::<Vec<_>>()
            .join(", ");
        let values = CHARACTER_COLUMNS
            .iter()
            .map(|_| "'active'")
            .collect::<Vec<_>>()
            .join(", ");
        session
            .batch(&format!(
                "INSERT INTO {}.`probe` (id, {columns}) VALUES (1, {values})",
                quote_ident(&database)
            ))
            .await
            .map_err(|e| format!("insert the lower-case row: {e}"))?;

        let mut equal_under_the_server = Vec::new();
        for (column, spelling) in CHARACTER_COLUMNS {
            let row = session
                .query_one(
                    &format!(
                        "SELECT COUNT(*) AS n FROM {}.`probe` WHERE {} = 'Active'",
                        quote_ident(&database),
                        quote_ident(column)
                    ),
                    &[],
                )
                .await
                .map_err(|e| format!("compare {column}: {e}"))?;
            let matched: i64 = row
                .try_get("n")
                .map_err(|e| format!("COUNT(*) for {column} did not decode: {e}"))?;
            if matched != 0 {
                equal_under_the_server.push(format!("{column} ({spelling})"));
            }
        }
        if !equal_under_the_server.is_empty() {
            let create = show_create_table(&session, &database, "probe").await?;
            return Err(format!(
                "MySQL compared 'Active' EQUAL to the stored 'active' in {} of {} \
                 character columns: {}. PostgreSQL and SQLite compare them different \
                 for the same authored schema. Table text:\n{create}",
                equal_under_the_server.len(),
                CHARACTER_COLUMNS.len(),
                equal_under_the_server.join(", ")
            ));
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// The uniqueness half, which is the one that loses DATA rather than a comparison: a
/// UNIQUE index over a `_ai_ci` column refuses the second of `'active'` / `'Active'`
/// with `ERROR 1062 Duplicate entry`.
///
/// The index is created by hand rather than rendered, deliberately: the subject is the
/// COLUMN's collation, which the index inherits, and this way an index-rendering
/// change cannot quietly become the thing under test.
#[compio::test]
async fn a_unique_index_keeps_two_cases_apart_on_mysql() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("qrycolluniq");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        deploy_probe(&session, &database).await?;
        let default_collation = database_collation(&session, &database).await?;
        if !default_collation.contains("_ci") {
            return Err(format!(
                "this database defaults to {default_collation}; this assertion cannot \
                 distinguish the fix from its absence"
            ));
        }
        session
            .batch(&format!(
                "CREATE UNIQUE INDEX `probe_bounded_uq` ON {}.`probe` (`bounded`)",
                quote_ident(&database)
            ))
            .await
            .map_err(|e| format!("create the unique index: {e}"))?;

        for (id, value) in [(1, "active"), (2, "Active")] {
            session
                .batch(&format!(
                    "INSERT INTO {}.`probe` (id, bounded) VALUES ({id}, '{value}')",
                    quote_ident(&database)
                ))
                .await
                .map_err(|e| {
                    format!(
                        "MySQL refused {value:?} beside the row already holding the other \
                         spelling, so a UNIQUE index over this column collapses two \
                         values the author kept apart: {e}"
                    )
                })?;
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// The cross-dialect control, through the SAME emitter's PostgreSQL arm - so the claim
/// above is a DISAGREEMENT between two servers rather than an opinion about one.
///
/// The PostgreSQL arm is the one production actually reaches, and it needs no
/// collation clause: `character varying` under any ordinary PostgreSQL collation is
/// already case-sensitive.
#[compio::test]
async fn postgres_keeps_the_same_two_cases_apart() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = format!("zm_qrycoll_{}", std::process::id());
    let _guard = support::SchemaGuard::arm(&session, [schema.clone()]);

    let result: Result<(), String> = async {
        session
            .batch(&format!("CREATE SCHEMA \"{schema}\""))
            .await
            .map_err(|e| format!("create the probe schema: {e}"))?;
        let create = render_create(SqlDialect::Postgres, &schema, "probe")?;
        session
            .batch(&create)
            .await
            .map_err(|e| format!("apply the probe CREATE TABLE: {e}\n{create}"))?;
        session
            .batch(&format!(
                "INSERT INTO \"{schema}\".\"probe\" (id, bounded) VALUES (1, 'active')"
            ))
            .await
            .map_err(|e| format!("insert the lower-case row: {e}"))?;
        let row = session
            .query_one(
                &format!(
                    "SELECT COUNT(*) AS n FROM \"{schema}\".\"probe\" WHERE bounded = 'Active'"
                ),
                &[],
            )
            .await
            .map_err(|e| format!("compare on PostgreSQL: {e}"))?;
        let matched: i64 = row
            .try_get("n")
            .map_err(|e| format!("COUNT(*) did not decode: {e}"))?;
        if matched != 0 {
            return Err(
                "PostgreSQL compared 'Active' equal to 'active', so the MySQL claim is \
                 not a cross-dialect disagreement"
                    .to_string(),
            );
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// The catalog half, and the witness that separates PINNED from INHERITED.
#[compio::test]
async fn the_catalog_reports_a_pinned_case_sensitive_collation_on_every_character_column() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("qrycollcat");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        deploy_probe(&session, &database).await?;
        let create = show_create_table(&session, &database, "probe").await?;
        let create_lower = create.to_ascii_lowercase();

        for (column, spelling) in CHARACTER_COLUMNS {
            let collation = catalog_collation(&session, &database, "probe", column)
                .await?
                .ok_or_else(|| {
                    format!(
                        "{column} ({spelling}) reported COLLATION_NAME NULL, which MySQL \
                         does not do for a character column"
                    )
                })?;
            if collation != "utf8mb4_0900_as_cs" {
                return Err(format!(
                    "{column} ({spelling}) carries {collation}; the engine's \
                     case-sensitive default is utf8mb4_0900_as_cs. Table text:\n{create}"
                ));
            }
            // The corroborating witness. A catalog row alone cannot tell a pin from an
            // inheritance that happened to agree.
            let clause = format!("`{column}` ");
            let line = create_lower
                .lines()
                .find(|line| line.trim_start().starts_with(&clause.to_ascii_lowercase()))
                .ok_or_else(|| format!("no line for {column} in the table text:\n{create}"))?;
            if !line.contains("collate utf8mb4_0900_as_cs") {
                return Err(format!(
                    "the table text carries no EXPLICIT collation on {column}, so the \
                     catalog reading above is inherited rather than pinned:\n{create}"
                ));
            }
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// The other direction, and the reason the pin reads the `caseSensitive` facet rather
/// than hard-coding one collation: a field that ASKS for case-insensitivity must get
/// it, and the server must then agree that `'Active'` is `'active'`.
#[compio::test]
async fn a_case_insensitive_field_gets_the_case_insensitive_collation() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("qrycollci");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        fresh_database(&session, &database).await?;
        let schema = serde_json::json!({
            "id":    { "type": "integer" },
            "label": { "type": "string", "maxLength": 64, "caseSensitive": false },
        });
        let create = build_create_table_with_fks_for_dialect_scoped_statements(
            &database,
            "ci_probe",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Mysql,
            SqliteEmitScope::AttachAlias,
            &support::no_inject(&database),
        )
        .map_err(|e| format!("render the case-insensitive probe: {e}"))?
        .into_iter()
        .next()
        .ok_or_else(|| "the emitter produced no CREATE TABLE".to_string())?;
        session
            .batch(&create)
            .await
            .map_err(|e| format!("apply the case-insensitive probe: {e}\n{create}"))?;

        let collation = catalog_collation(&session, &database, "ci_probe", "label")
            .await?
            .ok_or_else(|| "the label column reported no collation at all".to_string())?;
        if collation != "utf8mb4_0900_ai_ci" {
            return Err(format!(
                "a caseSensitive:false column carries {collation}; it asked for \
                 utf8mb4_0900_ai_ci"
            ));
        }
        session
            .batch(&format!(
                "INSERT INTO {}.`ci_probe` (id, label) VALUES (1, 'active')",
                quote_ident(&database)
            ))
            .await
            .map_err(|e| format!("insert the lower-case row: {e}"))?;
        let row = session
            .query_one(
                &format!(
                    "SELECT COUNT(*) AS n FROM {}.`ci_probe` WHERE label = 'Active'",
                    quote_ident(&database)
                ),
                &[],
            )
            .await
            .map_err(|e| format!("compare the case-insensitive column: {e}"))?;
        let matched: i64 = row
            .try_get("n")
            .map_err(|e| format!("COUNT(*) did not decode: {e}"))?;
        if matched != 1 {
            return Err(
                "a caseSensitive:false column did NOT match 'Active' against the stored \
                 'active', so the facet did not reach the server"
                    .to_string(),
            );
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

/// The half that must NOT move. A collation pinned onto a non-character spelling is
/// not a harmless redundancy: `JSON COLLATE ...` is a parse error, so a wrong pin
/// would take the `CREATE TABLE` down with it. The `CREATE` succeeding in
/// [`deploy_probe`] already proves the syntax; this proves the SEMANTIC half, that the
/// server assigned no collation to any of them.
#[compio::test]
async fn a_non_character_column_takes_no_collation() {
    let url = skip_if_no_mysql!();
    let session = MysqlDevSession::connect(&url);
    let database = support::mysql::database_token("qrycollbare");
    let _guard = DatabaseGuard::arm(&session, [database.clone()]);

    let result: Result<(), String> = async {
        deploy_probe(&session, &database).await?;
        for column in BARE_COLUMNS {
            if let Some(collation) = catalog_collation(&session, &database, "probe", column).await?
            {
                return Err(format!(
                    "{column} is not a character column yet carries {collation}; the pin \
                     reached a spelling it must leave bare"
                ));
            }
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}
