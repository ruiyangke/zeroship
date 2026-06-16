//! Squash a contiguous prefix of applied history into one supersession (design
//! §5 "Squash/baseline old", scenario 49) — Plan 9 sub-feature B.
//!
//! As a project accumulates migrations, replaying `[v1..vN]` on a fresh build (or
//! reading the journal) becomes unwieldy. A **squash** migration `S` collapses
//! `[v1..vN]` into a single equivalent step: `S.up` is the combined/equivalent DDL
//! of `[v1..vN]` (for a fresh build) and `S.supersedes = [v1..vN]`.
//!
//! The journal is **append-only / immutable**, so squash does NOT delete the
//! `v1..vN` events. It models squash as a **supersession**:
//!
//! - **Existing DB** (already applied `[v1..vN]`) — [`squash`] (this module)
//!   journals `S` as a `completed` event WITHOUT running its `up` (baseline-style;
//!   the effect is already present) and records the `S → v_i` supersession edges.
//!   The old `v1..vN` events remain (immutable); `S` supersedes them.
//! - **Fresh DB** (none of `[v1..vN]` applied) — an ordinary
//!   [`apply`](crate::executor::apply) of a set containing `S` runs `S.up` once and
//!   SKIPS `v1..vN` (the executor's pending computation treats a version superseded
//!   by an applied/being-applied squash as satisfied — see
//!   [`crate::executor::compute_superseded`]). `v1..vN` are never double-applied,
//!   and a later migration that `depends_on` a superseded version is satisfied by
//!   `S`.
//!
//! # The all-or-none rule
//!
//! A squash is consistent only at the two extremes of its superseded set:
//! - **ALL** of `[v1..vN]` net-applied ⇒ [`squash`] records the supersession
//!   (baseline-style, no `up` run);
//! - **NONE** applied ⇒ the fresh path through [`apply`](crate::executor::apply)
//!   runs `S.up`.
//!
//! A **partial** overlap (some but not all of `[v1..vN]` applied) is an
//! inconsistent state and is refused — both here ([`SquashError::PartialOverlap`])
//! and in the executor ([`crate::executor::ApplyError::SquashPartialOverlap`]).
//!
//! # Safety (design §1)
//!
//! `S.up` is guard-checked (defense in depth, even though it is recorded-not-run on
//! the existing-DB path); squash is privileged (runs as ADMIN under the project
//! advisory lock); the append-only journal is preserved (an immutable `completed`
//! row stamped `kind = 'squash'` + immutable supersession edges).

use std::collections::HashSet;

use compio_postgres::Client;

use crate::db::ExecutorConfig;
use crate::guard::{GuardConfig, GuardError, SqlGuard};
use crate::journal::{self, JournalError, Phase};
use crate::migration::Migration;

/// What [`squash`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquashOutcome {
    /// The squash version recorded (`mig_…`).
    pub version: String,
    /// The versions it now supersedes (`[v1..vN]`).
    pub superseded: Vec<String>,
    /// `true` if this was an idempotent re-squash (the squash was already
    /// net-applied); `false` if newly recorded.
    pub already_present: bool,
}

/// Error from [`squash`].
#[derive(Debug, thiserror::Error)]
pub enum SquashError {
    /// A database error outside a guarded/journaled step.
    #[error("db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// A journal operation failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// The squash's `up` was denied by the guard (RCE / priv-esc / cross-tenant /
    /// file / network). `S.up` is held to the same parse-time deny-list as any `up`
    /// (defense in depth), even though it is recorded-not-run here. Nothing was
    /// journaled.
    #[error("squash {version} denied by guard: {source}")]
    Guard {
        /// The squash version.
        version: String,
        /// The underlying guard rejection.
        #[source]
        source: GuardError,
    },
    /// The squash declares no `supersedes` set — a squash that supersedes nothing
    /// is malformed (it would be an ordinary migration). Nothing was journaled.
    #[error("squash {version} declares an empty `supersedes` set; a squash must supersede at least one version")]
    NoSupersedes {
        /// The squash version.
        version: String,
    },
    /// Not every version in `supersedes` is net-applied — this is the existing-DB
    /// path, which records the supersession only when ALL `[v1..vN]` are already
    /// applied. NONE applied is the FRESH path (use
    /// [`apply`](crate::executor::apply), which runs `S.up`); a partial set is the
    /// inconsistent [`PartialOverlap`](SquashError::PartialOverlap). Nothing was
    /// journaled.
    #[error(
        "squash {version} cannot be recorded: only {applied} of {total} superseded versions are \
         net-applied. squash() records a supersession only when ALL are applied (existing DB). If \
         NONE are applied, apply the squash normally (its up runs on a fresh DB)."
    )]
    NotAllApplied {
        /// The squash version.
        version: String,
        /// How many superseded versions are net-applied.
        applied: usize,
        /// The total number it supersedes.
        total: usize,
    },
    /// A PARTIAL overlap: some but not all of `[v1..vN]` are net-applied — an
    /// inconsistent state (a squash spans a clean prefix; a partial application of
    /// that prefix means history is not at a squashable boundary). Nothing was
    /// journaled. (Distinguished from [`NotAllApplied`](SquashError::NotAllApplied)
    /// only for a clearer message; both refuse.)
    #[error(
        "squash {version} has a partial overlap: {applied} of {total} superseded versions are \
         net-applied. A squash requires either ALL applied (record it) or NONE applied (apply it)."
    )]
    PartialOverlap {
        /// The squash version.
        version: String,
        /// How many superseded versions are net-applied.
        applied: usize,
        /// The total number it supersedes.
        total: usize,
    },
}

/// Record `squash_migration` as a supersession over an existing DB that already
/// applied all of `squash_migration.supersedes` (design §5, scenario 49).
///
/// Journals `S` as a `completed` event stamped `kind = 'squash'` WITHOUT running
/// its `up` (the effect of `[v1..vN]` is already present), and records the `S →
/// v_i` supersession edges — all as ADMIN, under the project advisory lock.
/// Idempotent if `S` is already net-applied. Refuses unless ALL of
/// `S.supersedes` are net-applied (NONE = use [`apply`](crate::executor::apply);
/// partial = inconsistent).
///
/// `applied_by` is the actor recorded in the journal (operator / admin).
///
/// # Errors
/// - [`SquashError::Guard`] — `S.up` was denied.
/// - [`SquashError::NoSupersedes`] — `S` supersedes nothing.
/// - [`SquashError::NotAllApplied`] / [`SquashError::PartialOverlap`] — the
///   superseded set is not fully net-applied.
/// - [`SquashError::Db`] / [`SquashError::Journal`] — infrastructure failures.
pub async fn squash(
    conn: &Client,
    cfg: &ExecutorConfig,
    squash_migration: &Migration,
    applied_by: &str,
) -> Result<SquashOutcome, SquashError> {
    let version = squash_migration.version.as_str();

    // A squash must supersede something.
    if squash_migration.supersedes.is_empty() {
        return Err(SquashError::NoSupersedes {
            version: version.to_string(),
        });
    }

    // GUARD (defense in depth) — BEFORE the lock, no DB needed. `S.up` is held to
    // the same deny-list as any up, even though it is recorded-not-run here.
    let guard = SqlGuard::new(GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    });
    guard
        .check(&squash_migration.up)
        .map_err(|source| SquashError::Guard {
            version: version.to_string(),
            source,
        })?;

    // Privileged: serialize against all migration activity, exactly like apply.
    conn.execute(
        "SELECT pg_advisory_lock(hashtext($1)::bigint)",
        &[&cfg.project_id],
    )
    .await?;
    let result = squash_locked(conn, cfg, squash_migration, applied_by).await;
    let unlock = conn
        .execute(
            "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
            &[&cfg.project_id],
        )
        .await;
    match result {
        Ok(o) => unlock.map(|_| o).map_err(SquashError::Db),
        Err(e) => Err(e),
    }
}

/// The squash body, run while holding the project advisory lock.
async fn squash_locked(
    conn: &Client,
    cfg: &ExecutorConfig,
    squash_migration: &Migration,
    applied_by: &str,
) -> Result<SquashOutcome, SquashError> {
    journal::ensure_journal(conn, cfg).await?;

    let version = squash_migration.version.as_str();
    let supersedes: Vec<&str> = squash_migration
        .supersedes
        .iter()
        .map(crate::migration::MigrationId::as_str)
        .collect();

    // Net-applied state (latest event per version = completed).
    let applied = journal::applied(conn, cfg).await?;
    let net: HashSet<&str> = applied
        .iter()
        .filter(|e| e.phase == Phase::Completed)
        .map(|e| e.version.as_str())
        .collect();

    // Idempotent: the squash is already recorded.
    if net.contains(version) {
        return Ok(SquashOutcome {
            version: version.to_string(),
            superseded: supersedes.iter().map(ToString::to_string).collect(),
            already_present: true,
        });
    }

    // All-or-none: every superseded version must be net-applied (existing-DB path).
    let total = supersedes.len();
    let applied_count = supersedes.iter().filter(|v| net.contains(*v)).count();
    if applied_count == 0 {
        return Err(SquashError::NotAllApplied {
            version: version.to_string(),
            applied: 0,
            total,
        });
    }
    if applied_count != total {
        return Err(SquashError::PartialOverlap {
            version: version.to_string(),
            applied: applied_count,
            total,
        });
    }

    // ALL applied: journal the squash as a `completed` 'squash' event WITHOUT
    // running its `up`, plus the supersession edges — ADMIN, append-only.
    journal::record_baseline(
        conn,
        cfg,
        journal::BaselineRecord {
            version,
            name: &squash_migration.name,
            checksum: squash_migration.checksum.as_str(),
            applied_by,
            kind: "squash",
            supersedes: &supersedes,
        },
    )
    .await?;

    Ok(SquashOutcome {
        version: version.to_string(),
        superseded: supersedes.iter().map(ToString::to_string).collect(),
        already_present: false,
    })
}
