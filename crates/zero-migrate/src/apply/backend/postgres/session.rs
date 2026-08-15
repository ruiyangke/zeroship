//! Postgres dialect SQL leaves for [`PostgresBackend`](super::PostgresBackend).
//!
//! These are the Postgres-specific session/lock/txn/journal/DML/rollback
//! operations the [`PostgresBackend`](super::PostgresBackend)
//! [`MigrationBackend`](crate::apply::backend::MigrationBackend) impl drives. They
//! were relocated here **verbatim** from the generic `apply::executor` so the
//! generic executor issues NO dialect-specific SQL: the
//! `pg_advisory_lock`/`pg_advisory_unlock` project lock, the GUC
//! snapshot/restore + unconditional `RESET ROLE` session hygiene, the
//! `SET LOCAL search_path`/`statement_timeout`/`lock_timeout` + `SET [LOCAL] ROLE`
//! confinement clauses, the transactional/non-transactional/DML apply paths, the
//! crash-recovery drop-of-INVALID-index residue, the fresh-path squash
//! supersession-edge writes, and the transactional rollback — every one of them
//! Postgres-flavoured (`$N` placeholders, `hashtext`, `pg_index`), so they live
//! in the Postgres backend, not the shared executor.
//!
//! The generic orchestration (partition, drift gate, `order_pending`, the
//! two-pass apply loop, the repeatable phase, rollback selection) stays in
//! [`crate::apply::executor`] and reaches every one of these through the
//! [`MigrationBackend`](crate::apply::backend::MigrationBackend) trait — never by
//! naming a leaf below directly.

use std::time::Instant;

use pg_query::protobuf::node::Node as NodeEnum;
use pg_query::protobuf::ObjectType;

use crate::apply::backend::{PgSessionSnapshot, ProjectLockHolder};
use crate::apply::executor::{authorize_existence_guard_schema, ApplyError, RollbackError};
use crate::apply::journal::{self, JournalError};
use crate::apply::timeout::{resolve_timeout_ms, IndefiniteTimeoutError as TimeoutError};
use crate::conn::ExecutorConfig;
use crate::driver::SqlSession;
use crate::model::migration::Migration;

/// PostgreSQL's author-side renderers use standard quote doubling for inline
/// string literals. Pin their interpretation inside each transaction so an
/// inherited `standard_conforming_strings = off` cannot reinterpret backslashes.
/// `SET LOCAL` also guarantees that commit or rollback restores the caller's
/// original session value without adding another session snapshot field.
pub(super) const AUTHOR_SQL_LITERAL_MODE: &str = "SET LOCAL standard_conforming_strings = on;";

/// A stable i64 advisory-lock key from the project id, mirroring the
/// `hashtext(project_id)` design intent: we run `pg_advisory_lock(hashtext($1))`
/// server-side so the key is computed by Postgres exactly as the design states
/// (`hashtext` is the canonical PG text hash; deterministic per cluster).
///
/// Holding it for the whole apply serializes concurrent deploys for the same
/// project; a second apply waits, then sees the first's
/// committed journal and no-ops.
///
/// Known limitation: `hashtext` yields a 32-bit hash, so two *unrelated*
/// project ids can collide onto the same advisory-lock key. The consequence is
/// liveness-only — two unrelated projects would serialize against each other
/// (one waits for the other's apply) — never a correctness/cross-tenant defect,
/// since each apply still operates strictly within its own meta + project
/// schema. Acceptable for v1. Revisit at scale with a 64-bit key
/// (`pg_advisory_lock(int4, int4)` from a SHA-256 prefix, or two keys).
#[cfg(pg_seam)]
pub(crate) async fn acquire_project_lock<D: SqlSession>(
    conn: &D,
    project_id: &str,
) -> Result<(), ApplyError> {
    match conn
        .exec(
            "SELECT pg_advisory_lock(hashtext($1)::bigint)",
            &[project_id.into()],
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(error) => {
            drop_grant_from_failed_acquire(conn, project_id).await;
            Err(error.into())
        }
    }
}

/// Drop a grant the server may have recorded for an acquisition that then failed,
/// so a session outliving the failure does not carry a lock nobody tracks.
///
/// PostgreSQL can grant a session advisory lock and still fail the acquiring
/// statement. `LockErrorCleanup` re-grants a waiter that was kicked off the lock
/// queue ("If they did grant us the lock, we'd better remember it in our local
/// table"), and `pg_advisory_lock` acquires with `sessionLock = true`, so the
/// grant outlives the transaction abort that follows. A cancelled lock wait is the
/// live case: a peer's release lands in the same instant as the acquirer's
/// `statement_timeout`, and the acquirer holds a lock it was told it never got. An
/// acquisition whose reply is lost on the way back has the same shape. In every
/// one of them the acquisition reported failure, so no caller has anything to
/// release with.
///
/// The shipped CLI is not exposed: it opens a fresh `pg.Client` per verb and
/// closes it in a `finally`, and session exit drops every advisory lock. What is
/// exposed is a Rust embedder, or any pooled or long-lived session, where the
/// connection survives the failed acquisition and nothing ever releases the grant.
///
/// Best effort by construction, on the ERROR path only. Releasing after a
/// SUCCESSFUL acquisition would free the lock the caller is about to rely on.
/// `pg_advisory_unlock` on a key this session never held returns false rather than
/// erroring, so compensating for a grant that never happened cannot turn a failed
/// acquisition into a worse failure. Session advisory locks stack by depth, so
/// this would drop an outer bracket's hold if one existed -- none does: every
/// acquisition site is the outermost bracket for its session, and an inner
/// sub-batch runs under `LockMode::AlreadyHeld` and takes no lock of its own.
///
/// PostgreSQL only, deliberately. MySQL's `GET_LOCK` grants nothing when it
/// returns 0 or errors, and SQLite's project lock is a local file try_lock, so
/// neither has a grant to compensate for and neither gets this call.
#[cfg(pg_seam)]
async fn drop_grant_from_failed_acquire<D: SqlSession>(conn: &D, project_id: &str) {
    if let Err(error) = release_project_lock(conn, project_id).await {
        tracing::warn!(
            error = %error,
            "zero-migrate: failed to drop a possible advisory-lock grant after a failed project-lock acquisition"
        );
    }
}

/// Take the project lock if it is free right now, without waiting.
///
/// `pg_try_advisory_lock` is the non-waiting peer of the `pg_advisory_lock` above
/// and takes the identical `hashtext(project_id)` key, so a reader and a deploy
/// contend on exactly the same lock.
///
/// `pg_try_advisory_lock` never waits, so it has no grant-then-cancel window of
/// its own; what it shares with the blocking acquisition is a lock the server
/// granted and the caller never learned about, whether the reply was lost in
/// transport or arrived undecodable. Both failure paths therefore compensate
/// through [`drop_grant_from_failed_acquire`], which documents why.
///
/// # Errors
/// [`ApplyError::Db`] on a driver failure, [`ApplyError::Backend`] if the boolean
/// result cannot be decoded.
#[cfg(pg_seam)]
pub(crate) async fn try_acquire_project_lock<D: SqlSession>(
    conn: &D,
    project_id: &str,
) -> Result<bool, ApplyError> {
    let row = match conn
        .query_one(
            "SELECT pg_try_advisory_lock(hashtext($1)::bigint) AS got",
            &[project_id.into()],
        )
        .await
    {
        Ok(row) => row,
        Err(error) => {
            drop_grant_from_failed_acquire(conn, project_id).await;
            return Err(error.into());
        }
    };
    match row.try_get::<_, bool>("got") {
        Ok(got) => Ok(got),
        Err(error) => {
            drop_grant_from_failed_acquire(conn, project_id).await;
            Err(ApplyError::Backend(format!(
                "pg_try_advisory_lock returned an undecodable result: {error}"
            )))
        }
    }
}

/// Report the sessions holding this project's advisory lock, for an operator
/// message naming who a reader is waiting behind.
///
/// Scoped to the project's own key rather than every advisory lock in the cluster:
/// a single-argument `pg_advisory_lock(int8)` is recorded as `objsubid = 1` with
/// the key split across `classid` (high 32 bits) and `objid` (low 32 bits), so
/// reassembling them reconstructs the exact `hashtext(project_id)` value. The
/// reassembly is signed-correct because `hashtext` is an `int4` that sign-extends
/// to a negative `int8` for roughly half of all project ids, and PostgreSQL's
/// `int8` shift wraps rather than erroring, which is what puts the sign bits back.
///
/// `query` is NULL unless the reading role may see other sessions' statement text
/// (superuser or `pg_read_all_stats`), so it is reported as optional rather than
/// demanded.
///
/// # Errors
/// [`ApplyError::Db`] on a driver failure.
#[cfg(pg_seam)]
pub(crate) async fn project_lock_holders<D: SqlSession>(
    conn: &D,
    project_id: &str,
) -> Result<Vec<ProjectLockHolder>, ApplyError> {
    let rows = conn
        .query(
            "SELECT a.pid::int8 AS pid, a.application_name, a.state, a.query \
               FROM pg_locks l JOIN pg_stat_activity a USING (pid) \
              WHERE l.locktype = 'advisory' AND l.granted AND l.objsubid = 1 \
                AND ((l.classid::bigint << 32) | l.objid::bigint) = hashtext($1)::bigint \
              ORDER BY a.pid",
            &[project_id.into()],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(ProjectLockHolder {
                pid: row.try_get("pid")?,
                application_name: row.try_get("application_name")?,
                state: row.try_get("state")?,
                query: row.try_get("query")?,
            })
        })
        .collect()
}

#[cfg(pg_seam)]
pub(crate) async fn release_project_lock<D: SqlSession>(
    conn: &D,
    project_id: &str,
) -> Result<(), ApplyError> {
    conn.exec(
        "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
        &[project_id.into()],
    )
    .await?;
    Ok(())
}

/// Read PostgreSQL's machine-comparable server version number (for example,
/// `180000` for PostgreSQL 18). Feature gates use this value instead of probing
/// for a function name whose availability could be changed by extensions or
/// `search_path`.
#[cfg(pg_seam)]
pub(crate) async fn server_version_num<D: SqlSession>(conn: &D) -> Result<i32, ApplyError> {
    let row = conn
        .query_one(
            "SELECT current_setting('server_version_num') AS server_version_num",
            &[],
        )
        .await?;
    let raw: String = row.try_get("server_version_num")?;
    raw.parse::<i32>().map_err(|error| {
        ApplyError::Backend(format!(
            "PostgreSQL returned invalid server_version_num {raw:?}: {error}"
        ))
    })
}

/// Read the session GUCs we are about to override, so they can be restored when
/// `apply` finishes. Uses `current_setting(name)` (text form, exactly what `SET`
/// round-trips).
#[cfg(pg_seam)]
pub(crate) async fn snapshot_session<D: SqlSession>(
    conn: &D,
) -> Result<PgSessionSnapshot, ApplyError> {
    let row = conn
        .query_one(
            "SELECT current_setting('statement_timeout') AS st, \
                    current_setting('lock_timeout')      AS lt, \
                    current_setting('search_path')       AS sp",
            &[],
        )
        .await?;
    Ok(PgSessionSnapshot {
        statement_timeout: row.try_get("st")?,
        lock_timeout: row.try_get("lt")?,
        search_path: row.try_get("sp")?,
    })
}

/// Restore the GUCs captured by [`snapshot_session`]. Uses `set_config(name,
/// value, false)` so the *value* is a bound literal, not interpolated SQL
/// (the snapshot strings are server-provided, but we keep the parameterized
/// path regardless).
#[cfg(pg_seam)]
pub(crate) async fn restore_session<D: SqlSession>(
    conn: &D,
    snap: &PgSessionSnapshot,
) -> Result<(), ApplyError> {
    // RESET ROLE first: belt-and-suspenders behind `apply`'s unconditional
    // `RESET ROLE`. The non-txn path's `SET ROLE` mutates the session, so
    // drop back to the admin role before anything else, ensuring the executor's
    // least-privilege confinement never leaks onto the pooled/long-lived
    // connection after `apply` returns. Harmless no-op when no SET ROLE ran
    // (txn-only applies use SET LOCAL ROLE, auto-reverted at COMMIT).
    conn.batch("RESET ROLE").await?;
    conn.exec(
        "SELECT set_config('statement_timeout', $1, false), \
                set_config('lock_timeout', $2, false), \
                set_config('search_path', $3, false)",
        &[
            (&snap.statement_timeout).into(),
            (&snap.lock_timeout).into(),
            (&snap.search_path).into(),
        ],
    )
    .await?;
    Ok(())
}

/// The two ways rendering a confined session can fail before any SQL is sent: an
/// engine identifier that is not quotable, and a timeout budget the database
/// would read as "no limit". Both are fail-closed pre-`BEGIN` refusals, and both
/// reach callers that report [`ApplyError`] (apply, identity, primary key) and
/// callers that report [`RollbackError`] (the `down` leaf), so the render names
/// one type and each caller's `?` widens it.
#[derive(Debug, thiserror::Error)]
pub(super) enum SessionRenderError {
    /// An engine-supplied identifier was not quotable.
    #[error(transparent)]
    IdentQuote(#[from] crate::render::dml::IdentQuoteError),
    /// A timeout budget resolved to the database's "no limit" sentinel.
    #[error(transparent)]
    IndefiniteTimeout(#[from] TimeoutError),
}

impl From<SessionRenderError> for ApplyError {
    fn from(error: SessionRenderError) -> Self {
        match error {
            SessionRenderError::IdentQuote(e) => Self::IdentQuote(e),
            SessionRenderError::IndefiniteTimeout(e) => Self::IndefiniteTimeout(e),
        }
    }
}

impl From<SessionRenderError> for RollbackError {
    fn from(error: SessionRenderError) -> Self {
        match error {
            SessionRenderError::IdentQuote(e) => Self::IdentQuote(e),
            SessionRenderError::IndefiniteTimeout(e) => Self::IndefiniteTimeout(e),
        }
    }
}

/// The effective `statement_timeout` for a migration: its per-migration
/// override ([`crate::model::migration::MigrationFlags::timeout_ms`]) if set, else
/// the executor-wide default.
///
/// Zero is refused, not clamped; see [`crate::apply::timeout`] for why the rule
/// lives at this resolution rather than only at the IR load gate.
fn effective_timeout_ms(cfg: &ExecutorConfig, m: &Migration) -> Result<u64, TimeoutError> {
    resolve_timeout_ms(
        m.version.as_str(),
        "statement_timeout",
        m.flags.timeout_ms,
        "timeout_ms",
        cfg.statement_timeout_ms(),
        "pg.statement_timeout",
    )
}

/// The effective `lock_timeout` for a migration: its per-migration override
/// ([`crate::model::migration::MigrationFlags::lock_timeout_ms`]) if set, else the
/// SHORT executor-wide default (the lock-safety envelope, 3s). This is the
/// per-deploy maintenance-window knob that makes the doc on
/// [`crate::conn::PgConfinement::lock_timeout`] honest: a single planned migration
/// can legitimately raise ITS OWN lock-acquisition budget (run during a quiet
/// window), while every other migration keeps the conservative fail-fast
/// default. It mirrors [`effective_timeout_ms`] exactly, refusal included.
fn effective_lock_timeout_ms(cfg: &ExecutorConfig, m: &Migration) -> Result<u64, TimeoutError> {
    resolve_timeout_ms(
        m.version.as_str(),
        "lock_timeout",
        m.flags.lock_timeout_ms,
        "lock_timeout_ms",
        cfg.lock_timeout_ms(),
        "pg.lock_timeout",
    )
}

/// `SET LOCAL …` clauses (transaction-scoped) for the **txn path** — they
/// vanish at COMMIT/ROLLBACK, so nothing leaks onto the session. Pins the
/// project `search_path` (project schema **only** — the meta schema is
/// deliberately OFF the migration-time path so an unqualified name in the `up`
/// can never resolve to the journal, defense-in-depth) and the mandatory
/// timeouts, with the per-migration timeout override applied.
///
/// This intentionally does **not** switch role: the role scoping is done
/// explicitly in [`apply_transactional`] so that `SET LOCAL ROLE migrator`
/// brackets ONLY the `<up>` and is `RESET` (back to admin) before the journal
/// INSERT — the migrator can no longer write the journal (its grant is revoked),
/// so the journal write must run as the admin, atomically in the SAME
/// transaction as the `up`.
pub(super) fn set_local_session_sql(
    cfg: &ExecutorConfig,
    m: &Migration,
) -> Result<String, SessionRenderError> {
    Ok(format!(
        "SET LOCAL search_path TO {}; \
         SET LOCAL statement_timeout = {}; \
         SET LOCAL lock_timeout = {};",
        cfg.search_path_clause()?,
        effective_timeout_ms(cfg, m)?,
        effective_lock_timeout_ms(cfg, m)?,
    ))
}

/// `SET LOCAL ROLE "<migrator>"` for the txn path, or empty when no migrator role
/// is configured (tests / single-tenant dev). Brackets ONLY the `<up>`; the
/// caller `RESET ROLE`s before the journal write.
///
/// The migrator role is an engine-supplied identifier, so it is quoted through
/// the ONE shared engine seam ([`crate::render::dml::quote_ident_checked`]) — fail-closed
/// on an empty / NUL name, byte-identical to the prior `escape_quote_ident` for
/// every real role.
pub(super) fn set_local_role_sql(
    cfg: &ExecutorConfig,
) -> Result<Option<String>, crate::render::dml::IdentQuoteError> {
    cfg.pg
        .migrator_role
        .as_ref()
        .map(|role| {
            Ok(format!(
                "SET LOCAL ROLE {}",
                crate::render::dml::quote_ident_checked(role)?
            ))
        })
        .transpose()
}

/// A DML step carries no per-migration override slot, so both budgets come from
/// the executor config, which is exactly where a sub-millisecond `Duration` can
/// truncate to the zero the database reads as "no limit", so this render runs the
/// same refusal.
fn dml_set_local_session_sql(cfg: &ExecutorConfig, version: &str) -> Result<String, ApplyError> {
    Ok(format!(
        "{AUTHOR_SQL_LITERAL_MODE} \
         SET LOCAL search_path TO {}; \
         SET LOCAL statement_timeout = {}; \
         SET LOCAL lock_timeout = {};",
        cfg.search_path_clause()?,
        resolve_timeout_ms(
            version,
            "statement_timeout",
            None,
            "timeout_ms",
            cfg.statement_timeout_ms(),
            "pg.statement_timeout",
        )?,
        resolve_timeout_ms(
            version,
            "lock_timeout",
            None,
            "lock_timeout_ms",
            cfg.lock_timeout_ms(),
            "pg.lock_timeout",
        )?,
    ))
}

/// Session-level `SET …` for the **non-txn path** (no transaction to scope to).
/// These DO mutate the session, but [`crate::apply::executor::apply`] restores the
/// original GUCs on exit via [`restore_session`] so they never leak.
/// Per-migration timeout override applied.
///
/// Runs as the **admin** role (no `SET ROLE` here): the non-txn journal I/O
/// (`record_started` / `record_completed`, which also deletes the inflight marker)
/// runs as admin, and
/// only the `<up>` is bracketed by an explicit `SET ROLE migrator` / `RESET ROLE`
/// in [`apply_non_transactional`]. `search_path` is the project schema
/// **only** — the meta schema is off the migration-time path so an unqualified
/// name in the `up` can never resolve to the journal.
#[cfg(pg_seam)]
pub(crate) async fn configure_session_non_txn<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
) -> Result<(), ApplyError> {
    let stmt = format!(
        "SET search_path TO {}; SET statement_timeout = {}; SET lock_timeout = {};",
        cfg.search_path_clause()?,
        effective_timeout_ms(cfg, m)?,
        effective_lock_timeout_ms(cfg, m)?,
    );
    conn.batch(&stmt).await?;
    Ok(())
}

/// Refuse the non-transactional `up` shapes that are known to be un-re-runnable.
///
/// This is a DENY list over three statement groups, not a proof of idempotency,
/// and it runs on the FRESH path before any execution. An `up` it does not
/// recognize is accepted - a `transaction:false` `CREATE TABLE` passes here and
/// applies fine, because nothing crashed. What such an `up` does NOT get is
/// automatic crash recovery: an interrupted apply leaves an armed marker, and
/// [`check_non_txn_up_replayable`] refuses to replay anything it cannot prove safe,
/// preserving the marker and handing the operator the repair. Deciding whether the
/// fresh path should ALSO refuse these shapes is a separate compatibility
/// question; it would reject migrations that work today.
///
/// What is refused here:
///
/// - `CREATE INDEX CONCURRENTLY …` MUST be `CREATE INDEX CONCURRENTLY IF NOT
///   EXISTS …`.
/// - `ALTER TYPE … ADD VALUE …` MUST be `… ADD VALUE IF NOT EXISTS …`.
///
/// (`DROP INDEX CONCURRENTLY` and `VACUUM`, the other non-txn ops the classifier
/// recognizes, are themselves naturally re-runnable.)
///
/// Bare DML — `INSERT` / `UPDATE` / `DELETE` / `MERGE` / `TRUNCATE` — is
/// **forbidden** on the non-txn path. The guard admits DML (it is safe data
/// access), and `transaction:false` will happily route a pure-DML `up` onto the
/// two-phase path; but recovery re-runs the `up` VERBATIM, and a bare
/// `INSERT`/`UPDATE`/`DELETE`/`MERGE` is NOT re-runnable — a success-then-crash
/// (the op committed, the `completed` row did not) double-applies it on recovery.
/// `TRUNCATE` is technically re-runnable, but it cannot run on the non-txn path
/// at all (recovery would have to re-run it AFTER the real op, wiping a table the
/// migration itself may have repopulated), so it is forbidden alongside the rest.
/// DML belongs in a transactional migration (the default), where a crash rolls
/// the whole `up` back atomically and re-apply is clean. There is no idempotent
/// form we can mechanically assert for arbitrary DML, so it is rejected
/// outright; an author who genuinely needs a non-txn data step must wrap it in an
/// idempotent guard (e.g. `INSERT … ON CONFLICT DO NOTHING` driven from a DDL op)
/// rather than a bare statement.
///
/// Why this migration's `down` cannot run inside the transaction the rollback leaf
/// opens, or `None` when nothing in it objects.
///
/// The rollback leaf issues `BEGIN` unconditionally, and gate (5b) of `plan_rollback`
/// used to consult only `flags.transactional` - the author's DECLARATION. A migration
/// declaring `transaction: true` whose `down` reverses itself with a statement
/// PostgreSQL refuses inside a transaction block therefore reached the `BEGIN` and
/// failed there, giving the operator a raw driver error for something the engine had
/// already accepted, and only after earlier downs in the batch had committed.
///
/// # Why only the CONCURRENTLY family
///
/// These forms are non-transactional on PostgreSQL 18 whatever the catalog holds, so
/// reading the text decides them with certainty. The forms that depend on catalog
/// facts - a named `CLUSTER`, `REINDEX` over a partitioned target, `DROP SUBSCRIPTION`
/// holding a replication slot - cannot be decided from text at all, and refusing them
/// on suspicion would block a VALID rollback at the moment an operator most needs one.
/// Failing closed is right for corruption risk; it is wrong for tool availability.
///
/// `ALTER TYPE ... ADD VALUE` is deliberately absent: PostgreSQL 12 and later run it
/// inside a transaction, so refusing it would reject downs that work.
///
/// Cluster-wide statements are not this function's business either. The line-1 guard
/// runs over the same `down` before the `BEGIN` and denies `ALTER SYSTEM`
/// (`zero_migrate_guard::guard`, `rule::ALTER_SYSTEM`) and `CREATE`/`DROP DATABASE`
/// (`rule::DATABASE_MANAGEMENT`) already.
///
/// # Unparseable SQL
///
/// Returns `None`, which is not a fail-open: the guard parses this same text and
/// errors on a syntax failure, and it runs as gate (5c) immediately after the gate
/// this feeds. An unparseable `down` is refused there, by the component that owns
/// that judgement.
pub(crate) fn non_transactional_down_reason(m: &Migration) -> Option<String> {
    let down = m.down.as_deref()?;
    let parsed = pg_query::parse(down).ok()?;
    for raw_stmt in &parsed.protobuf.stmts {
        let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
            continue;
        };
        let offender = match node {
            NodeEnum::IndexStmt(idx) if idx.concurrent => "CREATE INDEX CONCURRENTLY",
            NodeEnum::DropStmt(drop) if drop.concurrent => "DROP INDEX CONCURRENTLY",
            NodeEnum::ReindexStmt(r) if reindex_is_concurrent(r) => "REINDEX CONCURRENTLY",
            _ => continue,
        };
        return Some(format!(
            "its `down` runs `{offender}`, which PostgreSQL refuses inside a transaction \
             block, and every `down` runs inside one. Roll forward with a compensating \
             migration instead"
        ));
    }
    None
}

/// `REINDEX ... CONCURRENTLY` carries its flag as a `DefElem` in `params`, unlike
/// `CREATE INDEX` and `DROP INDEX` which carry a struct field.
fn reindex_is_concurrent(r: &pg_query::protobuf::ReindexStmt) -> bool {
    r.params.iter().any(|node| {
        matches!(
            node.node.as_ref(),
            Some(NodeEnum::DefElem(d)) if d.defname.eq_ignore_ascii_case("concurrently")
        )
    })
}

/// A violation is rejected with [`ApplyError::NonIdempotentNonTxn`].
pub(crate) fn validate_non_txn_idempotent(m: &Migration) -> Result<(), ApplyError> {
    let parsed = pg_query::parse(&m.up).map_err(|e| ApplyError::NonIdempotentNonTxn {
        version: m.version.as_str().to_string(),
        reason: format!("could not parse `up` SQL: {e}"),
    })?;
    for raw_stmt in &parsed.protobuf.stmts {
        let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
            continue;
        };
        match node {
            NodeEnum::IndexStmt(idx) if idx.concurrent && !idx.if_not_exists => {
                return Err(ApplyError::NonIdempotentNonTxn {
                    version: m.version.as_str().to_string(),
                    reason: format!(
                        "`CREATE INDEX CONCURRENTLY {}` lacks `IF NOT EXISTS`",
                        if idx.idxname.is_empty() {
                            "<unnamed>"
                        } else {
                            &idx.idxname
                        }
                    ),
                });
            }
            NodeEnum::AlterEnumStmt(e) if !e.new_val.is_empty() && !e.skip_if_new_val_exists => {
                return Err(ApplyError::NonIdempotentNonTxn {
                    version: m.version.as_str().to_string(),
                    reason: format!(
                        "`ALTER TYPE … ADD VALUE '{}'` lacks `IF NOT EXISTS`",
                        e.new_val
                    ),
                });
            }
            // Bare DML re-applies on success-then-crash recovery (the up is
            // re-run verbatim). Forbid it on the non-txn path — DML belongs in a
            // transactional migration where a crash rolls it back atomically.
            NodeEnum::InsertStmt(_)
            | NodeEnum::UpdateStmt(_)
            | NodeEnum::DeleteStmt(_)
            | NodeEnum::MergeStmt(_)
            | NodeEnum::TruncateStmt(_) => {
                return Err(ApplyError::NonIdempotentNonTxn {
                    version: m.version.as_str().to_string(),
                    reason: format!(
                        "`{}` is data-manipulation (DML), which crash-recovery would re-run \
                         verbatim and double-apply. Run DML in a transactional migration \
                         (do not set `transaction:false`).",
                        dml_keyword(node)
                    ),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// The SQL keyword for a DML node, for the `NonIdempotentNonTxn` reason message.
const fn dml_keyword(node: &NodeEnum) -> &'static str {
    match node {
        NodeEnum::InsertStmt(_) => "INSERT",
        NodeEnum::UpdateStmt(_) => "UPDATE",
        NodeEnum::DeleteStmt(_) => "DELETE",
        NodeEnum::MergeStmt(_) => "MERGE",
        NodeEnum::TruncateStmt(_) => "TRUNCATE",
        _ => "DML",
    }
}

/// Transactional apply: `BEGIN; <up>; INSERT journal; COMMIT`.
/// DDL + journal are atomic — a failure rolls back leaving no partial DDL and
/// no journal row.
///
/// `kind` is the journaled `kind` to stamp on the `completed` event: `'apply'` for
/// an ordinary once-only migration, `'squash'` for a fresh-path squash (non-empty
/// `supersedes`), or `'repeatable'` for a re-applied repeatable. The
/// caller passes it explicitly — the journaled kind is the tamper anchor, so it is
/// never inferred from anything the migration set supplies at apply time. A debug
/// assertion ties `'squash'` ⇔ non-empty `supersedes`.
#[cfg(pg_seam)]
pub(crate) async fn apply_transactional<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
    supersedes: &[&str],
    kind: &str,
) -> Result<(), ApplyError> {
    let started = Instant::now();
    // Render the fail-closed engine-identifier quote seams BEFORE `BEGIN`.
    // These are pure functions of `cfg`/`m` with no dependency on the open txn, so
    // computing them up front means a fail-closed `IdentQuoteError` returns before
    // any transaction is opened — no dangling txn left behind on the `?` path. The
    // rendered SQL still EXECUTES inside the txn below, exactly as before.
    let session_sql = set_local_session_sql(cfg, m)?;
    let role_sql = set_local_role_sql(cfg)?;
    if let Some(probe) = &m.existence_guard {
        authorize_existence_guard_schema(cfg, m, probe.schema())?;
    }

    // `transaction()` needs `&mut Client`; the apply flow owns the connection,
    // so we take a short-lived mutable borrow via a raw pointer-free path:
    // callers pass `&Client`, so we cannot call `transaction()` directly. We
    // instead drive BEGIN/COMMIT/ROLLBACK explicitly over the shared `&Client`
    // (still one physical session, still atomic) — this avoids requiring
    // `&mut` plumbing through the whole apply loop.
    conn.batch("BEGIN").await?;

    // Pin search_path + the mandatory timeouts (per-migration override applied)
    // with SET LOCAL so they are scoped to THIS transaction and vanish at
    // COMMIT/ROLLBACK — nothing leaks onto the session. This runs as
    // the admin (always permitted); the role switch is applied separately around
    // the `<up>` only.
    if let Err(e) = conn.batch(&session_sql).await {
        let _ = conn.batch("ROLLBACK").await;
        return Err(ApplyError::Db(e.into()));
    }

    // **Existence-guard catalog probe (no TOCTOU).** If this migration
    // carries an existence-guard probe, read the LIVE catalog as the ADMIN
    // (`snapshot_schema` is a privileged catalog read; the migrator role is assumed
    // only AFTER the decision) inside THIS already-open transaction, under the
    // project advisory lock the whole plan already holds — so no lock is acquired or
    // released across probe→decide→act and there is no window for the catalog to
    // change between the verdict and the action. `decide` is pure Rust over the
    // snapshot — never a SQL-level conditional.
    //
    // - `RunBare`       → fall through: SET LOCAL ROLE + run `up` + journal (normal).
    // - `SatisfiedNoop` → SKIP the `up` AND the role switch, but STILL journal the
    //                     `completed` row so the version LANDS (a re-deploy sees it
    //                     net-applied and skips it via pending computation).
    // - `FailDrift`     → ROLLBACK + a typed `ExistenceGuardDrift` error (never a
    //                     silent skip over a divergence).
    let mut skip_up = false;
    if let Some(probe) = &m.existence_guard {
        let live = match crate::apply::drift::snapshot_schema_for(conn, probe.schema()).await {
            Ok(s) => s,
            Err(e) => {
                let _ = conn.batch("ROLLBACK").await;
                // Reuse the same DriftError → ApplyError mapping `apply_locked` uses.
                return Err(match e {
                    crate::apply::drift::DriftError::Db(db) => ApplyError::Db(db),
                    crate::apply::drift::DriftError::Journal(j) => ApplyError::Journal(j),
                    crate::apply::drift::DriftError::Snapshot(s) => ApplyError::Backend(s),
                    crate::apply::drift::DriftError::Backend(b) => ApplyError::Backend(b),
                });
            }
        };
        match crate::render::existence_probe::decide(
            probe,
            &live,
            crate::schema::query::SqlDialect::Postgres,
        ) {
            crate::render::existence_probe::GuardVerdict::RunBare => { /* fall through */ }
            crate::render::existence_probe::GuardVerdict::SatisfiedNoop => {
                // Skip the `up` + the role switch; the journal block below still runs
                // so the version lands as net-applied.
                skip_up = true;
            }
            crate::render::existence_probe::GuardVerdict::FailDrift(d) => {
                if let Err(rb) = conn.batch("ROLLBACK").await {
                    tracing::warn!(error = %rb, version = %m.version.as_str(), "zero-migrate: ROLLBACK failed after an existence-guard drift");
                }
                return Err(ApplyError::ExistenceGuardDrift {
                    version: m.version.as_str().to_string(),
                    object: d.object,
                    field: d.field,
                    expected: d.expected,
                    actual: d.actual,
                });
            }
        }
    }

    // Drop to the least-privilege migrator role for the duration of the
    // `<up>` ONLY. `SET LOCAL ROLE` is transaction-scoped, so the role switch is
    // confined to this txn; we explicitly `RESET ROLE` (below) before the journal
    // INSERT so the journal write runs as the admin — the migrator's journal
    // grant is revoked (role.rs), so it could not write the journal even if it
    // tried. The up's DDL is thereby confined to the migrator's least privileges
    // while the journal stays unforgeable by the migration.
    // On a `SatisfiedNoop` verdict (`skip_up`) the role switch + `<up>` + RESET ROLE
    // are all skipped — the object already has the declared shape (ifNotExists) or is
    // already absent (ifExists), so there is nothing to run; only the journal row
    // below lands so the version is recorded net-applied.
    if !skip_up {
        if let Some(set_role) = &role_sql {
            if let Err(e) = conn.batch(set_role.as_str()).await {
                let _ = conn.batch("ROLLBACK").await;
                return Err(ApplyError::Db(e.into()));
            }
        }

        // Run the migration's up SQL (as the migrator, if a role is configured).
        if let Err(e) = conn.batch(&m.up).await {
            // Roll back; report the failure. No journal row was written.
            if let Err(rb) = conn.batch("ROLLBACK").await {
                tracing::warn!(error = %rb, version = %m.version.as_str(), "zero-migrate: ROLLBACK failed after a migration error");
            }
            return Err(ApplyError::MigrationFailed {
                version: m.version.as_str().to_string(),
                source: e.into(),
            });
        }

        // RESET ROLE back to the admin — still INSIDE the transaction — so the
        // journal INSERT below runs as the admin (the migrator cannot write the
        // journal). `RESET ROLE` mid-transaction is supported and does not end the
        // txn, so atomicity of `<up>` + journal is preserved.
        if cfg.pg.migrator_role.is_some() {
            if let Err(e) = conn.batch("RESET ROLE").await {
                let _ = conn.batch("ROLLBACK").await;
                return Err(ApplyError::Db(e.into()));
            }
        }

        // The `up` has run and the `completed` row has not landed - the boundary a
        // crash test arms to compare the two apply shapes. Here it is still inside
        // the open transaction, so the ROLLBACK undoes the `up` too and the next
        // apply sees a migration that never ran.
        if let Err(e) = crate::fault::trip(crate::fault::points::APPLY_AFTER_UP_BEFORE_COMPLETED) {
            if let Err(rb) = conn.batch("ROLLBACK").await {
                tracing::warn!(error = %rb, version = %m.version.as_str(), "zero-migrate: ROLLBACK failed after an injected crash");
            }
            return Err(e);
        }
    }

    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // Journal the completed row in the SAME transaction, as the admin. The `kind`
    // is passed by the caller (the journaled kind is the tamper anchor — never
    // inferred from the supplied set): `'apply'` for an ordinary migration,
    // `'squash'` for a fresh-path squash (non-empty `supersedes`, so its
    // supersession edges are honored by `journal::superseded_versions`, which filters
    // on `kind='squash'`), `'repeatable'` for a re-applied repeatable.
    debug_assert_eq!(
        kind == "squash",
        !supersedes.is_empty(),
        "kind='squash' iff supersedes is non-empty"
    );
    let meta = crate::render::dml::quote_ident_checked(&cfg.pg.meta_schema)?;
    if let Err(e) = conn
        .exec(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind, down)
                 VALUES ('{applied}', $1, $2, $3, $4, $5, 'completed', 'success', $6, $7)",
                applied = journal::EventKind::Applied.as_str()
            ),
            &[
                m.version.as_str().into(),
                (&m.name).into(),
                m.checksum.as_str().into(),
                applied_by.into(),
                exec_ms.into(),
                kind.into(),
                (&m.down).into(),
            ],
        )
        .await
    {
        if let Err(rb) = conn.batch("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zero-migrate: ROLLBACK failed after a journal-insert error");
        }
        return Err(ApplyError::Journal(JournalError::Db(e.into())));
    }

    // Write the fresh-DB squash supersession edges in the SAME transaction
    // as the `completed` row above (admin). Edges-last-but-same-txn — so `S`'s
    // net-applied state and its full edge set commit atomically. A failure here
    // rolls back the entire apply (no `completed` row, no edges).
    if let Err(e) = insert_supersedes_edges(conn, cfg, m.version.as_str(), supersedes).await {
        if let Err(rb) = conn.batch("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zero-migrate: ROLLBACK failed after a supersedes-edge error");
        }
        return Err(ApplyError::Journal(e));
    }

    conn.batch("COMMIT").await?;
    Ok(())
}

fn postgres_dml_params(
    binds: &[crate::render::step::BindValue],
) -> Result<Vec<Option<String>>, String> {
    binds
        .iter()
        .map(|bind| match bind {
            crate::render::step::BindValue::Null => Ok(None),
            crate::render::step::BindValue::Bool(value) => Ok(Some(if *value {
                "true".to_string()
            } else {
                "false".to_string()
            })),
            crate::render::step::BindValue::Int(value) => Ok(Some(value.to_string())),
            crate::render::step::BindValue::Decimal(value)
            | crate::render::step::BindValue::Text(value) => Ok(Some(value.clone())),
            crate::render::step::BindValue::Bytes(_) => Err(
                "postgres DML: raw binary bind reached the backend without a decode wrapper"
                    .to_string(),
            ),
        })
        .collect()
}

/// Transactional apply of a single **parameterized DML** step (`op.*` DSL)
/// — the PG executor behind
/// [`MigrationBackend::run_dml_step`](crate::apply::backend::MigrationBackend::run_dml_step).
///
/// Mirrors [`apply_transactional`]'s `BEGIN; SET LOCAL …; SET LOCAL ROLE; <stmt>;
/// RESET ROLE; INSERT journal; COMMIT` discipline, but the statement is the DML
/// `template` executed with `binds` bound **natively** as `$n` parameters (never
/// interpolated). The step journals a `completed` event under `version`/`name`
/// with a checksum over the template, so a re-run is a net-applied-skip
/// (idempotency is the caller's `applied()` pre-check). DDL + journal are atomic.
///
/// # Errors
/// [`ApplyError::MigrationFailed`] if the DML failed (rolled back, nothing
/// journaled); [`ApplyError::Db`]/[`ApplyError::Journal`] on infrastructure
/// failure.
#[cfg(pg_seam)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_dml_transactional<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    version: &str,
    checksum: &crate::model::migration::Checksum,
    name: &str,
    template: &str,
    binds: &[crate::render::step::BindValue],
    applied_by: &str,
) -> Result<(), ApplyError> {
    let started = Instant::now();
    // Materialize each typed bind to its **text representation** for NULL-aware,
    // text-format binding. Every value is sent in PG text format with a
    // server-INFERRED parameter type (no fixed OID), so it implicit-casts to the
    // target COLUMN type exactly as a quoted literal would — `'2026-01-01'` →
    // `timestamptz`, `'1.5'` → `numeric`, `'t'`/`'f'`/`'true'` → `boolean`, a uuid
    // string → `uuid`. This is the schema-blind coercion model the op.* assembler
    // (names-are-strings) needs: a concrete-typed binary bind (`text`/`int8`/
    // `bool` OID) would make PG REFUSE a value against a different column type
    // ("cannot bind text → timestamptz"). The value is STILL a native bind — never
    // interpolated into the SQL — so the bind-safety property holds (a metacharacter
    // value cannot alter the statement shape). `Null` → SQL NULL (no bytes).
    let params = postgres_dml_params(binds).map_err(ApplyError::Backend)?;

    // Render the fail-closed engine-identifier quote seams BEFORE `BEGIN`,
    // so a fail-closed `IdentQuoteError` returns before any transaction is opened
    // (no dangling txn). Both are pure functions of `cfg` — no dependency on the
    // open txn. The rendered SQL still EXECUTES inside the txn below, as before.
    // `set_local` is built from cfg directly (a DML step has no per-migration
    // timeout override slot).
    let set_local = dml_set_local_session_sql(cfg, version)?;
    let role_sql = set_local_role_sql(cfg)?;

    conn.batch("BEGIN").await?;
    // SET LOCAL search_path + the mandatory timeouts, txn-scoped (vanish at
    // COMMIT/ROLLBACK).
    if let Err(e) = conn.batch(&set_local).await {
        let _ = conn.batch("ROLLBACK").await;
        return Err(ApplyError::Db(e.into()));
    }
    // Drop to the migrator role for the DML ONLY (least-privilege confinement); RESET
    // before the journal write so the journal stays unforgeable by the step.
    if let Some(set_role) = &role_sql {
        if let Err(e) = conn.batch(set_role.as_str()).await {
            let _ = conn.batch("ROLLBACK").await;
            return Err(ApplyError::Db(e.into()));
        }
    }
    if let Err(e) = conn.exec_text(template, &params).await {
        if let Err(rb) = conn.batch("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %version, "zero-migrate: ROLLBACK failed after a DML error");
        }
        return Err(ApplyError::MigrationFailed {
            version: version.to_string(),
            source: e.into(),
        });
    }
    if cfg.pg.migrator_role.is_some() {
        if let Err(e) = conn.batch("RESET ROLE").await {
            let _ = conn.batch("ROLLBACK").await;
            return Err(ApplyError::Db(e.into()));
        }
    }
    // Fault seam (test-only): a simulated crash AFTER the DML statement ran but
    // BEFORE the journal row — the open txn rolls back the data write too, so the
    // step left NOTHING (resume re-applies cleanly).
    if let Err(e) = crate::fault::trip(crate::fault::points::DML_AFTER_STMT_BEFORE_JOURNAL) {
        let _ = conn.batch("ROLLBACK").await;
        return Err(e);
    }
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let meta = crate::render::dml::quote_ident_checked(&cfg.pg.meta_schema)?;
    if let Err(e) = conn
        .exec(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind)
                 VALUES ('{applied}', $1, $2, $3, $4, $5, 'completed', 'success', 'apply')",
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
        .await
    {
        if let Err(rb) = conn.batch("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %version, "zero-migrate: ROLLBACK failed after a DML journal-insert error");
        }
        return Err(ApplyError::Journal(JournalError::Db(e.into())));
    }
    // Fault seam (test-only): a simulated crash AFTER the journal INSERT but
    // BEFORE COMMIT — the INSERT is inside the uncommitted txn, so it rolls back
    // with the data write; the step still left NOTHING (resume re-applies).
    if let Err(e) = crate::fault::trip(crate::fault::points::DML_AFTER_JOURNAL_BEFORE_COMMIT) {
        let _ = conn.batch("ROLLBACK").await;
        return Err(e);
    }
    conn.batch("COMMIT").await?;
    Ok(())
}

/// Insert the `S → v_i` supersession edges for a squash whose `up` RAN this batch
/// (fresh path). Each `conn.execute` participates in whatever transaction
/// the caller has open — the txn apply path calls this INSIDE its `BEGIN…COMMIT`
/// so the edges are atomic with `S`'s `completed` row. No-op for a non-squash
/// (`supersedes` empty). Admin write (the migrator has no meta-schema grant).
#[cfg(pg_seam)]
async fn insert_supersedes_edges<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    squash_version: &str,
    supersedes: &[&str],
) -> Result<(), JournalError> {
    let meta = crate::render::dml::quote_ident_checked(&cfg.pg.meta_schema)?;
    for sup in supersedes {
        conn.exec(
            &format!(
                "INSERT INTO {meta}.schema_migrations_supersedes
                     (squash_version, superseded_version)
                 VALUES ($1, $2)"
            ),
            &[squash_version.into(), sup.into()],
        )
        .await
        .map_err(|e| JournalError::Db(e.into()))?;
    }
    Ok(())
}

/// Non-transactional apply: two-phase with a `started`
/// marker, plus the idempotent recovery path.
///
/// Returns `true` if this was a recovery (a prior `started` marker existed).
#[cfg(pg_seam)]
pub(crate) async fn apply_non_transactional<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
    had_inflight: bool,
    supersedes: &[&str],
) -> Result<bool, ApplyError> {
    let version = m.version.as_str();

    // Existence-guard catalog probe, under the held project lock. The no-TOCTOU
    // guarantee comes from the plan lock, not from being inside the transactional
    // apply path, so the two-phase path honors the same probe before it writes an
    // inflight marker or runs the bare `up`.
    if let Some(probe) = &m.existence_guard {
        authorize_existence_guard_schema(cfg, m, probe.schema())?;
        let probe_started = Instant::now();
        let live = match crate::apply::drift::snapshot_schema_for(conn, probe.schema()).await {
            Ok(s) => s,
            Err(e) => {
                return Err(match e {
                    crate::apply::drift::DriftError::Db(db) => ApplyError::Db(db),
                    crate::apply::drift::DriftError::Journal(j) => ApplyError::Journal(j),
                    crate::apply::drift::DriftError::Snapshot(s) => ApplyError::Backend(s),
                    crate::apply::drift::DriftError::Backend(b) => ApplyError::Backend(b),
                });
            }
        };
        match crate::render::existence_probe::decide(
            probe,
            &live,
            crate::schema::query::SqlDialect::Postgres,
        ) {
            crate::render::existence_probe::GuardVerdict::RunBare => { /* continue below */ }
            crate::render::existence_probe::GuardVerdict::SatisfiedNoop => {
                let exec_ms =
                    i64::try_from(probe_started.elapsed().as_millis()).unwrap_or(i64::MAX);
                finalize_non_txn(conn, cfg, m, applied_by, exec_ms, supersedes).await?;
                return Ok(false);
            }
            crate::render::existence_probe::GuardVerdict::FailDrift(d) => {
                return Err(ApplyError::ExistenceGuardDrift {
                    version: version.to_string(),
                    object: d.object,
                    field: d.field,
                    expected: d.expected,
                    actual: d.actual,
                });
            }
        }
    }

    // Journal / inflight I/O runs as the ADMIN: the migrator's grant on the
    // meta schema is revoked, so `record_started` and the `finalize_non_txn`
    // transaction that deletes the marker must NOT run under `SET ROLE migrator`.
    // Only the `<up>` (and the recovery `DROP INDEX`, which the migrator owns)
    // runs as the migrator.
    if had_inflight {
        // Recovery path: a prior run wrote `started` then crashed before
        // `completed`, so the `up` may or may not have committed and the journal
        // cannot say which. Re-running it verbatim is only sound for the `up`s
        // `check_non_txn_up_replayable` admits; for every other one, refuse with the
        // marker left exactly where it is. The marker is the evidence that this
        // version half-ran, so a refusal that consumed it would cost the operator
        // the one thing telling them to look.
        if let Err(reason) = check_non_txn_up_replayable(&m.up) {
            return Err(ApplyError::NonTxnRecoveryUnsafe {
                version: version.to_string(),
                reason,
                meta_schema: cfg.pg.meta_schema.clone(),
            });
        }
        // Runs entirely as the ADMIN (called BEFORE the `<up>`'s `SET ROLE`): the
        // admin is privileged over the project schema, so the INVALID-index DROP
        // needs no migrator role.
        recover_non_transactional(conn, cfg, m).await?;
    }
    // Arm the `started` marker BEFORE the `<up>` runs. `ON CONFLICT DO NOTHING`,
    // so on the recovery path - where the marker is still armed from the attempt
    // that crashed - this writes nothing and the original marker survives. Keeping
    // one continuously-armed marker rather than clearing and re-arming it removes
    // the window where a crash between the two left the version looking fresh:
    // the next apply would then skip the INVALID-index cleanup, and an interrupted
    // `CREATE INDEX CONCURRENTLY` left INVALID would satisfy `IF NOT EXISTS` and
    // never be rebuilt. Runs as admin (still before the `SET ROLE`).
    journal::record_started(conn, cfg, version, &m.name, m.checksum.as_str(), applied_by).await?;

    let started = Instant::now();
    // Bracket the `<up>` with SET ROLE / RESET ROLE so the migration's DDL
    // runs under least-privilege confinement, but the journal writes above/below run as
    // admin. `RESET ROLE` runs on ALL exit paths (including the error path) so
    // the role never leaks onto the session even if the `<up>` fails — and
    // `apply`'s `restore_session` is an unconditional backstop.
    if let Some(role) = &cfg.pg.migrator_role {
        let role_q = crate::render::dml::quote_ident_checked(role)?;
        conn.batch(&format!("SET ROLE {role_q}")).await?;
    }
    let up_result = conn.batch(&m.up).await;
    if cfg.pg.migrator_role.is_some() {
        // RESET ROLE regardless of the up's success, so the journal writes below
        // run as admin and no role leaks onto the session.
        if let Err(e) = conn.batch("RESET ROLE").await {
            // If RESET ROLE itself fails, surface it (apply's restore_session is
            // the unconditional backstop). Prefer surfacing the up's error if it failed.
            if up_result.is_ok() {
                return Err(ApplyError::Db(e.into()));
            }
        }
    }
    up_result.map_err(|e| ApplyError::MigrationFailed {
        version: version.to_string(),
        source: e.into(),
    })?;
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // The same boundary as the transactional path, with the opposite consequence:
    // the `up` auto-committed, so an abort here leaves the schema changed, no
    // `completed` row, and the inflight marker armed. That is the exact state the
    // next apply's recovery has to converge from.
    crate::fault::trip(crate::fault::points::APPLY_AFTER_UP_BEFORE_COMPLETED)?;

    // Phase 2: the completed row, the marker's deletion, and any squash edges.
    finalize_non_txn(conn, cfg, m, applied_by, exec_ms, supersedes).await?;

    Ok(had_inflight)
}

/// Land a two-phase apply's journal state: the immutable `completed` row, the
/// inflight marker's deletion, and any fresh-path squash edges, in ONE transaction.
///
/// The `<up>` has already auto-committed - that is what `transaction:false` means -
/// so the journal writes are the only part of the migration that can still be made
/// atomic, and each pair of them has a failure mode when it is not.
///
/// A `completed` row that outlives its marker deletion sends the NEXT apply into
/// recovery for a version the journal already reports as landed, which for an `up`
/// outside the replay-safe set is a refusal on a migration that is actually
/// finished. A `completed` row that outlives its edges leaves a squash net-applied
/// with `v1..vN` back in pending, re-running on top of the squash that replaced
/// them.
///
/// Runs as the admin: the migrator's meta-schema grant is revoked.
#[cfg(pg_seam)]
async fn finalize_non_txn<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
    exec_ms: i64,
    supersedes: &[&str],
) -> Result<(), ApplyError> {
    let version = m.version.as_str();
    // A fresh-path squash is stamped `kind='squash'` so its edges are honored by
    // `superseded_versions`, which filters on that kind.
    let kind = if supersedes.is_empty() {
        "apply"
    } else {
        "squash"
    };
    conn.batch("BEGIN").await?;
    let finalize = async {
        journal::record_completed(
            conn,
            cfg,
            journal::CompletedRecord {
                version,
                name: &m.name,
                checksum: m.checksum.as_str(),
                applied_by,
                exec_ms,
                kind,
                down: m.down.as_deref(),
            },
        )
        .await?;
        insert_supersedes_edges(conn, cfg, version, supersedes).await
    }
    .await;
    if let Err(e) = finalize {
        if let Err(rb) = conn.batch("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %version, "zero-migrate: ROLLBACK failed after a non-txn journal finalize error");
        }
        return Err(ApplyError::Journal(e));
    }
    conn.batch("COMMIT").await?;
    Ok(())
}

/// Prepare a crashed non-transactional migration for the caller's verbatim re-run
/// of its `<up>`.
///
/// A `started` marker with no `completed` row means the prior attempt may have
/// (a) failed mid-DDL, or (b) **succeeded then crashed** before recording
/// `completed`, and nothing in the journal distinguishes them. Reaching here at
/// all means [`check_non_txn_up_replayable`] has already admitted the `up`, so
/// both cases converge on a second run of it. An `up` it could not admit never
/// gets here - the caller refuses with the marker intact rather than re-running
/// a statement that may already have committed.
///
/// This performs the ONE cleanup an admitted `up` cannot do for itself: dropping
/// the `INVALID` index residue of an interrupted CONCURRENTLY build, which
/// satisfies `IF NOT EXISTS` and would otherwise never be rebuilt.
///
/// It deliberately does NOT clear the marker. Clearing it here and re-arming it
/// after the re-run opened a window where a crash in between erased the only
/// record that the version half-ran, and the next apply would then treat it as a
/// fresh one and skip this cleanup. The marker stays armed from the first attempt
/// until the `completed` row deletes it in the same transaction.
///
/// Runs as the **admin**: it is called BEFORE the `<up>`'s `SET ROLE`, and the
/// admin is privileged over the project schema, so the `DROP INDEX` succeeds
/// without the migrator role.
#[cfg(pg_seam)]
async fn recover_non_transactional<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
) -> Result<(), ApplyError> {
    // Drop the INVALID residue of an interrupted CONCURRENTLY build — but ONLY
    // the index(es) this migration's `up` names (scoped by design). An INVALID
    // index satisfies `IF NOT EXISTS`, so the caller's re-run of `<up>` would
    // never rebuild it; we must drop it first. We parse the `CREATE INDEX … name`
    // out of the `up` and drop *that* name (if it is currently invalid), rather
    // than every invalid index in the schema.
    //
    // Why scoped: an OUT-OF-BAND invalid index (a manual CONCURRENTLY build the
    // operator is running elsewhere in the project schema) must NOT be collateral
    // damage of recovering an unrelated migration. The per-project advisory lock
    // serializes the engine's own applies, but it does not stop a human's manual
    // session, so scoping is the correct fix.
    for idx in index_names_in_up(&m.up) {
        // Only drop it if it is currently INVALID — a valid index named here means
        // the prior attempt actually succeeded (case (b): completed then crashed
        // before journaling), and the re-run of the idempotent `up`'s
        // `IF NOT EXISTS` will correctly no-op over it. Dropping a valid index
        // would needlessly rebuild it.
        let is_invalid: bool = conn
            .query_one(
                "SELECT EXISTS (
                     SELECT 1
                       FROM pg_index x
                       JOIN pg_class c ON c.oid = x.indexrelid
                       JOIN pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = $1 AND c.relname = $2 AND x.indisvalid = false
                 ) AS invalid",
                &[(&cfg.project_schema).into(), (&idx).into()],
            )
            .await?
            .try_get("invalid")?;
        if is_invalid {
            let stmt = format!(
                "DROP INDEX IF EXISTS {}.{}",
                crate::render::dml::quote_ident_checked(&cfg.project_schema)?,
                crate::render::dml::quote_ident_checked(&idx)?,
            );
            conn.batch(&stmt).await?;
        }
    }

    Ok(())
}

/// May recovery re-run this non-transactional `up` verbatim after a crash, and if
/// not, what does the operator need to be told?
///
/// Recovery re-issues the WHOLE `up` batch through one `conn.batch`, which is
/// simple-query on every shipped host, so an `up` is admitted on two counts rather
/// than one.
///
/// The statement must tolerate having already run. That is what an interrupted
/// two-phase apply leaves behind: the DDL auto-committed and the `completed` row
/// did not, and no journal state tells the two apart.
///
/// And the batch must hold exactly one substantive statement, because statements
/// that are individually re-runnable are not re-runnable in composition:
/// `DROP TABLE IF EXISTS t; CREATE TABLE IF NOT EXISTS t (...)` runs clean the first
/// time and deletes every row written since the migration landed on the second.
///
/// The single-statement rule costs almost nothing measured against a live server:
/// PostgreSQL runs a multi-statement simple query inside one implicit transaction
/// block, and `CREATE INDEX CONCURRENTLY`, `DROP INDEX CONCURRENTLY` and `VACUUM`
/// all refuse to run in one, so three of the four shapes below cannot reach a
/// multi-statement `up` in the first place. `ALTER TYPE ... ADD VALUE` is the
/// exception: several of them in one `up` do apply, and after a crash they are
/// refused here rather than replayed.
///
/// The admitted set is small on purpose and is NOT the set the non-txn path
/// accepts - an `up` outside it applies normally and only loses AUTOMATIC
/// recovery. Adding to it means arguing that specific statement's replay case:
///
/// - `CREATE INDEX CONCURRENTLY IF NOT EXISTS <name>`, with an explicit name.
///   [`index_names_in_up`] skips an unnamed index, so recovery could not drop the
///   INVALID residue of one and the rebuild would silently never happen.
/// - `DROP INDEX CONCURRENTLY IF EXISTS`. The bare form errors on the second run.
/// - `ALTER TYPE ... ADD VALUE IF NOT EXISTS <label>`.
/// - `VACUUM`.
///
/// # Errors
/// The operator-facing reason, for [`ApplyError::NonTxnRecoveryUnsafe`].
fn check_non_txn_up_replayable(up: &str) -> Result<(), String> {
    let parsed = pg_query::parse(up).map_err(|e| format!("its `up` SQL does not parse: {e}"))?;
    let mut statements = parsed
        .protobuf
        .stmts
        .iter()
        .filter_map(|raw| raw.stmt.as_ref().and_then(|stmt| stmt.node.as_ref()));
    let Some(node) = statements.next() else {
        return Err("its `up` carries no statement to classify".to_string());
    };
    if statements.next().is_some() {
        return Err(
            "its `up` carries more than one statement and recovery re-runs the whole batch, \
             where statements that are each re-runnable alone need not be together"
                .to_string(),
        );
    }
    match node {
        NodeEnum::IndexStmt(idx)
            if idx.concurrent && idx.if_not_exists && !idx.idxname.is_empty() =>
        {
            Ok(())
        }
        NodeEnum::IndexStmt(idx) if idx.concurrent && idx.if_not_exists => Err(
            "`CREATE INDEX CONCURRENTLY IF NOT EXISTS` without an explicit index name leaves \
             recovery unable to name the INVALID residue it would have to drop"
                .to_string(),
        ),
        NodeEnum::IndexStmt(idx) if idx.concurrent => {
            Err("`CREATE INDEX CONCURRENTLY` without `IF NOT EXISTS`".to_string())
        }
        NodeEnum::DropStmt(drop)
            if drop.remove_type == ObjectType::ObjectIndex as i32
                && drop.concurrent
                && drop.missing_ok =>
        {
            Ok(())
        }
        NodeEnum::DropStmt(drop)
            if drop.remove_type == ObjectType::ObjectIndex as i32 && drop.concurrent =>
        {
            Err("`DROP INDEX CONCURRENTLY` without `IF EXISTS`".to_string())
        }
        NodeEnum::AlterEnumStmt(alter)
            if !alter.new_val.is_empty() && alter.skip_if_new_val_exists =>
        {
            Ok(())
        }
        NodeEnum::AlterEnumStmt(alter) if !alter.new_val.is_empty() => Err(format!(
            "`ALTER TYPE ... ADD VALUE '{}'` without `IF NOT EXISTS`",
            alter.new_val
        )),
        NodeEnum::VacuumStmt(_) => Ok(()),
        _ => Err(
            "its `up` is not one of the statements recovery can re-run: \
             CREATE INDEX CONCURRENTLY IF NOT EXISTS <name>, \
             DROP INDEX CONCURRENTLY IF EXISTS, \
             ALTER TYPE ... ADD VALUE IF NOT EXISTS, VACUUM"
                .to_string(),
        ),
    }
}

/// Parse the index name(s) created by `CREATE INDEX … name … ON …` statements in
/// a migration's `up`, via the real Postgres parser (so syntax we cannot parse
/// simply yields no names — recovery then drops nothing, which is the safe
/// default). Unnamed `CREATE INDEX` (no explicit name) is skipped: Postgres
/// derives the name, and recovery cannot target a name it does not know — the
/// non-txn idempotency rule already forbids unnamed `CONCURRENTLY` indirectly
/// (the author always emits a name; raw SQL must too to be re-runnable).
fn index_names_in_up(up: &str) -> Vec<String> {
    let Ok(parsed) = pg_query::parse(up) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for raw_stmt in &parsed.protobuf.stmts {
        if let Some(NodeEnum::IndexStmt(idx)) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref())
        {
            if !idx.idxname.is_empty() {
                names.push(idx.idxname.clone());
            }
        }
    }
    names
}

/// Roll back ONE migration transactionally: `BEGIN; SET LOCAL …; SET LOCAL ROLE
/// migrator; <down>; RESET ROLE; INSERT rolled_back (as admin); COMMIT`.
///
/// Atomic: the `down` + its `rolled_back` journal append commit together, so a
/// crash leaves either both (rolled back + recorded) or neither. The `down` runs
/// under the migrator role; the journal append runs as admin (the migrator has no
/// meta grant), exactly mirroring [`apply_transactional`].
#[cfg(pg_seam)]
pub(crate) async fn rollback_one_transactional<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
) -> Result<(), RollbackError> {
    // An irreversible migration is a REFUSAL, not a caller bug. The trait method is
    // public and advertises no `down is Some` precondition, and irreversible steps
    // are ordinary on the IR path (the expand backfill marker, drop-column contract,
    // primary-key and identity steps, every repeatable). Panicking here would abort
    // mid-batch, after earlier downs had already committed.
    let down = m
        .down
        .as_deref()
        .ok_or_else(|| RollbackError::Irreversible {
            version: m.version.as_str().to_string(),
            name: m.name.clone(),
        })?;

    // Line 1 over the `down`, for the same reason apply runs it over the `up`: this
    // is SQL from the migration file that is about to reach the database. Without
    // it, `down` is a way to run precisely what `up` is refused - an author whose
    // `up` is guard-denied could put the same statement in `down` and have it
    // execute the moment anything rolls back. The migrator role is line 2 below, not
    // a substitute.
    //
    // Guarding engine-synthesized SQL is not novel: the apply path already runs this
    // guard over `up`, which is equally synthesized on the IR path.
    crate::guard::guard_for(&cfg.guard_config().for_dialect(crate::SqlDialect::Postgres))
        .check(down)
        .map_err(|source| RollbackError::Guard {
            version: m.version.as_str().to_string(),
            source,
        })?;

    let started = Instant::now();
    // Render the fail-closed engine-identifier quote seams BEFORE `BEGIN`,
    // so a fail-closed `IdentQuoteError` returns before any transaction is opened
    // (no dangling txn). Both are pure functions of `cfg`/`m` — no dependency on
    // the open txn. The rendered SQL still EXECUTES inside the txn below, as before.
    let session_sql = set_local_session_sql(cfg, m)?;
    let role_sql = set_local_role_sql(cfg)?;

    conn.batch("BEGIN").await?;

    // Pin search_path + mandatory timeouts (SET LOCAL — vanish at COMMIT/ROLLBACK).
    if let Err(e) = conn.batch(&session_sql).await {
        let _ = conn.batch("ROLLBACK").await;
        return Err(RollbackError::Db(e.into()));
    }
    // Drop to the migrator role for the `<down>` ONLY (least-privilege confinement). RESET
    // ROLE before the journal append so the admin writes the rolled_back event.
    if let Some(set_role) = &role_sql {
        if let Err(e) = conn.batch(set_role.as_str()).await {
            let _ = conn.batch("ROLLBACK").await;
            return Err(RollbackError::Db(e.into()));
        }
    }
    // Run the `<down>` as the migrator.
    if let Err(e) = conn.batch(down).await {
        if let Err(rb) = conn.batch("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zero-migrate: ROLLBACK failed after a down error");
        }
        return Err(RollbackError::DownFailed {
            version: m.version.as_str().to_string(),
            source: e.into(),
        });
    }
    // RESET ROLE back to admin — still inside the txn — so the journal append runs
    // as the admin (the migrator cannot write the journal).
    if cfg.pg.migrator_role.is_some() {
        if let Err(e) = conn.batch("RESET ROLE").await {
            let _ = conn.batch("ROLLBACK").await;
            return Err(RollbackError::Db(e.into()));
        }
    }
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // Append the immutable `rolled_back` event in the SAME transaction, as admin.
    if let Err(error) = append_rolled_back(conn, cfg, m, applied_by, exec_ms).await {
        if let Err(rb) = conn.batch("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zero-migrate: ROLLBACK failed after a rolled_back-insert error");
        }
        return Err(error);
    }

    conn.batch("COMMIT").await?;
    Ok(())
}

async fn append_rolled_back<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    forward: &Migration,
    applied_by: &str,
    exec_ms: i64,
) -> Result<(), RollbackError> {
    let meta = crate::render::dml::quote_ident_checked(&cfg.pg.meta_schema)?;
    conn.exec(
        &format!(
            "INSERT INTO {meta}.schema_migrations
                 (event_kind, version, name, checksum, \"by\", exec_ms)
             VALUES ('{rolled_back}', $1, $2, $3, $4, $5)",
            rolled_back = journal::EventKind::RolledBack.as_str()
        ),
        &[
            forward.version.as_str().into(),
            (&forward.name).into(),
            forward.checksum.as_str().into(),
            applied_by.into(),
            exec_ms.into(),
        ],
    )
    .await
    .map_err(|error| RollbackError::Journal(JournalError::Db(error.into())))?;
    Ok(())
}

/// Run a lowered recorded inverse through PostgreSQL's native text-bind DML seam,
/// then journal the FORWARD identity as rolled back in the same transaction.
#[cfg(pg_seam)]
pub(crate) async fn rollback_dml_plan_transactional<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    forward: &Migration,
    inverse_steps: &[crate::render::step::PlanStep],
    applied_by: &str,
) -> Result<(), RollbackError> {
    let version = forward.version.as_str();
    let session_sql = dml_set_local_session_sql(cfg, version)
        .map_err(|error| RollbackError::Backend(error.to_string()))?;
    let role_sql =
        set_local_role_sql(cfg).map_err(|error| RollbackError::Backend(error.to_string()))?;
    let params: Vec<Vec<Option<String>>> = inverse_steps
        .iter()
        .map(|step| {
            let crate::render::step::PlanStep::Dml { binds, .. } = step else {
                return Err(RollbackError::RecordedInverseUnsupported {
                    version: version.to_string(),
                    reason: "non-DML step reached PostgreSQL recorded-inverse executor".to_string(),
                });
            };
            postgres_dml_params(binds).map_err(|reason| RollbackError::Backend(reason))
        })
        .collect::<Result<_, _>>()?;
    let started = Instant::now();

    conn.batch("BEGIN").await?;
    if let Err(error) = conn.batch(&session_sql).await {
        let _ = conn.batch("ROLLBACK").await;
        return Err(RollbackError::Db(error.into()));
    }
    if let Some(set_role) = &role_sql {
        if let Err(error) = conn.batch(set_role).await {
            let _ = conn.batch("ROLLBACK").await;
            return Err(RollbackError::Db(error.into()));
        }
    }

    for (step, params) in inverse_steps.iter().zip(&params) {
        let crate::render::step::PlanStep::Dml { template, .. } = step else {
            unreachable!("inverse shape was validated before BEGIN")
        };
        if let Err(error) = conn.exec_text(template, params).await {
            if let Err(rollback) = conn.batch("ROLLBACK").await {
                tracing::warn!(error = %rollback, version, "zero-migrate: ROLLBACK failed after inverse DML error");
            }
            return Err(RollbackError::DownFailed {
                version: version.to_string(),
                source: error.into(),
            });
        }
    }

    if cfg.pg.migrator_role.is_some() {
        if let Err(error) = conn.batch("RESET ROLE").await {
            let _ = conn.batch("ROLLBACK").await;
            return Err(RollbackError::Db(error.into()));
        }
    }
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    if let Err(error) = append_rolled_back(conn, cfg, forward, applied_by, exec_ms).await {
        let _ = conn.batch("ROLLBACK").await;
        return Err(error);
    }
    conn.batch("COMMIT").await?;
    Ok(())
}

#[cfg(test)]
mod pg_confinement_shape_tests {
    //! Pins the confinement shape: the **PG** apply leaf still emits its
    //! `SET LOCAL search_path` / `SET LOCAL ROLE` / `SET LOCAL statement_timeout`
    //! and `SET LOCAL lock_timeout` bracket from the
    //! [`PgConfinement`](crate::conn::PgConfinement) block (now grouped under
    //! `cfg.pg`, not flat on the neutral config), and a default (SQLite-shaped
    //! construction reuses this same `new`) carries the INERT PG confinement —
    //! never PG role/cross-schema confinement of its own.
    use super::*;
    use crate::model::migration::{Checksum, MigrationFlags, MigrationId};

    fn trivial_migration() -> Migration {
        let flags = MigrationFlags::default();
        let version = MigrationId::generate();
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: "CREATE TABLE t (id int)",
            down: None,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version,
            name: "n".into(),
            up: "CREATE TABLE t (id int)".into(),
            down: None,
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        }
    }

    /// PG confinement bracket is emitted from the grouped `cfg.pg` block:
    /// search_path = project schema
    /// (+ extension schema), and the mandatory timeouts in ms.
    ///
    /// This also pins the **lock-safety envelope** default: the DEFAULT
    /// `lock_timeout` (3000 ms) is SHORT and SEPARATE from the long-running
    /// `statement_timeout` (60000 ms). A regression that folded the two together
    /// (or restored the old long 30000 ms lock_timeout) flips this assertion RED.
    #[test]
    fn pg_confinement_bracket_is_emitted_from_the_pg_block() {
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"))
            .with_migrator_role("migrator_proj_x");
        let m = trivial_migration();

        let session = set_local_session_sql(&cfg, &m).expect("session sql renders");
        // search_path is the project schema, then the `public` extension schema
        // (the `new()` default) — pinned for confined resolution. The
        // statement_timeout is the long RUNNING budget (60s); the lock_timeout is
        // the SHORT lock-ACQUISITION budget (3s) — split by construction.
        assert_eq!(
            session,
            "SET LOCAL search_path TO \"proj_x\", \"public\"; \
             SET LOCAL statement_timeout = 60000; \
             SET LOCAL lock_timeout = 3000;",
            "PG SET LOCAL session bracket must come from cfg.pg byte-identically, \
             with a SHORT default lock_timeout split from the long statement_timeout"
        );

        // The lock-safety split is real: the short lock-acquisition budget must
        // be strictly shorter than the long running-statement budget — never the
        // same value (which would mean a blocking DDL waits the full statement
        // budget to acquire its lock, the outage this envelope prevents).
        assert!(
            cfg.lock_timeout_ms() < cfg.statement_timeout_ms(),
            "default lock_timeout ({} ms) must be strictly shorter than \
             statement_timeout ({} ms) — the lock-safety envelope",
            cfg.lock_timeout_ms(),
            cfg.statement_timeout_ms(),
        );

        // The migrator role bracket comes from cfg.pg.migrator_role.
        let role = set_local_role_sql(&cfg)
            .expect("role ident quotable")
            .expect("migrator role set");
        assert_eq!(role, "SET LOCAL ROLE \"migrator_proj_x\"");
    }

    #[test]
    fn structured_dml_pins_standard_string_literals_transaction_locally() {
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let session = dml_set_local_session_sql(&cfg, "mig_test").expect("DML session SQL renders");

        assert!(
            session.starts_with(AUTHOR_SQL_LITERAL_MODE),
            "the literal mode must be pinned before authored DML: {session}"
        );
        assert!(
            session.contains("SET LOCAL standard_conforming_strings = on;"),
            "the setting must be transaction-local so the inherited value is restored: {session}"
        );
    }

    /// The per-migration `lock_timeout_ms` override (the maintenance-window knob,
    /// mirroring `timeout_ms`) is honoured by the txn-path session render: a
    /// migration that sets its OWN lock budget renders THAT value, not the
    /// executor-wide default — while a migration that sets none falls back to the
    /// SHORT default. RED pre-change (the field did not exist and the render used
    /// `cfg.lock_timeout_ms()` unconditionally).
    #[test]
    fn per_migration_lock_timeout_override_renders_over_default() {
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        // Default budget (3000 ms) when the migration sets no override.
        let default_m = trivial_migration();
        let default_session = set_local_session_sql(&cfg, &default_m).expect("session sql renders");
        assert!(
            default_session.contains("SET LOCAL lock_timeout = 3000;"),
            "no override ⇒ the SHORT executor default (3000 ms); got: {default_session}"
        );

        // An explicit per-migration override raises ONLY this migration's budget.
        let mut override_m = trivial_migration();
        override_m.flags.lock_timeout_ms = Some(7000);
        let override_session =
            set_local_session_sql(&cfg, &override_m).expect("session sql renders");
        assert!(
            override_session.contains("SET LOCAL lock_timeout = 7000;"),
            "the per-migration lock_timeout_ms override (7000 ms) must win over the \
             3000 ms default; got: {override_session}"
        );
        // The statement_timeout is untouched by the lock override.
        assert!(
            override_session.contains("SET LOCAL statement_timeout = 60000;"),
            "the lock_timeout override must not perturb statement_timeout; got: {override_session}"
        );
    }

    /// A default-constructed config (the SHAPE the SQLite engine builds via
    /// `ExecutorConfig::new(app_id, app_id)`) carries NO migrator role — its
    /// PG confinement is inert; SQLite confines via its runtime authorizer
    /// mode-flip, never these PG params.
    #[test]
    fn sqlite_shaped_config_carries_no_pg_role_confinement() {
        // Exactly what crates/plugin-db sqlite_engine.rs constructs.
        let cfg = ExecutorConfig::new(
            "app_test",
            "app_test",
            crate::test_fixtures::no_inject("app_test"),
        );
        assert!(
            cfg.pg.migrator_role.is_none(),
            "a SQLite-shaped config must carry no PG migrator role (SET ROLE) — \
             it confines via the runtime authorizer mode-flip, not the PG bracket"
        );
        // And the PG-only role bracket is therefore absent.
        assert!(
            set_local_role_sql(&cfg)
                .expect("role ident quotable")
                .is_none(),
            "no SET LOCAL ROLE is emitted when the PG confinement carries no role"
        );
        // The neutral identity fields ARE populated (engine-agnostic).
        assert_eq!(cfg.project_id, "app_test");
        assert_eq!(cfg.project_schema, "app_test");
    }
}

#[cfg(test)]
mod non_transactional_down_tests {
    use super::*;
    use crate::model::migration::{Checksum, MigrationFlags, MigrationId};

    /// A transactional migration whose `down` is `sql`. Transactional on purpose: the
    /// flag is what gate (5b) already checked, so a fixture declaring `false` would
    /// pass this classifier's test while proving nothing about the text reading.
    fn with_down(sql: &str) -> Migration {
        let flags = MigrationFlags::default();
        let up = "CREATE TABLE t()";
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up,
            down: Some(sql),
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version: MigrationId::generate(),
            name: "n".into(),
            up: up.into(),
            down: Some(sql.into()),
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        }
    }

    #[test]
    fn the_concurrently_family_is_named_and_everything_else_passes() {
        // Each of the three carries its flag differently in the parse tree - two
        // struct fields and a DefElem - so all three are exercised rather than
        // assumed to follow the first.
        for (sql, expected) in [
            (
                "CREATE INDEX CONCURRENTLY i ON t (c)",
                "CREATE INDEX CONCURRENTLY",
            ),
            ("DROP INDEX CONCURRENTLY i", "DROP INDEX CONCURRENTLY"),
            ("REINDEX INDEX CONCURRENTLY i", "REINDEX CONCURRENTLY"),
        ] {
            let reason = non_transactional_down_reason(&with_down(sql))
                .unwrap_or_else(|| panic!("{sql} must be refused"));
            assert!(reason.contains(expected), "{sql}: {reason}");
            assert!(reason.contains("Roll forward"), "{sql}: {reason}");
        }

        // The non-concurrent spellings run inside a transaction perfectly well, so
        // refusing them would block valid rollbacks. This is the arm that fails if the
        // matcher ever keys on the statement kind instead of the CONCURRENTLY flag.
        for sql in [
            "DROP INDEX i",
            "CREATE INDEX i ON t (c)",
            "REINDEX INDEX i",
            "DROP TABLE t",
            "ALTER TABLE t DROP COLUMN c",
            // PostgreSQL 12 and later run this inside a transaction, so it must pass.
            "ALTER TYPE mood ADD VALUE 'sad'",
        ] {
            assert_eq!(
                non_transactional_down_reason(&with_down(sql)),
                None,
                "{sql} must not be refused"
            );
        }
    }

    #[test]
    fn an_absent_or_unparseable_down_raises_no_objection_here() {
        let mut irreversible = with_down("DROP TABLE t");
        irreversible.down = None;
        assert_eq!(non_transactional_down_reason(&irreversible), None);

        // Not a fail-open: the line-1 guard parses this same text as gate (5c) and
        // errors on a syntax failure, so an unparseable `down` is refused there.
        assert_eq!(
            non_transactional_down_reason(&with_down("this is not sql at all")),
            None
        );
    }
}

#[cfg(test)]
mod non_txn_idempotency_tests {
    use super::*;
    use crate::model::migration::{Checksum, MigrationFlags, MigrationId};

    /// Build a non-transactional migration whose `up` is `sql`.
    fn nontxn(sql: &str) -> Migration {
        let flags = MigrationFlags {
            transactional: false,
            ..MigrationFlags::default()
        };
        let version = MigrationId::generate();
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: sql,
            down: None,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version,
            name: "n".into(),
            up: sql.into(),
            down: None,
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        }
    }

    // REGRESSION (nontxn-dml-recovery-double-apply): a bare INSERT/UPDATE/
    // DELETE/MERGE/TRUNCATE on the non-txn two-phase path would be re-run
    // VERBATIM by crash recovery and double-apply on a success-then-crash.
    // Validation must reject every bare-DML non-txn `up`. Pre-fix these passed
    // (only CONCURRENTLY / ALTER TYPE ADD VALUE were fenced).
    #[test]
    fn bare_dml_on_non_txn_path_is_rejected() {
        for sql in [
            "INSERT INTO t (id) VALUES (1)",
            "UPDATE t SET x = 1 WHERE id = 1",
            "DELETE FROM t WHERE id = 1",
            "TRUNCATE t",
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DO NOTHING",
        ] {
            let m = nontxn(sql);
            let err = validate_non_txn_idempotent(&m)
                .expect_err(&format!("bare DML must be rejected on non-txn path: {sql}"));
            assert!(
                matches!(err, ApplyError::NonIdempotentNonTxn { .. }),
                "expected NonIdempotentNonTxn for `{sql}`, got {err:?}"
            );
        }
    }

    // A DML statement mixed in among DDL on the non-txn path is still caught.
    #[test]
    fn dml_mixed_with_ddl_on_non_txn_path_is_rejected() {
        let m = nontxn(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS i ON t (x);\n\
             INSERT INTO t (id) VALUES (1);",
        );
        let err = validate_non_txn_idempotent(&m).expect_err("DML among DDL must be rejected");
        assert!(
            matches!(err, ApplyError::NonIdempotentNonTxn { .. }),
            "got {err:?}"
        );
    }

    // The legitimate non-txn ops (idempotent CONCURRENTLY / ADD VALUE IF NOT
    // EXISTS, naturally-rerunnable DROP INDEX CONCURRENTLY / VACUUM) still pass —
    // the DML fence must not over-reject inherently-non-txn DDL.
    #[test]
    fn idempotent_non_txn_ddl_still_passes() {
        for sql in [
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS i ON t (x)",
            "DROP INDEX CONCURRENTLY IF EXISTS i",
            "ALTER TYPE mood ADD VALUE IF NOT EXISTS 'excited'",
            "VACUUM ANALYZE t",
        ] {
            let m = nontxn(sql);
            assert!(
                validate_non_txn_idempotent(&m).is_ok(),
                "idempotent non-txn op must pass: {sql}"
            );
        }
    }

    // The shapes recovery may re-run verbatim after a crash. Each one tolerates
    // having already run, and each is a single statement.
    #[test]
    fn the_replay_safe_set_is_admitted_for_recovery() {
        for sql in [
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS i ON t (x)",
            "DROP INDEX CONCURRENTLY IF EXISTS i",
            "ALTER TYPE mood ADD VALUE IF NOT EXISTS 'excited'",
            "VACUUM ANALYZE t",
            // Trailing semicolons and comments carry no statement of their own.
            "-- rebuild the tag index\nVACUUM t;",
        ] {
            assert!(
                check_non_txn_up_replayable(sql).is_ok(),
                "recovery must be able to re-run: {sql}"
            );
        }
    }

    // The near-misses of the admitted set. Each errors or double-applies on a
    // second run, so recovery refuses rather than replaying it.
    #[test]
    fn the_near_misses_of_the_replay_safe_set_are_refused() {
        for sql in [
            // Errors with `already exists` on the second run.
            "CREATE INDEX CONCURRENTLY i ON t (x)",
            // `index_names_in_up` skips an unnamed index, so recovery could not
            // drop the INVALID residue and the rebuild would never happen.
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS ON t (x)",
            // Errors with `does not exist` on the second run.
            "DROP INDEX CONCURRENTLY i",
            // Errors with `already exists` on the second run, and the message must
            // not accuse it of a missing CONCURRENTLY it never claimed.
            "CREATE INDEX i ON t (x)",
            // Errors with `label already exists` on the second run.
            "ALTER TYPE mood ADD VALUE 'excited'",
        ] {
            assert!(
                check_non_txn_up_replayable(sql).is_err(),
                "recovery must refuse to re-run: {sql}"
            );
        }
    }

    // Recovery re-issues the WHOLE `up` batch, so admitting it statement by
    // statement is not enough: the second run of a batch can undo what the first
    // run's later statements produced, or what the application wrote afterwards.
    #[test]
    fn a_multi_statement_up_is_refused_for_recovery() {
        let reason = check_non_txn_up_replayable(
            "DROP TABLE IF EXISTS t; CREATE TABLE IF NOT EXISTS t (id bigint PRIMARY KEY);",
        )
        .expect_err("a batch whose replay deletes post-migration data must be refused");
        assert!(
            reason.contains("more than one statement"),
            "the reason names the composition, not one of the statements: {reason}"
        );
        // Including a batch of statements that are each individually admitted.
        assert!(check_non_txn_up_replayable(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS a ON t (x); \
             CREATE INDEX CONCURRENTLY IF NOT EXISTS b ON t (y);"
        )
        .is_err());
    }

    // The `up` that measured the defect: it applies fine, and recovery must not
    // replay it. Refusing it at apply instead would reject a migration that works.
    #[test]
    fn a_non_txn_create_table_applies_but_is_not_replayable() {
        let m = nontxn("CREATE TABLE t (id bigint PRIMARY KEY)");
        assert!(
            validate_non_txn_idempotent(&m).is_ok(),
            "the fresh path accepts it, as it does today"
        );
        assert!(
            check_non_txn_up_replayable(&m.up).is_err(),
            "recovery must refuse to re-run a CREATE TABLE that may already have committed"
        );
    }
}
