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
//! `completed` row + clear the marker, with idempotent crash recovery. This is
//! exactly the shape Postgres uses for its `CREATE INDEX CONCURRENTLY` non-txn
//! migrations, generalized to every MySQL migration.
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
use crate::apply::executor::{ApplyError, RollbackError};
use crate::apply::journal::{self, CompletedRecord};
use crate::conn::ExecutorConfig;
use crate::model::migration::Migration;
use crate::driver::SqlSession;

/// How long `GET_LOCK` waits (seconds) for the project apply lock before the
/// acquire is treated as contended. Mirrors the "serialize concurrent deploys"
/// intent of the PG advisory lock — a second apply waits here, then sees the
/// first's committed journal and no-ops.
const PROJECT_LOCK_TIMEOUT_SECS: i64 = 10;

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
    let digest = Sha256::digest(project_id.as_bytes());
    hex::encode(digest) // 64 hex chars
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
    let got: Option<i64> = row.try_get("got").map_err(|e| ApplyError::Backend(format!(
        "mysql GET_LOCK returned an undecodable result: {e}"
    )))?;
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
fn effective_timeout_ms(cfg: &ExecutorConfig, m: &Migration) -> u64 {
    m.flags.timeout_ms.unwrap_or_else(|| cfg.statement_timeout_ms())
}

/// The effective `innodb_lock_wait_timeout` (seconds) for a migration: its
/// per-migration lock override (ms → whole seconds, min 1) if set, else the SHORT
/// executor-wide default. Mirrors the PG `lock_timeout` render + the lock-safety
/// envelope (a short lock-acquisition budget separate from the long statement
/// budget).
fn effective_lock_timeout_secs(cfg: &ExecutorConfig, m: &Migration) -> u64 {
    let ms = m
        .flags
        .lock_timeout_ms
        .unwrap_or_else(|| cfg.lock_timeout_ms());
    // Round UP to whole seconds (MySQL's unit), floor 1s so a sub-second budget
    // never becomes a 0 = "no wait" that fails every contended DDL.
    ((ms + 999) / 1000).max(1)
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
    let stmt = format!(
        "SET SESSION max_execution_time = {}, SESSION innodb_lock_wait_timeout = {}",
        effective_timeout_ms(cfg, m),
        effective_lock_timeout_secs(cfg, m),
    );
    conn.batch(&stmt).await?;
    Ok(())
}

/// The MySQL two-phase apply of ONE migration (auto-committing DDL forces the
/// non-txn path for every MySQL migration):
///
/// 1. If `had_inflight`, clear the stale marker (idempotent recovery — the `up` is
/// re-run, so it must tolerate having partially run; MySQL DDL is largely
/// `IF (NOT) EXISTS`-guardable by the author).
/// 2. (Re-)write the `started` marker (`INSERT IGNORE`).
/// 3. Run the migration's `up` (auto-commits).
/// 4. Append the immutable `completed` row + clear the marker.
///
/// Returns `true` iff a prior inflight marker was recovered.
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
) -> Result<bool, ApplyError> {
    let version = m.version.as_str();

    if had_inflight {
        // Recovery: clear the stale marker; the re-run of the (author-idempotent)
        // `up` below is safe. MySQL has no INVALID-index residue analogue to drop.
        journal_sql::clear_inflight(conn, cfg, version).await?;
    }
    // (Re-)arm the started marker before the `up` runs.
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

    // Phase 2: the immutable completed row + clear the marker. A fresh-path squash
    // is stamped `kind='squash'` so its supersession edges are honored by
    // `superseded_versions`; the edges are written after the completed row.
    let kind = if supersedes.is_empty() { "apply" } else { "squash" };
    journal_sql::record_completed(
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
    if !supersedes.is_empty() {
        insert_supersedes_edges(conn, cfg, version, supersedes).await?;
    }

    Ok(had_inflight)
}

/// Insert the `S → v_i` supersession edges for a fresh-path squash (the MySQL
/// analogue of the PG `insert_supersedes_edges`). MySQL DML is not wrapped in a txn
/// with the DDL (auto-commit), so the edges are appended right after the
/// `completed` row. `?` placeholders.
async fn insert_supersedes_edges<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    squash_version: &str,
    supersedes: &[&str],
) -> Result<(), ApplyError> {
    let meta = journal_sql::quote_ident_mysql(&cfg.pg.meta_schema)?;
    for sup in supersedes {
        conn.exec(
            &format!(
                "INSERT INTO {meta}.schema_migrations_supersedes
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
    let down = m
        .down
        .as_deref()
        .expect("rollback_one is only called for RollbackStep::Down (down is Some)");
    let version = m.version.as_str();
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
