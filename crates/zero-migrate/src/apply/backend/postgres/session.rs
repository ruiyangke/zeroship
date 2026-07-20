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

use crate::apply::backend::PgSessionSnapshot;
use crate::apply::executor::{ApplyError, RollbackError};
use crate::apply::journal::{self, JournalError};
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
    conn.exec(
        "SELECT pg_advisory_lock(hashtext($1)::bigint)",
        &[project_id.into()],
    )
    .await?;
    Ok(())
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

/// The effective `statement_timeout` for a migration: its per-migration
/// override ([`crate::model::migration::MigrationFlags::timeout_ms`]) if set, else
/// the executor-wide default.
fn effective_timeout_ms(cfg: &ExecutorConfig, m: &Migration) -> u64 {
    m.flags
        .timeout_ms
        .unwrap_or_else(|| cfg.statement_timeout_ms())
}

/// The effective `lock_timeout` for a migration: its per-migration override
/// ([`crate::model::migration::MigrationFlags::lock_timeout_ms`]) if set, else the
/// SHORT executor-wide default (the lock-safety envelope, 3s). This is the
/// per-deploy maintenance-window knob that makes the doc on
/// [`crate::conn::PgConfinement::lock_timeout`] honest: a single planned migration
/// can legitimately raise ITS OWN lock-acquisition budget (run during a quiet
/// window), while every other migration keeps the conservative fail-fast
/// default. It mirrors [`effective_timeout_ms`] exactly.
fn effective_lock_timeout_ms(cfg: &ExecutorConfig, m: &Migration) -> u64 {
    m.flags
        .lock_timeout_ms
        .unwrap_or_else(|| cfg.lock_timeout_ms())
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
) -> Result<String, crate::render::dml::IdentQuoteError> {
    Ok(format!(
        "SET LOCAL search_path TO {}; \
         SET LOCAL statement_timeout = {}; \
         SET LOCAL lock_timeout = {};",
        cfg.search_path_clause()?,
        effective_timeout_ms(cfg, m),
        effective_lock_timeout_ms(cfg, m),
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

fn dml_set_local_session_sql(
    cfg: &ExecutorConfig,
) -> Result<String, crate::render::dml::IdentQuoteError> {
    Ok(format!(
        "{AUTHOR_SQL_LITERAL_MODE} \
         SET LOCAL search_path TO {}; \
         SET LOCAL statement_timeout = {}; \
         SET LOCAL lock_timeout = {};",
        cfg.search_path_clause()?,
        cfg.statement_timeout_ms(),
        cfg.lock_timeout_ms(),
    ))
}

/// Session-level `SET …` for the **non-txn path** (no transaction to scope to).
/// These DO mutate the session, but [`apply`] restores the original GUCs on exit
/// via [`restore_session`] so they never leak. Per-migration timeout
/// override applied.
///
/// Runs as the **admin** role (no `SET ROLE` here): the non-txn journal I/O
/// (`record_started` / `record_completed` / `clear_inflight`) runs as admin, and
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
        effective_timeout_ms(cfg, m),
        effective_lock_timeout_ms(cfg, m),
    );
    conn.batch(&stmt).await?;
    Ok(())
}

/// Validate that a non-transactional migration's `up` is **idempotent**.
///
/// The two-phase non-txn recovery path re-runs `<up>` verbatim after a crash, so
/// every statement that cannot run in a transaction must tolerate already having
/// run. We enforce this statically, before any execution:
///
/// - `CREATE INDEX CONCURRENTLY …` MUST be `CREATE INDEX CONCURRENTLY IF NOT
///   EXISTS …`.
/// - `ALTER TYPE … ADD VALUE …` MUST be `… ADD VALUE IF NOT EXISTS …`.
///
/// (Other non-txn ops the classifier recognizes — `DROP INDEX CONCURRENTLY`,
/// `VACUUM` — are themselves naturally re-runnable.)
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
        let live = match crate::apply::drift::snapshot_schema(conn, probe.schema()).await {
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
                     (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind)
                 VALUES ('{applied}', $1, $2, $3, $4, $5, 'completed', 'success', $6)",
                applied = journal::EventKind::Applied.as_str()
            ),
            &[
                m.version.as_str().into(),
                (&m.name).into(),
                m.checksum.as_str().into(),
                applied_by.into(),
                exec_ms.into(),
                kind.into(),
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
    let params: Vec<Option<String>> = binds
        .iter()
        .map(|b| match b {
            crate::render::step::BindValue::Null => Ok(None),
            crate::render::step::BindValue::Bool(v) => Ok(Some(if *v {
                "true".to_string()
            } else {
                "false".to_string()
            })),
            crate::render::step::BindValue::Int(v) => Ok(Some(v.to_string())),
            crate::render::step::BindValue::Decimal(s) => Ok(Some(s.clone())),
            crate::render::step::BindValue::Text(s) => Ok(Some(s.clone())),
            crate::render::step::BindValue::Bytes(_) => Err(ApplyError::Backend(
                "postgres DML: raw binary bind reached the backend without a decode wrapper"
                    .to_string(),
            )),
        })
        .collect::<Result<_, _>>()?;

    // Render the fail-closed engine-identifier quote seams BEFORE `BEGIN`,
    // so a fail-closed `IdentQuoteError` returns before any transaction is opened
    // (no dangling txn). Both are pure functions of `cfg` — no dependency on the
    // open txn. The rendered SQL still EXECUTES inside the txn below, as before.
    // `set_local` is built from cfg directly (a DML step has no per-migration
    // timeout override slot).
    let set_local = dml_set_local_session_sql(cfg)?;
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
        let probe_started = Instant::now();
        let live = match crate::apply::drift::snapshot_schema(conn, probe.schema()).await {
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
                if supersedes.is_empty() {
                    journal::record_completed(
                        conn,
                        cfg,
                        journal::CompletedRecord {
                            version,
                            name: &m.name,
                            checksum: m.checksum.as_str(),
                            applied_by,
                            exec_ms,
                            kind: "apply",
                        },
                    )
                    .await?;
                } else {
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
                                kind: "squash",
                            },
                        )
                        .await?;
                        insert_supersedes_edges(conn, cfg, version, supersedes).await
                    }
                    .await;
                    if let Err(e) = finalize {
                        if let Err(rb) = conn.batch("ROLLBACK").await {
                            tracing::warn!(error = %rb, version = %version, "zero-migrate: ROLLBACK failed after a guarded non-txn satisfied-noop finalize error");
                        }
                        return Err(ApplyError::Journal(e));
                    }
                    conn.batch("COMMIT").await?;
                }
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
    // meta schema is revoked, so `record_started` / `recover_non_transactional`
    // (which clears the marker) / `record_completed` must NOT run under
    // `SET ROLE migrator`. Only the `<up>` (and the recovery `DROP INDEX`, which
    // the migrator owns) runs as the migrator.
    if had_inflight {
        // Recovery path: a prior run wrote `started` then crashed before
        // `completed`. Inspect the real state and make the apply idempotent.
        // `recover_non_transactional` runs entirely as the ADMIN (it is called
        // BEFORE the `<up>`'s `SET ROLE`): both the INVALID-index DROP and the
        // inflight `clear` run as admin (the admin is privileged over the project
        // schema, so it can DROP the index without the migrator role). See
        // `recover_non_transactional`. It CLEARS the inflight marker; the
        // `record_started` below then RE-ARMS it before the `<up>` re-runs.
        recover_non_transactional(conn, cfg, m).await?;
    }
    // Write (fresh path) or RE-WRITE (post-recovery) the `started` marker
    // BEFORE the `<up>` runs. The re-arm matters on the recovery path:
    // `recover_non_transactional` cleared the marker, but the re-run below can
    // itself crash before `record_completed`. Without a fresh `started` marker a
    // SECOND crash would observe `had_inflight = false` and treat the next attempt
    // as a FRESH apply — skipping the INVALID-index cleanup, so an interrupted
    // `CREATE INDEX CONCURRENTLY` left INVALID would satisfy `IF NOT EXISTS` and
    // never be rebuilt. Re-writing `started` here keeps every subsequent crash
    // re-entering recovery until a `completed` row lands. Runs as admin (still
    // before the `SET ROLE`). `record_started` is `ON CONFLICT DO NOTHING`, so on
    // the fresh path (where recovery did not run) it is the original single write.
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

    // Phase 2: immutable completed row + clear the marker (as admin). For a
    // fresh-DB squash, the supersession edges must commit ATOMICALLY with the
    // `completed` row, else a crash between leaves `S` net-applied with edges missing
    // → `v1..vN` re-enter pending and re-run on top of `S` (double-apply). The non-txn
    // `<up>` already committed (that is the nature of a non-txn migration), but the
    // JOURNAL writes — the completed row, the inflight clear, and the edges — are
    // bracketed in one transaction so they land together. No surrounding txn for a
    // non-squash: keep the original single-statement finalize.
    if supersedes.is_empty() {
        journal::record_completed(
            conn,
            cfg,
            journal::CompletedRecord {
                version,
                name: &m.name,
                checksum: m.checksum.as_str(),
                applied_by,
                exec_ms,
                kind: "apply",
            },
        )
        .await?;
    } else {
        conn.batch("BEGIN").await?;
        let finalize = async {
            // A fresh-path squash is stamped `kind='squash'` so its edges are honored
            // by `superseded_versions` (which filters on `kind='squash'`).
            journal::record_completed(
                conn,
                cfg,
                journal::CompletedRecord {
                    version,
                    name: &m.name,
                    checksum: m.checksum.as_str(),
                    applied_by,
                    exec_ms,
                    kind: "squash",
                },
            )
            .await?;
            insert_supersedes_edges(conn, cfg, version, supersedes).await
        }
        .await;
        if let Err(e) = finalize {
            if let Err(rb) = conn.batch("ROLLBACK").await {
                tracing::warn!(error = %rb, version = %version, "zero-migrate: ROLLBACK failed after a non-txn squash finalize error");
            }
            return Err(ApplyError::Journal(e));
        }
        conn.batch("COMMIT").await?;
    }

    Ok(had_inflight)
}

/// Idempotent recovery for a crashed non-transactional migration.
///
/// A `started` marker with no `completed` row means the prior attempt may have
/// (a) failed mid-DDL, or (b) **succeeded then crashed** before recording
/// `completed`. The old recovery reasoned per-op-type and blindly re-ran a
/// possibly-non-idempotent `<up>`, which permanently wedged case (b)
/// (`CREATE INDEX CONCURRENTLY` → `already exists`; `ALTER TYPE … ADD VALUE` →
/// `label already exists`).
///
/// The robust model: non-txn `up`s are **required to be idempotent**
/// (enforced up-front by [`validate_non_txn_idempotent`] — every non-txn
/// statement uses `IF NOT EXISTS`), so recovery does not need per-op reasoning.
/// It performs ONE cleanup that `IF NOT EXISTS` cannot itself do — dropping the
/// `INVALID` index residue of an interrupted CONCURRENTLY build (an INVALID
/// index satisfies `IF NOT EXISTS`, so it would otherwise never be rebuilt) —
/// then clears the marker. The caller then **re-runs the idempotent `<up>`**,
/// which is safe in both case (a) and case (b).
///
/// Runs as the **admin**: it is called BEFORE the `<up>`'s `SET ROLE` and
/// clears the inflight marker (`clear_inflight`), which is meta-schema I/O the
/// migrator has no grant for. The admin owns the meta schema and is privileged
/// over the project schema, so the project-schema `DROP INDEX` succeeds as admin
/// without needing the migrator role.
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

    // Clear the stale marker; the caller re-runs `<up>` and re-records.
    journal::clear_inflight(conn, cfg, m.version.as_str()).await?;
    Ok(())
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
    let down = m
        .down
        .as_deref()
        .expect("rollback_one_transactional is only called for RollbackStep::Down (down is Some)");
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
    let meta = crate::render::dml::quote_ident_checked(&cfg.pg.meta_schema)?;
    if let Err(e) = conn
        .exec(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (event_kind, version, name, checksum, \"by\", exec_ms)
                 VALUES ('{rolled_back}', $1, $2, $3, $4, $5)",
                rolled_back = journal::EventKind::RolledBack.as_str()
            ),
            &[
                m.version.as_str().into(),
                (&m.name).into(),
                m.checksum.as_str().into(),
                applied_by.into(),
                exec_ms.into(),
            ],
        )
        .await
    {
        if let Err(rb) = conn.batch("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zero-migrate: ROLLBACK failed after a rolled_back-insert error");
        }
        return Err(RollbackError::Journal(JournalError::Db(e.into())));
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
        let session = dml_set_local_session_sql(&cfg).expect("DML session SQL renders");

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
}
