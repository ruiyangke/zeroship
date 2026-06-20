//! The SQLite journal: schema, immutability, shared monotonic `event_seq`, and
//! the atomic single-connection apply (SQLite-parity design §2.2 / §2.2.1 /
//! §2.2.2).
//!
//! The journal lives in the attached `_mig` database (a separate file), mirroring
//! the PG per-project meta schema. It carries the SAME logical shape as
//! `journal.rs` (the PG side): `schema_migrations` + `_rolled_back` + `_supersedes`
//! event tables, an inflight side-table, and net-state computed via window
//! functions over a SHARED monotonic `event_seq` (M4 — one counter across all
//! three tables, never per-table AUTOINCREMENT).
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
//! → allocate `event_seq` (`UPDATE _mig.event_seq … RETURNING next-1`) + INSERT
//! the journal row → COMMIT. The creator `up` and the journal write are SEPARATE
//! prepare/execute calls (never one batch) so the mode flip lands between them and
//! is read at each prepare. All on the single migration connection, strictly
//! sequential — race-free by construction.

use crate::journal::{AppliedEntry, JournaledKind, Phase};
use crate::migration::Migration;

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

    // 1. The shared monotonic sequence (M4): a single-row counter, NOT per-table
    //    AUTOINCREMENT, so event_seq is comparable across all three event tables.
    actor
        .exec(
            "CREATE TABLE IF NOT EXISTS \"_mig\".event_seq (\
                id INTEGER PRIMARY KEY CHECK (id = 1), \
                next INTEGER NOT NULL)",
        )
        .await?;
    actor
        .exec("INSERT OR IGNORE INTO \"_mig\".event_seq (id, next) VALUES (1, 1)")
        .await?;

    // 2. The append-only event tables. event_seq is a plain INTEGER (the assigned
    //    value), the PRIMARY KEY + total order. version is NOT unique (rollback ↔
    //    re-apply appends multiple completed rows). TEXT CURRENT_TIMESTAMP replaces
    //    PG's TIMESTAMPTZ DEFAULT now().
    actor
        .exec(
            "CREATE TABLE IF NOT EXISTS \"_mig\".schema_migrations (\
                event_seq  INTEGER PRIMARY KEY, \
                version    TEXT NOT NULL, \
                name       TEXT NOT NULL, \
                checksum   TEXT NOT NULL, \
                applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                applied_by TEXT NOT NULL, \
                exec_ms    INTEGER, \
                phase      TEXT NOT NULL CHECK (phase IN ('started','completed')), \
                outcome    TEXT NOT NULL, \
                kind       TEXT NOT NULL DEFAULT 'apply' \
                             CHECK (kind IN ('apply','baseline','squash','repeatable')))",
        )
        .await?;
    actor
        .exec(
            "CREATE TABLE IF NOT EXISTS \"_mig\".schema_migrations_rolled_back (\
                event_seq      INTEGER PRIMARY KEY, \
                version        TEXT NOT NULL, \
                name           TEXT NOT NULL, \
                checksum       TEXT NOT NULL, \
                rolled_back_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                rolled_back_by TEXT NOT NULL, \
                exec_ms        INTEGER)",
        )
        .await?;
    actor
        .exec(
            "CREATE TABLE IF NOT EXISTS \"_mig\".schema_migrations_supersedes (\
                event_seq          INTEGER PRIMARY KEY, \
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

    // 4. Append-only immutability triggers on the three event tables. SQLite has
    //    `CREATE TRIGGER IF NOT EXISTS`, so no pg_trigger-style existence guard is
    //    needed. Short table-local names (`zs_immutable_trg`) — disambiguated per
    //    table by SQLite's per-table trigger namespace, but SQLite trigger names are
    //    schema-global, so we suffix with the table to keep them unique. NOTE:
    //    SQLite has no TRUNCATE and no DROP-fires-DELETE-trigger, so these defend
    //    row mutation only; DROP TABLE / wholesale wipe is closed by the authorizer
    //    + DEFENSIVE (§2.2.1), not by a trigger.
    for tbl in [
        "schema_migrations",
        "schema_migrations_rolled_back",
        "schema_migrations_supersedes",
    ] {
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

/// Allocate the next `event_seq` from the shared counter (§2.2). ONE statement,
/// inside the apply transaction, under engine mode: `UPDATE … RETURNING next - 1`
/// yields the pre-increment value atomically. Returns the allocated `event_seq`.
pub(crate) async fn alloc_event_seq(actor: &MigrationActor) -> Result<i64, SqliteActorError> {
    let rows = actor
        .query("UPDATE \"_mig\".event_seq SET next = next + 1 WHERE id = 1 RETURNING next - 1")
        .await?;
    let cell = rows
        .first()
        .and_then(|r| r.first())
        .and_then(|c| c.as_ref())
        .ok_or_else(|| SqliteActorError::Exec("event_seq allocation returned no row".to_string()))?;
    cell.parse::<i64>()
        .map_err(|e| SqliteActorError::Exec(format!("event_seq parse: {e}")))
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
        // 2. CreatorUp — the creator `up` is confined from `_mig`.
        actor.set_mode(Mode::CreatorUp).await?;
        // The `up` may be multiple statements; each is prepared+stepped under
        // CreatorUp via execute_batch (single mode for the whole creator `up`,
        // which is correct — the creator phase is one mode). It must NOT contain a
        // journal write (the authorizer denies it).
        actor.exec(&m.up).await?;

        // 3. EngineJournal — allocate event_seq + INSERT the completed row. SEPARATE
        //    prepares from the creator `up`, with the mode flip strictly between.
        actor.set_mode(Mode::EngineJournal).await?;
        let seq = alloc_event_seq(actor).await?;
        let name = sql_lit(&m.name);
        let checksum = sql_lit(m.checksum.as_str());
        let applied_by_lit = sql_lit(applied_by);
        let version_lit = sql_lit(version);
        actor
            .exec(&format!(
                "INSERT INTO \"_mig\".schema_migrations \
                 (event_seq, version, name, checksum, applied_by, phase, outcome, kind) \
                 VALUES ({seq}, {version_lit}, {name}, {checksum}, {applied_by_lit}, \
                         'completed', 'success', 'apply')"
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
    let sql = "\
        WITH events AS ( \
            SELECT version, checksum, event_seq, 'completed' AS event_kind, kind AS mig_kind \
              FROM \"_mig\".schema_migrations \
            UNION ALL \
            SELECT version, checksum, event_seq, 'rolled_back' AS event_kind, NULL AS mig_kind \
              FROM \"_mig\".schema_migrations_rolled_back \
        ), \
        ranked AS ( \
            SELECT version, checksum, event_kind, mig_kind, \
                   ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn \
              FROM events \
        ), \
        latest AS (SELECT version, checksum, event_kind, mig_kind FROM ranked WHERE rn = 1), \
        net_completed AS (SELECT version, checksum, mig_kind FROM latest WHERE event_kind = 'completed') \
        SELECT version, checksum, mig_kind, 'completed' AS phase FROM net_completed \
        UNION ALL \
        SELECT i.version, i.checksum, NULL AS mig_kind, 'started' AS phase \
          FROM \"_mig\".schema_migrations_inflight i \
         WHERE NOT EXISTS (SELECT 1 FROM net_completed n WHERE n.version = i.version) \
        ORDER BY version";
    let rows = actor.query(sql).await?;
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
    let sql = "\
        WITH events AS ( \
            SELECT version, event_seq, 'completed' AS event_kind, kind AS mig_kind \
              FROM \"_mig\".schema_migrations \
            UNION ALL \
            SELECT version, event_seq, 'rolled_back' AS event_kind, NULL AS mig_kind \
              FROM \"_mig\".schema_migrations_rolled_back \
        ), \
        ranked AS ( \
            SELECT version, event_kind, mig_kind, \
                   ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn \
              FROM events \
        ), \
        latest AS (SELECT version, event_kind, mig_kind FROM ranked WHERE rn = 1), \
        net_applied_squashes AS ( \
            SELECT version FROM latest WHERE event_kind = 'completed' AND mig_kind = 'squash' \
        ) \
        SELECT DISTINCT s.superseded_version AS v \
          FROM \"_mig\".schema_migrations_supersedes s \
          JOIN net_applied_squashes n ON n.version = s.squash_version \
         ORDER BY 1";
    let rows = actor.query(sql).await?;
    rows.iter().map(|r| cell(r, 0)).collect()
}

/// The latest `completed` checksum per repeatable identity (§2.2) — the
/// repeatable re-run oracle. Reads only `schema_migrations` where
/// `kind='repeatable'`.
pub(crate) async fn latest_completed_checksums(
    actor: &MigrationActor,
) -> Result<std::collections::HashMap<String, String>, SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    let sql = "\
        WITH ranked AS ( \
            SELECT version, checksum, \
                   ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC) AS rn \
              FROM \"_mig\".schema_migrations WHERE kind = 'repeatable' \
        ) \
        SELECT version, checksum FROM ranked WHERE rn = 1";
    let rows = actor.query(sql).await?;
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
