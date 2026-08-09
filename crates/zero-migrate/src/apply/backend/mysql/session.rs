//! MySQL dialect SQL leaves for [`MysqlBackend`](super::MysqlBackend).
//!
//! These are the MySQL-specific lock / session / apply / rollback operations the
//! [`MysqlBackend`](super::MysqlBackend)
//! [`MigrationBackend`](crate::apply::backend::MigrationBackend) impl drives — the
//! MySQL analogue of the Postgres
//! [`session`](crate::apply::backend::postgres::session) leaves. Every one of them
//! is MySQL-flavoured, so it lives in the MySQL backend, never the shared executor:
//!
//! - **project lock** — `GET_LOCK(name, timeout)` / `RELEASE_LOCK(name)`, MySQL's
//! named advisory lock, replaces `pg_advisory_lock(hashtext($1))`. The lock name
//! is derived from the project id (bounded to MySQL's 64-char lock-name limit).
//! A read-only caller takes the same lock with a zero timeout instead, so it
//! never spends any of a peer deploy's wall clock, and names the holder from
//! `performance_schema` when the lock is taken.
//! - **session setup** — `SET SESSION max_execution_time` +
//! `innodb_lock_wait_timeout` replaces the `SET [LOCAL] search_path` +
//! `statement_timeout` / `lock_timeout` GUCs (MySQL has no per-connection schema
//! search-path — a migration references its objects by explicit database, or the
//! connection's default database is the project database). No `SET ROLE`: the
//! least-privilege migrator-role confinement is a Postgres construct; on MySQL
//! the connecting user's grants ARE the confinement.
//! - **apply** — MySQL DDL is **auto-committing** (an implicit COMMIT brackets
//! every DDL statement), so a migration's `up` cannot be wrapped with its journal
//! row in one transaction. Every MySQL migration therefore takes the **two-phase
//! non-transactional path**: a `started` marker → run the `up` → an immutable
//! `completed` row + clear the marker. Because generated MySQL DDL is not safely
//! replayable after an ambiguous crash, an unmatched marker is preserved and
//! recovery fails closed for operator inspection instead of re-running `up`.
//! - **rollback** — the `down` runs, then a `rolled_back` event is appended. MySQL
//! DDL auto-commits, so the `down` + its journal append are NOT atomic (same
//! two-phase reality as apply); the append is best-effort-ordered after the
//! `down` succeeds.
//!
//! Placeholders are the anonymous positional `?`
//! ([`PlaceholderStyle::Question`](crate::apply::backend::PlaceholderStyle::Question)),
//! rendered here directly.

use std::time::Instant;

use crate::apply::backend::mysql::journal_sql;
use crate::apply::backend::ProjectLockHolder;
use crate::apply::executor::{ApplyError, RollbackError};
use crate::apply::journal::{self, CompletedRecord};
use crate::apply::timeout::{resolve_timeout_ms, IndefiniteTimeoutError as TimeoutError};
use crate::conn::ExecutorConfig;
use crate::driver::{Bind, SqlSession};
use crate::model::migration::{Checksum, Migration};
use crate::render::step::BindValue;

use super::MysqlSessionSnapshot;

/// How long `GET_LOCK` waits (seconds) for the project apply lock before the
/// acquire is treated as contended. Mirrors the "serialize concurrent deploys"
/// intent of the PG advisory lock — a second apply waits here, then sees the
/// first's committed journal and no-ops.
const PROJECT_LOCK_TIMEOUT_SECS: i64 = 10;

/// Live MySQL settings required to safely use a nondeterministic UUIDv4
/// expression default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DatabaseCapabilities {
    pub(crate) server_version: String,
    pub(crate) default_storage_engine: String,
    pub(crate) innodb_support: Option<String>,
    pub(crate) global_binlog_format: String,
    pub(crate) session_binlog_format: String,
}

/// Read the server/version, storage-engine, and replication settings that form
/// the MySQL UUIDv4 database-generation capability contract.
pub(crate) async fn database_capabilities<D: SqlSession>(
    conn: &D,
) -> Result<DatabaseCapabilities, ApplyError> {
    let row = conn
        .query_one(
            "SELECT VERSION() AS server_version,
                    @@SESSION.default_storage_engine AS default_storage_engine,
                    (SELECT SUPPORT
                       FROM information_schema.ENGINES
                      WHERE ENGINE = 'InnoDB') AS innodb_support,
                    @@GLOBAL.binlog_format AS global_binlog_format,
                    @@SESSION.binlog_format AS session_binlog_format",
            &[],
        )
        .await?;
    Ok(DatabaseCapabilities {
        server_version: row.try_get("server_version")?,
        default_storage_engine: row.try_get("default_storage_engine")?,
        innodb_support: row.try_get("innodb_support")?,
        global_binlog_format: row.try_get("global_binlog_format")?,
        session_binlog_format: row.try_get("session_binlog_format")?,
    })
}

/// Derive the MySQL named-lock string for a project id.
///
/// MySQL lock names are capped at 64 characters (since 5.7.5); a project id can be
/// longer, so we namespace-prefix and, if the result would exceed the cap, fold
/// the id to a stable 64-char form. The lock name is passed as a **bind** (never
/// interpolated), so this is purely about staying within MySQL's own limit — not a
/// quoting concern.
#[must_use]
pub(crate) fn project_lock_name(project_id: &str) -> String {
    const PREFIX: &str = "zero_migrate:";
    let raw = format!("{PREFIX}{project_id}");
    if raw.len() <= 64 {
        return raw;
    }
    // Fold to a deterministic, collision-resistant 64-char name: prefix + a
    // hex SHA-256 of the full id (64 hex chars is exactly the cap when the prefix
    // is dropped for the overflow case). Liveness-only if two ids ever collided
    // (they serialize against each other) — never a correctness defect, exactly
    // like the PG `hashtext` 32-bit-key limitation.
    use sha2::{Digest, Sha256};
    // Keep the historical overflow derivation stable so mixed-version deploys
    // still contend on the same project lock.
    let digest = Sha256::digest(project_id.as_bytes());
    hex::encode(digest) // 64 hex chars
}

/// A separate lock namespace for journal bootstrap. Status and apply both call
/// `ensure_journal`; serializing its metadata probes and conditional DDL prevents
/// two sessions from racing the same upgrade or trigger creation.
#[must_use]
pub(crate) fn journal_bootstrap_lock_name(project_id: &str) -> String {
    const PREFIX: &str = "zero_migrate_bootstrap:";
    let raw = format!("{PREFIX}{project_id}");
    if raw.len() <= 64 {
        return raw;
    }
    use sha2::{Digest, Sha256};
    // Hash the namespace too so an overflow bootstrap lock cannot collide with
    // the historical project-lock derivation for the same project id.
    hex::encode(Sha256::digest(raw.as_bytes()))
}

pub(crate) async fn ensure_idle_for_journal<D: SqlSession>(
    conn: &D,
) -> Result<(), journal::JournalError> {
    let row = conn
        .query_one(
            "SELECT
                EXISTS(
                    SELECT 1 FROM performance_schema.setup_consumers
                     WHERE NAME = 'events_transactions_current' AND ENABLED = 'YES'
                ) AND EXISTS(
                    SELECT 1 FROM performance_schema.setup_instruments
                     WHERE NAME = 'transaction' AND ENABLED = 'YES'
                ) AS transaction_tracking_enabled,
                EXISTS(
                    SELECT 1 FROM performance_schema.events_transactions_current
                     WHERE THREAD_ID = PS_CURRENT_THREAD_ID() AND STATE = 'ACTIVE'
                ) AS in_transaction",
            &[],
        )
        .await
        .map_err(|error| {
            journal::JournalError::Backend(format!(
                "mysql journal bootstrap cannot verify the dedicated session is idle; grant the migration account SELECT on Performance Schema transaction tables: {error}"
            ))
        })?;
    let tracking_enabled: i64 = row.try_get("transaction_tracking_enabled")?;
    if tracking_enabled != 1 {
        return Err(journal::JournalError::Backend(
            "mysql journal bootstrap cannot verify that the session is idle because Performance Schema transaction tracking is disabled; enable the transaction instrument and events_transactions_current consumer"
                .to_string(),
        ));
    }
    let in_transaction: i64 = row.try_get("in_transaction")?;
    if in_transaction != 0 {
        return Err(journal::JournalError::Backend(
            "mysql journal bootstrap requires a dedicated idle session; the supplied connection has an active transaction and MySQL DDL would implicitly commit caller-owned work"
                .to_string(),
        ));
    }
    Ok(())
}

pub(crate) async fn acquire_journal_bootstrap_lock<D: SqlSession>(
    conn: &D,
    project_id: &str,
) -> Result<(), journal::JournalError> {
    let name = journal_bootstrap_lock_name(project_id);
    let row = conn
        .query_one(
            "SELECT GET_LOCK(?, ?) AS got",
            &[name.as_str().into(), PROJECT_LOCK_TIMEOUT_SECS.into()],
        )
        .await?;
    let got: Option<i64> = row.try_get("got").map_err(|error| {
        journal::JournalError::Backend(format!(
            "mysql journal bootstrap GET_LOCK returned an undecodable result: {error}"
        ))
    })?;
    match got {
        Some(1) => Ok(()),
        Some(0) => Err(journal::JournalError::Backend(format!(
            "mysql journal bootstrap GET_LOCK('{name}') timed out after {PROJECT_LOCK_TIMEOUT_SECS}s"
        ))),
        _ => Err(journal::JournalError::Backend(format!(
            "mysql journal bootstrap GET_LOCK('{name}') returned NULL (lock error)"
        ))),
    }
}

pub(crate) async fn release_journal_bootstrap_lock<D: SqlSession>(
    conn: &D,
    project_id: &str,
) -> Result<(), journal::JournalError> {
    let name = journal_bootstrap_lock_name(project_id);
    conn.exec("DO RELEASE_LOCK(?)", &[name.as_str().into()])
        .await?;
    Ok(())
}

/// Acquire the project apply-serialization lock via MySQL `GET_LOCK` (the analogue
/// of `pg_advisory_lock`). The lock name is bound, not interpolated.
///
/// # Errors
/// [`ApplyError::Db`] on a driver failure; [`ApplyError::Backend`] if `GET_LOCK`
/// returns a non-1 result (0 = timeout, NULL = error/killed) — surfaced rather
/// than silently proceeding without the lock.
pub(crate) async fn acquire_project_lock<D: SqlSession>(
    conn: &D,
    project_id: &str,
) -> Result<(), ApplyError> {
    let name = project_lock_name(project_id);
    let row = conn
        .query_one(
            "SELECT GET_LOCK(?, ?) AS got",
            &[name.as_str().into(), PROJECT_LOCK_TIMEOUT_SECS.into()],
        )
        .await?;
    // GET_LOCK returns 1 (acquired), 0 (timeout), or NULL (error). mysql2 surfaces
    // the 1/0 as an integer cell.
    let got: Option<i64> = row.try_get("got").map_err(|e| {
        ApplyError::Backend(format!(
            "mysql GET_LOCK returned an undecodable result: {e}"
        ))
    })?;
    match got {
        Some(1) => Ok(()),
        Some(0) => Err(ApplyError::Backend(format!(
            "mysql GET_LOCK('{name}') timed out after {PROJECT_LOCK_TIMEOUT_SECS}s — \
             another apply holds the project lock"
        ))),
        _ => Err(ApplyError::Backend(format!(
            "mysql GET_LOCK('{name}') returned NULL (lock error)"
        ))),
    }
}

/// Take the project lock if it is free right now, without waiting.
///
/// The zero timeout is the whole point: `GET_LOCK(name, 0)` fails immediately
/// instead of spending the ten second budget the blocking acquisition above waits
/// out, so a reader never inherits any part of a peer's deploy. It contends on the
/// identical lock name, so a reader and a deploy still serialize against each
/// other.
///
/// A NULL result stays an error. It means the acquisition itself failed (the
/// session was killed, or the name was rejected), which is not the same fact as
/// "a peer holds it" and must not be reported as contention.
///
/// # Errors
/// [`ApplyError::Db`] on a driver failure; [`ApplyError::Backend`] if `GET_LOCK`
/// returns NULL or a result that cannot be decoded.
pub(crate) async fn try_acquire_project_lock<D: SqlSession>(
    conn: &D,
    project_id: &str,
) -> Result<bool, ApplyError> {
    let name = project_lock_name(project_id);
    let row = conn
        .query_one("SELECT GET_LOCK(?, 0) AS got", &[name.as_str().into()])
        .await?;
    let got: Option<i64> = row.try_get("got").map_err(|e| {
        ApplyError::Backend(format!(
            "mysql GET_LOCK returned an undecodable result: {e}"
        ))
    })?;
    match got {
        Some(1) => Ok(true),
        Some(0) => Ok(false),
        _ => Err(ApplyError::Backend(format!(
            "mysql GET_LOCK('{name}') returned NULL (lock error)"
        ))),
    }
}

/// Report the sessions holding this project's named lock, for an operator message
/// naming who a reader is waiting behind.
///
/// A `GET_LOCK` lock is a USER LEVEL LOCK rather than a table MDL, but the server
/// still publishes it in `performance_schema.metadata_locks` under that
/// `OBJECT_TYPE` with the lock name verbatim in `OBJECT_NAME`, so the probe is
/// scoped to this project's own lock rather than every lock on the server.
/// `OWNER_THREAD_ID` joins to `performance_schema.threads`, which is what turns an
/// internal thread id into the connection id `KILL` takes.
///
/// MySQL has no `application_name`, so the holder is identified by its account.
/// `PROCESSLIST_INFO` is NULL whenever the holder is running no statement -- the
/// common case, since a deploy that took the lock and is waiting on a long DDL
/// shows as `Sleep` between statements -- so the statement is reported as optional
/// and the command carries the fact instead of an empty string pretending to be a
/// query.
///
/// # Errors
/// [`ApplyError::Db`] on a driver failure, including the `SELECT` on
/// `performance_schema` that a least-privilege migrator account is not granted.
pub(crate) async fn project_lock_holders<D: SqlSession>(
    conn: &D,
    project_id: &str,
) -> Result<Vec<ProjectLockHolder>, ApplyError> {
    let name = project_lock_name(project_id);
    let rows = conn
        .query(
            "SELECT CAST(t.PROCESSLIST_ID AS SIGNED) AS pid, \
                    NULLIF(CONCAT_WS('@', t.PROCESSLIST_USER, t.PROCESSLIST_HOST), '') AS account, \
                    NULLIF(CONCAT_WS(': ', t.PROCESSLIST_COMMAND, \
                                     NULLIF(t.PROCESSLIST_STATE, '')), '') AS state, \
                    t.PROCESSLIST_INFO AS stmt \
               FROM performance_schema.metadata_locks l \
               JOIN performance_schema.threads t ON t.THREAD_ID = l.OWNER_THREAD_ID \
              WHERE l.OBJECT_TYPE = 'USER LEVEL LOCK' AND l.LOCK_STATUS = 'GRANTED' \
                AND l.OBJECT_NAME = ? AND t.PROCESSLIST_ID IS NOT NULL \
              ORDER BY t.PROCESSLIST_ID",
            &[name.as_str().into()],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(ProjectLockHolder {
                pid: row.try_get("pid")?,
                application_name: row.try_get("account")?,
                state: row.try_get("state")?,
                query: row.try_get("stmt")?,
            })
        })
        .collect()
}

/// Release the project apply-serialization lock via MySQL `RELEASE_LOCK` (the
/// analogue of `pg_advisory_unlock`). Best-effort on the result value (a release
/// of a lock we hold returns 1; the apply already succeeded/failed by here).
///
/// # Errors
/// [`ApplyError::Db`] on a driver failure.
pub(crate) async fn release_project_lock<D: SqlSession>(
    conn: &D,
    project_id: &str,
) -> Result<(), ApplyError> {
    let name = project_lock_name(project_id);
    conn.exec("DO RELEASE_LOCK(?)", &[name.as_str().into()])
        .await?;
    Ok(())
}

/// The effective `max_execution_time` (ms) for a migration: its per-migration
/// override if set, else the executor-wide default. Mirrors the PG
/// `statement_timeout` render.
///
/// MySQL reads `max_execution_time = 0` as "no limit", exactly as PostgreSQL
/// reads `statement_timeout = 0`, so a zero is refused here too. See
/// [`crate::apply::timeout`].
fn effective_timeout_ms(cfg: &ExecutorConfig, m: &Migration) -> Result<u64, TimeoutError> {
    resolve_timeout_ms(
        m.version.as_str(),
        "max_execution_time",
        m.flags.timeout_ms,
        "timeout_ms",
        cfg.statement_timeout_ms(),
        "pg.statement_timeout",
    )
}

/// The effective `innodb_lock_wait_timeout` (seconds) for a migration: its
/// per-migration lock override (ms → whole seconds, min 1) if set, else the SHORT
/// executor-wide default. Mirrors the PG `lock_timeout` render + the lock-safety
/// envelope (a short lock-acquisition budget separate from the long statement
/// budget).
///
/// The zero refusal runs BEFORE the rounding, so the two dialects answer a
/// `lock_timeout_ms: 0` migration identically. Rounding 500ms up to 1s narrows a
/// finite budget the author already asked for; rounding 0 up to 1s would
/// substitute a budget for an explicit "no limit" the migration checksum
/// describes, which is the clamp this rule exists to avoid.
fn effective_lock_timeout_secs(cfg: &ExecutorConfig, m: &Migration) -> Result<u64, TimeoutError> {
    let ms = resolve_timeout_ms(
        m.version.as_str(),
        "innodb_lock_wait_timeout",
        m.flags.lock_timeout_ms,
        "lock_timeout_ms",
        cfg.lock_timeout_ms(),
        "pg.lock_timeout",
    )?;
    // Round UP to whole seconds (MySQL's unit), floor 1s so a sub-second budget
    // never becomes a 0 = "no wait" that fails every contended DDL.
    Ok(ms.div_ceil(1000).max(1))
}

/// Build the session invariant that every author-controlled MySQL statement
/// executes under. `NO_BACKSLASH_ESCAPES` makes standard quote doubling stable
/// for grammar-only string positions (`ENUM`, `SIGNAL SQLSTATE`, defaults).
/// `NO_AUTO_VALUE_ON_ZERO` makes an explicitly imported legacy zero remain zero
/// instead of silently allocating a different identity.
/// Autocommit and relational integrity checks are pinned because MySQL otherwise
/// inherits them from the caller or server defaults. Every inherited value is
/// snapshotted and restored. `information_schema_stats_expiry=0` keeps the live
/// AUTO_INCREMENT metadata used by identity synchronization uncached.
fn session_settings_sql(statement_timeout_ms: u64, lock_timeout_secs: u64) -> String {
    format!(
        "SET SESSION sql_mode = CONCAT_WS(',', @@SESSION.sql_mode, \
             'NO_BACKSLASH_ESCAPES', 'STRICT_ALL_TABLES', 'ERROR_FOR_DIVISION_BY_ZERO', \
             'NO_AUTO_VALUE_ON_ZERO'), \
         SESSION time_zone = '+00:00', \
         SESSION max_execution_time = {statement_timeout_ms}, \
         SESSION innodb_lock_wait_timeout = {lock_timeout_secs}, \
         SESSION information_schema_stats_expiry = 0, \
         SESSION autocommit = 1, \
         SESSION foreign_key_checks = 1, \
         SESSION unique_checks = 1"
    )
}

async fn read_session_snapshot<D: SqlSession>(
    conn: &D,
) -> Result<MysqlSessionSnapshot, ApplyError> {
    let row = conn
        .query_one(
            "SELECT @@SESSION.sql_mode AS sql_mode, \
                    @@SESSION.time_zone AS time_zone, \
                    @@SESSION.max_execution_time AS max_execution_time, \
                    @@SESSION.innodb_lock_wait_timeout AS innodb_lock_wait_timeout, \
                    @@SESSION.information_schema_stats_expiry AS information_schema_stats_expiry, \
                    @@SESSION.autocommit AS autocommit, \
                    @@SESSION.foreign_key_checks AS foreign_key_checks, \
                    @@SESSION.unique_checks AS unique_checks, \
                    EXISTS( \
                        SELECT 1 FROM performance_schema.setup_consumers \
                         WHERE NAME = 'events_transactions_current' AND ENABLED = 'YES' \
                    ) AND EXISTS( \
                        SELECT 1 FROM performance_schema.setup_instruments \
                         WHERE NAME = 'transaction' AND ENABLED = 'YES' \
                    ) AS transaction_tracking_enabled, \
                    EXISTS( \
                        SELECT 1 FROM performance_schema.events_transactions_current \
                         WHERE THREAD_ID = PS_CURRENT_THREAD_ID() AND STATE = 'ACTIVE' \
                    ) AS in_transaction",
            &[],
        )
        .await
        .map_err(|error| {
            ApplyError::Backend(format!(
                "mysql apply cannot verify the dedicated session is idle; grant the migration account SELECT on Performance Schema transaction tables: {error}"
            ))
        })?;
    let tracking_enabled: i64 = row.try_get("transaction_tracking_enabled")?;
    if tracking_enabled != 1 {
        return Err(ApplyError::Backend(
            "mysql apply cannot verify that the session is idle because Performance Schema transaction tracking is disabled; enable the transaction instrument and events_transactions_current consumer"
                .to_string(),
        ));
    }
    let in_transaction: i64 = row.try_get("in_transaction")?;
    if in_transaction != 0 {
        return Err(ApplyError::Backend(
            "mysql apply requires a dedicated idle session; the supplied connection has an active transaction, and setting autocommit=1 would commit caller-owned work"
                .to_string(),
        ));
    }
    Ok(MysqlSessionSnapshot {
        sql_mode: row.try_get("sql_mode")?,
        time_zone: row.try_get("time_zone")?,
        max_execution_time: row.try_get("max_execution_time")?,
        innodb_lock_wait_timeout: row.try_get("innodb_lock_wait_timeout")?,
        information_schema_stats_expiry: row.try_get("information_schema_stats_expiry")?,
        autocommit: row.try_get("autocommit")?,
        foreign_key_checks: row.try_get("foreign_key_checks")?,
        unique_checks: row.try_get("unique_checks")?,
    })
}

async fn restore_session_snapshot<D: SqlSession>(
    conn: &D,
    snap: &MysqlSessionSnapshot,
) -> Result<(), crate::driver::DbError> {
    // `sql_mode` is server-provided text and remains a native bind. MySQL does
    // not accept prepared parameters for the numeric system variables, so their
    // already-decoded i64 values are the only interpolated tokens.
    conn.exec(
        &format!(
            "SET SESSION sql_mode = ?, SESSION time_zone = ?, \
             SESSION max_execution_time = {}, \
             SESSION innodb_lock_wait_timeout = {}, \
             SESSION information_schema_stats_expiry = {}, \
             SESSION autocommit = {}, \
             SESSION foreign_key_checks = {}, \
             SESSION unique_checks = {}",
            snap.max_execution_time,
            snap.innodb_lock_wait_timeout,
            snap.information_schema_stats_expiry,
            snap.autocommit,
            snap.foreign_key_checks,
            snap.unique_checks
        ),
        &[
            snap.sql_mode.as_str().into(),
            snap.time_zone.as_str().into(),
        ],
    )
    .await?;
    Ok(())
}

pub(crate) async fn snapshot_session<D: SqlSession>(
    conn: &D,
) -> Result<MysqlSessionSnapshot, ApplyError> {
    read_session_snapshot(conn).await
}

pub(crate) async fn restore_session<D: SqlSession>(
    conn: &D,
    snap: &MysqlSessionSnapshot,
) -> Result<(), ApplyError> {
    restore_session_snapshot(conn, snap)
        .await
        .map_err(|e| ApplyError::Db(e.into()))
}

/// Session-level `SET SESSION …` for the (always non-txn on MySQL) apply path.
/// Renders the `max_execution_time` (ms) + `innodb_lock_wait_timeout` (s) budgets;
/// no schema search-path (MySQL uses the connection default database) and no
/// `SET ROLE` (grants ARE the confinement). Idempotent + session-scoped.
///
/// # Errors
/// [`ApplyError::Db`] on failure.
pub(crate) async fn configure_session<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
) -> Result<(), ApplyError> {
    // `max_execution_time` caps SELECTs only in stock MySQL, but it is the closest
    // per-statement budget knob and is harmless for DDL; the lock-wait timeout is
    // the DDL-relevant one. Both are session-scoped SETs.
    let stmt = session_settings_sql(
        effective_timeout_ms(cfg, m)?,
        effective_lock_timeout_secs(cfg, m)?,
    );
    conn.batch(&stmt).await?;
    Ok(())
}

/// Apply the default execution and lock-wait budgets to a structured data step.
/// DML/backfill steps do not carry per-migration timeout overrides, so they use
/// the executor defaults. MySQL's settings are session-scoped and therefore must
/// be refreshed before each data step on a pooled connection.
///
/// Both budgets come from the config, which is where a sub-millisecond `Duration`
/// truncates to the zero MySQL reads as "no limit", so this render runs the same
/// refusal the per-migration one does.
pub(crate) async fn configure_data_session<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
) -> Result<(), ApplyError> {
    let timeout_ms = resolve_timeout_ms(
        version,
        "max_execution_time",
        None,
        "timeout_ms",
        cfg.statement_timeout_ms(),
        "pg.statement_timeout",
    )?;
    let lock_secs = resolve_timeout_ms(
        version,
        "innodb_lock_wait_timeout",
        None,
        "lock_timeout_ms",
        cfg.lock_timeout_ms(),
        "pg.lock_timeout",
    )?
    .div_ceil(1000)
    .max(1);
    conn.batch(&session_settings_sql(timeout_ms, lock_secs))
        .await?;
    Ok(())
}

/// Convert the closed plan bind union to the driver-neutral MySQL bind union.
/// Values stay typed and cross the driver seam separately from the SQL text.
fn mysql_binds(values: &[BindValue]) -> Result<Vec<Bind>, ApplyError> {
    values
        .iter()
        .map(|value| match value {
            BindValue::Null => Ok(Bind::Null),
            BindValue::Bool(value) => Ok(Bind::Bool(*value)),
            BindValue::Int(value) => Ok(Bind::Int(*value)),
            BindValue::Decimal(value) => Ok(Bind::Decimal(value.clone())),
            BindValue::Text(value) => Ok(Bind::Text(value.clone())),
            BindValue::Bytes(_) => Err(ApplyError::Backend(
                "mysql DML: raw binary bind reached the backend without a FROM_BASE64 wrapper"
                    .to_string(),
            )),
        })
        .collect()
}

/// Apply one structured DML statement and its completed journal event in the
/// same InnoDB transaction. Unlike MySQL DDL, ordinary DML is transactional, so
/// a crash cannot leave the row mutation committed without its idempotency
/// marker (or vice versa).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_dml_transactional<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
    checksum: &Checksum,
    name: &str,
    template: &str,
    binds: &[BindValue],
    target_schema: &str,
    target_table: &str,
    mutates_data: bool,
    conflict_target: Option<&[String]>,
    applied_by: &str,
) -> Result<(), ApplyError> {
    configure_data_session(conn, cfg, version).await?;

    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    let target_lock_sql = mutates_data
        .then(|| {
            Ok::<_, crate::apply::journal::JournalError>(format!(
                "SELECT 1 AS zero_migrate_metadata_lock FROM {}.{} LIMIT 0",
                journal_sql::quote_ident_mysql(target_schema)?,
                journal_sql::quote_ident_mysql(target_table)?,
            ))
        })
        .transpose()?;
    let params = mysql_binds(binds)?;
    let started = Instant::now();

    conn.batch("START TRANSACTION").await?;
    if let Some(lock_sql) = target_lock_sql.as_deref() {
        let preflight: Result<(), ApplyError> = async {
            // Open the target table inside this transaction before reading its
            // index metadata. MySQL retains the resulting shared metadata lock
            // through COMMIT/ROLLBACK, so a concurrent DDL cannot invalidate the
            // catalog proof between this check and the duplicate-key statement.
            conn.query(lock_sql, &[]).await?;
            super::ensure_transactional_dml_target(conn, target_schema, target_table).await?;
            if let Some(target_columns) = conflict_target {
                super::ensure_exact_unique_conflict_target(
                    conn,
                    target_schema,
                    target_table,
                    target_columns,
                )
                .await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = preflight {
            if let Err(rollback) = conn.batch("ROLLBACK").await {
                tracing::warn!(
                    error = %rollback,
                    version = %version,
                    "zero-migrate: MySQL ROLLBACK failed after target-table preflight"
                );
            }
            return Err(error);
        }
    }
    if let Err(error) = conn.exec(template, &params).await {
        if let Err(rollback) = conn.batch("ROLLBACK").await {
            tracing::warn!(
                error = %rollback,
                version = %version,
                "zero-migrate: MySQL ROLLBACK failed after a DML error"
            );
        }
        return Err(ApplyError::MigrationFailed {
            version: version.to_string(),
            source: error.into(),
        });
    }

    if let Err(error) = crate::fault::trip(crate::fault::points::DML_AFTER_STMT_BEFORE_JOURNAL) {
        let _ = conn.batch("ROLLBACK").await;
        return Err(error);
    }

    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let journal_result = conn
        .exec(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (event_kind, version, name, checksum, `by`, exec_ms, phase, outcome, kind)
                 VALUES ('{applied}', ?, ?, ?, ?, ?, 'completed', 'success', 'apply')",
                applied = journal::EventKind::Applied.as_str()
            ),
            &[
                version.into(),
                name.into(),
                checksum.as_str().into(),
                applied_by.into(),
                exec_ms.into(),
            ],
        )
        .await;
    if let Err(error) = journal_result {
        if let Err(rollback) = conn.batch("ROLLBACK").await {
            tracing::warn!(
                error = %rollback,
                version = %version,
                "zero-migrate: MySQL ROLLBACK failed after a DML journal error"
            );
        }
        return Err(ApplyError::Journal(journal::JournalError::Db(error.into())));
    }

    if let Err(error) = crate::fault::trip(crate::fault::points::DML_AFTER_JOURNAL_BEFORE_COMMIT) {
        let _ = conn.batch("ROLLBACK").await;
        return Err(error);
    }

    match conn.batch("COMMIT").await {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Err(rollback) = conn.batch("ROLLBACK").await {
                tracing::warn!(
                    error = %rollback,
                    version = %version,
                    "zero-migrate: MySQL ROLLBACK failed after an ambiguous DML COMMIT failure"
                );
            }
            Err(ApplyError::Db(error.into()))
        }
    }
}

/// The MySQL two-phase apply of ONE migration (auto-committing DDL forces the
/// non-txn path for every MySQL migration):
///
/// 1. Refuse an unmatched inflight marker. Auto-committing MySQL DDL may already
///    have landed, and generated `CREATE`/`ALTER` SQL cannot be blindly replayed.
/// 2. Write the `started` marker (`INSERT IGNORE`).
/// 3. Run the migration's `up` (auto-commits).
/// 4. Atomically append the immutable `completed` row, append every squash edge,
///    and clear the marker in one InnoDB transaction. The caller-supplied journal
///    kind is preserved so repeatable checksums remain discoverable.
///
/// Returns `false` on success. A prior inflight marker returns an error and is
/// left intact for an operator repair: either a direct `DELETE` on the mutable
/// inflight side-table after the operator restores the pre-migration shape, or
/// [`super::MysqlBackend::recover_inflight_ddl`] from a Rust host for the same
/// resolution plus a marker-identity check and an immutable audit row.
///
/// # Errors
/// [`ApplyError::MigrationFailed`] if the `up` failed; [`ApplyError::Db`] /
/// [`ApplyError::Journal`] on infrastructure failure.
pub(crate) async fn apply_two_phase<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
    had_inflight: bool,
    supersedes: &[&str],
    requested_kind: &str,
) -> Result<bool, ApplyError> {
    let version = m.version.as_str();

    if had_inflight {
        // Name the repair the person reading this can actually perform. The
        // inflight side-table is mutable by design and the applying credential
        // already deletes from it on every successful apply (`clear_inflight`),
        // so clearing the marker needs no extra privilege and no extra API.
        // `recover_inflight_ddl` is the audited alternative, and it is reachable
        // only from a Rust host that depends on this crate directly.
        let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
        return Err(ApplyError::Backend(format!(
            "mysql migration {version} has an inflight marker from an interrupted \
             auto-committing DDL apply; zero-migrate will not replay possibly-applied \
             CREATE/ALTER statements; inspect the live schema and the migration SQL, \
             then take one of two repairs. (1) Restore and verify the complete \
             pre-migration shape yourself, clear the marker with \
             DELETE FROM {meta}.schema_migrations_inflight WHERE version = '{version}', \
             and run apply again; this is a supported repair on the mutable inflight \
             side-table and is the only route available from the CLI and the Node SDK. \
             (2) From a Rust host that embeds this crate, call \
             MysqlBackend::recover_inflight_ddl with MarkAppliedAfterVerification \
             after verifying the complete new shape, or ClearForRetryAfterRollback \
             after restoring and verifying the complete old shape; over route (1) it \
             adds a marker-identity check against the reviewed migration and an \
             immutable recovery audit row. Neither route inspects the database: \
             recovery does NOT verify schema shape, it records your assertion about \
             it. Do not fabricate a completed event in the append-only journal"
        )));
    }
    let kind = match (requested_kind, supersedes.is_empty()) {
        ("apply" | "repeatable", true) | ("squash", false) => requested_kind,
        _ => {
            return Err(ApplyError::Backend(format!(
                "mysql migration {version} has inconsistent journal kind \
                 {requested_kind:?} and {} supersession edges",
                supersedes.len()
            )));
        }
    };
    // Arm the started marker before the `up` runs.
    journal_sql::record_started(conn, cfg, version, &m.name, m.checksum.as_str(), applied_by)
        .await?;

    let started = Instant::now();
    conn.batch(&m.up)
        .await
        .map_err(|e| ApplyError::MigrationFailed {
            version: version.to_string(),
            source: e.into(),
        })?;
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    finalize_two_phase(conn, cfg, m, applied_by, exec_ms, supersedes, kind).await?;

    Ok(false)
}

/// Finalize structured DDL whose started marker was armed while an explicit
/// target-table lock was still held. The DDL itself releases the MySQL table
/// lock; completion then uses the ordinary atomic journal-finalize transaction.
pub(crate) async fn finalize_started_structured_ddl<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
    exec_ms: i64,
) -> Result<(), ApplyError> {
    finalize_two_phase(conn, cfg, m, applied_by, exec_ms, &[], "apply").await
}

#[allow(clippy::too_many_arguments)]
async fn finalize_two_phase<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
    exec_ms: i64,
    supersedes: &[&str],
    kind: &str,
) -> Result<(), ApplyError> {
    let version = m.version.as_str();

    // Finalize the immutable completed row, all fresh-squash edges, and marker
    // cleanup commit together. A crash or statement error cannot expose a squash
    // completion without its complete coverage set, or delete the recovery marker
    // without the corresponding completion event.
    conn.batch("START TRANSACTION").await?;
    let finalize = async {
        journal_sql::append_completed(
            conn,
            cfg,
            CompletedRecord {
                version,
                name: &m.name,
                checksum: m.checksum.as_str(),
                applied_by,
                exec_ms,
                kind,
            },
        )
        .await?;
        insert_supersedes_edges(conn, cfg, version, supersedes).await?;
        journal_sql::clear_inflight(conn, cfg, version).await?;
        Ok::<(), ApplyError>(())
    }
    .await;
    if let Err(error) = finalize {
        if let Err(rollback) = conn.batch("ROLLBACK").await {
            tracing::warn!(
                error = %rollback,
                version = %version,
                "zero-migrate: MySQL ROLLBACK failed after journal finalization error"
            );
        }
        return Err(error);
    }
    if let Err(error) = conn.batch("COMMIT").await {
        if let Err(rollback) = conn.batch("ROLLBACK").await {
            tracing::warn!(
                error = %rollback,
                version = %version,
                "zero-migrate: MySQL ROLLBACK failed after ambiguous journal finalization COMMIT"
            );
        }
        return Err(ApplyError::Db(error.into()));
    }

    Ok(())
}

/// Insert the `S → v_i` supersession edges for a fresh-path squash (the MySQL
/// analogue of the PG `insert_supersedes_edges`). The caller holds the journal
/// finalization transaction. `INSERT IGNORE` plus the unique edge key makes a
/// retried repair idempotent. `?` placeholders.
pub(super) async fn insert_supersedes_edges<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    squash_version: &str,
    supersedes: &[&str],
) -> Result<(), ApplyError> {
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    for sup in supersedes {
        conn.exec(
            &format!(
                "INSERT IGNORE INTO {meta}.schema_migrations_supersedes
                     (squash_version, superseded_version)
                 VALUES (?, ?)"
            ),
            &[squash_version.into(), (*sup).into()],
        )
        .await
        .map_err(|e| ApplyError::Journal(journal::JournalError::Db(e.into())))?;
    }
    Ok(())
}

/// Roll back ONE migration: run the `down`, then append the `rolled_back` event
/// (the MySQL analogue of `rollback_one_transactional`). MySQL DDL auto-commits,
/// so — unlike PG — the `down` and its journal append are NOT one atomic
/// transaction; the append is ordered strictly after a successful `down`.
///
/// # Errors
/// [`RollbackError::DownFailed`] if the `down` failed (nothing journaled);
/// [`RollbackError::Journal`] on a journal-append failure.
pub(crate) async fn rollback_one<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
) -> Result<(), RollbackError> {
    // An irreversible migration is a REFUSAL, not a caller bug. See the same gate on
    // the PostgreSQL leaf: the trait method is public, advertises no `down is Some`
    // precondition, and irreversible steps are ordinary on the IR path.
    let down = m
        .down
        .as_deref()
        .ok_or_else(|| RollbackError::Irreversible {
            version: m.version.as_str().to_string(),
            name: m.name.clone(),
        })?;
    let version = m.version.as_str();
    let snapshot = read_session_snapshot(conn)
        .await
        .map_err(|error| match error {
            ApplyError::Db(error) => RollbackError::Db(error),
            ApplyError::Journal(error) => RollbackError::Journal(error),
            ApplyError::Backend(message) => RollbackError::Backend(message),
            other => RollbackError::Backend(other.to_string()),
        })?;
    let result: Result<(), RollbackError> = async {
        // Rollback is another author-SQL execution entry. Pin the same literal
        // mode and budgets as apply before the first byte of `down` reaches MySQL.
        conn.batch(&session_settings_sql(
            effective_timeout_ms(cfg, m)?,
            effective_lock_timeout_secs(cfg, m)?,
        ))
        .await
        .map_err(|e| RollbackError::Db(e.into()))?;
        let started = Instant::now();
        conn.batch(down)
            .await
            .map_err(|e| RollbackError::DownFailed {
                version: version.to_string(),
                source: e.into(),
            })?;
        let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        journal_sql::record_rolled_back(
            conn,
            cfg,
            version,
            &m.name,
            m.checksum.as_str(),
            applied_by,
            exec_ms,
        )
        .await
        .map_err(RollbackError::Journal)?;
        Ok(())
    }
    .await;
    let restored = restore_session_snapshot(conn, &snapshot).await;
    match (result, restored) {
        (Err(error), Err(restore)) => {
            tracing::warn!(
                error = %restore,
                version,
                "zero-migrate: failed to restore MySQL session after rollback error"
            );
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(RollbackError::Db(error.into())),
        (Ok(()), Ok(())) => Ok(()),
    }
}
