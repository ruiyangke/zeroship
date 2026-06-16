//! The versioned executor — the apply flow (design §2.3 / §2.4).
//!
//! The heart of the engine. Given a connection, an [`ExecutorConfig`], and the
//! project's full migration set, [`apply`]:
//!
//! 1. acquires the project advisory lock `pg_advisory_lock(hashtext(project_id))`
//!    (serialize all migration activity; released at end);
//! 2. bootstraps the journal (idempotent);
//! 3. computes `pending = set − applied`, in `UUIDv7` version order;
//! 4. re-verifies the checksums of already-applied migrations — a mismatch is a
//!    hard abort (drift / tamper, design §1.5 / §3.6);
//! 5. **first pass (static, all-up-front):** runs the **[`SqlGuard`]** over the
//!    `up` SQL of EVERY pending migration, and validates that every
//!    non-transactional `up` is **idempotent** (each non-txn statement uses the
//!    `IF NOT EXISTS` form). A denial / non-idempotent op aborts the whole apply
//!    before ANY migration executes — a denied batch applies *nothing* (no
//!    earlier migration half-commits);
//! 6. **second pass (execute):** for each pending migration, applies either:
//!    - **transactionally** (default): `BEGIN; SET LOCAL …; <up>; INSERT
//!      journal; COMMIT` — DDL + journal atomic, so a crash leaves
//!      applied+recorded *or* neither. The `SET LOCAL` timeouts/`search_path` are
//!      transaction-scoped, so they never leak onto the session;
//!    - **non-transactionally** (opt-in, e.g. `CREATE INDEX CONCURRENTLY IF NOT
//!      EXISTS`): two-phase `started` marker → run `<up>` → `completed` row +
//!      clear marker. A lone `started` marker on a re-run triggers the recovery
//!      path, which (because the `up` is required to be idempotent) simply drops
//!      any INVALID-index residue and **re-runs `<up>`** — safe whether the
//!      prior attempt failed mid-build OR succeeded then crashed before
//!      recording `completed`.
//! 7. restores the session GUCs it touched and releases the lock.
//!
//! Runs out-of-band at deploy, async on compio — ZERO tokio.

use std::collections::HashMap;
use std::time::Instant;

use compio_postgres::Client;
use pg_query::protobuf::node::Node as NodeEnum;

use crate::db::ExecutorConfig;
use crate::guard::{GuardConfig, GuardError, SqlGuard};
use crate::journal::{self, AppliedEntry, JournalError, Phase};
use crate::migration::Migration;

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Versions applied this run, in apply order. Empty = no-op.
    pub applied: Vec<String>,
    /// Versions that were already applied (skipped). Informational.
    pub skipped: Vec<String>,
    /// Versions recovered via the non-txn recovery path this run.
    pub recovered: Vec<String>,
}

impl ApplyOutcome {
    /// True if nothing was applied or recovered (idempotent re-run).
    #[must_use]
    pub const fn is_noop(&self) -> bool {
        self.applied.is_empty() && self.recovered.is_empty()
    }
}

/// Error from [`apply`].
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// A database error outside a guarded/journaled step.
    #[error("db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// A journal operation failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// The SQL guard denied a pending migration's `up` SQL — the whole apply is
    /// aborted and the migration never executed.
    #[error("migration {version} denied by guard: {source}")]
    Guard {
        /// The denied migration's version.
        version: String,
        /// The underlying guard rejection.
        #[source]
        source: GuardError,
    },
    /// A non-transactional migration's `up` contains a statement that is not
    /// safe to re-run idempotently — e.g. `CREATE INDEX CONCURRENTLY` without
    /// `IF NOT EXISTS`, or `ALTER TYPE … ADD VALUE` without `IF NOT EXISTS`.
    ///
    /// The two-phase non-txn path's crash-recovery re-runs `<up>` verbatim
    /// (C1/C2), so a non-idempotent op would wedge the migration permanently on
    /// a success-then-crash (`already exists` / `label already exists`). We
    /// reject such a migration **before any execution** with this clear error;
    /// the author must write the `IF NOT EXISTS` form.
    #[error(
        "migration {version} is non-transactional but its `up` is not idempotent: {reason}. \
         Non-transactional migrations may be re-run by crash recovery, so each statement \
         must use the `IF NOT EXISTS` form."
    )]
    NonIdempotentNonTxn {
        /// The offending migration's version.
        version: String,
        /// What specifically is not idempotent.
        reason: String,
    },
    /// An already-applied migration's recorded checksum no longer matches the
    /// migration in the set — drift / tamper. Hard abort (design §3.6).
    #[error("checksum drift on {version}: journal has {recorded}, set has {expected}")]
    ChecksumDrift {
        /// The drifting migration's version.
        version: String,
        /// The checksum recorded in the journal.
        recorded: String,
        /// The checksum of the migration now in the set.
        expected: String,
    },
    /// Applying a migration's `up` SQL failed (after the guard passed). The
    /// transaction (txn path) was rolled back; nothing was journaled.
    #[error("migration {version} failed to apply: {source}")]
    MigrationFailed {
        /// The failing migration's version.
        version: String,
        /// The DB error from the failed statement.
        #[source]
        source: compio_postgres::Error,
    },
}

/// A stable i64 advisory-lock key from the project id, mirroring the
/// `hashtext(project_id)` design intent: we run `pg_advisory_lock(hashtext($1))`
/// server-side so the key is computed by Postgres exactly as the design states
/// (`hashtext` is the canonical PG text hash; deterministic per cluster).
///
/// Holding it for the whole apply serializes concurrent deploys for the same
/// project (design §2.3 step 1); a second apply waits, then sees the first's
/// committed journal and no-ops.
///
/// M2 (known limitation): `hashtext` yields a 32-bit hash, so two *unrelated*
/// project ids can collide onto the same advisory-lock key. The consequence is
/// liveness-only — two unrelated projects would serialize against each other
/// (one waits for the other's apply) — never a correctness/cross-tenant defect,
/// since each apply still operates strictly within its own meta + project
/// schema. Acceptable for v1. Revisit at scale with a 64-bit key
/// (`pg_advisory_lock(int4, int4)` from a SHA-256 prefix, or two keys).
async fn acquire_project_lock(conn: &Client, project_id: &str) -> Result<(), ApplyError> {
    conn.execute(
        "SELECT pg_advisory_lock(hashtext($1)::bigint)",
        &[&project_id],
    )
    .await?;
    Ok(())
}

async fn release_project_lock(conn: &Client, project_id: &str) -> Result<(), ApplyError> {
    conn.execute(
        "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
        &[&project_id],
    )
    .await?;
    Ok(())
}

/// The session GUCs the executor must restore on exit so its settings never
/// leak onto the pooled/long-lived connection after `apply` returns (H2).
struct SessionSnapshot {
    statement_timeout: String,
    lock_timeout: String,
    search_path: String,
}

/// Read the session GUCs we are about to override, so they can be restored when
/// `apply` finishes. Uses `current_setting(name)` (text form, exactly what `SET`
/// round-trips).
async fn snapshot_session(conn: &Client) -> Result<SessionSnapshot, ApplyError> {
    let row = conn
        .query_one(
            "SELECT current_setting('statement_timeout') AS st, \
                    current_setting('lock_timeout')      AS lt, \
                    current_setting('search_path')       AS sp",
            &[],
        )
        .await?;
    Ok(SessionSnapshot {
        statement_timeout: row.get("st"),
        lock_timeout: row.get("lt"),
        search_path: row.get("sp"),
    })
}

/// Restore the GUCs captured by [`snapshot_session`]. Uses `set_config(name,
/// value, false)` so the *value* is a bound literal, not interpolated SQL
/// (the snapshot strings are server-provided, but we keep the parameterized
/// path regardless).
async fn restore_session(conn: &Client, snap: &SessionSnapshot) -> Result<(), ApplyError> {
    // RESET ROLE first: belt-and-suspenders behind `apply`'s unconditional
    // `RESET ROLE` (L1). The non-txn path's `SET ROLE` mutates the session, so
    // drop back to the admin role before anything else, ensuring the executor's
    // least-privilege confinement never leaks onto the pooled/long-lived
    // connection after `apply` returns (H2). Harmless no-op when no SET ROLE ran
    // (txn-only applies use SET LOCAL ROLE, auto-reverted at COMMIT).
    conn.batch_execute("RESET ROLE").await?;
    conn.execute(
        "SELECT set_config('statement_timeout', $1, false), \
                set_config('lock_timeout', $2, false), \
                set_config('search_path', $3, false)",
        &[
            &snap.statement_timeout,
            &snap.lock_timeout,
            &snap.search_path,
        ],
    )
    .await?;
    Ok(())
}

/// The effective `statement_timeout` for a migration: its per-migration
/// override ([`crate::migration::MigrationFlags::timeout_ms`], H3) if set, else
/// the executor-wide default.
fn effective_timeout_ms(cfg: &ExecutorConfig, m: &Migration) -> u64 {
    m.flags.timeout_ms.unwrap_or_else(|| cfg.statement_timeout_ms())
}

/// `SET LOCAL …` clauses (transaction-scoped) for the **txn path** — they
/// vanish at COMMIT/ROLLBACK, so nothing leaks onto the session (H2). Pins the
/// project `search_path` (project schema **only** — the meta schema is
/// deliberately OFF the migration-time path so an unqualified name in the `up`
/// can never resolve to the journal, C1 defense-in-depth) and the mandatory
/// timeouts (§1.5), with the per-migration timeout override applied (H3).
///
/// This intentionally does **not** switch role: the role scoping is done
/// explicitly in [`apply_transactional`] so that `SET LOCAL ROLE migrator`
/// brackets ONLY the `<up>` and is `RESET` (back to admin) before the journal
/// INSERT — the migrator can no longer write the journal (its grant is revoked;
/// C1 fix), so the journal write must run as the admin, atomically in the SAME
/// transaction as the `up`.
fn set_local_session_sql(cfg: &ExecutorConfig, m: &Migration) -> String {
    format!(
        "SET LOCAL search_path TO \"{}\"; \
         SET LOCAL statement_timeout = {}; \
         SET LOCAL lock_timeout = {};",
        cfg.project_schema.replace('"', "\"\""),
        effective_timeout_ms(cfg, m),
        cfg.lock_timeout_ms(),
    )
}

/// `SET LOCAL ROLE "<migrator>"` for the txn path, or empty when no migrator role
/// is configured (tests / single-tenant dev). Brackets ONLY the `<up>`; the
/// caller `RESET ROLE`s before the journal write (C1).
fn set_local_role_sql(cfg: &ExecutorConfig) -> Option<String> {
    cfg.migrator_role
        .as_ref()
        .map(|role| format!("SET LOCAL ROLE \"{}\"", role.replace('"', "\"\"")))
}

/// Session-level `SET …` for the **non-txn path** (no transaction to scope to).
/// These DO mutate the session, but [`apply`] restores the original GUCs on exit
/// via [`restore_session`] so they never leak (H2). Per-migration timeout
/// override applied (H3).
///
/// Runs as the **admin** role (no `SET ROLE` here): the non-txn journal I/O
/// (`record_started` / `record_completed` / `clear_inflight`) runs as admin, and
/// only the `<up>` is bracketed by an explicit `SET ROLE migrator` / `RESET ROLE`
/// in [`apply_non_transactional`] (C1 fix). `search_path` is the project schema
/// **only** — the meta schema is off the migration-time path so an unqualified
/// name in the `up` can never resolve to the journal.
async fn configure_session_non_txn(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
) -> Result<(), ApplyError> {
    let stmt = format!(
        "SET search_path TO \"{}\"; SET statement_timeout = {}; SET lock_timeout = {};",
        cfg.project_schema.replace('"', "\"\""),
        effective_timeout_ms(cfg, m),
        cfg.lock_timeout_ms(),
    );
    conn.batch_execute(&stmt).await?;
    Ok(())
}

/// Validate that a non-transactional migration's `up` is **idempotent** (C1/C2).
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
/// `VACUUM` — are themselves naturally re-runnable.) A violation is rejected
/// with [`ApplyError::NonIdempotentNonTxn`].
fn validate_non_txn_idempotent(m: &Migration) -> Result<(), ApplyError> {
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
                        if idx.idxname.is_empty() { "<unnamed>" } else { &idx.idxname }
                    ),
                });
            }
            NodeEnum::AlterEnumStmt(e)
                if !e.new_val.is_empty() && !e.skip_if_new_val_exists =>
            {
                return Err(ApplyError::NonIdempotentNonTxn {
                    version: m.version.as_str().to_string(),
                    reason: format!(
                        "`ALTER TYPE … ADD VALUE '{}'` lacks `IF NOT EXISTS`",
                        e.new_val
                    ),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// Apply the project's pending migrations (design §2.3). Idempotent: a re-run
/// with no new migrations is a no-op.
///
/// `applied_by` is the actor recorded in the journal (`app/actor/AI`).
///
/// # Errors
/// - [`ApplyError::Guard`] — a pending migration's `up` SQL was denied; the
///   whole apply aborts (all-up-front, before any migration runs).
/// - [`ApplyError::NonIdempotentNonTxn`] — a non-transactional migration's `up`
///   is not idempotent (missing `IF NOT EXISTS`); aborts before any run.
/// - [`ApplyError::ChecksumDrift`] — an already-applied migration was tampered.
/// - [`ApplyError::MigrationFailed`] — a migration's SQL failed (rolled back).
/// - [`ApplyError::Db`] / [`ApplyError::Journal`] — infrastructure failures.
pub async fn apply(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    applied_by: &str,
) -> Result<ApplyOutcome, ApplyError> {
    acquire_project_lock(conn, &cfg.project_id).await?;
    // Capture the session GUCs we will override so we can restore them on exit
    // — the executor's search_path / statement_timeout / lock_timeout must NOT
    // leak onto the (pooled / long-lived) connection after apply (H2).
    let snapshot = snapshot_session(conn).await;
    let result = apply_locked(conn, cfg, migrations, applied_by).await;
    // L1: RESET ROLE UNCONDITIONALLY — regardless of whether `snapshot_session`
    // succeeded. The non-txn path's `SET ROLE` mutates the session; if the
    // snapshot had failed we would otherwise skip `restore_session` entirely and
    // leak the migrator role onto the pooled/long-lived connection. So drop the
    // role back to admin on EVERY exit path first, then restore the GUCs if we
    // have a snapshot. (Harmless no-op when no `SET ROLE` ran.)
    if let Err(e) = conn.batch_execute("RESET ROLE").await {
        tracing::warn!(error = %e, "zeroship-migrate: failed to RESET ROLE after apply (L1)");
    }
    // Restore the original session settings (best-effort; logged on failure)
    // before releasing the lock. The txn path uses SET LOCAL so only the non-txn
    // path actually mutates the session, but restoring unconditionally is cheap
    // and keeps the guarantee total.
    if let Ok(snap) = &snapshot {
        if let Err(e) = restore_session(conn, snap).await {
            tracing::warn!(error = %e, "zeroship-migrate: failed to restore session GUCs after apply");
        }
    }
    // Always release the lock, even on error. Surface the original error.
    let unlock = release_project_lock(conn, &cfg.project_id).await;
    // Surface the apply error first if there was one; otherwise surface any
    // unlock failure. (The lock auto-releases on session end regardless.)
    match result {
        Ok(o) => unlock.map(|()| o),
        Err(e) => Err(e),
    }
}

/// The apply body, run while holding the project advisory lock.
async fn apply_locked(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    applied_by: &str,
) -> Result<ApplyOutcome, ApplyError> {
    journal::ensure_journal(conn, cfg).await?;

    // Index the journal by version for the drift check + pending computation.
    let journal_rows: Vec<AppliedEntry> = journal::applied(conn, cfg).await?;
    let mut completed: HashMap<&str, &AppliedEntry> = HashMap::new();
    let mut started: HashMap<&str, &AppliedEntry> = HashMap::new();
    for e in &journal_rows {
        match e.phase {
            Phase::Completed => {
                completed.insert(e.version.as_str(), e);
            }
            Phase::Started => {
                started.insert(e.version.as_str(), e);
            }
        }
    }

    // Drift / tamper check: every migration in the set that the journal records
    // as completed must still match its recorded checksum.
    for m in migrations {
        if let Some(entry) = completed.get(m.version.as_str()) {
            if entry.checksum != m.checksum.as_str() {
                return Err(ApplyError::ChecksumDrift {
                    version: m.version.as_str().to_string(),
                    recorded: entry.checksum.clone(),
                    expected: m.checksum.as_str().to_string(),
                });
            }
        }
    }

    // M1: a version recorded `completed` in the journal but ABSENT from the
    // supplied set is surfaced, not silently ignored — it usually means the
    // bundle is missing a migration the database already has (a downgrade / a
    // dropped slice). We log a warning; correctness is unaffected (we still
    // only apply what's pending), but the operator should know.
    {
        use std::collections::HashSet;
        let supplied: HashSet<&str> = migrations.iter().map(|m| m.version.as_str()).collect();
        for v in completed.keys() {
            if !supplied.contains(*v) {
                tracing::warn!(
                    version = %v,
                    project = %cfg.project_id,
                    "zeroship-migrate: journal records a completed migration absent from the supplied set"
                );
            }
        }
    }

    // Pending = set − completed, in UUIDv7 (version-string) order.
    let mut pending: Vec<&Migration> = migrations
        .iter()
        .filter(|m| !completed.contains_key(m.version.as_str()))
        .collect();
    pending.sort_by(|a, b| a.version.as_str().cmp(b.version.as_str()));

    let guard = SqlGuard::new(GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    });

    // FIRST PASS — static validation over EVERY pending migration BEFORE any
    // execution (H1). The guard runs per-migration inside the apply loop in the
    // original design, which means an earlier migration could commit before a
    // later one is denied (a half-applied batch). Hoisting the static checks
    // (guard deny-list + non-txn idempotency) up front makes a denial apply
    // NOTHING. (A migration failing at EXECUTION still legitimately leaves the
    // earlier ones applied — standard migration semantics; only the STATIC
    // checks are all-or-nothing.)
    for m in &pending {
        let version = m.version.as_str();
        // GUARD GATE — RCE / priv-esc / cross-tenant / file / network denials.
        guard.check(&m.up).map_err(|source| ApplyError::Guard {
            version: version.to_string(),
            source,
        })?;
        // C1/C2 — a non-transactional `up` must be idempotent (re-runnable by
        // crash recovery). Reject the non-idempotent form with a clear error.
        if !m.flags.transactional {
            validate_non_txn_idempotent(m)?;
        }
    }

    let mut outcome = ApplyOutcome {
        applied: Vec::new(),
        skipped: completed.keys().map(|v| (*v).to_string()).collect(),
        recovered: Vec::new(),
    };

    // SECOND PASS — execute. All static checks have already passed.
    for m in pending {
        let version = m.version.as_str();
        let had_inflight = started.contains_key(version);

        if m.flags.transactional {
            apply_transactional(conn, cfg, m, applied_by).await?;
        } else {
            // Mandatory timeouts + pinned search_path on the session (the non-txn
            // path has no transaction to SET LOCAL within). Restored on exit by
            // `apply` so nothing leaks (H2). Per-migration timeout applied (H3).
            configure_session_non_txn(conn, cfg, m).await?;
            let recovered =
                apply_non_transactional(conn, cfg, m, applied_by, had_inflight).await?;
            if recovered {
                outcome.recovered.push(version.to_string());
            }
        }
        outcome.applied.push(version.to_string());
    }

    Ok(outcome)
}

/// Transactional apply (design §2.3): `BEGIN; <up>; INSERT journal; COMMIT`.
/// DDL + journal are atomic — a failure rolls back leaving no partial DDL and
/// no journal row.
async fn apply_transactional(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
) -> Result<(), ApplyError> {
    let started = Instant::now();
    // `transaction()` needs `&mut Client`; the apply flow owns the connection,
    // so we take a short-lived mutable borrow via a raw pointer-free path:
    // callers pass `&Client`, so we cannot call `transaction()` directly. We
    // instead drive BEGIN/COMMIT/ROLLBACK explicitly over the shared `&Client`
    // (still one physical session, still atomic) — this avoids requiring
    // `&mut` plumbing through the whole apply loop.
    conn.batch_execute("BEGIN").await?;

    // Pin search_path + the mandatory timeouts (per-migration override applied)
    // with SET LOCAL so they are scoped to THIS transaction and vanish at
    // COMMIT/ROLLBACK — nothing leaks onto the session (H2 / H3). This runs as
    // the admin (always permitted); the role switch is applied separately around
    // the `<up>` only.
    if let Err(e) = conn.batch_execute(&set_local_session_sql(cfg, m)).await {
        let _ = conn.batch_execute("ROLLBACK").await;
        return Err(ApplyError::Db(e));
    }

    // C1: drop to the least-privilege migrator role for the duration of the
    // `<up>` ONLY. `SET LOCAL ROLE` is transaction-scoped, so the role switch is
    // confined to this txn; we explicitly `RESET ROLE` (below) before the journal
    // INSERT so the journal write runs as the admin — the migrator's journal
    // grant is revoked (role.rs), so it could not write the journal even if it
    // tried. The up's DDL is thereby confined to line-2 privileges (design §1.3)
    // while the journal stays unforgeable by the migration.
    if let Some(set_role) = set_local_role_sql(cfg) {
        if let Err(e) = conn.batch_execute(&set_role).await {
            let _ = conn.batch_execute("ROLLBACK").await;
            return Err(ApplyError::Db(e));
        }
    }

    // Run the migration's up SQL (as the migrator, if a role is configured).
    if let Err(e) = conn.batch_execute(&m.up).await {
        // Roll back; report the failure. No journal row was written.
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zeroship-migrate: ROLLBACK failed after a migration error (M4)");
        }
        return Err(ApplyError::MigrationFailed {
            version: m.version.as_str().to_string(),
            source: e,
        });
    }

    // C1: RESET ROLE back to the admin — still INSIDE the transaction — so the
    // journal INSERT below runs as the admin (the migrator cannot write the
    // journal). `RESET ROLE` mid-transaction is supported and does not end the
    // txn, so atomicity of `<up>` + journal is preserved.
    if cfg.migrator_role.is_some() {
        if let Err(e) = conn.batch_execute("RESET ROLE").await {
            let _ = conn.batch_execute("ROLLBACK").await;
            return Err(ApplyError::Db(e));
        }
    }

    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // Journal the completed row in the SAME transaction, as the admin.
    let meta = format!("\"{}\"", cfg.meta_schema.replace('"', "\"\""));
    if let Err(e) = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (version, name, checksum, applied_by, exec_ms, phase, outcome)
                 VALUES ($1, $2, $3, $4, $5, 'completed', 'success')"
            ),
            &[
                &m.version.as_str(),
                &m.name,
                &m.checksum.as_str(),
                &applied_by,
                &exec_ms,
            ],
        )
        .await
    {
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zeroship-migrate: ROLLBACK failed after a journal-insert error (M4)");
        }
        return Err(ApplyError::Journal(JournalError::Db(e)));
    }

    conn.batch_execute("COMMIT").await?;
    Ok(())
}

/// Non-transactional apply (design §2.3 / §2.4): two-phase with a `started`
/// marker, plus the idempotent recovery path.
///
/// Returns `true` if this was a recovery (a prior `started` marker existed).
async fn apply_non_transactional(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
    had_inflight: bool,
) -> Result<bool, ApplyError> {
    let version = m.version.as_str();

    // Journal / inflight I/O runs as the ADMIN (C1): the migrator's grant on the
    // meta schema is revoked, so `record_started` / `recover_non_transactional`
    // (which clears the marker) / `record_completed` must NOT run under
    // `SET ROLE migrator`. Only the `<up>` (and the recovery `DROP INDEX`, which
    // the migrator owns) runs as the migrator.
    if had_inflight {
        // Recovery path: a prior run wrote `started` then crashed before
        // `completed`. Inspect the real state and make the apply idempotent. The
        // INVALID-index DROP inside runs as the migrator (it owns the index); the
        // inflight `clear` runs as admin. See `recover_non_transactional`.
        recover_non_transactional(conn, cfg, m).await?;
    } else {
        journal::record_started(conn, cfg, version, &m.name, m.checksum.as_str(), applied_by)
            .await?;
    }

    let started = Instant::now();
    // C1: bracket the `<up>` with SET ROLE / RESET ROLE so the migration's DDL
    // runs under line-2 confinement, but the journal writes above/below run as
    // admin. `RESET ROLE` runs on ALL exit paths (including the error path) so
    // the role never leaks onto the session even if the `<up>` fails — and
    // `apply`'s `restore_session` is an unconditional backstop (L1).
    if let Some(role) = &cfg.migrator_role {
        conn.batch_execute(&format!("SET ROLE \"{}\"", role.replace('"', "\"\"")))
            .await?;
    }
    let up_result = conn.batch_execute(&m.up).await;
    if cfg.migrator_role.is_some() {
        // RESET ROLE regardless of the up's success, so the journal writes below
        // run as admin and no role leaks onto the session.
        if let Err(e) = conn.batch_execute("RESET ROLE").await {
            // If RESET ROLE itself fails, surface it (apply's restore_session is
            // the L1 backstop). Prefer surfacing the up's error if it failed.
            if up_result.is_ok() {
                return Err(ApplyError::Db(e));
            }
        }
    }
    up_result.map_err(|e| ApplyError::MigrationFailed {
        version: version.to_string(),
        source: e,
    })?;
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // Phase 2: immutable completed row + clear the marker (as admin).
    journal::record_completed(
        conn,
        cfg,
        version,
        &m.name,
        m.checksum.as_str(),
        applied_by,
        exec_ms,
    )
    .await?;

    Ok(had_inflight)
}

/// Idempotent recovery for a crashed non-transactional migration (design §2.4,
/// C1/C2).
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
/// Runs as the **admin** (C1): it is called BEFORE the `<up>`'s `SET ROLE` and
/// clears the inflight marker (`clear_inflight`), which is meta-schema I/O the
/// migrator has no grant for. The admin owns the meta schema and is privileged
/// over the project schema, so the project-schema `DROP INDEX` succeeds as admin
/// without needing the migrator role.
async fn recover_non_transactional(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
) -> Result<(), ApplyError> {
    // Drop the INVALID residue of an interrupted CONCURRENTLY build — but ONLY
    // the index(es) this migration's `up` names (v1.x scope fix). An INVALID
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
                &[&cfg.project_schema, &idx],
            )
            .await?
            .get("invalid");
        if is_invalid {
            let stmt = format!(
                "DROP INDEX IF EXISTS \"{}\".\"{}\"",
                cfg.project_schema.replace('"', "\"\""),
                idx.replace('"', "\"\""),
            );
            conn.batch_execute(&stmt).await?;
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
        if let Some(NodeEnum::IndexStmt(idx)) =
            raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref())
        {
            if !idx.idxname.is_empty() {
                names.push(idx.idxname.clone());
            }
        }
    }
    names
}
