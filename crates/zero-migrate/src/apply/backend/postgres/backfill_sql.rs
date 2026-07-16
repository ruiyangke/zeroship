//! PostgreSQL cursor-paged, crash-safe backfills over [`SqlSession`].
//!
//! Each batch updates a bounded key window and advances its progress cursor in
//! the same transaction. The outer plan orchestrator holds the project lock for
//! the whole ordered plan; batch transactions bound row locks and WAL growth.

use serde_json::Value;

use crate::apply::backend::{BackfillOutcome, BackfillProgressEntry, BackfillSpec};
use crate::apply::executor::ApplyError;
use crate::apply::journal::{self, JournalError};
use crate::approval::Approval;
use crate::conn::ExecutorConfig;
use crate::driver::{Bind, SqlSession};
use crate::guard::SqlGuard;
use crate::model::migration::{Checksum, MigrationId};

use super::session::AUTHOR_SQL_LITERAL_MODE;

const CURSOR_SESSION_SETTINGS: &str =
    "SET LOCAL DateStyle TO 'ISO, YMD'; SET LOCAL TimeZone TO 'UTC';";

fn backend_error(message: impl Into<String>) -> ApplyError {
    ApplyError::Backend(format!("postgres backfill: {}", message.into()))
}

fn validate_ident(what: &str, value: &str) -> Result<(), ApplyError> {
    let mut chars = value.chars();
    let valid_first = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    if value.is_empty() || !valid_first || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(backend_error(format!(
            "invalid {what} identifier {value:?}; expected [A-Za-z_][A-Za-z0-9_]*"
        )));
    }
    Ok(())
}

fn quote_ident(value: &str) -> Result<String, ApplyError> {
    Ok(crate::render::dml::quote_ident_checked(value)?)
}

fn build_batch_sql(
    spec: &BackfillSpec,
    cursor_type: &str,
    have_cursor: bool,
) -> Result<String, ApplyError> {
    let schema = quote_ident(&spec.schema)?;
    let table = quote_ident(&spec.table)?;
    let cursor = quote_ident(&spec.cursor_column)?;
    let (cursor_predicate, end_cursor_param) = if have_cursor {
        (format!("{cursor} > ($1::text)::{cursor_type}"), "$2")
    } else {
        ("TRUE".to_string(), "$1")
    };
    let filter = spec
        .filter
        .as_deref()
        .map(|value| format!(" AND ({value})"))
        .unwrap_or_default();

    Ok(format!(
        "WITH _bf_window AS ( \
             SELECT {cursor} AS _bf_key FROM {schema}.{table} \
             WHERE {cursor_predicate} \
               AND {cursor} <= ({end_cursor_param}::text)::{cursor_type}{filter} \
             ORDER BY {cursor} ASC LIMIT {batch_size} \
         ), _bf_updated AS ( \
             UPDATE {schema}.{table} AS _bf SET {set_clause} \
             WHERE _bf.{cursor} IN (SELECT _bf_key FROM _bf_window) \
             RETURNING 1 \
         ) \
         SELECT (SELECT count(*) FROM _bf_window) AS _bf_selected, \
                (SELECT count(*) FROM _bf_updated) AS _bf_rows, \
                (SELECT _bf_key::text FROM _bf_window \
                  ORDER BY _bf_key DESC LIMIT 1) AS _bf_cursor",
        batch_size = spec.batch_size,
        set_clause = spec.set_clause,
    ))
}

fn build_end_cursor_sql(spec: &BackfillSpec) -> Result<String, ApplyError> {
    let schema = quote_ident(&spec.schema)?;
    let table = quote_ident(&spec.table)?;
    let cursor = quote_ident(&spec.cursor_column)?;
    let filter = spec
        .filter
        .as_deref()
        .map(|value| format!(" AND ({value})"))
        .unwrap_or_default();
    Ok(format!(
        "SELECT {cursor}::text AS _bf_end_cursor \
           FROM {schema}.{table} \
          WHERE TRUE{filter} \
          ORDER BY {cursor} DESC LIMIT 1"
    ))
}

fn batch_binds(last_cursor: Option<&str>, end_cursor: &str) -> Vec<Bind> {
    let mut binds = Vec::with_capacity(2);
    if let Some(value) = last_cursor {
        binds.push(Bind::Text(value.to_string()));
    }
    binds.push(Bind::Text(end_cursor.to_string()));
    binds
}

fn batch_session_sql(cfg: &ExecutorConfig, spec: &BackfillSpec) -> Result<String, ApplyError> {
    Ok(format!(
        "{AUTHOR_SQL_LITERAL_MODE} \
         {CURSOR_SESSION_SETTINGS} \
         SET LOCAL search_path TO {}; \
         SET LOCAL statement_timeout = {}; \
         SET LOCAL lock_timeout = {};",
        quote_ident(&spec.schema)?,
        cfg.statement_timeout_ms(),
        cfg.lock_timeout_ms(),
    ))
}

fn assert_cursor_not_mutated(sql: &str, cursor_column: &str) -> Result<(), ApplyError> {
    let parsed = pg_query::parse(sql)
        .map_err(|error| backend_error(format!("could not parse assembled SQL: {error}")))?;
    let tree = serde_json::to_value(&parsed.protobuf)
        .map_err(|error| backend_error(format!("could not inspect assembled SQL: {error}")))?;
    let mut mutated = false;
    walk_update_targets(&tree, &mut |name| {
        if name.eq_ignore_ascii_case(cursor_column) {
            mutated = true;
            true
        } else {
            false
        }
    });
    if mutated {
        return Err(backend_error(format!(
            "set assigns cursor column {cursor_column:?}; page on an immutable key"
        )));
    }
    Ok(())
}

fn walk_update_targets(value: &Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match value {
        Value::Object(map) => {
            if let Some(update) = map.get("UpdateStmt") {
                if let Some(Value::Array(targets)) = update.get("target_list") {
                    for target in targets {
                        let result = target
                            .get("ResTarget")
                            .or_else(|| target.get("node").and_then(|node| node.get("ResTarget")));
                        if let Some(name) = result
                            .and_then(|node| node.get("name"))
                            .and_then(Value::as_str)
                        {
                            if !name.is_empty() && visit(name) {
                                return true;
                            }
                        }
                    }
                }
            }
            map.values().any(|child| walk_update_targets(child, visit))
        }
        Value::Array(items) => items.iter().any(|item| walk_update_targets(item, visit)),
        _ => false,
    }
}

/// Read the engine-owned progress table without creating or altering it.
///
/// Status can run before the first backfill, so the table is optional. Older
/// pre-release tables may also lack `checksum`; those rows are returned with a
/// missing checksum so reconciliation reports drift instead of trusting an
/// unanchored cursor.
pub(super) async fn read_progress_entries<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<BackfillProgressEntry>, JournalError> {
    let catalog = conn
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 FROM pg_catalog.pg_class c \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = 'schema_backfills' \
                   AND c.relkind IN ('r', 'p') \
             ) AS table_exists, \
             EXISTS ( \
                 SELECT 1 FROM pg_catalog.pg_attribute a \
                 JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = 'schema_backfills' \
                   AND a.attname = 'checksum' AND a.attnum > 0 \
                   AND NOT a.attisdropped \
             ) AS checksum_exists",
            &[Bind::Text(cfg.pg.meta_schema.clone())],
        )
        .await?;
    let table_exists: bool = catalog.try_get("table_exists")?;
    if !table_exists {
        return Ok(Vec::new());
    }
    let checksum_exists: bool = catalog.try_get("checksum_exists")?;
    let meta = crate::render::dml::quote_ident_checked(&cfg.pg.meta_schema)?;
    let checksum_expr = if checksum_exists {
        "checksum"
    } else {
        "NULL::text"
    };
    let rows = conn
        .query(
            &format!(
                "SELECT backfill_id, {checksum_expr} AS checksum, complete \
                   FROM {meta}.schema_backfills"
            ),
            &[],
        )
        .await?;
    rows.into_iter()
        .map(|row| {
            Ok(BackfillProgressEntry {
                version: row.try_get("backfill_id")?,
                checksum: row.try_get("checksum")?,
                complete: row.try_get("complete")?,
            })
        })
        .collect()
}

async fn ensure_progress<D: SqlSession>(conn: &D, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    conn.batch(&format!("CREATE SCHEMA IF NOT EXISTS {meta}"))
        .await?;
    conn.batch(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_backfills (\
            backfill_id TEXT PRIMARY KEY, \
            checksum TEXT, \
            name TEXT NOT NULL, \
            target_schema TEXT NOT NULL, \
            target_table TEXT NOT NULL, \
            cursor_column TEXT NOT NULL, \
            last_cursor TEXT, \
            end_cursor TEXT, \
            cohort_initialized BOOLEAN NOT NULL DEFAULT false, \
            rows_done BIGINT NOT NULL DEFAULT 0, \
            batches_done BIGINT NOT NULL DEFAULT 0, \
            complete BOOLEAN NOT NULL DEFAULT false, \
            applied_by TEXT NOT NULL, \
            started_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()\
        )"
    ))
    .await?;
    // Pre-release compatibility for progress tables created by an earlier build.
    conn.batch(&format!(
        "ALTER TABLE {meta}.schema_backfills ADD COLUMN IF NOT EXISTS checksum TEXT"
    ))
    .await?;
    conn.batch(&format!(
        "ALTER TABLE {meta}.schema_backfills ADD COLUMN IF NOT EXISTS end_cursor TEXT"
    ))
    .await?;
    conn.batch(&format!(
        "ALTER TABLE {meta}.schema_backfills \
             ADD COLUMN IF NOT EXISTS cohort_initialized BOOLEAN NOT NULL DEFAULT false"
    ))
    .await?;
    Ok(())
}

async fn resolve_cursor_type<D: SqlSession>(
    conn: &D,
    spec: &BackfillSpec,
) -> Result<String, ApplyError> {
    let rows = conn
        .query(
            "SELECT format_type(a.atttypid, a.atttypmod) AS coltype \
               FROM pg_attribute a \
               JOIN pg_class c ON c.oid = a.attrelid \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3 \
                AND a.attnum > 0 AND NOT a.attisdropped",
            &[
                Bind::Text(spec.schema.clone()),
                Bind::Text(spec.table.clone()),
                Bind::Text(spec.cursor_column.clone()),
            ],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(backend_error(format!(
            "{}.{} column {} was not found",
            spec.schema, spec.table, spec.cursor_column
        )));
    };
    let cursor_type: String = row.try_get("coltype")?;
    if !cursor_type_is_orderable(&cursor_type) {
        return Err(backend_error(format!(
            "cursor column {:?} has unsupported paging type {cursor_type:?}",
            spec.cursor_column
        )));
    }
    Ok(cursor_type)
}

fn cursor_type_is_orderable(cursor_type: &str) -> bool {
    let ty = cursor_type.to_ascii_lowercase();
    matches!(
        ty.as_str(),
        "smallint"
            | "integer"
            | "bigint"
            | "text"
            | "date"
            | "timestamp without time zone"
            | "timestamp with time zone"
            | "uuid"
    ) || ty.starts_with("numeric(")
        || ty == "numeric"
        || ty.starts_with("decimal(")
        || ty == "decimal"
        || ty.starts_with("character varying(")
        || ty == "character varying"
        || ty.starts_with("character(")
        || ty == "character"
}

fn window_reached_tail(selected: u64, batch_size: u32) -> bool {
    selected < u64::from(batch_size)
}

const CURSOR_SAFETY_SQL: &str = "SELECT a.attnotnull AS not_null, \
            EXISTS ( \
                SELECT 1 FROM pg_index i \
                 WHERE i.indrelid = c.oid \
                   AND i.indisprimary \
                   AND i.indisvalid AND i.indisready \
                   AND i.indnkeyatts = 1 \
                   AND i.indpred IS NULL \
                   AND i.indexprs IS NULL \
                   AND i.indkey[0] = a.attnum \
            ) AS is_primary \
       FROM pg_attribute a \
       JOIN pg_class c ON c.oid = a.attrelid \
       JOIN pg_namespace n ON n.oid = c.relnamespace \
      WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3 \
        AND a.attnum > 0 AND NOT a.attisdropped";

const ENABLED_USER_TRIGGER_SQL: &str = "WITH RECURSIVE target(relid) AS ( \
            SELECT c.oid \
              FROM pg_catalog.pg_class c \
              JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relname = $2 \
               AND c.relkind IN ('r', 'p') \
            UNION ALL \
            SELECT i.inhrelid \
              FROM pg_catalog.pg_inherits i \
              JOIN target parent ON parent.relid = i.inhparent \
        ) \
        SELECT EXISTS ( \
            SELECT 1 \
              FROM target \
              JOIN pg_catalog.pg_trigger t ON t.tgrelid = target.relid \
             WHERE NOT t.tgisinternal AND t.tgenabled <> 'D' \
               AND ($3::text IS NULL OR t.tgname <> $3) \
        ) AS has_enabled_user_trigger";

async fn validate_cursor<D: SqlSession>(conn: &D, spec: &BackfillSpec) -> Result<(), ApplyError> {
    let rows = conn
        .query(
            CURSOR_SAFETY_SQL,
            &[
                Bind::Text(spec.schema.clone()),
                Bind::Text(spec.table.clone()),
                Bind::Text(spec.cursor_column.clone()),
            ],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(backend_error(format!(
            "{}.{} column {} was not found",
            spec.schema, spec.table, spec.cursor_column
        )));
    };
    let not_null: bool = row.try_get("not_null")?;
    let primary: bool = row.try_get("is_primary")?;
    if !primary || !not_null {
        let reason = match (primary, not_null) {
            (false, _) => "it is not the complete single-column primary key",
            (_, false) => "it is nullable",
            _ => unreachable!(),
        };
        return Err(backend_error(format!(
            "cursor column {:?} on {:?} is unsafe: {reason}",
            spec.cursor_column, spec.table
        )));
    }
    Ok(())
}

async fn reject_enabled_user_triggers<D: SqlSession>(
    conn: &D,
    spec: &BackfillSpec,
    allowed_engine_trigger: Option<&str>,
) -> Result<(), ApplyError> {
    let rows = conn
        .query(
            ENABLED_USER_TRIGGER_SQL,
            &[
                Bind::Text(spec.schema.clone()),
                Bind::Text(spec.table.clone()),
                allowed_engine_trigger.map_or(Bind::Null, |name| Bind::Text(name.to_string())),
            ],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(backend_error(format!(
            "could not inspect triggers on {}.{}",
            spec.schema, spec.table
        )));
    };
    let has_enabled_user_trigger: bool = row.try_get("has_enabled_user_trigger")?;
    if has_enabled_user_trigger {
        return Err(backend_error(format!(
            "backfill target {}.{} or one of its inherited partitions has an enabled user trigger",
            spec.schema, spec.table
        )));
    }
    Ok(())
}

async fn validate_target_under_lock<D: SqlSession>(
    conn: &D,
    spec: &BackfillSpec,
    expected_cursor_type: &str,
    allowed_engine_trigger: Option<&str>,
) -> Result<(), ApplyError> {
    let target = format!(
        "{}.{}",
        quote_ident(&spec.schema)?,
        quote_ident(&spec.table)?
    );
    conn.batch(&format!("LOCK TABLE {target} IN ROW EXCLUSIVE MODE"))
        .await?;

    let actual_cursor_type = resolve_cursor_type(conn, spec).await?;
    validate_cursor(conn, spec).await?;
    reject_enabled_user_triggers(conn, spec, allowed_engine_trigger).await?;
    if actual_cursor_type != expected_cursor_type {
        return Err(backend_error(format!(
            "cursor column {:?} changed type from {expected_cursor_type:?} to \
             {actual_cursor_type:?} while the backfill was running",
            spec.cursor_column
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct Progress {
    last_cursor: Option<String>,
    end_cursor: Option<String>,
    cohort_initialized: bool,
    complete: bool,
    checksum: Option<String>,
}

async fn read_progress<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
) -> Result<Option<Progress>, ApplyError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "SELECT last_cursor, end_cursor, cohort_initialized, complete, checksum \
                   FROM {meta}.schema_backfills \
                  WHERE backfill_id = $1"
            ),
            &[Bind::Text(backfill_id.to_string())],
        )
        .await?;
    rows.first()
        .map(|row| {
            Ok(Progress {
                last_cursor: row.try_get("last_cursor")?,
                end_cursor: row.try_get("end_cursor")?,
                cohort_initialized: row.try_get("cohort_initialized")?,
                complete: row.try_get("complete")?,
                checksum: row.try_get("checksum")?,
            })
        })
        .transpose()
}

async fn initialize_progress<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    checksum: &Checksum,
    cursor_type: &str,
    spec: &BackfillSpec,
    allowed_engine_trigger: Option<&str>,
    applied_by: &str,
) -> Result<Option<String>, ApplyError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    conn.batch("BEGIN").await?;
    let result = async {
        conn.batch(&batch_session_sql(cfg, spec)?).await?;
        validate_target_under_lock(conn, spec, cursor_type, allowed_engine_trigger).await?;
        if let Some(role) = &cfg.pg.migrator_role {
            conn.batch(&format!("SET LOCAL ROLE {}", quote_ident(role)?))
                .await?;
        }

        // Capture the terminal key before the progress row is committed. If the
        // process dies anywhere in this transaction, neither marker survives;
        // a retry therefore captures a fresh, internally consistent cohort.
        let rows = conn.query(&build_end_cursor_sql(spec)?, &[]).await?;
        let end_cursor = rows
            .first()
            .map(|row| row.try_get::<_, String>("_bf_end_cursor"))
            .transpose()?;

        if cfg.pg.migrator_role.is_some() {
            conn.batch("RESET ROLE").await?;
        }
        let inserted = conn
            .exec(
                &format!(
                    "INSERT INTO {meta}.schema_backfills \
                        (backfill_id, checksum, name, target_schema, target_table, \
                         cursor_column, end_cursor, cohort_initialized, applied_by) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, true, $8)"
                ),
                &[
                    Bind::Text(backfill_id.to_string()),
                    Bind::Text(checksum.as_str().to_string()),
                    Bind::Text(spec.name.clone()),
                    Bind::Text(spec.schema.clone()),
                    Bind::Text(spec.table.clone()),
                    Bind::Text(spec.cursor_column.clone()),
                    end_cursor.clone().into(),
                    Bind::Text(applied_by.to_string()),
                ],
            )
            .await?;
        if inserted != 1 {
            return Err(backend_error(format!(
                "progress initialization affected {inserted} rows for {backfill_id:?}"
            )));
        }
        Ok::<Option<String>, ApplyError>(end_cursor)
    }
    .await;
    match result {
        Ok(end_cursor) => {
            if let Err(error) = conn.batch("COMMIT").await {
                rollback(conn).await;
                return Err(error.into());
            }
            Ok(end_cursor)
        }
        Err(error) => {
            rollback(conn).await;
            Err(error)
        }
    }
}

async fn finish_backfill<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
    applied_by: &str,
) -> Result<(), ApplyError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    conn.batch("BEGIN").await?;
    let result = async {
        let completed = conn
            .exec(
                &format!(
                    "UPDATE {meta}.schema_backfills \
                        SET complete = true, updated_at = now() \
                      WHERE backfill_id = $1 AND checksum = $2 \
                        AND target_schema = $3 AND target_table = $4 \
                        AND cursor_column = $5"
                ),
                &[
                    Bind::Text(version.as_str().to_string()),
                    Bind::Text(checksum.as_str().to_string()),
                    Bind::Text(spec.schema.clone()),
                    Bind::Text(spec.table.clone()),
                    Bind::Text(spec.cursor_column.clone()),
                ],
            )
            .await?;
        if completed != 1 {
            return Err(backend_error(format!(
                "completion update affected {completed} rows for {:?}",
                version.as_str()
            )));
        }
        conn.exec(
            &format!(
                "INSERT INTO {meta}.schema_migrations \
                    (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind) \
                 VALUES ('{applied}', $1, $2, $3, $4, 0, 'completed', 'success', 'apply')",
                applied = journal::EventKind::Applied.as_str()
            ),
            &[
                Bind::Text(version.as_str().to_string()),
                Bind::Text(spec.name.clone()),
                Bind::Text(checksum.as_str().to_string()),
                Bind::Text(applied_by.to_string()),
            ],
        )
        .await?;
        Ok::<(), ApplyError>(())
    }
    .await;
    match result {
        Ok(()) => conn.batch("COMMIT").await.map_err(Into::into),
        Err(error) => {
            rollback(conn).await;
            Err(error)
        }
    }
}

async fn rollback<D: SqlSession>(conn: &D) {
    if let Err(error) = conn.batch("ROLLBACK").await {
        tracing::warn!(error = %error, "zero-migrate: PostgreSQL backfill rollback failed");
    }
}

async fn lock_and_validate_progress<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
    backfill_id: &str,
    checksum: &Checksum,
    expected_last_cursor: Option<&str>,
    expected_end_cursor: &str,
) -> Result<(), ApplyError> {
    let meta = quote_ident(&cfg.pg.meta_schema)?;
    let rows = conn
        .query(
            &format!(
                "SELECT checksum, target_schema, target_table, cursor_column, \
                        last_cursor, end_cursor, cohort_initialized, complete \
                   FROM {meta}.schema_backfills \
                  WHERE backfill_id = $1 FOR UPDATE"
            ),
            &[Bind::Text(backfill_id.to_string())],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(backend_error(format!(
            "progress row disappeared for {backfill_id:?}"
        )));
    };

    let recorded_checksum: Option<String> = row.try_get("checksum")?;
    let recorded_checksum = recorded_checksum.as_deref().unwrap_or("<missing>");
    if recorded_checksum != checksum.as_str() {
        return Err(ApplyError::ChecksumDrift {
            version: backfill_id.to_string(),
            recorded: recorded_checksum.to_string(),
            expected: checksum.as_str().to_string(),
        });
    }

    let target_schema: String = row.try_get("target_schema")?;
    let target_table: String = row.try_get("target_table")?;
    let cursor_column: String = row.try_get("cursor_column")?;
    if target_schema != spec.schema
        || target_table != spec.table
        || cursor_column != spec.cursor_column
    {
        return Err(backend_error(format!(
            "progress target changed for {backfill_id:?}: recorded \
             {target_schema}.{target_table} cursor {cursor_column:?}, expected \
             {}.{} cursor {:?}",
            spec.schema, spec.table, spec.cursor_column
        )));
    }

    let cohort_initialized: bool = row.try_get("cohort_initialized")?;
    let complete: bool = row.try_get("complete")?;
    if !cohort_initialized || complete {
        return Err(backend_error(format!(
            "progress state changed for {backfill_id:?}: \
             cohort_initialized={cohort_initialized}, complete={complete}"
        )));
    }

    let last_cursor: Option<String> = row.try_get("last_cursor")?;
    let end_cursor: Option<String> = row.try_get("end_cursor")?;
    if last_cursor.as_deref() != expected_last_cursor
        || end_cursor.as_deref() != Some(expected_end_cursor)
    {
        return Err(backend_error(format!(
            "progress cursor changed for {backfill_id:?}: recorded last={last_cursor:?}, \
             end={end_cursor:?}; expected last={expected_last_cursor:?}, \
             end={expected_end_cursor:?}"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_batch<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
    cursor_type: &str,
    last_cursor: Option<&str>,
    end_cursor: &str,
    backfill_id: &str,
    checksum: &Checksum,
    allowed_engine_trigger: Option<&str>,
) -> Result<(u64, u64, Option<String>), ApplyError> {
    conn.batch("BEGIN").await?;

    let result = async {
        conn.batch(&batch_session_sql(cfg, spec)?).await?;
        lock_and_validate_progress(
            conn,
            cfg,
            spec,
            backfill_id,
            checksum,
            last_cursor,
            end_cursor,
        )
        .await?;
        validate_target_under_lock(conn, spec, cursor_type, allowed_engine_trigger).await?;
        if let Some(role) = &cfg.pg.migrator_role {
            conn.batch(&format!("SET LOCAL ROLE {}", quote_ident(role)?))
                .await?;
        }

        let sql = build_batch_sql(spec, cursor_type, last_cursor.is_some())?;
        let binds = batch_binds(last_cursor, end_cursor);
        let row = conn.query_one(&sql, &binds).await.map_err(|error| {
            backend_error(format!(
                "batch failed after cursor {last_cursor:?}: {error}"
            ))
        })?;

        if cfg.pg.migrator_role.is_some() {
            conn.batch("RESET ROLE").await?;
        }

        let selected_i64: i64 = row.try_get("_bf_selected")?;
        let selected = u64::try_from(selected_i64).map_err(|_| {
            backend_error(format!(
                "database returned invalid selected-row count {selected_i64}"
            ))
        })?;
        let rows_i64: i64 = row.try_get("_bf_rows")?;
        let rows = u64::try_from(rows_i64).map_err(|_| {
            backend_error(format!("database returned invalid row count {rows_i64}"))
        })?;
        let cursor: Option<String> = row.try_get("_bf_cursor")?;

        if rows != selected {
            return Err(backend_error(format!(
                "batch selected {selected} rows but updated {rows}; a BEFORE UPDATE \
                 trigger or asymmetric row-level security policy suppressed target rows"
            )));
        }

        if selected > 0 {
            let next_cursor = cursor
                .as_ref()
                .ok_or_else(|| backend_error("non-empty window returned no cursor"))?;
            let meta = quote_ident(&cfg.pg.meta_schema)?;
            let advanced = conn
                .exec(
                    &format!(
                        "UPDATE {meta}.schema_backfills \
                        SET last_cursor = $3, rows_done = rows_done + $4, \
                            batches_done = batches_done + 1, updated_at = now() \
                      WHERE backfill_id = $1 AND checksum = $2 AND complete = false"
                    ),
                    &[
                        Bind::Text(backfill_id.to_string()),
                        Bind::Text(checksum.as_str().to_string()),
                        Bind::Text(next_cursor.clone()),
                        Bind::Int(i64::try_from(rows).map_err(|_| {
                            backend_error(format!("row count {rows} cannot be checkpointed"))
                        })?),
                    ],
                )
                .await?;
            if advanced != 1 {
                return Err(backend_error(format!(
                    "progress update affected {advanced} rows for {backfill_id:?}"
                )));
            }
        }

        Ok::<(u64, u64, Option<String>), ApplyError>((selected, rows, cursor))
    }
    .await;

    match result {
        Ok(outcome) => {
            if let Err(error) = conn.batch("COMMIT").await {
                rollback(conn).await;
                return Err(error.into());
            }
            Ok(outcome)
        }
        Err(error) => {
            rollback(conn).await;
            Err(error)
        }
    }
}

pub(super) async fn run_backfill<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &MigrationId,
    checksum: &Checksum,
    spec: &BackfillSpec,
    approval: Approval,
    allowed_engine_trigger: Option<&str>,
    applied_by: &str,
) -> Result<BackfillOutcome, ApplyError> {
    if approval != Approval::Approved {
        return Err(ApplyError::ApprovalRequired);
    }
    validate_ident("table", &spec.table)?;
    validate_ident("cursor column", &spec.cursor_column)?;
    if spec.batch_size == 0 {
        return Err(backend_error("batch size must be greater than zero"));
    }

    ensure_progress(conn, cfg).await?;
    let backfill_id = version.as_str().to_string();
    let progress = read_progress(conn, cfg, &backfill_id).await?;
    if let Some(progress) = progress.as_ref() {
        let recorded = progress.checksum.as_deref().unwrap_or("<missing>");
        if recorded != checksum.as_str() {
            return Err(ApplyError::ChecksumDrift {
                version: version.as_str().to_string(),
                recorded: recorded.to_string(),
                expected: checksum.as_str().to_string(),
            });
        }
    }
    let resumed = progress
        .as_ref()
        .is_some_and(|value| value.last_cursor.is_some());
    if progress.as_ref().is_some_and(|value| value.complete) {
        // A previous process may have committed the progress marker before the
        // ordinary journal event. Repair that narrow state idempotently.
        finish_backfill(conn, cfg, version, checksum, spec, applied_by).await?;
        return Ok(BackfillOutcome {
            backfill_id,
            batches: 0,
            rows_updated: 0,
            resumed,
            complete: true,
        });
    }
    if progress
        .as_ref()
        .is_some_and(|value| !value.cohort_initialized)
    {
        return Err(backend_error(format!(
            "incomplete legacy progress for {backfill_id:?} has no terminal cohort boundary; \
             refusing an unsafe resume"
        )));
    }
    if progress.as_ref().is_some_and(|value| {
        value.cohort_initialized && value.end_cursor.is_none() && value.last_cursor.is_some()
    }) {
        return Err(backend_error(format!(
            "progress for {backfill_id:?} records a cursor for an empty cohort"
        )));
    }

    let cursor_type = resolve_cursor_type(conn, spec).await?;
    validate_cursor(conn, spec).await?;

    let guard = SqlGuard::new(cfg.guard_config());
    guard
        .check(&build_end_cursor_sql(spec)?)
        .map_err(|error| backend_error(format!("assembled SQL was denied: {error}")))?;
    for have_cursor in [false, true] {
        let sql = build_batch_sql(spec, &cursor_type, have_cursor)?;
        guard
            .check(&sql)
            .map_err(|error| backend_error(format!("assembled SQL was denied: {error}")))?;
    }
    assert_cursor_not_mutated(
        &build_batch_sql(spec, &cursor_type, true)?,
        &spec.cursor_column,
    )?;

    let (mut cursor, end_cursor) = match progress {
        Some(progress) => (progress.last_cursor, progress.end_cursor),
        None => (
            None,
            initialize_progress(
                conn,
                cfg,
                &backfill_id,
                checksum,
                &cursor_type,
                spec,
                allowed_engine_trigger,
                applied_by,
            )
            .await?,
        ),
    };
    let mut batches = 0_u64;
    let mut rows_updated = 0_u64;

    if let Some(end_cursor) = end_cursor.as_deref() {
        loop {
            let (selected, rows, next_cursor) = run_batch(
                conn,
                cfg,
                spec,
                &cursor_type,
                cursor.as_deref(),
                end_cursor,
                &backfill_id,
                checksum,
                allowed_engine_trigger,
            )
            .await?;
            if selected == 0 {
                break;
            }
            batches += 1;
            rows_updated = rows_updated.saturating_add(rows);
            cursor = next_cursor;
            if window_reached_tail(selected, spec.batch_size) {
                break;
            }
        }
    }

    finish_backfill(conn, cfg, version, checksum, spec, applied_by).await?;
    Ok(BackfillOutcome {
        backfill_id,
        batches,
        rows_updated,
        resumed,
        complete: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{DbError, Row};
    use std::cell::RefCell;

    struct RecordingSession {
        log: RefCell<Vec<String>>,
        selected: i64,
        updated: i64,
        cursor: Option<String>,
        checkpoint_rows: u64,
    }

    impl RecordingSession {
        fn batch_result(selected: i64, updated: i64, cursor: Option<&str>) -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                selected,
                updated,
                cursor: cursor.map(str::to_string),
                checkpoint_rows: 1,
            }
        }

        fn with_checkpoint_rows(mut self, rows: u64) -> Self {
            self.checkpoint_rows = rows;
            self
        }
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.log.borrow_mut().push(format!("batch: {sql}"));
            Ok(())
        }

        async fn exec(&self, sql: &str, _binds: &[Bind]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec: {sql}"));
            Ok(self.checkpoint_rows)
        }

        async fn exec_text(&self, sql: &str, _params: &[Option<String>]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec_text: {sql}"));
            Ok(0)
        }

        async fn query(&self, sql: &str, _binds: &[Bind]) -> Result<Vec<Row>, DbError> {
            self.log.borrow_mut().push(format!("query: {sql}"));
            if sql.contains("format_type(a.atttypid") {
                return Ok(vec![Row::new(
                    vec!["coltype".into()],
                    vec![crate::driver::Value::Text("bigint".into())],
                )]);
            }
            if sql == CURSOR_SAFETY_SQL {
                return Ok(vec![Row::new(
                    vec!["not_null".into(), "is_primary".into()],
                    vec![
                        crate::driver::Value::Bool(true),
                        crate::driver::Value::Bool(true),
                    ],
                )]);
            }
            if sql == ENABLED_USER_TRIGGER_SQL {
                return Ok(vec![Row::new(
                    vec!["has_enabled_user_trigger".into()],
                    vec![crate::driver::Value::Bool(false)],
                )]);
            }
            if sql.contains("schema_backfills") && sql.contains("FOR UPDATE") {
                return Ok(vec![Row::new(
                    vec![
                        "checksum".into(),
                        "target_schema".into(),
                        "target_table".into(),
                        "cursor_column".into(),
                        "last_cursor".into(),
                        "end_cursor".into(),
                        "cohort_initialized".into(),
                        "complete".into(),
                    ],
                    vec![
                        crate::driver::Value::Text(test_checksum().as_str().to_string()),
                        crate::driver::Value::Text("app".into()),
                        crate::driver::Value::Text("users".into()),
                        crate::driver::Value::Text("id".into()),
                        crate::driver::Value::Null,
                        self.cursor
                            .clone()
                            .map_or(crate::driver::Value::Null, crate::driver::Value::Text),
                        crate::driver::Value::Bool(true),
                        crate::driver::Value::Bool(false),
                    ],
                )]);
            }
            Ok(Vec::new())
        }

        async fn query_one(&self, sql: &str, _binds: &[Bind]) -> Result<Row, DbError> {
            self.log.borrow_mut().push(format!("query_one: {sql}"));
            if sql.contains("_bf_selected") {
                return Ok(Row::new(
                    vec![
                        "_bf_selected".into(),
                        "_bf_rows".into(),
                        "_bf_cursor".into(),
                    ],
                    vec![
                        crate::driver::Value::Int(self.selected),
                        crate::driver::Value::Int(self.updated),
                        self.cursor
                            .clone()
                            .map_or(crate::driver::Value::Null, crate::driver::Value::Text),
                    ],
                ));
            }
            Err(DbError::message("no canned row"))
        }
    }

    fn spec() -> BackfillSpec {
        BackfillSpec {
            schema: "app".into(),
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: 250,
            set_clause: "\"display_name\" = \"name\"".into(),
            filter: Some("\"display_name\" IS NULL".into()),
            name: "fill_display_name".into(),
        }
    }

    fn test_checksum() -> Checksum {
        Checksum::of(&crate::model::migration::ChecksumInput {
            up: "postgres backfill test",
            down: None,
            flags: &crate::model::migration::MigrationFlags::default(),
            owner_app: "app",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        })
    }

    #[test]
    fn batch_window_is_bounded_and_schema_qualified() {
        let sql = build_batch_sql(&spec(), "bigint", true).unwrap();
        assert!(sql.contains("FROM \"app\".\"users\""));
        assert!(sql.contains("\"id\" > ($1::text)::bigint"));
        assert!(sql.contains("\"id\" <= ($2::text)::bigint"));
        assert!(sql.contains("LIMIT 250"));
        assert!(sql.contains("AND (\"display_name\" IS NULL)"));
        assert!(sql.contains("count(*) FROM _bf_window) AS _bf_selected"));
        assert!(sql.contains("ORDER BY _bf_key DESC LIMIT 1"));
    }

    #[test]
    fn first_window_uses_the_terminal_cursor_as_its_first_bind() {
        let sql = build_batch_sql(&spec(), "uuid", false).unwrap();
        assert!(sql.contains("WHERE TRUE"));
        assert!(sql.contains("\"id\" <= ($1::text)::uuid"));
        assert!(!sql.contains("$2"));
        assert_eq!(batch_binds(None, "99"), [Bind::Text("99".into())]);
        assert_eq!(
            batch_binds(Some("42"), "99"),
            [Bind::Text("42".into()), Bind::Text("99".into())]
        );
    }

    #[test]
    fn terminal_cursor_query_uses_native_order_and_the_authored_filter() {
        let sql = build_end_cursor_sql(&spec()).unwrap();
        assert!(sql.contains("SELECT \"id\"::text AS _bf_end_cursor"));
        assert!(sql.contains("AND (\"display_name\" IS NULL)"));
        assert!(sql.contains("ORDER BY \"id\" DESC LIMIT 1"));
    }

    #[test]
    fn completion_uses_selected_window_size_not_updated_row_count() {
        assert!(!window_reached_tail(250, 250));
        assert!(window_reached_tail(249, 250));
    }

    #[test]
    fn assigning_the_cursor_is_rejected() {
        let mut value = spec();
        value.set_clause = "\"id\" = \"id\" + 1".into();
        let sql = build_batch_sql(&value, "bigint", true).unwrap();
        let error = assert_cursor_not_mutated(&sql, "id").unwrap_err();
        assert!(error.to_string().contains("assigns cursor column"));
    }

    #[test]
    fn unsafe_cursor_types_fail_closed() {
        for ty in ["real", "double precision", "jsonb", "bytea", "point"] {
            assert!(!cursor_type_is_orderable(ty), "{ty} must be rejected");
        }
        for ty in [
            "bigint",
            "numeric(20,4)",
            "character varying(64)",
            "timestamp with time zone",
            "uuid",
        ] {
            assert!(cursor_type_is_orderable(ty), "{ty} must be supported");
        }
    }

    #[test]
    fn cursor_primary_key_ignores_invalid_or_unready_indexes() {
        assert!(CURSOR_SAFETY_SQL.contains("i.indisvalid AND i.indisready"));
    }

    #[test]
    fn cursor_requires_the_complete_single_column_primary_key() {
        assert!(CURSOR_SAFETY_SQL.contains("i.indisprimary"));
        assert!(!CURSOR_SAFETY_SQL.contains("i.indisunique"));
        assert!(CURSOR_SAFETY_SQL.contains("i.indnkeyatts = 1"));
        assert!(CURSOR_SAFETY_SQL.contains("i.indkey[0] = a.attnum"));
    }

    #[test]
    fn cursor_text_format_is_stable_across_resume_sessions() {
        assert!(CURSOR_SESSION_SETTINGS.contains("DateStyle TO 'ISO, YMD'"));
        assert!(CURSOR_SESSION_SETTINGS.contains("TimeZone TO 'UTC'"));
    }

    #[test]
    fn trigger_check_covers_inherited_and_partitioned_targets() {
        assert!(ENABLED_USER_TRIGGER_SQL.contains("WITH RECURSIVE target"));
        assert!(ENABLED_USER_TRIGGER_SQL.contains("pg_catalog.pg_inherits"));
        assert!(ENABLED_USER_TRIGGER_SQL.contains("NOT t.tgisinternal"));
        assert!(ENABLED_USER_TRIGGER_SQL.contains("t.tgenabled <> 'D'"));
    }

    #[test]
    fn backfill_pins_standard_string_literals_transaction_locally() {
        let cfg = ExecutorConfig::new("prj_x", "app");
        let session = batch_session_sql(&cfg, &spec()).expect("backfill session SQL renders");

        assert!(
            session.starts_with(AUTHOR_SQL_LITERAL_MODE),
            "the literal mode must be pinned before the authored transform: {session}"
        );
        assert!(
            session.contains("SET LOCAL standard_conforming_strings = on;"),
            "the setting must be transaction-local so the inherited value is restored: {session}"
        );
    }

    #[compio::test]
    async fn suppressed_rows_roll_back_without_advancing_progress() {
        let conn = RecordingSession::batch_result(3, 2, Some("3"));
        let cfg = ExecutorConfig::new("prj_x", "app");
        let checksum = test_checksum();

        let error = run_batch(
            &conn,
            &cfg,
            &spec(),
            "bigint",
            None,
            "3",
            "bf_1",
            &checksum,
            None,
        )
        .await
        .expect_err("a short UPDATE result must fail closed");

        assert!(
            error.to_string().contains("selected 3 rows but updated 2"),
            "the error should identify trigger/RLS suppression: {error}"
        );
        let log = conn.log.borrow();
        assert!(log.iter().any(|entry| entry == "batch: ROLLBACK"));
        assert!(!log.iter().any(|entry| entry.starts_with("exec: UPDATE")));
        assert!(!log.iter().any(|entry| entry == "batch: COMMIT"));
    }

    #[compio::test]
    async fn missing_checkpoint_row_rolls_back_the_data_batch() {
        let conn = RecordingSession::batch_result(2, 2, Some("2")).with_checkpoint_rows(0);
        let cfg = ExecutorConfig::new("prj_x", "app");
        let checksum = test_checksum();

        let error = run_batch(
            &conn,
            &cfg,
            &spec(),
            "bigint",
            None,
            "2",
            "bf_1",
            &checksum,
            None,
        )
        .await
        .expect_err("a missing progress row must roll back the data update");

        assert!(
            error
                .to_string()
                .contains("progress update affected 0 rows"),
            "the error should identify the lost checkpoint: {error}"
        );
        let log = conn.log.borrow();
        assert!(log.iter().any(|entry| entry == "batch: ROLLBACK"));
        assert!(!log.iter().any(|entry| entry == "batch: COMMIT"));
    }

    #[compio::test]
    async fn batch_locks_and_revalidates_target_and_progress_before_advancing() {
        let conn = RecordingSession::batch_result(2, 2, Some("2"));
        let cfg = ExecutorConfig::new("prj_x", "app");
        let checksum = test_checksum();

        run_batch(
            &conn,
            &cfg,
            &spec(),
            "bigint",
            None,
            "2",
            "bf_1",
            &checksum,
            None,
        )
        .await
        .expect("safe batch applies");

        let log = conn.log.borrow();
        assert!(
            log.iter().any(|entry| {
                entry.contains("LOCK TABLE \"app\".\"users\" IN ROW EXCLUSIVE MODE")
            }),
            "the target must be locked before its cursor invariants are rechecked: {log:?}"
        );
        assert!(
            log.iter().any(|entry| entry.contains(CURSOR_SAFETY_SQL)),
            "cursor safety must be rechecked in the batch transaction: {log:?}"
        );
        assert!(
            log.iter().any(|entry| {
                entry.contains("schema_backfills") && entry.contains("FOR UPDATE")
            }),
            "the progress row must be locked and revalidated: {log:?}"
        );
        assert!(
            log.iter().any(|entry| {
                entry.starts_with("exec: UPDATE") && entry.contains("AND checksum = $2")
            }),
            "the checkpoint must be checksum-bound: {log:?}"
        );
    }
}
