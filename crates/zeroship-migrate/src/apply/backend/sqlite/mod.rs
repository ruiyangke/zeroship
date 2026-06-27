//! The SQLite [`MigrationBackend`](crate::backend::MigrationBackend) impl
//! (SQLite-parity design §2, P2: confinement folded in).
//!
//! `SqliteBackend` is the security core for SQLite migrations. It owns a
//! dedicated, hardened, CDC-free connection ([`actor::MigrationActor`], §2.1.1)
//! and enforces line-2 confinement through a two-mode `prepare`-time authorizer
//! ([`authorizer`], §2.5.1) — the runtime analog of Postgres's least-privilege
//! `migrator` role. The journal lives in an attached `_mig` database, immutable by
//! authorizer construction + DEFENSIVE + append-only triggers (§2.2.1), with the
//! native `event_seq` AUTOINCREMENT PK as the total order (§2.2). One migration's DDL and
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
mod backfill_sql;
mod drift_sql;
mod dump_sql;
mod journal_sql;
pub mod rebuild_sql;
mod rollback_sql;

use std::collections::HashMap;

use crate::apply::backend::MigrationBackend;
use crate::apply::baseline::{BaselineError, BaselineOutcome};
use crate::conn::ExecutorConfig;
use crate::apply::drift::{ChecksumDriftReport, DriftError, SchemaSnapshot};
use crate::apply::executor::{ApplyError, PreconditionVerdict, RollbackError};
use crate::apply::journal::{AppliedEntry, JournalError};
use crate::model::migration::Migration;
use zeroship_schema::query::SqlDialect;

pub use actor::{MigrationActor, SqliteActorError};
pub use authorizer::Mode;
pub use journal_sql::LoadedVersion;
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
    /// EngineJournal → INSERT journal (event_seq AUTOINCREMENT) → COMMIT. Idempotent:
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
    /// logical shape (§2.2) — window-function net-state over the native event_seq PK.
    pub async fn applied_sqlite(&self) -> Result<Vec<AppliedEntry>, SqliteActorError> {
        journal_sql::applied(&self.actor).await
    }

    /// Run (or resume) a SQLite **batched backfill** directly (the SQLite analog of
    /// [`crate::backfill::run_backfill_bounded`], §2.3.1) — the checkpointed /
    /// crash-fuzz seam tests drive. `set_clause` / `filter` are the inline SQL the
    /// shared assembler ([`crate::dml::assemble_backfill_clauses`]) renders; the
    /// executor-internal direct seam has NO approval gate (the generic executor
    /// gates approval before reaching the backend's `run_backfill_step`). Stops after
    /// at most `max_batches` committed batches (`None` = run to completion).
    ///
    /// # Errors
    /// [`crate::backfill::BackfillError`] on a malformed spec, an unsafe cursor
    /// column, a cursor-column mutation, a resumable batch failure, or a poisoned
    /// connection.
    pub async fn run_backfill_bounded_sqlite(
        &self,
        spec: &crate::ops::backfill::BackfillSpec,
        set_clause: &str,
        filter: Option<&str>,
        applied_by: &str,
        max_batches: Option<u64>,
    ) -> Result<crate::ops::backfill::BackfillOutcome, crate::ops::backfill::BackfillError> {
        backfill_sql::run_backfill_bounded(
            &self.actor,
            spec,
            set_clause,
            filter,
            applied_by,
            max_batches,
        )
        .await
    }

    /// Roll back ONE migration's `down` ADDITIVELY (§2.7, P5) + append a
    /// `rolled_back` event, atomically. The direct executor-internal seam (no
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

    /// Serialize the LIVE `main` schema as a deterministic CREATE-statement script
    /// for the `dump` command (engine-agnostic `dump` parity with the PG
    /// `pg_dump --schema-only` leg). Tables/views before indexes/triggers, each
    /// name-ordered; the `_mig` journal + `sqlite_*` internals never leak (§2.5.2).
    /// The bin appends the SAME applied-versions trailer the PG `dump` writes.
    ///
    /// # Errors
    /// [`SqliteActorError`] on a `sqlite_master` read failure.
    pub async fn dump_schema_sqlite(&self) -> Result<String, SqliteActorError> {
        dump_sql::dump_schema(&self.actor).await
    }

    /// `load` (a.k.a. `db:setup`) — RESTORE a dumped schema's DDL onto `main`. The
    /// SQLite peer of piping `schema.sql` into `psql`: the operator-/engine-generated
    /// dump body is replayed verbatim under engine mode (the Trusted/restore posture —
    /// this is an operator restore of a dump, not an untrusted creator `up`). Runs as
    /// one `execute_batch` (the dump body is multi-statement). The `_mig` journal is a
    /// SEPARATE attached DB and the dump never references it, so this only recreates
    /// `main` objects.
    ///
    /// # Errors
    /// [`SqliteActorError`] on a DDL failure (e.g. a malformed dump body).
    pub async fn restore_schema_sqlite(&self, ddl: &str) -> Result<(), SqliteActorError> {
        // Engine mode: an operator dump restore may CREATE tables/indexes/triggers/
        // views on `main`; the engine authorizer allows `main` DDL in EngineJournal.
        // (A no-op empty body is harmless.)
        if ddl.trim().is_empty() {
            return Ok(());
        }
        self.actor.set_mode(authorizer::Mode::EngineJournal).await?;
        self.actor.exec(ddl).await
    }

    /// Net-applied migrations as `(version, checksum, name)` for the dump trailer
    /// (M1+M2) — read straight from the `_mig` journal so the dumped checksum/name
    /// are the JOURNAL's, never re-derived from `--dir`. Per version, the LATEST
    /// event must be `applied` (net-applied); its `name`/`checksum` are taken from
    /// that latest completed event. Ordered by version (the trailer order).
    ///
    /// # Errors
    /// [`SqliteActorError`] on a journal read failure.
    pub async fn net_applied_trailer_sqlite(
        &self,
    ) -> Result<Vec<(String, String, String)>, SqliteActorError> {
        self.actor.set_mode(authorizer::Mode::EngineJournal).await?;
        let sql = format!(
            "WITH ranked AS ( \
                 SELECT version, name, checksum, event_kind, \
                        ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn \
                   FROM \"_mig\".schema_migrations \
             ) \
             SELECT version, name, checksum FROM ranked \
              WHERE rn = 1 AND event_kind = '{applied}' \
              ORDER BY version",
            applied = crate::apply::journal::EventKind::Applied.as_str()
        );
        let rows = self.actor.query(&sql).await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let version = r.first().and_then(|c| c.clone()).unwrap_or_default();
            let name = r.get(1).and_then(|c| c.clone()).unwrap_or_default();
            let checksum = r.get(2).and_then(|c| c.clone()).unwrap_or_default();
            out.push((version, name, checksum));
        }
        Ok(out)
    }

    /// `load` first-entry guard (H2) — run BEFORE any `main` mutation. Refuses
    /// (errors, nothing touched) if `main` already carries user objects (any
    /// `sqlite_master` row that is not an internal `sqlite_*` object) OR if the
    /// journal already records net-applied migrations. `load` bootstraps a FRESH DB;
    /// `restore_schema_sqlite` mutates `main`, so this MUST be checked before it
    /// (the in-`record_loaded_versions` journal check fires only AFTER the restore).
    ///
    /// # Errors
    /// [`SqliteActorError`] if `main` is non-empty / the journal is already managed /
    /// on a probe failure.
    pub async fn ensure_fresh_load_target_sqlite(&self) -> Result<(), SqliteActorError> {
        // (a) `main` user objects. Internal `sqlite_*` objects (autoindex, the
        //     `sqlite_sequence`/`sqlite_stat*` bookkeeping) are NOT user data.
        self.actor.set_mode(authorizer::Mode::EngineJournal).await?;
        let rows = self
            .actor
            .query(
                "SELECT count(*) FROM main.sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%'",
            )
            .await?;
        let user_objects: i64 = rows
            .first()
            .and_then(|r| r.first())
            .and_then(|c| c.as_deref())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if user_objects > 0 {
            return Err(SqliteActorError::Exec(format!(
                "cannot load: the target database already has {user_objects} user \
                 object(s) on `main`; `load` targets a fresh/empty database"
            )));
        }
        // (b) journal first-entry check (a managed DB with an emptied `main` is still
        //     already-managed). Reuse the same net-applied probe `load` records over.
        journal_sql::ensure_journal(&self.actor).await?;
        let net = journal_sql::applied(&self.actor)
            .await?
            .into_iter()
            .filter(|e| e.phase == crate::apply::journal::Phase::Completed)
            .count();
        if net > 0 {
            return Err(SqliteActorError::Exec(format!(
                "cannot load: the journal already records {net} net-applied \
                 migration(s); `load` targets a fresh/empty database (a DB the engine \
                 already manages cannot be loaded over)"
            )));
        }
        Ok(())
    }

    /// `load` — journal the dump trailer's versions as `baseline`-kind `completed`
    /// events WITHOUT running any `up` (the DDL was already restored by
    /// [`restore_schema_sqlite`](Self::restore_schema_sqlite)). First-entry-only: it
    /// refuses if the journal already records net-applied migrations (the
    /// already-managed guard). Returns how many versions were journaled.
    ///
    /// # Errors
    /// [`SqliteActorError`] if the DB is already engine-managed or on a journal-write
    /// failure.
    pub async fn record_loaded_versions_sqlite(
        &self,
        versions: &[journal_sql::LoadedVersion],
        applied_by: &str,
    ) -> Result<usize, SqliteActorError> {
        journal_sql::record_loaded_versions(&self.actor, versions, applied_by).await
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
    type SessionSnapshot = ();

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Sqlite
    }

    fn ddl_is_transactional(&self) -> bool {
        true
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

    async fn snapshot_session(&self) -> Result<Self::SessionSnapshot, ApplyError> {
        // No GUCs / session settings to restore on SQLite — confinement is by
        // authorizer state, not by per-session SET LOCAL.
        Ok(())
    }

    async fn restore_session(&self, _snap: &Self::SessionSnapshot) -> Result<(), ApplyError> {
        Ok(())
    }

    async fn reset_role_best_effort(&self) {
        // No roles on SQLite; the authorizer mode is reset structurally per apply.
    }

    // -- per-migration confined apply ---------------------------------------

    async fn apply_one(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
        _had_inflight: bool,
        supersedes: &[&str],
        kind: &str,
    ) -> Result<bool, ApplyError> {
        if self.uses_two_phase_path(m) {
            return Err(ApplyError::Backend(
                "sqlite backend: non-transactional apply does not exist on SQLite (design §2.3/L3)"
                    .to_string(),
            ));
        }
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
        // **PR10 Part B — existence-guard catalog probe (SQLite).** Mirror the PG
        // probe: read the live SQLite catalog and `decide` BEFORE running the `up`.
        // The read + the additive apply both run under the SAME held project lock +
        // atomic boundary `execute_pending` already enforces (apply_one is called
        // under the held lock), so there is no probe→act TOCTOU window.
        //
        // - RunBare       → normal `apply_one_additive`.
        // - SatisfiedNoop → journal the completed row WITHOUT running the `up` DDL.
        // - FailDrift     → typed `ExistenceGuardDrift` (parity with the PG arm) —
        //                   never a silent skip over a divergence.
        if let Some(probe) = &m.existence_guard {
            let live = self.snapshot_schema_sqlite().await.map_err(|e| {
                ApplyError::Backend(format!("sqlite existence-guard snapshot failed: {e}"))
            })?;
            match crate::render::existence_probe::decide(probe, &live, SqlDialect::Sqlite) {
                crate::render::existence_probe::GuardVerdict::RunBare => {
                    return journal_sql::apply_one_additive(&self.actor, m, applied_by)
                        .await
                        .map(|_| false)
                        .map_err(apply_err);
                }
                crate::render::existence_probe::GuardVerdict::SatisfiedNoop => {
                    return journal_sql::journal_satisfied_noop(&self.actor, m, applied_by)
                        .await
                        .map(|_| false)
                        .map_err(apply_err);
                }
                crate::render::existence_probe::GuardVerdict::FailDrift(d) => {
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
        journal_sql::apply_one_additive(&self.actor, m, applied_by)
            .await
            .map(|_| false)
            .map_err(apply_err)
    }

    async fn rollback_one_transactional(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        // P5 ADDITIVE rollback (§2.7): reverse the `down` (DROP TABLE/COLUMN/INDEX,
        // RENAME) transactionally + append a `rolled_back` event. A rebuild-needing
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
        Ok(crate::apply::drift::compare_applied_to_set(&applied, migrations))
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

    // -- squash -------------------------------------------------------------

    async fn record_squash(
        &self,
        _cfg: &ExecutorConfig,
        squash_migration: &Migration,
        _applied_by: &str,
        _supersedes: &[&str],
    ) -> Result<(), ApplyError> {
        // Squash is an OPERATOR-authored supersession over an existing project's PG
        // migration history. The SQLite dev leg applies only TRUSTED
        // descriptor-generated migrations (empty `supersedes`/`renames`), and the
        // declarative author never emits a squash — so a squash reaching the SQLite
        // backend is a routing bug. Fail closed with a clear error rather than
        // silently journaling a supersession the dev path never produces (mirrors
        // `rebuild_one` on the PG backend, and `evaluate_preconditions` here).
        Err(ApplyError::Backend(format!(
            "sqlite backend: squash requested for '{}' — the SQLite descriptor author \
             never produces squashes (routing bug)",
            squash_migration.version.as_str()
        )))
    }

    // -- declarative-only structured ops (P6a) ------------------------------

    async fn rebuild_one(
        &self,
        spec: &SqliteRebuildSpec,
        m: &Migration,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
    ) -> Result<(), ApplyError> {
        // **PR9b per-version scope (executor-layer defense in depth).** A rebuild on a
        // populated table is destructive (drop + recreate + copy; `m.flags.destructive`
        // is true for a `SqliteRebuild` by construction), so under
        // `ApprovalScope::Versions` it runs ONLY if the operator individually reviewed
        // THIS rebuild's version — mirroring the engine's per-version gate and keyed on
        // the same rule as `PlanStep::approval_scope_version`. So a direct seam caller
        // driving `rebuild_one` cannot bypass the per-version scope. Refuse BEFORE
        // touching the table, so a non-scoped rebuild rebuilds NOTHING.
        if m.flags.destructive && !scope.admits(m.version.as_str()) {
            return Err(ApplyError::ApprovalNotScoped {
                version: m.version.as_str().to_string(),
            });
        }
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

    async fn run_backfill_step(
        &self,
        _cfg: &ExecutorConfig,
        spec: &crate::ops::backfill::BackfillSpec,
        approval: crate::approval::Approval,
        _scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<crate::apply::executor::ApplyOutcome, ApplyError> {
        // PR6b — the SQLite batched/resumable backfill executor (§2.3.1), the SQLite
        // analog of the PG writable-CTE windowed UPDATE. Completes the §1.1 "one
        // script, both backends, DDL+DML" headline: a batched backfill is now
        // PORTABLE on BOTH backends. Each batch is its own committed
        // `BEGIN IMMEDIATE … COMMIT` on the single hardened connection, resumable
        // from the committed progress cursor in `_mig`.
        //
        // Gate — approval (defense-in-depth; design §1.6), mirroring the PG
        // `run_backfill`'s own Gate 1: a backfill mutates table data, so it requires
        // Approval::Approved. Refuse BEFORE any batch runs. `_lock_mode` is moot on
        // SQLite (the single actor serializes every statement structurally; there is
        // no project advisory lock to re-apply).
        if approval != crate::approval::Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        let outcome = backfill_sql::run_backfill_bounded(
            &self.actor,
            spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            applied_by,
            None,
        )
        .await
        .map_err(|e| ApplyError::Backend(format!("sqlite backfill step failed: {e}")))?;
        // Surface the backfill's name as an applied version only on completion
        // (a resumed-but-incomplete backfill reports nothing applied), mirroring PG.
        let applied = if outcome.complete {
            vec![spec.name.clone()]
        } else {
            Vec::new()
        };
        Ok(crate::apply::executor::ApplyOutcome {
            applied,
            skipped: Vec::new(),
            recovered: Vec::new(),
        })
    }

    async fn run_dml_step(
        &self,
        _cfg: &ExecutorConfig,
        version: &crate::model::migration::MigrationId,
        name: &str,
        template: &str,
        binds: &[crate::plan::BindValue],
        destructive: bool,
        owner_app: &str,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<bool, ApplyError> {
        // §PR6a — the SQLite one-shot DML executor. The `template` carries `?n`
        // placeholders; the binds are bound NATIVELY (never interpolated, §2.3.2).
        //
        // Defense-in-depth: a destructive DML (a `delete`) needs explicit approval,
        // mirroring the per-Migration gate + the PG `run_dml_step`. Refuse BEFORE
        // touching the journal or running the statement, so a refused destructive
        // DML applies NOTHING.
        if destructive && approval != crate::approval::Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        // **PR9b per-version scope (executor-layer defense in depth).** Mirrors the PG
        // `run_dml_step`: even under blanket `Approval::Approved`, a destructive DML
        // runs ONLY if `scope` admits its `version`, keyed on the same rule as
        // `PlanStep::approval_scope_version`. So a direct seam caller cannot bypass the
        // per-version scope. Fail-closed: refuse BEFORE the journal or the statement.
        if destructive && !scope.admits(version.as_str()) {
            return Err(ApplyError::ApprovalNotScoped {
                version: version.as_str().to_string(),
            });
        }
        // Net-applied-skip: if this sub-version is already journaled `completed`,
        // the re-run is a no-op (idempotency, §2.0.1).
        let already: bool = journal_sql::applied(&self.actor)
            .await
            .map_err(journal_err)
            .map_err(ApplyError::Journal)?
            .into_iter()
            .filter(|e| matches!(e.phase, crate::apply::journal::Phase::Completed))
            .any(|e| e.version == version.as_str());
        if already {
            return Ok(false);
        }
        // Map the plan binds to the transport-safe SQLite bind mirror (the shared
        // `?n`-binding seam — `SqliteBind::from_bind`).
        let sqlite_binds: Vec<crate::apply::backend::sqlite::actor::SqliteBind> =
            binds.iter().map(crate::apply::backend::sqlite::actor::SqliteBind::from_bind).collect();
        journal_sql::run_dml(
            &self.actor,
            version.as_str(),
            name,
            template,
            &sqlite_binds,
            owner_app,
            applied_by,
        )
        .await
        .map_err(apply_err)?;
        Ok(true)
    }

    fn online(&self) -> Option<&dyn crate::ops::expand_contract::OnlineSchemaChange> {
        // SQLite has NO online schema-change capability: a SQLite declarative rename
        // is routed to a `rebuild_one` (the 12-step offline rebuild), never
        // expand-contract, so `plan.renames` is structurally EMPTY on the SQLite leg
        // and `None` here is never reached with renames to drive (design §3.3 / H1).
        // Returning `None` (no PG `Client`) is what removes this backend's only
        // PG-driver dependency (L-b): nothing on the shared trait is PG-typed here.
        None
    }

    fn shadow(&self) -> Option<&dyn crate::ops::shadow::ShadowDryRun> {
        // SQLite has NO shadow dry-run capability (C3) — a DELIBERATE capability
        // gap, not a silent hole. The SQLite dev path applies only TRUSTED
        // descriptor-generated DDL (there is no untrusted/raw SQLite author whose
        // DDL would need previewing), and dev is recoverable (a local file the
        // developer can re-create), so a pre-apply shadow clone adds little. The
        // shadow exists to safely preview untrusted/AI-authored DDL before it
        // touches a DURABLE schema; neither condition holds here. A future
        // untrusted/prod non-PG engine WOULD provide one. Returning
        // `None` is honest: the engine's `dry_run` surfaces the explicit
        // `DryRunError::ShadowUnsupported`, never a fake "dry-run passed".
        None
    }

    fn pending_contracts(&self) -> Option<&dyn crate::apply::backend::CrossDeployObligations> {
        // SQLite has no cross-deploy pending-contract partition: a rebuild rename
        // is one atomic offline step, so there is no obligation to open or
        // recover. Generic callers treat `None` as empty/no-op.
        None
    }

    async fn baseline_one(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<BaselineOutcome, BaselineError> {
        // Delegate verbatim to the existing SQLite baseline body (first-entry /
        // idempotency over the net-state → atomic `kind='baseline'` journal write
        // under engine mode, the `up` NEVER run). SQLite has no project schema/lock
        // in the journal write (the single actor serializes structurally), so `cfg`
        // is unused here. Map the SQLite-specific outcome/error onto the neutral
        // trait types — the dialect split disappears at this boundary.
        journal_sql::baseline(&self.actor, m, applied_by)
            .await
            .map_err(|e| BaselineError::Backend(e.to_string()))
    }
}
