//! The SQLite journal: schema, immutability, native `event_seq`, and the atomic
//! single-connection apply (SQLite-parity design §2.2 / §2.2.1 / §2.2.2).
//!
//! The journal lives in the attached `_mig` database (a separate file), mirroring
//! the PG per-project meta schema. It carries the SAME logical shape as
//! `journal.rs` (the PG side): a SINGLE consolidated `schema_migrations` events
//! table (one row per `applied`/`rolled_back` event, discriminated by
//! `event_kind`), a `_supersedes` edge table, an inflight side-table, and
//! net-state computed via window functions over the native total order.
//!
//! **Native total order.** `event_seq INTEGER PRIMARY KEY AUTOINCREMENT` is the
//! total order: SQLite assigns a strictly-increasing rowid on every INSERT (the
//! INSERTs never supply it). AUTOINCREMENT (not bare rowid) is REQUIRED so the
//! order is strictly monotonic and never reused — a deleted row's seq can never be
//! recycled. There is NO standalone `event_seq` counter table and NO separate
//! `_rolled_back` table (both removed in the "go native seq" consolidation).
//!
//! # Immutability (§2.2.1, defense in depth)
//!
//! 1. `DEFENSIVE=ON` (set at open) makes `sqlite_master` read-only to SQL even
//!    under `writable_schema=ON`.
//! 2. `trusted_schema=OFF` (set at open) blocks schema objects from invoking
//!    non-allowlisted functions.
//! 3. The authorizer denies `PRAGMA` / writes / DROP / ALTER on `_mig` in
//!    CreatorUp (the primary deny, at prepare time).
//! 4. Append-only `BEFORE UPDATE`/`BEFORE DELETE` triggers (`RAISE(ABORT,…)`) are
//!    the in-DB backstop for row mutation (the Trusted/operator path where the
//!    authorizer relaxes; on the Confined path the authorizer already denied it).
//!
//! # Atomic apply (§2.2.2)
//!
//! `BEGIN IMMEDIATE` → mode=CreatorUp → run the creator `up` → mode=EngineJournal
//! → INSERT the journal row (the DB assigns `event_seq` via AUTOINCREMENT; no
//! separate allocation step) → COMMIT. The creator `up` and the journal write are
//! SEPARATE prepare/execute calls (never one batch) so the mode flip lands between
//! them and is read at each prepare. All on the single migration connection,
//! strictly sequential — race-free by construction.

use crate::apply::journal::{AppliedEntry, EventKind, JournaledKind, Phase};
use crate::model::migration::Migration;

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;

/// The fixed, short, table-local immutability trigger names (§2.9/L2). ASCII-safe
/// literals — never embed the (hyphenated-UUID) app id, which appears only in the
/// file path.
const IMMUTABLE_TRG: &str = "zs_immutable_trg";

/// Bootstrap the `_mig` journal idempotently, under **engine mode** (the engine
/// owns the journal objects; CreatorUp would deny these `_mig` CREATE/INSERTs).
///
/// All DDL targets the `"_mig"` alias so the authorizer keys on `database_name ==
/// Some("_mig")` and (in engine mode) allows it. Each statement is a discrete
/// `execute` call; the journal CREATE/INSERT/CREATE TRIGGER are all engine-mode.
pub(crate) async fn ensure_journal(actor: &MigrationActor) -> Result<(), SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;

    // 1. The SINGLE consolidated append-only events table. `event_seq INTEGER
    //    PRIMARY KEY AUTOINCREMENT` is the native total order — SQLite assigns it on
    //    INSERT (never supplied); AUTOINCREMENT (not bare rowid) guarantees strictly
    //    monotonic, never-reused values, so the latest event per version is a true
    //    total order. One row per migration EVENT: an `applied` (forward) event or a
    //    `rolled_back` event, discriminated by `event_kind`. version is NOT unique
    //    (rollback ↔ re-apply appends multiple rows). TEXT CURRENT_TIMESTAMP replaces
    //    PG's TIMESTAMPTZ DEFAULT now(); `at`/`by` unify the old
    //    applied_at/rolled_back_at and applied_by/rolled_back_by. The applied-only
    //    columns (kind/phase/outcome) are NULL on a `rolled_back` row; a CHECK
    //    documents the per-event_kind shape (mirrors the PG side). There is NO
    //    `event_seq` counter table and NO separate `_rolled_back` table any more.
    actor
        .exec(
            "CREATE TABLE IF NOT EXISTS \"_mig\".schema_migrations (\
                event_seq  INTEGER PRIMARY KEY AUTOINCREMENT, \
                event_kind TEXT NOT NULL CHECK (event_kind IN ('applied','rolled_back')), \
                version    TEXT NOT NULL, \
                name       TEXT NOT NULL, \
                checksum   TEXT NOT NULL, \
                \"at\"       TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                \"by\"       TEXT NOT NULL, \
                exec_ms    INTEGER, \
                phase      TEXT CHECK (phase IS NULL OR phase IN ('started','completed')), \
                outcome    TEXT, \
                kind       TEXT CHECK (kind IS NULL OR kind IN ('apply','baseline','squash','repeatable')), \
                CONSTRAINT schema_migrations_event_shape CHECK ( \
                    (event_kind = 'applied' \
                         AND kind IS NOT NULL AND phase IS NOT NULL AND outcome IS NOT NULL) \
                    OR \
                    (event_kind = 'rolled_back' \
                         AND kind IS NULL AND phase IS NULL AND outcome IS NULL)))",
        )
        .await?;
    // The supersedes edge table — a relation, not part of the event order, so it
    // gets its OWN native AUTOINCREMENT PK (no shared counter).
    actor
        .exec(
            "CREATE TABLE IF NOT EXISTS \"_mig\".schema_migrations_supersedes (\
                id                 INTEGER PRIMARY KEY AUTOINCREMENT, \
                squash_version     TEXT NOT NULL, \
                superseded_version TEXT NOT NULL, \
                recorded_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
        )
        .await?;

    // 3. The MUTABLE inflight side-table (NOT guarded by the immutability triggers
    //    — markers are deleted on completion). Present for parity; the non-txn path
    //    does not exist on SQLite, so this stays empty in P2.
    actor
        .exec(
            "CREATE TABLE IF NOT EXISTS \"_mig\".schema_migrations_inflight (\
                version    TEXT PRIMARY KEY, \
                name       TEXT NOT NULL, \
                checksum   TEXT NOT NULL, \
                started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                applied_by TEXT NOT NULL)",
        )
        .await?;

    // 4. Append-only immutability triggers on the two append-only tables (the
    //    consolidated events table + the supersedes edge table). SQLite has
    //    `CREATE TRIGGER IF NOT EXISTS`, so no pg_trigger-style existence guard is
    //    needed. Short table-local names (`zs_immutable_trg`) — disambiguated per
    //    table by SQLite's per-table trigger namespace, but SQLite trigger names are
    //    schema-global, so we suffix with the table to keep them unique. NOTE:
    //    SQLite has no TRUNCATE and no DROP-fires-DELETE-trigger, so these defend
    //    row mutation only; DROP TABLE / wholesale wipe is closed by the authorizer
    //    + DEFENSIVE (§2.2.1), not by a trigger.
    for tbl in ["schema_migrations", "schema_migrations_supersedes"] {
        for op in ["UPDATE", "DELETE"] {
            let trg = format!("{IMMUTABLE_TRG}_{tbl}_{}", op.to_ascii_lowercase());
            actor
                .exec(&format!(
                    "CREATE TRIGGER IF NOT EXISTS \"_mig\".\"{trg}\" \
                     BEFORE {op} ON \"{tbl}\" \
                     BEGIN SELECT RAISE(ABORT, 'migration journal is append-only (no UPDATE/DELETE)'); END"
                ))
                .await?;
        }
    }

    Ok(())
}

/// Apply ONE additive migration atomically with confinement (§2.2.2).
///
/// Idempotent: if the version's latest event is already `completed`, this is a
/// no-op and returns `false`. Otherwise runs the full phase sequence and returns
/// `true`. On any confinement denial / DDL failure the transaction is rolled back,
/// so the journal is never left half-written.
pub(crate) async fn apply_one_additive(
    actor: &MigrationActor,
    m: &Migration,
    applied_by: &str,
) -> Result<bool, SqliteActorError> {
    ensure_journal(actor).await?;

    // Idempotency: skip if already net-applied (latest event completed).
    let version = m.version.as_str().to_string();
    if applied(actor)
        .await?
        .iter()
        .any(|e| e.version == version && e.phase == Phase::Completed)
    {
        return Ok(false);
    }

    // -- the atomic phase sequence (§2.2.2) --
    run_apply_txn(actor, m, applied_by, &version).await?;
    Ok(true)
}

/// **PR10 Part B** — journal `m` as a `completed`, `kind='apply'` event WITHOUT
/// running its `up` DDL: the SQLite arm of an existence-guard
/// [`SatisfiedNoop`](crate::guard_probe::GuardVerdict::SatisfiedNoop). The guarded
/// object already has the declared shape (`ifNotExists`) or is already absent
/// (`ifExists`), so the `up` is a no-op, but the version must still LAND so a
/// re-deploy sees it net-applied and skips it via normal pending computation.
///
/// Mirrors [`apply_one_additive`]'s idempotency pre-check and [`run_apply_txn`]'s
/// atomic journal write, but SKIPS the CreatorUp `up` phase entirely (so no creator
/// DDL runs). The single `completed` row is written under one `BEGIN IMMEDIATE` in
/// `EngineJournal` mode — same recorded-not-run shape as [`baseline`], but with
/// `kind='apply'` (the guarded op IS an `apply`, just one whose effect is already
/// satisfied), so the journal `kind` matches a re-deploy's bare-apply expectation
/// and the checksum is `m`'s real checksum.
///
/// Returns `false` if `m` was already net-applied (idempotent no-op), `true` if a
/// fresh satisfied-noop row was journaled.
pub(crate) async fn journal_satisfied_noop(
    actor: &MigrationActor,
    m: &Migration,
    applied_by: &str,
) -> Result<bool, SqliteActorError> {
    ensure_journal(actor).await?;

    let version = m.version.as_str().to_string();
    if applied(actor)
        .await?
        .iter()
        .any(|e| e.version == version && e.phase == Phase::Completed)
    {
        return Ok(false);
    }

    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;
    let result = async {
        let name = sql_lit(&m.name);
        let checksum = sql_lit(m.checksum.as_str());
        let applied_by_lit = sql_lit(applied_by);
        let version_lit = sql_lit(&version);
        actor
            .exec(&format!(
                "INSERT INTO \"_mig\".schema_migrations \
                 (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
                 VALUES ('{applied}', {version_lit}, {name}, {checksum}, {applied_by_lit}, \
                         'completed', 'success', 'apply')",
                applied = EventKind::Applied.as_str()
            ))
            .await?;
        Ok::<(), SqliteActorError>(())
    }
    .await;

    match result {
        Ok(()) => {
            actor.exec("COMMIT").await?;
            Ok(true)
        }
        Err(e) => {
            let rb = actor.exec("ROLLBACK").await;
            match actor.is_autocommit().await {
                Ok(true) => Err(e),
                Ok(false) => Err(SqliteActorError::Poisoned(format!(
                    "transaction still open after ROLLBACK (rollback result: {rb:?}); \
                     original satisfied-noop journal error: {e}"
                ))),
                Err(probe) => Err(SqliteActorError::Poisoned(format!(
                    "could not confirm autocommit after ROLLBACK: {probe}; \
                     original satisfied-noop journal error: {e}"
                ))),
            }
        }
    }
}

/// Record `m` as the SQLite project's **baseline** — a `kind='baseline'`,
/// `completed` journal event WITHOUT running its `up` (the adoption path,
/// design §5 / H3). This is the SQLite arm behind the single neutral
/// [`MigrationBackend::baseline_one`](crate::backend::MigrationBackend::baseline_one)
/// (multi-engine abstraction L5); it returns the dialect-neutral
/// [`BaselineOutcome`](crate::baseline::BaselineOutcome), the same shape the PG arm
/// returns, so no SQLite-specific outcome type crosses the trait.
///
/// The motivating case (H3): a dev developer who ran the OLD `run_sqlite_pipeline`
/// has a `zs-default.sqlite` with user tables but an EMPTY `_mig` journal (the old
/// path was a stateless diff). The first engine boot against that file must NOT
/// re-create the tables or drift-abort — so we adopt the live schema by journaling
/// `m` (an `up` that DOCUMENTS the live shape but is recorded-not-run), after which
/// the declared diff applies only the *additional* ops on top.
///
/// First-entry semantics, mirroring the PG `baseline`:
/// - idempotent for the SAME version (a retried boot is safe → `already_present`);
/// - refuses if the journal already records a DIFFERENT net-applied migration
///   (the engine already manages this file) — fail-closed, nothing journaled.
///
/// The whole thing runs atomically inside one `BEGIN IMMEDIATE` under engine
/// mode (the `up` is NEVER executed, so there is no CreatorUp phase — this is the
/// key difference from [`apply_one_additive`]).
pub(crate) async fn baseline(
    actor: &MigrationActor,
    m: &Migration,
    applied_by: &str,
) -> Result<crate::apply::baseline::BaselineOutcome, SqliteActorError> {
    ensure_journal(actor).await?;

    let version = m.version.as_str().to_string();

    // First-entry + idempotency check, read once off the net-state. A baseline is
    // a first-entry operation: refuse if ANY net-applied migration already exists
    // (unless it is THIS exact version → idempotent).
    let net_completed: Vec<String> = applied(actor)
        .await?
        .into_iter()
        .filter(|e| e.phase == Phase::Completed)
        .map(|e| e.version)
        .collect();
    if net_completed.iter().any(|v| v == &version) {
        return Ok(crate::apply::baseline::BaselineOutcome {
            version,
            already_present: true,
        });
    }
    if !net_completed.is_empty() {
        return Err(SqliteActorError::Exec(format!(
            "cannot baseline: the journal already records {} net-applied migration(s); \
             baseline is a first-entry operation (a file the engine already manages \
             cannot be re-baselined)",
            net_completed.len()
        )));
    }

    // First entry: journal the baseline `completed` event WITHOUT running the
    // `up`. One atomic transaction under engine mode (no CreatorUp phase —
    // nothing of the creator's runs).
    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;
    let result = async {
        let name = sql_lit(&m.name);
        let checksum = sql_lit(m.checksum.as_str());
        let applied_by_lit = sql_lit(applied_by);
        let version_lit = sql_lit(&version);
        // event_seq is AUTOINCREMENT — not supplied. event_kind='applied' (a
        // baseline is a forward event recorded-not-run).
        actor
            .exec(&format!(
                "INSERT INTO \"_mig\".schema_migrations \
                 (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
                 VALUES ('{applied}', {version_lit}, {name}, {checksum}, {applied_by_lit}, \
                         'completed', 'success', 'baseline')",
                applied = EventKind::Applied.as_str()
            ))
            .await?;
        Ok::<(), SqliteActorError>(())
    }
    .await;

    match result {
        Ok(()) => {
            actor.exec("COMMIT").await?;
            Ok(crate::apply::baseline::BaselineOutcome {
                version,
                already_present: false,
            })
        }
        Err(e) => {
            let rb = actor.exec("ROLLBACK").await;
            match actor.is_autocommit().await {
                Ok(true) => Err(e),
                Ok(false) => Err(SqliteActorError::Poisoned(format!(
                    "transaction still open after ROLLBACK (rollback result: {rb:?}); \
                     original baseline error: {e}"
                ))),
                Err(probe) => Err(SqliteActorError::Poisoned(format!(
                    "could not confirm autocommit after ROLLBACK: {probe}; \
                     original baseline error: {e}"
                ))),
            }
        }
    }
}

/// A single trailer version to journal during `load` (a.k.a. `db:setup`): its
/// version, name, and checksum (sourced from the migration file in `--dir` so the
/// recorded checksum matches what a subsequent `migrate`/drift-check expects).
#[derive(Debug, Clone)]
pub struct LoadedVersion {
    /// The migration version (`<14-digit>` / `mig_…`).
    pub version: String,
    /// The migration's display name (best-effort from the file; may be empty).
    pub name: String,
    /// The migration's checksum (from the file; empty when the file is absent).
    pub checksum: String,
}

/// Record a batch of `load`ed versions as `baseline`-kind `completed` events
/// WITHOUT running any `up` (the schema DDL has ALREADY been replayed by the
/// caller). The journal-reconstruction half of `load` (dbmate `db:load`): after
/// the dump's DDL is restored, the dump trailer's versions are journaled so
/// `status` reports them applied and a subsequent `migrate` runs only the
/// genuinely-pending ones.
///
/// **First-entry-only**, mirroring [`baseline`]: refuses (errors, nothing
/// journaled) if the journal ALREADY records any net-applied migration — `load`
/// targets a FRESH/empty DB and must never clobber a DB the engine already
/// manages. The whole batch is recorded in ONE `BEGIN IMMEDIATE` transaction
/// under engine mode (no creator `up` runs — same recorded-not-run shape as
/// [`baseline`]); on any error the transaction is rolled back, leaving the journal
/// untouched.
///
/// # Errors
/// [`SqliteActorError`] if the journal already records net-applied migrations
/// (already-managed) or on an INSERT/commit failure.
pub(crate) async fn record_loaded_versions(
    actor: &MigrationActor,
    versions: &[LoadedVersion],
    applied_by: &str,
) -> Result<usize, SqliteActorError> {
    ensure_journal(actor).await?;

    // First-entry guard — `load` must not clobber an already-managed DB.
    let net_completed: Vec<String> = applied(actor)
        .await?
        .into_iter()
        .filter(|e| e.phase == Phase::Completed)
        .map(|e| e.version)
        .collect();
    if !net_completed.is_empty() {
        return Err(SqliteActorError::Exec(format!(
            "cannot load: the journal already records {} net-applied migration(s); \
             `load` targets a fresh/empty database (a DB the engine already manages \
             cannot be loaded over)",
            net_completed.len()
        )));
    }

    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;
    let result = async {
        for v in versions {
            let name = sql_lit(&v.name);
            let checksum = sql_lit(&v.checksum);
            let applied_by_lit = sql_lit(applied_by);
            let version_lit = sql_lit(&v.version);
            actor
                .exec(&format!(
                    "INSERT INTO \"_mig\".schema_migrations \
                     (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
                     VALUES ('{applied}', {version_lit}, {name}, {checksum}, {applied_by_lit}, \
                             'completed', 'success', 'baseline')",
                    applied = EventKind::Applied.as_str()
                ))
                .await?;
        }
        Ok::<(), SqliteActorError>(())
    }
    .await;

    match result {
        Ok(()) => {
            actor.exec("COMMIT").await?;
            Ok(versions.len())
        }
        Err(e) => {
            let rb = actor.exec("ROLLBACK").await;
            match actor.is_autocommit().await {
                Ok(true) => Err(e),
                Ok(false) => Err(SqliteActorError::Poisoned(format!(
                    "transaction still open after ROLLBACK (rollback result: {rb:?}); \
                     original load error: {e}"
                ))),
                Err(probe) => Err(SqliteActorError::Poisoned(format!(
                    "could not confirm autocommit after ROLLBACK: {probe}; \
                     original load error: {e}"
                ))),
            }
        }
    }
}

/// **PR6a** — apply a single parameterized one-shot DML step on SQLite (`insert`/
/// `update`/`del`), the SQLite peer of the PG `apply_dml_transactional`
/// (`executor.rs`). The `template` carries `?n` placeholders and `binds` the typed
/// values; they are bound NATIVELY (never interpolated) so a bind value cannot
/// alter the statement shape (§2.3.2). The DML runs under the confined
/// **CreatorUp** authorizer mode (denied from `_mig`, from PRAGMA / transaction
/// boundaries / vtables), then the `completed` journal row is written under
/// **EngineJournal** — DML + journal atomic in one `BEGIN IMMEDIATE`.
///
/// The journal checksum binds the DECLARING app's identity (`owner_app`, §2.0.1),
/// the SAME `Checksum::of(template, owner_app)` discipline the PG path uses — so
/// two DML steps with an identical `(template, binds)` authored by different apps
/// hash to different checksums.
///
/// # Errors
/// [`SqliteActorError`] on a DML / journal-write failure (rolled back, nothing
/// journaled); [`SqliteActorError::Poisoned`] if the connection cannot be returned
/// to autocommit after a rollback.
pub(crate) async fn run_dml(
    actor: &MigrationActor,
    version: &str,
    name: &str,
    template: &str,
    binds: &[crate::apply::backend::sqlite::actor::SqliteBind],
    owner_app: &str,
    applied_by: &str,
) -> Result<(), SqliteActorError> {
    ensure_journal(actor).await?;

    // 1. BEGIN IMMEDIATE under engine mode (the authorizer allows
    //    SQLITE_TRANSACTION only in EngineJournal — the engine owns txn boundaries).
    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;

    let result = async {
        // 2. Run the DML under the confined CreatorUp mode (denied from `_mig`,
        //    PRAGMA, transaction boundaries, vtables) — exactly the confinement a
        //    creator `up` runs under. The binds are positional `?n` params.
        actor.set_mode(Mode::CreatorUp).await?;
        actor.exec_params(template, binds).await?;

        // 3. EngineJournal — INSERT the `completed` row. SEPARATE prepares from the
        //    DML, with the mode flip strictly between (§2.2.2).
        actor.set_mode(Mode::EngineJournal).await?;
        let checksum = crate::model::migration::Checksum::of(&crate::model::migration::ChecksumInput {
            up: template,
            down: None,
            flags: &crate::model::migration::MigrationFlags::default(),
            owner_app,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        let name_lit = sql_lit(name);
        let checksum_lit = sql_lit(checksum.as_str());
        let applied_by_lit = sql_lit(applied_by);
        let version_lit = sql_lit(version);
        actor
            .exec(&format!(
                "INSERT INTO \"_mig\".schema_migrations \
                 (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
                 VALUES ('{applied}', {version_lit}, {name_lit}, {checksum_lit}, {applied_by_lit}, \
                         'completed', 'success', 'apply')",
                applied = EventKind::Applied.as_str()
            ))
            .await?;
        Ok::<(), SqliteActorError>(())
    }
    .await;

    match result {
        Ok(()) => {
            actor.set_mode(Mode::EngineJournal).await?;
            actor.exec("COMMIT").await?;
            Ok(())
        }
        Err(e) => {
            actor.set_mode(Mode::EngineJournal).await?;
            let rb = actor.exec("ROLLBACK").await;
            match actor.is_autocommit().await {
                Ok(true) => Err(e),
                Ok(false) => Err(SqliteActorError::Poisoned(format!(
                    "transaction still open after ROLLBACK (rollback result: {rb:?}); \
                     original DML error: {e}"
                ))),
                Err(probe) => Err(SqliteActorError::Poisoned(format!(
                    "could not confirm autocommit after ROLLBACK: {probe}; \
                     original DML error: {e}"
                ))),
            }
        }
    }
}

/// The transactional body, factored so a failure path can ROLLBACK cleanly.
async fn run_apply_txn(
    actor: &MigrationActor,
    m: &Migration,
    applied_by: &str,
    version: &str,
) -> Result<(), SqliteActorError> {
    // 1. BEGIN IMMEDIATE — one writer, RESERVED lock taken now. Issued under engine
    //    mode (the authorizer allows SQLITE_TRANSACTION only in EngineJournal — the
    //    engine owns transaction boundaries; the creator `up` in CreatorUp can never
    //    open/close a transaction, §2.5.1).
    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;

    // Run the rest; on ANY error, roll back and propagate.
    let result = async {
        // 2. Run the `up`. Ordinary creator/AI DDL runs under the confined
        //    **CreatorUp** mode (denied from `_mig`, from transaction boundaries,
        //    from PRAGMA, from making a vtable). The ONE exception is **engine-emitted
        //    goodie DDL** (§2.6, `engine_goodie_ddl`): the SQLite FTS5 virtual table +
        //    its sync triggers. That DDL is engine-AUTHORED from a `.fts()` descriptor
        //    (no untrusted SQL string), and the hardened authorizer allows
        //    `CREATE VIRTUAL TABLE … USING fts5(…)` ONLY in EngineJournal mode. So a
        //    goodie-DDL `up` runs under EngineJournal; everything else stays confined.
        //    (The `_mig` journal write below is a SEPARATE EngineJournal phase either
        //    way; the goodie `up` never touches `_mig` — it only creates the vtable +
        //    triggers in `main`.)
        let up_mode = if m.flags.engine_goodie_ddl {
            Mode::EngineJournal
        } else {
            Mode::CreatorUp
        };
        actor.set_mode(up_mode).await?;
        // The `up` may be multiple statements; each is prepared+stepped under the
        // chosen mode via execute_batch (single mode for the whole `up`, which is
        // correct — the up phase is one mode). A creator `up` must NOT contain a
        // journal write (the authorizer denies it); an engine-goodie `up` is
        // engine-authored DDL that touches only `main`.
        actor.exec(&m.up).await?;

        // 3. EngineJournal — INSERT the applied row (event_seq is AUTOINCREMENT, not
        //    supplied). SEPARATE prepares from the creator `up`, with the mode flip
        //    strictly between.
        actor.set_mode(Mode::EngineJournal).await?;
        let name = sql_lit(&m.name);
        let checksum = sql_lit(m.checksum.as_str());
        let applied_by_lit = sql_lit(applied_by);
        let version_lit = sql_lit(version);
        actor
            .exec(&format!(
                "INSERT INTO \"_mig\".schema_migrations \
                 (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
                 VALUES ('{applied}', {version_lit}, {name}, {checksum}, {applied_by_lit}, \
                         'completed', 'success', 'apply')",
                applied = EventKind::Applied.as_str()
            ))
            .await?;
        Ok::<(), SqliteActorError>(())
    }
    .await;

    match result {
        Ok(()) => {
            // 4. COMMIT — DDL + journal row commit together.
            actor.set_mode(Mode::EngineJournal).await?;
            actor.exec("COMMIT").await?;
            Ok(())
        }
        Err(e) => {
            // Roll back so a denied/failed `up` never leaves a partial journal.
            //
            // H1: the previous `let _ = actor.exec("ROLLBACK")` swallowed the result
            // entirely, which is unsafe on a LONG-LIVED, REUSED connection: if the
            // rollback leaves a transaction open, the NEXT apply fails with "cannot
            // start a transaction within a transaction". The ROLLBACK *error* alone
            // is NOT the wedge signal — a statement that auto-aborts the txn (e.g. an
            // `OR ROLLBACK` conflict) already closes it, after which an explicit
            // ROLLBACK spuriously errors with "no transaction is active" while the
            // connection is perfectly clean. The load-bearing invariant is the
            // AUTOCOMMIT STATE: after the rollback attempt the connection MUST be back
            // in autocommit. If it is not, the connection is wedged and the caller
            // must tear it down + rebuild before reuse — surfaced as `Poisoned` (more
            // severe than the `up` error, which is moot once the connection is dead).
            actor.set_mode(Mode::EngineJournal).await?;
            let rb = actor.exec("ROLLBACK").await; // may spuriously error post auto-abort
            match actor.is_autocommit().await {
                Ok(true) => Err(e), // clean: the original up error stands.
                Ok(false) => Err(SqliteActorError::Poisoned(format!(
                    "transaction still open after ROLLBACK (rollback result: {rb:?}); \
                     original up error: {e}"
                ))),
                Err(probe) => Err(SqliteActorError::Poisoned(format!(
                    "could not confirm autocommit after ROLLBACK: {probe}; \
                     original up error: {e}"
                ))),
            }
        }
    }
}

/// Single-quote a SQL string literal (double embedded quotes). The values here are
/// engine-controlled (migration metadata), but we quote defensively so a name with
/// an apostrophe can never break the INSERT.
pub(crate) fn sql_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The net-applied + lone-`started` entries (§2.2), mirroring PG `applied()`.
/// `DISTINCT ON` → `ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq
/// DESC)` (SQLite window functions, ≥3.25).
pub(crate) async fn applied(actor: &MigrationActor) -> Result<Vec<AppliedEntry>, SqliteActorError> {
    // Engine read of `_mig`: run under engine mode (the journal is engine
    // territory; a SELECT-only read does not write, but reading `_mig` should not
    // be gated by the creator deny on `_mig`).
    actor.set_mode(Mode::EngineJournal).await?;
    let sql = format!(
        "\
        WITH ranked AS ( \
            SELECT version, checksum, event_kind, kind AS mig_kind, \
                   ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn \
              FROM \"_mig\".schema_migrations \
        ), \
        latest AS (SELECT version, checksum, event_kind, mig_kind FROM ranked WHERE rn = 1), \
        net_applied AS (SELECT version, checksum, mig_kind FROM latest WHERE event_kind = '{applied}') \
        SELECT version, checksum, mig_kind, 'completed' AS phase FROM net_applied \
        UNION ALL \
        SELECT i.version, i.checksum, NULL AS mig_kind, 'started' AS phase \
          FROM \"_mig\".schema_migrations_inflight i \
         WHERE NOT EXISTS (SELECT 1 FROM net_applied n WHERE n.version = i.version) \
        ORDER BY version",
        applied = EventKind::Applied.as_str()
    );
    let rows = actor.query(&sql).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let version = cell(&r, 0)?;
        let checksum = cell(&r, 1)?;
        let mig_kind = r.get(2).and_then(|c| c.clone());
        let phase_s = cell(&r, 3)?;
        let phase = Phase::parse(&phase_s)
            .ok_or_else(|| SqliteActorError::Exec(format!("bad journal phase '{phase_s}'")))?;
        let kind = match mig_kind {
            Some(s) => Some(
                JournaledKind::parse(&s)
                    .ok_or_else(|| SqliteActorError::Exec(format!("bad journal kind '{s}'")))?,
            ),
            None => None,
        };
        out.push(AppliedEntry {
            version,
            checksum,
            phase,
            kind,
        });
    }
    Ok(out)
}

/// The versions covered by a net-applied squash (§2.2). Mirrors PG
/// `superseded_versions`: only a GENUINE recorded squash (`mig_kind='squash'`,
/// latest event completed) can supersede.
pub(crate) async fn superseded_versions(
    actor: &MigrationActor,
) -> Result<Vec<String>, SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    let sql = format!(
        "\
        WITH ranked AS ( \
            SELECT version, event_kind, kind AS mig_kind, \
                   ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn \
              FROM \"_mig\".schema_migrations \
        ), \
        latest AS (SELECT version, event_kind, mig_kind FROM ranked WHERE rn = 1), \
        net_applied_squashes AS ( \
            SELECT version FROM latest WHERE event_kind = '{applied}' AND mig_kind = 'squash' \
        ) \
        SELECT DISTINCT s.superseded_version AS v \
          FROM \"_mig\".schema_migrations_supersedes s \
          JOIN net_applied_squashes n ON n.version = s.squash_version \
         ORDER BY 1",
        applied = EventKind::Applied.as_str()
    );
    let rows = actor.query(&sql).await?;
    rows.iter().map(|r| cell(r, 0)).collect()
}

/// The latest `completed` checksum per repeatable identity (§2.2) — the
/// repeatable re-run oracle. Reads only `schema_migrations` where
/// `kind='repeatable'`.
pub(crate) async fn latest_completed_checksums(
    actor: &MigrationActor,
) -> Result<std::collections::HashMap<String, String>, SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    let sql = format!(
        "\
        WITH ranked AS ( \
            SELECT version, checksum, \
                   ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn \
              FROM \"_mig\".schema_migrations \
             WHERE event_kind = '{applied}' AND kind = 'repeatable' \
        ) \
        SELECT version, checksum FROM ranked WHERE rn = 1",
        applied = EventKind::Applied.as_str()
    );
    let rows = actor.query(&sql).await?;
    let mut map = std::collections::HashMap::new();
    for r in rows {
        map.insert(cell(&r, 0)?, cell(&r, 1)?);
    }
    Ok(map)
}

/// Extract a required text cell, erroring on NULL / missing.
fn cell(row: &[Option<String>], i: usize) -> Result<String, SqliteActorError> {
    row.get(i)
        .and_then(|c| c.clone())
        .ok_or_else(|| SqliteActorError::Exec(format!("missing journal cell at index {i}")))
}
