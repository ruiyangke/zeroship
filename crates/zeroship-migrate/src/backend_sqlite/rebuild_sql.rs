//! The SQLite 12-step table REBUILD (SQLite-parity design §2.4, P3b).
//!
//! `ALTER TABLE` on SQLite is limited to `ADD`/`RENAME`/`DROP COLUMN` + `RENAME
//! TO`. A column TYPE change, a nullability change, a column RENAME's contract, an
//! ADD/DROP CONSTRAINT, or an in-place FK redefinition has NO native `ALTER` — it
//! requires the canonical SQLite table-rebuild: create a new table with the desired
//! shape under a temp name, copy the mapped rows in, drop the old table, rename the
//! new one into place, and recreate the table's indexes / triggers / dependent
//! views. The new table's CREATE comes from the shared `zeroship-schema`
//! Sqlite/MainUnqualified emitter (so it carries the inline mask/enc goodie
//! sentinels and FKs); the index/trigger/view DDL is captured verbatim from the
//! LIVE `sqlite_master` before the swap and replayed after.
//!
//! # The EXACT statement / mode / PRAGMA sequence (the crux a critic attacks)
//!
//! `PRAGMA foreign_keys` is a **NO-OP inside a transaction** (a hard SQLite rule),
//! so the FK-enforcement toggles MUST straddle the transaction, in **autocommit**.
//! The rebuild therefore runs:
//!
//! ```text
//!  (engine, AUTOCOMMIT, EngineJournal)  PRAGMA foreign_keys = OFF
//!  (engine, EngineJournal)              BEGIN IMMEDIATE
//!  (engine→CreatorUp)                   CREATE TABLE <tmp> (...new shape...)   [main]
//!                                       INSERT INTO <tmp> (cols) SELECT cols FROM <t>
//!                                       DROP TABLE <t>
//!                                       ALTER TABLE <tmp> RENAME TO <t>
//!                                       <recreate each index / trigger / view> [main]
//!  (engine, EngineJournal)              PRAGMA foreign_key_check       -- abort on rows
//!  (engine, EngineJournal)              UPDATE _mig.event_seq ...; INSERT _mig journal
//!  (engine, EngineJournal)              COMMIT
//!  (engine, AUTOCOMMIT, EngineJournal)  PRAGMA foreign_keys = ON       -- ALL PATHS
//! ```
//!
//! ## Why the FK-OFF autocommit window is safe (constraint #1/#3/#4)
//!
//! The window where `foreign_keys=OFF` is **entirely engine-controlled**: it is
//! opened by the engine in EngineJournal mode, and the ONLY statement that prepares
//! between `OFF` and `ON` is the engine's own rebuild transaction. No untrusted
//! creator statement prepares while FK enforcement is off — the migration actor is
//! single-connection and strictly sequential, and the rebuild is driven entirely by
//! this function. The `foreign_key_check` step (which DOES work inside a txn) runs
//! before COMMIT, so a rebuild that would orphan a row aborts the transaction
//! (typed [`RebuildError::ForeignKeyViolation`]) rather than committing a corrupt
//! state. **`foreign_keys` is restored to ON in EVERY path** — success, FK-check
//! abort, DDL failure, and the wedge/poison branch — using the same H1
//! autocommit-probe discipline the apply/rollback paths use: after the txn is
//! closed (committed or rolled back), `PRAGMA foreign_keys=ON` runs in autocommit
//! and the connection is probed; a connection that cannot be confirmed back in
//! autocommit (or whose FK re-enable failed) is surfaced as poisoned so the caller
//! tears it down rather than reuse a connection with FK enforcement off.
//!
//! ## Mode boundaries (constraint #2)
//!
//! The rebuild DDL runs under **CreatorUp** — it is engine-authored but operates on
//! the creator's app schema (`main`), and CreatorUp legitimately allows
//! CREATE/INSERT/DROP/RENAME/CREATE INDEX/TRIGGER/VIEW on `main` while still denying
//! every `_mig` write, ATTACH/DETACH/PRAGMA/load_extension. Running the rebuild DDL
//! under the LEAST privilege that suffices means even an engine-generated statement
//! can never touch the journal. The PRAGMA toggles, `foreign_key_check`, and the
//! journal write run under **EngineJournal**. The mode flip lands BETWEEN separate
//! prepares — never inside one `execute_batch` that spans a boundary (§2.2.2).

use std::time::Instant;

use crate::migration::Migration;

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;
use super::journal_sql;

/// A typed error from the 12-step rebuild. Carries dialect-neutral `String`
/// payloads so it flows through the generic backend errors without leaking a SQLite
/// type.
#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    /// `PRAGMA foreign_key_check` reported orphaned rows — the rebuild would commit
    /// a referential-integrity violation. The transaction is rolled back; the
    /// original table is intact; `foreign_keys` is restored to ON.
    #[error(
        "sqlite rebuild of '{table}' aborted: PRAGMA foreign_key_check reported {violations} \
         orphaned row(s); the original table is intact and foreign_keys is back ON"
    )]
    ForeignKeyViolation {
        /// The table the rebuild targeted.
        table: String,
        /// The number of violation rows `foreign_key_check` reported.
        violations: usize,
    },
    /// A rebuild statement (prepare/step) failed or was denied by the authorizer.
    /// The transaction is rolled back; `foreign_keys` is restored to ON.
    #[error("sqlite rebuild of '{table}' failed: {source}")]
    Step {
        /// The table the rebuild targeted.
        table: String,
        /// The underlying actor error (a DENY surfaces here too).
        #[source]
        source: SqliteActorError,
    },
    /// The long-lived migration connection is wedged after a failed rebuild: the
    /// transaction did not cleanly close, OR `foreign_keys` could not be restored to
    /// ON. The connection can no longer be safely reused (it may still have FK
    /// enforcement OFF); the caller MUST tear it down and rebuild before the next
    /// apply (H1, the FK-off-window safety backstop).
    #[error("sqlite migration connection poisoned during rebuild of '{table}': {detail}")]
    Poisoned {
        /// The table the rebuild targeted.
        table: String,
        /// What went wrong (the rollback/FK-restore failure detail).
        detail: String,
    },
}

/// The fully-resolved specification for ONE table rebuild (design §2.4). The
/// differ builds this from the desired-vs-live diff; the backend executes it.
///
/// Everything here is engine-constructed from validated descriptors + live
/// introspection — never raw creator SQL. The `new_table_create` is the shared
/// emitter's output (carrying inline goodie sentinels + FKs), already rewritten to
/// the TEMP name. The `copy_columns` are the column names present in BOTH the old
/// and new shapes (the safe intersection to copy); the `recreate_objects` are the
/// table's indexes / triggers / dependent views captured verbatim from
/// `sqlite_master` so they survive the swap.
#[derive(Debug, Clone)]
pub struct SqliteRebuildSpec {
    /// The existing table being rebuilt (the final name; the new table is renamed
    /// INTO this).
    pub table: String,
    /// The temp name the new table is created under, then renamed FROM. Engine-
    /// chosen (`<table>__zsrebuild`), never creator input.
    pub tmp_table: String,
    /// The new table's `CREATE TABLE <tmp> (...)` DDL — the shared
    /// Sqlite/MainUnqualified emitter's output (goodie sentinels + FKs), with the
    /// table identifier already the TEMP name. UNqualified (`main` = the app file).
    pub new_table_create: String,
    /// The columns to copy from the old table into the new one, as `(dest, src)`
    /// pairs of BARE (unquoted) identifiers — emitted as
    /// `INSERT INTO <new> (dest…) SELECT src… FROM <old>`. For a plain kept column
    /// `dest == src`; for a column RENAME the pair maps `to ← from` so the data
    /// follows the rename. A dropped column is absent (excluded from both lists); an
    /// added column is absent (it takes its DEFAULT/NULL from the new CREATE).
    pub copy_columns: Vec<(String, String)>,
    /// The table's indexes / triggers / dependent views to recreate AFTER the
    /// rename, captured verbatim from the live `sqlite_master.sql` (so they carry
    /// any inline sentinels and exact definitions). Replayed in order under
    /// CreatorUp on `main`.
    pub recreate_objects: Vec<String>,
    /// A human-readable description of what change drove the rebuild (for the
    /// journal name + diagnostics), e.g. `alter column age type integer → text`.
    pub reason: String,
}

impl SqliteRebuildSpec {
    /// The engine-chosen temp-table name for `table`. A fixed ASCII suffix that can
    /// never collide with a creator table (creator identifiers cannot contain the
    /// `__zsrebuild` reserved infix in practice; and the rebuild drops it within the
    /// same transaction so it never persists).
    #[must_use]
    pub fn tmp_name(table: &str) -> String {
        format!("{table}__zsrebuild")
    }
}

/// Double-quote a SQLite identifier (escaping embedded quotes). Engine-controlled
/// identifiers, quoted defensively.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Execute ONE 12-step table rebuild atomically with confinement + journal it
/// (design §2.4). The DDL + the journal row commit in ONE `BEGIN IMMEDIATE`
/// transaction; the FK-enforcement toggles necessarily straddle it (per the SQLite
/// in-txn no-op rule), in engine-controlled autocommit windows.
///
/// Idempotency is the CALLER's concern (the executor's net-state gate); this is the
/// dialect-coupled execution + journal write only, the same contract as
/// [`journal_sql::apply_one_additive`] / [`super::rollback_sql`].
///
/// # Errors
/// - [`RebuildError::ForeignKeyViolation`] if `foreign_key_check` reports orphans
///   (the txn is rolled back; the original table is intact; FK enforcement ON).
/// - [`RebuildError::Step`] if a rebuild statement fails / is denied (rolled back).
/// - [`RebuildError::Poisoned`] if the connection cannot be confirmed clean +
///   `foreign_keys=ON` afterwards (the caller must tear it down).
pub(crate) async fn rebuild_one(
    actor: &MigrationActor,
    spec: &SqliteRebuildSpec,
    m: &Migration,
    applied_by: &str,
) -> Result<(), RebuildError> {
    // The journal must exist before we open the txn (engine mode, idempotent).
    journal_sql::ensure_journal(actor)
        .await
        .map_err(|e| step_err(&spec.table, e))?;

    // 1. PRAGMA foreign_keys=OFF — in AUTOCOMMIT, under engine mode. This MUST run
    //    outside any transaction (it is a no-op inside one), and only the engine may
    //    issue a PRAGMA (CreatorUp denies it). No creator SQL runs in this window.
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(|e| step_err(&spec.table, e))?;
    actor
        .exec("PRAGMA foreign_keys = OFF")
        .await
        .map_err(|e| step_err(&spec.table, e))?;

    // Run the txn body; whatever happens, restore foreign_keys=ON afterwards.
    let outcome = run_rebuild_txn(actor, spec, m, applied_by).await;

    // 2. Restore foreign_keys=ON — in AUTOCOMMIT, ALL PATHS (success / FK-abort /
    //    error). The txn is already closed (committed by the body, or rolled back by
    //    it). This is the FK-off-window safety backstop: the long-lived connection
    //    must NEVER be left with FK enforcement off. If the re-enable itself fails,
    //    OR the connection is not back in autocommit, the connection is poisoned —
    //    surface it so the caller tears it down rather than reuse an FK-off conn.
    let fk_on = actor.exec("PRAGMA foreign_keys = ON").await;
    let autocommit = actor.is_autocommit().await;
    match (fk_on, autocommit) {
        (Ok(()), Ok(true)) => outcome, // clean: the body's result stands.
        (fk, ac) => {
            // Either the FK re-enable failed, or the connection is still mid-txn /
            // unprobeable: the connection may have FK enforcement OFF and cannot be
            // safely reused. This is MORE severe than the body's error (which is moot
            // once the connection is dead), so it takes precedence.
            let body = match &outcome {
                Ok(()) => "rebuild body succeeded".to_string(),
                Err(e) => e.to_string(),
            };
            Err(RebuildError::Poisoned {
                table: spec.table.clone(),
                detail: format!(
                    "could not restore foreign_keys=ON to autocommit after rebuild \
                     (fk_on={fk:?}, autocommit={ac:?}); body: {body}"
                ),
            })
        }
    }
}

/// The transactional body (steps inside the one `BEGIN IMMEDIATE`), factored so the
/// caller can ALWAYS restore `foreign_keys=ON` afterwards regardless of outcome. On
/// any failure this rolls back and reports — never leaving a half-rebuilt table or a
/// partial journal. Returns `Ok(())` only if the rebuild DDL + journal row committed.
async fn run_rebuild_txn(
    actor: &MigrationActor,
    spec: &SqliteRebuildSpec,
    m: &Migration,
    applied_by: &str,
) -> Result<(), RebuildError> {
    let started = Instant::now();
    let table = spec.table.as_str();

    // BEGIN IMMEDIATE under engine mode (the engine owns txn boundaries). FK
    // enforcement is OFF for the connection right now (set in autocommit above), and
    // a PRAGMA toggle would be a no-op here anyway — which is exactly why it had to
    // be set outside.
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(|e| step_err(table, e))?;
    actor
        .exec("BEGIN IMMEDIATE")
        .await
        .map_err(|e| step_err(table, e))?;

    let result = run_rebuild_steps(actor, spec, m, applied_by, started).await;

    match result {
        Ok(()) => {
            actor
                .set_mode(Mode::EngineJournal)
                .await
                .map_err(|e| step_err(table, e))?;
            actor
                .exec("COMMIT")
                .await
                .map_err(|e| step_err(table, e))?;
            Ok(())
        }
        Err(e) => {
            // Roll back so a failed/aborted rebuild leaves the ORIGINAL table intact
            // and no partial journal. Same H1 discipline as apply/rollback: the
            // AUTOCOMMIT state — not the ROLLBACK result — is the wedge signal.
            actor
                .set_mode(Mode::EngineJournal)
                .await
                .map_err(|e2| step_err(table, e2))?;
            let rb = actor.exec("ROLLBACK").await;
            match actor.is_autocommit().await {
                Ok(true) => Err(e), // clean rollback: the original rebuild error stands.
                Ok(false) => Err(RebuildError::Poisoned {
                    table: table.to_string(),
                    detail: format!(
                        "transaction still open after ROLLBACK (rollback result: {rb:?}); \
                         original rebuild error: {e}"
                    ),
                }),
                Err(probe) => Err(RebuildError::Poisoned {
                    table: table.to_string(),
                    detail: format!(
                        "could not confirm autocommit after ROLLBACK: {probe}; \
                         original rebuild error: {e}"
                    ),
                }),
            }
        }
    }
}

/// The ordered statements inside the transaction (the 12-step proper + the FK
/// integrity check + the journal write). Each is a discrete prepare/execute call;
/// the mode flip lands strictly between the CreatorUp rebuild DDL and the
/// EngineJournal check/journal write.
async fn run_rebuild_steps(
    actor: &MigrationActor,
    spec: &SqliteRebuildSpec,
    m: &Migration,
    applied_by: &str,
    started: Instant,
) -> Result<(), RebuildError> {
    let table = spec.table.as_str();
    let table_q = quote_ident(&spec.table);
    let tmp_q = quote_ident(&spec.tmp_table);

    // --- The rebuild DDL runs under CreatorUp (engine-authored, on `main`). ---
    actor
        .set_mode(Mode::CreatorUp)
        .await
        .map_err(|e| step_err(table, e))?;

    // (a) Create the new table under the temp name (shared-emitter DDL).
    actor
        .exec(&spec.new_table_create)
        .await
        .map_err(|e| step_err(table, e))?;

    // (b) Copy the mapped rows: INSERT INTO <tmp> (dest…) SELECT src… FROM <t>. Only
    //     the carried columns are copied; a dropped column is absent and an added
    //     column takes its DEFAULT/NULL. A RENAME maps `to ← from`. An empty mapping
    //     (no overlapping columns) would make `INSERT … SELECT` over zero columns
    //     invalid, so skip the copy entirely (all-new shape, nothing to carry).
    if !spec.copy_columns.is_empty() {
        let dest = spec
            .copy_columns
            .iter()
            .map(|(d, _)| quote_ident(d))
            .collect::<Vec<_>>()
            .join(", ");
        let src = spec
            .copy_columns
            .iter()
            .map(|(_, s)| quote_ident(s))
            .collect::<Vec<_>>()
            .join(", ");
        actor
            .exec(&format!(
                "INSERT INTO {tmp_q} ({dest}) SELECT {src} FROM {table_q}"
            ))
            .await
            .map_err(|e| step_err(table, e))?;
    }

    // (c) Drop the old table.
    actor
        .exec(&format!("DROP TABLE {table_q}"))
        .await
        .map_err(|e| step_err(table, e))?;

    // (d) Rename the new table into place.
    actor
        .exec(&format!("ALTER TABLE {tmp_q} RENAME TO {table_q}"))
        .await
        .map_err(|e| step_err(table, e))?;

    // (e) Recreate the table's indexes / triggers / dependent views (verbatim
    //     captured DDL). SQLite drops a table's indexes/triggers WITH the table, so
    //     they must be replayed. Each is a CREATE on `main` — allowed in CreatorUp.
    for obj in &spec.recreate_objects {
        actor.exec(obj).await.map_err(|e| step_err(table, e))?;
    }

    // --- FK integrity check + journal write run under EngineJournal. ---
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(|e| step_err(table, e))?;

    // (f) PRAGMA foreign_key_check — this WORKS inside a transaction (unlike the
    //     foreign_keys toggle). Any row it returns is an orphaned FK reference the
    //     rebuild would commit; abort (typed error) so the txn rolls back and the
    //     original table is restored. Scope to the rebuilt table so an unrelated
    //     pre-existing violation elsewhere does not falsely abort this rebuild.
    let violations = actor
        .query(&format!("PRAGMA main.foreign_key_check({table_q})"))
        .await
        .map_err(|e| step_err(table, e))?;
    if !violations.is_empty() {
        return Err(RebuildError::ForeignKeyViolation {
            table: spec.table.clone(),
            violations: violations.len(),
        });
    }

    // (g) Allocate event_seq from the shared counter + INSERT the immutable journal
    //     row (a rebuild is an `apply`-kind completed event, like any other applied
    //     migration). SEPARATE prepares from the rebuild DDL; mode already flipped.
    let seq = journal_sql::alloc_event_seq(actor)
        .await
        .map_err(|e| step_err(table, e))?;
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let version = journal_sql::sql_lit(m.version.as_str());
    let name = journal_sql::sql_lit(&m.name);
    let checksum = journal_sql::sql_lit(m.checksum.as_str());
    let by = journal_sql::sql_lit(applied_by);
    actor
        .exec(&format!(
            "INSERT INTO \"_mig\".schema_migrations \
             (event_seq, version, name, checksum, applied_by, exec_ms, phase, outcome, kind) \
             VALUES ({seq}, {version}, {name}, {checksum}, {by}, {exec_ms}, \
                     'completed', 'success', 'apply')"
        ))
        .await
        .map_err(|e| step_err(table, e))?;

    Ok(())
}

/// Wrap an actor error as a [`RebuildError::Step`] for `table`.
fn step_err(table: &str, source: SqliteActorError) -> RebuildError {
    RebuildError::Step {
        table: table.to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmp_name_is_engine_chosen_suffix() {
        assert_eq!(SqliteRebuildSpec::tmp_name("users"), "users__zsrebuild");
    }

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(quote_ident("users"), "\"users\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
