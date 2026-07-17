//! Crash-safe, cursor-paged MySQL backfill execution.
//!
//! Each batch selects and locks at most `batch_size` unique cursor keys, updates
//! exactly those keys with native parameters, and advances the engine-owned
//! progress row in the same InnoDB transaction. A retry therefore starts after
//! the last committed key and never trusts an uncommitted cursor.

use std::collections::BTreeMap;
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::apply::backend::{BackfillError, BackfillOutcome, BackfillProgressEntry, BackfillSpec};
use crate::apply::executor::ApplyError;
use crate::apply::journal::{CompletedRecord, EventKind, JournalError};
use crate::conn::ExecutorConfig;
use crate::driver::{Bind, Row, SqlSession};
use crate::model::backfill::{
    generate_per_row_value, CursorColumnContract, CursorComparison, CursorContract,
    CursorScalarType, CursorTuple,
};
use crate::model::ir::{CursorStability, IrScalar, PerRowGenerator};
use crate::model::migration::{Checksum, MigrationId};

use super::{journal_sql, session};

const GUARD_PLANNED: &str = "planned";
const GUARD_INSTALLED: &str = "installed";
const GUARD_CLEANUP_PENDING: &str = "cleanupPending";
const GUARD_CLEANED: &str = "cleaned";
const EXTERNAL_INVARIANT: &str = "externalInvariant";

#[derive(Debug, Clone, Copy)]
struct ExpectedProgressColumn {
    name: &'static str,
    column_type: &'static str,
    nullable: &'static str,
    character_set: Option<&'static str>,
    collation: Option<&'static str>,
    column_key: &'static str,
}

const PROGRESS_COLUMNS: &[ExpectedProgressColumn] = &[
    progress_column(
        "backfill_id",
        "varchar(255)",
        "NO",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "PRI",
    ),
    progress_column(
        "checksum",
        "varchar(255)",
        "NO",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "",
    ),
    progress_column(
        "name",
        "varchar(255)",
        "NO",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "",
    ),
    progress_column(
        "target_schema",
        "varchar(255)",
        "NO",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "",
    ),
    progress_column(
        "target_table",
        "varchar(255)",
        "NO",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "",
    ),
    progress_column(
        "cursor_columns",
        "longtext",
        "NO",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "",
    ),
    progress_column(
        "cursor_contract",
        "longtext",
        "NO",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "",
    ),
    progress_column(
        "cursor_stability_mode",
        "varchar(32)",
        "NO",
        Some("ascii"),
        Some("ascii_bin"),
        "",
    ),
    progress_column(
        "external_invariant_name",
        "varchar(255)",
        "YES",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "",
    ),
    progress_column(
        "guard_name",
        "varchar(64)",
        "YES",
        Some("ascii"),
        Some("ascii_bin"),
        "",
    ),
    progress_column(
        "guard_definition_hash",
        "char(64)",
        "YES",
        Some("ascii"),
        Some("ascii_bin"),
        "",
    ),
    progress_column(
        "guard_state",
        "varchar(32)",
        "NO",
        Some("ascii"),
        Some("ascii_bin"),
        "",
    ),
    progress_column(
        "last_cursor",
        "longtext",
        "YES",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "",
    ),
    progress_column(
        "end_cursor",
        "longtext",
        "YES",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "",
    ),
    progress_column(
        "cohort_fingerprint",
        "char(64)",
        "YES",
        Some("ascii"),
        Some("ascii_bin"),
        "",
    ),
    progress_column(
        "checkpoint_fingerprint",
        "char(64)",
        "YES",
        Some("ascii"),
        Some("ascii_bin"),
        "",
    ),
    progress_column("cohort_initialized", "tinyint(1)", "NO", None, None, ""),
    progress_column("rows_done", "bigint unsigned", "NO", None, None, ""),
    progress_column("batches_done", "bigint unsigned", "NO", None, None, ""),
    progress_column("complete", "tinyint(1)", "NO", None, None, ""),
    progress_column(
        "applied_by",
        "varchar(255)",
        "NO",
        Some("utf8mb4"),
        Some("utf8mb4_bin"),
        "",
    ),
    progress_column("started_at", "timestamp(6)", "NO", None, None, ""),
    progress_column("updated_at", "timestamp(6)", "NO", None, None, ""),
];

const fn progress_column(
    name: &'static str,
    column_type: &'static str,
    nullable: &'static str,
    character_set: Option<&'static str>,
    collation: Option<&'static str>,
    column_key: &'static str,
) -> ExpectedProgressColumn {
    ExpectedProgressColumn {
        name,
        column_type,
        nullable,
        character_set,
        collation,
        column_key,
    }
}

#[derive(Debug, Clone)]
struct Progress {
    target_schema: String,
    target_table: String,
    cursor_columns_json: String,
    cursor_contract_json: String,
    cursor_stability_mode: String,
    external_invariant_name: Option<String>,
    guard_name: Option<String>,
    guard_definition_hash: Option<String>,
    guard_state: String,
    last_cursor_json: Option<String>,
    end_cursor_json: Option<String>,
    cohort_fingerprint: Option<String>,
    checkpoint_fingerprint: Option<String>,
    cohort_initialized: bool,
    complete: bool,
    exists: bool,
    checksum: Option<String>,
}

#[derive(Debug, Clone)]
struct CursorRuntimeColumn {
    name: String,
    quoted: String,
    scalar_type: CursorScalarType,
    bind_expression: String,
}

#[derive(Debug, Clone)]
struct CursorRuntime {
    columns: Vec<CursorRuntimeColumn>,
    contract: CursorContract,
}

#[derive(Debug, Clone)]
struct ValidatedProgress {
    last_cursor: Option<CursorTuple>,
    end_cursor: Option<CursorTuple>,
    cohort_initialized: bool,
    complete: bool,
    guard_state: String,
}

#[derive(Debug, Clone)]
struct GuardDescriptor {
    name: String,
    action_statement: String,
    create_sql: String,
    definition_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedGuardExpectation {
    FreshAbsent,
    Installed,
}

/// Run a backfill until its tail is reached.
pub(crate) async fn run_backfill<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
    applied_by: &str,
) -> Result<BackfillOutcome, ApplyError> {
    let contract = validate_spec(spec)?.clone();
    session::configure_data_session(conn, cfg).await?;
    ensure_progress_table(conn, cfg).await?;

    // The plan-step version is stable across content edits and is therefore the
    // progress key. Content-derived ids would orphan the old cursor on an edit
    // and could silently restart a changed transform from the beginning.
    let backfill_id = version.as_str().to_string();
    let guard = match &spec.cursor_stability {
        CursorStability::GuardUpdates => Some(guard_descriptor(version, spec)?),
        CursorStability::ExternalInvariant { .. } => None,
    };
    let mut progress = read_progress(conn, cfg, &backfill_id).await?;
    if !progress.exists {
        initialize_obligation(
            conn,
            cfg,
            &backfill_id,
            checksum,
            spec,
            &contract,
            guard.as_ref(),
            applied_by,
        )
        .await?;
        progress = read_progress(conn, cfg, &backfill_id).await?;
    }
    let mut validated = validate_resume_progress(
        &progress,
        &backfill_id,
        checksum,
        spec,
        &contract,
        guard.as_ref(),
    )?;

    let resumed = validated.cohort_initialized || validated.last_cursor.is_some();
    let started = Instant::now();
    if validated.complete {
        finalize_backfill(
            conn,
            cfg,
            version,
            checksum,
            spec,
            guard.as_ref(),
            &validated.guard_state,
            applied_by,
            0,
        )
        .await?;
        return Ok(BackfillOutcome {
            backfill_id,
            batches: 0,
            rows_updated: 0,
            resumed,
            complete: true,
        });
    }
    let schema_q = journal_sql::quote_ident_mysql(&spec.schema)?;
    let table_q = quote_bare("table", &spec.table)?;
    let qualified_table = format!("{schema_q}.{table_q}");

    if let Some(guard) = &guard {
        ensure_guard_installed(
            conn,
            cfg,
            &backfill_id,
            checksum,
            spec,
            guard,
            &validated.guard_state,
            validated.cohort_initialized,
        )
        .await?;
    }
    if !validated.cohort_initialized {
        initialize_cohort(
            conn,
            cfg,
            &backfill_id,
            checksum,
            &qualified_table,
            spec,
            &contract,
            guard.as_ref(),
        )
        .await?;
        progress = read_progress(conn, cfg, &backfill_id).await?;
        validated = validate_resume_progress(
            &progress,
            &backfill_id,
            checksum,
            spec,
            &contract,
            guard.as_ref(),
        )?;
    }

    let mut last_cursor = validated.last_cursor;
    let end_cursor = validated.end_cursor;
    let mut batches = 0_u64;
    let mut rows_updated = 0_u64;

    if let Some(end_cursor) = end_cursor.as_ref() {
        loop {
            let selected = run_one_batch(
                conn,
                cfg,
                &backfill_id,
                checksum,
                &qualified_table,
                spec,
                &contract,
                guard.as_ref(),
                last_cursor.as_ref(),
                end_cursor,
            )
            .await?;

            if selected.is_empty() {
                break;
            }

            let selected_count = selected.len() as u64;
            last_cursor = selected.last().cloned();
            batches = batches.saturating_add(1);
            rows_updated = rows_updated.saturating_add(selected_count);

            crate::fault::trip(crate::fault::points::BACKFILL_MID_BATCHES)?;

            if selected_count < u64::from(spec.batch_size) {
                break;
            }
        }
    }

    mark_durable_complete(
        conn,
        cfg,
        &backfill_id,
        checksum,
        &qualified_table,
        spec,
        &contract,
        guard.as_ref(),
    )
    .await?;
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    finalize_backfill(
        conn,
        cfg,
        version,
        checksum,
        spec,
        guard.as_ref(),
        if guard.is_some() {
            GUARD_CLEANUP_PENDING
        } else {
            EXTERNAL_INVARIANT
        },
        applied_by,
        exec_ms,
    )
    .await?;

    Ok(BackfillOutcome {
        backfill_id,
        batches,
        rows_updated,
        resumed,
        complete: true,
    })
}

fn validate_spec(spec: &BackfillSpec) -> Result<&CursorContract, ApplyError> {
    quote_bare("table", &spec.table)?;
    if spec.cursor_columns.is_empty() {
        return Err(ApplyError::Backend(
            BackfillError::InvalidSpec("cursorColumns must be non-empty".to_string()).to_string(),
        ));
    }
    for column in &spec.cursor_columns {
        quote_bare("cursor component", column)?;
    }
    let contract = spec.cursor_contract.as_ref().ok_or_else(|| {
        ApplyError::Backend(
            BackfillError::InvalidSpec(
                "an executable MySQL backfill requires a live planner cursor contract".to_string(),
            )
            .to_string(),
        )
    })?;
    contract
        .validate_columns(&spec.cursor_columns)
        .map_err(|error| {
            ApplyError::Backend(BackfillError::InvalidSpec(error.to_string()).to_string())
        })?;
    if spec.batch_size == 0 {
        return Err(ApplyError::Backend(
            BackfillError::InvalidBatchSize.to_string(),
        ));
    }
    if spec.set_clause.trim().is_empty() && spec.per_row.is_empty() {
        return Err(ApplyError::Backend(
            "mysql backfill: backfill set must not be empty".to_string(),
        ));
    }
    if !spec.set_clause.trim().is_empty() {
        assert_cursor_not_mutated(&spec.set_clause, &spec.cursor_columns)?;
    }
    for (column, assignment) in &spec.per_row {
        let generator = assignment.generator();
        quote_bare("per-row destination column", column)?;
        if let Some(cursor_component) = spec
            .cursor_columns
            .iter()
            .find(|cursor| cursor.eq_ignore_ascii_case(column))
        {
            return Err(cursor_component_mutated(cursor_component));
        }
        if !assignment.matches_target(&spec.schema, &spec.table, column) {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: per-row assignment for destination {column:?} was validated for a different target; regenerate the plan from the declared schema"
            )));
        }
        if let PerRowGenerator::TypeId { prefix } = generator {
            crate::model::ir::validate_type_id_prefix(prefix).map_err(|error| {
                ApplyError::Backend(format!(
                    "mysql backfill: invalid TypeID prefix for per-row destination {column:?}: {error}"
                ))
            })?;
        }
    }
    if let CursorStability::ExternalInvariant { name } = &spec.cursor_stability {
        if name.trim().is_empty() || name.chars().count() > 255 {
            return Err(ApplyError::Backend(
                BackfillError::InvalidSpec(
                    "externalInvariant.name must contain 1..=255 visible characters".to_string(),
                )
                .to_string(),
            ));
        }
    }
    Ok(contract)
}

fn cursor_tuple_unavailable(spec: &BackfillSpec, reason: impl Into<String>) -> ApplyError {
    ApplyError::Backend(
        BackfillError::CursorTupleUnavailable {
            table: spec.table.clone(),
            cursor_columns: spec.cursor_columns.clone(),
            reason: reason.into(),
        }
        .to_string(),
    )
}

fn cursor_component_mutated(component: &str) -> ApplyError {
    ApplyError::Backend(
        BackfillError::CursorComponentMutated {
            cursor_component: component.to_string(),
        }
        .to_string(),
    )
}

fn quote_bare(what: &'static str, ident: &str) -> Result<String, ApplyError> {
    let valid = ident
        .as_bytes()
        .first()
        .is_some_and(|first| first.is_ascii_alphabetic() || *first == b'_')
        && ident
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_');
    if !valid {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: invalid {what} identifier {ident:?}"
        )));
    }
    Ok(format!("`{ident}`"))
}

fn optional_text(value: Option<&str>) -> Bind {
    value.map_or(Bind::Null, |value| Bind::Text(value.to_string()))
}

/// Refuse a transform that assigns any component of the tuple it pages on. The
/// assembler emits backtick-quoted assignment targets; this scanner splits only
/// at top-level commas and therefore does not mistake a comparison inside a
/// string/function expression for an assignment target.
fn assert_cursor_not_mutated(
    set_clause: &str,
    cursor_columns: &[String],
) -> Result<(), ApplyError> {
    let bytes = set_clause.as_bytes();
    let mut start = 0_usize;
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut in_ident = false;
    let mut i = 0_usize;
    while i < bytes.len() {
        if in_string {
            if bytes[i] == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_string = false;
            }
            i += 1;
            continue;
        }
        if in_ident {
            if bytes[i] == b'`' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'`' {
                    i += 2;
                    continue;
                }
                in_ident = false;
            }
            i += 1;
            continue;
        }

        match bytes[i] {
            b'\'' => {
                in_string = true;
                i += 1;
            }
            b'`' => {
                in_ident = true;
                i += 1;
            }
            b'(' => {
                depth = depth.saturating_add(1);
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b',' if depth == 0 => {
                reject_cursor_assignment(&set_clause[start..i], cursor_columns)?;
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    reject_cursor_assignment(&set_clause[start..], cursor_columns)?;
    Ok(())
}

fn reject_cursor_assignment(assignment: &str, cursor_columns: &[String]) -> Result<(), ApplyError> {
    let assignment = assignment.trim_start();
    let Some(mut rest) = assignment.strip_prefix('`') else {
        return Ok(());
    };
    let mut target = String::new();
    loop {
        let Some(end) = rest.find('`') else {
            return Ok(());
        };
        target.push_str(&rest[..end]);
        rest = &rest[end + 1..];
        if let Some(escaped) = rest.strip_prefix('`') {
            target.push('`');
            rest = escaped;
            continue;
        }
        break;
    }
    if !rest.trim_start().starts_with('=') {
        return Ok(());
    }
    if let Some(component) = cursor_columns
        .iter()
        .find(|component| component.eq_ignore_ascii_case(&target))
    {
        return Err(cursor_component_mutated(component));
    }
    Ok(())
}

/// Read existing progress evidence without bootstrapping the progress table.
/// A missing table is the normal state before the first backfill. If an older
/// table has no checksum column, return a missing checksum so status reports
/// drift rather than trusting an unanchored cursor.
pub(super) async fn read_progress_entries<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<BackfillProgressEntry>, JournalError> {
    let catalog = conn
        .query_one(
            "SELECT CAST(EXISTS ( \
                 SELECT 1 FROM information_schema.TABLES \
                  WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'schema_backfills' \
             ) AS SIGNED) AS table_exists, \
             CAST(EXISTS ( \
                 SELECT 1 FROM information_schema.COLUMNS \
                  WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'schema_backfills' \
                    AND COLUMN_NAME = 'checksum' \
             ) AS SIGNED) AS checksum_exists",
            &[
                cfg.pg.meta_schema.as_str().into(),
                cfg.pg.meta_schema.as_str().into(),
            ],
        )
        .await?;
    let table_exists: i64 = catalog.try_get("table_exists")?;
    if table_exists == 0 {
        return Ok(Vec::new());
    }
    let checksum_exists: i64 = catalog.try_get("checksum_exists")?;
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    let checksum_expr = if checksum_exists != 0 {
        "checksum"
    } else {
        "CAST(NULL AS CHAR)"
    };
    let rows = conn
        .query(
            &format!(
                "SELECT backfill_id, {checksum_expr} AS checksum, \
                        CAST(complete AS SIGNED) AS complete \
                   FROM {meta}.schema_backfills"
            ),
            &[],
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            let complete: i64 = row.try_get("complete")?;
            Ok(BackfillProgressEntry {
                version: row.try_get("backfill_id")?,
                checksum: row.try_get("checksum")?,
                complete: complete != 0,
            })
        })
        .collect()
}

async fn ensure_progress_table<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<(), ApplyError> {
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_backfills (
            backfill_id    VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin
                           NOT NULL PRIMARY KEY,
            checksum       VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            name           VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            target_schema  VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            target_table   VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            cursor_columns LONGTEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            cursor_contract LONGTEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            cursor_stability_mode VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
            external_invariant_name VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin,
            guard_name     VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin,
            guard_definition_hash CHAR(64) CHARACTER SET ascii COLLATE ascii_bin,
            guard_state    VARCHAR(32) CHARACTER SET ascii COLLATE ascii_bin NOT NULL,
            last_cursor    LONGTEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin,
            end_cursor     LONGTEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin,
            cohort_fingerprint CHAR(64) CHARACTER SET ascii COLLATE ascii_bin,
            checkpoint_fingerprint CHAR(64) CHARACTER SET ascii COLLATE ascii_bin,
            cohort_initialized BOOLEAN NOT NULL DEFAULT FALSE,
            rows_done      BIGINT UNSIGNED NOT NULL DEFAULT 0,
            batches_done   BIGINT UNSIGNED NOT NULL DEFAULT 0,
            complete       BOOLEAN NOT NULL DEFAULT FALSE,
            applied_by     VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL,
            started_at     TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at     TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
                                           ON UPDATE CURRENT_TIMESTAMP(6)
        ) ENGINE=InnoDB"
    ))
    .await?;
    let tables = conn
        .query(
            "SELECT ENGINE AS table_engine
               FROM information_schema.TABLES
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'schema_backfills'",
            &[cfg.pg.meta_schema.as_str().into()],
        )
        .await?;
    let columns = conn
        .query(
            "SELECT COLUMN_NAME AS column_name, COLUMN_TYPE AS column_type,
                    IS_NULLABLE AS is_nullable,
                    CHARACTER_SET_NAME AS character_set_name,
                    COLLATION_NAME AS collation_name, COLUMN_KEY AS column_key
               FROM information_schema.COLUMNS
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'schema_backfills'
              ORDER BY ORDINAL_POSITION",
            &[cfg.pg.meta_schema.as_str().into()],
        )
        .await?;
    validate_progress_catalog(&tables, &columns)
}

fn validate_progress_catalog(tables: &[Row], columns: &[Row]) -> Result<(), ApplyError> {
    let [table] = tables else {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: expected exactly one schema_backfills base table, found {}",
            tables.len()
        )));
    };
    let engine: Option<String> = table.try_get("table_engine")?;
    if !engine
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("InnoDB"))
    {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: schema_backfills engine {engine:?} is not InnoDB; target writes and checkpoints would not be atomic"
        )));
    }
    if columns.len() != PROGRESS_COLUMNS.len() {
        return Err(stale_progress_schema(
            columns,
            format!(
                "expected {} columns, found {}",
                PROGRESS_COLUMNS.len(),
                columns.len()
            ),
        ));
    }
    for (position, (row, expected)) in columns.iter().zip(PROGRESS_COLUMNS).enumerate() {
        let name: String = row.try_get("column_name")?;
        let column_type: String = row.try_get("column_type")?;
        let nullable: String = row.try_get("is_nullable")?;
        let character_set: Option<String> = row.try_get("character_set_name")?;
        let collation: Option<String> = row.try_get("collation_name")?;
        let column_key: String = row.try_get("column_key")?;
        if name != expected.name
            || !column_type.eq_ignore_ascii_case(expected.column_type)
            || !nullable.eq_ignore_ascii_case(expected.nullable)
            || !optional_catalog_value_matches(character_set.as_deref(), expected.character_set)
            || !optional_catalog_value_matches(collation.as_deref(), expected.collation)
            || !column_key.eq_ignore_ascii_case(expected.column_key)
        {
            return Err(stale_progress_schema(
                columns,
                format!(
                    "column {} is ({name:?}, {column_type:?}, nullable={nullable:?}, charset={character_set:?}, collation={collation:?}, key={column_key:?}); expected ({:?}, {:?}, nullable={:?}, charset={:?}, collation={:?}, key={:?})",
                    position + 1,
                    expected.name,
                    expected.column_type,
                    expected.nullable,
                    expected.character_set,
                    expected.collation,
                    expected.column_key,
                ),
            ));
        }
    }
    Ok(())
}

fn optional_catalog_value_matches(actual: Option<&str>, expected: Option<&str>) -> bool {
    match (actual, expected) {
        (Some(actual), Some(expected)) => actual.eq_ignore_ascii_case(expected),
        (None, None) => true,
        _ => false,
    }
}

fn stale_progress_schema(columns: &[Row], detail: String) -> ApplyError {
    let actual = columns
        .iter()
        .map(|row| {
            row.try_get::<_, String>("column_name")
                .unwrap_or_else(|_| "<invalid catalog row>".to_string())
        })
        .collect::<Vec<_>>();
    let expected = PROGRESS_COLUMNS
        .iter()
        .map(|column| column.name)
        .collect::<Vec<_>>();
    ApplyError::Backend(format!(
        "mysql backfill: schema_backfills uses a stale pre-release schema {actual:?}: {detail}; expected {expected:?}; recreate the development metadata schema with the current cursorColumns/typed-tuple layout"
    ))
}

async fn read_progress<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
) -> Result<Progress, ApplyError> {
    read_progress_with_lock(conn, cfg, backfill_id, false).await
}

async fn read_progress_with_lock<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    for_update: bool,
) -> Result<Progress, ApplyError> {
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    let lock = if for_update { " FOR UPDATE" } else { "" };
    let rows = conn
        .query(
            &format!(
                "SELECT target_schema, target_table, cursor_columns, cursor_contract, cursor_stability_mode,
                        external_invariant_name, guard_name, guard_definition_hash,
                        guard_state, last_cursor, end_cursor, cohort_fingerprint,
                        checkpoint_fingerprint,
                        CAST(cohort_initialized AS SIGNED) AS cohort_initialized,
                        CAST(complete AS SIGNED) AS complete, checksum
                   FROM {meta}.schema_backfills WHERE backfill_id = ?{lock}"
            ),
            &[backfill_id.into()],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(Progress {
            target_schema: String::new(),
            target_table: String::new(),
            cursor_columns_json: String::new(),
            cursor_contract_json: String::new(),
            cursor_stability_mode: String::new(),
            external_invariant_name: None,
            guard_name: None,
            guard_definition_hash: None,
            guard_state: String::new(),
            last_cursor_json: None,
            end_cursor_json: None,
            cohort_fingerprint: None,
            checkpoint_fingerprint: None,
            cohort_initialized: false,
            complete: false,
            exists: false,
            checksum: None,
        });
    };
    Ok(Progress {
        target_schema: row.try_get("target_schema")?,
        target_table: row.try_get("target_table")?,
        cursor_columns_json: row.try_get("cursor_columns")?,
        cursor_contract_json: row.try_get("cursor_contract")?,
        cursor_stability_mode: row.try_get("cursor_stability_mode")?,
        external_invariant_name: row.try_get("external_invariant_name")?,
        guard_name: row.try_get("guard_name")?,
        guard_definition_hash: row.try_get("guard_definition_hash")?,
        guard_state: row.try_get("guard_state")?,
        last_cursor_json: row.try_get("last_cursor")?,
        end_cursor_json: row.try_get("end_cursor")?,
        cohort_fingerprint: row.try_get("cohort_fingerprint")?,
        checkpoint_fingerprint: row.try_get("checkpoint_fingerprint")?,
        cohort_initialized: row.try_get::<_, i64>("cohort_initialized")? != 0,
        complete: row.try_get::<_, i64>("complete")? != 0,
        exists: true,
        checksum: row.try_get("checksum")?,
    })
}

#[allow(clippy::too_many_arguments)]
async fn initialize_obligation<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    checksum: &Checksum,
    spec: &BackfillSpec,
    contract: &CursorContract,
    guard: Option<&GuardDescriptor>,
    applied_by: &str,
) -> Result<(), ApplyError> {
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    let schema_q = journal_sql::quote_ident_mysql(&spec.schema)?;
    let table_q = quote_bare("table", &spec.table)?;
    let qualified_table = format!("{schema_q}.{table_q}");
    let cursor_columns_json = serde_json::to_string(&spec.cursor_columns)
        .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))?;
    let cursor_contract_json = serde_json::to_string(contract)
        .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))?;
    let (stability_mode, external_invariant_name, guard_state) = match &spec.cursor_stability {
        CursorStability::GuardUpdates => ("guardUpdates", None, GUARD_PLANNED),
        CursorStability::ExternalInvariant { name } => {
            ("externalInvariant", Some(name.as_str()), EXTERNAL_INVARIANT)
        }
    };

    conn.batch("START TRANSACTION").await?;
    let result = async {
        validate_target_under_lock(
            conn,
            &qualified_table,
            spec,
            contract,
            guard,
            ManagedGuardExpectation::FreshAbsent,
        )
        .await?;
        let inserted = conn
            .exec(
                &format!(
                    "INSERT INTO {meta}.schema_backfills
                         (backfill_id, checksum, name, target_schema, target_table,
                          cursor_columns, cursor_contract, cursor_stability_mode,
                          external_invariant_name, guard_name, guard_definition_hash,
                          guard_state, cohort_initialized, applied_by)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, FALSE, ?)"
                ),
                &[
                    backfill_id.into(),
                    checksum.as_str().into(),
                    spec.name.as_str().into(),
                    spec.schema.as_str().into(),
                    spec.table.as_str().into(),
                    cursor_columns_json.as_str().into(),
                    cursor_contract_json.as_str().into(),
                    stability_mode.into(),
                    optional_text(external_invariant_name),
                    optional_text(guard.map(|value| value.name.as_str())),
                    optional_text(guard.map(|value| value.definition_hash.as_str())),
                    guard_state.into(),
                    applied_by.into(),
                ],
            )
            .await?;
        if inserted != 1 {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: progress initialization affected {inserted} rows for \
                 {backfill_id:?}"
            )));
        }
        Ok::<(), ApplyError>(())
    }
    .await;
    match result {
        Ok(()) => {
            if let Err(error) = conn.batch("COMMIT").await {
                rollback(
                    conn,
                    backfill_id,
                    "ambiguous guard-obligation COMMIT failure",
                )
                .await;
                return Err(ApplyError::Db(error.into()));
            }
            Ok(())
        }
        Err(error) => {
            rollback(conn, backfill_id, "guard-obligation initialization error").await;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn initialize_cohort<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    checksum: &Checksum,
    qualified_table: &str,
    spec: &BackfillSpec,
    contract: &CursorContract,
    guard: Option<&GuardDescriptor>,
) -> Result<(), ApplyError> {
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.batch("START TRANSACTION").await?;
    let result = async {
        let runtime =
            validate_target_under_lock(
                conn,
                qualified_table,
                spec,
                contract,
                guard,
                ManagedGuardExpectation::Installed,
            )
            .await?;
        let locked = conn
            .query(
                &format!(
                    "SELECT guard_state FROM {meta}.schema_backfills
                      WHERE backfill_id = ? AND checksum = ? FOR UPDATE"
                ),
                &[backfill_id.into(), checksum.as_str().into()],
            )
            .await?;
        let Some(locked) = locked.first() else {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: obligation row disappeared for {backfill_id:?}"
            )));
        };
        let guard_state: String = locked.try_get("guard_state")?;
        let expected_state = if guard.is_some() {
            GUARD_INSTALLED
        } else {
            EXTERNAL_INVARIANT
        };
        if guard_state != expected_state {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: cannot capture cohort while guard obligation is {guard_state:?}; expected {expected_state:?}"
            )));
        }

        let rows = conn
            .query(
                &build_end_cursor_sql(qualified_table, &runtime, spec.filter.as_deref()),
                &[],
            )
            .await?;
        let end_cursor = rows
            .first()
            .map(|row| decode_cursor_row(row, "end_cursor", &runtime))
            .transpose()?;
        let end_cursor_json = end_cursor
            .as_ref()
            .map(CursorTuple::to_json)
            .transpose()
            .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))?;
        let fingerprint = cohort_fingerprint(
            backfill_id,
            checksum,
            &spec.cursor_columns,
            contract,
            end_cursor_json.as_deref(),
        )?;
        let checkpoint_fingerprint = checkpoint_fingerprint(&fingerprint, None);
        let updated = conn
            .exec(
                &format!(
                    "UPDATE {meta}.schema_backfills
                        SET end_cursor = ?, cohort_fingerprint = ?,
                            checkpoint_fingerprint = ?,
                            cohort_initialized = TRUE
                      WHERE backfill_id = ? AND checksum = ?
                        AND cohort_initialized = FALSE AND guard_state = ?"
                ),
                &[
                    end_cursor_json.into(),
                    fingerprint.as_str().into(),
                    checkpoint_fingerprint.into(),
                    backfill_id.into(),
                    checksum.as_str().into(),
                    expected_state.into(),
                ],
            )
            .await?;
        if updated != 1 {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: cohort checkpoint initialization affected {updated} rows for {backfill_id:?}"
            )));
        }
        Ok::<(), ApplyError>(())
    }
    .await;
    match result {
        Ok(()) => {
            if let Err(error) = conn.batch("COMMIT").await {
                rollback(
                    conn,
                    backfill_id,
                    "ambiguous cohort initialization COMMIT failure",
                )
                .await;
                return Err(ApplyError::Db(error.into()));
            }
            Ok(())
        }
        Err(error) => {
            rollback(conn, backfill_id, "cohort initialization error").await;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn mark_durable_complete<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    checksum: &Checksum,
    qualified_table: &str,
    spec: &BackfillSpec,
    contract: &CursorContract,
    guard: Option<&GuardDescriptor>,
) -> Result<(), ApplyError> {
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.batch("START TRANSACTION").await?;
    let result = async {
        // Retain the target MDL through the completion checkpoint. In
        // particular, a concurrent DROP/ALTER TRIGGER cannot create an
        // unguarded gap between this proof and durable completion.
        validate_target_under_lock(
            conn,
            qualified_table,
            spec,
            contract,
            guard,
            ManagedGuardExpectation::Installed,
        )
        .await?;
        let progress = read_progress_with_lock(conn, cfg, backfill_id, true).await?;
        if !progress.exists {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: progress row disappeared before completion for {backfill_id:?}"
            )));
        }
        let validated = validate_resume_progress(
            &progress,
            backfill_id,
            checksum,
            spec,
            contract,
            guard,
        )?;
        if validated.complete || !validated.cohort_initialized {
            return Err(cursor_tuple_unavailable(
                spec,
                format!(
                    "invalid durable-completion source state: complete={}, cohort_initialized={}",
                    validated.complete, validated.cohort_initialized
                ),
            ));
        }

        let expected_state = if guard.is_some() {
            GUARD_INSTALLED
        } else {
            EXTERNAL_INVARIANT
        };
        let next_state = if guard.is_some() {
            GUARD_CLEANUP_PENDING
        } else {
            EXTERNAL_INVARIANT
        };
        if validated.guard_state != expected_state {
            return Err(cursor_tuple_unavailable(
                spec,
                format!(
                    "completion expected guard state {expected_state:?}, found {:?}",
                    validated.guard_state
                ),
            ));
        }
        let cohort_fingerprint = progress.cohort_fingerprint.as_deref().ok_or_else(|| {
            cursor_tuple_unavailable(spec, "initialized cohort has no cohort fingerprint")
        })?;
        let checkpoint_fingerprint =
            progress.checkpoint_fingerprint.as_deref().ok_or_else(|| {
                cursor_tuple_unavailable(spec, "initialized cohort has no checkpoint fingerprint")
            })?;
        let updated = conn
            .exec(
                &format!(
                    "UPDATE {meta}.schema_backfills
                        SET complete = TRUE, guard_state = ?
                      WHERE backfill_id = ? AND checksum = ?
                        AND cohort_initialized = TRUE AND complete = FALSE
                        AND guard_state = ? AND last_cursor <=> ?
                        AND end_cursor <=> ? AND cohort_fingerprint = ?
                        AND checkpoint_fingerprint = ?"
                ),
                &[
                    next_state.into(),
                    backfill_id.into(),
                    checksum.as_str().into(),
                    expected_state.into(),
                    optional_text(progress.last_cursor_json.as_deref()),
                    optional_text(progress.end_cursor_json.as_deref()),
                    cohort_fingerprint.into(),
                    checkpoint_fingerprint.into(),
                ],
            )
            .await?;
        if updated != 1 {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: durable completion transition affected {updated} rows after the locked progress proof"
            )));
        }
        Ok::<(), ApplyError>(())
    }
    .await;
    match result {
        Ok(()) => {
            if let Err(error) = conn.batch("COMMIT").await {
                rollback(
                    conn,
                    backfill_id,
                    "ambiguous durable-completion COMMIT failure",
                )
                .await;
                return Err(ApplyError::Db(error.into()));
            }
            Ok(())
        }
        Err(error) => {
            rollback(conn, backfill_id, "durable-completion error").await;
            Err(error)
        }
    }
}

/// Finish crash reconciliation after the durable completion checkpoint. A
/// managed guard is removed first; only then is the ordinary applied event
/// appended. Therefore an upstream completed-step skip can never strand a
/// trigger that still rejects application updates.
#[allow(clippy::too_many_arguments)]
async fn finalize_backfill<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
    guard: Option<&GuardDescriptor>,
    guard_state: &str,
    applied_by: &str,
    exec_ms: i64,
) -> Result<(), ApplyError> {
    if let Some(guard) = guard {
        match guard_state {
            GUARD_CLEANUP_PENDING => {
                cleanup_guard(conn, cfg, version.as_str(), checksum, spec, guard).await?;
            }
            GUARD_CLEANED => {}
            other => {
                return Err(ApplyError::Backend(format!(
                    "mysql backfill: completed progress has invalid guard obligation state {other:?}"
                )));
            }
        }
    } else if guard_state != EXTERNAL_INVARIANT {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: completed externalInvariant progress has guard state {guard_state:?}"
        )));
    }
    append_completed_journal(conn, cfg, version, checksum, spec, applied_by, exec_ms).await
}

#[allow(clippy::too_many_arguments)]
async fn append_completed_journal<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
    applied_by: &str,
    exec_ms: i64,
) -> Result<(), ApplyError> {
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.batch("START TRANSACTION").await?;
    let result = async {
        let progress = conn
            .query(
                &format!(
                    "SELECT checksum, CAST(complete AS SIGNED) AS complete, guard_state
                       FROM {meta}.schema_backfills
                      WHERE backfill_id = ? FOR UPDATE"
                ),
                &[version.as_str().into()],
            )
            .await?;
        let Some(progress) = progress.first() else {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: progress row disappeared for {:?}",
                version.as_str()
            )));
        };
        let progress_checksum: String = progress.try_get("checksum")?;
        if progress_checksum != checksum.as_str() {
            return Err(ApplyError::ChecksumDrift {
                version: version.as_str().to_string(),
                recorded: progress_checksum,
                expected: checksum.as_str().to_string(),
            });
        }
        let complete: i64 = progress.try_get("complete")?;
        let state: String = progress.try_get("guard_state")?;
        if complete == 0 || !matches!(state.as_str(), GUARD_CLEANED | EXTERNAL_INVARIANT) {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: refusing applied journal event before durable completion and guard cleanup (complete={complete}, guard_state={state:?})"
            )));
        }

        let latest = conn
            .query(
                &format!(
                    "SELECT event_kind, checksum
                       FROM {meta}.schema_migrations
                      WHERE version = ?
                      ORDER BY event_seq DESC LIMIT 1"
                ),
                &[version.as_str().into()],
            )
            .await?;
        let already_journaled = if let Some(row) = latest.first() {
            let event_kind_s: String = row.try_get("event_kind")?;
            let event_kind = EventKind::parse(&event_kind_s).ok_or_else(|| {
                ApplyError::Journal(crate::apply::journal::JournalError::BadEventKind(
                    event_kind_s,
                ))
            })?;
            if event_kind == EventKind::Applied {
                let recorded: String = row.try_get("checksum")?;
                if recorded != checksum.as_str() {
                    return Err(ApplyError::ChecksumDrift {
                        version: version.as_str().to_string(),
                        recorded,
                        expected: checksum.as_str().to_string(),
                    });
                }
                true
            } else {
                false
            }
        } else {
            false
        };

        if !already_journaled {
            journal_sql::record_completed_in_transaction(
                conn,
                cfg,
                CompletedRecord {
                    version: version.as_str(),
                    name: &spec.name,
                    checksum: checksum.as_str(),
                    applied_by,
                    exec_ms,
                    kind: "apply",
                },
            )
            .await
            .map_err(ApplyError::Journal)?;
        }
        Ok::<(), ApplyError>(())
    }
    .await;

    if let Err(error) = result {
        rollback(conn, version.as_str(), "finalization error").await;
        return Err(error);
    }
    if let Err(error) = conn.batch("COMMIT").await {
        rollback(
            conn,
            version.as_str(),
            "ambiguous finalization COMMIT failure",
        )
        .await;
        return Err(ApplyError::Db(error.into()));
    }
    Ok(())
}

async fn validate_cursor<D: SqlSession>(
    conn: &D,
    spec: &BackfillSpec,
) -> Result<CursorRuntime, ApplyError> {
    let expected = spec.cursor_contract.as_ref().ok_or_else(|| {
        ApplyError::Backend(
            BackfillError::InvalidSpec("missing live cursor contract".to_string()).to_string(),
        )
    })?;
    let table = conn
        .query(
            "SELECT ENGINE AS table_engine
               FROM information_schema.TABLES
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            &[spec.schema.as_str().into(), spec.table.as_str().into()],
        )
        .await?;
    let Some(table) = table.first() else {
        return Err(ApplyError::Backend(
            BackfillError::TargetNotFound(format!("{}.{}", spec.schema, spec.table)).to_string(),
        ));
    };
    let table_engine: Option<String> = table.try_get("table_engine")?;
    if !table_engine
        .as_deref()
        .is_some_and(|engine| engine.eq_ignore_ascii_case("InnoDB"))
    {
        return Err(cursor_tuple_unavailable(
            spec,
            format!(
                "target engine {table_engine:?} is not transactional InnoDB; data and checkpoint writes cannot commit atomically"
            ),
        ));
    }

    let placeholders = vec!["?"; spec.cursor_columns.len()].join(", ");
    let mut column_binds = vec![spec.schema.as_str().into(), spec.table.as_str().into()];
    column_binds.extend(spec.cursor_columns.iter().map(Bind::from));
    let columns = conn
        .query(
            &format!(
                "SELECT COLUMN_NAME AS column_name, IS_NULLABLE AS is_nullable,
                        DATA_TYPE AS data_type, COLUMN_TYPE AS column_type,
                        CHARACTER_SET_NAME AS character_set_name,
                        COLLATION_NAME AS collation_name, EXTRA AS extra,
                        GENERATION_EXPRESSION AS generation_expression
                   FROM information_schema.COLUMNS
                  WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
                    AND COLUMN_NAME IN ({placeholders})"
            ),
            &column_binds,
        )
        .await?;
    let mut by_name = BTreeMap::new();
    for row in columns {
        let name: String = row.try_get("column_name")?;
        by_name.insert(name.to_ascii_lowercase(), row);
    }
    let mut runtime_columns = Vec::with_capacity(spec.cursor_columns.len());
    let mut actual_contract = Vec::with_capacity(spec.cursor_columns.len());
    for name in &spec.cursor_columns {
        let Some(row) = by_name.get(&name.to_ascii_lowercase()) else {
            return Err(ApplyError::Backend(
                BackfillError::TargetNotFound(format!(
                    "{}.{} cursor component {name}",
                    spec.schema, spec.table
                ))
                .to_string(),
            ));
        };
        let (column_contract, bind_expression) = mysql_live_cursor_column(row, name, spec)?;
        runtime_columns.push(CursorRuntimeColumn {
            name: name.clone(),
            quoted: quote_bare("cursor component", name)?,
            scalar_type: column_contract.scalar_type,
            bind_expression,
        });
        actual_contract.push(column_contract);
    }
    let actual = CursorContract {
        columns: actual_contract,
    };
    if &actual != expected {
        return Err(cursor_tuple_unavailable(
            spec,
            format!(
                "live cursor type/collation contract drifted: planned {expected:?}, live {actual:?}"
            ),
        ));
    }

    let indexes = conn
        .query(
            "SELECT INDEX_NAME AS index_name, NON_UNIQUE AS non_unique,
                    INDEX_TYPE AS index_type, SEQ_IN_INDEX AS seq_in_index,
                    COLUMN_NAME AS column_name, SUB_PART AS sub_part,
                    EXPRESSION AS expression
               FROM information_schema.STATISTICS
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND NON_UNIQUE = 0
              ORDER BY INDEX_NAME, SEQ_IN_INDEX",
            &[spec.schema.as_str().into(), spec.table.as_str().into()],
        )
        .await?;
    if !has_exact_ordered_candidate_key(&indexes, &spec.cursor_columns)? {
        return Err(cursor_tuple_unavailable(
            spec,
            "the exact ordered tuple is not a full-column PRIMARY/UNIQUE BTREE candidate key",
        ));
    }
    Ok(CursorRuntime {
        columns: runtime_columns,
        contract: actual,
    })
}

fn mysql_live_cursor_column(
    row: &Row,
    name: &str,
    spec: &BackfillSpec,
) -> Result<(CursorColumnContract, String), ApplyError> {
    let nullable: String = row.try_get("is_nullable")?;
    if !nullable.eq_ignore_ascii_case("NO") {
        return Err(cursor_tuple_unavailable(
            spec,
            format!("cursor component {name:?} is nullable"),
        ));
    }
    let data_type: String = row.try_get("data_type")?;
    let column_type: String = row.try_get("column_type")?;
    let character_set: Option<String> = row.try_get("character_set_name")?;
    let collation: Option<String> = row.try_get("collation_name")?;
    let extra: Option<String> = row.try_get("extra")?;
    let generation_expression: Option<String> = row.try_get("generation_expression")?;
    if generation_expression
        .as_deref()
        .is_some_and(|expression| !expression.trim().is_empty())
        || extra
            .as_deref()
            .is_some_and(|value| value.to_ascii_lowercase().contains("on update"))
    {
        return Err(cursor_tuple_unavailable(
            spec,
            format!("cursor component {name:?} is generated or automatically updated"),
        ));
    }

    let lower_data_type = data_type.to_ascii_lowercase();
    let lower_column_type = column_type.to_ascii_lowercase();
    let integer = matches!(
        lower_data_type.as_str(),
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year"
    );
    let unsigned = integer
        && lower_column_type
            .split_ascii_whitespace()
            .any(|part| part == "unsigned");
    let scalar_type = if unsigned || matches!(lower_data_type.as_str(), "decimal" | "numeric") {
        CursorScalarType::Decimal
    } else if integer {
        CursorScalarType::Int64
    } else if matches!(
        lower_data_type.as_str(),
        "char"
            | "varchar"
            | "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
            | "date"
            | "datetime"
            | "timestamp"
            | "time"
    ) {
        CursorScalarType::String
    } else {
        return Err(cursor_tuple_unavailable(
            spec,
            format!("cursor component {name:?} has unsupported type {column_type:?}"),
        ));
    };
    let comparison = if matches!(
        lower_data_type.as_str(),
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext"
    ) {
        CursorComparison::MysqlText {
            character_set: validated_catalog_ident("character set", character_set.as_deref())?
                .to_string(),
            collation: validated_catalog_ident("collation", collation.as_deref())?.to_string(),
        }
    } else {
        CursorComparison::Default
    };
    let bind_expression = mysql_cursor_bind_expression(
        &data_type,
        &column_type,
        character_set.as_deref(),
        collation.as_deref(),
    )?;
    Ok((
        CursorColumnContract {
            name: name.to_string(),
            scalar_type,
            database_type: crate::schema::query::mysql_canonical_type(&column_type),
            comparison,
        },
        bind_expression,
    ))
}

fn has_exact_ordered_candidate_key(
    rows: &[Row],
    cursor_columns: &[String],
) -> Result<bool, ApplyError> {
    #[derive(Default)]
    struct Candidate {
        index_type: Option<String>,
        parts: Vec<CandidatePart>,
    }
    struct CandidatePart {
        sequence: i64,
        column: Option<String>,
        prefix: Option<i64>,
        expression: Option<String>,
    }
    let mut candidates = BTreeMap::<String, Candidate>::new();
    for row in rows {
        let index_name: String = row.try_get("index_name")?;
        let non_unique: i64 = row.try_get("non_unique")?;
        if non_unique != 0 {
            continue;
        }
        let index_type: String = row.try_get("index_type")?;
        let candidate = candidates.entry(index_name).or_default();
        if candidate
            .index_type
            .as_ref()
            .is_some_and(|recorded| !recorded.eq_ignore_ascii_case(&index_type))
        {
            return Ok(false);
        }
        candidate.index_type = Some(index_type);
        candidate.parts.push(CandidatePart {
            sequence: row.try_get("seq_in_index")?,
            column: row.try_get("column_name")?,
            prefix: row.try_get("sub_part")?,
            expression: row.try_get("expression")?,
        });
    }
    Ok(candidates.into_values().any(|mut candidate| {
        if !candidate
            .index_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("BTREE"))
            || candidate.parts.len() != cursor_columns.len()
        {
            return false;
        }
        candidate.parts.sort_by_key(|part| part.sequence);
        candidate
            .parts
            .iter()
            .zip(cursor_columns)
            .enumerate()
            .all(|(index, (part, expected))| {
                part.sequence == i64::try_from(index + 1).unwrap_or(i64::MAX)
                    && part.prefix.is_none()
                    && part.expression.is_none()
                    && part
                        .column
                        .as_deref()
                        .is_some_and(|column| column.eq_ignore_ascii_case(expected))
            })
    }))
}

fn mysql_cursor_bind_expression(
    data_type: &str,
    column_type: &str,
    character_set: Option<&str>,
    collation: Option<&str>,
) -> Result<String, ApplyError> {
    let data_type = data_type.to_ascii_lowercase();
    let column_type = column_type.to_ascii_lowercase();
    let expression = match data_type.as_str() {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year" => {
            if column_type
                .split_ascii_whitespace()
                .any(|part| part == "unsigned")
            {
                "CAST(? AS UNSIGNED)".to_string()
            } else {
                "CAST(? AS SIGNED)".to_string()
            }
        }
        "decimal" | "numeric" => {
            let base = column_type
                .strip_suffix(" unsigned")
                .unwrap_or(&column_type);
            let Some((_, dimensions)) = base.split_once('(') else {
                return Err(ApplyError::Backend(format!(
                    "mysql backfill: unsupported decimal cursor type {column_type:?}"
                )));
            };
            let Some(dimensions) = dimensions.strip_suffix(')') else {
                return Err(ApplyError::Backend(format!(
                    "mysql backfill: unsupported decimal cursor type {column_type:?}"
                )));
            };
            let valid_dimensions = {
                let mut parts = dimensions.split(',');
                let precision = parts.next().is_some_and(|value| {
                    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
                });
                let scale = parts.next().is_some_and(|value| {
                    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
                });
                precision && scale && parts.next().is_none()
            };
            if !valid_dimensions {
                return Err(ApplyError::Backend(format!(
                    "mysql backfill: unsupported decimal cursor type {column_type:?}"
                )));
            }
            format!("CAST(? AS DECIMAL({dimensions}))")
        }
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" => {
            let character_set = validated_catalog_ident("character set", character_set)?;
            let collation = validated_catalog_ident("collation", collation)?;
            format!("CONVERT(? USING {character_set}) COLLATE {collation}")
        }
        "date" => "CAST(? AS DATE)".to_string(),
        "datetime" | "timestamp" => {
            let suffix = temporal_precision_suffix(&column_type, &data_type)?;
            format!("CAST(? AS DATETIME{suffix})")
        }
        "time" => {
            let suffix = temporal_precision_suffix(&column_type, "time")?;
            format!("CAST(? AS TIME{suffix})")
        }
        _ => {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: unsupported paging type {data_type:?}"
            )));
        }
    };
    Ok(expression)
}

fn validated_catalog_ident<'a>(kind: &str, value: Option<&'a str>) -> Result<&'a str, ApplyError> {
    let Some(value) = value else {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: cursor {kind} metadata is missing"
        )));
    };
    let valid = value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: cursor {kind} metadata {value:?} is invalid"
        )));
    }
    Ok(value)
}

fn temporal_precision_suffix(column_type: &str, base: &str) -> Result<String, ApplyError> {
    if column_type == base {
        return Ok(String::new());
    }
    let Some(precision) = column_type
        .strip_prefix(&format!("{base}("))
        .and_then(|value| value.strip_suffix(')'))
    else {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: unsupported temporal cursor type {column_type:?}"
        )));
    };
    if precision.len() != 1 || !precision.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: unsupported temporal cursor type {column_type:?}"
        )));
    }
    Ok(format!("({precision})"))
}

fn build_window_sql(
    qualified_table: &str,
    runtime: &CursorRuntime,
    filter: Option<&str>,
    have_cursor: bool,
) -> String {
    let cursor_predicate = if have_cursor {
        lexicographic_predicate(runtime, LexicographicBound::StrictAfter)
    } else {
        "1 = 1".to_string()
    };
    let end_predicate = lexicographic_predicate(runtime, LexicographicBound::AtOrBefore);
    let filter = filter
        .map(|filter| format!(" AND ({filter})"))
        .unwrap_or_default();
    let projection = cursor_projection(runtime, "cursor_value");
    let order = cursor_order(runtime, "ASC");
    format!(
        "SELECT {projection}
           FROM {qualified_table}
          WHERE {cursor_predicate}
            AND ({end_predicate}){filter}
          ORDER BY {order}
          LIMIT ? FOR UPDATE"
    )
}

fn build_end_cursor_sql(
    qualified_table: &str,
    runtime: &CursorRuntime,
    filter: Option<&str>,
) -> String {
    let filter = filter
        .map(|filter| format!(" AND ({filter})"))
        .unwrap_or_default();
    let projection = cursor_projection(runtime, "end_cursor");
    let order = cursor_order(runtime, "DESC");
    format!(
        "SELECT {projection}
           FROM {qualified_table}
          WHERE 1 = 1{filter}
          ORDER BY {order}
          LIMIT 1"
    )
}

#[derive(Debug, Clone, Copy)]
enum LexicographicBound {
    StrictAfter,
    AtOrBefore,
}

fn lexicographic_predicate(runtime: &CursorRuntime, bound: LexicographicBound) -> String {
    let last = runtime.columns.len().saturating_sub(1);
    runtime
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let mut terms = runtime.columns[..index]
                .iter()
                .map(|prefix| format!("{} <=> {}", prefix.quoted, prefix.bind_expression))
                .collect::<Vec<_>>();
            let operator = match bound {
                LexicographicBound::StrictAfter => ">",
                LexicographicBound::AtOrBefore if index == last => "<=",
                LexicographicBound::AtOrBefore => "<",
            };
            terms.push(format!(
                "{} {operator} {}",
                column.quoted, column.bind_expression
            ));
            format!("({})", terms.join(" AND "))
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn cursor_projection(runtime: &CursorRuntime, prefix: &str) -> String {
    runtime
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            format!(
                "CAST({} AS CHAR CHARACTER SET utf8mb4) AS {prefix}_{index}",
                column.quoted
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn cursor_order(runtime: &CursorRuntime, direction: &str) -> String {
    runtime
        .columns
        .iter()
        .map(|column| format!("{} {direction}", column.quoted))
        .collect::<Vec<_>>()
        .join(", ")
}

fn tuple_lexicographic_binds(
    tuple: &CursorTuple,
    runtime: &CursorRuntime,
) -> Result<Vec<Bind>, ApplyError> {
    let mut binds = Vec::with_capacity(runtime.columns.len() * (runtime.columns.len() + 1) / 2);
    for index in 0..runtime.columns.len() {
        for prefix in 0..=index {
            binds.push(cursor_scalar_bind(
                &tuple.values()[prefix],
                runtime.columns[prefix].scalar_type,
            )?);
        }
    }
    Ok(binds)
}

fn window_binds(
    last_cursor: Option<&CursorTuple>,
    end_cursor: &CursorTuple,
    runtime: &CursorRuntime,
    batch_size: u32,
) -> Result<Vec<Bind>, ApplyError> {
    let mut binds = Vec::new();
    if let Some(cursor) = last_cursor {
        binds.extend(tuple_lexicographic_binds(cursor, runtime)?);
    }
    binds.extend(tuple_lexicographic_binds(end_cursor, runtime)?);
    binds.push(Bind::Int(i64::from(batch_size)));
    Ok(binds)
}

fn build_per_row_update_sql(
    qualified_table: &str,
    runtime: &CursorRuntime,
    spec: &BackfillSpec,
) -> Result<String, ApplyError> {
    let mut assignments = Vec::with_capacity(spec.per_row.len() + 1);
    if !spec.set_clause.trim().is_empty() {
        assignments.push(spec.set_clause.clone());
    }
    for column in spec.per_row.keys() {
        assignments.push(format!(
            "{} = ?",
            quote_bare("per-row destination column", column)?
        ));
    }
    let tuple_match = tuple_equality_predicate(runtime);
    Ok(format!(
        "UPDATE {qualified_table} SET {} WHERE {tuple_match}",
        assignments.join(", "),
    ))
}

fn tuple_equality_predicate(runtime: &CursorRuntime) -> String {
    runtime
        .columns
        .iter()
        .map(|column| format!("{} <=> {}", column.quoted, column.bind_expression))
        .collect::<Vec<_>>()
        .join(" AND ")
}

fn cursor_scalar_bind(scalar: &IrScalar, expected: CursorScalarType) -> Result<Bind, ApplyError> {
    match (expected, scalar) {
        (CursorScalarType::Int64, IrScalar::Int64(value) | IrScalar::Int(value)) => {
            Ok(Bind::Text(value.to_string()))
        }
        (CursorScalarType::Decimal, IrScalar::Decimal(value))
        | (CursorScalarType::String, IrScalar::Str(value)) => Ok(Bind::Text(value.clone())),
        _ => Err(ApplyError::Backend(format!(
            "mysql backfill: cursor scalar {scalar:?} does not match contract type {expected:?}"
        ))),
    }
}

fn tuple_equality_binds(
    tuple: &CursorTuple,
    runtime: &CursorRuntime,
) -> Result<Vec<Bind>, ApplyError> {
    tuple
        .values()
        .iter()
        .zip(&runtime.columns)
        .map(|(value, column)| cursor_scalar_bind(value, column.scalar_type))
        .collect()
}

fn decode_cursor_row(
    row: &Row,
    prefix: &str,
    runtime: &CursorRuntime,
) -> Result<CursorTuple, ApplyError> {
    let values = runtime
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let raw: String = row.try_get(format!("{prefix}_{index}").as_str())?;
            let value = match column.scalar_type {
                CursorScalarType::Int64 => {
                    let parsed = raw.parse::<i64>().map_err(|error| {
                        ApplyError::Backend(format!(
                            "mysql backfill: invalid signed cursor value {raw:?} for {:?}: {error}",
                            column.name
                        ))
                    })?;
                    IrScalar::Int64(parsed)
                }
                CursorScalarType::Decimal => IrScalar::Decimal(raw),
                CursorScalarType::String => IrScalar::Str(raw),
            };
            Ok::<IrScalar, ApplyError>(value)
        })
        .collect::<Result<Vec<_>, _>>()?;
    CursorTuple::new(values, &runtime.contract)
        .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))
}

/// Open the target table and validate every catalog fact the backfill relies on
/// while the transaction retains that table's metadata lock. Each batch calls
/// this independently because MySQL releases metadata locks at batch COMMIT.
async fn validate_target_under_lock<D: SqlSession>(
    conn: &D,
    qualified_table: &str,
    spec: &BackfillSpec,
    contract: &CursorContract,
    guard: Option<&GuardDescriptor>,
    guard_expectation: ManagedGuardExpectation,
) -> Result<CursorRuntime, ApplyError> {
    conn.query(
        &format!("SELECT 1 AS zero_migrate_metadata_lock FROM {qualified_table} LIMIT 0"),
        &[],
    )
    .await?;
    let runtime = validate_cursor(conn, spec).await?;
    if &runtime.contract != contract {
        return Err(cursor_tuple_unavailable(
            spec,
            "live cursor contract differs from the operation contract",
        ));
    }
    let managed_guard_present = inspect_guard_interactions(
        conn,
        spec,
        guard,
        guard_expectation == ManagedGuardExpectation::FreshAbsent,
    )
    .await?;
    if managed_guard_present && guard_expectation == ManagedGuardExpectation::FreshAbsent {
        let guard = guard.expect("only a managed guard can be reported present");
        return Err(cursor_tuple_unavailable(
            spec,
            format!(
                "pre-existing deterministic guard {:?} has no durable planned progress obligation; refusing to adopt or remove it",
                guard.name
            ),
        ));
    }
    Ok(runtime)
}

fn guard_descriptor(
    version: &MigrationId,
    spec: &BackfillSpec,
) -> Result<GuardDescriptor, ApplyError> {
    let digest = Sha256::digest(version.as_str().as_bytes());
    let name = format!("zm_bf_guard_{}", hex::encode(&digest[..12]));
    let schema = journal_sql::quote_ident_mysql(&spec.schema)?;
    let table = quote_bare("table", &spec.table)?;
    let trigger = quote_bare("managed guard trigger", &name)?;
    let contract = spec.cursor_contract.as_ref().ok_or_else(|| {
        ApplyError::Backend(
            BackfillError::InvalidSpec("missing live cursor contract".to_string()).to_string(),
        )
    })?;
    let changed = spec
        .cursor_columns
        .iter()
        .zip(&contract.columns)
        .map(|(column, column_contract)| {
            let column = quote_bare("cursor component", column)?;
            let comparison = match &column_contract.comparison {
                // Paging intentionally follows the declared collation, but the
                // guard must detect representation changes that a case- or
                // accent-insensitive collation considers equal.
                CursorComparison::MysqlText { .. } => {
                    format!("CAST(OLD.{column} AS BINARY) <=> CAST(NEW.{column} AS BINARY)")
                }
                _ => format!("OLD.{column} <=> NEW.{column}"),
            };
            Ok::<String, ApplyError>(format!("NOT ({comparison})"))
        })
        .collect::<Result<Vec<_>, _>>()?
        .join(" OR ");
    let action_statement = format!(
        "BEGIN IF {changed} THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = \
         'zero-migrate: cursor update blocked during resumable backfill'; END IF; END"
    );
    let create_sql = format!(
        "CREATE TRIGGER {schema}.{trigger} BEFORE UPDATE ON {schema}.{table} FOR EACH ROW {action_statement}"
    );
    let definition_hash = sha256_hex(&[
        spec.schema.as_bytes(),
        spec.table.as_bytes(),
        name.as_bytes(),
        action_statement.as_bytes(),
    ]);
    Ok(GuardDescriptor {
        name,
        action_statement,
        create_sql,
        definition_hash,
    })
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hex::encode(hash.finalize())
}

async fn inspect_guard_interactions<D: SqlSession>(
    conn: &D,
    spec: &BackfillSpec,
    guard: Option<&GuardDescriptor>,
    allow_missing_guard: bool,
) -> Result<bool, ApplyError> {
    let rows = conn
        .query(
            "SELECT TRIGGER_NAME AS trigger_name, ACTION_TIMING AS action_timing,
                    EVENT_MANIPULATION AS event_manipulation,
                    ACTION_ORIENTATION AS action_orientation,
                    ACTION_STATEMENT AS action_statement
               FROM information_schema.TRIGGERS
              WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ?
              ORDER BY TRIGGER_NAME",
            &[spec.schema.as_str().into(), spec.table.as_str().into()],
        )
        .await?;
    let Some(guard) = guard else {
        if let Some(row) = rows.first() {
            let name: String = row.try_get("trigger_name")?;
            return Err(cursor_tuple_unavailable(
                spec,
                format!(
                    "existing trigger {name:?} has interactions zero-migrate cannot prove safe"
                ),
            ));
        }
        return Ok(false);
    };
    let mut managed = None;
    for row in &rows {
        let name: String = row.try_get("trigger_name")?;
        if !name.eq_ignore_ascii_case(&guard.name) {
            return Err(cursor_tuple_unavailable(
                spec,
                format!(
                    "existing trigger {name:?} conflicts with managed guard {:?}; trigger ordering/side effects cannot be proven safe",
                    guard.name
                ),
            ));
        }
        if managed.replace(row).is_some() {
            return Err(cursor_tuple_unavailable(
                spec,
                format!(
                    "catalog returned duplicate managed trigger {:?}",
                    guard.name
                ),
            ));
        }
    }
    let Some(row) = managed else {
        if allow_missing_guard {
            return Ok(false);
        }
        return Err(cursor_tuple_unavailable(
            spec,
            format!(
                "managed cursor guard {:?} is missing; cursor immutability may have been violated",
                guard.name
            ),
        ));
    };
    let timing: String = row.try_get("action_timing")?;
    let event: String = row.try_get("event_manipulation")?;
    let orientation: String = row.try_get("action_orientation")?;
    let statement: String = row.try_get("action_statement")?;
    if !timing.eq_ignore_ascii_case("BEFORE")
        || !event.eq_ignore_ascii_case("UPDATE")
        || !orientation.eq_ignore_ascii_case("ROW")
        || normalize_trigger_statement(&statement)
            != normalize_trigger_statement(&guard.action_statement)
    {
        return Err(cursor_tuple_unavailable(
            spec,
            format!(
                "existing managed trigger {:?} does not match its recorded BEFORE UPDATE guard definition",
                guard.name
            ),
        ));
    }
    Ok(true)
}

fn normalize_trigger_statement(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[allow(clippy::too_many_arguments)]
async fn ensure_guard_installed<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    checksum: &Checksum,
    spec: &BackfillSpec,
    guard: &GuardDescriptor,
    state: &str,
    cohort_initialized: bool,
) -> Result<(), ApplyError> {
    if !matches!(state, GUARD_PLANNED | GUARD_INSTALLED) {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: cannot reconcile guard installation from state {state:?}"
        )));
    }
    let present = inspect_guard_interactions(conn, spec, Some(guard), true).await?;
    if state == GUARD_INSTALLED && !present {
        return Err(cursor_tuple_unavailable(
            spec,
            format!(
                "recorded managed guard {:?} disappeared{}; refusing resume because cursor updates may have occurred",
                guard.name,
                if cohort_initialized {
                    " after cohort capture"
                } else {
                    ""
                }
            ),
        ));
    }
    if !present {
        conn.batch(&guard.create_sql).await?;
    }
    inspect_guard_interactions(conn, spec, Some(guard), false).await?;
    if state == GUARD_PLANNED {
        let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
        let updated = conn
            .exec(
                &format!(
                    "UPDATE {meta}.schema_backfills SET guard_state = ?
                      WHERE backfill_id = ? AND checksum = ? AND guard_state = ?
                        AND guard_name = ? AND guard_definition_hash = ?"
                ),
                &[
                    GUARD_INSTALLED.into(),
                    backfill_id.into(),
                    checksum.as_str().into(),
                    GUARD_PLANNED.into(),
                    guard.name.as_str().into(),
                    guard.definition_hash.as_str().into(),
                ],
            )
            .await?;
        if updated != 1 {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: installed guard obligation checkpoint affected {updated} rows"
            )));
        }
    }
    Ok(())
}

async fn cleanup_guard<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    checksum: &Checksum,
    spec: &BackfillSpec,
    guard: &GuardDescriptor,
) -> Result<(), ApplyError> {
    let present = inspect_guard_interactions(conn, spec, Some(guard), true).await?;
    if present {
        let schema = journal_sql::quote_ident_mysql(&spec.schema)?;
        let trigger = quote_bare("managed guard trigger", &guard.name)?;
        conn.batch(&format!("DROP TRIGGER {schema}.{trigger}"))
            .await?;
    }
    if inspect_guard_interactions(conn, spec, Some(guard), true).await? {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: managed guard {:?} still exists after cleanup",
            guard.name
        )));
    }
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    let updated = conn
        .exec(
            &format!(
                "UPDATE {meta}.schema_backfills SET guard_state = ?
                  WHERE backfill_id = ? AND checksum = ? AND complete = TRUE
                    AND guard_state = ? AND guard_name = ? AND guard_definition_hash = ?"
            ),
            &[
                GUARD_CLEANED.into(),
                backfill_id.into(),
                checksum.as_str().into(),
                GUARD_CLEANUP_PENDING.into(),
                guard.name.as_str().into(),
                guard.definition_hash.as_str().into(),
            ],
        )
        .await?;
    if updated != 1 {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: guard cleanup checkpoint affected {updated} rows"
        )));
    }
    Ok(())
}

fn cohort_fingerprint(
    backfill_id: &str,
    checksum: &Checksum,
    cursor_columns: &[String],
    contract: &CursorContract,
    end_cursor_json: Option<&str>,
) -> Result<String, ApplyError> {
    let columns = serde_json::to_string(cursor_columns)
        .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))?;
    let contract = serde_json::to_string(contract)
        .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))?;
    Ok(sha256_hex(&[
        backfill_id.as_bytes(),
        checksum.as_str().as_bytes(),
        columns.as_bytes(),
        contract.as_bytes(),
        end_cursor_json.unwrap_or("<empty-cohort>").as_bytes(),
    ]))
}

fn checkpoint_fingerprint(cohort_fingerprint: &str, last_cursor_json: Option<&str>) -> String {
    sha256_hex(&[
        cohort_fingerprint.as_bytes(),
        last_cursor_json
            .unwrap_or("<before-first-batch>")
            .as_bytes(),
    ])
}

fn validate_resume_progress(
    progress: &Progress,
    backfill_id: &str,
    checksum: &Checksum,
    spec: &BackfillSpec,
    contract: &CursorContract,
    guard: Option<&GuardDescriptor>,
) -> Result<ValidatedProgress, ApplyError> {
    let Some(recorded_checksum) = progress.checksum.as_deref() else {
        return Err(ApplyError::ChecksumDrift {
            version: backfill_id.to_string(),
            recorded: "<missing checksum>".to_string(),
            expected: checksum.as_str().to_string(),
        });
    };
    if recorded_checksum != checksum.as_str() {
        return Err(ApplyError::ChecksumDrift {
            version: backfill_id.to_string(),
            recorded: recorded_checksum.to_string(),
            expected: checksum.as_str().to_string(),
        });
    }
    if progress.target_schema != spec.schema || progress.target_table != spec.table {
        return Err(cursor_tuple_unavailable(
            spec,
            format!(
                "stored target drifted from {}.{} to {}.{}",
                spec.schema, spec.table, progress.target_schema, progress.target_table
            ),
        ));
    }
    let columns: Vec<String> =
        serde_json::from_str(&progress.cursor_columns_json).map_err(|error| {
            ApplyError::Backend(format!(
                "mysql backfill: invalid stored cursorColumns JSON: {error}"
            ))
        })?;
    if columns != spec.cursor_columns {
        return Err(cursor_tuple_unavailable(
            spec,
            format!(
                "stored cursorColumns drifted from {:?} to {columns:?}",
                spec.cursor_columns
            ),
        ));
    }
    let stored_contract: CursorContract = serde_json::from_str(&progress.cursor_contract_json)
        .map_err(|error| {
            ApplyError::Backend(format!(
                "mysql backfill: invalid stored cursor contract: {error}"
            ))
        })?;
    if &stored_contract != contract {
        return Err(cursor_tuple_unavailable(
            spec,
            format!("stored cursor scalar/type/collation contract drifted: {stored_contract:?}"),
        ));
    }
    match (&spec.cursor_stability, guard) {
        (CursorStability::GuardUpdates, Some(guard)) => {
            if progress.cursor_stability_mode != "guardUpdates"
                || progress.external_invariant_name.is_some()
                || progress.guard_name.as_deref() != Some(guard.name.as_str())
                || progress.guard_definition_hash.as_deref() != Some(guard.definition_hash.as_str())
                || !matches!(
                    progress.guard_state.as_str(),
                    GUARD_PLANNED | GUARD_INSTALLED | GUARD_CLEANUP_PENDING | GUARD_CLEANED
                )
            {
                return Err(cursor_tuple_unavailable(
                    spec,
                    "stored guardUpdates obligation metadata drifted",
                ));
            }
            let valid_guard_state = if progress.complete {
                matches!(
                    progress.guard_state.as_str(),
                    GUARD_CLEANUP_PENDING | GUARD_CLEANED
                )
            } else if progress.cohort_initialized {
                progress.guard_state == GUARD_INSTALLED
            } else {
                matches!(
                    progress.guard_state.as_str(),
                    GUARD_PLANNED | GUARD_INSTALLED
                )
            };
            if !valid_guard_state {
                return Err(cursor_tuple_unavailable(
                    spec,
                    "stored guard obligation state is inconsistent with cohort/completion state",
                ));
            }
        }
        (CursorStability::ExternalInvariant { name }, None) => {
            if progress.cursor_stability_mode != "externalInvariant"
                || progress.external_invariant_name.as_deref() != Some(name.as_str())
                || progress.guard_name.is_some()
                || progress.guard_definition_hash.is_some()
                || progress.guard_state != EXTERNAL_INVARIANT
            {
                return Err(cursor_tuple_unavailable(
                    spec,
                    "stored externalInvariant name/mode drifted",
                ));
            }
        }
        _ => {
            return Err(cursor_tuple_unavailable(
                spec,
                "cursor stability mode and managed-guard plan disagree",
            ));
        }
    }
    let last_cursor = progress
        .last_cursor_json
        .as_deref()
        .map(|value| CursorTuple::from_json(value, contract))
        .transpose()
        .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))?;
    let end_cursor = progress
        .end_cursor_json
        .as_deref()
        .map(|value| CursorTuple::from_json(value, contract))
        .transpose()
        .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))?;
    if !progress.cohort_initialized {
        if last_cursor.is_some()
            || end_cursor.is_some()
            || progress.cohort_fingerprint.is_some()
            || progress.checkpoint_fingerprint.is_some()
        {
            return Err(cursor_tuple_unavailable(
                spec,
                "uninitialized cohort already contains cursor checkpoint data",
            ));
        }
    } else {
        if end_cursor.is_none() && last_cursor.is_some() {
            return Err(cursor_tuple_unavailable(
                spec,
                "empty cohort records a nonempty lastCursor",
            ));
        }
        let expected_fingerprint = cohort_fingerprint(
            backfill_id,
            checksum,
            &spec.cursor_columns,
            contract,
            progress.end_cursor_json.as_deref(),
        )?;
        if progress.cohort_fingerprint.as_deref() != Some(expected_fingerprint.as_str()) {
            return Err(cursor_tuple_unavailable(
                spec,
                "stored endCursor/cohort bound fingerprint drifted",
            ));
        }
        let expected_checkpoint =
            checkpoint_fingerprint(&expected_fingerprint, progress.last_cursor_json.as_deref());
        if progress.checkpoint_fingerprint.as_deref() != Some(expected_checkpoint.as_str()) {
            return Err(cursor_tuple_unavailable(
                spec,
                "stored lastCursor/checkpoint fingerprint drifted",
            ));
        }
    }
    if progress.complete && !progress.cohort_initialized {
        return Err(cursor_tuple_unavailable(
            spec,
            "completion is recorded before cohort initialization",
        ));
    }
    Ok(ValidatedProgress {
        last_cursor,
        end_cursor,
        cohort_initialized: progress.cohort_initialized,
        complete: progress.complete,
        guard_state: progress.guard_state.clone(),
    })
}

async fn rollback<D: SqlSession>(conn: &D, version: &str, reason: &'static str) {
    if let Err(error) = conn.batch("ROLLBACK").await {
        tracing::warn!(
            error = %error,
            version = %version,
            reason,
            "zero-migrate: MySQL backfill ROLLBACK failed"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_one_batch<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    checksum: &Checksum,
    qualified_table: &str,
    spec: &BackfillSpec,
    contract: &CursorContract,
    guard: Option<&GuardDescriptor>,
    last_cursor: Option<&CursorTuple>,
    end_cursor: &CursorTuple,
) -> Result<Vec<CursorTuple>, ApplyError> {
    conn.batch("START TRANSACTION").await?;
    let result = async {
        let runtime = validate_target_under_lock(
            conn,
            qualified_table,
            spec,
            contract,
            guard,
            ManagedGuardExpectation::Installed,
        )
        .await?;
        let window_binds = window_binds(last_cursor, end_cursor, &runtime, spec.batch_size)?;
        let window_sql = build_window_sql(
            qualified_table,
            &runtime,
            spec.filter.as_deref(),
            last_cursor.is_some(),
        );
        run_one_batch_inner(
            conn,
            cfg,
            backfill_id,
            checksum,
            qualified_table,
            &runtime,
            spec,
            contract,
            last_cursor,
            end_cursor,
            &window_sql,
            &window_binds,
        )
        .await
    }
    .await;
    match result {
        Ok(selected) => {
            if let Err(error) = conn.batch("COMMIT").await {
                rollback(conn, &spec.name, "ambiguous batch COMMIT failure").await;
                return Err(ApplyError::Db(error.into()));
            }
            Ok(selected)
        }
        Err(error) => {
            rollback(conn, &spec.name, "batch error").await;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_one_batch_inner<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    checksum: &Checksum,
    qualified_table: &str,
    runtime: &CursorRuntime,
    spec: &BackfillSpec,
    contract: &CursorContract,
    previous_cursor: Option<&CursorTuple>,
    end_cursor: &CursorTuple,
    window_sql: &str,
    window_binds: &[Bind],
) -> Result<Vec<CursorTuple>, ApplyError> {
    let rows: Vec<Row> = conn.query(window_sql, window_binds).await?;
    let selected = rows
        .iter()
        .map(|row| decode_cursor_row(row, "cursor_value", runtime))
        .collect::<Result<Vec<_>, _>>()?;
    if selected.is_empty() {
        return Ok(selected);
    }

    if spec.per_row.is_empty() {
        let tuple_match = tuple_equality_predicate(runtime);
        let predicate = vec![format!("({tuple_match})"); selected.len()].join(" OR ");
        let update_sql = format!(
            "UPDATE {qualified_table} SET {} WHERE {predicate}",
            spec.set_clause
        );
        let selected_binds = selected
            .iter()
            .map(|tuple| tuple_equality_binds(tuple, runtime))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        conn.exec(&update_sql, &selected_binds)
            .await
            .map_err(|error| ApplyError::MigrationFailed {
                version: spec.name.clone(),
                source: error.into(),
            })?;
    } else {
        let update_sql = build_per_row_update_sql(qualified_table, runtime, spec)?;
        for selected_cursor in &selected {
            let mut row_binds = spec
                .per_row
                .values()
                .map(|assignment| generate_per_row_value(assignment.generator()))
                .map(Bind::Text)
                .collect::<Vec<_>>();
            row_binds.extend(tuple_equality_binds(selected_cursor, runtime)?);
            let affected = conn.exec(&update_sql, &row_binds).await.map_err(|error| {
                ApplyError::MigrationFailed {
                    version: spec.name.clone(),
                    source: error.into(),
                }
            })?;
            if affected != 1 {
                return Err(ApplyError::Backend(format!(
                    "mysql backfill: per-row update at cursor {:?} affected {affected} rows; expected exactly one",
                    selected_cursor.values()
                )));
            }
        }
    }

    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    let last = selected
        .last()
        .expect("a non-empty selected window has a last cursor");
    let last_json = last
        .to_json()
        .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))?;
    let previous_json = previous_cursor
        .map(CursorTuple::to_json)
        .transpose()
        .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))?;
    let end_json = end_cursor
        .to_json()
        .map_err(|error| ApplyError::Backend(format!("mysql backfill: {error}")))?;
    let fingerprint = cohort_fingerprint(
        backfill_id,
        checksum,
        &spec.cursor_columns,
        contract,
        Some(&end_json),
    )?;
    let previous_checkpoint = checkpoint_fingerprint(&fingerprint, previous_json.as_deref());
    let next_checkpoint = checkpoint_fingerprint(&fingerprint, Some(&last_json));
    let advanced = conn
        .exec(
            &format!(
                "UPDATE {meta}.schema_backfills
                    SET last_cursor = ?, checkpoint_fingerprint = ?,
                        rows_done = rows_done + ?,
                        batches_done = batches_done + 1
                  WHERE backfill_id = ? AND checksum = ? AND complete = FALSE
                    AND cohort_initialized = TRUE AND last_cursor <=> ?
                    AND end_cursor <=> ? AND cohort_fingerprint = ?
                    AND checkpoint_fingerprint = ?"
            ),
            &[
                last_json.into(),
                next_checkpoint.into(),
                i64::try_from(selected.len()).unwrap_or(i64::MAX).into(),
                backfill_id.into(),
                checksum.as_str().into(),
                previous_json.into(),
                end_json.into(),
                fingerprint.into(),
                previous_checkpoint.into(),
            ],
        )
        .await?;
    if advanced != 1 {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: progress update affected {advanced} rows for {backfill_id:?}"
        )));
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{DbError, Value};
    use std::cell::RefCell;

    fn checksum(label: &str) -> Checksum {
        Checksum::of(&crate::model::migration::ChecksumInput {
            up: label,
            down: None,
            flags: &crate::model::migration::MigrationFlags::default(),
            owner_app: "app",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        })
    }

    fn composite_contract() -> CursorContract {
        CursorContract {
            columns: vec![
                CursorColumnContract {
                    name: "tenant_id".into(),
                    scalar_type: CursorScalarType::Int64,
                    database_type: "bigint".into(),
                    comparison: CursorComparison::Default,
                },
                CursorColumnContract {
                    name: "slug".into(),
                    scalar_type: CursorScalarType::String,
                    database_type: "text".into(),
                    comparison: CursorComparison::MysqlText {
                        character_set: "utf8mb4".into(),
                        collation: "utf8mb4_bin".into(),
                    },
                },
            ],
        }
    }

    fn runtime(contract: &CursorContract) -> CursorRuntime {
        CursorRuntime {
            columns: contract
                .columns
                .iter()
                .map(|column| CursorRuntimeColumn {
                    name: column.name.clone(),
                    quoted: format!("`{}`", column.name),
                    scalar_type: column.scalar_type,
                    bind_expression: match &column.comparison {
                        CursorComparison::MysqlText {
                            character_set,
                            collation,
                        } => {
                            format!("CONVERT(? USING {character_set}) COLLATE {collation}")
                        }
                        _ if column.scalar_type == CursorScalarType::Int64 => {
                            "CAST(? AS SIGNED)".into()
                        }
                        _ => "CAST(? AS CHAR)".into(),
                    },
                })
                .collect(),
            contract: contract.clone(),
        }
    }

    fn spec(stability: CursorStability) -> BackfillSpec {
        BackfillSpec {
            schema: "app".into(),
            table: "events".into(),
            cursor_columns: vec!["tenant_id".into(), "slug".into()],
            cursor_stability: stability,
            cursor_contract: Some(composite_contract()),
            batch_size: 50,
            set_clause: "`done` = TRUE".into(),
            per_row: BTreeMap::new(),
            filter: Some("`done` = FALSE".into()),
            name: "finish events".into(),
        }
    }

    fn tuple(tenant: i64, slug: &str) -> CursorTuple {
        CursorTuple::new(
            vec![IrScalar::Int64(tenant), IrScalar::Str(slug.into())],
            &composite_contract(),
        )
        .expect("tuple")
    }

    fn progress_table_catalog(engine: Option<&str>) -> Vec<Row> {
        vec![Row::new(
            vec!["table_engine".into()],
            vec![engine.map_or(Value::Null, |value| Value::Text(value.into()))],
        )]
    }

    fn progress_column_catalog() -> Vec<Row> {
        PROGRESS_COLUMNS
            .iter()
            .map(|column| {
                Row::new(
                    vec![
                        "column_name".into(),
                        "column_type".into(),
                        "is_nullable".into(),
                        "character_set_name".into(),
                        "collation_name".into(),
                        "column_key".into(),
                    ],
                    vec![
                        Value::Text(column.name.into()),
                        Value::Text(column.column_type.into()),
                        Value::Text(column.nullable.into()),
                        column
                            .character_set
                            .map_or(Value::Null, |value| Value::Text(value.into())),
                        column
                            .collation
                            .map_or(Value::Null, |value| Value::Text(value.into())),
                        Value::Text(column.column_key.into()),
                    ],
                )
            })
            .collect()
    }

    #[test]
    fn progress_catalog_rejects_stale_layout_and_non_innodb_engine() {
        let current = progress_column_catalog();
        validate_progress_catalog(&progress_table_catalog(Some("InnoDB")), &current).unwrap();

        let mut stale = current.clone();
        stale.remove(
            PROGRESS_COLUMNS
                .iter()
                .position(|column| column.name == "checkpoint_fingerprint")
                .unwrap(),
        );
        let error =
            validate_progress_catalog(&progress_table_catalog(Some("InnoDB")), &stale).unwrap_err();
        assert!(error.to_string().contains("stale pre-release schema"));
        assert!(error.to_string().contains("checkpoint"));

        let error = validate_progress_catalog(&progress_table_catalog(Some("MyISAM")), &current)
            .unwrap_err();
        assert!(error.to_string().contains("is not InnoDB"));
        assert!(error.to_string().contains("would not be atomic"));
    }

    fn index_row(
        index: &str,
        sequence: i64,
        column: Option<&str>,
        prefix: Option<i64>,
        expression: Option<&str>,
        index_type: &str,
    ) -> Row {
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
                Value::Text(index.into()),
                Value::Int(0),
                Value::Text(index_type.into()),
                Value::Int(sequence),
                column.map_or(Value::Null, |value| Value::Text(value.into())),
                prefix.map_or(Value::Null, Value::Int),
                expression.map_or(Value::Null, |value| Value::Text(value.into())),
            ],
        )
    }

    #[test]
    fn exact_ordered_primary_or_unique_btree_is_required() {
        let exact = vec![
            index_row("PRIMARY", 1, Some("tenant_id"), None, None, "BTREE"),
            index_row("PRIMARY", 2, Some("slug"), None, None, "BTREE"),
        ];
        assert!(
            has_exact_ordered_candidate_key(&exact, &["tenant_id".into(), "slug".into()]).unwrap()
        );
        assert!(
            !has_exact_ordered_candidate_key(&exact, &["slug".into(), "tenant_id".into()]).unwrap()
        );
        assert!(!has_exact_ordered_candidate_key(
            &[index_row(
                "prefix_key",
                1,
                Some("slug"),
                Some(20),
                None,
                "BTREE"
            )],
            &["slug".into()]
        )
        .unwrap());
        assert!(!has_exact_ordered_candidate_key(
            &[index_row("hash_key", 1, Some("slug"), None, None, "HASH")],
            &["slug".into()]
        )
        .unwrap());
    }

    #[test]
    fn composite_lexicographic_boundaries_repeat_prefix_binds() {
        let contract = composite_contract();
        let runtime = runtime(&contract);
        let sql = build_window_sql("`app`.`events`", &runtime, Some("`done` = FALSE"), true);
        assert!(sql.contains(
            "(`tenant_id` > CAST(? AS SIGNED)) OR (`tenant_id` <=> CAST(? AS SIGNED) AND `slug` >"
        ));
        assert!(sql.contains(
            "(`tenant_id` < CAST(? AS SIGNED)) OR (`tenant_id` <=> CAST(? AS SIGNED) AND `slug` <="
        ));
        assert!(sql.contains("ORDER BY `tenant_id` ASC, `slug` ASC"));
        assert!(sql.contains("LIMIT ? FOR UPDATE"));

        let binds = window_binds(Some(&tuple(4, "m")), &tuple(9, "z"), &runtime, 50).unwrap();
        assert_eq!(
            binds,
            vec![
                Bind::Text("4".into()),
                Bind::Text("4".into()),
                Bind::Text("m".into()),
                Bind::Text("9".into()),
                Bind::Text("9".into()),
                Bind::Text("z".into()),
                Bind::Int(50),
            ]
        );
    }

    #[test]
    fn any_cursor_component_assignment_is_rejected_without_expression_false_positives() {
        let columns = vec!["tenant_id".into(), "slug".into()];
        assert!(assert_cursor_not_mutated("`tenant_id` = 9", &columns).is_err());
        assert!(assert_cursor_not_mutated("`x` = 1, `slug` = 'x'", &columns).is_err());
        assert!(assert_cursor_not_mutated("`x` = IF(`ok`, `slug` = 'x', 0)", &columns).is_ok());
        assert!(assert_cursor_not_mutated("`x` = '`slug` = x'", &columns).is_ok());
    }

    #[test]
    fn unsigned_integer_cursor_uses_decimal_tag_and_unsigned_cast() {
        let unsigned = Row::new(
            vec![
                "is_nullable".into(),
                "data_type".into(),
                "column_type".into(),
                "character_set_name".into(),
                "collation_name".into(),
                "extra".into(),
                "generation_expression".into(),
            ],
            vec![
                Value::Text("NO".into()),
                Value::Text("bigint".into()),
                Value::Text("bigint unsigned".into()),
                Value::Null,
                Value::Null,
                Value::Text(String::new()),
                Value::Text(String::new()),
            ],
        );
        let mut unsigned_spec = spec(CursorStability::ExternalInvariant {
            name: "cursor frozen".into(),
        });
        unsigned_spec.cursor_columns = vec!["tenant_id".into()];
        unsigned_spec.cursor_contract = Some(CursorContract {
            columns: vec![CursorColumnContract {
                name: "tenant_id".into(),
                scalar_type: CursorScalarType::Decimal,
                database_type: "bigint unsigned".into(),
                comparison: CursorComparison::Default,
            }],
        });
        let (contract, bind) =
            mysql_live_cursor_column(&unsigned, "tenant_id", &unsigned_spec).unwrap();
        assert_eq!(contract.scalar_type, CursorScalarType::Decimal);
        assert_eq!(bind, "CAST(? AS UNSIGNED)");
        let tuple = CursorTuple::new(
            vec![IrScalar::Decimal("18446744073709551615".into())],
            unsigned_spec.cursor_contract.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(
            tuple.to_json().unwrap(),
            r#"[{"decimal":"18446744073709551615"}]"#
        );
    }

    #[test]
    fn resume_rejects_changed_columns_contract_and_cohort_bound() {
        let spec = spec(CursorStability::ExternalInvariant {
            name: "writers set done".into(),
        });
        let contract = spec.cursor_contract.as_ref().unwrap();
        let checksum = checksum("stable-plan");
        let last_json = tuple(4, "m").to_json().unwrap();
        let end_json = tuple(9, "z").to_json().unwrap();
        let fingerprint = cohort_fingerprint(
            "v1",
            &checksum,
            &spec.cursor_columns,
            contract,
            Some(&end_json),
        )
        .unwrap();
        let checkpoint = checkpoint_fingerprint(&fingerprint, Some(&last_json));
        let progress = Progress {
            target_schema: spec.schema.clone(),
            target_table: spec.table.clone(),
            cursor_columns_json: serde_json::to_string(&spec.cursor_columns).unwrap(),
            cursor_contract_json: serde_json::to_string(contract).unwrap(),
            cursor_stability_mode: "externalInvariant".into(),
            external_invariant_name: Some("writers set done".into()),
            guard_name: None,
            guard_definition_hash: None,
            guard_state: EXTERNAL_INVARIANT.into(),
            last_cursor_json: Some(last_json),
            end_cursor_json: Some(end_json),
            cohort_fingerprint: Some(fingerprint),
            checkpoint_fingerprint: Some(checkpoint),
            cohort_initialized: true,
            complete: false,
            exists: true,
            checksum: Some(checksum.as_str().into()),
        };
        validate_resume_progress(&progress, "v1", &checksum, &spec, contract, None).unwrap();

        let mut changed_bound = progress.clone();
        changed_bound.end_cursor_json = Some(tuple(10, "a").to_json().unwrap());
        let error =
            validate_resume_progress(&changed_bound, "v1", &checksum, &spec, contract, None)
                .unwrap_err();
        assert!(error.to_string().contains("cohort bound fingerprint"));

        let mut changed_checkpoint = progress.clone();
        changed_checkpoint.last_cursor_json = Some(tuple(5, "a").to_json().unwrap());
        let error =
            validate_resume_progress(&changed_checkpoint, "v1", &checksum, &spec, contract, None)
                .unwrap_err();
        assert!(error.to_string().contains("checkpoint fingerprint"));

        let mut changed_columns = progress.clone();
        changed_columns.cursor_columns_json = r#"["slug","tenant_id"]"#.into();
        assert!(
            validate_resume_progress(&changed_columns, "v1", &checksum, &spec, contract, None)
                .is_err()
        );

        let mut changed_contract = progress;
        changed_contract.cursor_contract_json = serde_json::to_string(&CursorContract {
            columns: vec![contract.columns[0].clone()],
        })
        .unwrap();
        assert!(validate_resume_progress(
            &changed_contract,
            "v1",
            &checksum,
            &spec,
            contract,
            None
        )
        .is_err());
    }

    #[derive(Clone)]
    struct TriggerRecord {
        name: String,
        timing: String,
        event: String,
        orientation: String,
        statement: String,
    }

    struct RecordingSession {
        log: RefCell<Vec<String>>,
        binds: RefCell<Vec<Vec<Bind>>>,
        trigger: RefCell<Option<TriggerRecord>>,
        expected_trigger: RefCell<Option<TriggerRecord>>,
        window: RefCell<Vec<Row>>,
        completion_catalog: bool,
    }

    impl RecordingSession {
        fn new() -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                binds: RefCell::new(Vec::new()),
                trigger: RefCell::new(None),
                expected_trigger: RefCell::new(None),
                window: RefCell::new(Vec::new()),
                completion_catalog: false,
            }
        }

        fn enable_completion_catalog(&mut self) {
            self.completion_catalog = true;
        }

        fn managed(&self, descriptor: &GuardDescriptor) {
            let trigger = TriggerRecord {
                name: descriptor.name.clone(),
                timing: "BEFORE".into(),
                event: "UPDATE".into(),
                orientation: "ROW".into(),
                statement: descriptor.action_statement.clone(),
            };
            *self.expected_trigger.borrow_mut() = Some(trigger);
        }

        fn install_managed(&self, descriptor: &GuardDescriptor) {
            self.managed(descriptor);
            let installed = self.expected_trigger.borrow().clone();
            *self.trigger.borrow_mut() = installed;
        }

        fn trigger_rows(&self) -> Vec<Row> {
            self.trigger
                .borrow()
                .as_ref()
                .map_or_else(Vec::new, |trigger| {
                    vec![Row::new(
                        vec![
                            "trigger_name".into(),
                            "action_timing".into(),
                            "event_manipulation".into(),
                            "action_orientation".into(),
                            "action_statement".into(),
                        ],
                        vec![
                            Value::Text(trigger.name.clone()),
                            Value::Text(trigger.timing.clone()),
                            Value::Text(trigger.event.clone()),
                            Value::Text(trigger.orientation.clone()),
                            Value::Text(trigger.statement.clone()),
                        ],
                    )]
                })
        }

        fn cursor_column_rows(&self) -> Vec<Row> {
            [
                ("tenant_id", "bigint", "bigint", None, None),
                ("slug", "text", "text", Some("utf8mb4"), Some("utf8mb4_bin")),
            ]
            .into_iter()
            .map(|(name, data_type, column_type, character_set, collation)| {
                Row::new(
                    vec![
                        "column_name".into(),
                        "is_nullable".into(),
                        "data_type".into(),
                        "column_type".into(),
                        "character_set_name".into(),
                        "collation_name".into(),
                        "extra".into(),
                        "generation_expression".into(),
                    ],
                    vec![
                        Value::Text(name.into()),
                        Value::Text("NO".into()),
                        Value::Text(data_type.into()),
                        Value::Text(column_type.into()),
                        character_set.map_or(Value::Null, |value| Value::Text(value.into())),
                        collation.map_or(Value::Null, |value| Value::Text(value.into())),
                        Value::Text(String::new()),
                        Value::Text(String::new()),
                    ],
                )
            })
            .collect()
        }
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.log.borrow_mut().push(format!("batch: {sql}"));
            if sql.starts_with("CREATE TRIGGER") {
                *self.trigger.borrow_mut() = self.expected_trigger.borrow().clone();
            } else if sql.starts_with("DROP TRIGGER") {
                self.trigger.borrow_mut().take();
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
            if sql.contains("information_schema.TRIGGERS") {
                return Ok(self.trigger_rows());
            }
            if self.completion_catalog && sql.contains("information_schema.TABLES") {
                return Ok(vec![Row::new(
                    vec!["table_engine".into()],
                    vec![Value::Text("InnoDB".into())],
                )]);
            }
            if self.completion_catalog && sql.contains("information_schema.COLUMNS") {
                return Ok(self.cursor_column_rows());
            }
            if self.completion_catalog && sql.contains("information_schema.STATISTICS") {
                return Ok(vec![
                    index_row("PRIMARY", 1, Some("tenant_id"), None, None, "BTREE"),
                    index_row("PRIMARY", 2, Some("slug"), None, None, "BTREE"),
                ]);
            }
            if sql.contains("AS cursor_value_0") {
                return Ok(self.window.borrow().clone());
            }
            Ok(Vec::new())
        }

        async fn query_one(&self, sql: &str, params: &[Bind]) -> Result<Row, DbError> {
            self.query(sql, params)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| DbError::message("no row"))
        }
    }

    #[test]
    fn guard_rejects_case_only_text_change_under_case_insensitive_collation() {
        let mut spec = spec(CursorStability::GuardUpdates);
        spec.cursor_contract.as_mut().unwrap().columns[1].comparison =
            CursorComparison::MysqlText {
                character_set: "utf8mb4".into(),
                collation: "utf8mb4_0900_ai_ci".into(),
            };
        let version = MigrationId::derive("mysql-ci-guard-test", b"events");
        let descriptor = guard_descriptor(&version, &spec).unwrap();

        // The declared collation considers `a` and `A` paging-equal. The guard
        // deliberately compares their binary representations instead, making
        // that case-only stored-value change enter the SIGNAL branch.
        assert_ne!(b"a", b"A");
        assert!(descriptor
            .action_statement
            .contains("NOT (CAST(OLD.`slug` AS BINARY) <=> CAST(NEW.`slug` AS BINARY))"));
        assert!(!descriptor
            .action_statement
            .contains("NOT (OLD.`slug` <=> NEW.`slug`)"));
    }

    #[compio::test]
    async fn durable_completion_refuses_guard_disappearance_after_last_batch() {
        let spec = spec(CursorStability::GuardUpdates);
        let contract = spec.cursor_contract.as_ref().unwrap().clone();
        let version = MigrationId::derive("mysql-completion-guard-test", b"events");
        let checksum = checksum("guarded completion plan");
        let guard = guard_descriptor(&version, &spec).unwrap();
        let mut rec = RecordingSession::new();
        rec.enable_completion_catalog();

        let error = mark_durable_complete(
            &rec,
            &ExecutorConfig::new("prj_x", "app"),
            version.as_str(),
            &checksum,
            "`app`.`events`",
            &spec,
            &contract,
            Some(&guard),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("managed cursor guard"));
        assert!(error.to_string().contains("is missing"));
        let log = rec.log.borrow().clone();
        assert!(
            log.iter()
                .any(|entry| entry.contains("zero_migrate_metadata_lock")),
            "{log:?}"
        );
        assert!(
            log.iter().any(|entry| entry == "batch: ROLLBACK"),
            "{log:?}"
        );
        assert!(
            !log.iter()
                .any(|entry| entry.contains("SET complete = TRUE")),
            "{log:?}"
        );
    }

    #[compio::test]
    async fn fresh_initialization_refuses_preexisting_deterministic_guard() {
        let spec = spec(CursorStability::GuardUpdates);
        let contract = spec.cursor_contract.as_ref().unwrap().clone();
        let version = MigrationId::derive("mysql-fresh-guard-test", b"events");
        let checksum = checksum("fresh guarded plan");
        let guard = guard_descriptor(&version, &spec).unwrap();
        let mut rec = RecordingSession::new();
        rec.enable_completion_catalog();
        rec.install_managed(&guard);

        let error = initialize_obligation(
            &rec,
            &ExecutorConfig::new("prj_x", "app"),
            version.as_str(),
            &checksum,
            &spec,
            &contract,
            Some(&guard),
            "tester",
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("no durable planned progress"));
        assert!(error.to_string().contains("refusing to adopt or remove"));
        assert!(
            rec.trigger.borrow().is_some(),
            "the trigger must remain untouched"
        );
        let log = rec.log.borrow().clone();
        assert!(
            log.iter().any(|entry| entry == "batch: ROLLBACK"),
            "{log:?}"
        );
        assert!(
            !log.iter().any(|entry| {
                entry.starts_with("exec: INSERT INTO") && entry.contains("schema_backfills")
            }),
            "{log:?}"
        );
        assert!(
            !log.iter()
                .any(|entry| entry.starts_with("batch: DROP TRIGGER")),
            "{log:?}"
        );
    }

    #[compio::test]
    async fn guard_install_recovers_create_before_checkpoint_and_cleanup_is_ordered() {
        let spec = spec(CursorStability::GuardUpdates);
        let version = MigrationId::derive("mysql-guard-test", b"events");
        let checksum = checksum("guarded plan");
        let descriptor = guard_descriptor(&version, &spec).unwrap();
        assert!(descriptor.action_statement.contains("OLD.`tenant_id`"));
        assert!(descriptor.action_statement.contains("OLD.`slug`"));

        let rec = RecordingSession::new();
        rec.managed(&descriptor);
        ensure_guard_installed(
            &rec,
            &ExecutorConfig::new("prj_x", "app"),
            version.as_str(),
            &checksum,
            &spec,
            &descriptor,
            GUARD_PLANNED,
            false,
        )
        .await
        .expect("install guard");
        let log = rec.log.borrow().clone();
        let create = log
            .iter()
            .position(|entry| entry.starts_with("batch: CREATE TRIGGER"))
            .unwrap();
        let checkpoint = log
            .iter()
            .position(|entry| entry.contains("SET guard_state = ?"))
            .unwrap();
        assert!(create < checkpoint, "{log:?}");

        // A crash after CREATE but before the installed checkpoint is reconciled
        // by proving the exact existing managed definition, without a second DDL.
        let before = rec
            .log
            .borrow()
            .clone()
            .iter()
            .filter(|entry| entry.starts_with("batch: CREATE TRIGGER"))
            .count();
        ensure_guard_installed(
            &rec,
            &ExecutorConfig::new("prj_x", "app"),
            version.as_str(),
            &checksum,
            &spec,
            &descriptor,
            GUARD_PLANNED,
            false,
        )
        .await
        .expect("recover installed guard");
        let after = rec
            .log
            .borrow()
            .iter()
            .filter(|entry| entry.starts_with("batch: CREATE TRIGGER"))
            .count();
        assert_eq!(before, after);

        cleanup_guard(
            &rec,
            &ExecutorConfig::new("prj_x", "app"),
            version.as_str(),
            &checksum,
            &spec,
            &descriptor,
        )
        .await
        .expect("cleanup guard");
        let log = rec.log.borrow();
        let drop_guard = log
            .iter()
            .position(|entry| entry.starts_with("batch: DROP TRIGGER"))
            .unwrap();
        let cleaned = log
            .iter()
            .rposition(|entry| entry.contains("SET guard_state = ?"))
            .unwrap();
        assert!(drop_guard < cleaned, "{log:?}");
    }

    #[compio::test]
    async fn unknown_or_changed_trigger_interactions_fail_closed() {
        let spec = spec(CursorStability::GuardUpdates);
        let version = MigrationId::derive("mysql-guard-test", b"events");
        let descriptor = guard_descriptor(&version, &spec).unwrap();
        let rec = RecordingSession::new();
        *rec.trigger.borrow_mut() = Some(TriggerRecord {
            name: "application_audit".into(),
            timing: "AFTER".into(),
            event: "UPDATE".into(),
            orientation: "ROW".into(),
            statement: "SET @seen = 1".into(),
        });
        assert!(
            inspect_guard_interactions(&rec, &spec, Some(&descriptor), true)
                .await
                .is_err()
        );

        *rec.trigger.borrow_mut() = Some(TriggerRecord {
            name: descriptor.name.clone(),
            timing: "BEFORE".into(),
            event: "UPDATE".into(),
            orientation: "ROW".into(),
            statement: "BEGIN SET @tampered = 1; END".into(),
        });
        assert!(
            inspect_guard_interactions(&rec, &spec, Some(&descriptor), false)
                .await
                .is_err()
        );
    }

    #[compio::test]
    async fn data_write_and_typed_tuple_checkpoint_share_one_transaction() {
        let spec = spec(CursorStability::ExternalInvariant {
            name: "writers set done".into(),
        });
        let contract = spec.cursor_contract.as_ref().unwrap().clone();
        let runtime = runtime(&contract);
        let rec = RecordingSession::new();
        *rec.window.borrow_mut() = vec![
            Row::new(
                vec!["cursor_value_0".into(), "cursor_value_1".into()],
                vec![Value::Text("1".into()), Value::Text("a|b".into())],
            ),
            Row::new(
                vec!["cursor_value_0".into(), "cursor_value_1".into()],
                vec![Value::Text("2".into()), Value::Text("z\0x".into())],
            ),
        ];
        let cfg = ExecutorConfig::new("prj_x", "app");
        let checksum = checksum("atomic tuple plan");
        let end = tuple(9, "zz");
        rec.batch("START TRANSACTION").await.unwrap();
        let selected = run_one_batch_inner(
            &rec,
            &cfg,
            "v1",
            &checksum,
            "`app`.`events`",
            &runtime,
            &spec,
            &contract,
            None,
            &end,
            "SELECT typed window AS cursor_value_0",
            &[],
        )
        .await
        .unwrap();
        rec.batch("COMMIT").await.unwrap();
        assert_eq!(selected.len(), 2);
        assert_eq!(
            selected[1].to_json().unwrap(),
            r#"[{"int64":"2"},"z\u0000x"]"#
        );
        let log = rec.log.borrow();
        let begin = log
            .iter()
            .position(|entry| entry == "batch: START TRANSACTION")
            .unwrap();
        let target = log
            .iter()
            .position(|entry| entry.starts_with("exec: UPDATE `app`.`events` SET"))
            .unwrap();
        let progress = log
            .iter()
            .position(|entry| entry.contains("SET last_cursor = ?"))
            .unwrap();
        let commit = log
            .iter()
            .position(|entry| entry == "batch: COMMIT")
            .unwrap();
        assert!(
            begin < target && target < progress && progress < commit,
            "{log:?}"
        );
        assert!(log[target].contains("`tenant_id` <=> CAST(? AS SIGNED)"));
        assert!(log[target].contains("`slug` <=> CONVERT(? USING utf8mb4)"));
    }
}
