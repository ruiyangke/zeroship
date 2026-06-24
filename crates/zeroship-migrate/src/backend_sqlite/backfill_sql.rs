//! The SQLite **batched / resumable backfill executor** (design §2.3.1) — the
//! SQLite analog of the Postgres `backfill.rs` (`run_backfill`), completing the
//! §1.1 "one script, both backends, DDL+DML" headline.
//!
//! The PG executor is structurally Postgres-only: it emits a data-modifying CTE
//! (`WITH _bf_window AS (…), _bf_upd AS (UPDATE … RETURNING …)`) — SQLite has no
//! writable CTEs — and derives the cursor type from `pg_catalog`. This module is
//! the SQLite peer: a loop of **plain statements**, each its own
//! `BEGIN IMMEDIATE … COMMIT` transaction on the single hardened migration
//! connection ([`MigrationActor`]).
//!
//! # Per-batch statement (the engine-owned window)
//!
//! ```text
//! BEGIN IMMEDIATE;                 -- engine mode owns the txn boundary
//!   (CreatorUp)  UPDATE "<table>" SET <authored set>
//!                  WHERE rowid IN (
//!                    SELECT rowid FROM "<table>"
//!                     WHERE <cursor_col> > ?1 [AND (<filter>)]   -- ?1 omitted on batch 1
//!                     ORDER BY <cursor_col> ASC LIMIT ?n
//!                  )
//!                RETURNING <cursor_col>;                          -- touched cursors
//!   (EngineJournal) UPDATE "_mig".schema_backfills SET last_cursor=…, rows_done+=…, … ;
//! COMMIT;                          -- batch mutation + progress advance commit together
//! ```
//!
//! The batch `UPDATE` runs under the confined **CreatorUp** authorizer mode (denied
//! from `_mig` / PRAGMA / txn boundaries / vtables — exactly the confinement a
//! creator `up` runs under); the progress write runs under **EngineJournal** (the
//! engine owns `_mig`). They commit in the SAME `BEGIN IMMEDIATE` transaction, so a
//! crash leaves **both or neither**: either the rows are updated AND the cursor
//! advanced, or nothing. On the next run the loop reads the last committed cursor
//! and re-runs strictly after it — never restarting from zero, never skipping,
//! never double-applying (a crash before COMMIT rolled the mutation back too, so
//! re-running the same window is correct even for a non-idempotent transform).
//!
//! # Reuse of the shared SQLite-DML seam (no re-implementation)
//!
//! - The per-batch `UPDATE … RETURNING` is run via
//!   [`MigrationActor::query_params`], binding the cursor + limit through the SAME
//!   native `?n` protocol (`crate::dml::sqlite_placeholder` /
//!   [`SqliteBind`](super::actor::SqliteBind)) the one-shot DML assembler uses — the
//!   two never fork a divergent `?n`-binding copy.
//! - The authored `set` / `filter` SQL strings come from
//!   [`crate::dml::assemble_backfill_clauses`] (the SAME assembler the PG path
//!   uses), which renders the closed-AST transform — including the §9
//!   `c.fn.splitPart` lowering — to inline SQL, `''`-escaping every string literal.
//!   The whole assembled statement runs under the hardened authorizer (the SQLite
//!   analog of the PG guard), so a hostile literal cannot alter statement shape.
//!
//! # Crash-safe progress
//!
//! Progress lives in the attached `_mig` database (the SQLite journal sibling),
//! the `schema_backfills` table — the SQLite mirror of the PG meta-schema progress
//! table. It is written under EngineJournal mode (the creator `UPDATE` in CreatorUp
//! can never forge or skip it — `_mig` is denied to CreatorUp). The progress-row
//! identity is the spec's stable [`BackfillSpec::backfill_id`] (table + cursor +
//! transform + name), so a re-authored backfill gets a fresh id and does not resume
//! against an incompatible cursor.

use crate::backfill::{BackfillOutcome, BackfillError, BackfillSpec};
use crate::dml::sqlite_placeholder;

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;
use super::journal_sql::sql_lit;

/// Validate a bare SQL identifier (non-empty, `[A-Za-z_][A-Za-z0-9_]*`). Rejects
/// schema-qualified / quoted-injection / whitespace, so the value is safe to
/// double-quote into an identifier position. Mirrors the PG executor's
/// `validate_ident`.
fn validate_ident(what: &'static str, value: &str) -> Result<(), BackfillError> {
    let ok = !value.is_empty()
        && value.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(BackfillError::InvalidIdentifier { what, value: value.to_string() });
    }
    Ok(())
}

/// Double-quote a validated identifier (`"` → `""` — belt-and-suspenders; the value
/// already passed [`validate_ident`] so it has no `"`).
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Resolved facts about the cursor column from `PRAGMA table_info` (an engine-only
/// pragma): whether it is the table's single-column PRIMARY KEY, whether it is
/// `NOT NULL`, and its declared-type **affinity** (for binding the prior cursor).
struct CursorInfo {
    /// `true` iff the cursor column is the sole `PRIMARY KEY` column.
    is_single_pk: bool,
    /// `true` iff the column is declared `NOT NULL` (a PK column is implicitly so).
    not_null: bool,
    /// `true` iff the column's declared-type affinity is NUMERIC/INTEGER/REAL (so
    /// the prior cursor binds as an integer for natural-order `>` comparison); else
    /// it binds as TEXT.
    numeric_affinity: bool,
    /// `true` iff the column exists at all.
    exists: bool,
}

/// SQLite type-affinity rule (the documented algorithm): a declared type whose name
/// contains `INT` → INTEGER affinity; `CHAR`/`CLOB`/`TEXT` → TEXT; `BLOB` or empty →
/// BLOB; `REAL`/`FLOA`/`DOUB` → REAL; else → NUMERIC. We only need to know whether
/// the affinity is numeric (INTEGER/REAL/NUMERIC) vs textual, to bind the cursor.
fn is_numeric_affinity(decl_type: &str) -> bool {
    let t = decl_type.to_ascii_uppercase();
    if t.contains("INT") {
        return true; // INTEGER affinity
    }
    if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
        return false; // TEXT affinity
    }
    if t.is_empty() || t.contains("BLOB") {
        return false; // BLOB affinity — bind as text (lexical), the conservative choice
    }
    // REAL / FLOA / DOUB → REAL; everything else → NUMERIC. Both numeric.
    true
}

/// Resolve the cursor column via `PRAGMA table_info("<table>")` (engine mode). The
/// rowid alias columns (`rowid`/`oid`/`_rowid_`) do not appear in `table_info`; a
/// caller paging on a non-INTEGER-PK should name a real UNIQUE NOT NULL column,
/// matching the PG executor's UNIQUE-NOT-NULL requirement.
async fn resolve_cursor_info(
    actor: &MigrationActor,
    spec: &BackfillSpec,
) -> Result<CursorInfo, SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    // table_info columns: cid, name, type, notnull, dflt_value, pk.
    let rows = actor
        .query(&format!("PRAGMA table_info({})", quote_ident(&spec.table)))
        .await?;
    let mut info = CursorInfo {
        is_single_pk: false,
        not_null: false,
        numeric_affinity: false,
        exists: false,
    };
    let mut pk_cols = 0usize;
    let mut this_pk = false;
    for r in &rows {
        let pk: i64 = r.get(5).and_then(|c| c.clone()).and_then(|s| s.parse().ok()).unwrap_or(0);
        if pk > 0 {
            pk_cols += 1;
        }
        let name = r.get(1).and_then(|c| c.clone()).unwrap_or_default();
        if name == spec.cursor_column {
            info.exists = true;
            let decl = r.get(2).and_then(|c| c.clone()).unwrap_or_default();
            info.numeric_affinity = is_numeric_affinity(&decl);
            let nn: i64 =
                r.get(3).and_then(|c| c.clone()).and_then(|s| s.parse().ok()).unwrap_or(0);
            info.not_null = nn != 0 || pk > 0;
            this_pk = pk > 0;
        }
    }
    info.is_single_pk = this_pk && pk_cols == 1;
    Ok(info)
}

/// Whether the cursor column is backed by a single-column UNIQUE index (the
/// non-PK route to a safe pageable key), via `PRAGMA index_list` + `index_info`.
async fn cursor_has_single_unique_index(
    actor: &MigrationActor,
    spec: &BackfillSpec,
) -> Result<bool, SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    // index_list columns: seq, name, unique, origin, partial.
    let idx = actor
        .query(&format!("PRAGMA index_list({})", quote_ident(&spec.table)))
        .await?;
    for r in &idx {
        let unique: i64 =
            r.get(2).and_then(|c| c.clone()).and_then(|s| s.parse().ok()).unwrap_or(0);
        let partial: i64 =
            r.get(4).and_then(|c| c.clone()).and_then(|s| s.parse().ok()).unwrap_or(0);
        if unique == 0 || partial != 0 {
            continue;
        }
        let name = r.get(1).and_then(|c| c.clone()).unwrap_or_default();
        // index_info columns: seqno, cid, name.
        let cols = actor.query(&format!("PRAGMA index_info({})", sql_lit(&name))).await?;
        if cols.len() == 1 {
            let col = cols[0].get(2).and_then(|c| c.clone()).unwrap_or_default();
            if col == spec.cursor_column {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Bootstrap (idempotently) the `_mig` **backfill progress** table — the SQLite
/// mirror of the PG meta-schema `schema_backfills`. Engine-mode (the migrator /
/// CreatorUp has no `_mig` grant — the journal's deny-by-absence model). Lives in
/// the same attached journal database as the immutable journal.
pub(crate) async fn ensure_backfill_progress(
    actor: &MigrationActor,
) -> Result<(), SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    actor
        .exec(
            "CREATE TABLE IF NOT EXISTS \"_mig\".schema_backfills (\
                backfill_id    TEXT PRIMARY KEY, \
                name           TEXT NOT NULL, \
                target_table   TEXT NOT NULL, \
                cursor_column  TEXT NOT NULL, \
                last_cursor    TEXT, \
                rows_done      INTEGER NOT NULL DEFAULT 0, \
                batches_done   INTEGER NOT NULL DEFAULT 0, \
                complete       INTEGER NOT NULL DEFAULT 0, \
                applied_by     TEXT NOT NULL, \
                started_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                updated_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
        )
        .await?;
    Ok(())
}

/// The committed progress of a backfill (the resume anchor). `None` until the
/// progress row is inserted.
struct Progress {
    last_cursor: Option<String>,
    complete: bool,
    exists: bool,
}

/// Read the committed progress row for `backfill_id` (engine mode, read-only).
async fn read_progress(
    actor: &MigrationActor,
    backfill_id: &str,
) -> Result<Progress, SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    let rows = actor
        .query(&format!(
            "SELECT last_cursor, complete FROM \"_mig\".schema_backfills WHERE backfill_id = {}",
            sql_lit(backfill_id)
        ))
        .await?;
    match rows.into_iter().next() {
        None => Ok(Progress { last_cursor: None, complete: false, exists: false }),
        Some(r) => Ok(Progress {
            last_cursor: r.first().and_then(|c| c.clone()),
            complete: r.get(1).and_then(|c| c.clone()).map(|s| s != "0").unwrap_or(false),
            exists: true,
        }),
    }
}

/// Build the per-batch `UPDATE … RETURNING` statement. `have_cursor` selects the
/// shape: with a prior cursor the window is `cursor_col > ?1 [AND (filter)]` and the
/// limit binds at `?2`; on the first batch there is no cursor bind and the limit
/// binds at `?1`. The authored `set` / `filter` are inline SQL strings from the
/// shared assembler (`assemble_backfill_clauses`), `''`-escaped; the cursor + limit
/// are NATIVE `?n` binds (`sqlite_placeholder`). RETURNING the cursor column yields
/// the touched cursors so the loop derives the next window's lower bound.
fn build_batch_sql(
    table_q: &str,
    cursor_q: &str,
    set_clause: &str,
    filter: Option<&str>,
    have_cursor: bool,
) -> String {
    let filter_sql = filter.map(|f| format!(" AND ({f})")).unwrap_or_default();
    if have_cursor {
        let cursor_ph = sqlite_placeholder(1);
        let limit_ph = sqlite_placeholder(2);
        format!(
            "UPDATE {table_q} SET {set_clause} \
             WHERE rowid IN ( \
                SELECT rowid FROM {table_q} \
                 WHERE {cursor_q} > {cursor_ph}{filter_sql} \
                 ORDER BY {cursor_q} ASC LIMIT {limit_ph} \
             ) RETURNING {cursor_q}"
        )
    } else {
        let limit_ph = sqlite_placeholder(1);
        format!(
            "UPDATE {table_q} SET {set_clause} \
             WHERE rowid IN ( \
                SELECT rowid FROM {table_q} \
                 WHERE 1=1{filter_sql} \
                 ORDER BY {cursor_q} ASC LIMIT {limit_ph} \
             ) RETURNING {cursor_q}"
        )
    }
}

/// Run (or resume) a SQLite batched backfill, the SQLite analog of
/// [`crate::backfill::run_backfill`]. Pages `spec.table` by `spec.cursor_column` in
/// `spec.batch_size` chunks, each its own committed transaction, resumable from the
/// committed progress cursor. `max_batches` bounds the run (`None` = run to
/// completion) — the checkpoint/crash-fuzz seam.
///
/// # Errors
/// [`BackfillError`] on a malformed spec, an unsafe cursor column (not a
/// single-column PK/UNIQUE, or nullable), a cursor-column mutation, a batch failure
/// (rolled back, resumable), or an infrastructure failure.
pub(crate) async fn run_backfill_bounded(
    actor: &MigrationActor,
    spec: &BackfillSpec,
    set_clause: &str,
    filter: Option<&str>,
    applied_by: &str,
    max_batches: Option<u64>,
) -> Result<BackfillOutcome, BackfillError> {
    // Gate 1 — identifier + batch-size validation, BEFORE any SQL is assembled.
    validate_ident("table", &spec.table)?;
    validate_ident("cursor_column", &spec.cursor_column)?;
    if spec.batch_size == 0 {
        return Err(BackfillError::InvalidBatchSize);
    }

    let backfill_id = spec.backfill_id();
    ensure_backfill_progress(actor).await.map_err(sqlite_journal_err)?;

    // Gate 2 — resolve the cursor column: it MUST exist, be a single-column
    // PRIMARY KEY or UNIQUE index, and be NOT NULL (the §5 exactly-once + bounded +
    // filter-honoring correctness rests on a UNIQUE NOT NULL cursor; a non-unique
    // cursor over-matches every row sharing a key, a NULL cursor is never paged).
    let info = resolve_cursor_info(actor, spec).await.map_err(sqlite_journal_err)?;
    if !info.exists {
        return Err(BackfillError::TargetNotFound(format!(
            "{} column {} not found",
            spec.table, spec.cursor_column
        )));
    }
    let is_unique = info.is_single_pk
        || cursor_has_single_unique_index(actor, spec).await.map_err(sqlite_journal_err)?;
    if !is_unique {
        return Err(BackfillError::CursorNotUniqueNotNull {
            table: spec.table.clone(),
            cursor_column: spec.cursor_column.clone(),
            reason: "not backed by a single-column PRIMARY KEY or UNIQUE index",
        });
    }
    if !info.not_null {
        return Err(BackfillError::CursorNotUniqueNotNull {
            table: spec.table.clone(),
            cursor_column: spec.cursor_column.clone(),
            reason: "it is nullable",
        });
    }

    let table_q = quote_ident(&spec.table);
    let cursor_q = quote_ident(&spec.cursor_column);

    // Gate 3 — the authored transform MUST NOT assign the cursor column itself
    // (mutating the paged key breaks the cursor → re-processing / loop /
    // double-apply). A structural pre-flight scan of the assembled SET assignments:
    // each assignment is rendered `"col" = …`, so a leading `"<cursor>" =` token at
    // an assignment boundary is the mutation. We scan the comma-separated SET list.
    assert_cursor_not_mutated(set_clause, &spec.cursor_column)?;

    // Resume from the last committed cursor (if any).
    let existing = read_progress(actor, &backfill_id).await.map_err(sqlite_journal_err)?;
    let resumed = existing.exists && existing.last_cursor.is_some();
    if existing.complete {
        // Already complete — idempotent no-op re-run.
        return Ok(BackfillOutcome {
            backfill_id,
            batches: 0,
            rows_updated: 0,
            resumed,
            complete: true,
        });
    }
    let mut last_cursor: Option<String> = existing.last_cursor.clone();

    // Insert the progress row up-front (idempotent) so observability sees the
    // backfill exists even before the first batch commits.
    upsert_progress_row(actor, &backfill_id, spec, applied_by).await.map_err(sqlite_journal_err)?;

    let mut batches: u64 = 0;
    let mut rows_updated: u64 = 0;
    let mut tail_reached = false;

    loop {
        if max_batches.is_some_and(|m| batches >= m) {
            break; // bound hit before the tail — leave NOT complete; resumable.
        }
        let (n, max_cursor) = run_one_batch(
            actor,
            &backfill_id,
            &table_q,
            &cursor_q,
            set_clause,
            filter,
            info.numeric_affinity,
            spec.batch_size,
            last_cursor.as_deref(),
        )
        .await?;
        if n == 0 {
            tail_reached = true;
            break;
        }
        batches += 1;
        rows_updated += n;
        last_cursor = max_cursor;
        // Fault seam (test-only): a simulated crash BETWEEN batches — the last
        // batch's UPDATE + cursor advance already COMMITted, but the backfill is NOT
        // marked complete, so a resume reads the committed cursor and finishes the
        // tail (the §5 resumability invariant). Identical seam to the PG executor.
        if let Err(e) = crate::fault::trip(crate::fault::points::BACKFILL_MID_BATCHES) {
            return Err(BackfillError::Fault(e.to_string()));
        }
        if n < u64::from(spec.batch_size) {
            tail_reached = true;
            break;
        }
    }
    if tail_reached {
        mark_complete(actor, &backfill_id).await.map_err(sqlite_journal_err)?;
    }

    Ok(BackfillOutcome {
        backfill_id,
        batches,
        rows_updated,
        resumed,
        complete: tail_reached,
    })
}

/// Reject an authored `set_clause` that assigns the cursor column. The assembler
/// renders each assignment as `"<col>" = <expr>`, comma-joined; a `"<cursor>" =`
/// token at an ASSIGNMENT BOUNDARY is the illegal mutation. Splitting on top-level
/// commas is unsafe (a CASE/function arg may contain commas), so instead we scan
/// the clause for the cursor-assignment LHS at a boundary — the very start of the
/// clause, or right after a top-level comma — while SKIPPING single-quoted string
/// literals (with `''` escaping). Skipping literals is what makes the check
/// correct AND precise: a string literal that embeds the `, "<cursor>" =` byte
/// sequence is RHS data, not an assignment, so it is NOT a false-positive reject
/// (the over-rejection the prior `contains`-based heuristic produced). Fail-closed:
/// any boundary occurrence of the cursor LHS rejects; a genuine later-position
/// cursor mutation is still caught.
fn assert_cursor_not_mutated(set_clause: &str, cursor_column: &str) -> Result<(), BackfillError> {
    let needle = format!("{} =", quote_ident(cursor_column));
    // The assembler emits `"<col>" = <expr>` for every assignment, comma-joined, so
    // the cursor is mutated iff the cursor-assignment LHS appears at an assignment
    // BOUNDARY: the very start of the clause, or right after a top-level (outside any
    // string literal) comma. We scan the clause OUTSIDE single-quoted string
    // literals only, so a literal that happens to embed `, "id" =` is RHS data, not a
    // mutation (the over-rejection the prior `contains` heuristic produced). SQLite
    // single-quote escaping is `''`; the scanner treats a doubled quote inside a
    // literal as an escaped quote, not a close, so it never desyncs. Fail-closed: any
    // boundary occurrence of the cursor-assignment LHS rejects.
    let bytes = set_clause.as_bytes();
    let mut i = 0usize;
    let mut in_str = false;
    // `at_boundary` is true at the start and immediately after a top-level comma
    // (skipping leading whitespace), i.e. exactly where an assignment LHS begins.
    let mut at_boundary = true;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\'' {
                // `''` is an escaped quote (stay in the literal); a lone `'` closes it.
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_str = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                in_str = true;
                at_boundary = false;
                i += 1;
            }
            b' ' | b'\t' | b'\n' | b'\r' => {
                // whitespace does not end an assignment boundary (allows `,  "id" =`).
                i += 1;
            }
            b',' => {
                at_boundary = true;
                i += 1;
            }
            _ => {
                if at_boundary && set_clause[i..].starts_with(&needle) {
                    return Err(BackfillError::CursorColumnMutated {
                        cursor_column: cursor_column.to_string(),
                    });
                }
                at_boundary = false;
                i += 1;
            }
        }
    }
    Ok(())
}

/// Map a [`SqliteActorError`] into a [`BackfillError`] for a journal/meta-side
/// failure (progress bootstrap/read/write), mirroring the PG path's
/// `BackfillError::Journal`.
fn sqlite_journal_err(e: SqliteActorError) -> BackfillError {
    BackfillError::Journal(crate::journal::JournalError::Backend(e.to_string()))
}

/// Insert the progress row for a fresh backfill (idempotent — `ON CONFLICT DO
/// NOTHING`, so a resumed run keeps its committed cursor). Engine mode.
async fn upsert_progress_row(
    actor: &MigrationActor,
    backfill_id: &str,
    spec: &BackfillSpec,
    applied_by: &str,
) -> Result<(), SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    actor
        .exec(&format!(
            "INSERT INTO \"_mig\".schema_backfills \
                 (backfill_id, name, target_table, cursor_column, applied_by) \
             VALUES ({}, {}, {}, {}, {}) \
             ON CONFLICT (backfill_id) DO NOTHING",
            sql_lit(backfill_id),
            sql_lit(&spec.name),
            sql_lit(&spec.table),
            sql_lit(&spec.cursor_column),
            sql_lit(applied_by),
        ))
        .await
}

/// Mark a backfill complete (engine mode, its own statement).
async fn mark_complete(actor: &MigrationActor, backfill_id: &str) -> Result<(), SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    actor
        .exec(&format!(
            // `updated_at` is intentionally NOT touched (CURRENT_TIMESTAMP as an
            // UPDATE-SET function is authorizer-denied; the column is
            // observability-only with an INSERT-time default).
            "UPDATE \"_mig\".schema_backfills \
                SET complete = 1 WHERE backfill_id = {}",
            sql_lit(backfill_id)
        ))
        .await
}

/// Run ONE batch in its own `BEGIN IMMEDIATE … COMMIT` transaction (the
/// non-blocking, crash-safe unit). Returns `(rows_updated, new_max_cursor)`.
///
/// The batch `UPDATE … RETURNING <cursor>` runs under CreatorUp (the confined
/// mode); the progress advance runs under EngineJournal; both commit together. On
/// any failure the transaction is rolled back (progress NOT advanced) and the
/// connection's autocommit state is re-asserted (H1 — a wedged connection is a
/// hard error, not a silent reuse).
#[allow(clippy::too_many_arguments)]
async fn run_one_batch(
    actor: &MigrationActor,
    backfill_id: &str,
    table_q: &str,
    cursor_q: &str,
    set_clause: &str,
    filter: Option<&str>,
    cursor_numeric: bool,
    batch_size: u32,
    last_cursor: Option<&str>,
) -> Result<(u64, Option<String>), BackfillError> {
    use super::actor::SqliteBind;

    let have_cursor = last_cursor.is_some();
    let batch_sql = build_batch_sql(table_q, cursor_q, set_clause, filter, have_cursor);

    // The cursor bind takes the column's affinity so `cursor > ?` uses NATURAL
    // (numeric) order, not lexical — the SQLite analog of the PG `::cursor_type`
    // cast. A numeric-affinity cursor binds as an integer (the IR/PK domain is
    // integer keys); a textual cursor binds as text.
    let mut binds: Vec<SqliteBind> = Vec::with_capacity(2);
    if let Some(lc) = last_cursor {
        if cursor_numeric {
            match lc.parse::<i64>() {
                Ok(i) => binds.push(SqliteBind::Int(i)),
                // A numeric-affinity column whose committed cursor is non-integral
                // (a REAL key) binds as text; SQLite's numeric affinity on the
                // column side still coerces the comparison. Conservative fallback.
                Err(_) => binds.push(SqliteBind::Text(lc.to_string())),
            }
        } else {
            binds.push(SqliteBind::Text(lc.to_string()));
        }
    }
    binds.push(SqliteBind::Int(i64::from(batch_size)));

    // 1. BEGIN IMMEDIATE under engine mode (the authorizer allows SQLITE_TRANSACTION
    //    only in EngineJournal — the engine owns txn boundaries).
    actor.set_mode(Mode::EngineJournal).await.map_err(batch_infra_err)?;
    actor.exec("BEGIN IMMEDIATE").await.map_err(batch_infra_err)?;

    let result = async {
        // 2. Run the batch UPDATE … RETURNING under the confined CreatorUp mode
        //    (denied from `_mig`, PRAGMA, txn boundaries, vtables). The cursor +
        //    limit are positional `?n` binds; the authored set/filter are inline.
        actor.set_mode(Mode::CreatorUp).await?;
        let returned = actor.query_params(&batch_sql, &binds).await?;
        let n = returned.len() as u64;
        // The new high-water mark = the NATURAL-order max of the touched cursors.
        // Compute it in Rust over the returned cursor cells (typed by affinity), so
        // a numeric cursor is not lexically mis-maxed.
        let max_cursor = max_returned_cursor(&returned, cursor_numeric);

        if n > 0 {
            // 3. Advance progress IN THE SAME TRANSACTION (both-or-neither),
            //    under EngineJournal.
            actor.set_mode(Mode::EngineJournal).await?;
            let mc_lit = match &max_cursor {
                Some(s) => sql_lit(s),
                None => "NULL".to_string(),
            };
            // `updated_at` is NOT refreshed here: `CURRENT_TIMESTAMP` in an UPDATE
            // SET position fires `SQLITE_FUNCTION("CURRENT_TIMESTAMP")`, which the
            // hardened authorizer denies (it is allow-listed only as a column DEFAULT
            // keyword, never as a callable function — and we do NOT widen the
            // function allow-list for an observability-only timestamp). The
            // INSERT-time default stamps `updated_at`; per-batch progress carries
            // `last_cursor`/`rows_done`/`batches_done`, the resume-critical columns.
            actor
                .exec(&format!(
                    "UPDATE \"_mig\".schema_backfills \
                        SET last_cursor = {mc_lit}, \
                            rows_done = rows_done + {n}, \
                            batches_done = batches_done + 1 \
                      WHERE backfill_id = {}",
                    sql_lit(backfill_id)
                ))
                .await?;
        }
        Ok::<(u64, Option<String>), SqliteActorError>((n, max_cursor))
    }
    .await;

    match result {
        Ok((n, max_cursor)) => {
            actor.set_mode(Mode::EngineJournal).await.map_err(batch_infra_err)?;
            actor.exec("COMMIT").await.map_err(batch_infra_err)?;
            Ok((n, max_cursor))
        }
        Err(e) => {
            actor.set_mode(Mode::EngineJournal).await.map_err(batch_infra_err)?;
            let rb = actor.exec("ROLLBACK").await;
            // H1 — confirm the connection is back in autocommit; a wedged
            // connection is a hard infra error, not a silent reuse.
            match actor.is_autocommit().await {
                Ok(true) => Err(BackfillError::SqliteBatchFailed {
                    at_cursor: last_cursor.map(str::to_string),
                    source_msg: e.to_string(),
                }),
                Ok(false) => Err(BackfillError::SqlitePoisoned(format!(
                    "transaction still open after ROLLBACK (rollback result: {rb:?}); \
                     original batch error: {e}"
                ))),
                Err(probe) => Err(BackfillError::SqlitePoisoned(format!(
                    "could not confirm autocommit after ROLLBACK: {probe}; \
                     original batch error: {e}"
                ))),
            }
        }
    }
}

/// The NATURAL-order max of the RETURNING'd cursor cells. For a numeric-affinity
/// cursor, take the numeric max; for a textual cursor, the lexical max. NULLs are
/// impossible (the cursor is NOT NULL).
///
/// The numeric branch parses each cell as `f64` (which subsumes integers AND the
/// non-integral REAL keys / scientific notation `run_query_params` emits via
/// `Real(f).to_string()`), so a touched cursor is NEVER silently dropped — the
/// symmetric peer of the BIND side's text fallback for a non-i64 REAL cursor. A
/// cell that is somehow non-numeric (should not occur for a numeric-affinity
/// column) falls through to a lexical comparison rather than vanishing, so the
/// high-water mark always reflects an actually-touched row: dropping it would write
/// `last_cursor=NULL` and re-scan from the start, breaking exactly-once.
fn max_returned_cursor(rows: &[Vec<Option<String>>], numeric: bool) -> Option<String> {
    let cells = rows.iter().filter_map(|r| r.first().and_then(|c| c.clone()));
    if numeric {
        // Keyed by f64 when parseable (the natural numeric order), with a lexical
        // tiebreak/fallback so a non-numeric cell is still considered (never dropped).
        cells
            .max_by(|a, b| {
                match (a.parse::<f64>(), b.parse::<f64>()) {
                    (Ok(fa), Ok(fb)) => {
                        fa.partial_cmp(&fb).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.cmp(b))
                    }
                    // A numeric cell always outranks a non-numeric one (keep the real
                    // high-water mark); two non-numerics compare lexically.
                    (Ok(_), Err(_)) => std::cmp::Ordering::Greater,
                    (Err(_), Ok(_)) => std::cmp::Ordering::Less,
                    (Err(_), Err(_)) => a.cmp(b),
                }
            })
    } else {
        cells.max()
    }
}

/// A BEGIN/COMMIT/mode-flip infrastructure failure around a batch (not the batch
/// UPDATE itself). Surfaced as a poisoned-connection error.
fn batch_infra_err(e: SqliteActorError) -> BackfillError {
    BackfillError::SqlitePoisoned(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_rules() {
        assert!(is_numeric_affinity("INTEGER"));
        assert!(is_numeric_affinity("BIGINT"));
        assert!(is_numeric_affinity("REAL"));
        assert!(is_numeric_affinity("NUMERIC"));
        assert!(!is_numeric_affinity("TEXT"));
        assert!(!is_numeric_affinity("VARCHAR(20)"));
        assert!(!is_numeric_affinity("")); // BLOB affinity → text bind
    }

    #[test]
    fn batch_sql_first_vs_resume_shape() {
        let first = build_batch_sql("\"t\"", "\"id\"", "\"a\" = 1", None, false);
        assert!(first.contains("WHERE 1=1 ORDER BY \"id\" ASC LIMIT ?1"), "{first}");
        assert!(first.ends_with("RETURNING \"id\""));
        let resume = build_batch_sql("\"t\"", "\"id\"", "\"a\" = 1", Some("\"a\" IS NULL"), true);
        assert!(resume.contains("WHERE \"id\" > ?1 AND (\"a\" IS NULL)"), "{resume}");
        assert!(resume.contains("LIMIT ?2"), "{resume}");
    }

    #[test]
    fn cursor_mutation_detected() {
        // mutating the cursor at the head of the SET clause
        assert!(assert_cursor_not_mutated("\"id\" = 5", "id").is_err());
        // mutating it in a later assignment
        assert!(assert_cursor_not_mutated("\"a\" = 1, \"id\" = 5", "id").is_err());
        // a column ref to the cursor on the RHS is NOT a mutation
        assert!(assert_cursor_not_mutated("\"a\" = (\"id\" + 1)", "id").is_ok());
    }

    /// LOW (PR6b code-critic): a SAFE backfill whose STRING LITERAL happens to embed
    /// the `, "<cursor>" =` byte sequence must NOT be a false-positive mutation
    /// reject — the literal is RHS data, not an assignment LHS. The scan now skips
    /// over single-quoted string literals, so the needle inside a literal is ignored.
    #[test]
    fn cursor_mutation_ignores_string_literal_needle() {
        // a non-mutating assignment whose literal contains `, "id" =`
        assert!(
            assert_cursor_not_mutated("\"a\" = ', \"id\" = x'", "id").is_ok(),
            "a string literal containing the needle is not a cursor mutation"
        );
        // leading-position literal that LOOKS like a cursor assignment but is RHS data
        assert!(
            assert_cursor_not_mutated("\"note\" = '\"id\" = 9'", "id").is_ok(),
            "a quoted-ident-looking string literal on the RHS is not a mutation"
        );
        // a `''`-escaped quote inside the literal does not desync the scanner
        assert!(
            assert_cursor_not_mutated("\"a\" = 'it''s, \"id\" = 1', \"b\" = 2", "id").is_ok(),
            "an escaped quote inside the literal is handled"
        );
        // …but a REAL cursor mutation AFTER a literal is still caught.
        assert!(
            assert_cursor_not_mutated("\"a\" = 'x', \"id\" = 5", "id").is_err(),
            "a genuine later-position cursor mutation is still detected"
        );
    }

    #[test]
    fn max_returned_numeric_vs_text() {
        let rows = vec![
            vec![Some("9".to_string())],
            vec![Some("100".to_string())],
            vec![Some("11".to_string())],
        ];
        assert_eq!(max_returned_cursor(&rows, true).as_deref(), Some("100"));
        // lexical max of the same cells is "9"
        assert_eq!(max_returned_cursor(&rows, false).as_deref(), Some("9"));
    }

    /// MED (PR6b code-critic): a numeric-affinity REAL cursor — NON-integral cells
    /// like "1.5" — must NOT be silently dropped (the old `parse::<i64>()` returned
    /// None for ALL of them, yielding a NULL high-water mark and a re-scan loop).
    /// f64 parse takes the natural numeric max; the touched cursor is preserved.
    #[test]
    fn max_returned_real_cursor_not_dropped() {
        let rows = vec![
            vec![Some("1.5".to_string())],
            vec![Some("10.5".to_string())], // numeric max; lexical max would be "9.5"
            vec![Some("9.5".to_string())],
        ];
        assert_eq!(
            max_returned_cursor(&rows, true).as_deref(),
            Some("10.5"),
            "REAL cells must be numerically maxed, never dropped"
        );
        // scientific notation (as run_query_params' Real(f).to_string() can emit).
        let sci = vec![
            vec![Some("1e2".to_string())], // 100.0 — the numeric max
            vec![Some("9.5".to_string())],
        ];
        assert_eq!(max_returned_cursor(&sci, true).as_deref(), Some("1e2"));
    }

    /// A mix of integral and non-integral numeric cells maxes correctly (the bind
    /// side already coerces via affinity; the max side now matches).
    #[test]
    fn max_returned_mixed_int_and_real() {
        let rows = vec![
            vec![Some("3".to_string())],
            vec![Some("2.5".to_string())],
            vec![Some("3.5".to_string())], // numeric max
        ];
        assert_eq!(max_returned_cursor(&rows, true).as_deref(), Some("3.5"));
    }
}
