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
//! 5. for each pending migration: sets `statement_timeout` + `lock_timeout`,
//!    runs the **[`SqlGuard`]** over the `up` SQL FIRST (deny ⇒ abort the whole
//!    apply; the DDL never executes), then applies either:
//!    - **transactionally** (default): `BEGIN; <up>; INSERT journal; COMMIT` —
//!      DDL + journal atomic, so a crash leaves applied+recorded *or* neither;
//!    - **non-transactionally** (opt-in, e.g. `CREATE INDEX CONCURRENTLY`):
//!      two-phase `started` marker → run `<up>` → `completed` row + clear
//!      marker. A lone `started` marker on a re-run triggers the recovery path.
//! 6. releases the lock.
//!
//! Runs out-of-band at deploy, async on compio — ZERO tokio.

use std::collections::HashMap;
use std::time::Instant;

use compio_postgres::Client;

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

/// Pin `search_path` to the project schema and set the mandatory per-statement
/// timeouts (§1.5). Applied on the session before each migration's SQL runs.
async fn configure_session(conn: &Client, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
    // search_path: project schema first, then the meta schema (so the journal
    // is reachable unqualified if a migration ever needed it — it does not, we
    // always qualify). Identifiers are platform-controlled but quoted.
    let stmt = format!(
        "SET search_path TO \"{}\", \"{}\"; SET statement_timeout = {}; SET lock_timeout = {};",
        cfg.project_schema.replace('"', "\"\""),
        cfg.meta_schema.replace('"', "\"\""),
        cfg.statement_timeout_ms(),
        cfg.lock_timeout_ms(),
    );
    conn.batch_execute(&stmt).await?;
    Ok(())
}

/// Apply the project's pending migrations (design §2.3). Idempotent: a re-run
/// with no new migrations is a no-op.
///
/// `applied_by` is the actor recorded in the journal (`app/actor/AI`).
///
/// # Errors
/// - [`ApplyError::Guard`] — a pending migration's `up` SQL was denied; the
///   whole apply aborts and that migration never executes.
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
    let result = apply_locked(conn, cfg, migrations, applied_by).await;
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

    let mut outcome = ApplyOutcome {
        applied: Vec::new(),
        skipped: completed.keys().map(|v| (*v).to_string()).collect(),
        recovered: Vec::new(),
    };

    for m in pending {
        let version = m.version.as_str();

        // GUARD GATE — run BEFORE any execution. A denial aborts the entire
        // apply; the migration's DDL never runs and no journal row is written.
        guard.check(&m.up).map_err(|source| ApplyError::Guard {
            version: version.to_string(),
            source,
        })?;

        // Mandatory timeouts + pinned search_path for this statement.
        configure_session(conn, cfg).await?;

        let had_inflight = started.contains_key(version);

        if m.flags.transactional {
            apply_transactional(conn, cfg, m, applied_by).await?;
        } else {
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

    // Run the migration's up SQL.
    if let Err(e) = conn.batch_execute(&m.up).await {
        // Roll back; report the failure. No journal row was written.
        let _ = conn.batch_execute("ROLLBACK").await;
        return Err(ApplyError::MigrationFailed {
            version: m.version.as_str().to_string(),
            source: e,
        });
    }

    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // Journal the completed row in the SAME transaction.
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
        let _ = conn.batch_execute("ROLLBACK").await;
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

    if had_inflight {
        // Recovery path: a prior run wrote `started` then crashed before
        // `completed`. Inspect the real state and make the apply idempotent.
        recover_non_transactional(conn, cfg, m).await?;
    } else {
        journal::record_started(conn, cfg, version, &m.name, m.checksum.as_str(), applied_by)
            .await?;
    }

    let started = Instant::now();
    // Run the up SQL OUTSIDE any transaction (CONCURRENTLY etc. forbid it).
    conn.batch_execute(&m.up)
        .await
        .map_err(|e| ApplyError::MigrationFailed {
            version: version.to_string(),
            source: e,
        })?;
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // Phase 2: immutable completed row + clear the marker.
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

/// Idempotent recovery for a crashed non-transactional migration (design §2.4).
///
/// A `started` marker with no `completed` row means the DDL may have partially
/// run. For the canonical case — `CREATE INDEX CONCURRENTLY` interrupted — the
/// index is left `INVALID`. We find every invalid index in the project schema
/// and drop it, so the subsequent re-run of `<up>` rebuilds it cleanly. Other
/// non-txn ops (`ALTER TYPE … ADD VALUE`, `VACUUM`) are themselves idempotent /
/// safe to re-run, so the marker is simply cleared and the SQL re-applied.
async fn recover_non_transactional(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
) -> Result<(), ApplyError> {
    // Drop INVALID indexes in the project schema — the residue of an
    // interrupted CONCURRENTLY build. Querying pg_index.indisvalid = false.
    let invalid: Vec<String> = conn
        .query(
            "SELECT c.relname
               FROM pg_index x
               JOIN pg_class c ON c.oid = x.indexrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1 AND x.indisvalid = false",
            &[&cfg.project_schema],
        )
        .await?
        .into_iter()
        .map(|r| r.get::<_, String>("relname"))
        .collect();

    for idx in invalid {
        let stmt = format!(
            "DROP INDEX IF EXISTS \"{}\".\"{}\"",
            cfg.project_schema.replace('"', "\"\""),
            idx.replace('"', "\"\""),
        );
        conn.batch_execute(&stmt).await?;
    }

    // Clear the stale marker; the caller re-runs `<up>` and re-records.
    journal::clear_inflight(conn, cfg, m.version.as_str()).await?;
    Ok(())
}
