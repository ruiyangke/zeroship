//! The SQLite **batched / resumable backfill executor** — the
//! SQLite analog of the Postgres `backfill.rs` (`run_backfill`), completing the
//! "one script, both backends, DDL+DML" headline.
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
//! BEGIN IMMEDIATE; -- engine mode owns the txn boundary
//! (CreatorUp) UPDATE "<table>" SET <authored set>
//! WHERE <cursor> IN (SELECT <cursor> FROM "<table>"
//! WHERE <cursor_col> > ?1 [AND (<filter>)] -- ?1 omitted on batch 1
//! ORDER BY <cursor_col> ASC LIMIT ?n
//!)
//! RETURNING <cursor_col>; -- touched cursors
//! (EngineJournal) UPDATE "_mig".schema_backfills SET last_cursor=…, rows_done+=…, …;
//! COMMIT; -- batch mutation + progress advance commit together
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
//! [`MigrationActor::query_params`], binding the cursor + limit through the SAME
//! native `?n` protocol (`crate::render::dml::sqlite_placeholder` /
//! [`SqliteBind`](super::actor::SqliteBind)) the one-shot DML assembler uses — the
//! two never fork a divergent `?n`-binding copy.
//! - The authored `set` / `filter` SQL strings come from
//! [`crate::render::dml::assemble_backfill_clauses`] (the SAME assembler the PG path
//! uses), which renders the closed-AST transform — including the
//! `c.fn.splitPart` lowering — to inline SQL, `''`-escaping every string literal.
//! The whole assembled statement runs under the hardened authorizer (the SQLite
//! analog of the PG guard), so a hostile literal cannot alter statement shape.
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

use crate::apply::backend::{BackfillError, BackfillOutcome, BackfillProgressEntry};
use crate::model::backfill::{generate_per_row_value, BackfillSpec};
use crate::model::ir::PerRowGenerator;
use crate::model::migration::{Checksum, MigrationId};
use crate::render::dml::sqlite_placeholder;

use super::actor::{MigrationActor, SqliteActorError};
use super::authorizer::Mode;
use super::journal_sql::sql_lit;

#[derive(Clone, Copy)]
pub(crate) struct PlanBackfillIdentity<'a> {
    pub(crate) version: &'a MigrationId,
    pub(crate) checksum: &'a Checksum,
}

/// Validate a bare SQL identifier (non-empty, `[A-Za-z_][A-Za-z0-9_]*`). Rejects
/// schema-qualified / quoted-injection / whitespace, so the value is safe to
/// double-quote into an identifier position. Mirrors the PG executor's
/// `validate_ident`.
fn validate_ident(what: &'static str, value: &str) -> Result<(), BackfillError> {
    let ok = !value.is_empty()
        && value.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(BackfillError::InvalidIdentifier {
            what,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Double-quote a validated identifier (`"` → `""` — belt-and-suspenders; the value
/// already passed [`validate_ident`] so it has no `"`).
fn quote_ident(ident: &str) -> String {
    crate::render::dml::escape_quote_ident(ident)
}

fn validate_per_row_spec(spec: &BackfillSpec, set_clause: &str) -> Result<(), BackfillError> {
    if set_clause.trim().is_empty() && spec.per_row.is_empty() {
        return Err(BackfillError::InvalidSpec(
            "backfill set must not be empty".to_string(),
        ));
    }
    for (column, assignment) in &spec.per_row {
        let generator = assignment.generator();
        validate_ident("per-row destination column", column)?;
        if column.eq_ignore_ascii_case(&spec.cursor_column) {
            return Err(BackfillError::CursorColumnMutated {
                cursor_column: spec.cursor_column.clone(),
            });
        }
        if !assignment.matches_target(&spec.schema, &spec.table, column) {
            return Err(BackfillError::InvalidSpec(format!(
                "per-row assignment for destination {column:?} was validated for a different target; regenerate the plan from the declared schema"
            )));
        }
        if let PerRowGenerator::TypeId { prefix } = generator {
            crate::model::ir::validate_type_id_prefix(prefix).map_err(|error| {
                BackfillError::InvalidSpec(format!(
                    "invalid TypeID prefix for per-row destination {column:?}: {error}"
                ))
            })?;
        }
    }
    Ok(())
}

/// The two SQLite cursor domains whose values can be checkpointed and rebound
/// without changing their ordering or representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorKind {
    Integer,
    Text,
}

impl CursorKind {
    const fn storage_class(self) -> &'static str {
        match self {
            Self::Integer => "integer",
            Self::Text => "text",
        }
    }
}

/// Resolved facts about the cursor column from `PRAGMA table_info` (an engine-only
/// pragma): whether it is the table's single-column PRIMARY KEY, whether it is
/// `NOT NULL`, and its safe declared-type affinity.
struct CursorInfo {
    /// `true` iff the cursor column is the sole `PRIMARY KEY` column.
    is_single_pk: bool,
    /// `true` iff the column is declared `NOT NULL` (a PK column is implicitly so).
    not_null: bool,
    /// The safe cursor domain. `None` rejects BLOB/no-affinity, REAL, and NUMERIC
    /// declarations because their values cannot be round-tripped through the text
    /// progress checkpoint without type or ordering ambiguity.
    kind: Option<CursorKind>,
    /// `true` iff the column exists at all.
    exists: bool,
}

/// Resolve only the SQLite affinities that have an exact, stable checkpoint
/// representation. The remaining affinity classes are deliberately unsupported:
/// BLOB/no-affinity is not text-safe, and REAL/NUMERIC can mix integer, real, and
/// text storage classes while still satisfying a declaration.
fn safe_cursor_kind(decl_type: &str) -> Option<CursorKind> {
    let t = decl_type.to_ascii_uppercase();
    if t.contains("INT") {
        return Some(CursorKind::Integer);
    }
    if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
        return Some(CursorKind::Text);
    }
    None
}

/// Resolve the cursor column via `PRAGMA table_info("<table>")` (engine mode). The
/// rowid alias columns (`rowid`/`oid`/`_rowid_`) do not appear in `table_info`.
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
        kind: None,
        exists: false,
    };
    let mut pk_cols = 0usize;
    let mut this_pk = false;
    for r in &rows {
        let pk: i64 = r
            .get(5)
            .and_then(|c| c.clone())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if pk > 0 {
            pk_cols += 1;
        }
        let name = r.get(1).and_then(|c| c.clone()).unwrap_or_default();
        if name == spec.cursor_column {
            info.exists = true;
            let decl = r.get(2).and_then(|c| c.clone()).unwrap_or_default();
            info.kind = safe_cursor_kind(&decl);
            let nn: i64 = r
                .get(3)
                .and_then(|c| c.clone())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            // Ordinary SQLite rowid tables allow NULL in a non-INTEGER PRIMARY
            // KEY unless it is explicitly NOT NULL. INTEGER PRIMARY KEY is the
            // rowid alias and is inherently non-null. WITHOUT ROWID table_info
            // reports PK columns as not-null, so it follows the first arm.
            info.not_null = nn != 0 || (pk > 0 && info.kind == Some(CursorKind::Integer));
            this_pk = pk > 0;
        }
    }
    info.is_single_pk = this_pk && pk_cols == 1;
    Ok(info)
}

/// Verify the live cursor values use exactly the storage class implied by the
/// safe declaration. SQLite's dynamic typing permits a BLOB or REAL value in many
/// declared columns; stringifying such a value into `last_cursor` would change the
/// next batch's comparison semantics. A mixed or unsupported runtime domain is a
/// hard preflight failure before any data mutation.
async fn validate_cursor_storage_classes(
    actor: &MigrationActor,
    spec: &BackfillSpec,
    kind: CursorKind,
) -> Result<(), BackfillError> {
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(sqlite_journal_err)?;
    let table_q = quote_ident(&spec.table);
    let cursor_q = quote_ident(&spec.cursor_column);
    let classes = actor
        .query(&format!(
            "SELECT DISTINCT typeof({cursor_q}) FROM {table_q}"
        ))
        .await
        .map_err(sqlite_journal_err)?;
    let expected = kind.storage_class();
    if classes.iter().any(|row| {
        row.first()
            .and_then(|cell| cell.as_deref())
            .is_none_or(|actual| actual != expected)
    }) {
        return Err(BackfillError::CursorNotUniqueNotNull {
            table: spec.table.clone(),
            cursor_column: spec.cursor_column.clone(),
            reason: "stored values are not a uniform INTEGER or TEXT domain matching the declared cursor type",
        });
    }
    if matches!(kind, CursorKind::Text) {
        actor
            .validate_text_utf8(&format!("SELECT {cursor_q} FROM {table_q}"))
            .await
            .map_err(sqlite_journal_err)?;
    }
    Ok(())
}

/// SQLite triggers can suppress an UPDATE with `RAISE(IGNORE)` or mutate other
/// tables. Either behavior breaks the claim that the selected batch, the data
/// change, and the durable cursor describe the same rows. Reject every trigger
/// on the target table. The check is repeated inside each batch transaction so a
/// trigger created after the initial preflight cannot race the mutation.
async fn ensure_no_target_triggers(
    actor: &MigrationActor,
    table: &str,
) -> Result<(), SqliteActorError> {
    use super::actor::SqliteBind;

    actor.set_mode(Mode::EngineJournal).await?;
    let rows = actor
        .query_params(
            "SELECT name FROM main.sqlite_master \
              WHERE type = 'trigger' AND tbl_name = ?1 \
              ORDER BY name LIMIT 1",
            &[SqliteBind::Text(table.to_string())],
        )
        .await?;
    if let Some(name) = rows
        .first()
        .and_then(|row| row.first())
        .and_then(Clone::clone)
    {
        return Err(SqliteActorError::Exec(format!(
            "sqlite backfill target table {table:?} has trigger {name:?}; trigger side effects and suppressed rows cannot be checkpointed safely"
        )));
    }
    Ok(())
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
                checksum       TEXT, \
                name           TEXT NOT NULL, \
                target_table   TEXT NOT NULL, \
                cursor_column  TEXT NOT NULL, \
                last_cursor    TEXT, \
                end_cursor     TEXT, \
                cohort_initialized INTEGER NOT NULL DEFAULT 0, \
                rows_done      INTEGER NOT NULL DEFAULT 0, \
                batches_done   INTEGER NOT NULL DEFAULT 0, \
                complete       INTEGER NOT NULL DEFAULT 0, \
                applied_by     TEXT NOT NULL, \
                started_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                updated_at     TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)",
        )
        .await?;
    let columns = actor
        .query("PRAGMA \"_mig\".table_info(schema_backfills)")
        .await?;
    for (column, definition) in [
        ("checksum", "checksum TEXT"),
        ("end_cursor", "end_cursor TEXT"),
        (
            "cohort_initialized",
            "cohort_initialized INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        let exists = columns.iter().any(|row| {
            row.get(1)
                .and_then(|cell| cell.as_deref())
                .is_some_and(|name| name == column)
        });
        if !exists {
            actor
                .exec(&format!(
                    "ALTER TABLE \"_mig\".schema_backfills ADD COLUMN {definition}"
                ))
                .await?;
        }
    }
    Ok(())
}

/// Read existing progress evidence without creating or altering the table.
/// Status uses this before any backfill may have run, so absence is normal. A
/// legacy table without `checksum` returns missing checksums for fail-closed
/// reconciliation.
pub(crate) async fn read_progress_entries(
    actor: &MigrationActor,
) -> Result<Vec<BackfillProgressEntry>, SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    let exists = actor
        .query(
            "SELECT 1 FROM \"_mig\".sqlite_schema \
              WHERE type = 'table' AND name = 'schema_backfills' LIMIT 1",
        )
        .await?;
    if exists.is_empty() {
        return Ok(Vec::new());
    }

    let columns = actor
        .query("PRAGMA \"_mig\".table_info(schema_backfills)")
        .await?;
    let has_checksum = columns.iter().any(|row| {
        row.get(1)
            .and_then(|cell| cell.as_deref())
            .is_some_and(|name| name == "checksum")
    });
    let checksum_expr = if has_checksum { "checksum" } else { "NULL" };
    let rows = actor
        .query(&format!(
            "SELECT backfill_id, {checksum_expr}, complete \
               FROM \"_mig\".schema_backfills"
        ))
        .await?;

    rows.into_iter()
        .map(|row| {
            let version = row.first().and_then(|cell| cell.clone()).ok_or_else(|| {
                SqliteActorError::Exec("backfill progress row has a null backfill_id".to_string())
            })?;
            let checksum = row.get(1).and_then(|cell| cell.clone());
            let complete = match row.get(2).and_then(|cell| cell.as_deref()) {
                Some("0") => false,
                Some("1") => true,
                value => {
                    return Err(SqliteActorError::Exec(format!(
                        "backfill progress row {version:?} has invalid complete value {value:?}"
                    )));
                }
            };
            Ok(BackfillProgressEntry {
                version,
                checksum,
                complete,
            })
        })
        .collect()
}

/// The committed progress of a backfill (the resume anchor). `None` until the
/// progress row is inserted.
struct Progress {
    last_cursor: Option<String>,
    end_cursor: Option<String>,
    cohort_initialized: bool,
    complete: bool,
    checksum: Option<String>,
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
            "SELECT last_cursor, end_cursor, cohort_initialized, complete, checksum \
               FROM \"_mig\".schema_backfills WHERE backfill_id = {}",
            sql_lit(backfill_id)
        ))
        .await?;
    match rows.into_iter().next() {
        None => Ok(Progress {
            last_cursor: None,
            end_cursor: None,
            cohort_initialized: false,
            complete: false,
            checksum: None,
            exists: false,
        }),
        Some(r) => Ok(Progress {
            last_cursor: r.first().and_then(|c| c.clone()),
            end_cursor: r.get(1).and_then(|cell| cell.clone()),
            cohort_initialized: decode_progress_bool(&r, 2, "cohort_initialized")?,
            complete: decode_progress_bool(&r, 3, "complete")?,
            checksum: r.get(4).and_then(|cell| cell.clone()),
            exists: true,
        }),
    }
}

fn decode_progress_bool(
    row: &[Option<String>],
    index: usize,
    column: &str,
) -> Result<bool, SqliteActorError> {
    match row.get(index).and_then(|cell| cell.as_deref()) {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        actual => Err(SqliteActorError::Exec(format!(
            "backfill progress has invalid {column} value {actual:?}"
        ))),
    }
}

/// The shared window predicate the per-batch `UPDATE` and the high-water-mark
/// `SELECT max(…)` BOTH page over — the SINGLE source of truth so the mutation and
/// the resume cursor never page over divergent windows. With a prior cursor the
/// window is `cursor_col > ?1 AND cursor_col <= ?2 [AND (filter)]`; on the first
/// batch it is `cursor_col <= ?1 [AND (filter)]`. The upper bound is the terminal
/// key captured once at cohort initialization, so a resume never expands to rows
/// appended after the backfill started. `''`-escaped filter SQL comes from the
/// shared assembler.
fn window_predicate(cursor_q: &str, filter_sql: &str, have_cursor: bool) -> String {
    if have_cursor {
        let cursor_ph = sqlite_placeholder(1);
        let end_ph = sqlite_placeholder(2);
        format!("{cursor_q} > {cursor_ph} AND {cursor_q} <= {end_ph}{filter_sql}")
    } else {
        let end_ph = sqlite_placeholder(1);
        format!("{cursor_q} <= {end_ph}{filter_sql}")
    }
}

/// Build the per-batch `UPDATE … RETURNING` statement. `have_cursor` selects the
/// shape: with a prior cursor the lower/upper bounds bind at `?1`/`?2` and the
/// limit at `?3`; on the first batch the upper bound/limit bind at `?1`/`?2`.
/// The authored `set` / `filter` are inline SQL strings from the
/// shared assembler (`assemble_backfill_clauses`), `''`-escaped; the cursor + limit
/// are NATIVE `?n` binds (`sqlite_placeholder`). RETURNING the cursor column yields
/// the touched cursors so the loop derives a count (and a non-empty signal); the
/// next window's lower bound is computed by [`build_window_max_sql`] in SQL under the
/// column collation (see its docs for why the Rust-side max is collation-unsafe).
fn build_batch_sql(
    table_q: &str,
    cursor_q: &str,
    set_clause: &str,
    filter: Option<&str>,
    have_cursor: bool,
) -> String {
    let filter_sql = filter.map(|f| format!(" AND ({f})")).unwrap_or_default();
    let limit_ph = if have_cursor {
        sqlite_placeholder(3)
    } else {
        sqlite_placeholder(2)
    };
    let pred = window_predicate(cursor_q, &filter_sql, have_cursor);
    format!(
        "UPDATE {table_q} SET {set_clause} \
         WHERE {cursor_q} IN ( \
            SELECT {cursor_q} FROM {table_q} \
             WHERE {pred} \
             ORDER BY {cursor_q} ASC LIMIT {limit_ph} \
         ) RETURNING {cursor_q}"
    )
}

fn build_per_row_window_sql(
    table_q: &str,
    cursor_q: &str,
    filter: Option<&str>,
    have_cursor: bool,
) -> String {
    let filter_sql = filter.map(|f| format!(" AND ({f})")).unwrap_or_default();
    let limit_ph = if have_cursor {
        sqlite_placeholder(3)
    } else {
        sqlite_placeholder(2)
    };
    let pred = window_predicate(cursor_q, &filter_sql, have_cursor);
    format!(
        "SELECT {cursor_q}, typeof({cursor_q}) FROM {table_q} \
         WHERE {pred} ORDER BY {cursor_q} ASC LIMIT {limit_ph}"
    )
}

fn build_per_row_update_sql(
    table_q: &str,
    cursor_q: &str,
    set_clause: &str,
    spec: &BackfillSpec,
) -> String {
    let mut assignments = Vec::with_capacity(spec.per_row.len() + 1);
    if !set_clause.trim().is_empty() {
        assignments.push(set_clause.to_string());
    }
    for (index, column) in spec.per_row.keys().enumerate() {
        assignments.push(format!(
            "{} = {}",
            quote_ident(column),
            sqlite_placeholder(index + 1)
        ));
    }
    let cursor_placeholder = sqlite_placeholder(spec.per_row.len() + 1);
    format!(
        "UPDATE {table_q} SET {} WHERE {cursor_q} = {cursor_placeholder} \
         RETURNING {cursor_q}",
        assignments.join(", ")
    )
}

/// Build the high-water-mark statement: `SELECT max(<cursor>), min(typeof(...)),
/// max(typeof(...)), count(*) FROM (<the SAME window the UPDATE pages>)`. This is the
/// SQLite analog of the PG executor's
/// `(SELECT max(_bf_key)::text FROM _bf_window)` (backfill.rs) — the resume cursor
/// is computed in SQL under the cursor COLUMN's declared collation, exactly as the
/// `ORDER BY <cursor> ASC` / `<cursor> > ?1` paging does. A Rust-side `cells.max`
/// would use BINARY (byte) ordering, which for a non-BINARY-collated TEXT cursor
/// (e.g. `COLLATE NOCASE`) can differ from the column's collation-max of the touched
/// window — so the next `cursor > last_cursor` (collation-compared) would re-include
/// or skip rows, breaking the headline exactly-once guarantee. Because the cursor
/// column is never mutated (Gate 3: `assert_cursor_not_mutated`) and runs on the
/// single exclusive migration connection inside the batch's `BEGIN IMMEDIATE`, this
/// SELECT and the UPDATE page the identical pre-mutation window. The cursor + limit
/// bind through the SAME native `?n` slots as the UPDATE (`have_cursor` selects the
/// `?1`/`?2`/`?3` shape), so the two never fork a divergent bind protocol.
fn build_window_max_sql(
    table_q: &str,
    cursor_q: &str,
    filter: Option<&str>,
    have_cursor: bool,
) -> String {
    let filter_sql = filter.map(|f| format!(" AND ({f})")).unwrap_or_default();
    let limit_ph = if have_cursor {
        sqlite_placeholder(3)
    } else {
        sqlite_placeholder(2)
    };
    let pred = window_predicate(cursor_q, &filter_sql, have_cursor);
    format!(
        "SELECT max({cursor_q}), min(typeof({cursor_q})), max(typeof({cursor_q})), count(*) FROM ( \
            SELECT {cursor_q} FROM {table_q} \
             WHERE {pred} \
             ORDER BY {cursor_q} ASC LIMIT {limit_ph} \
         )"
    )
}

/// Capture the terminal key of the initial filtered cohort. `ORDER BY ... DESC
/// LIMIT 1` uses the cursor column's own collation and returns no row for an empty
/// cohort, unlike an open-ended `max()` queried afresh on every resume.
fn build_end_cursor_sql(table_q: &str, cursor_q: &str, filter: Option<&str>) -> String {
    let filter_sql = filter.map(|f| format!(" AND ({f})")).unwrap_or_default();
    format!(
        "SELECT {cursor_q}, typeof({cursor_q}) FROM {table_q} \
          WHERE 1=1{filter_sql} \
          ORDER BY {cursor_q} DESC LIMIT 1"
    )
}

/// Run (or resume) a SQLite batched backfill, the SQLite analog of
/// the PG backfill runner. Pages `spec.table` by `spec.cursor_column` in
/// `spec.batch_size` chunks, each its own committed transaction, resumable from the
/// committed progress cursor. `max_batches` bounds the run (`None` = run to
/// completion) — the checkpoint/crash-fuzz seam.
///
/// # Errors
/// [`BackfillError`] on a malformed spec, an unsafe cursor column (not a
/// single-column PRIMARY KEY, nullable, or outside the exact INTEGER/TEXT domains),
/// a cursor-column mutation, a batch failure
/// (rolled back, resumable), or an infrastructure failure.
pub(crate) async fn run_backfill_bounded(
    actor: &MigrationActor,
    spec: &BackfillSpec,
    set_clause: &str,
    filter: Option<&str>,
    applied_by: &str,
    max_batches: Option<u64>,
    identity: Option<PlanBackfillIdentity<'_>>,
) -> Result<BackfillOutcome, BackfillError> {
    // Gate 1 — identifier + batch-size validation, BEFORE any SQL is assembled.
    validate_ident("table", &spec.table)?;
    validate_ident("cursor_column", &spec.cursor_column)?;
    if spec.batch_size == 0 {
        return Err(BackfillError::InvalidBatchSize);
    }
    validate_per_row_spec(spec, set_clause)?;

    // Gate 2: resolve the cursor column. It MUST exist, be the table's
    // single-column PRIMARY KEY, and be NOT NULL. SQLite UNIQUE indexes are not a
    // substitute: dynamic typing/collations and nullable legacy primary-key rules
    // make the arbitrary-index route unsafe for durable cursor checkpoints.
    let info = resolve_cursor_info(actor, spec)
        .await
        .map_err(sqlite_journal_err)?;
    if !info.exists {
        return Err(BackfillError::TargetNotFound(format!(
            "{} column {} not found",
            spec.table, spec.cursor_column
        )));
    }
    if !info.is_single_pk {
        return Err(BackfillError::CursorNotUniqueNotNull {
            table: spec.table.clone(),
            cursor_column: spec.cursor_column.clone(),
            reason: "it is not the table's single-column PRIMARY KEY",
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
    let Some(cursor_kind) = info.kind else {
        return Err(BackfillError::CursorNotUniqueNotNull {
            table: spec.table.clone(),
            cursor_column: spec.cursor_column.clone(),
            reason: "its declared type is not an exact INTEGER or TEXT cursor domain",
        });
    };

    // Gate 3: SQLite declarations are affinities, not runtime type guarantees.
    // Reject a BLOB/REAL/NULL/mixed live domain before creating progress or
    // mutating application data.
    validate_cursor_storage_classes(actor, spec, cursor_kind).await?;

    // Gate 4: target triggers can suppress selected updates or create side effects
    // that are not represented by the progress row. Reject them before progress
    // is created; every batch repeats this check under BEGIN IMMEDIATE.
    ensure_no_target_triggers(actor, &spec.table)
        .await
        .map_err(sqlite_journal_err)?;

    // Gate 5: the authored transform MUST NOT assign the cursor column itself
    // (mutating the paged key breaks the cursor → re-processing / loop /
    // double-apply). A structural pre-flight scan of the assembled SET assignments:
    // each assignment is rendered `"col" = …`, so a leading `"<cursor>" =` token at
    // an assignment boundary is the mutation. We scan the comma-separated SET list.
    assert_cursor_not_mutated(set_clause, &spec.cursor_column)?;

    let backfill_id = identity.map_or_else(
        || spec.backfill_id(),
        |identity| identity.version.as_str().to_string(),
    );
    let expected_checksum = identity.map(|identity| identity.checksum.as_str());
    ensure_backfill_progress(actor)
        .await
        .map_err(sqlite_journal_err)?;

    // Resume from the last committed cursor (if any).
    let existing = read_progress(actor, &backfill_id)
        .await
        .map_err(sqlite_journal_err)?;
    if let Some(identity) = identity {
        if existing.exists {
            let recorded = existing.checksum.as_deref().unwrap_or("<missing>");
            if recorded != identity.checksum.as_str() {
                return Err(BackfillError::ChecksumDrift {
                    version: identity.version.as_str().to_string(),
                    recorded: recorded.to_string(),
                    expected: identity.checksum.as_str().to_string(),
                });
            }
        }
    }
    let resumed = existing.exists && existing.last_cursor.is_some();
    if existing.complete {
        // Already complete — idempotent no-op re-run.
        if let Some(identity) = identity {
            finish_plan_backfill(actor, &backfill_id, identity, spec, applied_by)
                .await
                .map_err(sqlite_journal_err)?;
        }
        return Ok(BackfillOutcome {
            backfill_id,
            batches: 0,
            rows_updated: 0,
            resumed,
            complete: true,
        });
    }
    if existing.exists && !existing.cohort_initialized {
        return Err(sqlite_journal_err(SqliteActorError::Exec(format!(
            "incomplete legacy backfill progress for {backfill_id:?} has no terminal cohort boundary; refusing an unsafe resume"
        ))));
    }
    if existing.cohort_initialized
        && existing.end_cursor.is_none()
        && existing.last_cursor.is_some()
    {
        return Err(sqlite_journal_err(SqliteActorError::Exec(format!(
            "backfill progress for {backfill_id:?} records a cursor for an empty cohort"
        ))));
    }
    let mut last_cursor: Option<String> = existing.last_cursor.clone();
    let end_cursor = if existing.exists {
        existing.end_cursor.clone()
    } else {
        initialize_progress_row(
            actor,
            &backfill_id,
            identity.map(|value| value.checksum),
            spec,
            &table_q,
            &cursor_q,
            cursor_kind,
            filter,
            applied_by,
        )
        .await
        .map_err(sqlite_journal_err)?
    };

    let mut batches: u64 = 0;
    let mut rows_updated: u64 = 0;
    let mut tail_reached = false;

    if let Some(end_cursor) = end_cursor.as_deref() {
        loop {
            if max_batches.is_some_and(|m| batches >= m) {
                break; // bound hit before the tail; leave NOT complete and resumable.
            }
            let (n, max_cursor) = run_one_batch(
                actor,
                &backfill_id,
                expected_checksum,
                spec,
                &table_q,
                &cursor_q,
                set_clause,
                filter,
                cursor_kind,
                last_cursor.as_deref(),
                end_cursor,
            )
            .await?;
            if n == 0 {
                tail_reached = true;
                break;
            }
            batches += 1;
            rows_updated += n;
            last_cursor = max_cursor;
            // Fault seam (test-only): a simulated crash BETWEEN batches. The last
            // batch's UPDATE + cursor advance already COMMITted, but the backfill is NOT
            // marked complete, so a resume reads the committed cursor and finishes the
            // tail (the resumability invariant). Identical seam to the PG executor.
            if let Err(e) = crate::fault::trip(crate::fault::points::BACKFILL_MID_BATCHES) {
                return Err(BackfillError::Fault(e.to_string()));
            }
            if n < u64::from(spec.batch_size) {
                tail_reached = true;
                break;
            }
        }
    } else {
        tail_reached = true;
    }
    if tail_reached {
        if let Some(identity) = identity {
            finish_plan_backfill(actor, &backfill_id, identity, spec, applied_by)
                .await
                .map_err(sqlite_journal_err)?;
        } else {
            mark_complete(actor, &backfill_id, spec)
                .await
                .map_err(sqlite_journal_err)?;
        }
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
                // whitespace does not end an assignment boundary (allows `, "id" =`).
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
    BackfillError::Journal(crate::apply::journal::JournalError::Backend(e.to_string()))
}

/// Capture the terminal key and insert the fresh progress row in one transaction.
/// A crash commits both facts or neither, so no retry can mistake a partially
/// initialized legacy row for a bounded cohort.
#[allow(clippy::too_many_arguments)]
async fn initialize_progress_row(
    actor: &MigrationActor,
    backfill_id: &str,
    checksum: Option<&Checksum>,
    spec: &BackfillSpec,
    table_q: &str,
    cursor_q: &str,
    cursor_kind: CursorKind,
    filter: Option<&str>,
    applied_by: &str,
) -> Result<Option<String>, SqliteActorError> {
    use super::actor::SqliteBind;

    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;
    let result = async {
        actor.set_mode(Mode::CreatorUp).await?;
        let rows = actor
            .query(&build_end_cursor_sql(table_q, cursor_q, filter))
            .await?;
        let end_cursor = rows
            .first()
            .and_then(|row| row.first())
            .and_then(Clone::clone);
        let end_class = rows
            .first()
            .and_then(|row| row.get(1))
            .and_then(|cell| cell.as_deref());
        if end_cursor.is_some() && end_class != Some(cursor_kind.storage_class()) {
            return Err(SqliteActorError::Exec(format!(
                "backfill terminal cursor has unsafe storage class {end_class:?}; expected {:?}",
                cursor_kind.storage_class()
            )));
        }

        actor.set_mode(Mode::EngineJournal).await?;
        let checksum_bind = checksum.map_or(SqliteBind::Null, |value| {
            SqliteBind::Text(value.as_str().to_string())
        });
        let end_cursor_bind = end_cursor
            .as_ref()
            .map_or(SqliteBind::Null, |value| SqliteBind::Text(value.clone()));
        actor
            .exec_params(
                "INSERT INTO \"_mig\".schema_backfills \
                     (backfill_id, checksum, name, target_table, cursor_column, \
                      end_cursor, cohort_initialized, applied_by) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
                &[
                    SqliteBind::Text(backfill_id.to_string()),
                    checksum_bind,
                    SqliteBind::Text(spec.name.clone()),
                    SqliteBind::Text(spec.table.clone()),
                    SqliteBind::Text(spec.cursor_column.clone()),
                    end_cursor_bind,
                    SqliteBind::Text(applied_by.to_string()),
                ],
            )
            .await?;
        Ok::<Option<String>, SqliteActorError>(end_cursor)
    }
    .await;

    match result {
        Ok(end_cursor) => {
            actor
                .commit_or_cleanup("backfill cohort initialization")
                .await?;
            Ok(end_cursor)
        }
        Err(error) => Err(actor
            .cleanup_after_error("backfill cohort initialization", error)
            .await),
    }
}

/// Mark exactly one identity-matching progress row complete. SQLite reports the
/// updated row through `RETURNING`, which keeps a missing, replaced, or corrupted
/// progress row from being mistaken for successful finalization.
async fn complete_progress_row(
    actor: &MigrationActor,
    backfill_id: &str,
    expected_checksum: Option<&str>,
    spec: &BackfillSpec,
) -> Result<(), SqliteActorError> {
    use super::actor::SqliteBind;

    actor.set_mode(Mode::EngineJournal).await?;
    let completed = actor
        .query_params(
            "UPDATE \"_mig\".schema_backfills \
                SET complete = 1 \
              WHERE backfill_id = ?1 \
                AND checksum IS ?2 \
                AND target_table = ?3 \
                AND cursor_column = ?4 \
                AND cohort_initialized = 1 \
                AND complete IN (0, 1) \
            RETURNING backfill_id",
            &[
                SqliteBind::Text(backfill_id.to_string()),
                expected_checksum.map_or(SqliteBind::Null, |value| {
                    SqliteBind::Text(value.to_string())
                }),
                SqliteBind::Text(spec.table.clone()),
                SqliteBind::Text(spec.cursor_column.clone()),
            ],
        )
        .await?;
    if completed.len() != 1 {
        return Err(SqliteActorError::Exec(format!(
            "backfill completion update affected {} rows for {backfill_id:?}; expected exactly one matching progress row",
            completed.len()
        )));
    }
    Ok(())
}

/// Atomically mark a plan backfill complete and append its ordinary migration
/// journal event. A crash leaves both changes committed or neither; on retry the
/// caller can safely finish again after observing progress without a journal row.
async fn finish_plan_backfill(
    actor: &MigrationActor,
    backfill_id: &str,
    identity: PlanBackfillIdentity<'_>,
    spec: &BackfillSpec,
    applied_by: &str,
) -> Result<(), SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;
    let result = async {
        complete_progress_row(actor, backfill_id, Some(identity.checksum.as_str()), spec).await?;
        let latest = actor
            .query(&format!(
                "SELECT event_kind, checksum FROM \"_mig\".schema_migrations \
                  WHERE version = {} ORDER BY event_seq DESC LIMIT 1",
                sql_lit(identity.version.as_str())
            ))
            .await?;
        let already_matching = latest.first().is_some_and(|row| {
            row.first().and_then(|cell| cell.as_deref()) == Some("applied")
                && row.get(1).and_then(|cell| cell.as_deref()) == Some(identity.checksum.as_str())
        });
        if latest.first().is_some_and(|row| {
            row.first().and_then(|cell| cell.as_deref()) == Some("applied")
                && row.get(1).and_then(|cell| cell.as_deref()) != Some(identity.checksum.as_str())
        }) {
            let recorded = latest[0]
                .get(1)
                .and_then(|cell| cell.as_deref())
                .unwrap_or("<missing>");
            return Err(SqliteActorError::Exec(format!(
                "checksum drift while finalizing backfill {}: journal has {recorded}, plan has {}",
                identity.version.as_str(),
                identity.checksum.as_str()
            )));
        }
        if !already_matching {
            actor
                .exec(&format!(
                    "INSERT INTO \"_mig\".schema_migrations \
                        (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
                     VALUES ('applied', {}, {}, {}, {}, 'completed', 'success', 'apply')",
                    sql_lit(identity.version.as_str()),
                    sql_lit(&spec.name),
                    sql_lit(identity.checksum.as_str()),
                    sql_lit(applied_by),
                ))
                .await?;
        }
        Ok::<(), SqliteActorError>(())
    }
    .await;

    match result {
        Ok(()) => actor.commit_or_cleanup("backfill finalization").await,
        Err(error) => Err(actor
            .cleanup_after_error("backfill finalization", error)
            .await),
    }
}

/// Mark a backfill complete (engine mode, its own statement).
async fn mark_complete(
    actor: &MigrationActor,
    backfill_id: &str,
    spec: &BackfillSpec,
) -> Result<(), SqliteActorError> {
    actor.set_mode(Mode::EngineJournal).await?;
    actor.exec("BEGIN IMMEDIATE").await?;
    match complete_progress_row(actor, backfill_id, None, spec).await {
        Ok(()) => actor.commit_or_cleanup("backfill completion").await,
        Err(error) => Err(actor
            .cleanup_after_error("backfill completion", error)
            .await),
    }
}

/// Re-read the resume-critical progress fields after `BEGIN IMMEDIATE` has
/// acquired SQLite's writer reservation. The values read before entering the
/// batch are only hints: another connection may have deleted or corrupted the
/// journal row in the meantime. No application mutation may run unless this
/// transaction observes exactly the row and cursor anchor the caller planned.
async fn revalidate_batch_progress(
    actor: &MigrationActor,
    backfill_id: &str,
    expected_checksum: Option<&str>,
    spec: &BackfillSpec,
    expected_last_cursor: Option<&str>,
    expected_end_cursor: &str,
) -> Result<(), SqliteActorError> {
    use super::actor::SqliteBind;

    actor.set_mode(Mode::EngineJournal).await?;
    let rows = actor
        .query_params(
            "SELECT checksum, target_table, cursor_column, last_cursor, end_cursor, \
                    cohort_initialized, complete \
               FROM \"_mig\".schema_backfills \
              WHERE backfill_id = ?1",
            &[SqliteBind::Text(backfill_id.to_string())],
        )
        .await?;
    if rows.is_empty() {
        return Err(SqliteActorError::Exec(format!(
            "backfill progress row disappeared for {backfill_id:?}"
        )));
    }
    if rows.len() != 1 {
        return Err(SqliteActorError::Exec(format!(
            "backfill progress lookup returned {} rows for {backfill_id:?}; expected exactly one",
            rows.len()
        )));
    }

    let row = &rows[0];
    let checksum = row.first().and_then(Clone::clone);
    if checksum.as_deref() != expected_checksum {
        return Err(SqliteActorError::Exec(format!(
            "backfill progress checksum changed for {backfill_id:?}: recorded {checksum:?}, expected {expected_checksum:?}"
        )));
    }

    let target_table = row.get(1).and_then(Clone::clone);
    let cursor_column = row.get(2).and_then(Clone::clone);
    if target_table.as_deref() != Some(spec.table.as_str())
        || cursor_column.as_deref() != Some(spec.cursor_column.as_str())
    {
        return Err(SqliteActorError::Exec(format!(
            "backfill progress target changed for {backfill_id:?}: recorded table {target_table:?} cursor {cursor_column:?}, expected table {:?} cursor {:?}",
            spec.table, spec.cursor_column
        )));
    }

    let cohort_initialized = decode_progress_bool(row, 5, "cohort_initialized")?;
    let complete = decode_progress_bool(row, 6, "complete")?;
    if !cohort_initialized || complete {
        return Err(SqliteActorError::Exec(format!(
            "backfill progress state changed for {backfill_id:?}: cohort_initialized={cohort_initialized}, complete={complete}"
        )));
    }

    let last_cursor = row.get(3).and_then(Clone::clone);
    let end_cursor = row.get(4).and_then(Clone::clone);
    if last_cursor.as_deref() != expected_last_cursor
        || end_cursor.as_deref() != Some(expected_end_cursor)
    {
        return Err(SqliteActorError::Exec(format!(
            "backfill progress cursor changed for {backfill_id:?}: recorded last={last_cursor:?}, end={end_cursor:?}; expected last={expected_last_cursor:?}, end={expected_end_cursor:?}"
        )));
    }
    Ok(())
}

/// Run ONE batch in its own `BEGIN IMMEDIATE … COMMIT` transaction (the
/// non-blocking, crash-safe unit). Returns `(rows_updated, new_max_cursor)`.
///
/// The batch `UPDATE … RETURNING <cursor>` runs under CreatorUp (the confined
/// mode); the progress advance runs under EngineJournal; both commit together. On
/// any failure the transaction is rolled back (progress NOT advanced) and the
/// connection's autocommit state is re-asserted (a wedged connection is a
/// hard error, not a silent reuse).
#[allow(clippy::too_many_arguments)]
async fn run_one_batch(
    actor: &MigrationActor,
    backfill_id: &str,
    expected_checksum: Option<&str>,
    spec: &BackfillSpec,
    table_q: &str,
    cursor_q: &str,
    set_clause: &str,
    filter: Option<&str>,
    cursor_kind: CursorKind,
    last_cursor: Option<&str>,
    end_cursor: &str,
) -> Result<(u64, Option<String>), BackfillError> {
    use super::actor::SqliteBind;

    let have_cursor = last_cursor.is_some();
    let batch_sql = build_batch_sql(table_q, cursor_q, set_clause, filter, have_cursor);
    let window_max_sql = build_window_max_sql(table_q, cursor_q, filter, have_cursor);
    let per_row_window_sql = build_per_row_window_sql(table_q, cursor_q, filter, have_cursor);
    let per_row_update_sql = build_per_row_update_sql(table_q, cursor_q, set_clause, spec);

    // Rebind the committed cursor in the exact validated storage domain so
    // `cursor > ?` keeps the same comparison semantics across a resume.
    let cursor_bind = |value: &str| match cursor_kind {
        CursorKind::Integer => value.parse::<i64>().map(SqliteBind::Int).map_err(|_| {
            BackfillError::SqliteBatchFailed {
                at_cursor: last_cursor.map(str::to_string),
                source_msg: format!("committed INTEGER cursor {value:?} is not an exact i64"),
            }
        }),
        CursorKind::Text => Ok(SqliteBind::Text(value.to_string())),
    };
    let mut binds: Vec<SqliteBind> = Vec::with_capacity(3);
    if let Some(lc) = last_cursor {
        binds.push(cursor_bind(lc)?);
    }
    binds.push(cursor_bind(end_cursor)?);
    binds.push(SqliteBind::Int(i64::from(spec.batch_size)));

    // 1. BEGIN IMMEDIATE under engine mode (the authorizer allows SQLITE_TRANSACTION
    // only in EngineJournal — the engine owns txn boundaries).
    actor
        .set_mode(Mode::EngineJournal)
        .await
        .map_err(batch_infra_err)?;
    actor
        .exec("BEGIN IMMEDIATE")
        .await
        .map_err(batch_infra_err)?;

    let result = async {
        revalidate_batch_progress(
            actor,
            backfill_id,
            expected_checksum,
            spec,
            last_cursor,
            end_cursor,
        )
        .await?;

        // Revalidate the target while this BEGIN IMMEDIATE holds SQLite's writer
        // reservation. A concurrent schema writer cannot add a trigger between
        // this catalog read and the UPDATE.
        ensure_no_target_triggers(actor, &spec.table).await?;

        // 2. Compute the high-water mark and exact selected-row count IN SQL, under
        // the cursor column's collation, over the SAME pre-mutation window the
        // UPDATE pages, the SQLite analog
        // of the PG `max(_bf_key)::text`. This MUST run BEFORE the UPDATE: the
        // authored transform can mutate a filter column (e.g. `done = 1` against a
        // `done = 0` filter), so post-UPDATE the window predicate would no longer
        // match the just-touched rows. The cursor column itself is never mutated
        // (Gate 3), and the single exclusive connection inside this BEGIN IMMEDIATE
        // sees a stable snapshot, so the SELECT's window == the UPDATE's window.
        // Computing the max in SQL (not Rust `cells.max`) makes the resume cursor
        // collation-consistent with the `ORDER BY <cursor>` / `<cursor> > ?1` paging
        // (a Rust BINARY max diverges for a non-BINARY-collated TEXT cursor).
        actor.set_mode(Mode::CreatorUp).await?;
        let (n, max_cursor) = if spec.per_row.is_empty() {
            let max_rows = actor.query_params(&window_max_sql, &binds).await?;
            let max_row = max_rows.first().ok_or_else(|| {
                SqliteActorError::Exec("backfill window aggregate returned no row".to_string())
            })?;
            let max_cursor = max_row.first().and_then(Clone::clone);
            let min_class = max_row.get(1).and_then(|cell| cell.as_deref());
            let max_class = max_row.get(2).and_then(|cell| cell.as_deref());
            let selected_count = max_row
                .get(3)
                .and_then(|cell| cell.as_deref())
                .ok_or_else(|| {
                    SqliteActorError::Exec(
                        "backfill window aggregate returned no selected row count".to_string(),
                    )
                })?
                .parse::<u64>()
                .map_err(|_| {
                    SqliteActorError::Exec(
                        "backfill window aggregate returned an invalid selected row count"
                            .to_string(),
                    )
                })?;
            let expected_class = cursor_kind.storage_class();
            if (min_class.is_some() || max_class.is_some())
                && (min_class != Some(expected_class) || max_class != Some(expected_class))
            {
                return Err(SqliteActorError::Exec(format!(
                    "backfill window contains cursor storage classes outside {expected_class:?}: min={min_class:?}, max={max_class:?}"
                )));
            }

            // Keep the existing set-based statement as the fast path when no
            // apply-engine generator is present.
            let returned = actor.query_params(&batch_sql, &binds).await?;
            let n = returned.len() as u64;
            if n != selected_count {
                return Err(SqliteActorError::Exec(format!(
                    "backfill window selected {selected_count} rows but updated {n}; a constraint conflict may have suppressed rows"
                )));
            }
            (n, max_cursor)
        } else {
            // Freeze the ordered key window before evaluating any generator. Each
            // key is then updated independently, and every generator call happens
            // inside this batch transaction immediately before its bound UPDATE.
            let selected_rows = actor.query_params(&per_row_window_sql, &binds).await?;
            let expected_class = cursor_kind.storage_class();
            let mut selected = Vec::with_capacity(selected_rows.len());
            for row in &selected_rows {
                let cursor = row.first().and_then(Clone::clone).ok_or_else(|| {
                    SqliteActorError::Exec(
                        "per-row backfill window returned a null cursor".to_string(),
                    )
                })?;
                let storage_class = row.get(1).and_then(|cell| cell.as_deref());
                if storage_class != Some(expected_class) {
                    return Err(SqliteActorError::Exec(format!(
                        "per-row backfill window contains cursor storage class {storage_class:?}; expected {expected_class:?}"
                    )));
                }
                selected.push(cursor);
            }
            let max_cursor = selected.last().cloned();
            for selected_cursor in &selected {
                let mut row_binds = spec
                    .per_row
                    .values()
                    .map(|assignment| generate_per_row_value(assignment.generator()))
                    .map(SqliteBind::Text)
                    .collect::<Vec<_>>();
                let selected_cursor_bind = match cursor_kind {
                    CursorKind::Integer => selected_cursor
                        .parse::<i64>()
                        .map(SqliteBind::Int)
                        .map_err(|_| {
                            SqliteActorError::Exec(format!(
                                "selected INTEGER cursor {selected_cursor:?} is not an exact i64"
                            ))
                        })?,
                    CursorKind::Text => SqliteBind::Text(selected_cursor.clone()),
                };
                row_binds.push(selected_cursor_bind);
                let returned = actor
                    .query_params(&per_row_update_sql, &row_binds)
                    .await?;
                if returned.len() != 1 {
                    return Err(SqliteActorError::Exec(format!(
                        "per-row update at cursor {selected_cursor:?} affected {} rows; expected exactly one",
                        returned.len()
                    )));
                }
            }
            (selected.len() as u64, max_cursor)
        };

        if n > 0 {
            // 4. Advance progress IN THE SAME TRANSACTION (both-or-neither),
            // under EngineJournal.
            actor.set_mode(Mode::EngineJournal).await?;
            let max_cursor_bind = max_cursor.clone().ok_or_else(|| {
                SqliteActorError::Exec(
                    "non-empty backfill window produced no maximum cursor".to_string(),
                )
            })?;
            // `updated_at` is NOT refreshed here: `CURRENT_TIMESTAMP` in an UPDATE
            // SET position fires `SQLITE_FUNCTION("CURRENT_TIMESTAMP")`, which the
            // hardened authorizer denies (it is allow-listed only as a column DEFAULT
            // keyword, never as a callable function — and we do NOT widen the
            // function allow-list for an observability-only timestamp). The
            // INSERT-time default stamps `updated_at`; per-batch progress carries
            // `last_cursor`/`rows_done`/`batches_done`, the resume-critical columns.
            let advanced = actor
                .query_params(
                    "UPDATE \"_mig\".schema_backfills \
                        SET last_cursor = ?1, \
                            rows_done = rows_done + ?2, \
                            batches_done = batches_done + 1 \
                      WHERE backfill_id = ?3 \
                        AND checksum IS ?4 \
                        AND target_table = ?5 \
                        AND cursor_column = ?6 \
                        AND last_cursor IS ?7 \
                        AND end_cursor IS ?8 \
                        AND cohort_initialized = 1 \
                        AND complete = 0 \
                    RETURNING backfill_id",
                    &[
                        SqliteBind::Text(max_cursor_bind),
                        SqliteBind::Int(i64::try_from(n).map_err(|_| {
                            SqliteActorError::Exec("backfill row count exceeds i64".to_string())
                        })?),
                        SqliteBind::Text(backfill_id.to_string()),
                        expected_checksum.map_or(SqliteBind::Null, |value| {
                            SqliteBind::Text(value.to_string())
                        }),
                        SqliteBind::Text(spec.table.clone()),
                        SqliteBind::Text(spec.cursor_column.clone()),
                        last_cursor.map_or(SqliteBind::Null, |value| {
                            SqliteBind::Text(value.to_string())
                        }),
                        SqliteBind::Text(end_cursor.to_string()),
                    ],
                )
                .await?;
            if advanced.len() != 1 {
                return Err(SqliteActorError::Exec(format!(
                    "backfill progress update affected {} rows for {backfill_id:?}; expected exactly one",
                    advanced.len()
                )));
            }
        }
        Ok::<(u64, Option<String>), SqliteActorError>((n, max_cursor))
    }
    .await;

    match result {
        Ok((n, max_cursor)) => match actor.commit_or_cleanup("backfill batch").await {
            Ok(()) => Ok((n, max_cursor)),
            Err(SqliteActorError::Poisoned(message)) => Err(BackfillError::SqlitePoisoned(message)),
            Err(error) => Err(BackfillError::SqliteBatchFailed {
                at_cursor: last_cursor.map(str::to_string),
                source_msg: error.to_string(),
            }),
        },
        Err(e) => match actor.cleanup_after_error("backfill batch", e).await {
            SqliteActorError::Poisoned(message) => Err(BackfillError::SqlitePoisoned(message)),
            error => Err(BackfillError::SqliteBatchFailed {
                at_cursor: last_cursor.map(str::to_string),
                source_msg: error.to_string(),
            }),
        },
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

    #[compio::test]
    async fn progress_reader_handles_absence_and_decodes_existing_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");

        assert!(read_progress_entries(&actor)
            .await
            .expect("absent progress table")
            .is_empty());

        ensure_backfill_progress(&actor)
            .await
            .expect("create progress table");
        actor
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        actor
            .exec(
                "INSERT INTO \"_mig\".schema_backfills \
                    (backfill_id, checksum, name, target_table, cursor_column, applied_by) \
                 VALUES ('mig_progress', 'checksum_a', 'fill users', 'users', 'id', 'tester')",
            )
            .await
            .expect("insert progress row");

        assert_eq!(
            read_progress_entries(&actor)
                .await
                .expect("read progress row"),
            vec![BackfillProgressEntry {
                version: "mig_progress".into(),
                checksum: Some("checksum_a".into()),
                complete: false,
            }]
        );
    }

    #[test]
    fn cursor_affinity_rules_fail_closed() {
        assert_eq!(safe_cursor_kind("INTEGER"), Some(CursorKind::Integer));
        assert_eq!(safe_cursor_kind("BIGINT"), Some(CursorKind::Integer));
        assert_eq!(safe_cursor_kind("TEXT"), Some(CursorKind::Text));
        assert_eq!(safe_cursor_kind("VARCHAR(20)"), Some(CursorKind::Text));
        assert_eq!(safe_cursor_kind("REAL"), None);
        assert_eq!(safe_cursor_kind("NUMERIC"), None);
        assert_eq!(safe_cursor_kind("BLOB"), None);
        assert_eq!(safe_cursor_kind(""), None);
    }

    #[test]
    fn batch_sql_first_vs_resume_shape() {
        let first = build_batch_sql("\"t\"", "\"id\"", "\"a\" = 1", None, false);
        assert!(
            first.contains("WHERE \"id\" <= ?1 ORDER BY \"id\" ASC LIMIT ?2"),
            "{first}"
        );
        assert!(first.ends_with("RETURNING \"id\""));
        assert!(first.contains("WHERE \"id\" IN"), "{first}");
        assert!(!first.contains("rowid"), "{first}");
        let resume = build_batch_sql("\"t\"", "\"id\"", "\"a\" = 1", Some("\"a\" IS NULL"), true);
        assert!(
            resume.contains("WHERE \"id\" > ?1 AND \"id\" <= ?2 AND (\"a\" IS NULL)"),
            "{resume}"
        );
        assert!(resume.contains("LIMIT ?3"), "{resume}");
    }

    #[test]
    fn per_row_sql_has_only_bound_generator_and_cursor_values() {
        let mut spec = test_spec("items", "id");
        spec.set_clause.clear();
        spec.per_row.insert(
            "generated".into(),
            crate::model::backfill::PerRowAssignment::validated(
                "main",
                "items",
                "generated",
                PerRowGenerator::Ulid,
            ),
        );

        let sql = build_per_row_update_sql("\"items\"", "\"id\"", "", &spec);
        assert_eq!(
            sql,
            "UPDATE \"items\" SET \"generated\" = ?1 WHERE \"id\" = ?2 RETURNING \"id\""
        );
        assert!(!sql.contains("01J"), "no sampled literal belongs in SQL");
    }

    #[test]
    fn per_row_assignment_cannot_be_retargeted() {
        let mut spec = test_spec("items", "id");
        spec.set_clause.clear();
        spec.per_row.insert(
            "generated".into(),
            crate::model::backfill::PerRowAssignment::validated(
                "main",
                "other_items",
                "generated",
                PerRowGenerator::Ulid,
            ),
        );
        let error = validate_per_row_spec(&spec, "").expect_err("retargeted token must fail");
        assert!(
            matches!(error, BackfillError::InvalidSpec(message) if message.contains("validated for a different target"))
        );
    }

    fn test_spec(table: &str, cursor: &str) -> BackfillSpec {
        BackfillSpec {
            schema: "main".to_string(),
            table: table.to_string(),
            cursor_column: cursor.to_string(),
            batch_size: 2,
            set_clause: "\"value\" = (\"value\" + 1)".to_string(),
            per_row: std::collections::BTreeMap::new(),
            filter: None,
            name: format!("fill_{table}"),
        }
    }

    async fn seed_batch_progress(
        actor: &MigrationActor,
        spec: &BackfillSpec,
        backfill_id: &str,
        last_cursor: Option<&str>,
        end_cursor: &str,
    ) {
        use super::super::actor::SqliteBind;

        ensure_backfill_progress(actor)
            .await
            .expect("create progress table");
        actor
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        actor
            .exec_params(
                "INSERT INTO \"_mig\".schema_backfills \
                     (backfill_id, name, target_table, cursor_column, last_cursor, \
                      end_cursor, cohort_initialized, applied_by) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, 'tester')",
                &[
                    SqliteBind::Text(backfill_id.to_string()),
                    SqliteBind::Text(spec.name.clone()),
                    SqliteBind::Text(spec.table.clone()),
                    SqliteBind::Text(spec.cursor_column.clone()),
                    last_cursor.map_or(SqliteBind::Null, |value| {
                        SqliteBind::Text(value.to_string())
                    }),
                    SqliteBind::Text(end_cursor.to_string()),
                ],
            )
            .await
            .expect("seed progress row");
    }

    async fn assert_test_values_unchanged(actor: &MigrationActor, table: &str) {
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        let rows = actor
            .query(&format!(
                "SELECT count(*) FROM {} WHERE value != 0",
                quote_ident(table)
            ))
            .await
            .expect("read target rows");
        assert_eq!(rows[0][0].as_deref(), Some("0"));
        assert!(actor.is_autocommit().await.expect("autocommit probe"));
    }

    #[compio::test]
    async fn deleted_progress_row_cannot_commit_application_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE deleted_progress (id INTEGER PRIMARY KEY, value INTEGER NOT NULL); \
                 INSERT INTO deleted_progress (id, value) VALUES (1, 0), (2, 0)",
            )
            .await
            .expect("seed target rows");

        let spec = test_spec("deleted_progress", "id");
        let backfill_id = spec.backfill_id();
        seed_batch_progress(&actor, &spec, &backfill_id, None, "2").await;
        actor
            .exec_params(
                "DELETE FROM \"_mig\".schema_backfills WHERE backfill_id = ?1",
                &[super::super::actor::SqliteBind::Text(backfill_id.clone())],
            )
            .await
            .expect("delete progress row");

        let error = run_one_batch(
            &actor,
            &backfill_id,
            None,
            &spec,
            "\"deleted_progress\"",
            "\"id\"",
            &spec.set_clause,
            None,
            CursorKind::Integer,
            None,
            "2",
        )
        .await
        .expect_err("a missing checkpoint must abort the data batch");
        assert!(
            error.to_string().contains("progress row disappeared"),
            "{error}"
        );
        assert_test_values_unchanged(&actor, &spec.table).await;
    }

    #[compio::test]
    async fn corrupted_progress_cursor_cannot_commit_application_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE corrupt_progress (id INTEGER PRIMARY KEY, value INTEGER NOT NULL); \
                 INSERT INTO corrupt_progress (id, value) VALUES (1, 0), (2, 0)",
            )
            .await
            .expect("seed target rows");

        let spec = test_spec("corrupt_progress", "id");
        let backfill_id = spec.backfill_id();
        seed_batch_progress(&actor, &spec, &backfill_id, Some("1"), "2").await;

        let error = run_one_batch(
            &actor,
            &backfill_id,
            None,
            &spec,
            "\"corrupt_progress\"",
            "\"id\"",
            &spec.set_clause,
            None,
            CursorKind::Integer,
            None,
            "2",
        )
        .await
        .expect_err("a changed resume anchor must abort the data batch");
        assert!(
            error.to_string().contains("progress cursor changed"),
            "{error}"
        );
        assert_test_values_unchanged(&actor, &spec.table).await;
    }

    #[compio::test]
    async fn suppressed_checkpoint_update_rolls_back_application_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let app_path = dir.path().join("app.sqlite");
        let journal_path = dir.path().join("journal.sqlite");
        let actor = MigrationActor::open(&app_path, &journal_path).expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE suppressed_checkpoint (id INTEGER PRIMARY KEY, value INTEGER NOT NULL); \
                 INSERT INTO suppressed_checkpoint (id, value) VALUES (1, 0), (2, 0)",
            )
            .await
            .expect("seed target rows");

        let spec = test_spec("suppressed_checkpoint", "id");
        let backfill_id = spec.backfill_id();
        seed_batch_progress(&actor, &spec, &backfill_id, None, "2").await;

        // Simulate out-of-band journal corruption that suppresses the checkpoint
        // after the progress row has been successfully revalidated. Requiring one
        // UPDATE ... RETURNING row is what turns this into a batch rollback.
        let journal = rusqlite::Connection::open(&journal_path).expect("open journal directly");
        journal
            .execute_batch(
                "CREATE TRIGGER suppress_backfill_checkpoint \
                   BEFORE UPDATE OF last_cursor ON schema_backfills \
                   BEGIN \
                     DELETE FROM schema_backfills \
                      WHERE backfill_id = old.backfill_id; \
                   END",
            )
            .expect("install checkpoint suppression trigger");
        drop(journal);

        let error = run_one_batch(
            &actor,
            &backfill_id,
            None,
            &spec,
            "\"suppressed_checkpoint\"",
            "\"id\"",
            &spec.set_clause,
            None,
            CursorKind::Integer,
            None,
            "2",
        )
        .await
        .expect_err("a suppressed checkpoint must abort the data batch");
        assert!(
            error
                .to_string()
                .contains("progress update affected 0 rows"),
            "{error}"
        );
        actor
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        let progress = actor
            .query("SELECT count(*) FROM \"_mig\".schema_backfills")
            .await
            .expect("verify progress rollback");
        assert_eq!(progress[0][0].as_deref(), Some("1"));
        assert_test_values_unchanged(&actor, &spec.table).await;
    }

    #[compio::test]
    async fn without_rowid_table_pages_by_its_validated_primary_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE wr (id INTEGER PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID; \
                 INSERT INTO wr (id, value) VALUES (1, 0), (2, 0), (3, 0)",
            )
            .await
            .expect("seed WITHOUT ROWID table");

        let spec = test_spec("wr", "id");
        let outcome =
            run_backfill_bounded(&actor, &spec, &spec.set_clause, None, "tester", None, None)
                .await
                .expect("backfill WITHOUT ROWID table");
        assert!(outcome.complete);
        assert_eq!(outcome.rows_updated, 3);

        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        let rows = actor
            .query("SELECT count(*) FROM wr WHERE value = 1")
            .await
            .expect("verify rows");
        assert_eq!(rows[0][0].as_deref(), Some("3"));
    }

    #[compio::test]
    async fn per_row_generator_is_evaluated_for_every_selected_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE generated_items (\
                    id INTEGER PRIMARY KEY, generated TEXT\
                 ); \
                 INSERT INTO generated_items (id) VALUES (1), (2), (3), (4), (5)",
            )
            .await
            .expect("seed target rows");

        let mut spec = test_spec("generated_items", "id");
        spec.batch_size = 2;
        spec.set_clause.clear();
        spec.per_row.insert(
            "generated".into(),
            crate::model::backfill::PerRowAssignment::validated(
                "main",
                "generated_items",
                "generated",
                PerRowGenerator::Ulid,
            ),
        );
        let outcome = run_backfill_bounded(&actor, &spec, "", None, "tester", None, None)
            .await
            .expect("per-row backfill");
        assert_eq!(outcome.rows_updated, 5);
        assert_eq!(outcome.batches, 3);

        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        let rows = actor
            .query("SELECT generated FROM generated_items ORDER BY id")
            .await
            .expect("read generated values");
        let values = rows
            .iter()
            .map(|row| row[0].clone().expect("generated value"))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(values.len(), 5, "one literal must never be reused");
        assert!(values.iter().all(|value| {
            value.len() == 26
                && value
                    .bytes()
                    .all(|byte| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&byte))
        }));
    }

    #[compio::test]
    async fn resumed_backfill_stops_at_its_initial_terminal_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE bounded (id INTEGER PRIMARY KEY, value INTEGER NOT NULL); \
                 INSERT INTO bounded (id, value) VALUES (1, 0), (2, 0), (3, 0)",
            )
            .await
            .expect("seed initial cohort");

        let spec = test_spec("bounded", "id");
        let first = run_backfill_bounded(
            &actor,
            &spec,
            &spec.set_clause,
            None,
            "tester",
            Some(1),
            None,
        )
        .await
        .expect("first bounded batch");
        assert_eq!(first.rows_updated, 2);
        assert!(!first.complete);

        // These rows arrive after cohort initialization. Their keys are above the
        // fixed terminal cursor and must be left for a later migration.
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec("INSERT INTO bounded (id, value) VALUES (4, 0), (5, 0)")
            .await
            .expect("append rows after first batch");

        let resumed =
            run_backfill_bounded(&actor, &spec, &spec.set_clause, None, "tester", None, None)
                .await
                .expect("resume fixed cohort");
        assert!(resumed.resumed);
        assert!(resumed.complete);
        assert_eq!(resumed.rows_updated, 1, "only original id=3 remains");

        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        let rows = actor
            .query(
                "SELECT \
                    (SELECT count(*) FROM bounded WHERE id <= 3 AND value = 1), \
                    (SELECT count(*) FROM bounded WHERE id > 3 AND value = 0)",
            )
            .await
            .expect("verify fixed cohort");
        assert_eq!(rows[0][0].as_deref(), Some("3"));
        assert_eq!(rows[0][1].as_deref(), Some("2"));
    }

    #[compio::test]
    async fn empty_cohort_is_initialized_and_completed_without_a_cursor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec("CREATE TABLE empty_items (id INTEGER PRIMARY KEY, value INTEGER NOT NULL)")
            .await
            .expect("create empty table");

        let spec = test_spec("empty_items", "id");
        let outcome =
            run_backfill_bounded(&actor, &spec, &spec.set_clause, None, "tester", None, None)
                .await
                .expect("complete empty cohort");
        assert!(outcome.complete);
        assert_eq!(outcome.rows_updated, 0);

        actor
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        let rows = actor
            .query(
                "SELECT end_cursor, cohort_initialized, complete \
                   FROM \"_mig\".schema_backfills",
            )
            .await
            .expect("read initialized progress");
        assert_eq!(rows[0][0], None);
        assert_eq!(rows[0][1].as_deref(), Some("1"));
        assert_eq!(rows[0][2].as_deref(), Some("1"));
    }

    #[compio::test]
    async fn plan_finalization_keeps_one_matching_applied_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        super::super::journal_sql::ensure_journal(&actor)
            .await
            .expect("create journal");
        ensure_backfill_progress(&actor)
            .await
            .expect("create progress");

        let version = MigrationId::derive("backfill-finalization", b"stable");
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: "complete authored plan",
            down: None,
            flags: &crate::model::migration::MigrationFlags::default(),
            owner_app: "app",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        actor
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        actor
            .exec(&format!(
                "INSERT INTO \"_mig\".schema_backfills \
                    (backfill_id, checksum, name, target_table, cursor_column, \
                     cohort_initialized, applied_by) \
                 VALUES ({}, {}, 'fill items', 'items', 'id', 1, 'tester')",
                sql_lit(version.as_str()),
                sql_lit(checksum.as_str())
            ))
            .await
            .expect("seed progress");

        let identity = PlanBackfillIdentity {
            version: &version,
            checksum: &checksum,
        };
        let spec = test_spec("items", "id");
        finish_plan_backfill(&actor, version.as_str(), identity, &spec, "tester")
            .await
            .expect("first finalization");
        finish_plan_backfill(&actor, version.as_str(), identity, &spec, "tester")
            .await
            .expect("idempotent finalization");

        actor
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        let rows = actor
            .query(&format!(
                "SELECT count(*) FROM \"_mig\".schema_migrations \
                  WHERE version = {} AND event_kind = 'applied'",
                sql_lit(version.as_str())
            ))
            .await
            .expect("count applied events");
        assert_eq!(rows[0][0].as_deref(), Some("1"));
    }

    #[compio::test]
    async fn missing_progress_row_cannot_create_plan_journal_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        super::super::journal_sql::ensure_journal(&actor)
            .await
            .expect("create journal");
        ensure_backfill_progress(&actor)
            .await
            .expect("create progress table");

        let version = MigrationId::derive("missing-backfill-finalization", b"stable");
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: "missing progress must not finalize",
            down: None,
            flags: &crate::model::migration::MigrationFlags::default(),
            owner_app: "app",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        let identity = PlanBackfillIdentity {
            version: &version,
            checksum: &checksum,
        };
        let spec = test_spec("missing_items", "id");

        let error = finish_plan_backfill(&actor, version.as_str(), identity, &spec, "tester")
            .await
            .expect_err("missing progress must abort finalization");
        assert!(
            error
                .to_string()
                .contains("completion update affected 0 rows"),
            "{error}"
        );
        assert!(actor.is_autocommit().await.expect("autocommit probe"));

        actor
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        let rows = actor
            .query(&format!(
                "SELECT count(*) FROM \"_mig\".schema_migrations WHERE version = {}",
                sql_lit(version.as_str())
            ))
            .await
            .expect("verify journal remains empty");
        assert_eq!(rows[0][0].as_deref(), Some("0"));
    }

    #[compio::test]
    async fn incomplete_legacy_progress_is_upgraded_then_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE legacy_items (id INTEGER PRIMARY KEY, value INTEGER NOT NULL); \
                 INSERT INTO legacy_items (id, value) VALUES (1, 0), (2, 0)",
            )
            .await
            .expect("seed target");
        actor
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        actor
            .exec(
                "CREATE TABLE \"_mig\".schema_backfills (\
                    backfill_id TEXT PRIMARY KEY, checksum TEXT, name TEXT NOT NULL, \
                    target_table TEXT NOT NULL, cursor_column TEXT NOT NULL, \
                    last_cursor TEXT, rows_done INTEGER NOT NULL DEFAULT 0, \
                    batches_done INTEGER NOT NULL DEFAULT 0, \
                    complete INTEGER NOT NULL DEFAULT 0, applied_by TEXT NOT NULL\
                 ); \
                 INSERT INTO \"_mig\".schema_backfills \
                    (backfill_id, name, target_table, cursor_column, last_cursor, applied_by) \
                 VALUES ('legacy', 'fill_legacy_items', 'legacy_items', 'id', '1', 'tester')",
            )
            .await
            .expect("seed legacy progress");

        let spec = test_spec("legacy_items", "id");
        // Direct backfills use the spec-derived id, so make the seeded key match.
        let legacy_id = spec.backfill_id();
        actor
            .exec(&format!(
                "UPDATE \"_mig\".schema_backfills SET backfill_id = {} WHERE backfill_id = 'legacy'",
                sql_lit(&legacy_id)
            ))
            .await
            .expect("match progress key");
        let error =
            run_backfill_bounded(&actor, &spec, &spec.set_clause, None, "tester", None, None)
                .await
                .expect_err("legacy progress has no safe terminal boundary");
        assert!(
            error.to_string().contains("legacy backfill progress"),
            "{error}"
        );

        actor
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        let columns = actor
            .query("PRAGMA \"_mig\".table_info(schema_backfills)")
            .await
            .expect("inspect upgraded progress schema");
        assert!(columns
            .iter()
            .any(|row| row[1].as_deref() == Some("end_cursor")));
        assert!(columns
            .iter()
            .any(|row| row[1].as_deref() == Some("cohort_initialized")));

        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        let rows = actor
            .query("SELECT count(*) FROM legacy_items WHERE value <> 0")
            .await
            .expect("verify no mutation");
        assert_eq!(rows[0][0].as_deref(), Some("0"));
    }

    #[compio::test]
    async fn unique_non_primary_cursor_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE items (\
                    id INTEGER PRIMARY KEY, code TEXT NOT NULL UNIQUE, value INTEGER NOT NULL\
                 ); \
                 INSERT INTO items (id, code, value) VALUES (1, 'a', 0)",
            )
            .await
            .expect("seed table");

        let spec = test_spec("items", "code");
        let error =
            run_backfill_bounded(&actor, &spec, &spec.set_clause, None, "tester", None, None)
                .await
                .expect_err("a UNIQUE index is not a durable cursor contract");
        assert!(matches!(
            error,
            BackfillError::CursorNotUniqueNotNull { .. }
        ));
    }

    #[compio::test]
    async fn blob_or_mixed_storage_cursor_is_rejected_before_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE mixed (\
                    key TEXT PRIMARY KEY NOT NULL, value INTEGER NOT NULL\
                 ); \
                 INSERT INTO mixed (key, value) VALUES ('safe', 0), (X'80FF', 0)",
            )
            .await
            .expect("seed mixed storage classes");

        let spec = test_spec("mixed", "key");
        let error =
            run_backfill_bounded(&actor, &spec, &spec.set_clause, None, "tester", None, None)
                .await
                .expect_err("mixed TEXT/BLOB storage classes are unsafe");
        assert!(matches!(
            error,
            BackfillError::CursorNotUniqueNotNull { .. }
        ));

        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        let rows = actor
            .query("SELECT count(*) FROM mixed WHERE value <> 0")
            .await
            .expect("verify no mutation");
        assert_eq!(rows[0][0].as_deref(), Some("0"));
    }

    #[compio::test]
    async fn real_primary_key_cursor_is_rejected_before_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE real_keys (\
                    key REAL PRIMARY KEY NOT NULL, value INTEGER NOT NULL\
                 ); \
                 INSERT INTO real_keys (key, value) VALUES (1.5, 0)",
            )
            .await
            .expect("seed REAL key");

        let spec = test_spec("real_keys", "key");
        let error =
            run_backfill_bounded(&actor, &spec, &spec.set_clause, None, "tester", None, None)
                .await
                .expect_err("REAL checkpoints are unsupported");
        assert!(matches!(
            error,
            BackfillError::CursorNotUniqueNotNull { .. }
        ));
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

    /// A SAFE backfill whose STRING LITERAL happens to embed
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

    /// The high-water mark is computed IN SQL —
    /// `SELECT max(<cursor>), ..., count(*) FROM (<same window>)`, so it honors the cursor column's
    /// declared collation (mirroring the PG `max(_bf_key)::text`), NOT a Rust BINARY
    /// `cells.max`. This pins the statement shape (collation-correctness over real
    /// data is proven by the e2e `sqlite_backfill_nocase_cursor_exactly_once` +
    /// `sqlite_backfill_real_cursor_*` against temp-file SQLite). The window
    /// predicate/ordering/limit MUST match `build_batch_sql` byte-for-byte so the two
    /// page the identical window.
    #[test]
    fn window_max_first_vs_resume_shape() {
        let first = build_window_max_sql("\"t\"", "\"id\"", None, false);
        assert_eq!(
            first,
            "SELECT max(\"id\"), min(typeof(\"id\")), max(typeof(\"id\")), count(*) FROM ( \
                SELECT \"id\" FROM \"t\" \
                 WHERE \"id\" <= ?1 \
                 ORDER BY \"id\" ASC LIMIT ?2 \
             )",
            "{first}"
        );
        let resume = build_window_max_sql("\"t\"", "\"id\"", Some("\"a\" IS NULL"), true);
        assert_eq!(
            resume,
            "SELECT max(\"id\"), min(typeof(\"id\")), max(typeof(\"id\")), count(*) FROM ( \
                SELECT \"id\" FROM \"t\" \
                 WHERE \"id\" > ?1 AND \"id\" <= ?2 AND (\"a\" IS NULL) \
                 ORDER BY \"id\" ASC LIMIT ?3 \
             )",
            "{resume}"
        );
    }

    /// The window-max SELECT and the batch UPDATE MUST page the IDENTICAL window
    /// (same predicate, same ORDER BY, same `?n` limit slot) — otherwise the resume
    /// cursor and the mutation diverge. Assert the shared predicate/order/limit
    /// substring appears verbatim in both renderings.
    #[test]
    fn window_max_and_batch_share_the_window() {
        for (filter, have_cursor) in [(None, false), (Some("\"a\" IS NULL"), true), (None, true)] {
            let batch = build_batch_sql("\"t\"", "\"id\"", "\"v\" = 1", filter, have_cursor);
            let wmax = build_window_max_sql("\"t\"", "\"id\"", filter, have_cursor);
            let filter_sql = filter.map(|f| format!(" AND ({f})")).unwrap_or_default();
            let limit_ph = if have_cursor { "?3" } else { "?2" };
            let pred = window_predicate("\"id\"", &filter_sql, have_cursor);
            let shared = format!("WHERE {pred} ORDER BY \"id\" ASC LIMIT {limit_ph}");
            assert!(
                batch.contains(&shared),
                "batch missing shared window: {batch}"
            );
            assert!(
                wmax.contains(&shared),
                "window-max missing shared window: {wmax}"
            );
        }
    }

    #[test]
    fn terminal_cursor_query_uses_filter_and_cursor_collation_order() {
        assert_eq!(
            build_end_cursor_sql("\"t\"", "\"id\"", Some("\"ready\" = 1")),
            "SELECT \"id\", typeof(\"id\") FROM \"t\" WHERE 1=1 AND (\"ready\" = 1) ORDER BY \"id\" DESC LIMIT 1"
        );
    }
}
