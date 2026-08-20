//! The SQLite [`MigrationBackend`] impl
//! (confinement folded in).
//!
//! `SqliteBackend` is the security core for SQLite migrations. It owns a
//! dedicated, hardened, CDC-free connection ([`actor::MigrationActor`])
//! and enforces second-line confinement through a two-mode `prepare`-time authorizer
//! ([`authorizer`]) - the runtime analog of Postgres's least-privilege
//! `migrator` role. The journal lives in an attached `_mig` database, immutable by
//! authorizer construction + DEFENSIVE + append-only triggers, with the
//! native `event_seq` AUTOINCREMENT PK as the total order. One migration's DDL and
//! its journal row commit atomically on the single connection, with the creator
//! `up` confined from `_mig` and the journal write done under engine mode, the
//! mode flip landing between separate prepares.
//!
//! See the module-level docs of [`authorizer`] and [`actor`] for the mechanism.
//! Every claim above is proven against a real temp-file SQLite, split across two
//! files rather than the three this comment used to name - there is no
//! `tests/sqlite_journal.rs` and there never was, though the coverage it stood for
//! does exist:
//!
//! - `tests/sqlite_confinement.rs` covers the authorizer line. A creator `up` may
//!   not drop the `_mig` table (`confine_d_drop_mig_table_denied`) or its triggers
//!   (`confine_e_drop_mig_trigger_denied`), may not insert a journal row directly
//!   (`confine_f_direct_journal_insert_denied`), may not reach `_mig` through a
//!   trigger it defines (`confine_g_creator_trigger_writing_mig_denied`), and may
//!   not even read the journal (`confine_i_creator_read_of_mig_journal_denied`).
//! - `tests/sqlite_apply.rs` covers the journal's own properties. `event_seq` is a
//!   total order (`native_event_seq_is_monotonic`), rows resist UPDATE and DELETE
//!   under confinement (`journal_update_delete_denied_confined`) and again at the
//!   trigger when confinement is off (`journal_immutability_trigger_backstop` -
//!   the backstop is what makes append-only a property of the schema rather than
//!   of the authorizer alone), and a failing `up` takes its journal row with it
//!   (`failed_up_rolls_back_atomically`).
//!
//! Does NOT cover the mode flip as an interleaving. What is covered is each side
//! of it: `is_autocommit_detects_open_transaction` (sqlite_apply.rs) drives the
//! actor through `set_mode(EngineJournal)` and proves the transaction state it
//! produces is detectable, and `confine_g_creator_trigger_writing_mig_denied`
//! proves creator-defined SQL cannot reach `_mig` by deferring itself to a trigger.
//! What no test asserts is that engine mode is unreachable to creator SQL BETWEEN
//! the two prepares - the window is argued from the mode being flipped by the
//! backend rather than shown closed by a test that tries to open it. A hole, and
//! nothing else covers it.

pub mod actor;
pub mod authorizer;
mod backfill_sql;
mod drift_sql;
mod dump_sql;
mod identity_sql;
mod journal_sql;
mod primary_key_sql;
pub mod rebuild_sql;
mod rollback_sql;

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::apply::backend::MigrationBackend;
use crate::apply::baseline::{BaselineError, BaselineOutcome};
use crate::apply::drift::{ChecksumDriftReport, DriftError};
use crate::apply::executor::{
    authorize_existence_guard_schema, ApplyError, PreconditionVerdict, RollbackError,
};
use crate::apply::journal::{AppliedEntry, JournalError};
use crate::conn::ExecutorConfig;
use crate::model::migration::Migration;
use crate::model::snapshot::SchemaSnapshot;
use crate::render::plan::TableRebuildSpec;
use crate::schema::query::SqlDialect;

pub use actor::{MigrationActor, SqliteActorError};
pub use authorizer::Mode;
pub use journal_sql::LoadedVersion;
pub use rebuild_sql::RebuildError;

/// This backend's vendor identity, named ONCE for the whole `sqlite/` subtree.
///
/// The four apply-time SQL builders under this module (`backfill_sql`,
/// `identity_sql`, `primary_key_sql`, `rebuild_sql`) each spell identifiers into
/// SQL they send to a real SQLite database. They used to do it through the raw
/// crate-wide escape primitive, which reached no renderer at all — and because
/// they contained NO `SqlDialect::` literal, the one-dialect-literal grep read
/// them as clean: it looks for a FOREIGN literal, and "no literal" passes.
///
/// One const, read by all four, is the shape that makes their vendor greppable
/// without putting four literals in the tree. It mirrors
/// `render::backends::sqlite`'s own `DIALECT`.
const SQLITE_DIALECT: SqlDialect = SqlDialect::Sqlite;

/// The SQLite [`MigrationBackend`]. Holds the dedicated hardened migration actor
/// for ONE tenant. Construct via [`SqliteBackend::open`].
#[derive(Debug)]
pub struct SqliteBackend {
    actor: MigrationActor,
    journal_path: PathBuf,
    /// OS-backed whole-plan lock shared by every process opening this app file.
    project_lock: File,
    project_lock_path: PathBuf,
    project_lock_held: Mutex<bool>,
}

impl SqliteBackend {
    /// Open the hardened migration backend for one tenant.
    ///
    /// `app_path` is the tenant's `zs-<app_id>.sqlite`; `journal_path` is the
    /// tenant's separate journal file (`<app>.migrations.sqlite`). Both are
    /// engine-constructed from the authenticated `app_id`, never creator input
    /// The connection is hardened before any creator SQL can run.
    ///
    /// # Errors
    /// [`SqliteActorError`] on a failed open / hardening / sub-floor SQLite.
    pub fn open(app_path: &Path, journal_path: &Path) -> Result<Self, SqliteActorError> {
        let actor = MigrationActor::open(app_path, journal_path)?;
        let project_lock_path = project_lock_path(app_path)?;
        let project_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&project_lock_path)
            .map_err(|error| {
                SqliteActorError::Open(format!(
                    "open project lock {}: {error}",
                    project_lock_path.display()
                ))
            })?;
        Ok(Self {
            actor,
            journal_path: journal_path.to_path_buf(),
            project_lock,
            project_lock_path,
            project_lock_held: Mutex::new(false),
        })
    }

    /// Borrow the underlying actor (tests + the journal/apply helpers).
    #[must_use]
    pub fn actor(&self) -> &MigrationActor {
        &self.actor
    }

    /// Bootstrap the `_mig` journal (idempotent) under engine mode.
    pub async fn ensure_journal_sqlite(&self) -> Result<(), SqliteActorError> {
        journal_sql::ensure_journal(&self.actor).await
    }

    /// Apply ONE additive migration atomically with confinement. This is
    /// the end-to-end seam: BEGIN IMMEDIATE → CreatorUp → run `up` →
    /// EngineJournal → INSERT journal (event_seq AUTOINCREMENT) → COMMIT. Idempotent:
    /// a version whose latest event is `completed` is a no-op (returns `false`).
    /// Returns `true` iff the migration was newly applied.
    ///
    /// This is the EXECUTOR-INTERNAL direct seam — it has NO approval gate. The
    /// destructive/approval gate lives in the generic executor (`apply_locked`),
    /// which classifies a migration and demands an `Approval` for destructive ops
    /// BEFORE it ever calls down into a backend. Callers reaching this method
    /// directly (the additive-only path + tests) have already cleared that gate.
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

    /// The net-applied + lone-`started` entries, mirroring the PG `applied`
    /// logical shape — window-function net-state over the native event_seq PK.
    pub async fn applied_sqlite(&self) -> Result<Vec<AppliedEntry>, SqliteActorError> {
        journal_sql::applied(&self.actor).await
    }

    /// Run (or resume) a SQLite **batched backfill** directly (the SQLite analog of
    /// the PG bounded backfill runner) — the checkpointed /
    /// crash-fuzz seam tests drive. `set_clause` / `filter` are the inline SQL the
    /// shared assembler ([`crate::render::dml::assemble_backfill_clauses`]) renders; the
    /// executor-internal direct seam has NO approval gate (the generic executor
    /// gates approval before reaching the backend's `run_backfill_step`). Stops after
    /// at most `max_batches` committed batches (`None` = run to completion).
    ///
    /// # Errors
    /// [`crate::apply::backend::BackfillError`] on a malformed spec, an unsafe cursor
    /// column, a cursor-column mutation, a resumable batch failure, or a poisoned
    /// connection.
    pub async fn run_backfill_bounded_sqlite(
        &self,
        spec: &crate::model::backfill::BackfillSpec,
        set_clause: &str,
        filter: Option<&str>,
        applied_by: &str,
        max_batches: Option<u64>,
    ) -> Result<crate::apply::backend::BackfillOutcome, crate::apply::backend::BackfillError> {
        backfill_sql::run_backfill_bounded(
            &self.actor,
            spec,
            set_clause,
            filter,
            applied_by,
            max_batches,
            None,
        )
        .await
    }

    /// Roll back ONE migration's `down` ADDITIVELY + append a
    /// `rolled_back` event, atomically. The direct executor-internal seam (no
    /// approval gate; the generic executor gates approval before reaching here).
    /// A rebuild-needing `down` is refused with
    /// [`RollbackError::SqliteRebuildRequired`];
    /// the rebuild is not built.
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

    /// Apply ONE 12-step table REBUILD atomically with confinement + journal it.
    /// The rebuild DDL (drop-stale-temp / CREATE new / copy / drop old /
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
    /// dialect-coupled drive. The GATED production path is the engine's
    /// generic [`apply_declarative`](crate::engine::MigrationEngine::apply_declarative),
    /// which classifies the rebuild's `destructive + requires_approval` journal
    /// migration and refuses an un-approved rebuild BEFORE calling down into
    /// [`MigrationBackend::rebuild_one`]
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
        spec: &TableRebuildSpec,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RebuildError> {
        rebuild_sql::rebuild_one(&self.actor, spec, m, applied_by).await
    }

    /// Introspect the LIVE `main` (app file) schema into a dialect-agnostic
    /// [`SchemaSnapshot`] — the drift surface, the same shape the PG path
    /// returns. Recovers inline `zero-migrate:mask:` / `zero-migrate:enc:` sentinels from
    /// `sqlite_master.sql`.
    ///
    /// # Errors
    /// [`DriftError`] on a `sqlite_master` / PRAGMA read failure.
    pub async fn snapshot_schema_sqlite(&self) -> Result<SchemaSnapshot, DriftError> {
        drift_sql::snapshot_schema_for(&self.actor, "main").await
    }

    /// Serialize the LIVE `main` schema as a deterministic CREATE-statement script
    /// for the `dump` command (engine-agnostic `dump` parity with the PG
    /// `pg_dump --schema-only` leg). Tables/views before indexes/triggers, each
    /// name-ordered; the `_mig` journal + `sqlite_*` internals never leak.
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
    /// — read straight from the `_mig` journal so the dumped checksum/name
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

    /// `load` first-entry guard — run BEFORE any `main` mutation. Refuses
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
        // `sqlite_sequence`/`sqlite_stat*` bookkeeping) are NOT user data.
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
        // already-managed). Reuse the same net-applied probe `load` records over.
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

/// Choose the file used for the process-wide migration lock. The lock is keyed on
/// the database's `(dev, ino)` so symlinks and hard links cannot create separate
/// lock identities, but it is taken on a SIDECAR file — NEVER on the database file
/// itself.
///
/// # Why the sidecar, and not the database inode
///
/// `acquire_project_lock` takes `File::try_lock`, which is `flock(2)` on Unix. On
/// Linux `flock` and the POSIX `fcntl` locks SQLite's unix VFS uses live in
/// independent lock spaces, so locking the database file appeared to work. On
/// **Darwin they are the same lock space**: an exclusive `flock` on the database
/// file blocks SQLite's own writes to it — from the SAME process, on a different
/// fd — and surfaces as `SQLITE_BUSY`:
///
/// ```text
/// sqlite migration statement failed: database is locked
/// ```
///
/// so every SQLite apply self-deadlocked on macOS the moment the project lock was
/// taken, with zero migrations needed to reproduce. Measured, not hypothesised.
/// The sidecar keeps the cross-process exclusion this lock exists for while
/// leaving the database's own locking untouched.
fn project_lock_path(app_path: &Path) -> Result<PathBuf, SqliteActorError> {
    let canonical = std::fs::canonicalize(app_path).map_err(|error| {
        SqliteActorError::Open(format!(
            "canonicalize app path {} for project lock: {error}",
            app_path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        // `(dev, ino)` is the inode identity the old implementation got by locking
        // the file itself. Naming the sidecar after it preserves that property:
        // two hard links to one database still name one lock file.
        let meta = std::fs::metadata(&canonical).map_err(|error| {
            SqliteActorError::Open(format!(
                "stat app path {} for project lock: {error}",
                canonical.display()
            ))
        })?;
        let parent = canonical.parent().unwrap_or_else(|| Path::new("."));
        Ok(parent.join(format!(".zero-migrate-{}-{}.lock", meta.dev(), meta.ino())))
    }
    #[cfg(not(unix))]
    {
        let mut path = canonical.into_os_string();
        path.push(".zero-migrate.lock");
        Ok(PathBuf::from(path))
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
    // The actor serializes statements within one process. The sidecar file lock
    // extends that boundary across processes for the complete migration plan, so
    // catalog probes, DDL, DML, progress updates, and journal writes cannot
    // interleave with another zero-migrate process targeting the same app file.
    //
    // No compensating release on the acquisition's error path, unlike the
    // PostgreSQL leaf, and deliberately so. PostgreSQL can grant a session advisory
    // lock and still fail the acquiring statement, so a failed acquisition there
    // can leave a grant nobody tracks. This lock is a local file `try_lock` whose
    // result IS the acquisition: it either returns the handle or it does not, with
    // no window between the grant and the caller learning about it.

    async fn acquire_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        let mut held = self.project_lock_held.lock().map_err(|_| {
            ApplyError::Backend(format!(
                "sqlite project lock state poisoned for {}",
                self.project_lock_path.display()
            ))
        })?;
        if *held {
            return Err(ApplyError::Backend(format!(
                "sqlite project lock is already held for {}",
                self.project_lock_path.display()
            )));
        }
        // The project-lock budget, NOT the DDL budget. Queueing behind a peer
        // deploy is not competing with live application traffic, so tightening
        // `lock_timeout` to protect that traffic must not shorten how long this
        // deploy is willing to wait for its turn.
        let timeout = Duration::from_millis(cfg.project_lock_timeout_ms());
        let started = Instant::now();
        loop {
            match self.project_lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if started.elapsed() >= timeout => {
                    return Err(ApplyError::Backend(format!(
                        "timed out after {} ms acquiring sqlite project lock {}",
                        cfg.project_lock_timeout_ms(),
                        self.project_lock_path.display()
                    )));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
                Err(error) => {
                    return Err(ApplyError::Backend(format!(
                        "acquire sqlite project lock {}: {error}",
                        self.project_lock_path.display()
                    )));
                }
            }
        }
        *held = true;
        Ok(())
    }

    async fn release_project_lock(&self, _cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        let mut held = self.project_lock_held.lock().map_err(|_| {
            ApplyError::Backend(format!(
                "sqlite project lock state poisoned for {}",
                self.project_lock_path.display()
            ))
        })?;
        if !*held {
            return Err(ApplyError::Backend(format!(
                "sqlite project lock is not held for {}",
                self.project_lock_path.display()
            )));
        }
        self.project_lock.unlock().map_err(|error| {
            ApplyError::Backend(format!(
                "release sqlite project lock {}: {error}",
                self.project_lock_path.display()
            ))
        })?;
        *held = false;
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
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
        _had_inflight: bool,
        supersedes: &[&str],
        kind: &str,
    ) -> Result<bool, ApplyError> {
        if self.uses_two_phase_path(m) {
            return Err(ApplyError::Backend(
                "sqlite backend: non-transactional apply does not exist on SQLite".to_string(),
            ));
        }
        // The additive path covers a caller-supplied CREATE TABLE up + the
        // atomic journal write. Squash/supersession + repeatable journaling are not
        // implemented here; reject rather than silently dropping the edges.
        if !supersedes.is_empty() {
            return Err(ApplyError::Backend(
                "sqlite backend: supersession (squash) journaling is not supported on SQLite"
                    .to_string(),
            ));
        }
        if kind != "apply" {
            return Err(ApplyError::Backend(format!(
                "sqlite backend: journal kind '{kind}' is not supported (only 'apply')"
            )));
        }
        // Existence-guard catalog probe (SQLite). Mirror the PG
        // probe: read the live SQLite catalog and `decide` BEFORE running the `up`.
        // The read + the additive apply both run under the SAME held project lock +
        // atomic boundary `execute_pending` already enforces (apply_one is called
        // under the held lock), so there is no probe→act TOCTOU window.
        //
        // - RunBare → normal `apply_one_additive`.
        // - SatisfiedNoop → journal the completed row WITHOUT running the `up` DDL.
        // - FailDrift → typed `ExistenceGuardDrift` (parity with the PG arm) —
        // never a silent skip over a divergence.
        if let Some(probe) = &m.existence_guard {
            authorize_existence_guard_schema(cfg, m, probe.schema())?;
            // The probe's schema is authorized above but is NOT what SQLite snapshots.
            // SQLite's schema argument names an ATTACHED DATABASE - it reaches the
            // catalog as `PRAGMA <db>.table_info(...)` and `<db>.sqlite_master` - while
            // the probe carries the engine's logical project schema, which SQLite has no
            // equivalent of. Passing the probe's value here asks for a database that was
            // never attached and fails with `no such table: <project>.sqlite_master`.
            let live = drift_sql::snapshot_schema_for(&self.actor, "main")
                .await
                .map_err(|e| {
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
        // ADDITIVE rollback: reverse the `down` (DROP TABLE/COLUMN/INDEX,
        // RENAME) transactionally + append a `rolled_back` event. A rebuild-needing
        // `down` is refused with `SqliteRebuildRequired` (the rebuild is not built).
        rollback_sql::rollback_one_transactional(&self.actor, m, applied_by).await
    }

    async fn rollback_plan_transactional(
        &self,
        _cfg: &ExecutorConfig,
        forward: &Migration,
        inverse_steps: &[crate::render::step::PlanStep],
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        rollback_sql::rollback_dml_plan_transactional(
            &self.actor,
            forward,
            inverse_steps,
            applied_by,
        )
        .await
    }

    // -- parse-time validation ----------------------------------------------

    /// SQLite has no non-transactional DDL at all, so no `down` can object.
    fn non_transactional_down_reason(&self, _m: &Migration) -> Option<String> {
        None
    }

    fn validate_non_txn(&self, m: &Migration) -> Result<(), ApplyError> {
        // Reject transaction:false at the dialect boundary: SQLite DDL is
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

    async fn journal_exists(&self, _cfg: &ExecutorConfig) -> Result<bool, JournalError> {
        let exists = self.journal_path.try_exists().map_err(|error| {
            JournalError::Backend(format!(
                "inspect sqlite journal path {}: {error}",
                self.journal_path.display()
            ))
        })?;
        if !exists {
            return Ok(false);
        }

        let conn = rusqlite::Connection::open_with_flags(
            &self.journal_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|error| {
            JournalError::Backend(format!(
                "open sqlite journal {} read-only: {error}",
                self.journal_path.display()
            ))
        })?;
        let exists = conn
            .query_row(
                "SELECT EXISTS (
                     SELECT 1
                       FROM sqlite_master
                      WHERE type = 'table' AND name = 'schema_migrations'
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| {
                JournalError::Backend(format!(
                    "inspect sqlite journal {} schema: {error}",
                    self.journal_path.display()
                ))
            })?;
        Ok(exists)
    }

    async fn ensure_journal(&self, _cfg: &ExecutorConfig) -> Result<(), JournalError> {
        journal_sql::ensure_journal(&self.actor)
            .await
            .map_err(journal_err)
    }

    async fn applied(&self, _cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        journal_sql::applied(&self.actor).await.map_err(journal_err)
    }

    async fn net_rolled_back_versions(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        journal_sql::net_rolled_back_versions(&self.actor)
            .await
            .map_err(journal_err)
    }

    async fn backfill_progress(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<Vec<crate::apply::backend::BackfillProgressEntry>, JournalError> {
        backfill_sql::read_progress_entries(&self.actor)
            .await
            .map_err(journal_err)
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
        // the comparison is dialect-agnostic (shared `compare_applied_to_set`);
        // only the journal read underneath is dialect-coupled. Read the net-applied
        // journal entries (SQLite window-function net-state) and feed the generic
        // comparison — identical rules to the PG path.
        let applied = journal_sql::applied(&self.actor)
            .await
            .map_err(|e| DriftError::Backend(e.to_string()))?;
        Ok(crate::apply::drift::compare_applied_to_set(
            &applied, migrations,
        ))
    }

    async fn snapshot_schema(&self, _cfg: &ExecutorConfig) -> Result<SchemaSnapshot, DriftError> {
        // introspect the LIVE `main` (app file) schema via sqlite_master +
        // PRAGMAs into the same `SchemaSnapshot` shape the PG path returns, under
        // engine mode. Recovers inline mask/encryption sentinels.
        drift_sql::snapshot_schema_for(&self.actor, "main").await
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
        // fails closed rather than silently treating it as met (the declarative
        // path only needs the no-precondition path the descriptor diff produces).
        if m.preconditions.is_empty() {
            Ok(PreconditionVerdict::AllMet)
        } else {
            Err(ApplyError::Backend(
                "sqlite backend: precondition evaluation is not supported on SQLite \
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

    // -- declarative-only structured ops ------------------------------

    async fn rebuild_one(
        &self,
        spec: &TableRebuildSpec,
        m: &Migration,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
    ) -> Result<(), ApplyError> {
        // **Per-version scope (executor-layer defense in depth).** A rebuild on a
        // populated table is destructive (drop + recreate + copy; `m.flags.destructive`
        // is true for a `TableRebuild` by construction), so under
        // `ApprovalScope::Versions` it runs ONLY if the operator individually reviewed
        // THIS rebuild's version — mirroring the engine's per-version gate and keyed on
        // the same rule as `PlanStep::approval_scope_version`. So a direct seam caller
        // driving `rebuild_one` cannot bypass the per-version scope. Refuse BEFORE
        // touching the table, so a non-scoped rebuild rebuilds NOTHING.
        if (m.flags.destructive || m.flags.requires_approval) && !scope.admits(m.version.as_str()) {
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

    async fn alter_primary_key(
        &self,
        cfg: &ExecutorConfig,
        step: &crate::render::step::AlterPrimaryKeyStep,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
    ) -> Result<bool, ApplyError> {
        journal_sql::ensure_journal(&self.actor)
            .await
            .map_err(journal_err)
            .map_err(ApplyError::Journal)?;
        if let Some(entry) = journal_sql::applied(&self.actor)
            .await
            .map_err(journal_err)
            .map_err(ApplyError::Journal)?
            .into_iter()
            .filter(|entry| matches!(entry.phase, crate::apply::journal::Phase::Completed))
            .find(|entry| entry.version == step.migration.version.as_str())
        {
            if entry.checksum != step.migration.checksum.as_str() {
                return Err(ApplyError::ChecksumDrift {
                    version: step.migration.version.as_str().to_string(),
                    recorded: entry.checksum,
                    expected: step.migration.checksum.as_str().to_string(),
                });
            }
            return Ok(false);
        }
        if step.migration.flags.destructive || step.migration.flags.requires_approval {
            if approval != crate::approval::Approval::Approved {
                return Err(ApplyError::ApprovalRequired);
            }
            if !scope.admits(step.migration.version.as_str()) {
                return Err(ApplyError::ApprovalNotScoped {
                    version: step.migration.version.as_str().to_string(),
                });
            }
        }
        if !step.schema.eq_ignore_ascii_case(&cfg.project_schema)
            && !step.schema.eq_ignore_ascii_case("main")
        {
            return Err(ApplyError::Backend(format!(
                "sqlite primary-key lifecycle schema {:?} is outside configured project schema {:?}",
                step.schema, cfg.project_schema
            )));
        }
        rebuild_sql::rebuild_primary_key(
            &self.actor,
            &step.schema,
            &step.table,
            &step.action,
            &step.migration,
            applied_by,
        )
        .await
        .map_err(|error| ApplyError::Backend(error.to_string()))?;
        Ok(true)
    }

    async fn synchronize_identity(
        &self,
        cfg: &ExecutorConfig,
        step: &crate::render::step::SynchronizeIdentityStep,
        applied_by: &str,
    ) -> Result<bool, ApplyError> {
        if step.writes_quiesced.trim().is_empty() {
            return Err(ApplyError::Backend(
                "sqlite synchronizeIdentity writesQuiesced must name the maintenance window or no-concurrent-writer invariant"
                    .to_string(),
            ));
        }
        journal_sql::ensure_journal(&self.actor)
            .await
            .map_err(journal_err)
            .map_err(ApplyError::Journal)?;
        if let Some(entry) = journal_sql::applied(&self.actor)
            .await
            .map_err(journal_err)
            .map_err(ApplyError::Journal)?
            .into_iter()
            .filter(|entry| matches!(entry.phase, crate::apply::journal::Phase::Completed))
            .find(|entry| entry.version == step.migration.version.as_str())
        {
            if entry.checksum != step.migration.checksum.as_str() {
                return Err(ApplyError::ChecksumDrift {
                    version: step.migration.version.as_str().to_string(),
                    recorded: entry.checksum,
                    expected: step.migration.checksum.as_str().to_string(),
                });
            }
            return Ok(false);
        }
        if !step.schema.eq_ignore_ascii_case(&cfg.project_schema)
            && !step.schema.eq_ignore_ascii_case("main")
        {
            return Err(ApplyError::Backend(format!(
                "sqlite synchronizeIdentity schema {:?} is outside configured project schema {:?}",
                step.schema, cfg.project_schema
            )));
        }
        identity_sql::synchronize_identity(
            &self.actor,
            &step.table,
            &step.column,
            &step.migration,
            applied_by,
        )
        .await
        .map_err(apply_err)?;
        Ok(true)
    }

    async fn run_backfill_step(
        &self,
        _cfg: &ExecutorConfig,
        version: &crate::model::migration::MigrationId,
        checksum: &crate::model::migration::Checksum,
        spec: &crate::model::backfill::BackfillSpec,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<crate::apply::executor::ApplyOutcome, ApplyError> {
        // The SQLite batched/resumable backfill executor, the SQLite
        // analog of the PG writable-CTE windowed UPDATE. Completes the "one
        // script, both backends, DDL+DML" headline: a batched backfill is now
        // PORTABLE on BOTH backends. Each batch is its own committed
        // `BEGIN IMMEDIATE … COMMIT` on the single hardened connection, resumable
        // from the committed progress cursor in `_mig`.
        //
        if let Some(entry) = journal_sql::applied(&self.actor)
            .await
            .map_err(journal_err)
            .map_err(ApplyError::Journal)?
            .into_iter()
            .filter(|entry| matches!(entry.phase, crate::apply::journal::Phase::Completed))
            .find(|entry| entry.version == version.as_str())
        {
            if entry.checksum != checksum.as_str() {
                return Err(ApplyError::ChecksumDrift {
                    version: version.as_str().to_string(),
                    recorded: entry.checksum,
                    expected: checksum.as_str().to_string(),
                });
            }
            return Ok(crate::apply::executor::ApplyOutcome {
                applied: Vec::new(),
                skipped: vec![version.as_str().to_string()],
                recovered: Vec::new(),
            });
        }
        // A pending backfill mutates table data and requires explicit approval.
        // A completed matching step above is an idempotent skip and does not need
        // renewed approval.
        if approval != crate::approval::Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        if !scope.admits(version.as_str()) {
            return Err(ApplyError::ApprovalNotScoped {
                version: version.as_str().to_string(),
            });
        }
        let outcome = backfill_sql::run_backfill_bounded(
            &self.actor,
            spec,
            &spec.set_clause,
            spec.filter.as_deref(),
            applied_by,
            None,
            Some(backfill_sql::PlanBackfillIdentity { version, checksum }),
        )
        .await
        .map_err(|error| match error {
            crate::apply::backend::BackfillError::ChecksumDrift {
                version,
                recorded,
                expected,
            } => ApplyError::ChecksumDrift {
                version,
                recorded,
                expected,
            },
            other => ApplyError::Backend(format!("sqlite backfill step failed: {other}")),
        })?;
        // Surface the backfill's name as an applied version only on completion
        // (a resumed-but-incomplete backfill reports nothing applied), mirroring PG.
        let applied = if outcome.complete {
            vec![version.as_str().to_string()]
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
        checksum: &crate::model::migration::Checksum,
        name: &str,
        template: &str,
        binds: &[crate::render::step::BindValue],
        _target_schema: &str,
        _target_table: &str,
        _conflict_target: Option<&[String]>,
        _mutates_data: bool,
        destructive: bool,
        _owner_app: &str,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<bool, ApplyError> {
        // The SQLite one-shot DML executor. The `template` carries `?n`
        // placeholders; the binds are bound NATIVELY (never interpolated).
        //
        // Net-applied-skip: if this sub-version is already journaled `completed`,
        // the re-run is a no-op (idempotency).
        let completed = journal_sql::applied(&self.actor)
            .await
            .map_err(journal_err)
            .map_err(ApplyError::Journal)?
            .into_iter()
            .filter(|e| matches!(e.phase, crate::apply::journal::Phase::Completed))
            .find(|e| e.version == version.as_str());
        if let Some(entry) = completed {
            if entry.checksum != checksum.as_str() {
                return Err(ApplyError::ChecksumDrift {
                    version: version.as_str().to_string(),
                    recorded: entry.checksum,
                    expected: checksum.as_str().to_string(),
                });
            }
            return Ok(false);
        }
        if destructive && approval != crate::approval::Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        // Per-version scope defense in depth. A pending destructive DML runs only
        // when the operator approved this exact stable step identity.
        if destructive && !scope.admits(version.as_str()) {
            return Err(ApplyError::ApprovalNotScoped {
                version: version.as_str().to_string(),
            });
        }
        // Map the plan binds to the transport-safe SQLite bind mirror (the shared
        // `?n`-binding seam — `SqliteBind::from_bind`).
        let sqlite_binds: Vec<crate::apply::backend::sqlite::actor::SqliteBind> = binds
            .iter()
            .map(crate::apply::backend::sqlite::actor::SqliteBind::from_bind)
            .collect();
        journal_sql::run_dml(
            &self.actor,
            version.as_str(),
            checksum,
            name,
            template,
            &sqlite_binds,
            applied_by,
        )
        .await
        .map_err(apply_err)?;
        Ok(true)
    }

    fn online(&self) -> Option<&dyn crate::apply::backend::OnlineSchemaChange> {
        // SQLite has NO online schema-change capability: a SQLite declarative rename
        // is routed to a `rebuild_one` (the 12-step offline rebuild), never
        // expand-contract, so `plan.renames` is structurally EMPTY on the SQLite leg
        // and `None` here is never reached with renames to drive.
        // Returning `None` (no PG `Client`) is what removes this backend's only
        // PG-driver dependency: nothing on the shared trait is PG-typed here.
        None
    }

    fn shadow(&self) -> Option<&dyn crate::apply::backend::ShadowDryRun> {
        // SQLite has NO shadow dry-run capability — a DELIBERATE capability
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

#[cfg(test)]
mod lock_tests {
    use super::*;

    #[compio::test]
    async fn project_lock_excludes_a_second_backend_for_the_same_app() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = dir.path().join("app.sqlite");
        let journal = dir.path().join("journal.sqlite");
        let first = SqliteBackend::open(&app, &journal).expect("first backend");
        let second = SqliteBackend::open(&app, &journal).expect("second backend");
        let cfg = ExecutorConfig::new("project", "main", crate::test_fixtures::no_inject("main"));

        first
            .acquire_project_lock(&cfg)
            .await
            .expect("acquire first lock");
        let contention = second
            .project_lock
            .try_lock()
            .expect_err("a separate file handle cannot acquire the held project lock");
        assert!(matches!(contention, std::fs::TryLockError::WouldBlock));

        first
            .release_project_lock(&cfg)
            .await
            .expect("release first lock");
        second
            .project_lock
            .try_lock()
            .expect("lock becomes available after release");
        second.project_lock.unlock().expect("unlock direct probe");
    }

    #[compio::test]
    async fn project_lock_respects_the_configured_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = dir.path().join("app.sqlite");
        let first =
            SqliteBackend::open(&app, &dir.path().join("journal-a.sqlite")).expect("first backend");
        let second = SqliteBackend::open(&app, &dir.path().join("journal-b.sqlite"))
            .expect("second backend");
        let mut cfg =
            ExecutorConfig::new("project", "main", crate::test_fixtures::no_inject("main"));
        cfg.confinement.project_lock_timeout = Duration::from_millis(25);

        first.acquire_project_lock(&cfg).await.expect("first lock");
        let started = Instant::now();
        let error = second
            .acquire_project_lock(&cfg)
            .await
            .expect_err("the second backend must time out");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "lock acquisition must remain bounded"
        );
        first.release_project_lock(&cfg).await.expect("release");
    }

    /// The project-lock wait is its own budget, not the DDL one. An operator who
    /// tightens `lock_timeout` to keep a blocking statement off live traffic must
    /// not thereby shorten how long a deploy queues behind a peer deploy - the two
    /// numbers answer different questions, and 3 seconds is shorter than many real
    /// migrations.
    #[compio::test]
    async fn the_project_lock_wait_is_not_bounded_by_the_ddl_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = dir.path().join("app.sqlite");
        let first =
            SqliteBackend::open(&app, &dir.path().join("journal-a.sqlite")).expect("first backend");
        let second = SqliteBackend::open(&app, &dir.path().join("journal-b.sqlite"))
            .expect("second backend");
        let mut cfg =
            ExecutorConfig::new("project", "main", crate::test_fixtures::no_inject("main"));
        // A DDL budget far SHORTER than the project-lock budget. If the two are
        // coupled the second acquire gives up after 25ms.
        cfg.confinement.lock_timeout = Duration::from_millis(25);
        cfg.confinement.project_lock_timeout = Duration::from_millis(300);

        first.acquire_project_lock(&cfg).await.expect("first lock");
        let started = Instant::now();
        let error = second
            .acquire_project_lock(&cfg)
            .await
            .expect_err("the second backend must still time out eventually");
        let waited = started.elapsed();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            waited >= Duration::from_millis(150),
            "the deploy queued for {waited:?}, which is the 25ms DDL budget rather than the \
             300ms project-lock budget"
        );
        assert!(
            waited < Duration::from_secs(2),
            "still bounded by the project-lock budget, waited {waited:?}"
        );
        first.release_project_lock(&cfg).await.expect("release");
    }

    #[cfg(unix)]
    #[compio::test]
    async fn project_lock_cannot_be_bypassed_with_a_hard_link() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = dir.path().join("app.sqlite");
        let first =
            SqliteBackend::open(&app, &dir.path().join("journal-a.sqlite")).expect("first backend");
        let alias = dir.path().join("app-alias.sqlite");
        std::fs::hard_link(&app, &alias).expect("hard link app database");
        let second = SqliteBackend::open(&alias, &dir.path().join("journal-b.sqlite"))
            .expect("hard-link backend");
        let cfg = ExecutorConfig::new("project", "main", crate::test_fixtures::no_inject("main"));

        first.acquire_project_lock(&cfg).await.expect("first lock");
        assert!(matches!(
            second.project_lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        first.release_project_lock(&cfg).await.expect("release");
    }
}
