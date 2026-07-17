//! Apply-time SQLite identity-generator synchronization.
//!
//! SQLite has two integer rowid allocation modes which deliberately share the
//! same column shape:
//!
//! - an ordinary `INTEGER PRIMARY KEY` aliases the table rowid and chooses from
//!   the live rowids directly, so it has no separate generator to synchronize;
//! - `INTEGER PRIMARY KEY AUTOINCREMENT` additionally owns a row in
//!   `sqlite_sequence`, whose `seq` value is the last/high-water rowid rather
//!   than the next value.
//!
//! This module proves the live column is the true rowid alias, distinguishes the
//! two modes from the stored `CREATE TABLE` text, monotonically raises
//! `sqlite_sequence` for the latter, and journals the operation in the same
//! `BEGIN IMMEDIATE` transaction.  The stored DDL is authoritative for the
//! `AUTOINCREMENT` distinction: `sqlite_sequence` can exist because of another
//! table, and an AUTOINCREMENT table has no row there until its first write.

use std::time::Instant;

use crate::model::migration::Migration;

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;
use super::journal_sql;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowidAllocation {
    Ordinary,
    Autoincrement,
}

#[derive(Debug)]
struct Column {
    name: String,
    declared_type: String,
    pk_ordinal: i64,
}

fn fail(message: impl Into<String>) -> SqliteActorError {
    SqliteActorError::Exec(format!(
        "synchronizeIdentity SQLite precondition failed: {}",
        message.into()
    ))
}

fn lit(value: &str) -> String {
    journal_sql::sql_lit(value)
}

fn ident(value: &str) -> String {
    crate::render::dml::escape_quote_ident(value)
}

fn integer_cell(row: &[Option<String>], index: usize) -> i64 {
    row.get(index)
        .and_then(Clone::clone)
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_default()
}

fn required_cell(
    row: &[Option<String>],
    index: usize,
    field: &str,
) -> Result<String, SqliteActorError> {
    row.get(index)
        .and_then(Clone::clone)
        .ok_or_else(|| fail(format!("SQLite catalog omitted {field}")))
}

async fn table_columns(
    actor: &MigrationActor,
    table: &str,
) -> Result<Vec<Column>, SqliteActorError> {
    let rows = actor
        .query(&format!("PRAGMA main.table_info({})", lit(table)))
        .await?;
    if rows.is_empty() {
        return Err(fail(format!("table {table:?} does not exist")));
    }
    rows.into_iter()
        .map(|row| {
            Ok(Column {
                name: required_cell(&row, 1, "table_info.name")?,
                declared_type: row.get(2).and_then(Clone::clone).unwrap_or_default(),
                pk_ordinal: integer_cell(&row, 5),
            })
        })
        .collect()
}

async fn primary_key_has_separate_index(
    actor: &MigrationActor,
    table: &str,
) -> Result<bool, SqliteActorError> {
    Ok(actor
        .query(&format!("PRAGMA main.index_list({})", lit(table)))
        .await?
        .iter()
        .any(|row| {
            row.get(3)
                .and_then(Clone::clone)
                .is_some_and(|origin| origin.eq_ignore_ascii_case("pk"))
        }))
}

/// Prove the target is SQLite's actual integer rowid alias and classify its
/// allocation algorithm.  A sole exact `INTEGER` primary key is not sufficient:
/// `WITHOUT ROWID` and the historical inline `INTEGER PRIMARY KEY DESC` spelling
/// both have a real primary-key index and do not alias the rowid.
async fn resolve_rowid_allocation(
    actor: &MigrationActor,
    table: &str,
    column: &str,
) -> Result<RowidAllocation, SqliteActorError> {
    let create_rows = actor
        .query(&format!(
            "SELECT sql FROM main.sqlite_master WHERE type = 'table' AND name = {}",
            lit(table)
        ))
        .await?;
    let stored_create = create_rows
        .first()
        .and_then(|row| row.first())
        .and_then(Clone::clone)
        .ok_or_else(|| fail(format!("table {table:?} has no stored CREATE TABLE")))?;
    let columns = table_columns(actor, table).await?;
    let Some(target) = columns
        .iter()
        .find(|candidate| candidate.name.eq_ignore_ascii_case(column))
    else {
        return Err(fail(format!(
            "column {column:?} does not exist on table {table:?}"
        )));
    };

    let primary = columns
        .iter()
        .filter(|candidate| candidate.pk_ordinal > 0)
        .collect::<Vec<_>>();
    let without_rowid = crate::render::declarative::sqlite_create_is_without_rowid(&stored_create);
    let primary_has_index = primary_key_has_separate_index(actor, table).await?;
    let aliases_rowid = primary.len() == 1
        && target.pk_ordinal == 1
        && target.declared_type.trim().eq_ignore_ascii_case("INTEGER")
        && !without_rowid
        && !primary_has_index;
    if !aliases_rowid {
        return Err(fail(format!(
            "column {column:?} on table {table:?} is not the generated INTEGER PRIMARY KEY rowid alias"
        )));
    }

    if column_declares_autoincrement(&stored_create, &target.name)? {
        Ok(RowidAllocation::Autoincrement)
    } else {
        Ok(RowidAllocation::Ordinary)
    }
}

/// Split a stored SQLite table body into its top-level clauses.  Quoted text,
/// comments, and nested expressions cannot contribute a comma.  Returning
/// `None` for malformed stored SQL keeps the identity classification fail-closed.
fn table_clauses(body: &str) -> Option<Vec<&str>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Normal,
        Single,
        Double,
        Backtick,
        Bracket,
        LineComment,
        BlockComment,
    }

    let bytes = body.as_bytes();
    let mut clauses = Vec::new();
    let mut state = State::Normal;
    let mut depth = 0_usize;
    let mut start = 0_usize;
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let next = bytes.get(cursor + 1).copied();
        match state {
            State::Normal => match (byte, next) {
                (b'-', Some(b'-')) => {
                    state = State::LineComment;
                    cursor += 2;
                    continue;
                }
                (b'/', Some(b'*')) => {
                    state = State::BlockComment;
                    cursor += 2;
                    continue;
                }
                (b'\'', _) => state = State::Single,
                (b'"', _) => state = State::Double,
                (b'`', _) => state = State::Backtick,
                (b'[', _) => state = State::Bracket,
                (b'(', _) => depth = depth.checked_add(1)?,
                (b')', _) => depth = depth.checked_sub(1)?,
                (b',', _) if depth == 0 => {
                    clauses.push(body[start..cursor].trim());
                    start = cursor + 1;
                }
                _ => {}
            },
            State::Single if byte == b'\'' => {
                if next == Some(b'\'') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Double if byte == b'"' => {
                if next == Some(b'"') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Backtick if byte == b'`' => {
                if next == Some(b'`') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Bracket if byte == b']' => state = State::Normal,
            State::LineComment if matches!(byte, b'\n' | b'\r') => state = State::Normal,
            State::BlockComment if byte == b'*' && next == Some(b'/') => {
                state = State::Normal;
                cursor += 2;
                continue;
            }
            _ => {}
        }
        cursor += 1;
    }
    if depth != 0
        || matches!(
            state,
            State::Single | State::Double | State::Backtick | State::Bracket | State::BlockComment
        )
    {
        return None;
    }
    clauses.push(body[start..].trim());
    Some(clauses)
}

/// Return unquoted words at parenthesis depth zero.  Byte spans are unnecessary
/// here; the ordered grammar tokens are enough to recognize the narrow inline
/// `PRIMARY KEY [ASC|DESC] [ON CONFLICT action] AUTOINCREMENT` production.
fn top_level_words(sql: &str) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Normal,
        Single,
        Double,
        Backtick,
        Bracket,
        LineComment,
        BlockComment,
    }

    fn word_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
    }

    let bytes = sql.as_bytes();
    let mut words = Vec::new();
    let mut state = State::Normal;
    let mut depth = 0_usize;
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        let next = bytes.get(cursor + 1).copied();
        match state {
            State::Normal => match (byte, next) {
                (b'-', Some(b'-')) => {
                    state = State::LineComment;
                    cursor += 2;
                    continue;
                }
                (b'/', Some(b'*')) => {
                    state = State::BlockComment;
                    cursor += 2;
                    continue;
                }
                (b'\'', _) => state = State::Single,
                (b'"', _) => state = State::Double,
                (b'`', _) => state = State::Backtick,
                (b'[', _) => state = State::Bracket,
                (b'(', _) => depth = depth.checked_add(1)?,
                (b')', _) => depth = depth.checked_sub(1)?,
                (_, _) if depth == 0 && word_byte(byte) => {
                    let start = cursor;
                    while bytes.get(cursor).is_some_and(|byte| word_byte(*byte)) {
                        cursor += 1;
                    }
                    words.push(sql[start..cursor].to_string());
                    continue;
                }
                _ => {}
            },
            State::Single if byte == b'\'' => {
                if next == Some(b'\'') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Double if byte == b'"' => {
                if next == Some(b'"') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Backtick if byte == b'`' => {
                if next == Some(b'`') {
                    cursor += 2;
                    continue;
                }
                state = State::Normal;
            }
            State::Bracket if byte == b']' => state = State::Normal,
            State::LineComment if matches!(byte, b'\n' | b'\r') => state = State::Normal,
            State::BlockComment if byte == b'*' && next == Some(b'/') => {
                state = State::Normal;
                cursor += 2;
                continue;
            }
            _ => {}
        }
        cursor += 1;
    }
    (depth == 0
        && !matches!(
            state,
            State::Single | State::Double | State::Backtick | State::Bracket | State::BlockComment
        ))
    .then_some(words)
}

fn column_declares_autoincrement(create_sql: &str, column: &str) -> Result<bool, SqliteActorError> {
    let (open, close) = crate::render::declarative::sqlite_create_body_bounds(create_sql)
        .ok_or_else(|| fail("stored CREATE TABLE body could not be parsed"))?;
    let clauses = table_clauses(&create_sql[open + 1..close])
        .ok_or_else(|| fail("stored CREATE TABLE clauses could not be parsed"))?;
    let mut found_column = false;
    for clause in clauses {
        let mut cursor = 0_usize;
        let Some(name) = crate::render::declarative::sqlite_ddl_word(clause, &mut cursor) else {
            return Err(fail("stored CREATE TABLE contains an empty clause"));
        };
        if !name.eq_ignore_ascii_case(column) {
            continue;
        }
        found_column = true;
        let words = top_level_words(&clause[cursor..]).ok_or_else(|| {
            fail(format!(
                "stored definition for column {column:?} is malformed"
            ))
        })?;
        let Some(primary) = words.windows(2).position(|pair| {
            pair[0].eq_ignore_ascii_case("PRIMARY") && pair[1].eq_ignore_ascii_case("KEY")
        }) else {
            continue;
        };
        let mut next = primary + 2;
        if words.get(next).is_some_and(|word| {
            word.eq_ignore_ascii_case("ASC") || word.eq_ignore_ascii_case("DESC")
        }) {
            next += 1;
        }
        if words.get(next..next + 2).is_some_and(|pair| {
            pair[0].eq_ignore_ascii_case("ON") && pair[1].eq_ignore_ascii_case("CONFLICT")
        }) {
            next += 2;
            if words.get(next).is_some_and(|word| {
                ["ROLLBACK", "ABORT", "FAIL", "IGNORE", "REPLACE"]
                    .iter()
                    .any(|action| word.eq_ignore_ascii_case(action))
            }) {
                next += 1;
            }
        }
        if words
            .get(next)
            .is_some_and(|word| word.eq_ignore_ascii_case("AUTOINCREMENT"))
        {
            return Ok(true);
        }
    }
    if !found_column {
        return Err(fail(format!(
            "stored CREATE TABLE has no definition for column {column:?}"
        )));
    }
    Ok(false)
}

async fn reconcile_autoincrement(
    actor: &MigrationActor,
    table: &str,
    column: &str,
) -> Result<(), SqliteActorError> {
    let rows = actor
        .query(&format!(
            "SELECT CAST(MAX({}) AS TEXT) FROM main.{}",
            ident(column),
            ident(table)
        ))
        .await?;
    let Some(maximum) = rows
        .first()
        .and_then(|row| row.first())
        .and_then(Clone::clone)
    else {
        // A never-written AUTOINCREMENT table legitimately has no sequence row.
        return Ok(());
    };
    let maximum = maximum.parse::<i64>().map_err(|_| {
        fail(format!(
            "MAX({column:?}) on table {table:?} was not a signed 64-bit integer"
        ))
    })?;
    // SQLite-generated rowids are positive.  Zero is therefore a safe floor for
    // an import containing only explicit zero/negative rowids, and remains at
    // least the current live maximum.
    let high_water = maximum.max(0);
    let table_lit = lit(table);
    actor
        .exec(&format!(
            "UPDATE main.sqlite_sequence \
             SET seq = CASE \
                 WHEN seq IS NULL OR CAST(seq AS INTEGER) < {high_water} \
                 THEN {high_water} ELSE seq END \
             WHERE name = {table_lit}"
        ))
        .await?;
    actor
        .exec(&format!(
            "INSERT INTO main.sqlite_sequence (name, seq) \
             SELECT {table_lit}, {high_water} \
             WHERE NOT EXISTS (SELECT 1 FROM main.sqlite_sequence WHERE name = {table_lit})"
        ))
        .await?;
    Ok(())
}

async fn journal_completed(
    actor: &MigrationActor,
    migration: &Migration,
    applied_by: &str,
    exec_ms: i64,
) -> Result<(), SqliteActorError> {
    let version = lit(migration.version.as_str());
    let name = lit(&migration.name);
    let checksum = lit(migration.checksum.as_str());
    let by = lit(applied_by);
    actor
        .exec(&format!(
            "INSERT INTO \"_mig\".schema_migrations \
             (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind) \
             VALUES ('{applied}', {version}, {name}, {checksum}, {by}, {exec_ms}, \
                     'completed', 'success', 'apply')",
            applied = crate::apply::journal::EventKind::Applied.as_str()
        ))
        .await
}

/// Synchronize one SQLite rowid generator and journal the structured operation.
/// Validation, the monotonic sequence update, and the completed event share one
/// `BEGIN IMMEDIATE`, so no SQLite writer can change the maximum between them.
pub(crate) async fn synchronize_identity(
    actor: &MigrationActor,
    table: &str,
    column: &str,
    migration: &Migration,
    applied_by: &str,
) -> Result<(), SqliteActorError> {
    journal_sql::ensure_journal(actor).await?;
    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;
    let started = Instant::now();

    let result = async {
        match resolve_rowid_allocation(actor, table, column).await? {
            RowidAllocation::Ordinary => {}
            RowidAllocation::Autoincrement => {
                reconcile_autoincrement(actor, table, column).await?;
            }
        }
        let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        journal_completed(actor, migration, applied_by, exec_ms).await
    }
    .await;

    match result {
        Ok(()) => actor.commit_or_cleanup("SQLite synchronizeIdentity").await,
        Err(error) => Err(actor
            .cleanup_after_error("SQLite synchronizeIdentity", error)
            .await),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autoincrement_parser_targets_only_the_identity_clause() {
        assert!(column_declares_autoincrement(
            r#"CREATE TABLE t ("id" INTEGER CONSTRAINT "t pk" PRIMARY KEY ON CONFLICT ABORT AUTOINCREMENT, note TEXT)"#,
            "id"
        )
        .unwrap());
        assert!(!column_declares_autoincrement(
            r#"CREATE TABLE t (id INTEGER PRIMARY KEY, note TEXT DEFAULT 'AUTOINCREMENT', CHECK (note <> 'AUTOINCREMENT'))"#,
            "id"
        )
        .unwrap());
        assert!(!column_declares_autoincrement(
            r#"CREATE TABLE t (id INTEGER, other INTEGER PRIMARY KEY AUTOINCREMENT)"#,
            "id"
        )
        .unwrap());
    }

    #[test]
    fn clause_parser_ignores_nested_commas_and_comments() {
        let body = "id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT DEFAULT ('a,b'), \
                    -- comma, in comment\n CHECK (length(value) IN (1, 2))";
        let clauses = table_clauses(body).unwrap();
        assert_eq!(clauses.len(), 3);
    }
}
