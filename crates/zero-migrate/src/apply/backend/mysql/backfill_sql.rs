//! Crash-safe, cursor-paged MySQL backfill execution.
//!
//! Each batch selects and locks at most `batch_size` unique cursor keys, updates
//! exactly those keys with native parameters, and advances the engine-owned
//! progress row in the same InnoDB transaction. A retry therefore starts after
//! the last committed key and never trusts an uncommitted cursor.

use std::time::Instant;

use crate::apply::backend::{BackfillOutcome, BackfillProgressEntry, BackfillSpec};
use crate::apply::executor::ApplyError;
use crate::apply::journal::{CompletedRecord, EventKind, JournalError};
use crate::conn::ExecutorConfig;
use crate::driver::{Bind, Row, SqlSession};
use crate::model::backfill::generate_per_row_value;
use crate::model::ir::PerRowGenerator;
use crate::model::migration::{Checksum, MigrationId};

use super::{journal_sql, session};

struct Progress {
    last_cursor: Option<String>,
    end_cursor: Option<String>,
    cohort_initialized: bool,
    complete: bool,
    exists: bool,
    checksum: Option<String>,
}

#[derive(Debug, Clone)]
struct CursorType {
    bind_expression: String,
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
    validate_spec(spec)?;
    session::configure_data_session(conn, cfg).await?;
    ensure_progress_table(conn, cfg).await?;

    // The plan-step version is stable across content edits and is therefore the
    // progress key. Content-derived ids would orphan the old cursor on an edit
    // and could silently restart a changed transform from the beginning.
    let backfill_id = version.as_str().to_string();
    let progress = read_progress(conn, cfg, &backfill_id).await?;
    if progress.exists {
        let Some(recorded) = progress.checksum.as_deref() else {
            return Err(ApplyError::ChecksumDrift {
                version: version.as_str().to_string(),
                recorded: "<missing legacy checksum>".to_string(),
                expected: checksum.as_str().to_string(),
            });
        };
        if recorded != checksum.as_str() {
            return Err(ApplyError::ChecksumDrift {
                version: version.as_str().to_string(),
                recorded: recorded.to_string(),
                expected: checksum.as_str().to_string(),
            });
        }
    }

    let resumed = progress.exists && progress.last_cursor.is_some();
    let started = Instant::now();
    if progress.complete {
        finalize_backfill(conn, cfg, version, checksum, spec, applied_by, 0).await?;
        return Ok(BackfillOutcome {
            backfill_id,
            batches: 0,
            rows_updated: 0,
            resumed,
            complete: true,
        });
    }
    if progress.exists && !progress.cohort_initialized {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: incomplete legacy progress for {backfill_id:?} has no terminal \
             cohort boundary; refusing an unsafe resume"
        )));
    }
    if progress.cohort_initialized
        && progress.end_cursor.is_none()
        && progress.last_cursor.is_some()
    {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: progress for {backfill_id:?} records a cursor for an empty cohort"
        )));
    }

    let schema_q = journal_sql::quote_ident_mysql(&spec.schema)?;
    let table_q = quote_bare("table", &spec.table)?;
    let cursor_q = quote_bare("cursor_column", &spec.cursor_column)?;
    let qualified_table = format!("{schema_q}.{table_q}");
    let mut last_cursor = progress.last_cursor;
    let end_cursor = if progress.exists {
        progress.end_cursor
    } else {
        initialize_progress(
            conn,
            cfg,
            &backfill_id,
            checksum,
            &qualified_table,
            &cursor_q,
            spec,
            applied_by,
        )
        .await?
    };
    let mut batches = 0_u64;
    let mut rows_updated = 0_u64;

    if let Some(end_cursor) = end_cursor.as_deref() {
        loop {
            let selected = run_one_batch(
                conn,
                cfg,
                &backfill_id,
                checksum,
                &qualified_table,
                &cursor_q,
                spec,
                last_cursor.as_deref(),
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

    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    finalize_backfill(conn, cfg, version, checksum, spec, applied_by, exec_ms).await?;

    Ok(BackfillOutcome {
        backfill_id,
        batches,
        rows_updated,
        resumed,
        complete: true,
    })
}

fn validate_spec(spec: &BackfillSpec) -> Result<(), ApplyError> {
    quote_bare("table", &spec.table)?;
    quote_bare("cursor_column", &spec.cursor_column)?;
    if spec.batch_size == 0 {
        return Err(ApplyError::Backend(
            "mysql backfill: batch_size must be non-zero".to_string(),
        ));
    }
    if spec.set_clause.trim().is_empty() && spec.per_row.is_empty() {
        return Err(ApplyError::Backend(
            "mysql backfill: backfill set must not be empty".to_string(),
        ));
    }
    if !spec.set_clause.trim().is_empty() {
        assert_cursor_not_mutated(&spec.set_clause, &spec.cursor_column)?;
    }
    for (column, assignment) in &spec.per_row {
        let generator = assignment.generator();
        quote_bare("per-row destination column", column)?;
        if column.eq_ignore_ascii_case(&spec.cursor_column) {
            return Err(ApplyError::Backend(format!(
                "mysql backfill: per-row generator assigns cursor column {:?}; page on an immutable key",
                spec.cursor_column
            )));
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
    Ok(())
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

/// Refuse a transform that assigns the key it pages on. The assembler emits a
/// comma-separated assignment list; this scanner recognizes only top-level
/// assignment boundaries and skips strings, quoted identifiers, and parentheses.
fn assert_cursor_not_mutated(set_clause: &str, cursor: &str) -> Result<(), ApplyError> {
    let needle = format!("`{cursor}` =");
    let bytes = set_clause.as_bytes();
    let mut i = 0_usize;
    let mut depth = 0_u32;
    let mut in_string = false;
    let mut in_ident = false;
    let mut at_boundary = true;

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
                at_boundary = false;
                i += 1;
            }
            b'`' if at_boundary && depth == 0 => {
                if set_clause[i..].starts_with(&needle) {
                    return Err(ApplyError::Backend(format!(
                        "mysql backfill: set clause assigns cursor column {cursor:?}"
                    )));
                }
                in_ident = true;
                at_boundary = false;
                i += 1;
            }
            b'`' => {
                in_ident = true;
                at_boundary = false;
                i += 1;
            }
            b'(' => {
                depth = depth.saturating_add(1);
                at_boundary = false;
                i += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                at_boundary = false;
                i += 1;
            }
            b',' if depth == 0 => {
                at_boundary = true;
                i += 1;
            }
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            _ => {
                at_boundary = false;
                i += 1;
            }
        }
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
            name           VARCHAR(255) NOT NULL,
            target_schema  VARCHAR(255) NOT NULL,
            target_table   VARCHAR(255) NOT NULL,
            cursor_column  VARCHAR(255) NOT NULL,
            last_cursor    LONGTEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin,
            end_cursor     LONGTEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin,
            cohort_initialized BOOLEAN NOT NULL DEFAULT FALSE,
            rows_done      BIGINT UNSIGNED NOT NULL DEFAULT 0,
            batches_done   BIGINT UNSIGNED NOT NULL DEFAULT 0,
            complete       BOOLEAN NOT NULL DEFAULT FALSE,
            applied_by     VARCHAR(255) NOT NULL,
            started_at     TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
            updated_at     TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
                                           ON UPDATE CURRENT_TIMESTAMP(6)
        ) ENGINE=InnoDB"
    ))
    .await?;
    let checksum_column = conn
        .query(
            "SELECT COLUMN_NAME AS column_name
               FROM information_schema.COLUMNS
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'schema_backfills'
                AND COLUMN_NAME = 'checksum'
              LIMIT 1",
            &[cfg.pg.meta_schema.as_str().into()],
        )
        .await?;
    if checksum_column.is_empty() {
        conn.batch(&format!(
            "ALTER TABLE {meta}.schema_backfills
                 ADD COLUMN checksum VARCHAR(255)
                     CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NULL"
        ))
        .await?;
    }
    for (column, definition) in [
        (
            "end_cursor",
            "end_cursor LONGTEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NULL",
        ),
        (
            "cohort_initialized",
            "cohort_initialized BOOLEAN NOT NULL DEFAULT FALSE",
        ),
    ] {
        let existing = conn
            .query(
                "SELECT COLUMN_NAME AS column_name
                   FROM information_schema.COLUMNS
                  WHERE TABLE_SCHEMA = ? AND TABLE_NAME = 'schema_backfills'
                    AND COLUMN_NAME = ?
                  LIMIT 1",
                &[
                    cfg.pg.meta_schema.as_str().into(),
                    Bind::Text(column.to_string()),
                ],
            )
            .await?;
        if existing.is_empty() {
            conn.batch(&format!(
                "ALTER TABLE {meta}.schema_backfills ADD COLUMN {definition}"
            ))
            .await?;
        }
    }
    Ok(())
}

async fn read_progress<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
) -> Result<Progress, ApplyError> {
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "SELECT last_cursor, end_cursor,
                        CAST(cohort_initialized AS SIGNED) AS cohort_initialized,
                        CAST(complete AS SIGNED) AS complete, checksum
                   FROM {meta}.schema_backfills WHERE backfill_id = ?"
            ),
            &[backfill_id.into()],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(Progress {
            last_cursor: None,
            end_cursor: None,
            cohort_initialized: false,
            complete: false,
            exists: false,
            checksum: None,
        });
    };
    Ok(Progress {
        last_cursor: row.try_get("last_cursor")?,
        end_cursor: row.try_get("end_cursor")?,
        cohort_initialized: row.try_get::<_, i64>("cohort_initialized")? != 0,
        complete: row.try_get::<_, i64>("complete")? != 0,
        exists: true,
        checksum: row.try_get("checksum")?,
    })
}

#[allow(clippy::too_many_arguments)]
async fn initialize_progress<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    checksum: &Checksum,
    qualified_table: &str,
    cursor_q: &str,
    spec: &BackfillSpec,
    applied_by: &str,
) -> Result<Option<String>, ApplyError> {
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    conn.batch("START TRANSACTION").await?;
    let result = async {
        let _cursor_type = validate_target_under_lock(conn, qualified_table, spec).await?;
        // Capture and persist the terminal key in the same transaction. An
        // interrupted initialization leaves no half-initialized progress row.
        let rows = conn
            .query(
                &build_end_cursor_sql(qualified_table, cursor_q, spec.filter.as_deref()),
                &[],
            )
            .await?;
        let end_cursor = rows
            .first()
            .map(|row| row.try_get::<_, String>("end_cursor"))
            .transpose()?;
        let inserted = conn
            .exec(
                &format!(
                    "INSERT INTO {meta}.schema_backfills
                         (backfill_id, checksum, name, target_schema, target_table,
                          cursor_column, end_cursor, cohort_initialized, applied_by)
                     VALUES (?, ?, ?, ?, ?, ?, ?, TRUE, ?)"
                ),
                &[
                    backfill_id.into(),
                    checksum.as_str().into(),
                    spec.name.as_str().into(),
                    spec.schema.as_str().into(),
                    spec.table.as_str().into(),
                    spec.cursor_column.as_str().into(),
                    end_cursor.clone().into(),
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
        Ok::<Option<String>, ApplyError>(end_cursor)
    }
    .await;
    match result {
        Ok(end_cursor) => {
            if let Err(error) = conn.batch("COMMIT").await {
                rollback(
                    conn,
                    backfill_id,
                    "ambiguous cohort initialization COMMIT failure",
                )
                .await;
                return Err(ApplyError::Db(error.into()));
            }
            Ok(end_cursor)
        }
        Err(error) => {
            rollback(conn, backfill_id, "cohort initialization error").await;
            Err(error)
        }
    }
}

/// Atomically mark progress complete and append the ordinary completed journal
/// event for the stable plan-step version. The progress row is locked first, so
/// repeating this function is harmless: an existing matching latest applied
/// event is kept, while a different checksum is always drift.
#[allow(clippy::too_many_arguments)]
async fn finalize_backfill<D: SqlSession>(
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
                    "SELECT checksum FROM {meta}.schema_backfills
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

        conn.exec(
            &format!(
                "UPDATE {meta}.schema_backfills
                    SET complete = TRUE
                  WHERE backfill_id = ? AND checksum = ?"
            ),
            &[version.as_str().into(), checksum.as_str().into()],
        )
        .await?;

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

const CURSOR_PRIMARY_KEY_SQL: &str = "SELECT INDEX_NAME AS index_name
               FROM information_schema.STATISTICS
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND INDEX_NAME = 'PRIMARY'
              GROUP BY INDEX_NAME
             HAVING COUNT(*) = 1
                AND MAX(COLUMN_NAME = ? AND SUB_PART IS NULL) = 1
              LIMIT 1";

async fn validate_cursor<D: SqlSession>(
    conn: &D,
    spec: &BackfillSpec,
) -> Result<CursorType, ApplyError> {
    let columns = conn
        .query(
            "SELECT c.IS_NULLABLE AS is_nullable, c.DATA_TYPE AS data_type,
                    c.COLUMN_TYPE AS column_type,
                    c.CHARACTER_SET_NAME AS character_set_name,
                    c.COLLATION_NAME AS collation_name,
                    c.EXTRA AS extra, c.GENERATION_EXPRESSION AS generation_expression,
                    t.ENGINE AS table_engine
               FROM information_schema.COLUMNS c
               JOIN information_schema.TABLES t
                 ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME
              WHERE c.TABLE_SCHEMA = ? AND c.TABLE_NAME = ? AND c.COLUMN_NAME = ?",
            &[
                spec.schema.as_str().into(),
                spec.table.as_str().into(),
                spec.cursor_column.as_str().into(),
            ],
        )
        .await?;
    let Some(column) = columns.first() else {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: target {}.{} or cursor column {} was not found",
            spec.schema, spec.table, spec.cursor_column
        )));
    };
    let nullable: String = column.try_get("is_nullable")?;
    if !nullable.eq_ignore_ascii_case("NO") {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: cursor column {:?} on {:?} is nullable",
            spec.cursor_column, spec.table
        )));
    }
    let data_type: String = column.try_get("data_type")?;
    if !cursor_type_is_orderable(&data_type) {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: cursor column {:?} has unsupported paging type {:?}",
            spec.cursor_column, data_type
        )));
    }
    let column_type: String = column.try_get("column_type")?;
    let character_set: Option<String> = column.try_get("character_set_name")?;
    let collation: Option<String> = column.try_get("collation_name")?;
    let cursor_type = CursorType {
        bind_expression: mysql_cursor_bind_expression(
            &data_type,
            &column_type,
            character_set.as_deref(),
            collation.as_deref(),
        )?,
    };
    let table_engine: Option<String> = column.try_get("table_engine")?;
    if !table_engine
        .as_deref()
        .is_some_and(|engine| engine.eq_ignore_ascii_case("InnoDB"))
    {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: target table {:?} uses non-transactional or unsupported engine {:?}; crash-safe backfills require InnoDB",
            spec.table, table_engine
        )));
    }
    let extra: Option<String> = column.try_get("extra")?;
    let generation_expression: Option<String> = column.try_get("generation_expression")?;
    if generation_expression
        .as_deref()
        .is_some_and(|expression| !expression.trim().is_empty())
        || extra
            .as_deref()
            .is_some_and(|value| value.to_ascii_lowercase().contains("on update"))
    {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: cursor column {:?} is generated or automatically updated and is not stable during paging",
            spec.cursor_column
        )));
    }

    let primary = conn
        .query(
            CURSOR_PRIMARY_KEY_SQL,
            &[
                spec.schema.as_str().into(),
                spec.table.as_str().into(),
                spec.cursor_column.as_str().into(),
            ],
        )
        .await?;
    if primary.is_empty() {
        return Err(ApplyError::Backend(format!(
            "mysql backfill: cursor column {:?} on {:?} is not the complete single-column PRIMARY KEY",
            spec.cursor_column, spec.table
        )));
    }
    Ok(cursor_type)
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

fn cursor_type_is_orderable(data_type: &str) -> bool {
    matches!(
        data_type.to_ascii_lowercase().as_str(),
        "tinyint"
            | "smallint"
            | "mediumint"
            | "int"
            | "integer"
            | "bigint"
            | "decimal"
            | "numeric"
            | "char"
            | "varchar"
            | "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
            | "date"
            | "datetime"
            | "timestamp"
            | "time"
            | "year"
    )
}

fn build_window_sql(
    qualified_table: &str,
    cursor_q: &str,
    cursor_type: &CursorType,
    filter: Option<&str>,
    have_cursor: bool,
) -> String {
    let cursor_predicate = if have_cursor {
        format!("{cursor_q} > {}", cursor_type.bind_expression)
    } else {
        "1 = 1".to_string()
    };
    let filter = filter
        .map(|filter| format!(" AND ({filter})"))
        .unwrap_or_default();
    format!(
        "SELECT CAST({cursor_q} AS CHAR CHARACTER SET utf8mb4) AS cursor_value
           FROM {qualified_table}
          WHERE {cursor_predicate}
            AND {cursor_q} <= {end_cursor}{filter}
          ORDER BY {cursor_q} ASC
          LIMIT ? FOR UPDATE",
        end_cursor = cursor_type.bind_expression,
    )
}

fn build_end_cursor_sql(qualified_table: &str, cursor_q: &str, filter: Option<&str>) -> String {
    let filter = filter
        .map(|filter| format!(" AND ({filter})"))
        .unwrap_or_default();
    format!(
        "SELECT CAST({cursor_q} AS CHAR CHARACTER SET utf8mb4) AS end_cursor
           FROM {qualified_table}
          WHERE 1 = 1{filter}
          ORDER BY {cursor_q} DESC
          LIMIT 1"
    )
}

fn window_binds(last_cursor: Option<&str>, end_cursor: &str, batch_size: u32) -> Vec<Bind> {
    let mut binds = Vec::with_capacity(3);
    if let Some(cursor) = last_cursor {
        binds.push(Bind::Text(cursor.to_string()));
    }
    binds.push(Bind::Text(end_cursor.to_string()));
    binds.push(Bind::Int(i64::from(batch_size)));
    binds
}

fn build_per_row_update_sql(
    qualified_table: &str,
    cursor_q: &str,
    cursor_type: &CursorType,
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
    Ok(format!(
        "UPDATE {qualified_table} SET {} WHERE {cursor_q} = {}",
        assignments.join(", "),
        cursor_type.bind_expression
    ))
}

/// Open the target table and validate every catalog fact the backfill relies on
/// while the transaction retains that table's metadata lock. Each batch calls
/// this independently because MySQL releases metadata locks at batch COMMIT.
async fn validate_target_under_lock<D: SqlSession>(
    conn: &D,
    qualified_table: &str,
    spec: &BackfillSpec,
) -> Result<CursorType, ApplyError> {
    conn.query(
        &format!("SELECT 1 AS zero_migrate_metadata_lock FROM {qualified_table} LIMIT 0"),
        &[],
    )
    .await?;
    let cursor_type = validate_cursor(conn, spec).await?;
    super::ensure_no_user_triggers(conn, &spec.schema, &spec.table).await?;
    Ok(cursor_type)
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
    cursor_q: &str,
    spec: &BackfillSpec,
    last_cursor: Option<&str>,
    end_cursor: &str,
) -> Result<Vec<String>, ApplyError> {
    conn.batch("START TRANSACTION").await?;
    let result = async {
        let cursor_type = validate_target_under_lock(conn, qualified_table, spec).await?;
        let window_binds = window_binds(last_cursor, end_cursor, spec.batch_size);
        let window_sql = build_window_sql(
            qualified_table,
            cursor_q,
            &cursor_type,
            spec.filter.as_deref(),
            last_cursor.is_some(),
        );
        run_one_batch_inner(
            conn,
            cfg,
            backfill_id,
            checksum,
            qualified_table,
            cursor_q,
            &cursor_type,
            spec,
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
    cursor_q: &str,
    cursor_type: &CursorType,
    spec: &BackfillSpec,
    window_sql: &str,
    window_binds: &[Bind],
) -> Result<Vec<String>, ApplyError> {
    let rows: Vec<Row> = conn.query(window_sql, window_binds).await?;
    let selected = rows
        .iter()
        .map(|row| row.try_get::<_, String>("cursor_value"))
        .collect::<Result<Vec<_>, _>>()?;
    if selected.is_empty() {
        return Ok(selected);
    }

    if spec.per_row.is_empty() {
        let placeholders = vec![cursor_type.bind_expression.as_str(); selected.len()].join(", ");
        let update_sql = format!(
            "UPDATE {qualified_table} SET {} WHERE {cursor_q} IN ({placeholders})",
            spec.set_clause
        );
        let selected_binds = selected.iter().cloned().map(Bind::Text).collect::<Vec<_>>();
        conn.exec(&update_sql, &selected_binds)
            .await
            .map_err(|error| ApplyError::MigrationFailed {
                version: spec.name.clone(),
                source: error.into(),
            })?;
    } else {
        let update_sql = build_per_row_update_sql(qualified_table, cursor_q, cursor_type, spec)?;
        for selected_cursor in &selected {
            let mut row_binds = spec
                .per_row
                .values()
                .map(|assignment| generate_per_row_value(assignment.generator()))
                .map(Bind::Text)
                .collect::<Vec<_>>();
            row_binds.push(Bind::Text(selected_cursor.clone()));
            let affected = conn.exec(&update_sql, &row_binds).await.map_err(|error| {
                ApplyError::MigrationFailed {
                    version: spec.name.clone(),
                    source: error.into(),
                }
            })?;
            if affected != 1 {
                return Err(ApplyError::Backend(format!(
                    "mysql backfill: per-row update at cursor {selected_cursor:?} affected {affected} rows; expected exactly one"
                )));
            }
        }
    }

    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    let last = selected
        .last()
        .expect("a non-empty selected window has a last cursor");
    let advanced = conn
        .exec(
            &format!(
                "UPDATE {meta}.schema_backfills
                    SET last_cursor = ?, rows_done = rows_done + ?,
                        batches_done = batches_done + 1
                  WHERE backfill_id = ? AND checksum = ?"
            ),
            &[
                last.as_str().into(),
                i64::try_from(selected.len()).unwrap_or(i64::MAX).into(),
                backfill_id.into(),
                checksum.as_str().into(),
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
    use std::cell::{Cell, RefCell};

    type ProgressRow = (Option<String>, Option<String>, bool, bool, Option<String>);

    struct RecordingSession {
        log: RefCell<Vec<String>>,
        binds: RefCell<Vec<Vec<Bind>>>,
        windows: Cell<u32>,
        progress: RefCell<Option<ProgressRow>>,
        captured_end_cursor: RefCell<Option<String>>,
        cursor_as_text: Cell<bool>,
        journal: RefCell<Option<(String, String)>>,
    }

    impl RecordingSession {
        fn new() -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                binds: RefCell::new(Vec::new()),
                windows: Cell::new(0),
                progress: RefCell::new(None),
                captured_end_cursor: RefCell::new(Some("2".to_string())),
                cursor_as_text: Cell::new(true),
                journal: RefCell::new(None),
            }
        }

        fn with_progress(last: Option<&str>, complete: bool, checksum: &Checksum) -> Self {
            let session = Self::new();
            *session.progress.borrow_mut() = Some((
                last.map(str::to_string),
                Some("99".to_string()),
                true,
                complete,
                Some(checksum.as_str().to_string()),
            ));
            session
        }

        fn with_legacy_progress(last: Option<&str>) -> Self {
            let session = Self::new();
            *session.progress.borrow_mut() =
                Some((last.map(str::to_string), None, false, false, None));
            session
        }

        fn with_unbounded_progress(last: Option<&str>, checksum: &Checksum) -> Self {
            let session = Self::new();
            *session.progress.borrow_mut() = Some((
                last.map(str::to_string),
                None,
                false,
                false,
                Some(checksum.as_str().to_string()),
            ));
            session
        }

        fn rows_for(&self, sql: &str) -> Vec<Row> {
            if sql.contains("SELECT last_cursor") && sql.contains("schema_backfills") {
                return self
                    .progress
                    .borrow()
                    .as_ref()
                    .map_or_else(Vec::new, |row| {
                        vec![Row::new(
                            vec![
                                "last_cursor".into(),
                                "end_cursor".into(),
                                "cohort_initialized".into(),
                                "complete".into(),
                                "checksum".into(),
                            ],
                            vec![
                                row.0.clone().map_or(Value::Null, Value::Text),
                                row.1.clone().map_or(Value::Null, Value::Text),
                                Value::Int(i64::from(row.2)),
                                Value::Int(i64::from(row.3)),
                                row.4.clone().map_or(Value::Null, Value::Text),
                            ],
                        )]
                    });
            }
            if sql.contains("SELECT checksum FROM") && sql.contains("FOR UPDATE") {
                return self
                    .progress
                    .borrow()
                    .as_ref()
                    .map_or_else(Vec::new, |row| {
                        vec![Row::new(
                            vec!["checksum".into()],
                            vec![row.4.clone().map_or(Value::Null, Value::Text)],
                        )]
                    });
            }
            if sql.contains("SELECT event_kind, checksum") && sql.contains("schema_migrations") {
                return self.journal.borrow().as_ref().map_or_else(Vec::new, |row| {
                    vec![Row::new(
                        vec!["event_kind".into(), "checksum".into()],
                        vec![Value::Text("applied".into()), Value::Text(row.1.clone())],
                    )]
                });
            }
            if sql.contains("information_schema.COLUMNS") {
                return vec![Row::new(
                    vec![
                        "is_nullable".into(),
                        "data_type".into(),
                        "column_type".into(),
                        "character_set_name".into(),
                        "collation_name".into(),
                        "extra".into(),
                        "generation_expression".into(),
                        "table_engine".into(),
                    ],
                    vec![
                        Value::Text("NO".into()),
                        Value::Text("bigint".into()),
                        Value::Text("bigint".into()),
                        Value::Null,
                        Value::Null,
                        Value::Text(String::new()),
                        Value::Text(String::new()),
                        Value::Text("InnoDB".into()),
                    ],
                )];
            }
            if sql.contains("information_schema.STATISTICS") {
                return vec![Row::new(
                    vec!["index_name".into()],
                    vec![Value::Text("PRIMARY".into())],
                )];
            }
            if sql.contains("AS end_cursor") {
                return self.captured_end_cursor.borrow().as_ref().map_or_else(
                    Vec::new,
                    |cursor| {
                        vec![Row::new(
                            vec!["end_cursor".into()],
                            vec![Value::Text(cursor.clone())],
                        )]
                    },
                );
            }
            if sql.contains("AS cursor_value") {
                self.windows.set(self.windows.get() + 1);
                if !self.cursor_as_text.get() {
                    return vec![Row::new(
                        vec!["cursor_value".into()],
                        vec![Value::Int(9_007_199_254_740_993_i64)],
                    )];
                }
                return vec![
                    Row::new(vec!["cursor_value".into()], vec![Value::Text("1".into())]),
                    Row::new(vec!["cursor_value".into()], vec![Value::Text("2".into())]),
                ];
            }
            Vec::new()
        }
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.log.borrow_mut().push(format!("batch: {sql}"));
            Ok(())
        }

        async fn exec(&self, sql: &str, params: &[Bind]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            if sql.contains("INSERT INTO") && sql.contains("schema_backfills") {
                let checksum = match params.get(1) {
                    Some(Bind::Text(value)) => value.clone(),
                    other => {
                        return Err(DbError::message(format!(
                            "missing checksum bind: {other:?}"
                        )));
                    }
                };
                let end_cursor = match params.get(6) {
                    Some(Bind::Text(value)) => Some(value.clone()),
                    Some(Bind::Null) => None,
                    other => {
                        return Err(DbError::message(format!(
                            "missing end-cursor bind: {other:?}"
                        )));
                    }
                };
                *self.progress.borrow_mut() = Some((None, end_cursor, true, false, Some(checksum)));
            } else if sql.contains("SET complete = TRUE") && sql.contains("schema_backfills") {
                if let Some(progress) = self.progress.borrow_mut().as_mut() {
                    progress.3 = true;
                }
            } else if sql.contains("SET last_cursor = ?") && sql.contains("schema_backfills") {
                if let (Some(progress), Some(Bind::Text(last))) =
                    (self.progress.borrow_mut().as_mut(), params.first())
                {
                    progress.0 = Some(last.clone());
                }
            } else if sql.contains("INSERT INTO") && sql.contains("schema_migrations") {
                let version = match params.first() {
                    Some(Bind::Text(value)) => value.clone(),
                    other => {
                        return Err(DbError::message(format!("missing version bind: {other:?}")));
                    }
                };
                let checksum = match params.get(2) {
                    Some(Bind::Text(value)) => value.clone(),
                    other => {
                        return Err(DbError::message(format!(
                            "missing checksum bind: {other:?}"
                        )));
                    }
                };
                *self.journal.borrow_mut() = Some((version, checksum));
            }
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
                .ok_or_else(|| DbError::message("no canned row"))
        }
    }

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

    #[test]
    fn window_sql_uses_native_cursor_and_limit_binds() {
        let cursor_type = CursorType {
            bind_expression: "CAST(? AS SIGNED)".into(),
        };
        let first = build_window_sql(
            "`app`.`users`",
            "`id`",
            &cursor_type,
            Some("`done` = FALSE"),
            false,
        );
        assert!(first.contains("WHERE 1 = 1"));
        assert!(first.contains("AND (`done` = FALSE)"));
        assert!(first.contains("AND `id` <= CAST(? AS SIGNED)"));
        assert!(first.contains("ORDER BY `id` ASC"));
        assert!(first.contains("LIMIT ? FOR UPDATE"));

        let resumed = build_window_sql("`app`.`users`", "`id`", &cursor_type, None, true);
        assert!(resumed.contains("WHERE `id` > CAST(? AS SIGNED)"));
        assert!(resumed.contains("AND `id` <= CAST(? AS SIGNED)"));
        assert!(resumed.contains("LIMIT ? FOR UPDATE"));
        assert!(!resumed.contains("$1"));
        assert_eq!(
            window_binds(None, "99", 250),
            [Bind::Text("99".into()), Bind::Int(250)]
        );
        assert_eq!(
            window_binds(Some("42"), "99", 250),
            [
                Bind::Text("42".into()),
                Bind::Text("99".into()),
                Bind::Int(250),
            ]
        );
    }

    #[test]
    fn cursor_binds_are_cast_to_catalog_types() {
        assert_eq!(
            mysql_cursor_bind_expression("bigint", "bigint unsigned", None, None).unwrap(),
            "CAST(? AS UNSIGNED)"
        );
        assert_eq!(
            mysql_cursor_bind_expression("decimal", "decimal(38,7)", None, None).unwrap(),
            "CAST(? AS DECIMAL(38,7))"
        );
        assert_eq!(
            mysql_cursor_bind_expression(
                "varchar",
                "varchar(200)",
                Some("utf8mb4"),
                Some("utf8mb4_bin")
            )
            .unwrap(),
            "CONVERT(? USING utf8mb4) COLLATE utf8mb4_bin"
        );
        assert_eq!(
            mysql_cursor_bind_expression("timestamp", "timestamp(6)", None, None).unwrap(),
            "CAST(? AS DATETIME(6))"
        );
    }

    #[test]
    fn cursor_must_be_the_single_column_primary_key() {
        assert!(CURSOR_PRIMARY_KEY_SQL.contains("INDEX_NAME = 'PRIMARY'"));
        assert!(!CURSOR_PRIMARY_KEY_SQL.contains("NON_UNIQUE = 0"));
        assert!(CURSOR_PRIMARY_KEY_SQL.contains("SUB_PART IS NULL"));
    }

    #[test]
    fn cursor_assignment_is_rejected_without_function_false_positives() {
        assert!(assert_cursor_not_mutated("`id` = `id` + 1", "id").is_err());
        assert!(assert_cursor_not_mutated("`x` = 1, `id` = 9", "id").is_err());
        assert!(assert_cursor_not_mutated("`x` = IF(`ok`, `id` = 9, 0)", "id").is_ok());
        assert!(assert_cursor_not_mutated("`x` = '`id` = 9'", "id").is_ok());
    }

    #[test]
    fn per_row_update_contains_placeholders_not_sampled_values() {
        let mut per_row = std::collections::BTreeMap::new();
        per_row.insert(
            "generated".into(),
            crate::model::backfill::PerRowAssignment::validated(
                "app",
                "users",
                "generated",
                PerRowGenerator::Ulid,
            ),
        );
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 3,
            set_clause: String::new(),
            per_row,
            filter: None,
            name: "generate ids".into(),
        };
        let cursor_type = CursorType {
            bind_expression: "CAST(? AS SIGNED)".into(),
        };
        let sql = build_per_row_update_sql("`app`.`users`", "`id`", &cursor_type, &spec).unwrap();
        assert_eq!(
            sql,
            "UPDATE `app`.`users` SET `generated` = ? WHERE `id` = CAST(? AS SIGNED)"
        );
        assert!(!sql.contains("01J"), "no sampled ULID belongs in SQL");
    }

    #[test]
    fn per_row_assignment_cannot_be_retargeted() {
        let mut per_row = std::collections::BTreeMap::new();
        per_row.insert(
            "generated".into(),
            crate::model::backfill::PerRowAssignment::validated(
                "app",
                "other_users",
                "generated",
                PerRowGenerator::Ulid,
            ),
        );
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 3,
            set_clause: String::new(),
            per_row,
            filter: None,
            name: "generate ids".into(),
        };
        let error = validate_spec(&spec).expect_err("retargeted token must fail");
        assert!(error
            .to_string()
            .contains("validated for a different target"));
    }

    #[test]
    fn unsafe_cursor_types_fail_closed() {
        for ty in ["float", "double", "json", "blob", "geometry", "enum"] {
            assert!(!cursor_type_is_orderable(ty), "{ty} must be rejected");
        }
        for ty in ["bigint", "decimal", "varchar", "datetime"] {
            assert!(cursor_type_is_orderable(ty), "{ty} must be supported");
        }
    }

    #[compio::test]
    async fn batch_update_and_progress_advance_commit_together() {
        let rec = RecordingSession::new();
        let cfg = ExecutorConfig::new("prj_x", "app");
        let version = MigrationId::derive("mysql-backfill-test", b"finish-users");
        let checksum = checksum("authoritative backfill artifact");
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 3,
            set_clause: "`done` = TRUE".into(),
            per_row: std::collections::BTreeMap::new(),
            filter: Some("`done` = FALSE".into()),
            name: "finish users".into(),
        };

        let outcome = run_backfill(&rec, &cfg, &version, &checksum, &spec, "tester")
            .await
            .expect("backfill runs");
        assert_eq!(outcome.batches, 1);
        assert_eq!(outcome.rows_updated, 2);
        assert!(outcome.complete);
        assert_eq!(outcome.backfill_id, version.as_str());

        let log = rec.log.borrow();
        let initialization_begin = log
            .iter()
            .position(|entry| entry == "batch: START TRANSACTION")
            .expect("cohort initialization begins a transaction");
        let capture = log
            .iter()
            .position(|entry| entry.contains("AS end_cursor"))
            .expect("terminal cursor is captured");
        let initialization_lock = log[..capture]
            .iter()
            .position(|entry| entry.contains("zero_migrate_metadata_lock"))
            .expect("initialization acquires the target metadata lock");
        let initialization_validation = log[..capture]
            .iter()
            .position(|entry| entry.contains("c.IS_NULLABLE AS is_nullable"))
            .expect("initialization validates the cursor under the target lock");
        let progress_insert = log
            .iter()
            .position(|entry| {
                entry.starts_with("exec: INSERT INTO `app_migrations`.schema_backfills")
            })
            .expect("initialized cohort is persisted");
        let initialization_commit = log
            .iter()
            .position(|entry| entry == "batch: COMMIT")
            .expect("cohort initialization commits");
        assert!(
            initialization_begin < capture
                && initialization_begin < initialization_lock
                && initialization_lock < initialization_validation
                && initialization_validation < capture
                && capture < progress_insert
                && progress_insert < initialization_commit,
            "terminal capture and progress insertion must commit together: {log:?}"
        );
        let target_update = log
            .iter()
            .position(|entry| {
                entry.starts_with(
                    "exec: UPDATE `app`.`users` SET `done` = TRUE WHERE `id` IN (CAST(? AS SIGNED), CAST(? AS SIGNED))",
                )
            })
            .expect("bounded target update executes");
        let begin = log[..target_update]
            .iter()
            .rposition(|entry| entry == "batch: START TRANSACTION")
            .expect("batch transaction begins");
        let batch_lock = log[..target_update]
            .iter()
            .rposition(|entry| entry.contains("zero_migrate_metadata_lock"))
            .expect("batch acquires the target metadata lock");
        let batch_validation = log[..target_update]
            .iter()
            .rposition(|entry| entry.contains("c.IS_NULLABLE AS is_nullable"))
            .expect("batch validates the cursor under its target lock");
        let progress = log
            .iter()
            .position(|entry| {
                entry.starts_with(
                    "exec: UPDATE `app_migrations`.schema_backfills\n                    SET last_cursor",
                )
            })
            .expect("progress advances");
        let commit = log[target_update..]
            .iter()
            .position(|entry| entry == "batch: COMMIT")
            .map(|offset| target_update + offset)
            .expect("batch commits");
        assert!(
            begin < batch_lock
                && batch_lock < batch_validation
                && batch_validation < target_update
                && target_update < progress
                && progress < commit,
            "{log:?}"
        );
        assert!(
            log.iter().any(|entry| {
                entry.contains("LIMIT ? FOR UPDATE")
                    && entry.contains("WHERE 1 = 1")
                    && entry.contains("AND (`done` = FALSE)")
                    && entry.contains("AND `id` <= CAST(? AS SIGNED)")
            }),
            "the window is bounded, filtered, ordered, and locked: {log:?}"
        );
        assert!(
            rec.binds
                .borrow()
                .iter()
                .any(|params| params.as_slice() == [Bind::Text("1".into()), Bind::Text("2".into())]),
            "selected cursor values stay native binds: {:?}",
            rec.binds.borrow()
        );
        assert!(
            rec.binds
                .borrow()
                .iter()
                .any(|params| { params.as_slice() == [Bind::Text("2".into()), Bind::Int(3)] }),
            "the terminal cursor precedes the limit bind: {:?}",
            rec.binds.borrow()
        );

        let final_begin = log
            .iter()
            .rposition(|entry| entry == "batch: START TRANSACTION")
            .expect("finalization transaction begins");
        let complete = log
            .iter()
            .position(|entry| {
                entry.starts_with(
                    "exec: UPDATE `app_migrations`.schema_backfills\n                    SET complete = TRUE",
                )
            })
            .expect("progress is marked complete");
        let journal = log
            .iter()
            .position(|entry| {
                entry.starts_with("exec: INSERT INTO `app_migrations`.schema_migrations")
            })
            .expect("normal completed event is appended");
        let final_commit = log
            .iter()
            .rposition(|entry| entry == "batch: COMMIT")
            .expect("finalization transaction commits");
        assert!(
            final_begin < complete && complete < journal && journal < final_commit,
            "completion and its journal event must commit atomically: {log:?}"
        );
        assert!(
            rec.binds.borrow().iter().any(|params| {
                params.first() == Some(&Bind::Text(version.as_str().to_string()))
                    && params.get(2) == Some(&Bind::Text(checksum.as_str().to_string()))
            }),
            "the authoritative checksum is persisted in the completed event: {:?}",
            rec.binds.borrow()
        );
    }

    #[compio::test]
    async fn per_row_batch_binds_a_fresh_value_for_each_selected_key() {
        let rec = RecordingSession::new();
        let cfg = ExecutorConfig::new("prj_x", "app");
        let version = MigrationId::derive("mysql-backfill-test", b"per-row-users");
        let checksum = checksum("per-row backfill artifact");
        let mut per_row = std::collections::BTreeMap::new();
        per_row.insert(
            "generated".into(),
            crate::model::backfill::PerRowAssignment::validated(
                "app",
                "users",
                "generated",
                PerRowGenerator::Ulid,
            ),
        );
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 3,
            set_clause: String::new(),
            per_row,
            filter: None,
            name: "generate ids".into(),
        };

        let outcome = run_backfill(&rec, &cfg, &version, &checksum, &spec, "tester")
            .await
            .expect("per-row backfill runs");
        assert_eq!(outcome.rows_updated, 2);

        let generated = rec
            .binds
            .borrow()
            .iter()
            .filter_map(|params| match params.as_slice() {
                [Bind::Text(value), Bind::Text(cursor)]
                    if value.len() == 26 && (cursor == "1" || cursor == "2") =>
                {
                    Some(value.clone())
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(generated.len(), 2, "one literal must never be reused");
        assert!(generated.iter().all(|value| value
            .bytes()
            .all(|byte| { b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte) })));
        assert_eq!(
            rec.log
                .borrow()
                .iter()
                .filter(|entry| {
                    entry.starts_with("exec: UPDATE `app`.`users` SET `generated` = ?")
                })
                .count(),
            2,
            "one bound UPDATE per selected row"
        );
    }

    #[compio::test]
    async fn progress_checksum_mismatch_aborts_before_cursor_or_target_io() {
        let version = MigrationId::derive("mysql-backfill-test", b"stable-step");
        let recorded = checksum("old backfill artifact");
        let expected = checksum("edited backfill artifact");
        let rec = RecordingSession::with_progress(Some("42"), false, &recorded);
        let cfg = ExecutorConfig::new("prj_x", "app");
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 3,
            set_clause: "`done` = TRUE".into(),
            per_row: std::collections::BTreeMap::new(),
            filter: None,
            name: "finish users".into(),
        };

        let result = run_backfill(&rec, &cfg, &version, &expected, &spec, "tester").await;
        assert!(matches!(
            result,
            Err(ApplyError::ChecksumDrift {
                version: ref drift_version,
                ref recorded,
                ref expected,
            }) if drift_version == version.as_str()
                && recorded == checksum("old backfill artifact").as_str()
                && expected == checksum("edited backfill artifact").as_str()
        ));
        let log = rec.log.borrow();
        assert!(
            !log.iter().any(|entry| {
                entry.contains("c.IS_NULLABLE AS is_nullable")
                    || entry.contains("UPDATE `app`.`users`")
                    || entry == "batch: START TRANSACTION"
            }),
            "progress drift must abort before validation, resume, or target I/O: {log:?}"
        );
    }

    #[compio::test]
    async fn legacy_progress_without_checksum_fails_closed_before_target_io() {
        let rec = RecordingSession::with_legacy_progress(Some("42"));
        let cfg = ExecutorConfig::new("prj_x", "app");
        let version = MigrationId::derive("mysql-backfill-test", b"legacy-step");
        let checksum = checksum("authoritative artifact");
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 3,
            set_clause: "`done` = TRUE".into(),
            per_row: std::collections::BTreeMap::new(),
            filter: None,
            name: "finish users".into(),
        };

        let result = run_backfill(&rec, &cfg, &version, &checksum, &spec, "tester").await;
        assert!(matches!(
            result,
            Err(ApplyError::ChecksumDrift { ref recorded, .. })
                if recorded == "<missing legacy checksum>"
        ));
        assert!(
            !rec.log.borrow().iter().any(|entry| {
                entry.contains("AS cursor_value") || entry.contains("UPDATE `app`.`users`")
            }),
            "an unanchored legacy cursor must never reach target I/O: {:?}",
            rec.log.borrow()
        );
    }

    #[compio::test]
    async fn incomplete_legacy_progress_without_a_terminal_bound_fails_closed() {
        let checksum = checksum("bounded artifact");
        let rec = RecordingSession::with_unbounded_progress(Some("42"), &checksum);
        let cfg = ExecutorConfig::new("prj_x", "app");
        let version = MigrationId::derive("mysql-backfill-test", b"unbounded-step");
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 3,
            set_clause: "`done` = TRUE".into(),
            per_row: std::collections::BTreeMap::new(),
            filter: None,
            name: "finish users".into(),
        };

        let result = run_backfill(&rec, &cfg, &version, &checksum, &spec, "tester").await;
        assert!(matches!(
            result,
            Err(ApplyError::Backend(ref message))
                if message.contains("has no terminal cohort boundary")
        ));
        assert!(
            !rec.log.borrow().iter().any(|entry| {
                entry.contains("c.IS_NULLABLE AS is_nullable")
                    || entry.contains("AS end_cursor")
                    || entry.contains("AS cursor_value")
                    || entry.contains("UPDATE `app`.`users`")
            }),
            "an unbounded legacy cursor must fail before target I/O: {:?}",
            rec.log.borrow()
        );
    }

    #[compio::test]
    async fn an_empty_initial_cohort_is_initialized_and_completed_without_a_window() {
        let rec = RecordingSession::new();
        *rec.captured_end_cursor.borrow_mut() = None;
        let cfg = ExecutorConfig::new("prj_x", "app");
        let version = MigrationId::derive("mysql-backfill-test", b"empty-cohort");
        let checksum = checksum("empty cohort artifact");
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 3,
            set_clause: "`done` = TRUE".into(),
            per_row: std::collections::BTreeMap::new(),
            filter: Some("`done` = FALSE".into()),
            name: "finish users".into(),
        };

        let outcome = run_backfill(&rec, &cfg, &version, &checksum, &spec, "tester")
            .await
            .expect("empty cohorts complete");
        assert_eq!(outcome.batches, 0);
        assert_eq!(outcome.rows_updated, 0);
        assert_eq!(rec.windows.get(), 0);
        let progress = rec.progress.borrow();
        let progress = progress.as_ref().expect("progress is recorded");
        assert_eq!(progress.1, None, "empty cohort has no terminal cursor");
        assert!(
            progress.2,
            "empty cohort is distinguishable from legacy state"
        );
        assert!(progress.3, "empty cohort is finalized");
        assert!(rec.log.borrow().iter().any(|entry| {
            entry.contains("AS end_cursor") && entry.contains("AND (`done` = FALSE)")
        }));
    }

    #[compio::test]
    async fn completed_progress_and_journal_finalization_are_idempotent() {
        let rec = RecordingSession::new();
        let cfg = ExecutorConfig::new("prj_x", "app");
        let version = MigrationId::derive("mysql-backfill-test", b"idempotent-finish");
        let checksum = checksum("idempotent finish artifact");
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 3,
            set_clause: "`done` = TRUE".into(),
            per_row: std::collections::BTreeMap::new(),
            filter: None,
            name: "finish users".into(),
        };

        run_backfill(&rec, &cfg, &version, &checksum, &spec, "tester")
            .await
            .expect("first run completes and journals");
        let windows_after_first = rec.windows.get();
        run_backfill(&rec, &cfg, &version, &checksum, &spec, "tester")
            .await
            .expect("repeated finalization is a no-op");

        assert_eq!(
            rec.windows.get(),
            windows_after_first,
            "completed progress must not scan the target again"
        );
        let journal_inserts = rec
            .log
            .borrow()
            .iter()
            .filter(|entry| {
                entry.starts_with("exec: INSERT INTO `app_migrations`.schema_migrations")
            })
            .count();
        assert_eq!(journal_inserts, 1, "completion event is append-once");
    }

    #[compio::test]
    async fn bigint_cursor_driver_number_is_rejected_before_any_update() {
        let rec = RecordingSession::new();
        rec.cursor_as_text.set(false);
        let cfg = ExecutorConfig::new("prj_x", "app");
        let version = MigrationId::derive("mysql-backfill-test", b"exact-cursor");
        let checksum = checksum("exact cursor artifact");
        let spec = BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 3,
            set_clause: "`done` = TRUE".into(),
            per_row: std::collections::BTreeMap::new(),
            filter: None,
            name: "finish users".into(),
        };

        let result = run_backfill(&rec, &cfg, &version, &checksum, &spec, "tester").await;
        assert!(
            result.is_err(),
            "a CAST(... AS CHAR) row must cross as text"
        );
        let log = rec.log.borrow();
        assert!(
            log.iter().any(|entry| entry == "batch: ROLLBACK"),
            "the failed cursor decode rolls back its batch: {log:?}"
        );
        assert!(
            !log.iter()
                .any(|entry| entry.contains("UPDATE `app`.`users`")),
            "an inexact driver cursor must never reach the target UPDATE: {log:?}"
        );
    }
}
