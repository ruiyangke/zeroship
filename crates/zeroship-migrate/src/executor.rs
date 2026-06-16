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

use crate::approval::Approval;
use crate::db::ExecutorConfig;
use crate::guard::{GuardConfig, GuardError, SqlGuard};
use crate::journal::{self, AppliedEntry, JournalError, Phase};
use crate::migration::{Migration, MigrationId};

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
    /// The pending batch contains a destructive migration (`flags.destructive`)
    /// but the caller passed [`Approval::None`]. This is the executor's OWN
    /// defense-in-depth approval gate — independent of (and additional to) the
    /// engine's gate ([`crate::engine::MigrationEngine::apply`]), so a caller that
    /// drives [`apply`] directly, bypassing the engine, still cannot run a
    /// destructive batch without explicit approval. Nothing was applied.
    #[error("apply contains a destructive migration but Approval::Approved was not given")]
    ApprovalRequired,
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
    /// A pending migration's `depends_on` names a version that is neither already
    /// applied nor present in the supplied set — the dependency graph is
    /// unsatisfiable, so no ordering exists. Hard abort before any execution.
    #[error("migration {version} depends on unknown migration {missing} (not in the set or journal)")]
    MissingDependency {
        /// The migration with the dangling dependency.
        version: String,
        /// The unknown version it depends on.
        missing: String,
    },
    /// The pending migrations' `depends_on` edges form a cycle, so no topological
    /// order exists. Hard abort before any execution.
    #[error("dependency cycle among pending migrations: {0}")]
    DependencyCycle(String),
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
/// `approval` is the caller's approval decision. This is the executor's OWN
/// defense-in-depth approval gate (design §1.6): if any pending migration is
/// flagged [`destructive`](crate::migration::MigrationFlags::destructive) and
/// `approval != Approval::Approved`, the apply is refused with
/// [`ApplyError::ApprovalRequired`] before any migration executes — independent
/// of (and additional to) the engine's gate, so a caller driving [`apply`]
/// directly cannot bypass approval. A non-destructive batch runs with
/// [`Approval::None`].
///
/// # Errors
/// - [`ApplyError::ApprovalRequired`] — a destructive migration without approval;
///   aborts before any migration runs.
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
    approval: Approval,
    applied_by: &str,
) -> Result<ApplyOutcome, ApplyError> {
    // Defense-in-depth approval gate (design §1.6) — refuse a destructive batch
    // without explicit approval BEFORE doing anything (not even the lock). The
    // engine has its own gate; this is the independent executor-layer check so a
    // direct caller cannot bypass it.
    if approval != Approval::Approved
        && migrations.iter().any(|m| m.flags.destructive)
    {
        return Err(ApplyError::ApprovalRequired);
    }
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

    // Pending = set − completed. Ordered by `depends_on` when present
    // (topological, version-tiebroken & stable), else pure UUIDv7 version order.
    let pending: Vec<&Migration> = order_pending(migrations, &completed)?;

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

/// Order the pending migrations honoring `depends_on` (design §4 cross-slice
/// ordering) when set, falling back to pure version order otherwise.
///
/// The default order is UUIDv7 version (time-ordered), but `depends_on` can pull a
/// *higher*-version migration to run **after** a lower-version one it depends on —
/// or, the converse the task calls out: a later-version migration whose
/// `depends_on` is empty may still need to run *after* an earlier-version one
/// because that earlier one depends on **it**. We therefore topologically sort the
/// dependency DAG (edge `dep -> m` for each `dep` in `m.depends_on`), using a
/// version-ordered worklist so the result is **stable** and version-tiebroken
/// (among nodes with no outstanding deps, the lowest version goes first).
///
/// Dependencies already satisfied by the journal (a `completed` version not in the
/// pending set) are treated as pre-met edges — they impose no ordering on the
/// pending batch but must still resolve to a real version (set or journal),
/// otherwise the graph is unsatisfiable.
///
/// # Errors
/// - [`ApplyError::MissingDependency`] — a `depends_on` names a version absent
///   from both the supplied set and the journal.
/// - [`ApplyError::DependencyCycle`] — the pending edges form a cycle.
fn order_pending<'a>(
    migrations: &'a [Migration],
    completed: &HashMap<&str, &AppliedEntry>,
) -> Result<Vec<&'a Migration>, ApplyError> {
    use std::collections::{BTreeMap, HashSet};

    // The pending set, indexed by version, plus the set of all known versions
    // (pending ∪ completed) for dependency-existence checks.
    let pending: Vec<&Migration> = migrations
        .iter()
        .filter(|m| !completed.contains_key(m.version.as_str()))
        .collect();
    let pending_versions: HashSet<&str> =
        pending.iter().map(|m| m.version.as_str()).collect();
    let known: HashSet<&str> = pending_versions
        .iter()
        .copied()
        .chain(completed.keys().copied())
        .collect();

    // Validate every dependency resolves to a real version (set or journal), and
    // build the in-degree + adjacency over the PENDING subgraph only (an edge from
    // an already-completed dep imposes no ordering on the batch).
    //
    // `adj[dep]` = pending migrations that must run AFTER `dep`.
    // `indeg[m]` = number of *pending* deps `m` is still waiting on.
    let mut indeg: BTreeMap<&str, usize> =
        pending.iter().map(|m| (m.version.as_str(), 0usize)).collect();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for m in &pending {
        for dep in &m.depends_on {
            let dep_v = dep.as_str();
            if !known.contains(dep_v) {
                return Err(ApplyError::MissingDependency {
                    version: m.version.as_str().to_string(),
                    missing: dep_v.to_string(),
                });
            }
            // Only deps that are themselves PENDING constrain batch order; a dep
            // already in the journal is pre-satisfied.
            if pending_versions.contains(dep_v) {
                adj.entry(dep_v).or_default().push(m.version.as_str());
                *indeg.get_mut(m.version.as_str()).expect("pending node") += 1;
            }
        }
    }

    // Kahn's algorithm with a version-ordered ready set (BTreeSet keys are sorted),
    // so the topo order is deterministic and version-tiebroken: among migrations
    // with no remaining unmet dep, the lowest version emits first. This degrades to
    // pure version order when no `depends_on` edges exist.
    let by_version: HashMap<&str, &Migration> =
        pending.iter().map(|m| (m.version.as_str(), *m)).collect();
    let mut ready: std::collections::BTreeSet<&str> = indeg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&v, _)| v)
        .collect();
    let mut ordered: Vec<&Migration> = Vec::with_capacity(pending.len());
    while let Some(&v) = ready.iter().next() {
        ready.remove(v);
        ordered.push(by_version[v]);
        if let Some(succs) = adj.get(v) {
            for &s in succs {
                let e = indeg.get_mut(s).expect("successor node");
                *e -= 1;
                if *e == 0 {
                    ready.insert(s);
                }
            }
        }
    }

    if ordered.len() != pending.len() {
        // The leftover nodes (still indeg > 0) are exactly the cycle members.
        let mut cyclic: Vec<&str> = indeg
            .iter()
            .filter(|(_, &d)| d > 0)
            .map(|(&v, _)| v)
            .collect();
        cyclic.sort_unstable();
        return Err(ApplyError::DependencyCycle(cyclic.join(", ")));
    }
    Ok(ordered)
}

/// Order the migrations being rolled back in **REVERSE TOPOLOGICAL** order of
/// `depends_on` — the transpose of [`order_pending`].
///
/// Invariant (design §5): a migration's `down` must run **before** the `down` of
/// any migration it `depends_on`. The depended-on object (e.g. a parent table)
/// must survive until every dependent that references it (e.g. a child with an FK)
/// has been torn down. This is exactly the reverse of apply's topo order, so we
/// compute the forward topo order (Kahn, lowest-version-ready-first — identical to
/// `order_pending`, restricted to the rollback set) and reverse it. The result is
/// stable + version-tiebroken: with no `depends_on` edges it degrades to strict
/// **reverse-version** order (preserving the pre-#1 behavior for the version-aligned
/// case).
///
/// The graph is restricted to the set being rolled back: a `depends_on` pointing
/// OUTSIDE the set (a migration that stays applied, or one already rolled back)
/// imposes no ordering on the batch — it is not torn down here.
///
/// A `depends_on` pointing to a version selected for rollback but absent from the
/// supplied set is already rejected by the caller's `MissingFromSet` pre-flight
/// (every selected version is resolved to a `Migration` before this runs), so the
/// graph is always buildable; the only fail-closed case here is a cycle.
///
/// # Errors
/// - [`RollbackError::DependencyCycle`] — the selected migrations' `depends_on`
///   edges form a cycle (impossible if apply enforced acyclicity; defended anyway).
fn order_rollback<'a>(
    selected: &[&'a Migration],
) -> Result<Vec<&'a Migration>, RollbackError> {
    use std::collections::{BTreeMap, HashSet};

    // Versions in the rollback set; edges to deps OUTSIDE the set are ignored
    // (those objects are not being torn down in this batch).
    let in_set: HashSet<&str> = selected.iter().map(|m| m.version.as_str()).collect();
    let by_version: HashMap<&str, &Migration> =
        selected.iter().map(|m| (m.version.as_str(), *m)).collect();

    // Forward topo over the rollback subgraph: edge `dep -> m` for each `dep` in
    // `m.depends_on` that is ALSO in the set. `adj[dep]` = members that must apply
    // AFTER `dep`; `indeg[m]` = # of in-set deps `m` still waits on.
    let mut indeg: BTreeMap<&str, usize> =
        selected.iter().map(|m| (m.version.as_str(), 0usize)).collect();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for m in selected {
        for dep in &m.depends_on {
            let dep_v = dep.as_str();
            if in_set.contains(dep_v) {
                adj.entry(dep_v).or_default().push(m.version.as_str());
                *indeg.get_mut(m.version.as_str()).expect("in-set node") += 1;
            }
        }
    }

    // Kahn with a version-ordered ready set (BTreeSet → lowest version first), so
    // the forward order is deterministic + version-tiebroken, mirroring
    // `order_pending`.
    let mut ready: std::collections::BTreeSet<&str> = indeg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&v, _)| v)
        .collect();
    let mut topo: Vec<&Migration> = Vec::with_capacity(selected.len());
    while let Some(&v) = ready.iter().next() {
        ready.remove(v);
        topo.push(by_version[v]);
        if let Some(succs) = adj.get(v) {
            for &s in succs {
                let e = indeg.get_mut(s).expect("successor node");
                *e -= 1;
                if *e == 0 {
                    ready.insert(s);
                }
            }
        }
    }

    if topo.len() != selected.len() {
        let mut cyclic: Vec<&str> = indeg
            .iter()
            .filter(|(_, &d)| d > 0)
            .map(|(&v, _)| v)
            .collect();
        cyclic.sort_unstable();
        return Err(RollbackError::DependencyCycle(cyclic.join(", ")));
    }

    // Reverse the topo order → reverse-topological: dependents first, depended-on
    // last (each migration's down runs before the downs of its `depends_on`).
    topo.reverse();
    Ok(topo)
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

// ===========================================================================
// Rollback (Plan 5) — apply `down` SQL in reverse to a target.
// ===========================================================================

/// One resolved step of a rollback plan (internal): either run a migration's
/// `down`, or force-skip an irreversible (`down: None`) migration.
enum RollbackStep<'a> {
    /// Run this migration's `down` (it is `Some`) and journal a `rolled_back`.
    Down(&'a Migration),
    /// Skip this irreversible migration (force path); it stays applied.
    SkipIrreversible(&'a Migration),
}

/// How far [`rollback`] should unwind the applied migrations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackTarget {
    /// Roll back every net-applied migration whose version is **strictly after**
    /// this one — i.e. unwind *down to* (and keeping) this version. The target
    /// itself is NOT rolled back.
    ToVersion(MigrationId),
    /// Roll back the `n` most-recently-applied migrations (the `n` highest
    /// net-applied versions). `Steps(0)` is a no-op; `Steps(k)` with `k` ≥ the
    /// applied count behaves like [`RollbackTarget::All`].
    Steps(usize),
    /// Roll back **all** net-applied migrations.
    All,
}

/// A complete rollback request.
///
/// How far to unwind ([`RollbackTarget`]) plus the irreversible-handling
/// [`RollbackOptions`]. Bundled so [`rollback`] and
/// [`MigrationEngine::rollback`](crate::engine::MigrationEngine::rollback) carry
/// one parameter rather than two.
#[derive(Debug, Clone)]
pub struct RollbackRequest {
    /// How far to unwind.
    pub target: RollbackTarget,
    /// Irreversible (`down: None`) handling.
    pub options: RollbackOptions,
}

impl RollbackRequest {
    /// A request for `target` with default (refuse-irreversible) options.
    #[must_use]
    pub fn new(target: RollbackTarget) -> Self {
        Self {
            target,
            options: RollbackOptions::default(),
        }
    }

    /// Set the irreversible-handling options (builder convenience).
    #[must_use]
    pub const fn with_options(mut self, options: RollbackOptions) -> Self {
        self.options = options;
        self
    }
}

/// Options controlling [`rollback`] over irreversible (`down: None`) migrations.
#[derive(Debug, Clone, Copy, Default)]
pub struct RollbackOptions {
    /// Proceed across a migration with `down: None` (irreversible) by **skipping**
    /// its down step instead of refusing. Off by default: rollback refuses to
    /// cross an irreversible migration and directs the operator to roll-forward
    /// (author a compensating migration). Requires [`backup_acknowledged`] too.
    ///
    /// [`backup_acknowledged`]: RollbackOptions::backup_acknowledged
    pub force: bool,
    /// The operator's acknowledgement that a backup exists. `force` is honored
    /// ONLY when this is also set — forcing past an irreversible step is a
    /// data-loss operation, so it requires both a deliberate force and a backup
    /// acknowledgement (design §5).
    pub backup_acknowledged: bool,
}

/// What [`rollback`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackOutcome {
    /// Versions whose `down` ran + were journaled `rolled_back`, in the order they
    /// were rolled back: **reverse topological order of `depends_on`** (each
    /// migration's `down` ran before the downs of everything it `depends_on`). This
    /// is the transpose of apply's topo order and degrades to strict
    /// reverse-version order when there are no `depends_on` edges (the
    /// version-aligned case), so it is ≈ reverse apply order.
    pub rolled_back: Vec<String>,
    /// Versions skipped because they are irreversible (`down: None`) and `force`
    /// was given (design §5). Empty unless forcing.
    pub skipped_irreversible: Vec<String>,
}

impl RollbackOutcome {
    /// True if nothing was rolled back (and nothing force-skipped).
    #[must_use]
    pub const fn is_noop(&self) -> bool {
        self.rolled_back.is_empty() && self.skipped_irreversible.is_empty()
    }
}

/// Error from [`rollback`].
#[derive(Debug, thiserror::Error)]
pub enum RollbackError {
    /// A database error outside a guarded/journaled step.
    #[error("db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// A journal operation failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// Rollback was requested with [`Approval::None`]. A `down` is inherently
    /// destructive (it tears structure down), so rollback ALWAYS requires
    /// [`Approval::Approved`]. This is the executor's OWN defense-in-depth gate,
    /// independent of (and additional to) the engine's gate
    /// ([`crate::engine::MigrationEngine::rollback`]), so a caller driving
    /// [`rollback`] directly cannot bypass approval. Nothing was rolled back.
    #[error("rollback requires Approval::Approved (every down is destructive) but it was not given")]
    ApprovalRequired,
    /// The [`RollbackTarget::ToVersion`] target is not a currently net-applied
    /// migration (never applied, or already rolled back). Nothing was rolled back.
    #[error("rollback target {version} is not currently applied")]
    UnknownTarget {
        /// The requested target version.
        version: String,
    },
    /// A version selected for rollback is not present in the supplied migration
    /// set, so its `down` SQL is unavailable. Nothing was rolled back.
    #[error("migration {version} is applied but absent from the supplied set; cannot roll back (its `down` is unavailable)")]
    MissingFromSet {
        /// The applied-but-absent version.
        version: String,
    },
    /// The rollback would cross a migration with `down: None` (irreversible) and
    /// neither `force` nor `backup_acknowledged` was given. The default guidance
    /// is **roll-forward**: author a new compensating migration. Nothing was
    /// rolled back (refuse-by-default, before ANY down runs).
    #[error(
        "migration {version} ('{name}') is irreversible (down: None); rollback refuses by default. \
         Prefer ROLL-FORWARD: author a compensating migration. To override, pass \
         RollbackOptions {{ force: true, backup_acknowledged: true }} (skips the irreversible step; data loss)."
    )]
    Irreversible {
        /// The irreversible migration's version.
        version: String,
        /// Its human-readable name.
        name: String,
    },
    /// The SQL guard denied a `down`'s SQL — the down is SQL too and goes through
    /// the SAME defenses as an up (design §5). The whole rollback aborts before
    /// any down runs (all-up-front, mirroring apply).
    #[error("rollback of {version} denied by guard: {source}")]
    Guard {
        /// The denied migration's version.
        version: String,
        /// The underlying guard rejection.
        #[source]
        source: GuardError,
    },
    /// An already-applied migration's recorded checksum no longer matches the
    /// migration in the set — drift / tamper. Hard abort (mirrors apply).
    #[error("checksum drift on {version}: journal has {recorded}, set has {expected}")]
    ChecksumDrift {
        /// The drifting migration's version.
        version: String,
        /// The checksum recorded in the journal.
        recorded: String,
        /// The checksum of the migration now in the set.
        expected: String,
    },
    /// Running a migration's `down` SQL failed (after the guard passed). The
    /// transaction (txn path) was rolled back; no `rolled_back` event was written.
    #[error("rollback of {version} failed: {source}")]
    DownFailed {
        /// The failing migration's version.
        version: String,
        /// The DB error from the failed `down`.
        #[source]
        source: compio_postgres::Error,
    },
    /// The `depends_on` edges among the versions selected for rollback form a
    /// cycle, so no reverse-topological order exists. This should be impossible if
    /// apply enforced acyclicity, but rollback defends anyway (fail-closed before
    /// any down runs). Nothing was rolled back.
    #[error("rollback dependency cycle among selected migrations: {0}")]
    DependencyCycle(String),
    /// A force-skipped irreversible (`down: None`) migration `kept` `depends_on`
    /// `dependency` (directly or transitively), but `dependency` is selected for
    /// ACTUAL rollback. Tearing `dependency` down would leave `kept` (still applied,
    /// because its down never runs) referencing a dropped object — a dangling FK or
    /// a mid-batch `DownFailed`. Refused even under `force`+`backup_acknowledged`;
    /// nothing was rolled back. Roll-forward instead (author a compensating
    /// migration).
    #[error(
        "cannot force-skip irreversible migration {kept} while rolling back {dependency}: \
         {kept} depends_on {dependency} (directly or transitively), so the kept migration would \
         be left referencing a torn-down object. Roll-forward instead (author a compensating migration)."
    )]
    ForceSkipDependencyConflict {
        /// The irreversible migration being force-skipped (kept applied).
        kept: String,
        /// The depended-on version that would be rolled back beneath it.
        dependency: String,
    },
}

/// Roll back applied migrations to a [`RollbackTarget`] (design §5).
///
/// Applies the `down` SQL of net-applied-and-not-rolled-back migrations **after**
/// the target, in **reverse topological order of `depends_on`** (the transpose of
/// apply: a migration's `down` runs before the downs of everything it `depends_on`,
/// so a depended-on object survives until every dependent is torn down). This
/// degrades to reverse-version order when there are no `depends_on` edges. Each
/// `down` runs under the project advisory lock.
///
/// **The `down` is privileged SQL too — it gets the SAME defenses as an `up`:**
/// every selected `down` is run through the [`SqlGuard`] up-front (a denial aborts
/// the whole rollback before any down runs), and each `down` executes under the
/// least-privilege `migrator` role (`SET LOCAL ROLE` for the txn path), exactly
/// like the up path. A malicious/buggy `down` (cross-schema, RCE) is just as
/// dangerous as an up.
///
/// **Append-only journaling:** each `down` + its `rolled_back` journal append run
/// in ONE transaction (txn path: `BEGIN; SET LOCAL ROLE migrator; <down>; RESET
/// ROLE; INSERT rolled_back as admin; COMMIT`). The completed row is never
/// deleted; a rolled-back version becomes pending again (re-appliable).
///
/// **Irreversible (`down: None`):** by default rollback **refuses** to cross such
/// a migration ([`RollbackError::Irreversible`]) and directs the operator to
/// roll-forward. With [`RollbackOptions::force`] + [`RollbackOptions::backup_acknowledged`]
/// it proceeds, **skipping** the irreversible step (recorded in
/// [`RollbackOutcome::skipped_irreversible`]; the down cannot be journaled because
/// it never ran, so no `rolled_back` event is written for it — it stays applied).
///
/// `approval` is the caller's approval decision. A `down` is inherently
/// destructive, so rollback ALWAYS requires [`Approval::Approved`]: with
/// [`Approval::None`] this returns [`RollbackError::ApprovalRequired`] before any
/// `down` executes. This is the executor's OWN defense-in-depth gate (design
/// §1.6), independent of (and additional to) the engine's gate, so a caller
/// driving [`rollback`] directly cannot bypass approval.
///
/// # Errors
/// See [`RollbackError`]. All pre-flight checks (approval, unknown target,
/// missing-from-set, guard denial, irreversible-without-force, checksum drift)
/// abort before any `down` executes.
pub async fn rollback(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    request: RollbackRequest,
    approval: Approval,
    applied_by: &str,
) -> Result<RollbackOutcome, RollbackError> {
    // Defense-in-depth approval gate (design §1.6) — every `down` is destructive,
    // so rollback ALWAYS requires explicit approval. Refuse BEFORE doing anything
    // (not even the lock). Independent of the engine's own gate.
    if approval != Approval::Approved {
        return Err(RollbackError::ApprovalRequired);
    }
    // Acquire the project advisory lock directly (raw SQL, so the error type is
    // compio_postgres::Error → RollbackError::Db) — serializes against concurrent
    // apply/rollback for the same project, exactly like `apply`.
    conn.execute(
        "SELECT pg_advisory_lock(hashtext($1)::bigint)",
        &[&cfg.project_id],
    )
    .await?;
    let snapshot = snapshot_session(conn).await.ok();
    let result = rollback_locked(conn, cfg, migrations, request, applied_by).await;
    // Mirror apply's exit discipline: unconditional RESET ROLE, then restore GUCs,
    // then release the lock — so the migrator role / executor GUCs never leak.
    if let Err(e) = conn.batch_execute("RESET ROLE").await {
        tracing::warn!(error = %e, "zeroship-migrate: failed to RESET ROLE after rollback (L1)");
    }
    if let Some(snap) = &snapshot {
        if let Err(e) = restore_session(conn, snap).await {
            tracing::warn!(error = %e, "zeroship-migrate: failed to restore session GUCs after rollback");
        }
    }
    let unlock = conn
        .execute(
            "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
            &[&cfg.project_id],
        )
        .await;
    match result {
        Ok(o) => unlock.map(|_| o).map_err(RollbackError::Db),
        Err(e) => Err(e),
    }
}

/// Resolve which net-applied migrations to roll back and in what order — the pure
/// (non-async) core of [`rollback_locked`]. Selects per [`RollbackTarget`], resolves
/// each version to its `Migration` (checking `MissingFromSet` + checksum drift),
/// reverse-topologically orders the downs by `depends_on` ([`order_rollback`], #1),
/// then runs the all-up-front pre-flight (irreversible-without-force, guard) and
/// returns the executable [`RollbackStep`] plan in execution order.
///
/// # Errors
/// [`RollbackError::UnknownTarget`], [`RollbackError::MissingFromSet`],
/// [`RollbackError::ChecksumDrift`], [`RollbackError::DependencyCycle`],
/// [`RollbackError::Irreversible`], or [`RollbackError::Guard`] — all before any
/// `down` runs.
fn build_rollback_plan<'a>(
    net_applied: &[&'a AppliedEntry],
    migrations: &'a [Migration],
    target: &RollbackTarget,
    opts: RollbackOptions,
    cfg: &ExecutorConfig,
) -> Result<Vec<RollbackStep<'a>>, RollbackError> {
    // Index the supplied set by version (source of the `down` SQL + checksum).
    let by_version: HashMap<&str, &Migration> =
        migrations.iter().map(|m| (m.version.as_str(), m)).collect();

    // Which net-applied versions to roll back (ascending order for now).
    let selected: Vec<&AppliedEntry> = match target {
        RollbackTarget::All => net_applied.to_vec(),
        RollbackTarget::Steps(n) => {
            let n = (*n).min(net_applied.len());
            net_applied[net_applied.len() - n..].to_vec()
        }
        RollbackTarget::ToVersion(v) => {
            let tv = v.as_str();
            if !net_applied.iter().any(|e| e.version == tv) {
                return Err(RollbackError::UnknownTarget {
                    version: tv.to_string(),
                });
            }
            net_applied
                .iter()
                .copied()
                .filter(|e| e.version.as_str() > tv)
                .collect()
        }
    };

    // Resolve each selected version to its `Migration`. MissingFromSet + checksum
    // drift are version-order-stable, so check them here BEFORE ordering — the
    // reverse-topo sort needs the `Migration`s in hand.
    let mut selected_migs: Vec<&Migration> = Vec::with_capacity(selected.len());
    for entry in &selected {
        let v = entry.version.as_str();
        let Some(m) = by_version.get(v).copied() else {
            return Err(RollbackError::MissingFromSet {
                version: v.to_string(),
            });
        };
        if entry.checksum != m.checksum.as_str() {
            return Err(RollbackError::ChecksumDrift {
                version: v.to_string(),
                recorded: entry.checksum.clone(),
                expected: m.checksum.as_str().to_string(),
            });
        }
        selected_migs.push(m);
    }

    // Order the downs in REVERSE TOPOLOGICAL order of `depends_on` (#1): a
    // migration's `down` runs BEFORE the downs of everything it depends_on, so a
    // depended-on object (parent table) survives until every dependent (child FK)
    // is gone. Degrades to reverse-version order with no edges (regression-guarded).
    let ordered = order_rollback(&selected_migs)?;

    // ---- PRE-FLIGHT (all-up-front, before ANY down runs) -------------------
    // 1. Irreversible (`down: None`): refuse unless force + backup_acknowledged.
    // 2. The `down` SQL passes the guard (down is SQL too — same defenses).
    // (MissingFromSet + checksum already verified above.)
    let force_ok = opts.force && opts.backup_acknowledged;
    let guard = SqlGuard::new(GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    });
    let mut plan: Vec<RollbackStep> = Vec::with_capacity(ordered.len());
    for m in ordered {
        let v = m.version.as_str();
        match &m.down {
            None => {
                if !force_ok {
                    return Err(RollbackError::Irreversible {
                        version: v.to_string(),
                        name: m.name.clone(),
                    });
                }
                plan.push(RollbackStep::SkipIrreversible(m));
            }
            Some(down) => {
                // The down is privileged SQL — run it through the guard up-front.
                guard.check(down).map_err(|source| RollbackError::Guard {
                    version: v.to_string(),
                    source,
                })?;
                plan.push(RollbackStep::Down(m));
            }
        }
    }

    // #2 — force-skip dependency guard. A force-skipped irreversible migration M is
    // KEPT applied (its down never runs). If any migration selected for ACTUAL
    // rollback (a `Down` step) is one M `depends_on` (directly or transitively),
    // tearing it down would leave M referencing a dropped object — a dangling FK or
    // a mid-batch `DownFailed`. Refuse the whole rollback even under force; the safe
    // path is roll-forward. (No-op when nothing is force-skipped.)
    let rolling: std::collections::HashSet<&str> = plan
        .iter()
        .filter_map(|s| match s {
            RollbackStep::Down(m) => Some(m.version.as_str()),
            RollbackStep::SkipIrreversible(_) => None,
        })
        .collect();
    if !rolling.is_empty() {
        for step in &plan {
            if let RollbackStep::SkipIrreversible(kept) = step {
                if let Some(dep) = first_dependency_in_set(kept, &rolling, &by_version) {
                    return Err(RollbackError::ForceSkipDependencyConflict {
                        kept: kept.version.as_str().to_string(),
                        dependency: dep,
                    });
                }
            }
        }
    }

    Ok(plan)
}

/// Walk `m`'s `depends_on` transitively (over `by_version`) and return the first
/// dependency version found in `target`. Used by the #2 force-skip guard to detect
/// whether a kept (irreversible) migration depends on anything being torn down.
/// Deterministic (sorted-version frontier) and cycle-safe via a visited set.
fn first_dependency_in_set(
    m: &Migration,
    target: &std::collections::HashSet<&str>,
    by_version: &HashMap<&str, &Migration>,
) -> Option<String> {
    use std::collections::{BTreeSet, HashSet};
    let mut visited: HashSet<&str> = HashSet::new();
    let mut frontier: BTreeSet<&str> = m.depends_on.iter().map(MigrationId::as_str).collect();
    while let Some(&dep_v) = frontier.iter().next() {
        frontier.remove(dep_v);
        if !visited.insert(dep_v) {
            continue;
        }
        if target.contains(dep_v) {
            return Some(dep_v.to_string());
        }
        // Recurse into the dependency's own deps (if it is in the supplied set).
        if let Some(dep_m) = by_version.get(dep_v) {
            for d in &dep_m.depends_on {
                let dv = d.as_str();
                if !visited.contains(dv) {
                    frontier.insert(dv);
                }
            }
        }
    }
    None
}

/// The rollback body, run while holding the project advisory lock.
async fn rollback_locked(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    request: RollbackRequest,
    applied_by: &str,
) -> Result<RollbackOutcome, RollbackError> {
    let RollbackRequest { target, options: opts } = request;
    journal::ensure_journal(conn, cfg).await?;

    // Net-applied versions (latest event is `completed`), with their recorded
    // checksums, in ascending version order.
    let journal_rows = journal::applied(conn, cfg).await?;
    let mut net_applied: Vec<&AppliedEntry> = journal_rows
        .iter()
        .filter(|e| e.phase == Phase::Completed)
        .collect();
    net_applied.sort_by(|a, b| a.version.cmp(&b.version));

    // Resolve selection + reverse-topo ordering + all-up-front pre-flight into the
    // executable plan (pure / non-async — see `build_rollback_plan`).
    let plan = build_rollback_plan(&net_applied, migrations, &target, opts, cfg)?;

    // ---- EXECUTE -----------------------------------------------------------
    let mut outcome = RollbackOutcome {
        rolled_back: Vec::new(),
        skipped_irreversible: Vec::new(),
    };
    for step in plan {
        match step {
            RollbackStep::SkipIrreversible(m) => {
                tracing::warn!(
                    version = %m.version.as_str(),
                    project = %cfg.project_id,
                    "zeroship-migrate: force-skipping irreversible migration during rollback (down: None)"
                );
                outcome
                    .skipped_irreversible
                    .push(m.version.as_str().to_string());
            }
            RollbackStep::Down(m) => {
                rollback_one_transactional(conn, cfg, m, applied_by).await?;
                outcome.rolled_back.push(m.version.as_str().to_string());
            }
        }
    }
    Ok(outcome)
}

/// Roll back ONE migration transactionally: `BEGIN; SET LOCAL …; SET LOCAL ROLE
/// migrator; <down>; RESET ROLE; INSERT rolled_back (as admin); COMMIT`.
///
/// Atomic: the `down` + its `rolled_back` journal append commit together, so a
/// crash leaves either both (rolled back + recorded) or neither. The `down` runs
/// under the migrator role; the journal append runs as admin (the migrator has no
/// meta grant — C1), exactly mirroring [`apply_transactional`].
async fn rollback_one_transactional(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
) -> Result<(), RollbackError> {
    let down = m
        .down
        .as_deref()
        .expect("rollback_one_transactional is only called for RollbackStep::Down (down is Some)");
    let started = Instant::now();
    conn.batch_execute("BEGIN").await?;

    // Pin search_path + mandatory timeouts (SET LOCAL — vanish at COMMIT/ROLLBACK).
    if let Err(e) = conn.batch_execute(&set_local_session_sql(cfg, m)).await {
        let _ = conn.batch_execute("ROLLBACK").await;
        return Err(RollbackError::Db(e));
    }
    // Drop to the migrator role for the `<down>` ONLY (line-2 confinement). RESET
    // ROLE before the journal append so the admin writes the rolled_back event.
    if let Some(set_role) = set_local_role_sql(cfg) {
        if let Err(e) = conn.batch_execute(&set_role).await {
            let _ = conn.batch_execute("ROLLBACK").await;
            return Err(RollbackError::Db(e));
        }
    }
    // Run the `<down>` as the migrator.
    if let Err(e) = conn.batch_execute(down).await {
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zeroship-migrate: ROLLBACK failed after a down error");
        }
        return Err(RollbackError::DownFailed {
            version: m.version.as_str().to_string(),
            source: e,
        });
    }
    // RESET ROLE back to admin — still inside the txn — so the journal append runs
    // as the admin (the migrator cannot write the journal).
    if cfg.migrator_role.is_some() {
        if let Err(e) = conn.batch_execute("RESET ROLE").await {
            let _ = conn.batch_execute("ROLLBACK").await;
            return Err(RollbackError::Db(e));
        }
    }
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // Append the immutable `rolled_back` event in the SAME transaction, as admin.
    let meta = format!("\"{}\"", cfg.meta_schema.replace('"', "\"\""));
    if let Err(e) = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations_rolled_back
                     (version, name, checksum, rolled_back_by, exec_ms)
                 VALUES ($1, $2, $3, $4, $5)"
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
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zeroship-migrate: ROLLBACK failed after a rolled_back-insert error");
        }
        return Err(RollbackError::Journal(JournalError::Db(e)));
    }

    conn.batch_execute("COMMIT").await?;
    Ok(())
}

#[cfg(test)]
mod order_tests {
    use super::*;
    use crate::journal::Phase;
    use crate::migration::{Checksum, MigrationFlags, MigrationId};
    use std::collections::HashMap;

    fn m(version: MigrationId, depends_on: Vec<MigrationId>) -> Migration {
        let up = format!("CREATE TABLE t_{}()", version.as_str());
        Migration {
            version,
            name: "n".into(),
            up: up.clone(),
            down: None,
            checksum: Checksum::of(&up, None),
            flags: MigrationFlags::default(),
            owner_app: "app_test".into(),
            depends_on,
        }
    }

    fn pos(ordered: &[&Migration], v: &str) -> usize {
        ordered.iter().position(|x| x.version.as_str() == v).expect("present")
    }

    #[test]
    fn no_depends_on_is_pure_version_order() {
        // Three migrations, no edges: result is strict ascending version order.
        let a = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let c = MigrationId::generate();
        let set = vec![
            m(c.clone(), vec![]),
            m(a.clone(), vec![]),
            m(b.clone(), vec![]),
        ];
        let completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        let ordered = order_pending(&set, &completed).expect("order");
        let vs: Vec<&str> = ordered.iter().map(|x| x.version.as_str()).collect();
        assert_eq!(vs, vec![a.as_str(), b.as_str(), c.as_str()]);
    }

    #[test]
    fn later_version_runs_before_earlier_when_earlier_depends_on_it() {
        // The task's case: the EARLIER-version migration depends on the
        // LATER-version one, so topo order must run the later one FIRST, inverting
        // pure version order.
        let earlier = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let later = MigrationId::generate();
        assert!(later.as_str() > earlier.as_str(), "later must sort after earlier");
        // earlier depends_on later; later depends_on nothing.
        let set = vec![
            m(earlier.clone(), vec![later.clone()]),
            m(later.clone(), vec![]),
        ];
        let completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        let ordered = order_pending(&set, &completed).expect("order");
        assert!(
            pos(&ordered, later.as_str()) < pos(&ordered, earlier.as_str()),
            "the depended-on (later-version) migration must run first"
        );
    }

    #[test]
    fn cycle_is_a_clear_error() {
        let a = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MigrationId::generate();
        // a -> b -> a
        let set = vec![
            m(a.clone(), vec![b.clone()]),
            m(b.clone(), vec![a.clone()]),
        ];
        let completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        let err = order_pending(&set, &completed).unwrap_err();
        match err {
            ApplyError::DependencyCycle(members) => {
                assert!(members.contains(a.as_str()) && members.contains(b.as_str()));
            }
            other => panic!("expected DependencyCycle, got {other:?}"),
        }
    }

    #[test]
    fn missing_dependency_is_an_error() {
        let a = MigrationId::generate();
        let ghost = MigrationId::generate();
        let set = vec![m(a.clone(), vec![ghost.clone()])];
        let completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        let err = order_pending(&set, &completed).unwrap_err();
        assert!(matches!(err, ApplyError::MissingDependency { .. }), "got {err:?}");
    }

    #[test]
    fn dependency_already_in_journal_is_pre_satisfied() {
        // A dep that is already completed (in the journal, not in the pending set)
        // resolves fine and imposes no batch ordering.
        let done = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let pend = MigrationId::generate();
        let entry = AppliedEntry {
            version: done.as_str().to_string(),
            checksum: String::new(),
            phase: Phase::Completed,
        };
        let mut completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        completed.insert(done.as_str(), &entry);
        // Only `pend` is in the supplied set (depends on the completed `done`).
        let set = vec![m(pend.clone(), vec![done.clone()])];
        let ordered = order_pending(&set, &completed).expect("order");
        let vs: Vec<&str> = ordered.iter().map(|x| x.version.as_str()).collect();
        assert_eq!(vs, vec![pend.as_str()], "only the pending one is ordered");
    }
}
