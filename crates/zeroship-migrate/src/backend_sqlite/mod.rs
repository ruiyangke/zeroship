//! The SQLite [`MigrationBackend`](crate::backend::MigrationBackend) impl
//! (SQLite-parity design §2, P2: confinement folded in).
//!
//! `SqliteBackend` is the security core for SQLite migrations. It owns a
//! dedicated, hardened, CDC-free connection ([`actor::MigrationActor`], §2.1.1)
//! and enforces line-2 confinement through a two-mode `prepare`-time authorizer
//! ([`authorizer`], §2.5.1) — the runtime analog of Postgres's least-privilege
//! `migrator` role. The journal lives in an attached `_mig` database, immutable by
//! authorizer construction + DEFENSIVE + append-only triggers (§2.2.1), with a
//! single shared monotonic `event_seq` counter (§2.2, M4). One migration's DDL and
//! its journal row commit atomically on the single connection, with the creator
//! `up` confined from `_mig` and the journal write done under engine mode, the
//! mode flip landing between separate prepares (§2.2.2).
//!
//! See the module-level docs of [`authorizer`] and [`actor`] for the mechanism;
//! every confinement claim is proven against a real temp-file SQLite in
//! `tests/sqlite_confinement.rs` / `tests/sqlite_journal.rs` /
//! `tests/sqlite_apply.rs`.

pub mod actor;
pub mod authorizer;
mod drift_sql;
mod journal_sql;
pub mod rebuild_sql;
mod rollback_sql;

use std::collections::HashMap;

use crate::backend::{MigrationBackend, SessionSnapshot};
use crate::db::ExecutorConfig;
use crate::drift::{ChecksumDriftReport, DriftError, SchemaSnapshot};
use crate::executor::{ApplyError, PreconditionVerdict, RollbackError};
use crate::journal::{AppliedEntry, JournalError};
use crate::migration::Migration;
use zeroship_schema::query::SqlDialect;

pub use actor::{MigrationActor, SqliteActorError};
pub use authorizer::Mode;
pub use rebuild_sql::{RebuildError, SqliteRebuildSpec};

/// The SQLite [`MigrationBackend`]. Holds the dedicated hardened migration actor
/// for ONE tenant. Construct via [`SqliteBackend::open`].
#[derive(Debug)]
pub struct SqliteBackend {
    actor: MigrationActor,
}

impl SqliteBackend {
    /// Open the hardened migration backend for one tenant.
    ///
    /// `app_path` is the tenant's `zs-<app_id>.sqlite`; `journal_path` is the
    /// tenant's separate journal file (`<app>.migrations.sqlite`). Both are
    /// engine-constructed from the authenticated `app_id`, never creator input
    /// (§2.5.2). The connection is hardened (§2.5.1) before any creator SQL can run.
    ///
    /// # Errors
    /// [`SqliteActorError`] on a failed open / hardening / sub-floor SQLite.
    pub fn open(
        app_path: &std::path::Path,
        journal_path: &std::path::Path,
    ) -> Result<Self, SqliteActorError> {
        Ok(Self {
            actor: MigrationActor::open(app_path, journal_path)?,
        })
    }

    /// Borrow the underlying actor (tests + the journal/apply helpers).
    #[must_use]
    pub fn actor(&self) -> &MigrationActor {
        &self.actor
    }

    /// Bootstrap the `_mig` journal (idempotent) under engine mode (§2.2.1).
    pub async fn ensure_journal_sqlite(&self) -> Result<(), SqliteActorError> {
        journal_sql::ensure_journal(&self.actor).await
    }

    /// Apply ONE additive migration atomically with confinement (§2.2.2). This is
    /// the P2 end-to-end seam: BEGIN IMMEDIATE → CreatorUp → run `up` →
    /// EngineJournal → allocate event_seq + INSERT journal → COMMIT. Idempotent:
    /// a version whose latest event is `completed` is a no-op (returns `false`).
    /// Returns `true` iff the migration was newly applied.
    ///
    /// M4: this is the EXECUTOR-INTERNAL direct seam — it has NO approval gate. The
    /// destructive/approval gate lives in the generic executor (`apply_locked`),
    /// which classifies a migration and demands an `Approval` for destructive ops
    /// BEFORE it ever calls down into a backend. Callers reaching this method
    /// directly (the additive-only P2 path + tests) have already cleared that gate.
    ///
    /// # Errors
    /// [`SqliteActorError`] on confinement denial / DDL failure (the transaction
    /// is rolled back, leaving the journal uncorrupted).
    pub async fn apply_one_additive(
        &self,
        m: &Migration,
        applied_by: &str,
    ) -> Result<bool, SqliteActorError> {
        journal_sql::apply_one_additive(&self.actor, m, applied_by).await
    }

    /// The net-applied + lone-`started` entries, mirroring the PG `applied()`
    /// logical shape (§2.2) — window-function net-state over the shared event_seq.
    pub async fn applied_sqlite(&self) -> Result<Vec<AppliedEntry>, SqliteActorError> {
        journal_sql::applied(&self.actor).await
    }

    /// Roll back ONE migration's `down` ADDITIVELY (§2.7, P5) + append a
    /// `_rolled_back` event, atomically. The direct executor-internal seam (no
    /// approval gate; the generic executor gates approval before reaching here).
    /// A rebuild-needing `down` is refused with
    /// [`RollbackError::SqliteRebuildRequired`](crate::executor::RollbackError::SqliteRebuildRequired);
    /// the rebuild is P3b.
    ///
    /// # Errors
    /// [`RollbackError`] on a rebuild-needing `down`, a confinement denial, a failed
    /// `down`, or a journal-write/commit failure.
    pub async fn rollback_one_additive(
        &self,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        rollback_sql::rollback_one_transactional(&self.actor, m, applied_by).await
    }

    /// Apply ONE 12-step table REBUILD atomically with confinement + journal it
    /// (§2.4, P3b). The rebuild DDL (drop-stale-temp / CREATE new / copy / drop old /
    /// rename / replay verbatim-captured indexes+triggers) runs under CreatorUp on
    /// `main`; the `foreign_keys` toggles straddle the transaction in engine-
    /// controlled autocommit windows (the SQLite in-txn no-op rule), the dependent-
    /// object capture + the UNSCOPED `foreign_key_check` integrity gate + the journal
    /// write run under EngineJournal, and the DDL + journal row commit atomically.
    /// `foreign_keys` is restored to ON in ALL paths.
    ///
    /// # This is the EXECUTOR-INTERNAL direct seam (no approval gate here)
    ///
    /// This inherent method runs the rebuild WITHOUT an approval gate — it is the raw
    /// dialect-coupled drive. As of P6a the GATED production path is the engine's
    /// generic [`apply_declarative`](crate::engine::MigrationEngine::apply_declarative),
    /// which classifies the rebuild's `destructive + requires_approval` journal
    /// migration and refuses an un-approved rebuild BEFORE calling down into
    /// [`MigrationBackend::rebuild_one`](crate::backend::MigrationBackend::rebuild_one)
    /// (which forwards here). So a caller reaching THIS inherent method directly
    /// (tests) has bypassed that gate and MUST gate approval itself; callers going
    /// through the engine get the gate for free.
    ///
    /// # Errors
    /// [`RebuildError`] on an FK-check abort, a confinement denial / DDL failure (the
    /// transaction is rolled back, leaving the original table intact and
    /// `foreign_keys` back ON), or a poisoned connection.
    pub async fn rebuild_one(
        &self,
        spec: &SqliteRebuildSpec,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RebuildError> {
        rebuild_sql::rebuild_one(&self.actor, spec, m, applied_by).await
    }

    /// Introspect the LIVE `main` (app file) schema into a dialect-agnostic
    /// [`SchemaSnapshot`] (§2.7) — the drift surface, the same shape the PG path
    /// returns. Recovers inline `__zsmask:` / `zsenc:` sentinels from
    /// `sqlite_master.sql`.
    ///
    /// # Errors
    /// [`DriftError`] on a `sqlite_master` / PRAGMA read failure.
    pub async fn snapshot_schema_sqlite(&self) -> Result<SchemaSnapshot, DriftError> {
        drift_sql::snapshot_schema(&self.actor).await
    }
}

/// Map a SQLite actor error onto the dialect-neutral `Backend` arm of [`ApplyError`].
fn apply_err(e: SqliteActorError) -> ApplyError {
    ApplyError::Backend(e.to_string())
}

/// Map a SQLite actor error onto the dialect-neutral `Backend` arm of [`JournalError`].
fn journal_err(e: SqliteActorError) -> JournalError {
    JournalError::Backend(e.to_string())
}

impl MigrationBackend for SqliteBackend {
    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }

    // -- connection / session I/O -------------------------------------------
    // The in-process lock is the single-actor serialization itself (§2.3): one
    // writer, one flume queue. Cross-process is P5b (NOT built here). So the
    // lock methods are honest no-ops — structural serialization already holds.

    async fn acquire_project_lock(&self, _project_id: &str) -> Result<(), ApplyError> {
        Ok(())
    }

    async fn release_project_lock(&self, _project_id: &str) -> Result<(), ApplyError> {
        Ok(())
    }

    async fn snapshot_session(&self) -> Result<SessionSnapshot, ApplyError> {
        // No GUCs / session settings to restore on SQLite — confinement is by
        // authorizer state, not by per-session SET LOCAL. Empty snapshot.
        Ok(SessionSnapshot::default())
    }

    async fn restore_session(&self, _snap: &SessionSnapshot) -> Result<(), ApplyError> {
        Ok(())
    }

    async fn reset_role_best_effort(&self) {
        // No roles on SQLite; the authorizer mode is reset structurally per apply.
    }

    // -- per-migration confined apply ---------------------------------------

    async fn apply_up_transactional(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
        supersedes: &[&str],
        kind: &str,
    ) -> Result<(), ApplyError> {
        // P2 covers the additive path (a caller-supplied CREATE TABLE up + the
        // atomic journal write). Squash/supersession + repeatable journaling are
        // P5/P6 surface; reject here rather than silently dropping the edges.
        if !supersedes.is_empty() {
            return Err(ApplyError::Backend(
                "sqlite backend P2: supersession (squash) journaling is not yet implemented (P5/P6)"
                    .to_string(),
            ));
        }
        if kind != "apply" {
            return Err(ApplyError::Backend(format!(
                "sqlite backend P2: journal kind '{kind}' not yet implemented (only 'apply')"
            )));
        }
        journal_sql::apply_one_additive(&self.actor, m, applied_by)
            .await
            .map(|_| ())
            .map_err(apply_err)
    }

    async fn configure_session_non_txn(
        &self,
        _cfg: &ExecutorConfig,
        _m: &Migration,
    ) -> Result<(), ApplyError> {
        // SQLite has no non-txn DDL path; validate_non_txn rejects transaction:false
        // before this is reached. If reached, it is a logic error.
        Err(ApplyError::Backend(
            "sqlite backend: non-transactional apply does not exist on SQLite (design §2.3/L3)"
                .to_string(),
        ))
    }

    async fn apply_up_non_transactional(
        &self,
        _cfg: &ExecutorConfig,
        _m: &Migration,
        _applied_by: &str,
        _had_inflight: bool,
        _supersedes: &[&str],
    ) -> Result<bool, ApplyError> {
        Err(ApplyError::Backend(
            "sqlite backend: non-transactional apply does not exist on SQLite (design §2.3/L3)"
                .to_string(),
        ))
    }

    async fn rollback_one_transactional(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        // P5 ADDITIVE rollback (§2.7): reverse the `down` (DROP TABLE/COLUMN/INDEX,
        // RENAME) transactionally + append a `_rolled_back` event. A rebuild-needing
        // `down` is refused with `SqliteRebuildRequired` (the rebuild is P3b).
        rollback_sql::rollback_one_transactional(&self.actor, m, applied_by).await
    }

    // -- parse-time validation ----------------------------------------------

    fn validate_non_txn(&self, m: &Migration) -> Result<(), ApplyError> {
        // Reject transaction:false at the dialect boundary (§2.3, L3): SQLite DDL is
        // transactional; there is no CONCURRENTLY / ADD VALUE non-txn path to
        // classify. A real guard at the seam, not an implicit assumption.
        if m.flags.transactional {
            Ok(())
        } else {
            Err(ApplyError::NonTxnUnsupportedOnDialect {
                version: m.version.as_str().to_string(),
                dialect: "sqlite",
            })
        }
    }

    // -- journal row I/O ----------------------------------------------------

    async fn ensure_journal(&self, _cfg: &ExecutorConfig) -> Result<(), JournalError> {
        journal_sql::ensure_journal(&self.actor)
            .await
            .map_err(journal_err)
    }

    async fn applied(&self, _cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        journal_sql::applied(&self.actor).await.map_err(journal_err)
    }

    async fn superseded_versions(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        journal_sql::superseded_versions(&self.actor)
            .await
            .map_err(journal_err)
    }

    async fn latest_completed_checksums(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<HashMap<String, String>, JournalError> {
        journal_sql::latest_completed_checksums(&self.actor)
            .await
            .map_err(journal_err)
    }

    // -- DB-coupled validation / introspection ------------------------------

    async fn check_checksum_drift(
        &self,
        _cfg: &ExecutorConfig,
        migrations: &[Migration],
    ) -> Result<ChecksumDriftReport, DriftError> {
        // P5: the comparison is dialect-agnostic (shared `compare_applied_to_set`);
        // only the journal read underneath is dialect-coupled. Read the net-applied
        // journal entries (SQLite window-function net-state) and feed the generic
        // comparison — identical rules to the PG path.
        let applied = journal_sql::applied(&self.actor)
            .await
            .map_err(|e| DriftError::Backend(e.to_string()))?;
        Ok(crate::drift::compare_applied_to_set(&applied, migrations))
    }

    async fn snapshot_schema(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<SchemaSnapshot, DriftError> {
        // P5: introspect the LIVE `main` (app file) schema via sqlite_master +
        // PRAGMAs into the same `SchemaSnapshot` shape the PG path returns, under
        // engine mode (§2.5.1). Recovers inline mask/encryption sentinels.
        drift_sql::snapshot_schema(&self.actor).await
    }

    // -- preconditions ------------------------------------------------------

    async fn evaluate_preconditions(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<PreconditionVerdict, ApplyError> {
        // Descriptor-generated SQLite migrations carry NO preconditions (the
        // declarative author never emits them), so the common case is trivially
        // `AllMet`. Precondition EVALUATION against a live SQLite schema is a later
        // capability; until then a migration that actually declares a precondition
        // fails closed rather than silently treating it as met (P6a only needs the
        // no-precondition path the descriptor diff produces).
        if m.preconditions.is_empty() {
            Ok(PreconditionVerdict::AllMet)
        } else {
            Err(ApplyError::Backend(
                "sqlite backend: precondition evaluation is a later-phase capability \
                 (descriptor migrations carry none)"
                    .to_string(),
            ))
        }
    }

    // -- declarative-only structured ops (P6a) ------------------------------

    async fn rebuild_one(
        &self,
        spec: &SqliteRebuildSpec,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), ApplyError> {
        // Drive the built, tested 12-step rebuild engine (the inherent
        // `SqliteBackend::rebuild_one`). The `RebuildError` is mapped onto the
        // dialect-neutral `Backend` arm so it flows through the generic engine's
        // `ApplyError` without leaking a SQLite type. Idempotency is the CALLER's
        // concern (the engine's net-state gate skips an already-completed rebuild),
        // exactly like the additive apply seam.
        rebuild_sql::rebuild_one(&self.actor, spec, m, applied_by)
            .await
            .map_err(|e| ApplyError::Backend(e.to_string()))
    }

    fn expand_conn(&self) -> Option<&compio_postgres::Client> {
        // SQLite has no online expand-contract drive: a SQLite declarative rename is
        // routed to a `rebuild_one`, never expand-contract, so `plan.renames` is
        // EMPTY on the SQLite leg and this `None` is never reached with renames to
        // drive (design §3.3 / P6a CONDITION ii / H1).
        None
    }
}
