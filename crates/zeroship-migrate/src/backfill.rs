//! Large-table batched / resumable backfill (design §5 "Large-table backfill").
//!
//! A **backfill** is a long-running data transformation over a large table —
//! populate a new column, normalize values, recompute a denormalized field —
//! that must NOT run as one giant transaction (it would hold locks, bloat WAL,
//! and risk the mandatory `statement_timeout`). Instead it runs in bounded
//! **batches**, each its own committed transaction, advancing a cursor over the
//! table's ordered key, **resumable** after a crash via a meta-schema progress
//! row.
//!
//! # Why this is shaped the way it is
//!
//! - **Each batch is its own `BEGIN; … COMMIT`** under the migrator role with a
//!   `SET LOCAL statement_timeout`, mirroring [`crate::executor::apply`]'s
//!   transactional path. The loop never holds one big transaction, so it is
//!   non-blocking: every batch commits and releases its row locks before the
//!   next starts.
//!
//! - **The engine OWNS the batch-window predicate.** The creator/AI authors only
//!   the per-row transform (a SQL `SET`-clause body, plus an optional row
//!   filter). The engine builds the full `UPDATE <table> SET <authored> WHERE
//!   <engine cursor window> [AND (<authored filter>)]` itself, with the cursor
//!   bounds passed as **bound parameters** ($1) and the table / cursor column as
//!   **validated, quoted identifiers** — the author can neither inject into the
//!   loop-control predicate nor escape the target table.
//!
//! - **The assembled statement is guard-checked.** Before any batch runs, the
//!   fully-assembled `UPDATE` goes through [`crate::guard::SqlGuard::check`] —
//!   the SAME parse-time deny-list / cross-schema confinement a normal
//!   migration's `up` passes — and then executes under the least-privilege
//!   `migrator` role. So the whole statement (engine predicate + authored
//!   transform) is validated, not just a fragment.
//!
//! - **Resumability is crash-safe by construction.** The batch `UPDATE` and the
//!   progress-cursor advance commit in the SAME transaction (the progress write
//!   runs as the **admin** — the migrator has no meta-schema grant, the journal's
//!   deny-by-absence model). A crash mid-batch leaves **both or neither**: either
//!   the rows are updated AND the cursor advanced, or nothing. On the next
//!   `run_backfill`, the loop reads the last committed cursor and re-runs the
//!   window strictly after it — never restarting from zero, never skipping rows,
//!   never double-applying (a crash before the commit rolled back the mutation
//!   too, so re-running the same window is correct even for a non-idempotent
//!   transform like `val = val + 1`).
//!
//! # Locking model
//!
//! Unlike apply/rollback (which hold the project advisory lock for the whole
//! run), a backfill can run for **minutes or hours** — holding the project lock
//! that long would block every other deploy for the project. So a backfill
//! acquires the project advisory lock **per batch** (`pg_advisory_xact_lock`
//! inside each batch transaction, auto-released at COMMIT), serializing each
//! batch against concurrent apply/rollback/other-backfill activity without
//! starving them between batches. Correctness across batches does NOT depend on
//! the lock — it rests on the committed progress cursor, so re-acquiring per
//! batch is safe: a concurrent migration that commits between two batches is
//! simply observed by the next batch's window.

use compio_postgres::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::approval::Approval;
use crate::db::ExecutorConfig;
use crate::guard::{GuardConfig, GuardError, SqlGuard};
use crate::journal::JournalError;

/// The structured definition of a large-table backfill (design §5).
///
/// The creator/AI supplies the **target** ([`table`](Self::table)), the ordered
/// key to page by ([`cursor_column`](Self::cursor_column)), the
/// [`batch_size`](Self::batch_size), the per-row transform
/// ([`set_clause`](Self::set_clause)), and an optional row
/// [`filter`](Self::filter). The engine owns everything else — the cursor-window
/// predicate, the parameter binding, the loop control — so the authored input
/// can never escape the target table or inject into the loop control.
#[derive(Debug, Clone)]
pub struct BackfillSpec {
    /// The target table — a bare (unqualified) identifier in the project schema.
    /// Validated as a real identifier and quoted; a schema-qualified or otherwise
    /// malformed name is rejected ([`BackfillError::InvalidIdentifier`]). The
    /// engine pins `search_path` to the project schema, so the resolved table is
    /// always the project's own.
    pub table: String,
    /// The ordered, unique-ish key to page by (the table's primary key or another
    /// strictly-ordered column). Validated as an identifier and quoted. The engine
    /// pages `WHERE cursor_column > $last ORDER BY cursor_column ASC LIMIT
    /// batch_size`, so it must be totally ordered for the window to make progress
    /// and not skip rows.
    pub cursor_column: String,
    /// Rows per batch. Each batch updates at most this many rows in its own
    /// transaction. Must be non-zero ([`BackfillError::InvalidBatchSize`]).
    pub batch_size: u32,
    /// The authored per-row transform — the body of the `UPDATE … SET <here>`
    /// (e.g. `normalized = lower(raw)`, `seq = seq + 1`,
    /// `full_name = first || ' ' || last`). It may reference the row's own
    /// columns. The engine assembles it into the full statement and **guard-checks
    /// the assembled `UPDATE`** before running, so this is validated exactly like a
    /// migration `up` — a cross-schema / dangerous transform is denied.
    pub set_clause: String,
    /// An optional authored row filter — extra `WHERE` conjunct AND-ed onto the
    /// engine's cursor window (e.g. `normalized IS NULL` to touch only un-backfilled
    /// rows). `None` backfills every row. Combined as `… AND (<filter>)` and
    /// included in the guard-checked assembled statement.
    pub filter: Option<String>,
    /// A human-readable name for the backfill (used in the progress row + an
    /// optional stable id seed). E.g. `"normalize_emails"`.
    pub name: String,
}

impl BackfillSpec {
    /// A stable identity for this backfill, used as the progress-row PK so a
    /// resumed run finds its own progress. Derived from the target table, cursor
    /// column, the authored transform + filter, and the name — so changing the
    /// transform yields a *different* backfill id (a re-authored backfill does not
    /// silently resume against an old, incompatible cursor).
    #[must_use]
    pub fn backfill_id(&self) -> String {
        let mut h = Sha256::new();
        for field in [
            self.name.as_str(),
            self.table.as_str(),
            self.cursor_column.as_str(),
            self.set_clause.as_str(),
            self.filter.as_deref().unwrap_or("\u{0}<none>"),
        ] {
            h.update((field.len() as u64).to_be_bytes());
            h.update(field.as_bytes());
        }
        h.update(self.batch_size.to_be_bytes());
        format!("bf_{}", hex::encode(&h.finalize()[..16]))
    }
}

/// What [`run_backfill`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillOutcome {
    /// The backfill's stable id (the progress-row key).
    pub backfill_id: String,
    /// Number of batches committed **this run** (excludes batches a prior,
    /// resumed run committed).
    pub batches: u64,
    /// Number of rows updated **this run**.
    pub rows_updated: u64,
    /// `true` if a prior run's progress row was found and this run resumed from
    /// its committed cursor (vs. starting fresh).
    pub resumed: bool,
    /// `true` when the backfill reached the end of the table (a batch updated
    /// fewer than `batch_size` rows) — the progress row is marked complete.
    pub complete: bool,
}

/// Error from [`run_backfill`].
#[derive(Debug, thiserror::Error)]
pub enum BackfillError {
    /// A database error outside a guarded/progress step.
    #[error("db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// A progress / journal-schema operation failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// A backfill is destructive-ish data mutation, so it requires
    /// [`Approval::Approved`]. With [`Approval::None`] this is refused before any
    /// batch runs — the executor's own defense-in-depth approval gate (design
    /// §1.6), independent of any engine-layer gate.
    #[error("backfill requires Approval::Approved (it mutates table data) but it was not given")]
    ApprovalRequired,
    /// The [`BackfillSpec::table`] or [`BackfillSpec::cursor_column`] is not a
    /// valid bare SQL identifier (it is empty, schema-qualified, or contains
    /// characters outside `[A-Za-z_][A-Za-z0-9_]*`). Rejected before any SQL is
    /// assembled — an injection attempt via the identifier cannot reach the DB.
    #[error("invalid identifier for {what}: {value:?} (must be a bare [A-Za-z_][A-Za-z0-9_]* identifier)")]
    InvalidIdentifier {
        /// Which field was invalid (`"table"` / `"cursor_column"`).
        what: &'static str,
        /// The offending value.
        value: String,
    },
    /// [`BackfillSpec::batch_size`] was zero.
    #[error("batch_size must be non-zero")]
    InvalidBatchSize,
    /// The assembled `UPDATE` (engine cursor window + authored transform + filter)
    /// was denied by the SQL guard — the whole backfill aborts before any batch
    /// runs, nothing is mutated.
    #[error("backfill assembled statement denied by guard: {source}")]
    Guard {
        /// The underlying guard rejection.
        #[source]
        source: GuardError,
    },
    /// The target table or cursor column does not exist in the project schema, or
    /// the column's type could not be resolved for the cursor cast. Aborts before
    /// any batch runs.
    #[error("backfill target not found: {0}")]
    TargetNotFound(String),
    /// The [`BackfillSpec::cursor_column`] is not safe to page on: it is NOT
    /// backed by a single-column `PRIMARY KEY` / `UNIQUE` index, or it is
    /// nullable. The whole batched design's exactly-once + non-blocking +
    /// filter-honoring correctness rests on a UNIQUE NOT NULL cursor — the UPDATE
    /// matches `cursor IN (window keys)`, so a NON-unique cursor over-matches
    /// every row sharing a key (updating far more than `batch_size` rows in one
    /// txn, defeating the bound, and mutating rows that do NOT match the authored
    /// filter), and a NULL cursor value is silently never paged. Rejected in
    /// pre-flight before any batch runs.
    #[error("cursor_column {cursor_column:?} on {table:?} is not safe to page on ({reason}); page on the table's PRIMARY KEY")]
    CursorNotUniqueNotNull {
        /// The target table.
        table: String,
        /// The offending cursor column.
        cursor_column: String,
        /// Why it was rejected (`"not backed by a single-column PRIMARY KEY or UNIQUE index"` / `"it is nullable"`).
        reason: &'static str,
    },
    /// The authored [`BackfillSpec::set_clause`] assigns the
    /// [`BackfillSpec::cursor_column`] itself. Mutating the cursor column breaks
    /// paging: the window reads pre-update cursor values and advances to their
    /// max, but the UPDATE changed those values, so rows whose cursor increased
    /// re-enter a later window (`cursor > old_max`) — re-processed, looping, and
    /// double-applying a non-idempotent transform. Rejected in pre-flight.
    #[error("set_clause assigns the cursor_column {cursor_column:?}; a backfill must not mutate the column it pages on (author a separate migration / page on an immutable key)")]
    CursorColumnMutated {
        /// The cursor column the transform illegally assigns.
        cursor_column: String,
    },
    /// A batch's `UPDATE` failed at execution (after the guard passed). The batch
    /// transaction was rolled back, so its progress was NOT advanced — the
    /// backfill is left at the last committed cursor and is safely resumable.
    #[error("backfill batch failed at cursor {at_cursor:?}: {source}")]
    BatchFailed {
        /// The last committed cursor when the failing batch started (`None` =
        /// from the beginning).
        at_cursor: Option<String>,
        /// The DB error from the failed batch.
        #[source]
        source: compio_postgres::Error,
    },
}

/// Validate a bare SQL identifier: non-empty, starts with a letter/underscore,
/// and contains only `[A-Za-z0-9_]`. This rejects schema-qualified names
/// (`control.users`), quoted-injection attempts (`t"; DROP …`), whitespace, and
/// any punctuation — so the value is safe to double-quote into an identifier
/// position. The resulting quoted identifier is still defense-in-depth: the
/// engine pins `search_path` to the project schema and runs under the migrator
/// role, so even a name that passed here resolves only to a project object.
fn validate_ident(what: &'static str, value: &str) -> Result<(), BackfillError> {
    let mut chars = value.chars();
    let ok_first = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if value.is_empty() || !ok_first || !ok_rest {
        return Err(BackfillError::InvalidIdentifier {
            what,
            value: value.to_string(),
        });
    }
    Ok(())
}

/// Double-quote a validated identifier (the value has already passed
/// [`validate_ident`], so it contains no `"`; the replace is belt-and-suspenders).
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// Build the per-batch statement (the ASSEMBLED statement that gets guard-checked
/// and then run each batch).
///
/// The cursor window is engine-owned. A `window` CTE picks the next `batch_size`
/// keys strictly after the last committed cursor (`cursor_column > $1`, the bound
/// parameter `$1` carrying the last cursor as text cast to the column's type),
/// ordered by the cursor ASC — the next contiguous slice. The `UPDATE` mutates
/// exactly those keys; a trailing `SELECT` returns the batch row-count and the
/// **max cursor of the window** rendered `::text`, both computed by Postgres under
/// the column's NATURAL order (so numerics are not lexically mis-maxed). The
/// author's `set_clause` and optional `filter` are interpolated as the SET body /
/// an extra conjunct — but the WHOLE statement is guard-checked, so an injection
/// in those fragments is caught at the assembled-statement level (cross-schema
/// ref, dangerous func, disallowed statement shape), not trusted.
///
/// `$1` is the prior cursor; when `have_cursor` is false (first batch) the window
/// degrades to "everything ordered, take the first `batch_size`". Two shapes are
/// emitted so the first batch has no dangling parameter.
///
/// `cursor_type` is the resolved column type from `pg_catalog` introspection
/// (e.g. `integer`, `text`, `bigint`) — a TRUSTED string, never the author's
/// input — used only for the bound-cursor cast.
fn build_batch_sql(
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
    cursor_type: &str,
    have_cursor: bool,
) -> String {
    let schema = quote_ident(&cfg.project_schema);
    let table = quote_ident(&spec.table);
    let col = quote_ident(&spec.cursor_column);
    let filter = spec
        .filter
        .as_deref()
        .map(|f| format!(" AND ({f})"))
        .unwrap_or_default();
    // `$1` carries the last cursor as TEXT (so it round-trips any column type
    // losslessly). The inner `$1::text` pins the parameter's inferred type to text
    // — without it, the prepared-statement describe step infers `$1` as the column
    // type and the driver rejects our text bind. The outer `::cursor_type` casts
    // the text back to the column's real type so the `>` comparison uses the
    // column's NATURAL order (numeric, not lexical). `cursor_type` is a trusted
    // catalog-derived string, never the author's input.
    let cursor_pred = if have_cursor {
        format!("{col} > ($1::text)::{cursor_type}")
    } else {
        "TRUE".to_string()
    };
    // CTE pipeline, one statement, run inside the batch transaction:
    //  - `_bf_window`: the next batch_size keys (engine-owned predicate).
    //  - `_bf_upd`: the authored UPDATE restricted to those keys, RETURNING the
    //               touched cursor.
    //  - final SELECT: COUNT(*) + MAX(window cursor)::text — natural-order max,
    //               so numeric columns are not lexically mis-ordered.
    format!(
        "WITH _bf_window AS ( \
             SELECT {col} AS _bf_key FROM {schema}.{table} \
             WHERE {cursor_pred}{filter} \
             ORDER BY {col} ASC \
             LIMIT {batch} \
         ), \
         _bf_upd AS ( \
             UPDATE {schema}.{table} AS _bf \
             SET {set_clause} \
             WHERE _bf.{col} IN (SELECT _bf_key FROM _bf_window) \
             RETURNING 1 \
         ) \
         SELECT (SELECT count(*) FROM _bf_upd) AS _bf_rows, \
                (SELECT max(_bf_key)::text FROM _bf_window) AS _bf_cursor",
        set_clause = spec.set_clause,
        batch = spec.batch_size,
    )
}

/// Bootstrap (idempotently) the meta-schema **backfill progress** table.
///
/// Lives in the same per-project meta schema as the journal — writable only by
/// the **admin** (the migrator role has NO grant on the meta schema, the journal's
/// deny-by-absence model). A backfill's batch `UPDATE` runs under the migrator
/// role and so CANNOT forge or skip its own progress. Unlike the journal of
/// record it is **mutable** (the cursor advances every batch), like the inflight
/// side-table — but progress + batch mutation commit together, so it is
/// crash-safe regardless.
///
/// # Errors
/// [`JournalError::Db`] on any DDL failure.
pub async fn ensure_backfill_progress(
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<(), JournalError> {
    let meta = quote_ident(&cfg.meta_schema);
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {meta}"))
        .await?;
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {meta}.schema_backfills (
            backfill_id    TEXT PRIMARY KEY,
            name           TEXT NOT NULL,
            target_table   TEXT NOT NULL,
            cursor_column  TEXT NOT NULL,
            last_cursor    TEXT,
            rows_done      BIGINT NOT NULL DEFAULT 0,
            batches_done   BIGINT NOT NULL DEFAULT 0,
            complete       BOOLEAN NOT NULL DEFAULT false,
            applied_by     TEXT NOT NULL,
            started_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
        )"
    ))
    .await?;
    Ok(())
}

/// The progress of a backfill, read from the meta-schema progress row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillProgress {
    /// The backfill's stable id.
    pub backfill_id: String,
    /// Human-readable name.
    pub name: String,
    /// The target table.
    pub target_table: String,
    /// The cursor column.
    pub cursor_column: String,
    /// The last committed cursor position (text form), or `None` if no batch has
    /// committed yet.
    pub last_cursor: Option<String>,
    /// Rows updated so far (across all runs).
    pub rows_done: i64,
    /// Batches committed so far (across all runs).
    pub batches_done: i64,
    /// `true` once the backfill reached the end of the table.
    pub complete: bool,
    /// When the last batch committed (RFC-3339).
    pub updated_at: String,
}

/// Read the progress of a backfill (observability — design §5 / Plan 6 surface).
///
/// Returns `None` if the backfill has never run (no progress row). **Read-only.**
/// Bootstraps the progress table idempotently first so a fresh project reports
/// cleanly.
///
/// # Errors
/// [`JournalError::Db`] on a read/bootstrap failure.
pub async fn backfill_progress(
    conn: &Client,
    cfg: &ExecutorConfig,
    backfill_id: &str,
) -> Result<Option<BackfillProgress>, JournalError> {
    ensure_backfill_progress(conn, cfg).await?;
    let meta = quote_ident(&cfg.meta_schema);
    let rows = conn
        .query(
            &format!(
                "SELECT backfill_id, name, target_table, cursor_column, last_cursor,
                        rows_done, batches_done, complete,
                        to_char(updated_at, 'YYYY-MM-DD\"T\"HH24:MI:SS.USOF') AS updated_at
                   FROM {meta}.schema_backfills WHERE backfill_id = $1"
            ),
            &[&backfill_id],
        )
        .await?;
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    Ok(Some(BackfillProgress {
        backfill_id: row.get("backfill_id"),
        name: row.get("name"),
        target_table: row.get("target_table"),
        cursor_column: row.get("cursor_column"),
        last_cursor: row.get("last_cursor"),
        rows_done: row.get("rows_done"),
        batches_done: row.get("batches_done"),
        complete: row.get("complete"),
        updated_at: row.get("updated_at"),
    }))
}

/// List EVERY backfill for the project (the project-wide observability surface,
/// alongside Plan 6's [`crate::status::status`]), in `started_at` order.
///
/// **Read-only.** Bootstraps the progress table idempotently first. A control
/// plane / dashboard reads this to show in-flight + completed backfills with
/// their cursor position, rows done, and completion state.
///
/// # Errors
/// [`JournalError::Db`] on a read/bootstrap failure.
pub async fn list_backfills(
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<Vec<BackfillProgress>, JournalError> {
    ensure_backfill_progress(conn, cfg).await?;
    let meta = quote_ident(&cfg.meta_schema);
    let rows = conn
        .query(
            &format!(
                "SELECT backfill_id, name, target_table, cursor_column, last_cursor,
                        rows_done, batches_done, complete,
                        to_char(updated_at, 'YYYY-MM-DD\"T\"HH24:MI:SS.USOF') AS updated_at
                   FROM {meta}.schema_backfills ORDER BY started_at, backfill_id"
            ),
            &[],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| BackfillProgress {
            backfill_id: row.get("backfill_id"),
            name: row.get("name"),
            target_table: row.get("target_table"),
            cursor_column: row.get("cursor_column"),
            last_cursor: row.get("last_cursor"),
            rows_done: row.get("rows_done"),
            batches_done: row.get("batches_done"),
            complete: row.get("complete"),
            updated_at: row.get("updated_at"),
        })
        .collect())
}

/// Resolve the cursor column's SQL type for the bound-cursor cast, also asserting
/// the table + column exist in the project schema. Returns the
/// `format_type`-rendered type (e.g. `integer`, `text`, `timestamp with time
/// zone`) which is a TRUSTED, catalog-derived string (never the author's input).
async fn resolve_cursor_type(
    conn: &Client,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
) -> Result<String, BackfillError> {
    let rows = conn
        .query(
            "SELECT format_type(a.atttypid, a.atttypmod) AS coltype
               FROM pg_attribute a
               JOIN pg_class c ON c.oid = a.attrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3
                AND a.attnum > 0 AND NOT a.attisdropped",
            &[&cfg.project_schema, &spec.table, &spec.cursor_column],
        )
        .await?;
    rows.into_iter()
        .next()
        .map(|r| r.get::<_, String>("coltype"))
        .ok_or_else(|| {
            BackfillError::TargetNotFound(format!(
                "{}.{} column {} not found",
                cfg.project_schema, spec.table, spec.cursor_column
            ))
        })
}

/// Assert the cursor column is safe to page on: backed by a single-column
/// `PRIMARY KEY` / `UNIQUE` index AND `NOT NULL`.
///
/// The whole backfill's correctness depends on this. The per-batch UPDATE
/// matches `cursor_column IN (SELECT key FROM window)` and advances the cursor to
/// the window's `max(key)`:
///   - a **non-unique** cursor over-matches EVERY row sharing a key value —
///     updating far more than `batch_size` rows in one transaction (a
///     low-cardinality cursor like a boolean rewrites a huge fraction of the
///     table in a single txn, defeating the non-blocking bound and the
///     `statement_timeout`), AND bypassing the authored `filter` (a row that does
///     not match the filter but shares a key with one that does is mutated too);
///   - a **NULL** cursor value never satisfies `cursor > $last` (NULL compares as
///     unknown) and sorts last, so NULL-keyed rows are silently never paged.
///
/// Both are closed by requiring a UNIQUE NOT NULL key (a `PRIMARY KEY` is the
/// canonical choice). Introspection binds schema/table/column as parameters
/// (injection-safe, like [`resolve_cursor_type`]).
async fn validate_cursor_column(
    conn: &Client,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
) -> Result<(), BackfillError> {
    // `indkey[0]` is the (0-based) single key attnum of a single-key
    // (`indnkeyatts = 1`) unique/primary index — so we match a one-column
    // PK/UNIQUE on exactly the cursor column (a composite or INCLUDE index does
    // not qualify).
    let rows = conn
        .query(
            "SELECT a.attnotnull AS not_null,
                    EXISTS (
                        SELECT 1 FROM pg_index i
                        WHERE i.indrelid = c.oid
                          AND (i.indisunique OR i.indisprimary)
                          AND i.indnkeyatts = 1
                          AND i.indkey[0] = a.attnum
                    ) AS is_unique
               FROM pg_attribute a
               JOIN pg_class c ON c.oid = a.attrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = $1 AND c.relname = $2 AND a.attname = $3
                AND a.attnum > 0 AND NOT a.attisdropped",
            &[&cfg.project_schema, &spec.table, &spec.cursor_column],
        )
        .await?;
    let Some(row) = rows.into_iter().next() else {
        // The column does not exist (resolve_cursor_type would have caught this
        // already, but stay defensive).
        return Err(BackfillError::TargetNotFound(format!(
            "{}.{} column {} not found",
            cfg.project_schema, spec.table, spec.cursor_column
        )));
    };
    let not_null: bool = row.get("not_null");
    let is_unique: bool = row.get("is_unique");
    if !is_unique {
        return Err(BackfillError::CursorNotUniqueNotNull {
            table: spec.table.clone(),
            cursor_column: spec.cursor_column.clone(),
            reason: "not backed by a single-column PRIMARY KEY or UNIQUE index",
        });
    }
    if !not_null {
        return Err(BackfillError::CursorNotUniqueNotNull {
            table: spec.table.clone(),
            cursor_column: spec.cursor_column.clone(),
            reason: "it is nullable",
        });
    }
    Ok(())
}

/// Reject an authored `set_clause` that assigns the cursor column itself.
///
/// Parses the ASSEMBLED statement (the exact `WITH … UPDATE … SET <set_clause> …`
/// we run) with the real Postgres parser and inspects the `UPDATE` CTE's SET
/// target list. If the cursor column is among the assigned targets the backfill
/// is refused — mutating the paged column breaks the cursor (rows whose key is
/// changed re-enter a later window → re-processing / infinite loop /
/// double-apply). Done once in pre-flight, not per batch.
fn assert_cursor_not_mutated(
    assembled_sql: &str,
    cursor_column: &str,
) -> Result<(), BackfillError> {
    // The assembled SQL has already passed the guard (which re-parses it), so a
    // parse failure here is not expected; treat it as a target problem rather
    // than panicking.
    let parsed = pg_query::parse(assembled_sql).map_err(|e| {
        BackfillError::TargetNotFound(format!("could not parse assembled backfill SQL: {e}"))
    })?;
    let json = serde_json::to_value(&parsed.protobuf).unwrap_or(Value::Null);
    let mut mutates = false;
    walk_update_set_targets(&json, &mut |name| {
        if name.eq_ignore_ascii_case(cursor_column) {
            mutates = true;
            return true;
        }
        false
    });
    if mutates {
        return Err(BackfillError::CursorColumnMutated {
            cursor_column: cursor_column.to_string(),
        });
    }
    Ok(())
}

/// Walk the serialized parse tree for every `UpdateStmt` and invoke `visit` with
/// each SET target column name (`UpdateStmt.target_list[].ResTarget.name`).
/// `visit` returns `true` to short-circuit. Covers the UPDATE nested in the
/// backfill's CTE (the walk is depth-agnostic).
fn walk_update_set_targets(v: &Value, visit: &mut dyn FnMut(&str) -> bool) -> bool {
    match v {
        Value::Object(map) => {
            if let Some(upd) = map.get("UpdateStmt") {
                if let Some(Value::Array(targets)) = upd.get("target_list") {
                    for t in targets {
                        // Each target is a prost `Node` wrapper, serialized as
                        // `{"node": {"ResTarget": {name, …}}}`; tolerate the
                        // unwrapped `{"ResTarget": {…}}` shape too.
                        let res = t
                            .get("ResTarget")
                            .or_else(|| t.get("node").and_then(|n| n.get("ResTarget")));
                        if let Some(name) =
                            res.and_then(|r| r.get("name")).and_then(Value::as_str)
                        {
                            if !name.is_empty() && visit(name) {
                                return true;
                            }
                        }
                    }
                }
            }
            map.values().any(|c| walk_update_set_targets(c, visit))
        }
        Value::Array(items) => items.iter().any(|i| walk_update_set_targets(i, visit)),
        _ => false,
    }
}

/// Run (or resume) a large-table backfill (design §5).
///
/// Pages over `spec.table` by `spec.cursor_column` in `spec.batch_size` chunks.
/// Each batch is its own committed transaction:
///
/// ```text
/// BEGIN;
///   pg_advisory_xact_lock(project)     -- per-batch serialization, auto-released at COMMIT
///   SET LOCAL search_path = <project>; SET LOCAL statement_timeout = …; SET LOCAL lock_timeout = …;
///   SET LOCAL ROLE migrator;
///     UPDATE <table> SET <authored> WHERE <engine cursor window> [AND (<filter>)] RETURNING cursor;
///   RESET ROLE;                        -- back to admin for the progress write
///   UPDATE <meta>.schema_backfills SET last_cursor = <max>, rows_done += n, … ;  -- as admin
/// COMMIT;
/// ```
///
/// The batch mutation + the progress advance commit **together**, so a crash
/// leaves both-or-neither and the next `run_backfill` resumes from the last
/// committed cursor — never restarting from zero, never skipping, never
/// double-applying.
///
/// `approval` must be [`Approval::Approved`] (a backfill mutates table data).
/// `applied_by` is recorded on the progress row.
///
/// # Errors
/// - [`BackfillError::ApprovalRequired`] — `approval != Approved`.
/// - [`BackfillError::InvalidIdentifier`] / [`BackfillError::InvalidBatchSize`] —
///   a malformed spec; rejected before any SQL is assembled.
/// - [`BackfillError::Guard`] — the assembled `UPDATE` was denied; nothing ran.
/// - [`BackfillError::TargetNotFound`] — the table/column does not exist.
/// - [`BackfillError::BatchFailed`] — a batch's `UPDATE` failed; the batch rolled
///   back, progress is at the last committed cursor, safely resumable.
/// - [`BackfillError::Db`] / [`BackfillError::Journal`] — infrastructure failures.
pub async fn run_backfill(
    conn: &Client,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
    approval: Approval,
    applied_by: &str,
) -> Result<BackfillOutcome, BackfillError> {
    run_backfill_bounded(conn, cfg, spec, approval, applied_by, None).await
}

/// Run (or resume) a backfill, stopping after at most `max_batches` committed
/// batches (`None` = run to completion, the [`run_backfill`] behaviour).
///
/// This is the checkpointed form: a caller (or a long-deploy orchestrator) can
/// run a bounded number of batches, yield, and resume later — each batch commits
/// its progress, so a partial run leaves a valid resumable cursor and is NOT
/// marked complete. When the bound is hit before the table tail, the returned
/// [`BackfillOutcome::complete`] is `false`; a subsequent call resumes from the
/// committed cursor. Reaching the table tail within the bound marks complete and
/// returns `complete = true`, identical to [`run_backfill`].
///
/// # Errors
/// Same as [`run_backfill`].
pub async fn run_backfill_bounded(
    conn: &Client,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
    approval: Approval,
    applied_by: &str,
    max_batches: Option<u64>,
) -> Result<BackfillOutcome, BackfillError> {
    // Gate 1 — approval (defense-in-depth; design §1.6).
    if approval != Approval::Approved {
        return Err(BackfillError::ApprovalRequired);
    }
    // Gate 2 — identifier + batch-size validation, BEFORE any SQL is assembled.
    validate_ident("table", &spec.table)?;
    validate_ident("cursor_column", &spec.cursor_column)?;
    if spec.batch_size == 0 {
        return Err(BackfillError::InvalidBatchSize);
    }

    let backfill_id = spec.backfill_id();
    ensure_backfill_progress(conn, cfg).await?;

    // Resolve the cursor column type (also asserts table + column exist). The
    // returned type is catalog-derived (trusted), used only for the bound-cursor
    // cast — never interpolated from the author.
    let cursor_type = resolve_cursor_type(conn, cfg, spec).await?;

    // Gate 3a — the cursor column MUST be a single-column UNIQUE/PK key and NOT
    // NULL. Without this the `cursor IN (window keys)` UPDATE over-matches every
    // row sharing a key (defeating the batch bound + bypassing the filter) and
    // NULL keys are silently skipped. Rejected before any batch runs.
    validate_cursor_column(conn, cfg, spec).await?;

    // Gate 3b — GUARD the ASSEMBLED statement (both window shapes: with and without
    // the cursor parameter), so the engine predicate + authored transform + filter
    // are validated as one unit before any batch runs. A denial aborts everything.
    let guard = SqlGuard::new(GuardConfig::confined(cfg.project_schema.clone()));
    for have_cursor in [false, true] {
        let sql = build_batch_sql(cfg, spec, &cursor_type, have_cursor);
        guard
            .check(&sql)
            .map_err(|source| BackfillError::Guard { source })?;
    }

    // Gate 3c — the authored transform MUST NOT assign the cursor column itself
    // (mutating the paged key breaks the cursor → re-processing / loop /
    // double-apply). Inspect the assembled UPDATE's SET target list.
    assert_cursor_not_mutated(
        &build_batch_sql(cfg, spec, &cursor_type, true),
        &spec.cursor_column,
    )?;

    // Read existing progress to resume from the last committed cursor (if any).
    let existing = backfill_progress(conn, cfg, &backfill_id).await?;
    let resumed = existing.as_ref().is_some_and(|p| p.last_cursor.is_some());
    let mut last_cursor: Option<String> = existing.as_ref().and_then(|p| p.last_cursor.clone());
    // A backfill already marked complete is a no-op (idempotent re-run).
    if existing.as_ref().is_some_and(|p| p.complete) {
        return Ok(BackfillOutcome {
            backfill_id,
            batches: 0,
            rows_updated: 0,
            resumed,
            complete: true,
        });
    }

    // Insert the progress row up-front (idempotent) so observability sees the
    // backfill exists even before the first batch commits. Runs as admin.
    upsert_progress_row(conn, cfg, &backfill_id, spec, applied_by).await?;

    let mut batches: u64 = 0;
    let mut rows_updated: u64 = 0;

    // The loop runs each batch as a committed transaction until the window is
    // exhausted (or the `max_batches` bound is hit). Resumption (the hard case) is
    // ACROSS invocations — a crash kills the process mid-loop; the next call reads
    // the committed cursor and re-enters from there. Each batch commits its
    // progress, so a bounded/crashed run always leaves a valid resumable cursor.
    let mut tail_reached = false;
    loop {
        if max_batches.is_some_and(|m| batches >= m) {
            // Bound hit before the tail — leave NOT complete; resumable next call.
            break;
        }
        let (n, max_cursor) =
            run_one_batch(conn, cfg, spec, &cursor_type, last_cursor.as_deref(), &backfill_id)
                .await?;
        if n == 0 {
            // Empty window — nothing left to do; the backfill is complete.
            tail_reached = true;
            break;
        }
        batches += 1;
        rows_updated += n;
        last_cursor = max_cursor;
        // A short batch (fewer than batch_size rows) means the window had no more
        // rows after it — the table tail is reached.
        if n < u64::from(spec.batch_size) {
            tail_reached = true;
            break;
        }
    }
    if tail_reached {
        mark_complete(conn, cfg, &backfill_id).await?;
    }

    Ok(BackfillOutcome {
        backfill_id,
        batches,
        rows_updated,
        resumed,
        complete: tail_reached,
    })
}

/// Insert the progress row for a fresh backfill (idempotent — `ON CONFLICT DO
/// NOTHING`, so a resumed run keeps its committed cursor). Runs as admin.
async fn upsert_progress_row(
    conn: &Client,
    cfg: &ExecutorConfig,
    backfill_id: &str,
    spec: &BackfillSpec,
    applied_by: &str,
) -> Result<(), BackfillError> {
    let meta = quote_ident(&cfg.meta_schema);
    conn.execute(
        &format!(
            "INSERT INTO {meta}.schema_backfills
                 (backfill_id, name, target_table, cursor_column, applied_by)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (backfill_id) DO NOTHING"
        ),
        &[
            &backfill_id,
            &spec.name,
            &spec.table,
            &spec.cursor_column,
            &applied_by,
        ],
    )
    .await?;
    Ok(())
}

/// Mark a backfill's progress row complete (runs as admin, its own statement).
async fn mark_complete(
    conn: &Client,
    cfg: &ExecutorConfig,
    backfill_id: &str,
) -> Result<(), BackfillError> {
    let meta = quote_ident(&cfg.meta_schema);
    conn.execute(
        &format!(
            "UPDATE {meta}.schema_backfills
                SET complete = true, updated_at = now()
              WHERE backfill_id = $1"
        ),
        &[&backfill_id],
    )
    .await?;
    Ok(())
}

/// Run ONE batch in its own transaction (the non-blocking unit). Returns
/// `(rows_updated, new_max_cursor)`.
///
/// The batch `UPDATE` (as the migrator) and the progress advance (as the admin)
/// commit in the SAME transaction — crash-safe both-or-neither. The project
/// advisory lock is taken with `pg_advisory_xact_lock` so it auto-releases at
/// COMMIT (per-batch serialization without holding the lock across the whole
/// long-running backfill).
async fn run_one_batch(
    conn: &Client,
    cfg: &ExecutorConfig,
    spec: &BackfillSpec,
    cursor_type: &str,
    last_cursor: Option<&str>,
    backfill_id: &str,
) -> Result<(u64, Option<String>), BackfillError> {
    let have_cursor = last_cursor.is_some();
    let batch_sql = build_batch_sql(cfg, spec, cursor_type, have_cursor);

    conn.batch_execute("BEGIN").await?;

    // Per-batch project advisory lock (xact-scoped: auto-released at COMMIT/ROLLBACK).
    if let Err(e) = conn
        .execute(
            "SELECT pg_advisory_xact_lock(hashtext($1)::bigint)",
            &[&cfg.project_id],
        )
        .await
    {
        let _ = conn.batch_execute("ROLLBACK").await;
        return Err(BackfillError::Db(e));
    }

    // Pin search_path + mandatory timeouts (SET LOCAL — scoped to this txn).
    let set_local = format!(
        "SET LOCAL search_path TO \"{}\"; \
         SET LOCAL statement_timeout = {}; \
         SET LOCAL lock_timeout = {};",
        cfg.project_schema.replace('"', "\"\""),
        cfg.statement_timeout_ms(),
        cfg.lock_timeout_ms(),
    );
    if let Err(e) = conn.batch_execute(&set_local).await {
        let _ = conn.batch_execute("ROLLBACK").await;
        return Err(BackfillError::Db(e));
    }

    // Drop to the migrator role for the authored `UPDATE` ONLY (line-2 confinement).
    if let Some(role) = &cfg.migrator_role {
        if let Err(e) = conn
            .batch_execute(&format!("SET LOCAL ROLE \"{}\"", role.replace('"', "\"\"")))
            .await
        {
            let _ = conn.batch_execute("ROLLBACK").await;
            return Err(BackfillError::Db(e));
        }
    }

    // Run the batch statement as the migrator. The single result row carries the
    // batch row-count (`_bf_rows`) and the window's max cursor (`_bf_cursor`,
    // natural-order max rendered ::text), both computed by Postgres in the SAME
    // statement as the UPDATE.
    let query_result = if have_cursor {
        let lc = last_cursor.expect("have_cursor implies Some");
        conn.query_one(&batch_sql, &[&lc]).await
    } else {
        conn.query_one(&batch_sql, &[]).await
    };
    let row = match query_result {
        Ok(row) => row,
        Err(e) => {
            if let Err(rb) = conn.batch_execute("ROLLBACK").await {
                tracing::warn!(error = %rb, "zeroship-migrate: ROLLBACK failed after a backfill batch error");
            }
            return Err(BackfillError::BatchFailed {
                at_cursor: last_cursor.map(str::to_string),
                source: e,
            });
        }
    };

    // RESET ROLE back to admin (still inside the txn) so the progress write — meta
    // schema, which the migrator has no grant for — runs as the admin.
    if cfg.migrator_role.is_some() {
        if let Err(e) = conn.batch_execute("RESET ROLE").await {
            let _ = conn.batch_execute("ROLLBACK").await;
            return Err(BackfillError::Db(e));
        }
    }

    let n = u64::try_from(row.get::<_, i64>("_bf_rows")).unwrap_or(0);
    // The new cursor high-water-mark = the window's natural-order max (NULL on an
    // empty window). The next batch's window is `cursor > this`, advancing
    // monotonically with no skips.
    let max_cursor: Option<String> = row.get("_bf_cursor");

    if n > 0 {
        // Advance progress IN THE SAME TRANSACTION as the UPDATE (both-or-neither).
        let meta = quote_ident(&cfg.meta_schema);
        if let Err(e) = conn
            .execute(
                &format!(
                    "UPDATE {meta}.schema_backfills
                        SET last_cursor = $2,
                            rows_done = rows_done + $3,
                            batches_done = batches_done + 1,
                            updated_at = now()
                      WHERE backfill_id = $1"
                ),
                &[
                    &backfill_id,
                    &max_cursor,
                    &i64::try_from(n).unwrap_or(i64::MAX),
                ],
            )
            .await
        {
            if let Err(rb) = conn.batch_execute("ROLLBACK").await {
                tracing::warn!(error = %rb, "zeroship-migrate: ROLLBACK failed after a backfill progress error");
            }
            return Err(BackfillError::Journal(JournalError::Db(e)));
        }
    }

    conn.batch_execute("COMMIT").await?;
    Ok((n, max_cursor))
}
