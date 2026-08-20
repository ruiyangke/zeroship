//! MySQL column retype, executed by RESTATING the live column definition.
//!
//! MySQL has no `ALTER COLUMN … TYPE`. Its retype is `MODIFY COLUMN`, which takes
//! the COMPLETE column definition and silently DISCARDS every facet the statement
//! omits. `tests/mysql_engine/mysql_setcolumntype_restate.rs` measures that against
//! a live server: a bare `MODIFY COLUMN label varchar(128)` changes the type and
//! destroys the column's `NOT NULL`, its `DEFAULT`, its `COLLATE` and its `COMMENT`
//! in the same statement, without a warning or an error.
//!
//! So the statement cannot be written until the definition is known, and the
//! definition is not in the op — `Op::SetColumnType` carries one field. It is read
//! here, under the same explicit table lock the primary-key path takes, from
//! `SHOW CREATE TABLE`: the server's own spelling of the column, reproduced verbatim
//! with ONLY its type token replaced. Every facet the server reports is therefore
//! carried across by construction rather than by re-derivation, which is what makes
//! the operation safe on a column whose facets the engine's own snapshot cannot see.
//!
//! `primary_key_sql.rs` already does exactly this for `dropIdentityFrom`; the
//! `SHOW CREATE TABLE` clause scanner is shared with it rather than re-implemented.

use std::time::Instant;

use crate::apply::executor::ApplyError;
use crate::apply::journal::Phase;
use crate::approval::{Approval, ApprovalScope};
use crate::conn::ExecutorConfig;
use crate::driver::SqlSession;
use crate::render::step::AlterColumnTypeStep;

use super::primary_key_sql::{column_clause_for, show_create_table};
use super::{journal_sql, session};

/// Apply one column retype by restating the live column definition.
pub(super) async fn alter_column_type<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    step: &AlterColumnTypeStep,
    approval: Approval,
    scope: &ApprovalScope,
    applied_by: &str,
) -> Result<bool, ApplyError> {
    let entries = journal_sql::applied(conn, cfg).await?;
    let mut had_inflight = false;
    for entry in entries
        .iter()
        .filter(|entry| entry.version == step.migration.version.as_str())
    {
        if entry.checksum != step.migration.checksum.as_str() {
            return Err(ApplyError::ChecksumDrift {
                version: step.migration.version.as_str().to_string(),
                recorded: entry.checksum.clone(),
                expected: step.migration.checksum.as_str().to_string(),
            });
        }
        match entry.phase {
            Phase::Completed => return Ok(false),
            Phase::Started => had_inflight = true,
        }
    }

    let approval_gated = step.migration.flags.destructive || step.migration.flags.requires_approval;
    if approval_gated {
        if approval != Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        if !scope.admits(step.migration.version.as_str()) {
            return Err(ApplyError::ApprovalNotScoped {
                version: step.migration.version.as_str().to_string(),
            });
        }
    }

    let snapshot = session::snapshot_session(conn).await?;
    let result = async {
        session::configure_session(conn, cfg, &step.migration).await?;

        // The same ambiguity every auto-committing MySQL DDL step has: a matching
        // started marker means the prior ALTER may or may not have landed. Refuse
        // before reading or changing the target table.
        if had_inflight {
            session::apply_two_phase(conn, cfg, &step.migration, applied_by, true, &[], "apply")
                .await?;
            unreachable!("MySQL two-phase apply always refuses an inflight marker");
        }

        // Hold the table from the `SHOW CREATE TABLE` read through issuance of the
        // ALTER, for the reason the primary-key path states: without it, external
        // DDL can change the definition between the read and the restate, and the
        // restate would then write back a definition that is no longer the column's.
        let schema_q = journal_sql::quote_ident_mysql(&step.schema)?;
        let table_q = journal_sql::quote_ident_mysql(&step.table)?;
        let meta_q = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
        let inflight_q = journal_sql::quote_ident_mysql("schema_migrations_inflight")?;
        conn.batch(&format!(
            "LOCK TABLES {schema_q}.{table_q} WRITE, {meta_q}.{inflight_q} WRITE"
        ))
        .await?;

        let ddl = match resolve_alter(conn, step).await {
            Ok(ddl) => ddl,
            Err(error) => {
                if let Err(unlock) = conn.batch("UNLOCK TABLES").await {
                    return Err(ApplyError::Backend(format!(
                        "{error}; additionally failed to release the MySQL column-restate lock: {unlock}"
                    )));
                }
                return Err(error);
            }
        };
        if let Err(error) = journal_sql::record_started(
            conn,
            cfg,
            step.migration.version.as_str(),
            &step.migration.name,
            step.migration.checksum.as_str(),
            applied_by,
        )
        .await
        {
            if let Err(unlock) = conn.batch("UNLOCK TABLES").await {
                return Err(ApplyError::Backend(format!(
                    "{error}; additionally failed to release the MySQL column-restate lock: {unlock}"
                )));
            }
            return Err(error.into());
        }

        let started = Instant::now();
        let ddl_result = conn.batch(&ddl).await;
        let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let unlock_result = conn.batch("UNLOCK TABLES").await;
        if let Err(source) = ddl_result {
            if let Err(unlock) = unlock_result {
                return Err(ApplyError::Backend(format!(
                    "MySQL column restate failed ({source}) and its explicit table lock could not be released ({unlock}); the inflight marker was retained"
                )));
            }
            return Err(ApplyError::MigrationFailed {
                version: step.migration.version.as_str().to_string(),
                source: source.into(),
            });
        }
        if let Err(unlock) = unlock_result {
            return Err(ApplyError::Backend(format!(
                "MySQL column restate completed but explicit table-lock cleanup failed: {unlock}; the inflight marker was retained for recovery"
            )));
        }
        session::finalize_started_structured_ddl(conn, cfg, &step.migration, applied_by, exec_ms)
            .await?;
        Ok(true)
    }
    .await;

    let restored = session::restore_session(conn, &snapshot).await;
    match (result, restored) {
        (Err(error), Err(restore)) => {
            tracing::warn!(
                error = %restore,
                version = %step.migration.version.as_str(),
                "zero-migrate: failed to restore MySQL session after a column restate error"
            );
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(ran), Ok(())) => Ok(ran),
    }
}

/// Read the live column definition and produce the one `ALTER TABLE` that restates
/// it with the target type.
async fn resolve_alter<D: SqlSession>(
    conn: &D,
    step: &AlterColumnTypeStep,
) -> Result<String, ApplyError> {
    let show_create = show_create_table(conn, &step.schema, &step.table).await?;
    let clause = column_clause_for(&show_create, &step.column).ok_or_else(|| {
        ApplyError::Backend(format!(
            "mysql column restate {}.{}: SHOW CREATE TABLE has no definition for column {:?}",
            step.schema, step.table, step.column
        ))
    })?;
    let restated = replace_column_type(clause, &step.ddl_type).ok_or_else(|| {
        ApplyError::Backend(format!(
            "mysql column restate {}.{}.{}: could not locate the type token in the live \
             definition {clause:?}",
            step.schema, step.table, step.column
        ))
    })?;
    let schema = journal_sql::quote_ident_mysql(&step.schema)?;
    let table = journal_sql::quote_ident_mysql(&step.table)?;
    Ok(format!(
        "ALTER TABLE {schema}.{table} MODIFY COLUMN {restated}"
    ))
}

/// Replace the TYPE token of one `SHOW CREATE TABLE` column clause, keeping the name
/// and every trailing facet byte-identical.
///
/// A MySQL type token is an identifier optionally followed by ONE parenthesised
/// argument list, and that list can contain whitespace, commas and string literals
/// (`decimal(10, 2)`, `enum('a','b, c')`). It can then be followed by unparenthesised
/// modifier words that are part of the type rather than of the column: `unsigned`,
/// `zerofill`, and the `character set` / `collate` pair. Those are NOT skipped here.
/// `unsigned` and `zerofill` belong to the numeric type the caller is replacing, so
/// carrying them onto a new type would be wrong (`bigint unsigned` → `varchar(64)
/// unsigned` is not a statement MySQL accepts); leaving them is equally wrong.
///
/// The honest boundary is therefore: this replaces the token and its argument list,
/// and REFUSES when the next word is a type-level modifier it would otherwise
/// silently carry or silently drop. A refusal here surfaces as an `ApplyError`
/// naming the definition, which is the loud failure the whole design exists to keep.
fn replace_column_type(clause: &str, new_type: &str) -> Option<String> {
    let bytes = clause.as_bytes();
    let name_end = quoted_name_end(bytes)?;
    let type_start = name_end + skip_whitespace(&bytes[name_end..]);
    let type_end = type_start + type_token_len(&bytes[type_start..])?;
    let tail = clause.get(type_end..)?;
    if leads_with_type_modifier(tail) {
        return None;
    }
    let head = clause.get(..type_start)?;
    Some(format!("{head}{new_type}{tail}"))
}

/// The byte index just past the column clause's leading backtick-quoted name.
fn quoted_name_end(bytes: &[u8]) -> Option<usize> {
    if bytes.first().copied()? != b'`' {
        return None;
    }
    let mut index = 1usize;
    while index < bytes.len() {
        if bytes[index] == b'`' {
            if bytes.get(index + 1) == Some(&b'`') {
                index += 2;
                continue;
            }
            return Some(index + 1);
        }
        index += 1;
    }
    None
}

fn skip_whitespace(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len())
}

/// The byte length of one type token: a bare word, plus ONE balanced parenthesised
/// argument list when the word is immediately followed by `(`. Quoted runs inside the
/// list are skipped so a `)` inside a string literal does not close it early.
fn type_token_len(bytes: &[u8]) -> Option<usize> {
    let word = bytes
        .iter()
        .position(|b| b.is_ascii_whitespace() || *b == b'(')
        .unwrap_or(bytes.len());
    if word == 0 {
        return None;
    }
    if bytes.get(word) != Some(&b'(') {
        return Some(word);
    }
    let mut index = word;
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == delimiter {
                if bytes.get(index + 1) == Some(&delimiter) {
                    index += 2;
                    continue;
                }
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Whether what follows the type token is a TYPE-level modifier rather than a
/// column-level facet. See [`replace_column_type`] for why this refuses.
fn leads_with_type_modifier(tail: &str) -> bool {
    let word = tail
        .trim_start()
        .split_ascii_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(word.as_str(), "unsigned" | "signed" | "zerofill")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_type_token_is_replaced_and_every_facet_is_kept() {
        let clause =
            "`label` varchar(64) COLLATE utf8mb4_bin NOT NULL DEFAULT 'unset' COMMENT 'keep me'";
        assert_eq!(
            replace_column_type(clause, "varchar(128)").as_deref(),
            Some(
                "`label` varchar(128) COLLATE utf8mb4_bin NOT NULL DEFAULT 'unset' COMMENT 'keep me'"
            )
        );
    }

    /// The case the test-file spike's line scan could not do: a type whose argument
    /// list contains whitespace, commas AND a string literal holding a `)`.
    #[test]
    fn an_argument_list_with_commas_quotes_and_parens_is_one_token() {
        let clause = "`state` enum('a','b, c',') d') NOT NULL DEFAULT 'a'";
        assert_eq!(
            replace_column_type(clause, "varchar(16)").as_deref(),
            Some("`state` varchar(16) NOT NULL DEFAULT 'a'")
        );
        let clause = "`amount` decimal(10, 2) DEFAULT NULL";
        assert_eq!(
            replace_column_type(clause, "bigint").as_deref(),
            Some("`amount` bigint DEFAULT NULL")
        );
    }

    /// A backtick inside a doubled-quoted column name must not end the name early.
    #[test]
    fn a_doubled_backtick_in_the_name_does_not_end_it() {
        let clause = "`we``ird` int NOT NULL";
        assert_eq!(
            replace_column_type(clause, "bigint").as_deref(),
            Some("`we``ird` bigint NOT NULL")
        );
    }

    /// The REFUSAL, and it is the safety property rather than a limitation to be
    /// apologized for: `bigint unsigned` has a modifier this scan would either carry
    /// onto the new type (producing SQL MySQL rejects) or drop (producing a silently
    /// different column). It declines instead, and the caller turns that into an
    /// error naming the definition.
    #[test]
    fn a_type_level_modifier_is_refused_rather_than_carried_or_dropped() {
        assert_eq!(
            replace_column_type("`n` bigint unsigned NOT NULL", "varchar(64)"),
            None
        );
        assert_eq!(
            replace_column_type("`n` int(10) UNSIGNED ZEROFILL NOT NULL", "bigint"),
            None
        );
    }

    #[test]
    fn a_clause_that_does_not_start_with_a_quoted_name_is_refused() {
        assert_eq!(replace_column_type("PRIMARY KEY (`id`)", "bigint"), None);
    }
}
