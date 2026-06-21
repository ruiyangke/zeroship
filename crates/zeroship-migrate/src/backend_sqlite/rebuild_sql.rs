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
//! LIVE `sqlite_master` **at execution time, here in the backend** (this is the
//! layer holding the connection) BEFORE the `DROP TABLE`, and replayed verbatim
//! AFTER the rename — so partial/expression/collation/DESC index attributes and
//! creator triggers survive the rebuild exactly (C2). A dependent object that
//! genuinely cannot be replayed (it references a now-dropped column) FAILS CLOSED
//! ([`RebuildError::DependentReplayFailed`]) — it is never silently destroyed.
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
//!  (engine, EngineJournal)              <capture verbatim sql FROM sqlite_master
//!                                        for <t>'s indexes + triggers>   [main, read]
//!  (engine, EngineJournal)              DROP TABLE IF EXISTS <tmp>      -- H2: clear pollution
//!  (engine→CreatorUp)                   CREATE TABLE <tmp> (...new shape...)   [main]
//!                                       INSERT INTO <tmp> (cols) SELECT cols FROM <t>
//!                                       DROP TABLE <t>
//!                                       ALTER TABLE <tmp> RENAME TO <t>
//!                                       <replay each captured index / trigger
//!                                        VERBATIM + any explicit recreate_objects> [main]
//!  (engine, EngineJournal)              PRAGMA foreign_key_check       -- UNSCOPED (H1)
//!  (engine, EngineJournal)              INSERT _mig journal (event_seq AUTOINCREMENT)
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
    /// A dependent object (index / trigger / view) captured verbatim from the live
    /// `sqlite_master` BEFORE the swap could NOT be replayed after the swap (e.g. it
    /// references a column the new shape dropped). FAIL CLOSED rather than silently
    /// lose a creator trigger/view/index: the transaction is rolled back; the
    /// original table — and every dependent object — is intact (C2).
    #[error(
        "sqlite rebuild of '{table}' aborted: the dependent {kind} '{object}' could not be \
         recreated after the rebuild ({source}); the original table and its dependents are \
         intact. The object likely references a column the new shape removed — author a \
         compensating change"
    )]
    DependentReplayFailed {
        /// The table the rebuild targeted.
        table: String,
        /// The kind of dependent object (`index` / `trigger` / `view`).
        kind: String,
        /// The dependent object's name (from `sqlite_master`).
        object: String,
        /// The underlying actor error from the failed replay.
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
    /// EXTRA dependent DDL to replay AFTER the rename, on top of the verbatim
    /// objects the executor captures from the live `sqlite_master` itself (C2). The
    /// executor is the source of truth for the table's OWN indexes/triggers: it
    /// snapshots their `sql` TEXT verbatim before the `DROP TABLE` and replays it
    /// after the rename, so partial/expression/collation/DESC index attributes and
    /// creator triggers are preserved exactly. The differ therefore leaves this
    /// EMPTY for the table's own objects. It remains as an explicit escape hatch for
    /// direct-spec callers/tests; each entry is a single CREATE replayed under
    /// CreatorUp on `main`. A replay failure FAILS CLOSED
    /// ([`RebuildError::DependentReplayFailed`]) — never silently lost.
    pub recreate_objects: Vec<String>,
    /// **H1** — the BARE names of columns being DROPPED by this rebuild (live
    /// columns absent from the new shape, excluding a rename's `from`). A captured
    /// dependent (index / trigger) whose verbatim DDL references one of these
    /// columns CANNOT be replayed after the swap (the column no longer exists) — so
    /// the executor SKIPS replaying it (the dependent is intentionally dropped WITH
    /// the column, the data-preserving outcome). Empty for a rebuild that drops no
    /// column (type / nullability / rename / FK-set change), where every captured
    /// dependent is replayed verbatim as before.
    pub dropped_columns: Vec<String>,
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
            //
            // M2 — flip to EngineJournal BEFORE the ROLLBACK. `set_mode` is an
            // INFALLIBLE atomic store on the actor's `AuthMode` flag (it returns
            // `Result` only for the queue round-trip; the flip itself cannot fail),
            // so this never leaves the connection mid-mode. It MUST precede the
            // ROLLBACK because `ROLLBACK` is a transaction-control statement and the
            // authorizer gates transaction control to EngineJournal: were we still in
            // CreatorUp here, the authorizer would DENY the ROLLBACK at prepare, the
            // transaction would stay open, and the connection would wedge (the
            // is_autocommit probe below would then correctly flag it Poisoned).
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

    // (0) C2 — Capture, VERBATIM, the `sql` TEXT of every dependent object the
    //     `DROP TABLE <t>` below would destroy: the table's own indexes and its
    //     triggers, read from the LIVE `main.sqlite_master` BEFORE the drop. This is
    //     a read of `main`, allowed under EngineJournal (the engine is doing it). We
    //     capture the EXACT stored DDL so partial/expression/collation/DESC index
    //     attributes and creator triggers survive — the lossy desired-IndexSnapshot
    //     recreate is gone (it dropped those attrs and never touched triggers). The
    //     table name is unchanged after the RENAME, so the captured DDL re-applies
    //     cleanly. Views are DB-global (not dropped WITH the table) and are left
    //     untouched; if a view referenced a now-removed column SQLite surfaces that
    //     at query time, not here.
    let captured = capture_dependents(actor, &spec.table).await?;

    // --- The rebuild DDL runs under CreatorUp (engine-authored, on `main`). ---
    actor
        .set_mode(Mode::CreatorUp)
        .await
        .map_err(|e| step_err(table, e))?;

    // (a0) H2 — Drop any stale temp table FIRST. The shared emitter renders the temp
    //      CREATE as `CREATE TABLE IF NOT EXISTS <tmp>`; a creator who pre-created
    //      `<t>__zsrebuild` in a prior CreatorUp migration would otherwise have the
    //      `IF NOT EXISTS` SILENTLY REUSE their polluted table — we'd INSERT…SELECT
    //      into the stale shape and RENAME the pollution into place. This engine-
    //      controlled DROP (under CreatorUp, on `main`) clears any such pollution so
    //      the CREATE below always lands on a clean temp. It is inside the txn, so a
    //      later abort rolls it back too.
    actor
        .exec(&format!("DROP TABLE IF EXISTS {tmp_q}"))
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

    // (e) C2 — Replay the VERBATIM captured dependent DDL (the table's own indexes +
    //     triggers), then any EXTRA explicit `recreate_objects` the spec carries.
    //     SQLite drops a table's indexes/triggers WITH the table, so they must be
    //     replayed; the captured `sql` is the EXACT stored definition (full attrs).
    //     Each is a CREATE on `main` — allowed in CreatorUp. A replay that FAILS
    //     (e.g. the object references a column the new shape dropped) FAILS CLOSED
    //     with a typed error so the txn rolls back and the object is never silently
    //     lost — the original table + all dependents are restored.
    for obj in &captured {
        // H1 — a captured dependent (index / trigger) whose DDL references a column
        // this rebuild DROPS cannot be replayed (the column is gone); it is dropped
        // WITH the column, which is the data-preserving intent. SKIP it rather than
        // FAIL CLOSED. The match is whole-word case-insensitive so a column name is
        // not matched as a substring of another identifier. (A dependent that
        // references ONLY surviving columns still replays verbatim, preserving its
        // full attributes; a dependent that fails for an UNRELATED reason still
        // FAILS CLOSED below.)
        if spec
            .dropped_columns
            .iter()
            .any(|dc| ddl_references_column(&obj.sql, dc))
        {
            continue;
        }
        actor.exec(&obj.sql).await.map_err(|e| RebuildError::DependentReplayFailed {
            table: spec.table.clone(),
            kind: obj.kind.clone(),
            object: obj.name.clone(),
            source: e,
        })?;
    }
    for obj in &spec.recreate_objects {
        actor.exec(obj).await.map_err(|e| step_err(table, e))?;
    }

    // --- FK integrity check + journal write run under EngineJournal. ---
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(|e| step_err(table, e))?;

    // (f) H1 — PRAGMA foreign_key_check, UNSCOPED (no table arg). This WORKS inside a
    //     transaction (unlike the foreign_keys toggle). A check scoped to <t> only
    //     catches orphans IN <t>; but a rebuild of a PARENT that drops a referenced
    //     row orphans rows in a CHILD table — a scoped check passes and commits a
    //     referentially-broken DB. The engine's steady state is `foreign_keys=ON`, so
    //     there are NO pre-existing violations on the connection; therefore ANY row an
    //     unscoped check returns was introduced by THIS rebuild. Any such row aborts
    //     (typed error) so the txn rolls back and the original tables are restored.
    let violations = actor
        .query("PRAGMA main.foreign_key_check")
        .await
        .map_err(|e| step_err(table, e))?;
    if !violations.is_empty() {
        return Err(RebuildError::ForeignKeyViolation {
            table: spec.table.clone(),
            violations: violations.len(),
        });
    }

    // (g) INSERT the immutable journal row — a rebuild is an `applied`/`apply`-kind
    //     event, like any other applied migration (event_seq is AUTOINCREMENT, not
    //     supplied). SEPARATE prepares from the rebuild DDL; mode already flipped.
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let version = journal_sql::sql_lit(m.version.as_str());
    let name = journal_sql::sql_lit(&m.name);
    let checksum = journal_sql::sql_lit(m.checksum.as_str());
    let by = journal_sql::sql_lit(applied_by);
    actor
        .exec(&format!(
            "INSERT INTO \"_mig\".schema_migrations \
             (event_kind, version, name, checksum, \"by\", exec_ms, phase, outcome, kind) \
             VALUES ('applied', {version}, {name}, {checksum}, {by}, {exec_ms}, \
                     'completed', 'success', 'apply')"
        ))
        .await
        .map_err(|e| step_err(table, e))?;

    Ok(())
}

/// One dependent object captured VERBATIM from `sqlite_master` before the drop, so
/// it can be replayed after the rename (C2).
struct CapturedObject {
    /// `index` or `trigger` (the `type` column of `sqlite_master`).
    kind: String,
    /// The object's name (for the typed fail-closed error).
    name: String,
    /// The EXACT stored `sql` DDL — replayed verbatim.
    sql: String,
}

/// C2 — Capture the VERBATIM `sql` of every dependent object the `DROP TABLE <table>`
/// will destroy: the table's own indexes and its triggers, from the live
/// `main.sqlite_master`, BEFORE the drop.
///
/// - Indexes: `type='index' AND tbl_name=<table> AND sql IS NOT NULL`. The
///   `sql IS NOT NULL` filter SKIPS auto-indexes (the implicit index SQLite creates
///   for a UNIQUE/PRIMARY KEY constraint has a NULL `sql` — it is re-derived from the
///   new CREATE, never replayed as DDL).
/// - Triggers: `type='trigger' AND tbl_name=<table>` (a trigger always has `sql`).
///
/// Views are NOT captured here: a view is DB-global and is NOT dropped WITH the
/// table, so it survives the rebuild untouched (the table name is unchanged after the
/// rename). Run under EngineJournal — this is an engine read of `main.sqlite_master`.
async fn capture_dependents(
    actor: &MigrationActor,
    table: &str,
) -> Result<Vec<CapturedObject>, RebuildError> {
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(|e| step_err(table, e))?;
    // The table name is bound via a SQL string literal (engine-controlled identifier,
    // quoted defensively). We read `type`, `name`, `sql` and ORDER BY rowid so the
    // replay order is deterministic (the stored creation order).
    let table_lit = journal_sql::sql_lit(table);
    let rows = actor
        .query(&format!(
            "SELECT type, name, sql FROM main.sqlite_master \
             WHERE tbl_name = {table_lit} AND type IN ('index', 'trigger') \
               AND sql IS NOT NULL \
             ORDER BY rowid"
        ))
        .await
        .map_err(|e| step_err(table, e))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        // Defensive: each row is (type, name, sql); skip any with a NULL sql (the
        // WHERE already excludes them, but the materializer hands back Options).
        let kind = r.first().and_then(|c| c.clone());
        let name = r.get(1).and_then(|c| c.clone());
        let sql = r.get(2).and_then(|c| c.clone());
        if let (Some(kind), Some(name), Some(sql)) = (kind, name, sql) {
            out.push(CapturedObject { kind, name, sql });
        }
    }
    Ok(out)
}

/// True iff `ddl` references the column `col` as a whole word (case-insensitive).
/// Used by the H1 replay-skip: a captured dependent (index / trigger) whose DDL
/// names a dropped column cannot be replayed after the swap. Whole-word so `id`
/// does not match `idx` / `user_id`; a double-quote is a word boundary so a quoted
/// `"col"` reference matches. Conservative: a coincidental match (e.g. the column
/// name inside a string literal) only causes the dependent to be DROPPED rather
/// than replayed — never a wrongful replay that would error the whole rebuild.
fn ddl_references_column(ddl: &str, col: &str) -> bool {
    if col.is_empty() {
        return false;
    }
    let hay = ddl.to_ascii_lowercase();
    let need = col.to_ascii_lowercase();
    let hb = hay.as_bytes();
    let nb = need.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
    let mut start = 0;
    while let Some(pos) = hay[start..].find(&need) {
        let abs = start + pos;
        let before = abs.checked_sub(1).map(|p| hb[p]);
        let after = hb.get(abs + nb.len()).copied();
        if before.is_none_or(|b| !is_ident(b)) && after.is_none_or(|b| !is_ident(b)) {
            return true;
        }
        start = abs + 1;
    }
    false
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

    // H1 — the replay-skip column scan. A captured dependent's DDL referencing a
    // dropped column (whole-word, case-insensitive, quoted or bare) must match, so
    // the executor skips replaying it; a substring of another identifier must NOT.
    #[test]
    fn ddl_references_column_is_whole_word_ci() {
        assert!(ddl_references_column(
            "CREATE INDEX i ON t (\"drop_me\")",
            "drop_me"
        ));
        assert!(ddl_references_column("CREATE INDEX i ON t (drop_me)", "drop_me"));
        assert!(ddl_references_column("CREATE INDEX i ON t (DROP_ME)", "drop_me"));
        // Substring must NOT match: `id` is not `user_id` / `idx`.
        assert!(!ddl_references_column(
            "CREATE INDEX idx ON t (user_id)",
            "id"
        ));
        // A dependent over an unrelated column does not match.
        assert!(!ddl_references_column(
            "CREATE INDEX i ON t (keep)",
            "drop_me"
        ));
        assert!(!ddl_references_column("anything", ""));
    }
}
