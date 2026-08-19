//! Explicit MySQL primary-key lifecycle execution.
//!
//! MySQL auto-commits DDL, so the catalog-validated operation is resolved to one
//! `ALTER TABLE` and then rides the backend's existing started-marker → DDL →
//! completed-row protocol. In particular, removing `AUTO_INCREMENT` is never a
//! preceding `ALTER`: every required `MODIFY COLUMN`, the old-key drop, and the
//! optional new-key add are clauses of that one statement.

use std::collections::BTreeMap;
use std::time::Instant;

use crate::apply::executor::ApplyError;
use crate::apply::journal::Phase;
use crate::approval::{Approval, ApprovalScope};
use crate::conn::ExecutorConfig;
use crate::driver::SqlSession;
use crate::model::ir::AlterPrimaryKeyAction;
use crate::render::step::AlterPrimaryKeyStep;

use super::{journal_sql, session};

#[derive(Debug)]
struct ColumnFacts {
    name: String,
    nullable: bool,
    auto_increment: bool,
}

#[derive(Debug)]
struct IndexPart {
    sequence: i64,
    column: Option<String>,
    prefix: Option<i64>,
    expression: Option<String>,
}

#[derive(Debug, Default)]
struct IndexFacts {
    name: String,
    non_unique: Option<i64>,
    index_type: Option<String>,
    parts: Vec<IndexPart>,
}

#[derive(Debug, Default)]
struct InboundForeignKey {
    child_schema: String,
    child_table: String,
    constraint_name: String,
    parts: Vec<(i64, String)>,
}

/// Apply one live-catalog-resolved primary-key operation.
pub(super) async fn alter_primary_key<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    step: &AlterPrimaryKeyStep,
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

    // Mirror every other structured MySQL step: prove the dedicated session is
    // idle before `configure_session` can set autocommit=1, and restore every
    // caller-owned session setting on both success and failure. The operation
    // body remains responsible for releasing its explicit table locks before
    // this outer hygiene wrapper runs.
    let snapshot = session::snapshot_session(conn).await?;
    let result = async {
        session::configure_session(conn, cfg, &step.migration).await?;

    // Preserve the ordinary MySQL DDL recovery contract: a matching started
    // marker is ambiguous because the prior auto-committing ALTER may have
    // landed. Refuse before reading or changing the target table.
    if had_inflight {
        session::apply_two_phase(conn, cfg, &step.migration, applied_by, true, &[], "apply")
            .await?;
        unreachable!("MySQL two-phase apply always refuses an inflight marker");
    }

    // Hold an explicit MySQL table/metadata lock from the first catalog read
    // through issuance of the ALTER. INFORMATION_SCHEMA remains readable while
    // LOCK TABLES is active, related FK tables are locked implicitly by MySQL,
    // and ALTER TABLE releases the explicit lock as part of taking over the
    // target. This closes the otherwise-dangerous window where external DDL
    // could swap PRIMARY after expectedColumns was checked but before the
    // column-less `DROP PRIMARY KEY` clause ran.
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
                    "{error}; additionally failed to release MySQL primary-key validation lock: {unlock}"
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
                "{error}; additionally failed to release MySQL primary-key validation lock: {unlock}"
            )));
        }
        return Err(error.into());
    }

    let started = Instant::now();
    let ddl_result = conn.batch(&ddl).await;
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    // InnoDB ALTER TABLE releases a preceding LOCK TABLES lock. An explicit
    // UNLOCK is harmless after that release and is required on every early/error
    // path where ALTER did not take over the lock.
    let unlock_result = conn.batch("UNLOCK TABLES").await;
    if let Err(source) = ddl_result {
        if let Err(unlock) = unlock_result {
            return Err(ApplyError::Backend(format!(
                "MySQL primary-key ALTER failed ({source}) and its explicit table lock could not be released ({unlock}); the inflight marker was retained"
            )));
        }
        return Err(ApplyError::MigrationFailed {
            version: step.migration.version.as_str().to_string(),
            source: source.into(),
        });
    }
    if let Err(unlock) = unlock_result {
        return Err(ApplyError::Backend(format!(
            "MySQL primary-key ALTER completed but explicit table-lock cleanup failed: {unlock}; the inflight marker was retained for recovery"
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
                "zero-migrate: failed to restore MySQL session after primary-key operation error"
            );
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(ran), Ok(())) => Ok(ran),
    }
}

async fn resolve_alter<D: SqlSession>(
    conn: &D,
    step: &AlterPrimaryKeyStep,
) -> Result<String, ApplyError> {
    let columns = read_columns(conn, &step.schema, &step.table).await?;
    if columns.is_empty() {
        return Err(pk_error(step, "target table was not found"));
    }
    let indexes = read_indexes(conn, &step.schema, &step.table).await?;
    let current_primary = current_primary_key(step, &indexes)?;

    match &step.action {
        AlterPrimaryKeyAction::Add { .. } if current_primary.is_some() => {
            return Err(pk_error(
                step,
                format!(
                    "add requires no current primary key, but the live ordered key is {:?}",
                    current_primary.as_deref().unwrap_or_default()
                ),
            ));
        }
        AlterPrimaryKeyAction::Replace {
            expected_columns, ..
        }
        | AlterPrimaryKeyAction::Drop {
            expected_columns, ..
        } => {
            let Some(actual) = current_primary.as_deref() else {
                return Err(pk_error(
                    step,
                    format!(
                        "expected current primary key {expected_columns:?}, but the table has no primary key"
                    ),
                ));
            };
            if !same_ordered_columns(actual, expected_columns) {
                return Err(pk_error(
                    step,
                    format!(
                        "expected current primary key {expected_columns:?}, but the live ordered key is {actual:?}"
                    ),
                ));
            }
            if step
                .action
                .target_columns()
                .is_some_and(|target| same_ordered_columns(actual, target))
            {
                return Err(pk_error(
                    step,
                    "replacement target is the same live primary key under MySQL identifier semantics",
                ));
            }
        }
        AlterPrimaryKeyAction::Add { .. } => {}
    }

    if let Some(target) = step.action.target_columns() {
        validate_target_columns(step, &columns, target)?;
        if !has_exact_non_primary_unique(&indexes, target) {
            return Err(pk_error(
                step,
                format!(
                    "target columns {target:?} do not have an exact pre-existing full-column UNIQUE BTREE candidate key"
                ),
            ));
        }
    }

    if let Some(old_primary) = current_primary.as_deref() {
        validate_inbound_foreign_keys(conn, step, old_primary, &indexes).await?;
    }

    let drop_identity = step.action.drop_identity_from();
    validate_identity_transition(step, &columns, drop_identity)?;

    let mut clauses = Vec::new();
    if !drop_identity.is_empty() {
        let show_create = show_create_table(conn, &step.schema, &step.table).await?;
        for requested in drop_identity {
            let column = find_column(&columns, requested).ok_or_else(|| {
                pk_error(
                    step,
                    format!("dropIdentityFrom column {requested:?} was not found"),
                )
            })?;
            let definition =
                column_definition_without_auto_increment(&show_create, column.name.as_str())?;
            clauses.push(format!("MODIFY COLUMN {definition}"));
        }
    }

    if !matches!(step.action, AlterPrimaryKeyAction::Add { .. }) {
        clauses.push("DROP PRIMARY KEY".to_string());
    }
    if let Some(target) = step.action.target_columns() {
        let quoted = target
            .iter()
            .map(|column| journal_sql::quote_ident_mysql(column))
            .collect::<Result<Vec<_>, _>>()?;
        clauses.push(format!("ADD PRIMARY KEY ({})", quoted.join(", ")));
    }

    let schema = journal_sql::quote_ident_mysql(&step.schema)?;
    let table = journal_sql::quote_ident_mysql(&step.table)?;
    Ok(format!(
        "ALTER TABLE {schema}.{table} {}",
        clauses.join(", ")
    ))
}

async fn read_columns<D: SqlSession>(
    conn: &D,
    schema: &str,
    table: &str,
) -> Result<Vec<ColumnFacts>, ApplyError> {
    let rows = conn
        .query(
            "SELECT COLUMN_NAME AS column_name, IS_NULLABLE AS is_nullable, EXTRA AS extra
               FROM information_schema.COLUMNS
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
              ORDER BY ORDINAL_POSITION",
            &[schema.into(), table.into()],
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            let extra: String = row.try_get("extra")?;
            Ok(ColumnFacts {
                name: row.try_get("column_name")?,
                nullable: row
                    .try_get::<_, String>("is_nullable")?
                    .eq_ignore_ascii_case("YES"),
                auto_increment: extra
                    .split_ascii_whitespace()
                    .any(|part| part.eq_ignore_ascii_case("auto_increment")),
            })
        })
        .collect()
}

async fn read_indexes<D: SqlSession>(
    conn: &D,
    schema: &str,
    table: &str,
) -> Result<Vec<IndexFacts>, ApplyError> {
    let rows = conn
        .query(
            "SELECT INDEX_NAME AS index_name, NON_UNIQUE AS non_unique,
                    INDEX_TYPE AS index_type, SEQ_IN_INDEX AS seq_in_index,
                    COLUMN_NAME AS column_name, SUB_PART AS sub_part,
                    EXPRESSION AS expression
               FROM information_schema.STATISTICS
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
              ORDER BY INDEX_NAME, SEQ_IN_INDEX",
            &[schema.into(), table.into()],
        )
        .await?;
    let mut by_name = BTreeMap::<String, IndexFacts>::new();
    for row in rows {
        let name: String = row.try_get("index_name")?;
        let key = name.to_ascii_lowercase();
        let facts = by_name.entry(key).or_insert_with(|| IndexFacts {
            name: name.clone(),
            ..IndexFacts::default()
        });
        let non_unique: i64 = row.try_get("non_unique")?;
        let index_type: String = row.try_get("index_type")?;
        if facts
            .non_unique
            .is_some_and(|recorded| recorded != non_unique)
            || facts
                .index_type
                .as_deref()
                .is_some_and(|recorded| !recorded.eq_ignore_ascii_case(&index_type))
        {
            return Err(ApplyError::Backend(format!(
                "mysql primary key: catalog returned inconsistent metadata for index {name:?}"
            )));
        }
        facts.non_unique = Some(non_unique);
        facts.index_type = Some(index_type);
        facts.parts.push(IndexPart {
            sequence: row.try_get("seq_in_index")?,
            column: row.try_get("column_name")?,
            prefix: row.try_get("sub_part")?,
            expression: row.try_get("expression")?,
        });
    }
    Ok(by_name.into_values().collect())
}

fn current_primary_key(
    step: &AlterPrimaryKeyStep,
    indexes: &[IndexFacts],
) -> Result<Option<Vec<String>>, ApplyError> {
    let Some(primary) = indexes
        .iter()
        .find(|index| index.name.eq_ignore_ascii_case("PRIMARY"))
    else {
        return Ok(None);
    };
    exact_plain_columns(primary)
        .ok_or_else(|| {
            pk_error(
                step,
                "live PRIMARY index is not a full-column ordered UNIQUE BTREE key",
            )
        })
        .map(Some)
}

fn exact_plain_columns(index: &IndexFacts) -> Option<Vec<String>> {
    if index.non_unique != Some(0)
        || !index
            .index_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("BTREE"))
        || index.parts.is_empty()
    {
        return None;
    }
    let mut parts = index.parts.iter().collect::<Vec<_>>();
    parts.sort_by_key(|part| part.sequence);
    let mut columns = Vec::with_capacity(parts.len());
    for (offset, part) in parts.into_iter().enumerate() {
        if part.sequence != i64::try_from(offset + 1).ok()?
            || part.prefix.is_some()
            || part
                .expression
                .as_deref()
                .is_some_and(|expression| !expression.trim().is_empty())
        {
            return None;
        }
        columns.push(part.column.clone()?);
    }
    Some(columns)
}

fn has_exact_non_primary_unique(indexes: &[IndexFacts], columns: &[String]) -> bool {
    indexes.iter().any(|index| {
        !index.name.eq_ignore_ascii_case("PRIMARY")
            && exact_plain_columns(index)
                .as_deref()
                .is_some_and(|actual| same_ordered_columns(actual, columns))
    })
}

fn validate_target_columns(
    step: &AlterPrimaryKeyStep,
    columns: &[ColumnFacts],
    target: &[String],
) -> Result<(), ApplyError> {
    let mut normalized = std::collections::BTreeSet::new();
    for requested in target {
        if !normalized.insert(requested.to_ascii_lowercase()) {
            return Err(pk_error(
                step,
                format!("target columns contain duplicate MySQL identifier {requested:?}"),
            ));
        }
        let Some(column) = find_column(columns, requested) else {
            return Err(pk_error(
                step,
                format!("target column {requested:?} was not found"),
            ));
        };
        if column.nullable {
            return Err(pk_error(
                step,
                format!(
                    "target column {:?} is nullable; primary-key components must already be NOT NULL",
                    column.name
                ),
            ));
        }
    }
    Ok(())
}

fn validate_identity_transition(
    step: &AlterPrimaryKeyStep,
    columns: &[ColumnFacts],
    drop_identity: &[String],
) -> Result<(), ApplyError> {
    let mut normalized = std::collections::BTreeSet::new();
    for requested in drop_identity {
        if !normalized.insert(requested.to_ascii_lowercase()) {
            return Err(pk_error(
                step,
                format!("dropIdentityFrom contains duplicate MySQL identifier {requested:?}"),
            ));
        }
        let Some(column) = find_column(columns, requested) else {
            return Err(pk_error(
                step,
                format!("dropIdentityFrom column {requested:?} was not found"),
            ));
        };
        if !column.auto_increment {
            return Err(pk_error(
                step,
                format!(
                    "dropIdentityFrom column {:?} is not AUTO_INCREMENT in the live table",
                    column.name
                ),
            ));
        }
    }

    let target = step.action.target_columns();
    for column in columns.iter().filter(|column| column.auto_increment) {
        let retains_identity_contract = target.is_some_and(|target| {
            target.len() == 1 && target[0].eq_ignore_ascii_case(&column.name)
        });
        let declared_drop = drop_identity
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&column.name));
        if !retains_identity_contract && !declared_drop {
            return Err(pk_error(
                step,
                format!(
                    "AUTO_INCREMENT column {:?} would no longer be the single-column primary key; list it in dropIdentityFrom",
                    column.name
                ),
            ));
        }
    }
    Ok(())
}

async fn validate_inbound_foreign_keys<D: SqlSession>(
    conn: &D,
    step: &AlterPrimaryKeyStep,
    old_primary: &[String],
    indexes: &[IndexFacts],
) -> Result<(), ApplyError> {
    let rows = conn
        .query(
            "SELECT kcu.CONSTRAINT_SCHEMA AS child_schema,
                    kcu.TABLE_NAME AS child_table,
                    kcu.CONSTRAINT_NAME AS constraint_name,
                    kcu.ORDINAL_POSITION AS ordinal_position,
                    kcu.REFERENCED_COLUMN_NAME AS referenced_column_name
               FROM information_schema.KEY_COLUMN_USAGE kcu
               JOIN information_schema.REFERENTIAL_CONSTRAINTS rc
                 ON rc.CONSTRAINT_SCHEMA = kcu.CONSTRAINT_SCHEMA
                AND rc.TABLE_NAME = kcu.TABLE_NAME
                AND rc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME
              WHERE kcu.REFERENCED_TABLE_SCHEMA = ?
                AND kcu.REFERENCED_TABLE_NAME = ?
              ORDER BY kcu.CONSTRAINT_SCHEMA, kcu.TABLE_NAME,
                       kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION",
            &[step.schema.as_str().into(), step.table.as_str().into()],
        )
        .await?;
    let mut foreign_keys = BTreeMap::<(String, String, String), InboundForeignKey>::new();
    for row in rows {
        let child_schema: String = row.try_get("child_schema")?;
        let child_table: String = row.try_get("child_table")?;
        let constraint_name: String = row.try_get("constraint_name")?;
        let key = (
            child_schema.to_ascii_lowercase(),
            child_table.to_ascii_lowercase(),
            constraint_name.to_ascii_lowercase(),
        );
        let foreign_key = foreign_keys
            .entry(key)
            .or_insert_with(|| InboundForeignKey {
                child_schema,
                child_table,
                constraint_name,
                ..InboundForeignKey::default()
            });
        foreign_key.parts.push((
            row.try_get("ordinal_position")?,
            row.try_get("referenced_column_name")?,
        ));
    }

    for mut foreign_key in foreign_keys.into_values() {
        foreign_key.parts.sort_by_key(|(position, _)| *position);
        if foreign_key
            .parts
            .iter()
            .enumerate()
            .any(|(offset, (position, _))| {
                *position != i64::try_from(offset + 1).unwrap_or(i64::MAX)
            })
        {
            return Err(pk_error(
                step,
                format!(
                    "catalog returned non-contiguous column ordinals for inbound foreign key {:?}.{:?}.{:?}",
                    foreign_key.child_schema, foreign_key.child_table, foreign_key.constraint_name
                ),
            ));
        }
        let referenced = foreign_key
            .parts
            .into_iter()
            .map(|(_, column)| column)
            .collect::<Vec<_>>();
        // InnoDB historically permits an FK to reference the leftmost prefix of
        // an index when non-standard referenced keys are enabled. Such an FK is
        // just as dependent on the primary index as one that names the full PK.
        // The lifecycle operation never migrates FKs, so require a standalone
        // exact UNIQUE key for the tuple the FK actually references.
        let uses_primary = referenced.len() <= old_primary.len()
            && referenced
                .iter()
                .zip(old_primary)
                .all(|(referenced, primary)| referenced.eq_ignore_ascii_case(primary));
        if uses_primary && !has_exact_non_primary_unique(indexes, &referenced) {
            return Err(pk_error(
                step,
                format!(
                    "inbound foreign key {:?}.{:?}.{:?} depends on primary-key prefix {referenced:?}, and no exact alternate UNIQUE key exists",
                    foreign_key.child_schema,
                    foreign_key.child_table,
                    foreign_key.constraint_name
                ),
            ));
        }
    }
    Ok(())
}

fn find_column<'a>(columns: &'a [ColumnFacts], requested: &str) -> Option<&'a ColumnFacts> {
    columns
        .iter()
        .find(|column| column.name.eq_ignore_ascii_case(requested))
}

fn same_ordered_columns(actual: &[String], expected: &[String]) -> bool {
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
}

fn pk_error(step: &AlterPrimaryKeyStep, detail: impl std::fmt::Display) -> ApplyError {
    ApplyError::Backend(format!(
        "mysql primary key {}.{}: {detail}",
        step.schema, step.table
    ))
}

async fn show_create_table<D: SqlSession>(
    conn: &D,
    schema: &str,
    table: &str,
) -> Result<String, ApplyError> {
    let schema = journal_sql::quote_ident_mysql(schema)?;
    let table = journal_sql::quote_ident_mysql(table)?;
    let row = conn
        .query_one(&format!("SHOW CREATE TABLE {schema}.{table}"), &[])
        .await?;
    // MySQL labels this column `Create Table`; positional access is stable even
    // when a host driver changes the label's casing.
    Ok(row.try_get(1usize)?)
}

fn column_definition_without_auto_increment(
    show_create: &str,
    column_name: &str,
) -> Result<String, ApplyError> {
    let clauses = create_table_clauses(show_create).ok_or_else(|| {
        ApplyError::Backend(
            "mysql primary key: could not parse SHOW CREATE TABLE output".to_string(),
        )
    })?;
    let clause = clauses
        .into_iter()
        .find(|clause| {
            column_clause_name(clause)
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(column_name))
        })
        .ok_or_else(|| {
            ApplyError::Backend(format!(
                "mysql primary key: SHOW CREATE TABLE omitted AUTO_INCREMENT column {column_name:?}"
            ))
        })?;
    let (definition, removed) = strip_top_level_auto_increment(clause);
    if !removed {
        return Err(ApplyError::Backend(format!(
            "mysql primary key: information_schema reports {column_name:?} as AUTO_INCREMENT, but its SHOW CREATE TABLE column definition has no AUTO_INCREMENT facet"
        )));
    }
    Ok(definition)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScanState {
    Normal,
    SingleQuote,
    DoubleQuote,
    Backtick,
    LineComment,
    BlockComment,
}

fn create_table_clauses(sql: &str) -> Option<Vec<&str>> {
    let bytes = sql.as_bytes();
    let mut state = ScanState::Normal;
    let mut open = None;
    let mut depth = 0usize;
    let mut clause_start = 0usize;
    let mut clauses = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            ScanState::SingleQuote | ScanState::DoubleQuote | ScanState::Backtick => {
                let delimiter = match state {
                    ScanState::SingleQuote => b'\'',
                    ScanState::DoubleQuote => b'"',
                    ScanState::Backtick => b'`',
                    _ => unreachable!(),
                };
                if byte == b'\\' && state != ScanState::Backtick {
                    index = index.saturating_add(2);
                    continue;
                }
                if byte == delimiter {
                    if bytes.get(index + 1) == Some(&delimiter) {
                        index += 2;
                        continue;
                    }
                    state = ScanState::Normal;
                }
            }
            ScanState::LineComment => {
                if byte == b'\n' || byte == b'\r' {
                    state = ScanState::Normal;
                }
            }
            ScanState::BlockComment => {
                if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    state = ScanState::Normal;
                    index += 2;
                    continue;
                }
            }
            ScanState::Normal => match byte {
                b'\'' => state = ScanState::SingleQuote,
                b'"' => state = ScanState::DoubleQuote,
                b'`' => state = ScanState::Backtick,
                b'#' => state = ScanState::LineComment,
                b'-' if bytes.get(index + 1) == Some(&b'-')
                    && bytes.get(index + 2).is_some_and(u8::is_ascii_whitespace) =>
                {
                    state = ScanState::LineComment;
                    index += 2;
                    continue;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    state = ScanState::BlockComment;
                    index += 2;
                    continue;
                }
                b'(' => {
                    if open.is_none() {
                        open = Some(index);
                        clause_start = index + 1;
                    }
                    depth += 1;
                }
                b')' if open.is_some() => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        clauses.push(sql.get(clause_start..index)?.trim());
                        return Some(clauses);
                    }
                }
                b',' if open.is_some() && depth == 1 => {
                    clauses.push(sql.get(clause_start..index)?.trim());
                    clause_start = index + 1;
                }
                _ => {}
            },
        }
        index += 1;
    }
    None
}

fn column_clause_name(clause: &str) -> Option<String> {
    let clause = clause.trim_start();
    let bytes = clause.as_bytes();
    let delimiter = match bytes.first().copied()? {
        b'`' => Some(b'`'),
        b'"' => Some(b'"'),
        _ => None,
    };
    if let Some(delimiter) = delimiter {
        let mut out = Vec::new();
        let mut index = 1usize;
        while index < bytes.len() {
            if bytes[index] == delimiter {
                if bytes.get(index + 1) == Some(&delimiter) {
                    out.push(delimiter);
                    index += 2;
                    continue;
                }
                return String::from_utf8(out).ok();
            }
            out.push(bytes[index]);
            index += 1;
        }
        return None;
    }
    let end = bytes
        .iter()
        .position(u8::is_ascii_whitespace)
        .unwrap_or(bytes.len());
    (end > 0).then(|| clause[..end].to_string())
}

fn strip_top_level_auto_increment(clause: &str) -> (String, bool) {
    let bytes = clause.as_bytes();
    let mut state = ScanState::Normal;
    let mut depth = 0usize;
    let mut ranges = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            ScanState::SingleQuote | ScanState::DoubleQuote | ScanState::Backtick => {
                let delimiter = match state {
                    ScanState::SingleQuote => b'\'',
                    ScanState::DoubleQuote => b'"',
                    ScanState::Backtick => b'`',
                    _ => unreachable!(),
                };
                if byte == b'\\' && state != ScanState::Backtick {
                    index = index.saturating_add(2);
                    continue;
                }
                if byte == delimiter {
                    if bytes.get(index + 1) == Some(&delimiter) {
                        index += 2;
                        continue;
                    }
                    state = ScanState::Normal;
                }
            }
            ScanState::LineComment => {
                if byte == b'\n' || byte == b'\r' {
                    state = ScanState::Normal;
                }
            }
            ScanState::BlockComment => {
                if byte == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    state = ScanState::Normal;
                    index += 2;
                    continue;
                }
            }
            ScanState::Normal => match byte {
                b'\'' => state = ScanState::SingleQuote,
                b'"' => state = ScanState::DoubleQuote,
                b'`' => state = ScanState::Backtick,
                b'#' => state = ScanState::LineComment,
                b'-' if bytes.get(index + 1) == Some(&b'-')
                    && bytes.get(index + 2).is_some_and(u8::is_ascii_whitespace) =>
                {
                    state = ScanState::LineComment;
                    index += 2;
                    continue;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    state = ScanState::BlockComment;
                    index += 2;
                    continue;
                }
                b'(' => depth += 1,
                b')' => depth = depth.saturating_sub(1),
                _ if depth == 0 && (byte.is_ascii_alphabetic() || byte == b'_') => {
                    let start = index;
                    index += 1;
                    while bytes.get(index).is_some_and(|byte| {
                        byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'$'
                    }) {
                        index += 1;
                    }
                    if clause[start..index].eq_ignore_ascii_case("AUTO_INCREMENT") {
                        ranges.push(start..index);
                    }
                    continue;
                }
                _ => {}
            },
        }
        index += 1;
    }
    if ranges.is_empty() {
        return (clause.trim().to_string(), false);
    }
    let mut out = String::with_capacity(clause.len());
    let mut copied = 0usize;
    for range in ranges {
        out.push_str(&clause[copied..range.start]);
        copied = range.end;
    }
    out.push_str(&clause[copied..]);
    (out.trim().to_string(), true)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use crate::apply::backend::MigrationBackend;
    use crate::driver::{Bind, DbError, Row, Value};
    use crate::model::migration::{
        Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
    };

    use super::*;
    use crate::apply::backend::mysql::MysqlBackend;

    struct RecordingSession {
        log: RefCell<Vec<String>>,
        binds: RefCell<Vec<Vec<Bind>>>,
        columns: Vec<Row>,
        indexes: Vec<Row>,
        inbound: Vec<Row>,
        show_create: String,
        journal: Option<(String, String, Phase)>,
        fail_alter: bool,
        in_transaction: bool,
    }

    impl RecordingSession {
        fn new(columns: Vec<Row>, indexes: Vec<Row>, show_create: &str) -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                binds: RefCell::new(Vec::new()),
                columns,
                indexes,
                inbound: Vec::new(),
                show_create: show_create.to_string(),
                journal: None,
                fail_alter: false,
                in_transaction: false,
            }
        }

        fn with_inbound(mut self, inbound: Vec<Row>) -> Self {
            self.inbound = inbound;
            self
        }

        fn with_journal(mut self, step: &AlterPrimaryKeyStep, phase: Phase) -> Self {
            self.journal = Some((
                step.migration.version.as_str().to_string(),
                step.migration.checksum.as_str().to_string(),
                phase,
            ));
            self
        }

        fn with_alter_failure(mut self) -> Self {
            self.fail_alter = true;
            self
        }

        fn with_active_transaction(mut self) -> Self {
            self.in_transaction = true;
            self
        }

        fn rows_for(&self, sql: &str) -> Vec<Row> {
            if sql.contains("WITH ranked AS") {
                self.journal.as_ref().map_or_else(Vec::new, |entry| {
                    vec![Row::new(
                        vec![
                            "version".into(),
                            "checksum".into(),
                            "mig_kind".into(),
                            "event_seq".into(),
                            "phase".into(),
                            // The applied read selects the stored reverse now.
                            "down".into(),
                        ],
                        vec![
                            Value::Text(entry.0.clone()),
                            Value::Text(entry.1.clone()),
                            if entry.2 == Phase::Completed {
                                Value::Text("apply".into())
                            } else {
                                Value::Null
                            },
                            Value::Int(1),
                            Value::Text(entry.2.as_str().into()),
                            Value::Null,
                        ],
                    )]
                })
            } else if sql.contains("@@SESSION.sql_mode AS sql_mode") {
                vec![Row::new(
                    vec![
                        "sql_mode".into(),
                        "time_zone".into(),
                        "max_execution_time".into(),
                        "innodb_lock_wait_timeout".into(),
                        "information_schema_stats_expiry".into(),
                        "autocommit".into(),
                        "foreign_key_checks".into(),
                        "unique_checks".into(),
                        "transaction_tracking_enabled".into(),
                        "in_transaction".into(),
                    ],
                    vec![
                        Value::Text("ANSI_QUOTES".into()),
                        Value::Text("SYSTEM".into()),
                        Value::Int(17),
                        Value::Int(23),
                        Value::Int(47),
                        Value::Int(0),
                        Value::Int(1),
                        Value::Int(0),
                        Value::Int(1),
                        Value::Int(i64::from(self.in_transaction)),
                    ],
                )]
            } else if sql.contains("FROM information_schema.COLUMNS") {
                self.columns.clone()
            } else if sql.contains("FROM information_schema.STATISTICS") {
                self.indexes.clone()
            } else if sql.contains("FROM information_schema.KEY_COLUMN_USAGE") {
                self.inbound.clone()
            } else if sql.starts_with("SHOW CREATE TABLE") {
                vec![Row::new(
                    vec!["Table".into(), "Create Table".into()],
                    vec![
                        Value::Text("orders".into()),
                        Value::Text(self.show_create.clone()),
                    ],
                )]
            } else {
                Vec::new()
            }
        }

        fn alter_statements(&self) -> Vec<String> {
            self.log
                .borrow()
                .iter()
                .filter_map(|entry| {
                    entry
                        .strip_prefix("batch: ALTER TABLE ")
                        .map(str::to_string)
                })
                .collect()
        }
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.log.borrow_mut().push(format!("batch: {sql}"));
            if self.fail_alter && sql.starts_with("ALTER TABLE ") {
                return Err(DbError::message("injected primary-key ALTER failure"));
            }
            Ok(())
        }

        async fn exec(&self, sql: &str, params: &[Bind]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            Ok(1)
        }

        async fn exec_text(&self, sql: &str, _params: &[Option<String>]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec_text: {sql}"));
            Ok(1)
        }

        async fn query(&self, sql: &str, params: &[Bind]) -> Result<Vec<Row>, DbError> {
            self.log.borrow_mut().push(format!("query: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            Ok(self.rows_for(sql))
        }

        async fn query_one(&self, sql: &str, params: &[Bind]) -> Result<Row, DbError> {
            self.log.borrow_mut().push(format!("query_one: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            self.rows_for(sql)
                .into_iter()
                .next()
                .ok_or_else(|| DbError::message("query_one: no canned row"))
        }
    }

    fn column(name: &str, nullable: bool, auto_increment: bool) -> Row {
        Row::new(
            vec!["column_name".into(), "is_nullable".into(), "extra".into()],
            vec![
                Value::Text(name.into()),
                Value::Text(if nullable { "YES" } else { "NO" }.into()),
                Value::Text(if auto_increment { "auto_increment" } else { "" }.into()),
            ],
        )
    }

    fn index(name: &str, sequence: i64, column: Option<&str>, prefix: Option<i64>) -> Row {
        Row::new(
            vec![
                "index_name".into(),
                "non_unique".into(),
                "index_type".into(),
                "seq_in_index".into(),
                "column_name".into(),
                "sub_part".into(),
                "expression".into(),
            ],
            vec![
                Value::Text(name.into()),
                Value::Int(0),
                Value::Text("BTREE".into()),
                Value::Int(sequence),
                column.map_or(Value::Null, |value| Value::Text(value.into())),
                prefix.map_or(Value::Null, Value::Int),
                Value::Null,
            ],
        )
    }

    fn inbound(
        child_schema: &str,
        child_table: &str,
        constraint: &str,
        sequence: i64,
        referenced_column: &str,
    ) -> Row {
        Row::new(
            vec![
                "child_schema".into(),
                "child_table".into(),
                "constraint_name".into(),
                "ordinal_position".into(),
                "referenced_column_name".into(),
            ],
            vec![
                Value::Text(child_schema.into()),
                Value::Text(child_table.into()),
                Value::Text(constraint.into()),
                Value::Int(sequence),
                Value::Text(referenced_column.into()),
            ],
        )
    }

    fn step(action: AlterPrimaryKeyAction) -> AlterPrimaryKeyStep {
        let mut flags = MigrationFlags::default();
        flags.destructive = !matches!(action, AlterPrimaryKeyAction::Add { .. });
        flags.requires_approval = flags.destructive;
        let version = MigrationId::generate();
        let up = "-- structured alter primary key";
        let checksum = Checksum::of(&ChecksumInput {
            up,
            down: None,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        AlterPrimaryKeyStep {
            migration: Migration {
                version,
                name: "alter_orders_primary_key".into(),
                up: up.into(),
                down: None,
                checksum,
                flags,
                owner_app: "app_test".into(),
                depends_on: Vec::new(),
                supersedes: Vec::new(),
                preconditions: Vec::new(),
                existence_guard: None,
                effect: None,
            },
            schema: "app".into(),
            table: "orders".into(),
            action,
        }
    }

    fn cfg() -> ExecutorConfig {
        ExecutorConfig::new(
            "prj_pk",
            "pk_meta",
            crate::test_fixtures::no_inject("pk_meta"),
        )
    }

    fn assert_session_restored_after(rec: &RecordingSession, operation_marker: &str) {
        let all = rec.log.borrow().join("\n");
        let snapshot = all
            .find("@@SESSION.sql_mode AS sql_mode")
            .expect("session settings are snapshotted before configuration");
        let configured = all
            .find("batch: SET SESSION sql_mode = CONCAT_WS")
            .expect("primary-key operation configures a bounded session");
        let operation = all
            .find(operation_marker)
            .unwrap_or_else(|| panic!("missing operation marker {operation_marker:?}: {all}"));
        let restored = all
            .rfind("exec: SET SESSION sql_mode = ?, SESSION time_zone = ?")
            .expect("session settings are restored");
        assert!(
            snapshot < configured && configured < operation && operation < restored,
            "snapshot/configure/operation/restore order: {all}"
        );
        assert!(
            all[restored..].contains("SESSION max_execution_time = 17")
                && all[restored..].contains("SESSION innodb_lock_wait_timeout = 23")
                && all[restored..].contains("SESSION information_schema_stats_expiry = 47")
                && all[restored..].contains("SESSION autocommit = 0")
                && all[restored..].contains("SESSION foreign_key_checks = 1")
                && all[restored..].contains("SESSION unique_checks = 0"),
            "the exact caller snapshot must be restored: {all}"
        );
    }

    const SHOW_CREATE: &str = "CREATE TABLE `orders` (\n  `id` bigint unsigned NOT NULL AUTO_INCREMENT COMMENT 'AUTO_INCREMENT is documentation, not a facet',\n  `tenant_id` bigint NOT NULL,\n  `order_id` bigint NOT NULL,\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `orders_tenant_order_uq` (`tenant_id`,`order_id`)\n) ENGINE=InnoDB AUTO_INCREMENT=42";

    #[test]
    fn show_create_rewrite_removes_only_the_column_facet() {
        let definition = column_definition_without_auto_increment(SHOW_CREATE, "id")
            .expect("SHOW CREATE column is parsed");
        assert_eq!(
            definition,
            "`id` bigint unsigned NOT NULL  COMMENT 'AUTO_INCREMENT is documentation, not a facet'"
        );
        assert_eq!(
            definition.matches("AUTO_INCREMENT").count(),
            1,
            "the quoted comment is preserved while the top-level facet is removed"
        );
    }

    #[compio::test]
    async fn replace_combines_auto_increment_removal_and_key_swap_in_one_alter() {
        let step = step(AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["id".into()],
            columns: vec!["tenant_id".into(), "order_id".into()],
            drop_identity_from: Some(vec!["id".into()]),
        });
        let rec = RecordingSession::new(
            vec![
                column("id", false, true),
                column("tenant_id", false, false),
                column("order_id", false, false),
            ],
            vec![
                index("PRIMARY", 1, Some("id"), None),
                index("orders_old_id_uq", 1, Some("id"), None),
                index("orders_tenant_order_uq", 1, Some("tenant_id"), None),
                index("orders_tenant_order_uq", 2, Some("order_id"), None),
            ],
            SHOW_CREATE,
        )
        .with_inbound(vec![inbound("app", "items", "items_order_fk", 1, "id")]);
        let backend = MysqlBackend::new_generic(&rec);

        assert!(backend
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect("validated replacement applies"));

        let alters = rec.alter_statements();
        assert_eq!(alters.len(), 1, "one schema ALTER is emitted: {alters:?}");
        let alter = &alters[0];
        assert!(alter.starts_with("`app`.`orders` MODIFY COLUMN `id` bigint unsigned NOT NULL"));
        assert!(alter.contains("COMMENT 'AUTO_INCREMENT is documentation, not a facet'"));
        assert!(alter.contains(", DROP PRIMARY KEY, ADD PRIMARY KEY (`tenant_id`, `order_id`)"));
        let all = rec.log.borrow().join("\n");
        assert!(
            !all.lines().any(|line| {
                let compact = line
                    .to_ascii_lowercase()
                    .chars()
                    .filter(|character| !character.is_whitespace())
                    .collect::<String>();
                compact.contains("foreign_key_checks=0")
                    || compact.contains("foreign_key_checks=off")
                    || compact.contains("foreign_key_checks=false")
            }),
            "FK checks are never disabled through any supported assignment spelling: {all}"
        );
        assert!(all.contains("SESSION foreign_key_checks = 1"));
        let locked = all
            .find("LOCK TABLES `app`.`orders` WRITE")
            .expect("target table lock");
        let validated = all
            .find("FROM information_schema.COLUMNS")
            .expect("catalog validation");
        let started = all
            .find("INSERT INTO `pk_meta_migrations`.schema_migrations_inflight")
            .expect("started marker");
        let altered = all.find("batch: ALTER TABLE").expect("table alteration");
        let unlocked = all.find("batch: UNLOCK TABLES").expect("table unlock");
        let completed = all
            .rfind("INSERT INTO `pk_meta_migrations`.schema_migrations")
            .expect("completed event");
        assert!(
            locked < validated
                && validated < started
                && started < altered
                && altered < unlocked
                && unlocked < completed,
            "locked validation and two-phase order: {all}"
        );
        assert_session_restored_after(&rec, "batch: COMMIT");
    }

    #[compio::test]
    async fn identity_transition_is_refused_without_drop_identity_from() {
        let step = step(AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["id".into()],
            columns: vec!["tenant_id".into(), "order_id".into()],
            drop_identity_from: None,
        });
        let rec = RecordingSession::new(
            vec![
                column("id", false, true),
                column("tenant_id", false, false),
                column("order_id", false, false),
            ],
            vec![
                index("PRIMARY", 1, Some("id"), None),
                index("orders_tenant_order_uq", 1, Some("tenant_id"), None),
                index("orders_tenant_order_uq", 2, Some("order_id"), None),
            ],
            SHOW_CREATE,
        );
        let backend = MysqlBackend::new_generic(&rec);
        let error = backend
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("implicit identity removal is refused");
        assert!(error.to_string().contains("dropIdentityFrom"));
        assert!(rec.alter_statements().is_empty());
        assert!(rec
            .log
            .borrow()
            .iter()
            .any(|entry| entry == "batch: UNLOCK TABLES"));
        assert_session_restored_after(&rec, "batch: UNLOCK TABLES");
    }

    #[compio::test]
    async fn ddl_failure_releases_table_lock_and_restores_the_session() {
        let step = step(AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["id".into()],
            columns: vec!["tenant_id".into(), "order_id".into()],
            drop_identity_from: Some(vec!["id".into()]),
        });
        let rec = RecordingSession::new(
            vec![
                column("id", false, true),
                column("tenant_id", false, false),
                column("order_id", false, false),
            ],
            vec![
                index("PRIMARY", 1, Some("id"), None),
                index("orders_old_id_uq", 1, Some("id"), None),
                index("orders_tenant_order_uq", 1, Some("tenant_id"), None),
                index("orders_tenant_order_uq", 2, Some("order_id"), None),
            ],
            SHOW_CREATE,
        )
        .with_alter_failure();

        let error = MysqlBackend::new_generic(&rec)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("injected ALTER failure is surfaced");
        assert!(matches!(error, ApplyError::MigrationFailed { .. }));
        assert_eq!(
            rec.alter_statements().len(),
            1,
            "the key swap remains one ALTER"
        );
        let all = rec.log.borrow().join("\n");
        assert!(all.contains("INSERT INTO `pk_meta_migrations`.schema_migrations_inflight"));
        assert!(all.contains("batch: UNLOCK TABLES"));
        assert!(
            !all.contains("INSERT INTO `pk_meta_migrations`.schema_migrations\n"),
            "a failed ALTER must retain only its inflight recovery marker: {all}"
        );
        assert_session_restored_after(&rec, "batch: UNLOCK TABLES");
    }

    #[compio::test]
    async fn expected_primary_key_must_match_exact_order() {
        let step = step(AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["id", "tenant_id"]
                .into_iter()
                .map(str::to_string)
                .collect(),
            columns: vec!["order_id".into()],
            drop_identity_from: None,
        });
        let rec = RecordingSession::new(
            vec![
                column("id", false, false),
                column("tenant_id", false, false),
                column("order_id", false, false),
            ],
            vec![
                index("PRIMARY", 1, Some("tenant_id"), None),
                index("PRIMARY", 2, Some("id"), None),
                index("orders_order_uq", 1, Some("order_id"), None),
            ],
            SHOW_CREATE,
        );
        let backend = MysqlBackend::new_generic(&rec);
        let error = backend
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("reordered expected tuple is drift");
        assert!(error.to_string().contains("live ordered key"));
        assert!(rec.alter_statements().is_empty());
    }

    #[compio::test]
    async fn single_to_composite_replacement_rejects_expected_columns_drift() {
        let step = step(AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["tenant_id".into()],
            columns: vec!["tenant_id".into(), "order_id".into()],
            drop_identity_from: None,
        });
        let rec = RecordingSession::new(
            vec![
                column("id", false, false),
                column("tenant_id", false, false),
                column("order_id", false, false),
            ],
            vec![
                index("PRIMARY", 1, Some("id"), None),
                index("orders_tenant_order_uq", 1, Some("tenant_id"), None),
                index("orders_tenant_order_uq", 2, Some("order_id"), None),
            ],
            SHOW_CREATE,
        );
        let error = MysqlBackend::new_generic(&rec)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("a stale single-column precondition is drift");
        assert!(error.to_string().contains("live ordered key"));
        assert!(rec.alter_statements().is_empty());
    }

    #[compio::test]
    async fn exact_composite_to_single_replacement_succeeds() {
        let step = step(AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["tenant_id".into(), "order_id".into()],
            columns: vec!["id".into()],
            drop_identity_from: None,
        });
        let rec = RecordingSession::new(
            vec![
                column("id", false, false),
                column("tenant_id", false, false),
                column("order_id", false, false),
            ],
            vec![
                index("PRIMARY", 1, Some("tenant_id"), None),
                index("PRIMARY", 2, Some("order_id"), None),
                index("orders_id_uq", 1, Some("id"), None),
            ],
            SHOW_CREATE,
        );
        let backend = MysqlBackend::new_generic(&rec);

        assert!(backend
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect("exact composite-to-single replacement applies"));
        assert_eq!(
            rec.alter_statements(),
            vec!["`app`.`orders` DROP PRIMARY KEY, ADD PRIMARY KEY (`id`)"]
        );
    }

    #[compio::test]
    async fn drop_auto_increment_requires_and_honors_drop_identity_from() {
        let columns = vec![column("id", false, true)];
        let indexes = vec![index("PRIMARY", 1, Some("id"), None)];
        let declared = step(AlterPrimaryKeyAction::Drop {
            expected_columns: vec!["id".into()],
            drop_identity_from: Some(vec!["id".into()]),
        });
        let rec = RecordingSession::new(columns.clone(), indexes.clone(), SHOW_CREATE);
        assert!(MysqlBackend::new_generic(&rec)
            .alter_primary_key(
                &cfg(),
                &declared,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect("declared AUTO_INCREMENT removal applies"));
        let alters = rec.alter_statements();
        assert_eq!(alters.len(), 1);
        assert!(alters[0].contains("MODIFY COLUMN `id` bigint unsigned NOT NULL"));
        assert!(alters[0].ends_with(", DROP PRIMARY KEY"));

        let omitted = step(AlterPrimaryKeyAction::Drop {
            expected_columns: vec!["id".into()],
            drop_identity_from: None,
        });
        let rec = RecordingSession::new(columns, indexes, SHOW_CREATE);
        let error = MysqlBackend::new_generic(&rec)
            .alter_primary_key(
                &cfg(),
                &omitted,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("implicit AUTO_INCREMENT removal is refused on drop");
        assert!(error.to_string().contains("dropIdentityFrom"));
        assert!(rec.alter_statements().is_empty());
    }

    #[compio::test]
    async fn drop_identity_from_must_name_a_live_auto_increment_column() {
        let step = step(AlterPrimaryKeyAction::Drop {
            expected_columns: vec!["id".into()],
            drop_identity_from: Some(vec!["id".into()]),
        });
        let rec = RecordingSession::new(
            vec![column("id", false, false)],
            vec![index("PRIMARY", 1, Some("id"), None)],
            SHOW_CREATE,
        );
        let error = MysqlBackend::new_generic(&rec)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("declared identity transition is itself a live precondition");
        assert!(error.to_string().contains("is not AUTO_INCREMENT"));
        assert!(rec.alter_statements().is_empty());
        assert!(
            !rec.log.borrow().join("\n").contains("SHOW CREATE TABLE"),
            "catalog identity mismatch fails before stored-definition parsing"
        );
    }

    #[compio::test]
    async fn add_requires_no_primary_and_an_exact_not_null_candidate() {
        let step = step(AlterPrimaryKeyAction::Add {
            columns: vec!["tenant_id".into(), "order_id".into()],
        });
        let candidate = vec![
            index("orders_tenant_order_uq", 1, Some("tenant_id"), None),
            index("orders_tenant_order_uq", 2, Some("order_id"), None),
        ];
        let rec = RecordingSession::new(
            vec![
                column("tenant_id", false, false),
                column("order_id", false, false),
            ],
            candidate,
            SHOW_CREATE,
        );
        let backend = MysqlBackend::new_generic(&rec);
        assert!(backend
            .alter_primary_key(&cfg(), &step, Approval::None, &ApprovalScope::All, "tester",)
            .await
            .expect("add applies"));
        let alters = rec.alter_statements();
        assert_eq!(alters.len(), 1);
        assert_eq!(
            alters[0],
            "`app`.`orders` ADD PRIMARY KEY (`tenant_id`, `order_id`)"
        );

        let rec_with_pk = RecordingSession::new(
            vec![
                column("tenant_id", false, false),
                column("order_id", false, false),
            ],
            vec![
                index("PRIMARY", 1, Some("tenant_id"), None),
                index("PRIMARY", 2, Some("order_id"), None),
                index("orders_tenant_order_uq", 1, Some("tenant_id"), None),
                index("orders_tenant_order_uq", 2, Some("order_id"), None),
            ],
            SHOW_CREATE,
        );
        let error = MysqlBackend::new_generic(&rec_with_pk)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("add over an existing PK is refused");
        assert!(error
            .to_string()
            .contains("requires no current primary key"));
    }

    #[compio::test]
    async fn nullable_or_prefix_only_candidate_is_refused() {
        let step = step(AlterPrimaryKeyAction::Add {
            columns: vec!["public_id".into()],
        });
        let nullable = RecordingSession::new(
            vec![column("public_id", true, false)],
            vec![index("orders_public_uq", 1, Some("public_id"), None)],
            SHOW_CREATE,
        );
        let error = MysqlBackend::new_generic(&nullable)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("nullable candidate is refused");
        assert!(error.to_string().contains("already be NOT NULL"));

        let prefix = RecordingSession::new(
            vec![column("public_id", false, false)],
            vec![index("orders_public_uq", 1, Some("public_id"), Some(16))],
            SHOW_CREATE,
        );
        let error = MysqlBackend::new_generic(&prefix)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("prefix uniqueness is not a candidate key");
        assert!(error.to_string().contains("exact pre-existing"));
    }

    #[compio::test]
    async fn inbound_fk_requires_exact_alternate_unique_key() {
        let step = step(AlterPrimaryKeyAction::Drop {
            expected_columns: vec!["tenant_id".into(), "order_id".into()],
            drop_identity_from: None,
        });
        let columns = vec![
            column("tenant_id", false, false),
            column("order_id", false, false),
        ];
        let primary = vec![
            index("PRIMARY", 1, Some("tenant_id"), None),
            index("PRIMARY", 2, Some("order_id"), None),
        ];
        let inbound_rows = vec![
            inbound("app", "items", "items_order_fk", 1, "tenant_id"),
            inbound("app", "items", "items_order_fk", 2, "order_id"),
        ];
        let unsafe_rec = RecordingSession::new(columns.clone(), primary.clone(), SHOW_CREATE)
            .with_inbound(inbound_rows.clone());
        let error = MysqlBackend::new_generic(&unsafe_rec)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("the only referenced key cannot be removed");
        assert!(error.to_string().contains("no exact alternate UNIQUE key"));
        assert!(unsafe_rec.alter_statements().is_empty());

        let mut safe_indexes = primary;
        safe_indexes.push(index("orders_old_pk_uq", 1, Some("tenant_id"), None));
        safe_indexes.push(index("orders_old_pk_uq", 2, Some("order_id"), None));
        let safe_rec =
            RecordingSession::new(columns, safe_indexes, SHOW_CREATE).with_inbound(inbound_rows);
        assert!(MysqlBackend::new_generic(&safe_rec)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect("exact alternate protects inbound FK"));
        assert_eq!(
            safe_rec.alter_statements(),
            vec!["`app`.`orders` DROP PRIMARY KEY"]
        );
    }

    #[compio::test]
    async fn replace_refuses_inbound_fk_without_exact_alternate_unique_key() {
        let step = step(AlterPrimaryKeyAction::Replace {
            expected_columns: vec!["tenant_id".into(), "order_id".into()],
            columns: vec!["id".into()],
            drop_identity_from: None,
        });
        let columns = vec![
            column("id", false, false),
            column("tenant_id", false, false),
            column("order_id", false, false),
        ];
        let primary_and_target = vec![
            index("PRIMARY", 1, Some("tenant_id"), None),
            index("PRIMARY", 2, Some("order_id"), None),
            index("orders_id_uq", 1, Some("id"), None),
        ];
        let inbound_rows = vec![
            inbound("app", "items", "items_order_fk", 1, "tenant_id"),
            inbound("app", "items", "items_order_fk", 2, "order_id"),
        ];
        let unsafe_rec =
            RecordingSession::new(columns.clone(), primary_and_target.clone(), SHOW_CREATE)
                .with_inbound(inbound_rows.clone());
        let error = MysqlBackend::new_generic(&unsafe_rec)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("replace cannot remove the inbound FK's only key");
        assert!(error.to_string().contains("no exact alternate UNIQUE key"));
        assert!(unsafe_rec.alter_statements().is_empty());

        let mut safe_indexes = primary_and_target;
        safe_indexes.push(index("orders_old_pk_uq", 1, Some("tenant_id"), None));
        safe_indexes.push(index("orders_old_pk_uq", 2, Some("order_id"), None));
        let safe_rec =
            RecordingSession::new(columns, safe_indexes, SHOW_CREATE).with_inbound(inbound_rows);
        assert!(MysqlBackend::new_generic(&safe_rec)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect("an exact alternate keeps the old referenced tuple valid"));
        assert_eq!(
            safe_rec.alter_statements(),
            vec!["`app`.`orders` DROP PRIMARY KEY, ADD PRIMARY KEY (`id`)"]
        );
    }

    #[compio::test]
    async fn inbound_fk_on_primary_prefix_requires_its_own_exact_alternate() {
        let step = step(AlterPrimaryKeyAction::Drop {
            expected_columns: vec!["tenant_id".into(), "order_id".into()],
            drop_identity_from: None,
        });
        let columns = vec![
            column("tenant_id", false, false),
            column("order_id", false, false),
        ];
        let mut indexes = vec![
            index("PRIMARY", 1, Some("tenant_id"), None),
            index("PRIMARY", 2, Some("order_id"), None),
            // An alternate for the full PK does not preserve the FK's actual
            // one-column referenced tuple.
            index("orders_old_pk_uq", 1, Some("tenant_id"), None),
            index("orders_old_pk_uq", 2, Some("order_id"), None),
        ];
        let inbound_rows = vec![inbound("app", "items", "items_tenant_fk", 1, "tenant_id")];
        let unsafe_rec = RecordingSession::new(columns.clone(), indexes.clone(), SHOW_CREATE)
            .with_inbound(inbound_rows.clone());
        let error = MysqlBackend::new_generic(&unsafe_rec)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect_err("a full-tuple alternate cannot protect a prefix FK");
        assert!(error.to_string().contains("primary-key prefix"));
        assert!(unsafe_rec.alter_statements().is_empty());

        indexes.push(index("orders_tenant_uq", 1, Some("tenant_id"), None));
        let safe_rec =
            RecordingSession::new(columns, indexes, SHOW_CREATE).with_inbound(inbound_rows);
        assert!(MysqlBackend::new_generic(&safe_rec)
            .alter_primary_key(
                &cfg(),
                &step,
                Approval::Approved,
                &ApprovalScope::All,
                "tester",
            )
            .await
            .expect("the exact prefix alternate protects the inbound FK"));
        assert_eq!(
            safe_rec.alter_statements(),
            vec!["`app`.`orders` DROP PRIMARY KEY"]
        );
    }

    #[compio::test]
    async fn completed_matching_step_skips_before_approval_or_catalog_reads() {
        let step = step(AlterPrimaryKeyAction::Drop {
            expected_columns: vec!["id".into()],
            drop_identity_from: None,
        });
        let rec = RecordingSession::new(Vec::new(), Vec::new(), SHOW_CREATE)
            .with_journal(&step, Phase::Completed);
        let backend = MysqlBackend::new_generic(&rec);
        let empty_scope = ApprovalScope::Versions(Default::default());
        assert!(!backend
            .alter_primary_key(&cfg(), &step, Approval::None, &empty_scope, "tester")
            .await
            .expect("matching completed operation is an idempotent skip"));
        let all = rec.log.borrow().join("\n");
        assert!(!all.contains("information_schema") && !all.contains("ALTER TABLE"));
    }

    #[compio::test]
    async fn active_caller_transaction_is_refused_before_session_configuration_or_locks() {
        let step = step(AlterPrimaryKeyAction::Add {
            columns: vec!["id".into()],
        });
        let rec =
            RecordingSession::new(Vec::new(), Vec::new(), SHOW_CREATE).with_active_transaction();

        let error = MysqlBackend::new_generic(&rec)
            .alter_primary_key(&cfg(), &step, Approval::None, &ApprovalScope::All, "tester")
            .await
            .expect_err("caller-owned transaction must not be committed by session setup");
        assert!(error.to_string().contains("dedicated idle session"));
        let all = rec.log.borrow().join("\n");
        assert!(all.contains("@@SESSION.autocommit AS autocommit"));
        assert!(!all.contains("batch: SET SESSION"), "{all}");
        assert!(!all.contains("LOCK TABLES"), "{all}");
        assert!(!all.contains("ALTER TABLE"), "{all}");
    }
}
