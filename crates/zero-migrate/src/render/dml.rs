//! The creator-**DML assembler** + the closed-AST **expression renderer**.
//!
//! `IrAuthor::lower` compiles the DDL ops (`createTable`/`alter*`/…) into the
//! same [`Migration`](crate::model::migration::Migration) shape the declarative differ
//! emits. This module is the peer for the **DML** ops — `insert` / `update` /
//! `del` / `backfill` — and for the closed expression AST ([`crate::model::expr::Expr`])
//! they carry in their `set` / `where` / `filter` positions.
//!
//! # Two rendering modes, one source of truth
//!
//! A migration value can reach the database two structurally-distinct ways, and
//! this module owns both:
//!
//! 1. **Parameterized one-shot DML** ([`assemble_insert`] / [`assemble_update`] /
//!    [`assemble_delete`]). Every authored VALUE — an `insert` row scalar, an
//!    `update SET` literal, a `where`-predicate literal — is emitted as a NATIVE
//!    placeholder (`$n` on Postgres, `?n` on SQLite) carried on a
//!    [`PlanStep::Dml`](crate::render::step::PlanStep::Dml) `binds` vector, NEVER
//!    string-interpolated. So a value containing a quote / semicolon / comment
//!    cannot change the *shape* of the statement on either backend (the
//!    bind-safety property). The expression renderer ([`render_expr_bound`]) walks
//!    the closed AST and appends a placeholder for each [`Expr::Literal`].
//!
//! 2. **Batched backfill** (`assemble_backfill`). The existing
//!    [`BackfillSpec`](crate::model::backfill::BackfillSpec) executor (PG `backfill.rs`)
//!    consumes a `set_clause` / `filter` SQL *string* (it assembles a windowed
//!    `UPDATE … WHERE cursor > $last … AND (<filter>)` and guard-checks the WHOLE
//!    statement). A backfill expression references the row's own columns and is
//!    paged, so it cannot carry positional binds the way a one-shot statement can.
//!    Here the renderer ([`render_expr_inline`]) emits a SQL string in which a
//!    `Literal` is an INLINE SQL literal (numeric verbatim; a string single-quoted
//!    with `''` doubling — the canonical escape the guard's real-parser deny-list
//!    then re-validates). The assembled `UPDATE` is guard-checked by the executor
//!    before any batch runs, so the inline path inherits the same parse-time
//!    confinement the rest of the engine relies on.
//!
//! # Identifier safety
//!
//! Every identifier (table, column) is validated as a bare
//! `[A-Za-z_][A-Za-z0-9_]*` identifier and double-quoted with `"` doubling
//! ([`quote_ident`]). A schema-qualified or otherwise malformed identifier is
//! rejected at assemble time — an injection attempt through an identifier slot
//! cannot reach the database. On **Postgres** the table is qualified to the project
//! schema (`"schema"."table"`) so the resolved relation is always the project's
//! own; on **SQLite** the table lives in the connection's `main` database (the app
//! file) and is referenced UNqualified, matching the engine's SQLite DDL.
//!
//! # Portability boundary
//!
//! - `insert { onConflict }` renders natively on **Postgres**; on **SQLite** it is
//!   a hard authoring error ([`DmlError::OnConflictNotPortable`], surfaced as
//!   `dialect_scope = PgOnly` / `UNSUPPORTED { kind: "op" }`) — there is NO raw
//!   route (property A) and we never silently drop the conflict clause.
//! - A **batched** `backfill` targets the `BackfillSpec` executor, PORTABLE on
//!   BOTH backends: PG via the
//!   writable-CTE windowed `UPDATE` (`backfill.rs`), SQLite via the batched
//!   per-batch-txn executor (`apply::backend::sqlite::backfill_sql`). The inline
//!   `set`/`filter` differ per dialect (the `c.fn.splitPart` lowering,
//!   NULL-skipping `concatWs`); the `BackfillSpec` shape is uniform.
//!
//! # The shared SQLite-DML module seam
//!
//! The SQLite numbered `?n` placeholder emission lives in [`sqlite_placeholder`]
//! (called through [`placeholder`]), the SINGLE place the one-shot DML assembler
//! and the batched-backfill SQLite executor both emit positional
//! placeholders — so the two paths never fork a divergent copy of the `?n`-binding
//! logic (ONE SQLite-DML-assembly module). The transport-safe bind mirror
//! ([`crate::apply::backend::sqlite::actor::SqliteBind`]) is likewise the single
//! value-binding path the SQLite executor uses.

use std::collections::BTreeMap;

use crate::render::renderer::{Capability, DialectSupports};
use crate::schema::query::SqlDialect;

use crate::model::expr::{
    AggFunc, BinaryOp, Duration, Expr, ExtractField, PgExtractField, ScalarFn, SynthFn, UnaryOp,
};
use crate::model::ir::{IrScalar, IrValue};
use crate::render::step::BindValue;

/// A failure assembling a DML op into a statement (template + binds, or a backfill
/// spec). Distinct from the structural [`crate::model::validate::AuthoringError`]
/// (which gates the expression AST *before* assembly): this carries the
/// assembler-level rejections — a malformed identifier, an empty insert, an
/// expression node the renderer cannot lower, and the two SQLite portability
/// boundaries (`onConflict`, batched backfill).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmlError {
    /// An identifier (table / column) is not a bare `[A-Za-z_][A-Za-z0-9_]*`
    /// identifier — empty, schema-qualified, or containing characters outside the
    /// safe set. Rejected before any SQL is assembled.
    #[error("invalid identifier for {what}: {value:?} (must be a bare [A-Za-z_][A-Za-z0-9_]* identifier)")]
    InvalidIdentifier {
        /// Which slot was invalid (`"table"` / `"column"`).
        what: &'static str,
        /// The offending value.
        value: String,
    },
    /// An `insert` carried no columns, or a row whose arity does not match the
    /// column list. A zero-column / ragged insert is malformed on both dialects.
    #[error("malformed insert into {table:?}: {reason}")]
    MalformedInsert {
        /// The target table.
        table: String,
        /// What was wrong.
        reason: String,
    },
    /// An `update` / `backfill` whose `set` map is empty — nothing to assign.
    #[error("malformed {op} into {table:?}: empty `set` (a transform must assign at least one column)")]
    EmptySet {
        /// The op kind (`"update"` / `"backfill"`).
        op: &'static str,
        /// The target table.
        table: String,
    },
    /// The closed-AST expression renderer cannot lower a node (an unsupported /
    /// out-of-policy shape that the structural validator should have rejected
    /// first — this is the assembler's fail-closed backstop, never a silent
    /// emission). Carries a description of the offending node.
    #[error("cannot render expression node ({0}) — the structural validator must reject it before assembly")]
    UnrenderableExpr(String),
    /// `insert { onConflict }` on a **SQLite** target. PG `ON CONFLICT … DO UPDATE`
    /// and SQLite upsert clauses are incompatible and there is no raw route
    /// (property A), so `onConflict` is PG-only — a hard authoring error on SQLite
    /// (`dialect_scope = PgOnly`), not a silently-dropped conflict clause.
    #[error(
        "insert into {table:?} carries `onConflict`, which is PostgreSQL-only — SQLite \
         has no portable upsert and there is no raw route; restructure as separate \
         insert + update, or mark the migration dialect_scope=PgOnly (PG-only)"
    )]
    OnConflictNotPortable {
        /// The target table.
        table: String,
    },
    /// A single `insert` assembled more bind parameters than the wire protocol
    /// admits (PostgreSQL caps a statement at 65535 positional parameters; the
    /// `Bind` message length is a `u16`). Reject at assemble time with a bounded
    /// error rather than emitting a statement the driver fails mid-flight. Splitting
    /// the insert into chunks touches the executor / atomicity boundary and is
    /// deliberately out of scope here.
    #[error(
        "insert into {table:?} assembles {count} bind parameters, over the {max} \
         protocol limit; split the rows into smaller batches"
    )]
    TooManyBinds {
        /// The target table.
        table: String,
        /// The assembled bind count.
        count: usize,
        /// The protocol ceiling.
        max: usize,
    },
}

/// The maximum number of positional bind parameters a single statement may carry
/// (PostgreSQL `Bind` parameter count is a `u16`).
pub(crate) const MAX_BIND_PARAMS: usize = 65535;

/// Validate a bare SQL identifier and double-quote it (`"` → `""`). The ONLY
/// identifier-emission path the assembler uses — a schema-qualified / malformed
/// name is rejected, so an injection through an identifier slot cannot reach the
/// DB. Bare-identifier validation mirrors [`crate::model::backfill::BackfillSpec`].
fn quote_ident(what: &'static str, ident: &str) -> Result<String, DmlError> {
    quote_ident_for_dialect(what, ident, SqlDialect::Postgres)
}

pub(crate) fn quote_ident_for_dialect(
    what: &'static str,
    ident: &str,
    dialect: SqlDialect,
) -> Result<String, DmlError> {
    let ok = !ident.is_empty()
        && ident.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(DmlError::InvalidIdentifier { what, value: ident.to_string() });
    }
    Ok(escape_quote_ident_for_dialect(ident, dialect))
}

/// Public-in-crate wrapper for author-supplied bare identifiers. Trigger-body
/// rendering needs the same strict table/column/name gate as the DML assembler.
pub(crate) fn quote_bare_ident(what: &'static str, ident: &str) -> Result<String, DmlError> {
    quote_ident(what, ident)
}

pub(crate) fn quote_bare_ident_for_dialect(
    what: &'static str,
    ident: &str,
    dialect: SqlDialect,
) -> Result<String, DmlError> {
    quote_ident_for_dialect(what, ident, dialect)
}

/// A render-seam rejection of an **engine-supplied identifier** (project schema,
/// migrator role, meta schema, …) — the single fail-closed gate every engine
/// quoting seam routes through ([`quote_ident_checked`]). Distinct from
/// [`DmlError::InvalidIdentifier`], which gates *author-supplied* bare
/// identifiers with the strict `[A-Za-z_][A-Za-z0-9_]*` rule; this gate is for
/// names the engine itself produces (a UUIDv7 schema carries `-`, so the strict
/// rule cannot apply), and only refuses the two bytes that double-quote escaping
/// cannot neutralise: an empty string and a NUL byte. Each module maps it to its
/// own local error variant so the fail-closed message stays honest per surface.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("engine-supplied identifier is not quotable ({reason}): {value:?}")]
pub struct IdentQuoteError {
    /// Why the identifier could not be quoted (`"empty"` / `"contains NUL"`).
    pub reason: &'static str,
    /// The offending value.
    pub value: String,
}

/// The ONE canonical render seam for an **engine-supplied identifier** — the
/// project schema, the migrator role, the meta schema, a derived trigger name,
/// etc. Every engine quoting helper (`author` / `backfill` / `role` / `journal`
/// / `dml`) routes through this so all seams are **byte-identical** AND
/// **uniformly self-defending**.
///
/// Unlike a bare authored identifier ([`quote_ident`]), an engine-supplied name
/// is NOT a bare `[A-Za-z_][A-Za-z0-9_]*` ident — under the Confined posture the
/// project schema is the app id (a `UUIDv7` carrying `-`). So it is emitted
/// escape-and-quote: double an embedded `"`, wrap in `"`.
///
/// The name is never author-supplied, but this seam still fails closed rather
/// than trust the caller: it refuses an empty string and any value carrying a
/// NUL byte — the one byte that `"`-doubling cannot neutralise (PG rejects NUL
/// inside an identifier outright). Everything else (including `"`) is rendered
/// safely by escaping, **byte-identically** to a bare
/// `format!("\"{}\"", x.replace('"', "\"\""))`.
pub(crate) fn quote_ident_checked(ident: &str) -> Result<String, IdentQuoteError> {
    quote_ident_checked_for_dialect(ident, SqlDialect::Postgres)
}

pub(crate) fn quote_ident_checked_for_dialect(
    ident: &str,
    dialect: SqlDialect,
) -> Result<String, IdentQuoteError> {
    if ident.is_empty() {
        return Err(IdentQuoteError { reason: "empty", value: ident.to_string() });
    }
    if ident.contains('\0') {
        return Err(IdentQuoteError { reason: "contains NUL", value: ident.to_string() });
    }
    Ok(escape_quote_ident_for_dialect(ident, dialect))
}

/// The ONE raw double-quote escape primitive for the whole crate: double every
/// embedded `"`, wrap in `"`. This is the *single physical home* of the
/// `replace('"', "\"\"")` byte-logic — every identifier-quoting seam in
/// `zero-migrate` routes through it, either DIRECTLY (the author-boundary
/// helpers, whose input is already gated by an upstream `validate_ident`:
/// `declarative` / `shadow` / `expand_contract` / `precondition`'s structured-check
/// `quote_ident` / `render::lower` / `apply::backend::sqlite`) or via the fail-closed engine
/// wrapper [`quote_ident_checked`] (every **engine-supplied** identifier render
/// seam — project schema, migrator role, meta schema, recovery index name — in
/// `conn` / `executor` / `precondition` / `baseline` /
/// `author` / `backfill` / `role` / `journal`). Centralising it keeps every render
/// seam byte-identical and makes the "no remaining bare escape seam" claim
/// *structurally* true — enforced by [`no_bare_escape_seam_outside_dml`] below.
///
/// It is infallible by construction: double-quote escaping neutralises every byte
/// EXCEPT the empty string and a NUL (which PG rejects in an identifier outright).
/// Callers that handle **engine-supplied** identifiers (project schema, migrator
/// role, meta schema) MUST therefore route through [`quote_ident_checked`], which
/// adds the empty/NUL fail-closed gate on top of this primitive — so EVERY
/// engine-identifier render seam in the crate is uniformly self-defending, not
/// just the five (`dml`/`role`/`author`/`backfill`/`journal`) that originally
/// adopted the wrapper. Callers quoting author identifiers already gated upstream
/// (`validate_ident`) may call this directly.
pub(crate) fn escape_quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

pub(crate) fn escape_quote_ident_for_dialect(ident: &str, dialect: SqlDialect) -> String {
    crate::render::renderer::renderer(dialect).quote_ident(ident)
}

/// Qualify a validated bare table name for the target dialect.
///
/// - **Postgres**: `"schema"."table"` — the project schema is engine-supplied
///   (never author-supplied) and the migrator's `search_path` is pinned to it, but
///   we qualify explicitly so the resolved relation is unambiguously the project's.
/// - **SQLite**: the table lives in the connection's `main` database (the app
///   file is `main`) — there is NO schema namespace, and a
///   `"schema"."table"` reference would resolve to a non-existent attached DB. So
///   the SQLite form is the BARE quoted table, matching the engine's UNqualified
///   SQLite DDL emission (the same property the createTable lowering relies on).
fn qualify_table(
    project_schema: &str,
    dialect: SqlDialect,
    table: &str,
) -> Result<String, DmlError> {
    crate::render::renderer::renderer(dialect).qualify_table(project_schema, table)
}

/// Map an [`IrScalar`] to a [`BindValue`] for native parameter binding — the
/// one-shot DML path. The IR numeric domain (`Int` `|v| < 2^53` / decimal-string)
/// carries through verbatim; `Bytes` is carried as its canonical base64 text (the
/// PG/SQLite executors bind it as text and the column type coerces it). NEVER
/// inlined.
fn scalar_to_bind(s: &IrScalar) -> BindValue {
    match s {
        IrScalar::Null => BindValue::Null,
        IrScalar::Bool(b) => BindValue::Bool(*b),
        IrScalar::Int(i) => BindValue::Int(*i),
        IrScalar::Decimal(d) => BindValue::Decimal(d.clone()),
        IrScalar::Str(s) => BindValue::Text(s.clone()),
        // Carry bytes as canonical base64 text; the column type coerces. (The IR
        // already round-trips Bytes through canonical base64.)
        IrScalar::Bytes(b) => {
            use base64::Engine as _;
            BindValue::Text(base64::engine::general_purpose::STANDARD.encode(b))
        }
    }
}

/// The dialect-specific positional placeholder for the `n`-th (1-based) bind.
/// Postgres uses `$n`; SQLite uses `?n` (the numbered form, so the binds stay
/// positional and reusable). This is the SINGLE placeholder-emission point the
/// one-shot assembler and the batched-backfill SQLite executor both call —
/// the shared SQLite-DML seam. Postgres routes through the same fn for one
/// consistent counter.
#[must_use]
pub fn placeholder(dialect: SqlDialect, n: usize) -> String {
    match dialect {
        SqlDialect::Postgres => format!("${n}"),
        SqlDialect::Sqlite => sqlite_placeholder(n),
        SqlDialect::Mysql => "?".to_string(),
    }
}

/// The SQLite numbered placeholder (`?n`) — factored out as the shared-module
/// entry the batched-backfill SQLite executor reuses for per-batch statement
/// assembly, so the two paths bind values through ONE path.
#[must_use]
pub fn sqlite_placeholder(n: usize) -> String {
    format!("?{n}")
}

/// Render a single inline SQL literal for the **backfill** string path
/// ([`render_expr_inline`]). Numeric/bool literals print verbatim; a string is
/// single-quoted with `''` doubling (the canonical SQL escape) — the assembled
/// statement is then guard-checked by the real Postgres parser before any batch
/// runs, so a hostile literal cannot alter the statement shape past the parser.
/// NULL renders as the keyword. Bytes are not inline-renderable in the backfill
/// path (they have no portable inline literal form across both dialects) → a
/// fail-closed error.
/// Render a SQL string literal using the canonical single-quote escape.
#[must_use]
pub(crate) fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub(crate) fn inline_literal(s: &IrScalar) -> Result<String, DmlError> {
    Ok(match s {
        IrScalar::Null => "NULL".to_string(),
        IrScalar::Bool(b) => if *b { "TRUE".to_string() } else { "FALSE".to_string() },
        IrScalar::Int(i) => i.to_string(),
        IrScalar::Decimal(d) => d.clone(),
        IrScalar::Str(s) => sql_string_literal(s),
        IrScalar::Bytes(_) => {
            return Err(DmlError::UnrenderableExpr(
                "a bytes literal is not inline-renderable in a backfill transform \
                 (use a one-shot update with a native bind for byte values)"
                    .to_string(),
            ));
        }
    })
}

fn pg_text_literal(s: &str, what: &'static str) -> Result<String, DmlError> {
    if s.is_empty() {
        return Err(DmlError::UnrenderableExpr(format!("{what} must be non-empty")));
    }
    if s.contains('\0') {
        return Err(DmlError::UnrenderableExpr(format!("{what} contains a NUL byte")));
    }
    Ok(format!("{}::text", sql_string_literal(s)))
}

fn in_list_text_literal(s: &str, what: &'static str) -> Result<String, DmlError> {
    if s.is_empty() {
        return Err(DmlError::UnrenderableExpr(format!("{what} must be non-empty")));
    }
    if s.contains('\0') {
        return Err(DmlError::UnrenderableExpr(format!("{what} contains a NUL byte")));
    }
    Ok(sql_string_literal(s))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InListScalarKind {
    Text,
    Number,
    Bool,
    Null,
}

fn in_list_scalar_kind(elem: &IrScalar) -> Result<InListScalarKind, DmlError> {
    Ok(match elem {
        IrScalar::Str(_) => InListScalarKind::Text,
        IrScalar::Int(_) | IrScalar::Decimal(_) => InListScalarKind::Number,
        IrScalar::Bool(_) => InListScalarKind::Bool,
        IrScalar::Null => InListScalarKind::Null,
        IrScalar::Bytes(_) => {
            return Err(DmlError::UnrenderableExpr(
                "inList elements must be string, number, boolean, or null; bytes are not allowed"
                    .to_string(),
            ));
        }
    })
}

fn homogeneous_in_list_kind(elems: &[IrScalar]) -> Result<Option<InListScalarKind>, DmlError> {
    let mut kind = None;
    for elem in elems {
        let elem_kind = in_list_scalar_kind(elem)?;
        if let Some(first) = kind {
            if elem_kind != first {
                return Err(DmlError::UnrenderableExpr(
                    "inList elements must be homogeneous".to_string(),
                ));
            }
        } else {
            kind = Some(elem_kind);
        }
    }
    Ok(kind)
}

fn render_in_list_elem_pg(elem: &IrScalar) -> Result<String, DmlError> {
    Ok(match elem {
        IrScalar::Str(s) => pg_text_literal(s, "inList element")?,
        IrScalar::Int(i) => i.to_string(),
        IrScalar::Decimal(d) => d.clone(),
        IrScalar::Bool(b) => {
            if *b { "TRUE".to_string() } else { "FALSE".to_string() }
        }
        IrScalar::Null => "NULL".to_string(),
        IrScalar::Bytes(_) => {
            return Err(DmlError::UnrenderableExpr(
                "inList elements must be string, number, boolean, or null; bytes are not allowed"
                    .to_string(),
            ));
        }
    })
}

fn render_in_list_elem_portable(elem: &IrScalar) -> Result<String, DmlError> {
    Ok(match elem {
        IrScalar::Str(s) => in_list_text_literal(s, "inList element")?,
        IrScalar::Int(i) => i.to_string(),
        IrScalar::Decimal(d) => d.clone(),
        IrScalar::Bool(b) => {
            if *b { "TRUE".to_string() } else { "FALSE".to_string() }
        }
        IrScalar::Null => "NULL".to_string(),
        IrScalar::Bytes(_) => {
            return Err(DmlError::UnrenderableExpr(
                "inList elements must be string, number, boolean, or null; bytes are not allowed"
                    .to_string(),
            ));
        }
    })
}

fn render_in_list(
    expr: &str,
    elems: &[IrScalar],
    negated: bool,
    dialect: SqlDialect,
) -> Result<String, DmlError> {
    if elems.is_empty() {
        return Ok(if negated { "TRUE" } else { "FALSE" }.to_string());
    }
    let kind = homogeneous_in_list_kind(elems)?.expect("non-empty list has kind");
    let joiner = if matches!(kind, InListScalarKind::Text) { ", " } else { "," };
    match dialect {
        SqlDialect::Postgres => {
            let rendered: Result<Vec<_>, _> =
                elems.iter().map(render_in_list_elem_pg).collect();
            let (cmp, quantifier) = if negated { ("<>", "ALL") } else { ("=", "ANY") };
            Ok(format!(
                "({expr} {cmp} {quantifier} (ARRAY[{}]))",
                rendered?.join(joiner)
            ))
        }
        SqlDialect::Sqlite | SqlDialect::Mysql => {
            let rendered = elems
                .iter()
                .map(render_in_list_elem_portable)
                .collect::<Result<Vec<_>, _>>()?;
            let op = if negated { "NOT IN" } else { "IN" };
            Ok(format!("({expr} {op} ({}))", rendered.join(joiner)))
        }
    }
}

fn render_pg_regex_match(
    expr: &str,
    pattern: &str,
    dialect: SqlDialect,
) -> Result<String, DmlError> {
    match dialect {
        SqlDialect::Postgres => Ok(format!(
            "({expr} ~ {})",
            pg_text_literal(pattern, "PG regex pattern")?
        )),
        SqlDialect::Mysql => Ok(format!(
            "({expr} REGEXP {})",
            in_list_text_literal(pattern, "regex pattern")?
        )),
        SqlDialect::Sqlite => Err(DmlError::UnrenderableExpr(
            "regex is not supported on SQLite (no stock REGEXP); use dialect({...}) to port"
                .to_string(),
        )),
    }
}

fn render_extract_field(field: ExtractField) -> &'static str {
    match field {
        ExtractField::Year => "year",
        ExtractField::Month => "month",
        ExtractField::Day => "day",
        ExtractField::Hour => "hour",
        ExtractField::Minute => "minute",
        ExtractField::Dow => "dow",
    }
}

fn render_extract(field: ExtractField, expr: &str, dialect: SqlDialect) -> Result<String, DmlError> {
    Ok(match dialect {
        SqlDialect::Postgres => format!("EXTRACT({} FROM {expr})", render_extract_field(field)),
        SqlDialect::Sqlite => {
            let fmt = match field {
                ExtractField::Year => "%Y",
                ExtractField::Month => "%m",
                ExtractField::Day => "%d",
                ExtractField::Hour => "%H",
                ExtractField::Minute => "%M",
                ExtractField::Dow => "%w",
            };
            format!("CAST(strftime('{fmt}', {expr}) AS INTEGER)")
        }
        SqlDialect::Mysql => match field {
            ExtractField::Dow => format!("(DAYOFWEEK({expr}) - 1)"),
            _ => format!(
                "EXTRACT({} FROM {expr})",
                render_extract_field(field).to_ascii_uppercase()
            ),
        },
    })
}

fn render_pg_extract_field(field: PgExtractField) -> &'static str {
    match field {
        PgExtractField::Second => "second",
        PgExtractField::Doy => "doy",
        PgExtractField::Epoch => "epoch",
        PgExtractField::Quarter => "quarter",
        PgExtractField::Week => "week",
        PgExtractField::Isodow => "isodow",
        PgExtractField::Isoyear => "isoyear",
        PgExtractField::Century => "century",
        PgExtractField::Decade => "decade",
        PgExtractField::Millennium => "millennium",
        PgExtractField::Microseconds => "microseconds",
        PgExtractField::Milliseconds => "milliseconds",
        PgExtractField::Timezone => "timezone",
        PgExtractField::TimezoneHour => "timezone_hour",
        PgExtractField::TimezoneMinute => "timezone_minute",
    }
}

fn render_pg_extract(
    field: PgExtractField,
    expr: &str,
    dialect: SqlDialect,
) -> Result<String, DmlError> {
    if !matches!(dialect, SqlDialect::Postgres) {
        return Err(DmlError::UnrenderableExpr(
            "PG EXTRACT is PostgreSQL-only".to_string(),
        ));
    }
    Ok(format!("EXTRACT({} FROM {expr})", render_pg_extract_field(field)))
}

fn render_pg_interval_literal(duration: &Duration, dialect: SqlDialect) -> Result<String, DmlError> {
    if !matches!(dialect, SqlDialect::Postgres) {
        return Err(DmlError::UnrenderableExpr(
            "PG interval literal is PostgreSQL-only".to_string(),
        ));
    }

    let mut parts = Vec::new();
    for (value, singular, plural) in [
        (duration.years, "year", "years"),
        (duration.months, "month", "months"),
        (duration.days, "day", "days"),
        (duration.hours, "hour", "hours"),
        (duration.minutes, "minute", "minutes"),
        (duration.seconds, "second", "seconds"),
    ] {
        if let Some(value) = value {
            let unit = if value == 1 || value == -1 { singular } else { plural };
            parts.push(format!("{value} {unit}"));
        }
    }
    if parts.is_empty() {
        return Err(DmlError::UnrenderableExpr(
            "PG interval duration must include at least one field".to_string(),
        ));
    }
    Ok(format!("INTERVAL {}", sql_string_literal(&parts.join(" "))))
}

/// The SQL spelling of a binary operator (the method↔node table). `Concat` is
/// `||` — the one place PG/SQLite NULL semantics agree.
fn binary_op_sql(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Eq => "=",
        BinaryOp::Ne => "<>",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::And => "AND",
        BinaryOp::Or => "OR",
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Concat => "||",
    }
}

/// Render a binary operation to SQL, **dialect-aware**. Every operator is a
/// portable infix EXCEPT string concatenation on MySQL: MySQL's `||` is *logical
/// OR* (not concat, absent the non-default `PIPES_AS_CONCAT` sql_mode), so a
/// `Concat` rendered as `a || b` there would silently corrupt to a boolean. MySQL
/// concatenation is the `CONCAT(a, b)` function. PG and SQLite use the `||`
/// operator, where it is genuinely concatenation.
fn render_binop(op: BinaryOp, l: &str, r: &str, dialect: SqlDialect) -> String {
    if matches!(op, BinaryOp::Concat) && matches!(dialect, SqlDialect::Mysql) {
        format!("CONCAT({l}, {r})")
    } else {
        format!("({} {} {})", l, binary_op_sql(op), r)
    }
}

/// Render the portable `distinctFrom` NULL-safe inequality node, **dialect-aware**.
/// PG and SQLite both support the standard `IS DISTINCT FROM` operator directly.
/// MySQL has NO `IS DISTINCT FROM`, so the engine owns the lowering to
/// `NOT (<l> <=> <r>)` — `<=>` is MySQL's NULL-safe equality operator, so its
/// negation is exactly the "distinct from" (NULL-aware inequality) predicate.
fn render_distinct_from(l: &str, r: &str, dialect: SqlDialect) -> String {
    match dialect {
        SqlDialect::Mysql => format!("(NOT ({l} <=> {r}))"),
        SqlDialect::Postgres | SqlDialect::Sqlite => format!("({l} IS DISTINCT FROM {r})"),
    }
}

/// The SQL spelling of an allow-listed named scalar function. These
/// are the provably-identical cross-dialect scalars (same name + semantics on PG
/// and SQLite), so the spelling is dialect-neutral.
fn scalar_fn_sql(f: ScalarFn) -> &'static str {
    match f {
        ScalarFn::Coalesce => "coalesce",
        ScalarFn::Nullif => "nullif",
        ScalarFn::Lower => "lower",
        ScalarFn::Upper => "upper",
        ScalarFn::Trim => "trim",
        ScalarFn::Length => "length",
        ScalarFn::Abs => "abs",
        // Portable scalar fns — identical spelling on PG/SQLite/MySQL.
        // `Mod` renders as the `%` OPERATOR, special-cased in `render_scalar_fn_call`
        // (SQLite has no `mod()` fn); this fallback name is never reached for it.
        ScalarFn::Mod => "mod",
        ScalarFn::Round => "round",
        ScalarFn::Floor => "floor",
        ScalarFn::Ceil => "ceil",
        ScalarFn::Substr => "substr",
        ScalarFn::Replace => "replace",
        // VENDOR scalars. `current_user` is a reserved keyword
        // rendered WITHOUT parens — the FnCall render arms special-case it; this
        // spelling is the fallback name.
        ScalarFn::CurrentSetting => "current_setting",
        ScalarFn::CurrentUser => "current_user",
    }
}

/// Render an allow-listed [`ScalarFn`] call from its already-rendered argument
/// fragments. Most are `<name>(<args>)`; the VENDOR `CurrentUser` is a bare
/// reserved keyword with NO parens (PG rejects `current_user()`).
fn render_scalar_fn_call(f: ScalarFn, args: &[String], dialect: SqlDialect) -> String {
    match f {
        ScalarFn::CurrentUser => "current_user".to_string(),
        // `mod` renders as the `%` OPERATOR — NOT a `mod(...)` call — because
        // SQLite has no `mod()` SQL function (`%` is universal on PG/SQLite/MySQL).
        // `args.join(" % ")` wrapped in parens is byte-identical to `(<a> % <b>)`
        // for the 2-arg case the builder produces, and never index-panics on a
        // malformed hand-crafted arity.
        ScalarFn::Mod => format!("({})", args.join(" % ")),
        // The portable `length()` intent is CHARACTER length (PG + SQLite
        // `length(text)`). MySQL's `LENGTH()` is *byte* length — wrong for any
        // multibyte string — so MySQL must use `CHAR_LENGTH()`.
        ScalarFn::Length if matches!(dialect, SqlDialect::Mysql) => {
            format!("char_length({})", args.join(", "))
        }
        _ => format!("{}({})", scalar_fn_sql(f), args.join(", ")),
    }
}

/// The lower-cased SQL name of an [`AggFunc`].
fn agg_fn_sql(f: AggFunc) -> &'static str {
    match f {
        AggFunc::Count => "count",
        AggFunc::Sum => "sum",
        AggFunc::Avg => "avg",
        AggFunc::Min => "min",
        AggFunc::Max => "max",
        AggFunc::StringAgg => "string_agg",
        AggFunc::ArrayAgg => "array_agg",
        AggFunc::BoolAnd => "bool_and",
        AggFunc::BoolOr => "bool_or",
    }
}

/// Render an aggregate application from already-rendered argument fragments.
/// `arg = None` (only `Count`) → `count(*)`; `StringAgg` renders its
/// required delimiter as the second argument.
fn render_agg(
    f: AggFunc,
    arg_sql: Option<&str>,
    delimiter_sql: Option<&str>,
    distinct: bool,
) -> Result<String, DmlError> {
    let name = agg_fn_sql(f);
    if matches!(f, AggFunc::StringAgg) {
        let (Some(a), Some(d)) = (arg_sql, delimiter_sql) else {
            return Err(DmlError::UnrenderableExpr(
                "string_agg requires an argument and delimiter".to_string(),
            ));
        };
        let prefix = if distinct { "DISTINCT " } else { "" };
        return Ok(format!("{name}({prefix}{a}, {d})"));
    }
    if delimiter_sql.is_some() {
        return Err(DmlError::UnrenderableExpr(
            "aggregate delimiter is only valid for string_agg".to_string(),
        ));
    }
    match arg_sql {
        None if matches!(f, AggFunc::ArrayAgg | AggFunc::BoolAnd | AggFunc::BoolOr) => {
            Err(DmlError::UnrenderableExpr(format!(
                "{} requires an argument",
                agg_fn_sql(f)
            )))
        }
        None => Ok(format!("{name}(*)")),
        Some(a) if distinct => Ok(format!("{name}(DISTINCT {a})")),
        Some(a) => Ok(format!("{name}({a})")),
    }
}

/// The portable cast-target SQL type per dialect. `bytes` is `BYTEA` on
/// PG / `BLOB` on SQLite; the rest share spelling.
fn cast_target_sql(target: crate::model::expr::CastTarget, dialect: SqlDialect) -> &'static str {
    crate::render::renderer::renderer(dialect).cast_target(target)
}

/// Render a `c.fn.concatWs(delim, a, b, …)` per dialect: PG `concat_ws`;
/// SQLite has no `concat_ws`, so it lowers to a NULL-skipping fold over `||` using
/// the pinned, portable shape. The args are already rendered fragments.
fn render_concat_ws(rendered: &[String], dialect: SqlDialect) -> String {
    crate::render::renderer::renderer(dialect).render_concat_ws(rendered)
}

/// The MAX literal part index `c.fn.splitPart` admits — the O(2ⁿ) inline-unroll
/// bound (`~17 KB` at `n=8`). MUST equal
/// [`crate::model::validate::SPLIT_PART_MAX_N`] — the validator gates the envelope and
/// this renderer assumes it; a fixture pins their equality.
pub(crate) const SPLIT_PART_MAX_N: i64 = crate::model::validate::SPLIT_PART_MAX_N;

/// Render the PINNED `c.fn.splitPart(col, d, n)` per dialect, given the
/// already-rendered `col_sql` fragment and the raw delimiter + `n` IR args. The
/// renderer is **dialect-aware** because the portability ENVELOPE is dialect-gated
/// (mirroring `validate::check_split_part`, which returns early on a Postgres
/// target): PG's native `split_part` is multi-char-delimiter-capable and takes any
/// positive `n`, so on a Postgres target a `dialect_scope=PgOnly` out-of-envelope
/// splitPart is a first-class, renderable node — NOT a hard error.
///
/// On BOTH dialects the GRAMMAR is enforced (the renderer's fail-closed backstop):
/// the delimiter must be a non-empty string literal and `n` a positive integer
/// literal — a non-literal delim/n, a non-string delim, an empty delim, or `n ≤ 0`
/// is unrenderable everywhere. The widening on PG is of the envelope (multi-char /
/// non-ASCII delim, `n > SPLIT_PART_MAX_N`), never the grammar.
///
/// - **Postgres:** `split_part(<col>, '<d>', n)` — verbatim for ANY literal
///   delimiter (single-quotes `''`-escaped) and any positive literal `n`. Returns
///   `''` past the token count.
/// - **SQLite:** restricted to the proven envelope — a SINGLE-ASCII-byte delimiter
///   and `1 ≤ n ≤ SPLIT_PART_MAX_N` — then the engine-owned `instr`/`substr`
///   unroll, byte-identical to PG `split_part` against SQLite 3.51.2. The delimiter
///   is required to be a single ASCII byte precisely because UTF-8 never embeds an
///   ASCII byte inside a multibyte sequence, so the byte-wise `instr` scan finds
///   exactly the boundaries PG's character-wise `split_part` does (the byte-identity
///   proof). Append a sentinel delimiter (`cur₀ = col || 'd'`) so every token is
///   delimiter-terminated, then walk the boundary to literal depth `n`:
///   `curᵢ = substr(curᵢ₋₁, instr(curᵢ₋₁, 'd') + 1)` for `i = 1 … n−1`, and the
///   result is `substr(cur_{n-1}, 1, instr(cur_{n-1}, 'd') − 1)`. The unroll
///   references `curᵢ₋₁` twice per level, so it grows O(2ⁿ) (~17 KB at `n=8`) — the
///   reason `n` is capped at 8. The delimiter is emitted as a single-quoted SQL
///   literal (`''`-escaped), NOT a bind: it is an engine-pinned constant of the
///   pinned expression, and the authorizer (with `instr` allow-listed) vets the
///   whole statement.
fn render_split_part(
    col_sql: &str,
    delim_arg: &Expr,
    n_arg: &Expr,
    dialect: SqlDialect,
) -> Result<String, DmlError> {
    // GRAMMAR (both dialects): a string-literal delimiter, a positive integer
    // literal n. These are unrenderable on EITHER backend.
    let delim = match delim_arg {
        Expr::Literal { value: IrScalar::Str(s) } if !s.is_empty() => s.as_str(),
        Expr::Literal { value: IrScalar::Str(_) } => {
            return Err(DmlError::UnrenderableExpr(
                "c.fn.splitPart delimiter must be a non-empty string literal".to_string(),
            ));
        }
        other => {
            return Err(DmlError::UnrenderableExpr(format!(
                "c.fn.splitPart delimiter must be a string literal \
                 (a runtime/computed delimiter is not renderable); got {other:?}"
            )));
        }
    };
    let n = match n_arg {
        Expr::Literal { value: IrScalar::Int(n) } if *n >= 1 => *n,
        Expr::Literal { value: IrScalar::Int(n) } => {
            return Err(DmlError::UnrenderableExpr(format!(
                "c.fn.splitPart part index n must be a positive integer literal; got {n}"
            )));
        }
        other => {
            return Err(DmlError::UnrenderableExpr(format!(
                "c.fn.splitPart part index n must be a positive integer literal; got {other:?}"
            )));
        }
    };

    crate::render::renderer::renderer(dialect).render_split_part(col_sql, delim, n)
}

/// Select the [`Expr::Dialectal`] leg to render for `dialect`: the
/// target dialect's OWN leg if present, else the `default` leg. Returns a borrow
/// of the chosen leg. This is the one leg-selection rule shared by both the bound
/// and inline render paths.
///
/// A `Dialectal` with neither an own leg nor a `default` for the target is
/// UNREACHABLE here because [`crate::model::validate`] refuses it per-target
/// (`EXPR_NOT_PORTABLE`) before assembly — but the seam is fail-closed
/// defensively: it returns [`DmlError::UnrenderableExpr`] rather than silently
/// dropping the value.
fn select_dialect_leg<'a>(
    dialect: SqlDialect,
    default: &'a Option<Box<Expr>>,
    pg: &'a Option<Box<Expr>>,
    sqlite: &'a Option<Box<Expr>>,
    mysql: &'a Option<Box<Expr>>,
) -> Result<&'a Expr, DmlError> {
    let own = match dialect {
        SqlDialect::Postgres => pg,
        SqlDialect::Sqlite => sqlite,
        SqlDialect::Mysql => mysql,
    };
    own.as_deref().or_else(|| default.as_deref()).ok_or_else(|| {
        DmlError::UnrenderableExpr(format!(
            "dialect() has no leg for the {dialect:?} target and no default — the \
             structural validator must refuse this before assembly"
        ))
    })
}

/// A bind accumulator carried through the parameterized render walk: it owns the
/// running placeholder counter (1-based, dialect-specific) and the ordered
/// [`BindValue`] list.
struct BindCtx {
    dialect: SqlDialect,
    binds: Vec<BindValue>,
}

impl BindCtx {
    fn new(dialect: SqlDialect) -> Self {
        Self { dialect, binds: Vec::new() }
    }

    /// Append a bound scalar and return its dialect placeholder.
    fn push_bind(&mut self, b: BindValue) -> String {
        self.binds.push(b);
        placeholder(self.dialect, self.binds.len())
    }
}

/// Render a closed-AST [`Expr`] to a parameterized SQL fragment, appending a
/// native bind for every [`Expr::Literal`] (the one-shot DML path). A `ColRef`
/// renders to its quoted identifier; a `Literal` to a placeholder; operators /
/// functions / casts to their SQL spelling. The bind safety property:
/// statement structure is fixed by the AST shape, never by a literal's content.
fn render_expr_bound(expr: &Expr, ctx: &mut BindCtx) -> Result<String, DmlError> {
    Ok(match expr {
        Expr::ColRef { name, table } => match table {
            // Qualified ref (`c("orders", "id")`): `<quoted table>.<quoted col>`,
            // both halves through the same per-dialect identifier quoting.
            Some(t) => format!(
                "{}.{}",
                quote_ident_for_dialect("table", t, ctx.dialect)?,
                quote_ident_for_dialect("column", name, ctx.dialect)?
            ),
            None => quote_ident_for_dialect("column", name, ctx.dialect)?,
        },
        Expr::Literal { value } => ctx.push_bind(scalar_to_bind(value)),
        Expr::BinOp { op, lhs, rhs } => {
            let l = render_expr_bound(lhs, ctx)?;
            let r = render_expr_bound(rhs, ctx)?;
            render_binop(*op, &l, &r, ctx.dialect)
        }
        Expr::UnaryOp { op, operand } => {
            let o = render_expr_bound(operand, ctx)?;
            render_unary(*op, &o, ctx.dialect)
        }
        Expr::Case { branches, r#else } => {
            let mut s = String::from("CASE");
            for b in branches {
                let c = render_expr_bound(&b.when, ctx)?;
                let r = render_expr_bound(&b.then, ctx)?;
                s.push_str(&format!(" WHEN {c} THEN {r}"));
            }
            if let Some(e) = r#else {
                let e = render_expr_bound(e, ctx)?;
                s.push_str(&format!(" ELSE {e}"));
            }
            s.push_str(" END");
            s
        }
        Expr::FnCall { r#fn, args } => {
            let mut rs = Vec::with_capacity(args.len());
            for a in args {
                rs.push(render_expr_bound(a, ctx)?);
            }
            render_scalar_fn_call(*r#fn, &rs, ctx.dialect)
        }
        Expr::FnSynth { r#fn, args } => render_synth_bound(*r#fn, args, ctx)?,
        Expr::Cast { operand, target } => {
            let o = render_expr_bound(operand, ctx)?;
            format!("CAST({o} AS {})", cast_target_sql(*target, ctx.dialect))
        }
        Expr::Between { operand, low, high } => {
            let o = render_expr_bound(operand, ctx)?;
            let lo = render_expr_bound(low, ctx)?;
            let hi = render_expr_bound(high, ctx)?;
            format!("({o} BETWEEN {lo} AND {hi})")
        }
        Expr::Like { operand, pattern } => {
            let o = render_expr_bound(operand, ctx)?;
            let p = render_expr_bound(pattern, ctx)?;
            format!("({o} LIKE {p})")
        }
        Expr::DistinctFrom { left, right } => {
            let l = render_expr_bound(left, ctx)?;
            let r = render_expr_bound(right, ctx)?;
            render_distinct_from(&l, &r, ctx.dialect)
        }
        Expr::Agg { func, arg, delimiter, distinct } => {
            let a = match arg {
                Some(e) => Some(render_expr_bound(e, ctx)?),
                None => None,
            };
            let d = match delimiter {
                Some(e) => Some(render_expr_bound(e, ctx)?),
                None => None,
            };
            render_agg(*func, a.as_deref(), d.as_deref(), *distinct)?
        }
        Expr::InList { expr, elems, negated } => {
            let e = render_expr_bound(expr, ctx)?;
            render_in_list(&e, elems, *negated, ctx.dialect)?
        }
        Expr::PgRegexMatch { expr, pattern } => {
            let e = render_expr_bound(expr, ctx)?;
            render_pg_regex_match(&e, pattern, ctx.dialect)?
        }
        Expr::PgColumnSize { expr } => {
            if !matches!(ctx.dialect, SqlDialect::Postgres) {
                return Err(DmlError::UnrenderableExpr(
                    "pg_column_size is PostgreSQL-only".to_string(),
                ));
            }
            let e = render_expr_bound(expr, ctx)?;
            format!("pg_column_size({e})")
        }
        Expr::Extract { field, from } => {
            let e = render_expr_bound(from, ctx)?;
            render_extract(*field, &e, ctx.dialect)?
        }
        Expr::PgExtract { field, from } => {
            let e = render_expr_bound(from, ctx)?;
            render_pg_extract(*field, &e, ctx.dialect)?
        }
        Expr::PgInterval { duration } => render_pg_interval_literal(duration, ctx.dialect)?,
        Expr::Dialectal { default, pg, sqlite, mysql } => {
            let leg = select_dialect_leg(ctx.dialect, default, pg, sqlite, mysql)?;
            render_expr_bound(leg, ctx)?
        }
    })
}

/// Render a `FnSynth` in the parameterized path. `concatWs` and `splitPart` lower
/// per dialect via the portable-helper renderers; `now` /
/// `genRandomUuid` render to the apply-time DB scalar.
fn render_synth_bound(f: SynthFn, args: &[Expr], ctx: &mut BindCtx) -> Result<String, DmlError> {
    match f {
        SynthFn::ConcatWs => {
            let mut rs = Vec::with_capacity(args.len());
            for a in args {
                rs.push(render_expr_bound(a, ctx)?);
            }
            Ok(render_concat_ws(&rs, ctx.dialect))
        }
        SynthFn::Now => Ok(crate::render::renderer::renderer(ctx.dialect).synth_now()),
        SynthFn::GenRandomUuid => Ok(crate::render::renderer::renderer(ctx.dialect).synth_uuid()),
        SynthFn::SplitPart => {
            // splitPart(col, delim, n): the column arg may itself be a ColRef or an
            // in-AST sub-expression — render it (binding any nested Literals), then
            // extract the pinned single-ASCII delim + literal n envelope. The
            // delim/n are engine-pinned constants of the lowering, NOT binds.
            if args.len() != 3 {
                return Err(DmlError::UnrenderableExpr(format!(
                    "c.fn.splitPart takes exactly (column, delim, n); got {} args",
                    args.len()
                )));
            }
            let col_sql = render_expr_bound(&args[0], ctx)?;
            render_split_part(&col_sql, &args[1], &args[2], ctx.dialect)
        }
    }
}

fn render_value_bound(value: &IrValue, ctx: &mut BindCtx) -> Result<String, DmlError> {
    match value {
        IrValue::Scalar(s) => Ok(ctx.push_bind(scalar_to_bind(s))),
        IrValue::Expr(e) => render_expr_bound(e, ctx),
    }
}

/// Render a unary op around an already-rendered operand, dialect-aware.
fn render_unary(op: UnaryOp, operand: &str, dialect: SqlDialect) -> String {
    match op {
        UnaryOp::Not => format!("(NOT {operand})"),
        UnaryOp::IsNull => format!("({operand} IS NULL)"),
        UnaryOp::IsNotNull => format!("({operand} IS NOT NULL)"),
        // SQLite has no native boolean type (values are 0/1) and rejects the
        // `IS TRUE` / `IS FALSE` predicates at apply — render them as `= 1` / `= 0`
        // there. PG and MySQL both support the standard spelling.
        UnaryOp::IsTrue if matches!(dialect, SqlDialect::Sqlite) => format!("({operand} = 1)"),
        UnaryOp::IsFalse if matches!(dialect, SqlDialect::Sqlite) => format!("({operand} = 0)"),
        UnaryOp::IsTrue => format!("({operand} IS TRUE)"),
        UnaryOp::IsFalse => format!("({operand} IS FALSE)"),
    }
}

/// Render a closed-AST [`Expr`] to an INLINE SQL string (the backfill path). A
/// `ColRef` is its quoted identifier; a `Literal` is an inline SQL literal
/// ([`inline_literal`], guard-revalidated downstream); operators / functions /
/// casts render the same SQL spelling as the bound path. NO binds — the backfill
/// executor pages the statement and cannot carry positional binds.
pub(crate) fn render_expr_inline(expr: &Expr, dialect: SqlDialect) -> Result<String, DmlError> {
    render_expr_inline_with_col(expr, dialect, &|name| {
        quote_ident_for_dialect("column", name, dialect)
    })
}

pub(crate) fn render_value_inline(value: &IrValue, dialect: SqlDialect) -> Result<String, DmlError> {
    match value {
        IrValue::Scalar(s) => inline_literal(s),
        IrValue::Expr(e) => render_expr_inline(e, dialect),
    }
}

pub(crate) fn render_expr_inline_with_col<F>(
    expr: &Expr,
    dialect: SqlDialect,
    col_ref: &F,
) -> Result<String, DmlError>
where
    F: Fn(&str) -> Result<String, DmlError>,
{
    Ok(match expr {
        Expr::ColRef { name, table } => match table {
            // Qualified ref: quote the table via the per-dialect identifier
            // quoter; delegate the column to the caller-supplied `col_ref` closure.
            Some(t) => format!(
                "{}.{}",
                quote_ident_for_dialect("table", t, dialect)?,
                col_ref(name)?
            ),
            None => col_ref(name)?,
        },
        Expr::Literal { value } => inline_literal(value)?,
        Expr::BinOp { op, lhs, rhs } => {
            let l = render_expr_inline_with_col(lhs, dialect, col_ref)?;
            let r = render_expr_inline_with_col(rhs, dialect, col_ref)?;
            render_binop(*op, &l, &r, dialect)
        }
        Expr::UnaryOp { op, operand } => render_unary(
            *op,
            &render_expr_inline_with_col(operand, dialect, col_ref)?,
            dialect,
        ),
        Expr::Case { branches, r#else } => {
            let mut s = String::from("CASE");
            for b in branches {
                let c = render_expr_inline_with_col(&b.when, dialect, col_ref)?;
                let r = render_expr_inline_with_col(&b.then, dialect, col_ref)?;
                s.push_str(&format!(" WHEN {c} THEN {r}"));
            }
            if let Some(e) = r#else {
                s.push_str(&format!(
                    " ELSE {}",
                    render_expr_inline_with_col(e, dialect, col_ref)?
                ));
            }
            s.push_str(" END");
            s
        }
        Expr::FnCall { r#fn, args } => {
            let rs: Result<Vec<_>, _> =
                args.iter()
                    .map(|a| render_expr_inline_with_col(a, dialect, col_ref))
                    .collect();
            render_scalar_fn_call(*r#fn, &rs?, dialect)
        }
        Expr::FnSynth { r#fn, args } => match r#fn {
            SynthFn::SplitPart => {
                // The column arg renders inline; the delim/n are engine-pinned
                // constants of the lowering, extracted raw (NOT inline-rendered
                // as generic literals). The backfill (inline) path is exactly where
                // the hero split lands.
                if args.len() != 3 {
                    return Err(DmlError::UnrenderableExpr(format!(
                        "c.fn.splitPart takes exactly (column, delim, n); got {} args",
                        args.len()
                    )));
                }
                let col_sql = render_expr_inline_with_col(&args[0], dialect, col_ref)?;
                render_split_part(&col_sql, &args[1], &args[2], dialect)?
            }
            SynthFn::ConcatWs => {
                let rs: Result<Vec<_>, _> =
                    args.iter()
                        .map(|a| render_expr_inline_with_col(a, dialect, col_ref))
                        .collect();
                render_concat_ws(&rs?, dialect)
            }
            SynthFn::Now => crate::render::renderer::renderer(dialect).synth_now(),
            SynthFn::GenRandomUuid => crate::render::renderer::renderer(dialect).synth_uuid(),
        },
        Expr::Cast { operand, target } => {
            format!(
                "CAST({} AS {})",
                render_expr_inline_with_col(operand, dialect, col_ref)?,
                cast_target_sql(*target, dialect)
            )
        }
        Expr::Between { operand, low, high } => {
            let o = render_expr_inline_with_col(operand, dialect, col_ref)?;
            let lo = render_expr_inline_with_col(low, dialect, col_ref)?;
            let hi = render_expr_inline_with_col(high, dialect, col_ref)?;
            format!("({o} BETWEEN {lo} AND {hi})")
        }
        Expr::Like { operand, pattern } => {
            let o = render_expr_inline_with_col(operand, dialect, col_ref)?;
            let p = render_expr_inline_with_col(pattern, dialect, col_ref)?;
            format!("({o} LIKE {p})")
        }
        Expr::DistinctFrom { left, right } => {
            let l = render_expr_inline_with_col(left, dialect, col_ref)?;
            let r = render_expr_inline_with_col(right, dialect, col_ref)?;
            render_distinct_from(&l, &r, dialect)
        }
        Expr::Agg { func, arg, delimiter, distinct } => {
            let a = match arg {
                Some(e) => Some(render_expr_inline_with_col(e, dialect, col_ref)?),
                None => None,
            };
            let d = match delimiter {
                Some(e) => Some(render_expr_inline_with_col(e, dialect, col_ref)?),
                None => None,
            };
            render_agg(*func, a.as_deref(), d.as_deref(), *distinct)?
        }
        Expr::InList { expr, elems, negated } => {
            let e = render_expr_inline_with_col(expr, dialect, col_ref)?;
            render_in_list(&e, elems, *negated, dialect)?
        }
        Expr::PgRegexMatch { expr, pattern } => {
            let e = render_expr_inline_with_col(expr, dialect, col_ref)?;
            render_pg_regex_match(&e, pattern, dialect)?
        }
        Expr::PgColumnSize { expr } => {
            if !matches!(dialect, SqlDialect::Postgres) {
                return Err(DmlError::UnrenderableExpr(
                    "pg_column_size is PostgreSQL-only".to_string(),
                ));
            }
            format!("pg_column_size({})", render_expr_inline_with_col(expr, dialect, col_ref)?)
        }
        Expr::Extract { field, from } => {
            let e = render_expr_inline_with_col(from, dialect, col_ref)?;
            render_extract(*field, &e, dialect)?
        }
        Expr::PgExtract { field, from } => {
            let e = render_expr_inline_with_col(from, dialect, col_ref)?;
            render_pg_extract(*field, &e, dialect)?
        }
        Expr::PgInterval { duration } => render_pg_interval_literal(duration, dialect)?,
        Expr::Dialectal { default, pg, sqlite, mysql } => {
            let leg = select_dialect_leg(dialect, default, pg, sqlite, mysql)?;
            render_expr_inline_with_col(leg, dialect, col_ref)?
        }
    })
}

/// **VENDOR** — render a CLOSED [`Expr`] predicate to an inline Postgres SQL
/// fragment for the vendor `CREATE POLICY` `USING`/`WITH CHECK` and `CREATE
/// TRIGGER` `WHEN` clauses. These DDL positions carry NO binds
/// (a policy/trigger predicate is part of the catalog definition, not a
/// parameterized statement), so the inline renderer is the right seam — a
/// `ColRef` is its quoted identifier, a `Literal` an inline SQL literal, and the
/// VENDOR `c.fn.currentSetting`/`currentUser` scalars render their PG form. The
/// whole rendered statement is then `pg_query`-parsed by the guard, so the inline
/// literals are re-validated by the real parser before any apply. PG dialect only
/// (vendor predicates are `PgOnly`).
///
/// # Errors
/// [`DmlError::UnrenderableExpr`] for an expression node that has no inline form
/// (e.g. a `bytes` literal).
pub(crate) fn render_predicate_pg(expr: &Expr) -> Result<String, DmlError> {
    render_expr_inline(expr, SqlDialect::Postgres)
}

/// Render a CLOSED trigger/view/check predicate to inline SQLite SQL.
pub(crate) fn render_predicate_sqlite(expr: &Expr) -> Result<String, DmlError> {
    render_expr_inline(expr, SqlDialect::Sqlite)
}

/// The `onConflict` facet of an `insert`. PG-only.
#[derive(Debug, Clone, PartialEq)]
pub struct OnConflict {
    /// The conflict-target columns (`ON CONFLICT (cols)`).
    pub columns: Vec<String>,
    /// `Some` SET assignments ⇒ `DO UPDATE SET …`; `None` ⇒ `DO NOTHING`.
    pub do_update: Option<BTreeMap<String, IrValue>>,
}

/// The assembled one-shot DML statement: the placeholder template + ordered binds.
/// Fed straight into [`PlanStep::Dml`](crate::render::step::PlanStep::Dml).
#[derive(Debug, Clone, PartialEq)]
pub struct AssembledDml {
    /// The placeholder SQL (`$n`/`?n` — never an inlined value).
    pub template: String,
    /// The ordered native binds.
    pub binds: Vec<BindValue>,
}

/// Assemble an `insert` op into a parameterized one-shot statement. Every
/// value is a native bind; `onConflict` renders on PG and is a hard error on
/// SQLite ([`DmlError::OnConflictNotPortable`]).
///
/// # Errors
/// [`DmlError`] on a malformed identifier / empty-or-ragged insert / a SQLite
/// `onConflict`.
pub fn assemble_insert(
    project_schema: &str,
    dialect: SqlDialect,
    table: &str,
    columns: &[String],
    rows: &[Vec<IrValue>],
    on_conflict: Option<&OnConflict>,
) -> Result<AssembledDml, DmlError> {
    if columns.is_empty() {
        return Err(DmlError::MalformedInsert {
            table: table.to_string(),
            reason: "no columns".to_string(),
        });
    }
    if rows.is_empty() {
        return Err(DmlError::MalformedInsert {
            table: table.to_string(),
            reason: "no rows".to_string(),
        });
    }
    let qtable = qualify_table(project_schema, dialect, table)?;
    let qcols: Result<Vec<_>, _> =
        columns.iter().map(|c| quote_ident_for_dialect("column", c, dialect)).collect();
    let qcols = qcols?;

    let mut ctx = BindCtx::new(dialect);
    let mut value_groups: Vec<String> = Vec::with_capacity(rows.len());
    for (ri, row) in rows.iter().enumerate() {
        if row.len() != columns.len() {
            return Err(DmlError::MalformedInsert {
                table: table.to_string(),
                reason: format!(
                    "row {ri} has {} value(s) but {} column(s) were named",
                    row.len(),
                    columns.len()
                ),
            });
        }
        let placeholders: Result<Vec<String>, DmlError> =
            row.iter().map(|v| render_value_bound(v, &mut ctx)).collect();
        let placeholders = placeholders?;
        value_groups.push(format!("({})", placeholders.join(", ")));
    }

    let mut template = format!(
        "INSERT INTO {qtable} ({}) VALUES {}",
        qcols.join(", "),
        value_groups.join(", ")
    );

    if let Some(oc) = on_conflict {
        if !dialect.supports(Capability::InsertOnConflictClause) {
            return Err(DmlError::OnConflictNotPortable { table: table.to_string() });
        }
        template.push_str(&render_on_conflict(oc, &mut ctx)?);
    }

    if ctx.binds.len() > MAX_BIND_PARAMS {
        return Err(DmlError::TooManyBinds {
            table: table.to_string(),
            count: ctx.binds.len(),
            max: MAX_BIND_PARAMS,
        });
    }

    Ok(AssembledDml { template, binds: ctx.binds })
}

/// Render the PG `ON CONFLICT (cols) DO {NOTHING|UPDATE SET …}` tail. The
/// `do_update` SET values are native binds (appended to the running counter).
fn render_on_conflict(oc: &OnConflict, ctx: &mut BindCtx) -> Result<String, DmlError> {
    if oc.columns.is_empty() {
        return Err(DmlError::MalformedInsert {
            table: "<onConflict>".to_string(),
            reason: "onConflict carries no target columns".to_string(),
        });
    }
    let qcols: Result<Vec<_>, _> =
        oc.columns.iter().map(|c| quote_ident_for_dialect("column", c, ctx.dialect)).collect();
    let target = format!("ON CONFLICT ({})", qcols?.join(", "));
    match &oc.do_update {
        None => Ok(format!(" {target} DO NOTHING")),
        Some(set) => {
            if set.is_empty() {
                return Ok(format!(" {target} DO NOTHING"));
            }
            // BTreeMap ⇒ deterministic column order (canonical).
            let mut assigns = Vec::with_capacity(set.len());
            for (col, val) in set {
                let qc = quote_ident_for_dialect("column", col, ctx.dialect)?;
                let ph = render_value_bound(val, ctx)?;
                assigns.push(format!("{qc} = {ph}"));
            }
            Ok(format!(" {target} DO UPDATE SET {}", assigns.join(", ")))
        }
    }
}

/// Assemble a one-shot `update` op (no `batch`) into a parameterized statement.
/// `set` RHS values render through [`render_value_bound`] and the optional `where`
/// renders through [`render_expr_bound`], so scalar set values and expression
/// literals are native binds. Portable on both backends.
///
/// # Errors
/// [`DmlError`] on a malformed identifier / empty `set` / an unrenderable node.
pub fn assemble_update(
    project_schema: &str,
    dialect: SqlDialect,
    table: &str,
    set: &BTreeMap<String, IrValue>,
    r#where: Option<&Expr>,
) -> Result<AssembledDml, DmlError> {
    if set.is_empty() {
        return Err(DmlError::EmptySet { op: "update", table: table.to_string() });
    }
    let qtable = qualify_table(project_schema, dialect, table)?;
    let mut ctx = BindCtx::new(dialect);
    // BTreeMap ⇒ deterministic, canonical assignment order.
    let mut assigns = Vec::with_capacity(set.len());
    for (col, rhs) in set {
        let qc = quote_ident_for_dialect("column", col, dialect)?;
        let r = render_value_bound(rhs, &mut ctx)?;
        assigns.push(format!("{qc} = {r}"));
    }
    let mut template = format!("UPDATE {qtable} SET {}", assigns.join(", "));
    if let Some(pred) = r#where {
        let w = render_expr_bound(pred, &mut ctx)?;
        template.push_str(&format!(" WHERE {w}"));
    }
    Ok(AssembledDml { template, binds: ctx.binds })
}

/// Assemble a `del` op into a parameterized `DELETE`. The mandatory `where`
/// renders through [`render_expr_bound`]. An optional `limit` is enforced via a
/// primary-rowid subquery on BOTH backends (`ctid` on PG, `rowid` on SQLite):
/// PG never supported `DELETE … LIMIT n`, and the bundled rusqlite is built
/// WITHOUT `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`, so a bare `DELETE … LIMIT ?n`
/// is a hard syntax error there too. The limit-free form is a plain `DELETE …
/// WHERE …`.
///
/// # Errors
/// [`DmlError`] on a malformed identifier / an unrenderable predicate.
pub fn assemble_delete(
    project_schema: &str,
    dialect: SqlDialect,
    table: &str,
    r#where: &Expr,
    limit: Option<u64>,
) -> Result<AssembledDml, DmlError> {
    let qtable = qualify_table(project_schema, dialect, table)?;
    let mut ctx = BindCtx::new(dialect);
    let w = render_expr_bound(r#where, &mut ctx)?;
    let template = match (dialect, limit) {
        (_, None) => format!("DELETE FROM {qtable} WHERE {w}"),
        // Neither backend can take a bare `DELETE … WHERE … LIMIT n` portably:
        // the bundled rusqlite (features=["bundled"]) does NOT compile
        // `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`, so `DELETE … LIMIT ?n` is a hard
        // syntax error at apply; PG never supported the form. Both therefore lower
        // to a primary-rowid subquery (`rowid` on SQLite, `ctid` on PG) that picks
        // exactly `limit` matching rows. The limit binds natively.
        (SqlDialect::Sqlite, Some(n)) => {
            let ph = ctx.push_bind(BindValue::Int(i64::try_from(n).unwrap_or(i64::MAX)));
            format!(
                "DELETE FROM {qtable} WHERE rowid IN \
                 (SELECT rowid FROM {qtable} WHERE {w} LIMIT {ph})"
            )
        }
        (SqlDialect::Postgres, Some(n)) => {
            let ph = ctx.push_bind(BindValue::Int(i64::try_from(n).unwrap_or(i64::MAX)));
            format!(
                "DELETE FROM {qtable} WHERE ctid IN \
                 (SELECT ctid FROM {qtable} WHERE {w} LIMIT {ph})"
            )
        }
        (SqlDialect::Mysql, Some(n)) => {
            let ph = ctx.push_bind(BindValue::Int(i64::try_from(n).unwrap_or(i64::MAX)));
            format!("DELETE FROM {qtable} WHERE {w} LIMIT {ph}")
        }
    };
    Ok(AssembledDml { template, binds: ctx.binds })
}

/// The rendered backfill clauses (the SQL strings the
/// [`BackfillSpec`](crate::model::backfill::BackfillSpec) carries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillClauses {
    /// The `SET` body (e.g. `"normalized" = lower("raw")`).
    pub set_clause: String,
    /// The optional extra `WHERE` conjunct.
    pub filter: Option<String>,
}

/// Assemble a `backfill` op's `set` / `filter` into the inline SQL strings the
/// [`BackfillSpec`](crate::model::backfill::BackfillSpec) executor consumes. Renders for
/// EITHER dialect: the inline transform is dialect-rendered (the
/// `c.fn.splitPart` lowering, NULL-skipping `concatWs`), and the PG (`backfill.rs`)
/// or SQLite (`apply::backend::sqlite::backfill_sql`) executor consumes the result.
///
/// # Errors
/// [`DmlError`] on a malformed identifier / empty `set` / an unrenderable node.
pub fn assemble_backfill_clauses(
    dialect: SqlDialect,
    table: &str,
    set: &BTreeMap<String, IrValue>,
    filter: Option<&Expr>,
) -> Result<BackfillClauses, DmlError> {
    if set.is_empty() {
        return Err(DmlError::EmptySet { op: "backfill", table: table.to_string() });
    }
    // BTreeMap ⇒ canonical order.
    let mut assigns = Vec::with_capacity(set.len());
    for (col, rhs) in set {
        let qc = quote_ident_for_dialect("column", col, dialect)?;
        let r = render_value_inline(rhs, dialect)?;
        assigns.push(format!("{qc} = {r}"));
    }
    let set_clause = assigns.join(", ");
    let filter = match filter {
        Some(f) => Some(render_expr_inline(f, dialect)?),
        None => None,
    };
    Ok(BackfillClauses { set_clause, filter })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::expr::Expr;

    const SCHEMA: &str = "app_proj";

    // ---- the ONE shared engine identifier seam --------------------------------

    /// `quote_ident_checked` fails CLOSED on the two bytes `"`-doubling
    /// cannot neutralise: an empty string and a NUL byte. Without the guard, the
    /// peer seams (`author`/`backfill`/`role`/`journal`) would
    /// have ACCEPTED a NUL and emitted `"a\0b"`.
    #[test]
    fn quote_ident_checked_fails_closed_on_empty_and_nul() {
        assert!(quote_ident_checked("").is_err(), "empty must fail closed");
        assert!(quote_ident_checked("a\0b").is_err(), "NUL must fail closed");
        assert_eq!(quote_ident_checked("").unwrap_err().reason, "empty");
        assert_eq!(quote_ident_checked("a\0b").unwrap_err().reason, "contains NUL");
    }

    /// For any non-empty / non-NUL identifier the output is
    /// byte-identical to the bare `format!("\"{}\"", x.replace('"', "\"\""))` the
    /// peers used, including a quote-bearing schema (the dml goldens stay green).
    #[test]
    fn quote_ident_checked_is_byte_identical_to_bare_format() {
        for s in ["app_proj", "019efd94-1a2b-7000-8000-000000000000", "a\"b", "\"\""] {
            assert_eq!(
                quote_ident_checked(s).unwrap(),
                format!("\"{}\"", s.replace('"', "\"\"")),
                "byte-identity for {s:?}"
            );
        }
        // explicit quote-doubling spot-check
        assert_eq!(quote_ident_checked("a\"b").unwrap(), "\"a\"\"b\"");
    }

    /// The four engine peer seams (`author`/`backfill`/`role`/`journal`)
    /// now all route through `quote_ident_checked`, so they emit BYTE-IDENTICAL
    /// output for the same quote-bearing schema (the "uniform render seam"
    /// requirement). The peers wrap the shared helper, so comparing each to the
    /// canonical helper proves the uniformity for all five.
    #[test]
    fn all_engine_seams_render_uniformly() {
        let schema = "ap\"p"; // a quote-bearing engine schema
        let canonical = quote_ident_checked(schema).unwrap();
        // author (infallible-on-valid wrapper) — maps to its own error on failure.
        assert_eq!(crate::plan::author::quote_ident_for_test(schema).unwrap(), canonical);
        assert_eq!(crate::apply::role::quote_ident_for_test(schema).unwrap(), canonical);
        assert_eq!(crate::apply::journal::quote_ident_for_test(schema).unwrap(), canonical);
        // …and they fail closed uniformly on a NUL too.
        assert!(crate::plan::author::quote_ident_for_test("a\0b").is_err());
        assert!(crate::apply::role::quote_ident_for_test("a\0b").is_err());
        assert!(crate::apply::journal::quote_ident_for_test("a\0b").is_err());
    }

    /// STRUCTURAL enforcement of the "no remaining bare
    /// `format!`/`replace` escape seam" claim. The raw `"` → `""` escape logic
    /// (`replace('"', "\"\"")`) must live in EXACTLY one physical home —
    /// [`escape_quote_ident`] in this module — and nowhere else in the crate
    /// source. Every other quoting seam routes through it (directly for infallible
    /// author-validated helpers, or via [`quote_ident_checked`] for the fail-closed
    /// engine-identifier surfaces).
    ///
    /// The `"` → `""` escape logic must NOT recur inline across sites such as
    /// `executor` / `precondition` / `baseline` / `expand_contract` / `shadow` /
    /// `declarative` / `db` / `render::lower` / `apply::backend::sqlite` — only
    /// `dml.rs` (the helper + this test's own needle strings) may contain the
    /// pattern.
    #[test]
    fn no_bare_escape_seam_outside_dml() {
        use std::path::Path;
        // The exact escape-call byte-pattern. We scan for the `replace` call that
        // doubles a double-quote; the ONLY legitimate occurrences live in dml.rs.
        let needle = ['r', 'e', 'p', 'l', 'a', 'c', 'e']
            .iter()
            .collect::<String>()
            + "('\"', \"\\\"\\\"\")";
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![src_root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                // dml.rs is the single sanctioned home (helper + this test).
                if path.file_name().and_then(|n| n.to_str()) == Some("dml.rs") {
                    continue;
                }
                // The `schema/` module tree is the
                // schema-authority DDL layer with its OWN identifier-quoting
                // primitive (`schema::query::quote_ident`). That escape is a
                // distinct module layer from this engine's render seam — the
                // structural invariant this test enforces is about the RENDER
                // layer (`render::*` / `apply::*` / `command::*`), not the
                // schema kernel — so the `schema/` subtree is exempt.
                if path.components().any(|c| c.as_os_str() == "schema") {
                    continue;
                }
                let body = std::fs::read_to_string(&path).expect("read src file");
                if body.contains(&needle) {
                    offenders.push(path.strip_prefix(&src_root).unwrap().display().to_string());
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "bare `\"`-escape seam found outside dml.rs — route these through \
             dml::escape_quote_ident / quote_ident_checked: {offenders:?}"
        );
    }

    /// STRUCTURAL proof that the "every engine-supplied
    /// identifier render seam fail-closes" contract is TRUE, not just true for the
    /// five seams (`dml`/`role`/`author`/`backfill`/`journal`) that first adopted
    /// the wrapper. The infallible primitive [`escape_quote_ident`] must NEVER be
    /// handed an **engine-supplied** identifier (project schema / migrator role /
    /// meta schema) — those must route through [`quote_ident_checked`] so they
    /// fail closed on empty / NUL. We scan the crate source for the give-away
    /// byte-patterns (`escape_quote_ident(&cfg.pg.meta_schema)`,
    /// `escape_quote_ident(&cfg.project_schema)`, `escape_quote_ident(role)`,
    /// `escape_quote_ident(&exec_cfg.pg.meta_schema)`) — every such site is an
    /// engine-identifier seam that must NOT use the infallible escaper.
    ///
    /// Engine-identifier sites such as `precondition.rs` (project_schema + role),
    /// `executor.rs` (role + meta_schema ×4 + project_schema + recovery index),
    /// `baseline.rs` (meta_schema), and
    /// `db.rs::search_path_clause` (project/platform/extension schemas) must NOT
    /// feed an engine identifier to `escape_quote_ident`.
    ///
    /// **SCOPE — this is a PER-SITE regression pin, NOT a general invariant.** It
    /// only catches the exact call-site *spellings* in `needles` above (the give-away
    /// `escape_quote_ident(&cfg.…)` / `(role)` byte-patterns). A future engine-identifier
    /// seam bound to a *differently-named* variable — e.g.
    /// `let s = &cfg.pg.meta_schema; escape_quote_ident(s)` — would slip past this scan
    /// undetected. The broader, spelling-independent guarantee that NO bare `"`-escape
    /// seam exists outside `dml.rs` is held by `no_bare_escape_seam` (above); this test
    /// complements it by naming the specific engine-identifier sites and proving they
    /// route through the fail-closed wrapper. When adding a new engine-identifier render
    /// seam, add its spelling to `needles` here.
    #[test]
    fn no_engine_identifier_uses_the_infallible_escaper() {
        use std::path::Path;
        // The engine-supplied identifier argument patterns. `quote_ident_checked`
        // takes the SAME args; the infallible `escape_quote_ident` must not.
        let esc = ['e', 's', 'c', 'a', 'p', 'e', '_', 'q', 'u', 'o', 't', 'e', '_', 'i', 'd', 'e', 'n', 't']
            .iter()
            .collect::<String>();
        let needles = [
            format!("{esc}(&cfg.pg.meta_schema)"),
            format!("{esc}(&cfg.project_schema)"),
            format!("{esc}(&exec_cfg.pg.meta_schema)"),
            format!("{esc}(&exec_cfg.project_schema)"),
            format!("{esc}(role)"),
        ];
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders: Vec<String> = Vec::new();
        let mut stack = vec![src_root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read_dir src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                // dml.rs holds only the helper + this test's needle literals.
                if path.file_name().and_then(|n| n.to_str()) == Some("dml.rs") {
                    continue;
                }
                let body = std::fs::read_to_string(&path).expect("read src file");
                for needle in &needles {
                    if body.contains(needle.as_str()) {
                        offenders.push(format!(
                            "{} ({needle})",
                            path.strip_prefix(&src_root).unwrap().display()
                        ));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "engine-supplied identifier handed to the INFALLIBLE escaper — route \
             through dml::quote_ident_checked so it fails closed on empty/NUL: {offenders:?}"
        );
    }

    fn lit_str(s: &str) -> Expr {
        Expr::lit(IrScalar::Str(s.to_string()))
    }
    fn lit_int(i: i64) -> Expr {
        Expr::lit(IrScalar::Int(i))
    }
    fn val(s: IrScalar) -> IrValue {
        IrValue::Scalar(s)
    }
    fn dml_expr(e: Expr) -> IrValue {
        IrValue::Expr(e)
    }

    // ── Concat is dialect-specific (regression: MySQL `||` is logical OR) ─────

    #[test]
    fn concat_renders_per_dialect_pg_sqlite_mysql() {
        // Regression guard: on MySQL, `||` is *logical OR*, so rendering `Concat`
        // as `a || b` there silently corrupts a string concat to a boolean. It
        // MUST render as `CONCAT(a, b)`. PG + SQLite keep the `||` operator.
        let expr = Expr::BinOp {
            op: BinaryOp::Concat,
            lhs: Box::new(Expr::col("first")),
            rhs: Box::new(Expr::col("last")),
        };

        let pg = render_expr_inline(&expr, SqlDialect::Postgres).unwrap();
        assert_eq!(pg, "(\"first\" || \"last\")", "PG uses the || concat operator");

        let sqlite = render_expr_inline(&expr, SqlDialect::Sqlite).unwrap();
        assert_eq!(sqlite, "(\"first\" || \"last\")", "SQLite uses the || concat operator");

        let mysql = render_expr_inline(&expr, SqlDialect::Mysql).unwrap();
        assert!(
            mysql.starts_with("CONCAT(") && !mysql.contains("||"),
            "MySQL MUST render Concat as CONCAT(...), never `||` (logical OR): got {mysql}"
        );
    }

    #[test]
    fn cast_renders_per_dialect_type_names() {
        use crate::model::expr::CastTarget;

        let cases = [
            (
                CastTarget::Int,
                "CAST(\"x\" AS integer)",
                "CAST(\"x\" AS integer)",
                "CAST(`x` AS signed)",
            ),
            (
                CastTarget::Bytes,
                "CAST(\"x\" AS bytea)",
                "CAST(\"x\" AS blob)",
                "CAST(`x` AS binary)",
            ),
            (
                CastTarget::Text,
                "CAST(\"x\" AS text)",
                "CAST(\"x\" AS text)",
                "CAST(`x` AS char)",
            ),
        ];

        for (target, pg, sqlite, mysql) in cases {
            let expr = Expr::Cast { operand: Box::new(Expr::col("x")), target };
            assert_eq!(render_expr_inline(&expr, SqlDialect::Postgres).unwrap(), pg);
            assert_eq!(render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(), sqlite);
            assert_eq!(render_expr_inline(&expr, SqlDialect::Mysql).unwrap(), mysql);
        }
    }

    // ── Qualified column refs (the join-ON fix) ──────────────────────────────

    /// A qualified `ColRef { table, name }` renders `<table>.<col>` with the SAME
    /// per-dialect identifier quoting as an unqualified ref: PG/SQLite double-quote
    /// each half, MySQL backticks each half. An unqualified ref is unchanged.
    #[test]
    fn qualified_colref_renders_dotted_per_dialect() {
        let qualified = Expr::col_qualified("users", "id");
        assert_eq!(
            render_expr_inline(&qualified, SqlDialect::Postgres).unwrap(),
            "\"users\".\"id\"",
            "PG qualifies with double-quoted table.col"
        );
        assert_eq!(
            render_expr_inline(&qualified, SqlDialect::Sqlite).unwrap(),
            "\"users\".\"id\"",
            "SQLite qualifies with double-quoted table.col"
        );
        assert_eq!(
            render_expr_inline(&qualified, SqlDialect::Mysql).unwrap(),
            "`users`.`id`",
            "MySQL qualifies with backtick-quoted table.col"
        );

        // Unqualified stays exactly as today — no table segment, no dot.
        let plain = Expr::col("id");
        assert_eq!(render_expr_inline(&plain, SqlDialect::Postgres).unwrap(), "\"id\"");
        assert_eq!(render_expr_inline(&plain, SqlDialect::Sqlite).unwrap(), "\"id\"");
        assert_eq!(render_expr_inline(&plain, SqlDialect::Mysql).unwrap(), "`id`");

        // The parameterized (bind) path mirrors the inline path for the ColRef arm.
        assert_eq!(
            render_expr_bound(&qualified, &mut BindCtx::new(SqlDialect::Postgres)).unwrap(),
            "\"users\".\"id\""
        );
        assert_eq!(
            render_expr_bound(&qualified, &mut BindCtx::new(SqlDialect::Mysql)).unwrap(),
            "`users`.`id`"
        );
        assert_eq!(
            render_expr_bound(&plain, &mut BindCtx::new(SqlDialect::Postgres)).unwrap(),
            "\"id\""
        );
    }

    #[test]
    fn length_is_char_length_on_mysql() {
        // Regression: MySQL LENGTH() is BYTE length; the portable length() intent
        // is CHARACTER length (PG/SQLite length()). MySQL MUST use CHAR_LENGTH().
        let expr = Expr::FnCall { r#fn: ScalarFn::Length, args: vec![Expr::col("name")] };
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Postgres).unwrap(),
            "length(\"name\")"
        );
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(),
            "length(\"name\")"
        );
        let mysql = render_expr_inline(&expr, SqlDialect::Mysql).unwrap();
        assert!(
            mysql.starts_with("char_length("),
            "MySQL length() must render as CHAR_LENGTH (LENGTH is byte length): got {mysql}"
        );
    }

    /// Portable scalar fns: `round`/`floor`/`ceil`/`substr`/`replace` all
    /// spell IDENTICALLY on PG, SQLite, and MySQL, so they render byte-identically
    /// on every dialect via the neutral `<name>(<args>)` path.
    #[test]
    fn portable_scalar_fns_render_identically_on_all_three() {
        let cases: &[(Expr, &str)] = &[
            (
                Expr::FnCall { r#fn: ScalarFn::Round, args: vec![Expr::col("x")] },
                "round(\"x\")",
            ),
            (
                Expr::FnCall {
                    r#fn: ScalarFn::Round,
                    args: vec![Expr::col("x"), Expr::lit(IrScalar::Int(2))],
                },
                "round(\"x\", 2)",
            ),
            (
                Expr::FnCall { r#fn: ScalarFn::Floor, args: vec![Expr::col("x")] },
                "floor(\"x\")",
            ),
            (
                Expr::FnCall { r#fn: ScalarFn::Ceil, args: vec![Expr::col("x")] },
                "ceil(\"x\")",
            ),
            (
                Expr::FnCall {
                    r#fn: ScalarFn::Substr,
                    args: vec![Expr::col("s"), Expr::lit(IrScalar::Int(1)), Expr::lit(IrScalar::Int(3))],
                },
                "substr(\"s\", 1, 3)",
            ),
            (
                Expr::FnCall {
                    r#fn: ScalarFn::Replace,
                    args: vec![
                        Expr::col("s"),
                        Expr::lit(IrScalar::Str("a".into())),
                        Expr::lit(IrScalar::Str("b".into())),
                    ],
                },
                "replace(\"s\", 'a', 'b')",
            ),
        ];
        for (expr, pg_sqlite_expect) in cases {
            // PG and SQLite quote identifiers with `"`; the fn spelling is identical.
            assert_eq!(
                &render_expr_inline(expr, SqlDialect::Postgres).unwrap(),
                pg_sqlite_expect,
                "PG render mismatch"
            );
            assert_eq!(
                &render_expr_inline(expr, SqlDialect::Sqlite).unwrap(),
                pg_sqlite_expect,
                "SQLite render mismatch"
            );
            // MySQL differs ONLY in identifier quoting (backticks); the fn name +
            // arg shape are identical (CEIL/SUBSTR are MySQL aliases).
            let mysql_expect = pg_sqlite_expect.replace('"', "`");
            assert_eq!(
                render_expr_inline(expr, SqlDialect::Mysql).unwrap(),
                mysql_expect,
                "MySQL render mismatch"
            );
        }
    }

    #[test]
    fn portable_extract_fields_render_equivalent_date_parts_on_all_three() {
        let cases = [
            (
                ExtractField::Year,
                "EXTRACT(year FROM \"ts\")",
                "CAST(strftime('%Y', \"ts\") AS INTEGER)",
                "EXTRACT(YEAR FROM `ts`)",
            ),
            (
                ExtractField::Month,
                "EXTRACT(month FROM \"ts\")",
                "CAST(strftime('%m', \"ts\") AS INTEGER)",
                "EXTRACT(MONTH FROM `ts`)",
            ),
            (
                ExtractField::Day,
                "EXTRACT(day FROM \"ts\")",
                "CAST(strftime('%d', \"ts\") AS INTEGER)",
                "EXTRACT(DAY FROM `ts`)",
            ),
            (
                ExtractField::Hour,
                "EXTRACT(hour FROM \"ts\")",
                "CAST(strftime('%H', \"ts\") AS INTEGER)",
                "EXTRACT(HOUR FROM `ts`)",
            ),
            (
                ExtractField::Minute,
                "EXTRACT(minute FROM \"ts\")",
                "CAST(strftime('%M', \"ts\") AS INTEGER)",
                "EXTRACT(MINUTE FROM `ts`)",
            ),
            (
                ExtractField::Dow,
                "EXTRACT(dow FROM \"ts\")",
                "CAST(strftime('%w', \"ts\") AS INTEGER)",
                "(DAYOFWEEK(`ts`) - 1)",
            ),
        ];

        for (field, pg, sqlite, mysql) in cases {
            let expr = Expr::Extract {
                field,
                from: Box::new(Expr::col("ts")),
            };
            assert_eq!(render_expr_inline(&expr, SqlDialect::Postgres).unwrap(), pg);
            assert_eq!(render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(), sqlite);
            assert_eq!(render_expr_inline(&expr, SqlDialect::Mysql).unwrap(), mysql);
        }
    }

    #[test]
    fn pg_extract_renders_only_on_postgres() {
        let expr = Expr::PgExtract {
            field: PgExtractField::Epoch,
            from: Box::new(Expr::col("ts")),
        };
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Postgres).unwrap(),
            "EXTRACT(epoch FROM \"ts\")"
        );
        for dialect in [SqlDialect::Sqlite, SqlDialect::Mysql] {
            let err = render_expr_inline(&expr, dialect).unwrap_err();
            assert!(
                err.to_string().contains("PostgreSQL-only"),
                "pgExtract must refuse {dialect:?}: {err}"
            );
        }

        let second = Expr::PgExtract {
            field: PgExtractField::Second,
            from: Box::new(Expr::col("ts")),
        };
        assert_eq!(
            render_expr_inline(&second, SqlDialect::Postgres).unwrap(),
            "EXTRACT(second FROM \"ts\")",
            "second stays PG-only because PG preserves fractional seconds"
        );
    }

    #[test]
    fn regex_match_renders_postgres_and_mysql_but_refuses_sqlite() {
        let expr = Expr::PgRegexMatch {
            expr: Box::new(Expr::col("name")),
            pattern: "^a$".to_string(),
        };
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Postgres).unwrap(),
            "(\"name\" ~ '^a$'::text)"
        );
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Mysql).unwrap(),
            "(`name` REGEXP '^a$')"
        );

        let err = render_expr_inline(&expr, SqlDialect::Sqlite).unwrap_err();
        assert!(
            err.to_string().contains("SQLite") && err.to_string().contains("REGEXP"),
            "SQLite regex must fail closed with a precise message: {err}"
        );
    }

    /// `c.fn.mod(a, b)` renders as the `%` OPERATOR — NOT `mod(...)` — on all three
    /// dialects. This is the one portable arithmetic fn whose spelling is an
    /// operator (SQLite has no `mod()` SQL function; `%` is universal).
    #[test]
    fn mod_renders_as_percent_operator_on_all_three() {
        let expr = Expr::FnCall {
            r#fn: ScalarFn::Mod,
            args: vec![Expr::col("n"), Expr::lit(IrScalar::Int(3))],
        };
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Postgres).unwrap(),
            "(\"n\" % 3)"
        );
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(),
            "(\"n\" % 3)"
        );
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Mysql).unwrap(),
            "(`n` % 3)"
        );
        // The bound (parameterized) path lowers identically (operator form).
        assert_eq!(
            render_expr_bound(&expr, &mut BindCtx::new(SqlDialect::Postgres)).unwrap(),
            "(\"n\" % $1)"
        );
    }

    #[test]
    fn is_true_is_false_rewritten_for_sqlite() {
        // SQLite has no IS TRUE / IS FALSE (no boolean type) — render as = 1 / = 0.
        // PG + MySQL keep the standard spelling.
        for (op, sqlite_expect, std_frag) in [
            (UnaryOp::IsTrue, "= 1", "IS TRUE"),
            (UnaryOp::IsFalse, "= 0", "IS FALSE"),
        ] {
            let e = Expr::UnaryOp { op, operand: Box::new(Expr::col("active")) };
            let pg = render_expr_inline(&e, SqlDialect::Postgres).unwrap();
            assert!(pg.contains(std_frag), "PG keeps `{std_frag}`: {pg}");
            let mysql = render_expr_inline(&e, SqlDialect::Mysql).unwrap();
            assert!(mysql.contains(std_frag), "MySQL keeps `{std_frag}`: {mysql}");
            let sqlite = render_expr_inline(&e, SqlDialect::Sqlite).unwrap();
            assert!(
                sqlite.contains(sqlite_expect) && !sqlite.contains("IS TRUE") && !sqlite.contains("IS FALSE"),
                "SQLite must rewrite `{std_frag}` to `{sqlite_expect}`: {sqlite}"
            );
        }
    }

    // ── portable predicate nodes: between / like / distinctFrom ──────────────

    #[test]
    fn between_renders_identically_on_all_three_dialects() {
        // `(operand BETWEEN low AND high)` is standard SQL — IDENTICAL on PG,
        // SQLite, and MySQL. The inline path binds no placeholders.
        let expr = Expr::Between {
            operand: Box::new(Expr::col("age")),
            low: Box::new(lit_int(18)),
            high: Box::new(lit_int(65)),
        };
        let expect_pg_sqlite = "(\"age\" BETWEEN 18 AND 65)";
        let expect_mysql = "(`age` BETWEEN 18 AND 65)";
        assert_eq!(render_expr_inline(&expr, SqlDialect::Postgres).unwrap(), expect_pg_sqlite);
        assert_eq!(render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(), expect_pg_sqlite);
        assert_eq!(render_expr_inline(&expr, SqlDialect::Mysql).unwrap(), expect_mysql);

        // Bound path: operand is an identifier; low/high become placeholders.
        for (dialect, ident) in [
            (SqlDialect::Postgres, "\"age\""),
            (SqlDialect::Sqlite, "\"age\""),
            (SqlDialect::Mysql, "`age`"),
        ] {
            let mut ctx = BindCtx::new(dialect);
            let sql = render_expr_bound(&expr, &mut ctx).unwrap();
            assert!(
                sql.starts_with(&format!("({ident} BETWEEN ")) && sql.contains(" AND "),
                "BETWEEN keeps its shape on {dialect:?}: {sql}"
            );
            assert_eq!(ctx.binds.len(), 2, "low + high bind on {dialect:?}");
        }
    }

    #[test]
    fn like_renders_same_syntax_on_all_three_dialects() {
        // `(operand LIKE pattern)` — same syntax on PG, SQLite, MySQL. (Per-dialect
        // case-sensitivity semantics differ; the parity PROOF is a Phase-4 claim,
        // not this slice — see the Expr::Like doc comment.)
        let expr = Expr::Like {
            operand: Box::new(Expr::col("name")),
            pattern: Box::new(Expr::lit(IrScalar::Str("A%".to_string()))),
        };
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Postgres).unwrap(),
            "(\"name\" LIKE 'A%')"
        );
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(),
            "(\"name\" LIKE 'A%')"
        );
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Mysql).unwrap(),
            "(`name` LIKE 'A%')"
        );
    }

    #[test]
    fn in_list_renders_pg_any_all_and_sql_in_not_in_on_all_three_dialects() {
        let includes = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![IrScalar::Str("a".into()), IrScalar::Str("b".into())],
            negated: false,
        };
        assert_eq!(
            render_expr_inline(&includes, SqlDialect::Postgres).unwrap(),
            "(\"status\" = ANY (ARRAY['a'::text, 'b'::text]))"
        );
        assert_eq!(
            render_expr_inline(&includes, SqlDialect::Sqlite).unwrap(),
            "(\"status\" IN ('a', 'b'))"
        );
        assert_eq!(
            render_expr_inline(&includes, SqlDialect::Mysql).unwrap(),
            "(`status` IN ('a', 'b'))"
        );

        let excludes = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![IrScalar::Str("x".into()), IrScalar::Str("y".into())],
            negated: true,
        };
        assert_eq!(
            render_expr_inline(&excludes, SqlDialect::Postgres).unwrap(),
            "(\"status\" <> ALL (ARRAY['x'::text, 'y'::text]))"
        );
        assert_eq!(
            render_expr_inline(&excludes, SqlDialect::Sqlite).unwrap(),
            "(\"status\" NOT IN ('x', 'y'))"
        );
        assert_eq!(
            render_expr_inline(&excludes, SqlDialect::Mysql).unwrap(),
            "(`status` NOT IN ('x', 'y'))"
        );

        let status_codes = Expr::InList {
            expr: Box::new(Expr::col("http_status")),
            elems: vec![IrScalar::Int(200), IrScalar::Int(404), IrScalar::Int(500)],
            negated: false,
        };
        assert_eq!(
            render_expr_inline(&status_codes, SqlDialect::Postgres).unwrap(),
            "(\"http_status\" = ANY (ARRAY[200,404,500]))"
        );
        assert_eq!(
            render_expr_inline(&status_codes, SqlDialect::Sqlite).unwrap(),
            "(\"http_status\" IN (200,404,500))"
        );
        assert_eq!(
            render_expr_inline(&status_codes, SqlDialect::Mysql).unwrap(),
            "(`http_status` IN (200,404,500))"
        );

        let enabled = Expr::InList {
            expr: Box::new(Expr::col("enabled")),
            elems: vec![IrScalar::Bool(true), IrScalar::Bool(false)],
            negated: false,
        };
        assert_eq!(
            render_expr_inline(&enabled, SqlDialect::Postgres).unwrap(),
            "(\"enabled\" = ANY (ARRAY[TRUE,FALSE]))"
        );
        assert_eq!(
            render_expr_inline(&enabled, SqlDialect::Sqlite).unwrap(),
            "(\"enabled\" IN (TRUE,FALSE))"
        );
        assert_eq!(
            render_expr_inline(&enabled, SqlDialect::Mysql).unwrap(),
            "(`enabled` IN (TRUE,FALSE))"
        );
    }

    #[test]
    fn in_list_empty_list_renders_boolean_constants() {
        let includes_empty = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![],
            negated: false,
        };
        let excludes_empty = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![],
            negated: true,
        };
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
            assert_eq!(render_expr_inline(&includes_empty, dialect).unwrap(), "FALSE");
            assert_eq!(render_expr_inline(&excludes_empty, dialect).unwrap(), "TRUE");
            assert_eq!(
                render_expr_bound(&includes_empty, &mut BindCtx::new(dialect)).unwrap(),
                "FALSE"
            );
            assert_eq!(
                render_expr_bound(&excludes_empty, &mut BindCtx::new(dialect)).unwrap(),
                "TRUE"
            );
        }
    }

    #[test]
    fn in_list_escapes_text_elements() {
        let expr = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![IrScalar::Str("a'b".into())],
            negated: false,
        };
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Postgres).unwrap(),
            "(\"status\" = ANY (ARRAY['a''b'::text]))"
        );
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(),
            "(\"status\" IN ('a''b'))"
        );
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Mysql).unwrap(),
            "(`status` IN ('a''b'))"
        );
    }

    #[test]
    fn in_list_rejects_mixed_and_bytes_elements() {
        let mixed = Expr::InList {
            expr: Box::new(Expr::col("status")),
            elems: vec![IrScalar::Str("ok".into()), IrScalar::Int(200)],
            negated: false,
        };
        let err = render_expr_inline(&mixed, SqlDialect::Postgres).unwrap_err();
        assert!(
            err.to_string().contains("homogeneous"),
            "mixed inList should fail homogeneous check: {err}"
        );

        let bytes = Expr::InList {
            expr: Box::new(Expr::col("payload")),
            elems: vec![IrScalar::Bytes(vec![1, 2, 3])],
            negated: false,
        };
        let err = render_expr_inline(&bytes, SqlDialect::Sqlite).unwrap_err();
        assert!(
            err.to_string().contains("bytes are not allowed"),
            "bytes inList should fail closed: {err}"
        );
    }

    #[test]
    fn distinct_from_diverges_pg_sqlite_vs_mysql() {
        // The whole point of the node: PG + SQLite support `IS DISTINCT FROM`
        // directly; MySQL has no such operator, so the engine lowers it to
        // `NOT (x <=> y)` (`<=>` is MySQL's NULL-safe equality).
        let expr = Expr::DistinctFrom {
            left: Box::new(Expr::col("a")),
            right: Box::new(Expr::col("b")),
        };
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Postgres).unwrap(),
            "(\"a\" IS DISTINCT FROM \"b\")",
            "PG uses IS DISTINCT FROM"
        );
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(),
            "(\"a\" IS DISTINCT FROM \"b\")",
            "SQLite uses IS DISTINCT FROM"
        );
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Mysql).unwrap(),
            "(NOT (`a` <=> `b`))",
            "MySQL lowers to NOT (a <=> b) — no IS DISTINCT FROM operator"
        );

        // Bound path renders the same divergent spellings.
        assert_eq!(
            render_expr_bound(&expr, &mut BindCtx::new(SqlDialect::Postgres)).unwrap(),
            "(\"a\" IS DISTINCT FROM \"b\")"
        );
        assert_eq!(
            render_expr_bound(&expr, &mut BindCtx::new(SqlDialect::Mysql)).unwrap(),
            "(NOT (`a` <=> `b`))"
        );
    }

    // ── the Layer-2 dialect() per-dialect value escape ───────────────────────

    #[test]
    fn dialectal_renders_the_target_dialects_own_leg() {
        // dialect({ pg: A, sqlite: B, mysql: C }) renders A on PG, B on SQLite,
        // C on MySQL — each target picks its OWN leg.
        let expr = Expr::Dialectal {
            default: None,
            pg: Some(Box::new(lit_str("A"))),
            sqlite: Some(Box::new(lit_str("B"))),
            mysql: Some(Box::new(lit_str("C"))),
        };
        // Inline path: each leg is an inline string literal.
        assert_eq!(render_expr_inline(&expr, SqlDialect::Postgres).unwrap(), "'A'");
        assert_eq!(render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(), "'B'");
        assert_eq!(render_expr_inline(&expr, SqlDialect::Mysql).unwrap(), "'C'");

        // Bound path: each leg's literal becomes exactly ONE placeholder — the
        // shape is fixed by the chosen leg, not by the other legs.
        for (dialect, ph) in [
            (SqlDialect::Postgres, "$1"),
            (SqlDialect::Sqlite, "?1"),
            (SqlDialect::Mysql, "?"),
        ] {
            let mut ctx = BindCtx::new(dialect);
            let sql = render_expr_bound(&expr, &mut ctx).unwrap();
            assert_eq!(sql, ph, "dialect() binds its chosen leg on {dialect:?}");
            assert_eq!(ctx.binds.len(), 1, "exactly one leg's literal binds on {dialect:?}");
        }
    }

    #[test]
    fn dialectal_falls_back_to_default_when_no_own_leg() {
        // dialect({ default: D, pg: A }) renders A on PG (its own leg) but D on
        // SQLite AND MySQL (fallback to default).
        let expr = Expr::Dialectal {
            default: Some(Box::new(lit_str("D"))),
            pg: Some(Box::new(lit_str("A"))),
            sqlite: None,
            mysql: None,
        };
        assert_eq!(render_expr_inline(&expr, SqlDialect::Postgres).unwrap(), "'A'", "PG uses its own leg");
        assert_eq!(render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(), "'D'", "SQLite falls back to default");
        assert_eq!(render_expr_inline(&expr, SqlDialect::Mysql).unwrap(), "'D'", "MySQL falls back to default");
    }

    #[test]
    fn dialectal_recurses_into_the_chosen_leg_expression() {
        // A leg is a full Expr, not just a literal — the chosen leg renders
        // recursively (here a BETWEEN on PG vs a bare column on the default).
        let expr = Expr::Dialectal {
            default: Some(Box::new(Expr::col("age"))),
            pg: Some(Box::new(Expr::Between {
                operand: Box::new(Expr::col("age")),
                low: Box::new(lit_int(1)),
                high: Box::new(lit_int(9)),
            })),
            sqlite: None,
            mysql: None,
        };
        assert_eq!(
            render_expr_inline(&expr, SqlDialect::Postgres).unwrap(),
            "(\"age\" BETWEEN 1 AND 9)",
        );
        assert_eq!(render_expr_inline(&expr, SqlDialect::Sqlite).unwrap(), "\"age\"");
    }

    #[test]
    fn dialectal_with_no_leg_for_target_is_a_fail_closed_render_backstop() {
        // A dialect({ pg: A }) (no default) has no SQLite leg — validate refuses
        // this per-target BEFORE assembly, but the renderer is defensively
        // fail-closed rather than silently dropping the value.
        let expr = Expr::Dialectal {
            default: None,
            pg: Some(Box::new(lit_str("A"))),
            sqlite: None,
            mysql: None,
        };
        assert!(render_expr_inline(&expr, SqlDialect::Postgres).is_ok());
        let err = render_expr_inline(&expr, SqlDialect::Sqlite).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "no SQLite leg → fail-closed: {err:?}");
    }

    // ── portable aggregate node: c.agg.count/sum/avg/min/max + DISTINCT ──────

    #[test]
    fn agg_renders_identically_on_all_three_dialects() {
        use crate::model::expr::AggFunc;

        // count(*) — no arg — is byte-identical everywhere (no identifier at all).
        let count_star = Expr::Agg {
            func: AggFunc::Count,
            arg: None,
            delimiter: None,
            distinct: false,
        };
        for d in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
            assert_eq!(
                render_expr_inline(&count_star, d).unwrap(),
                "count(*)",
                "count(*) is identical on {d:?}"
            );
        }

        // count(DISTINCT <col>) — only the identifier quoting differs (MySQL backticks).
        let count_distinct = Expr::Agg {
            func: AggFunc::Count,
            arg: Some(Box::new(Expr::col("x"))),
            delimiter: None,
            distinct: true,
        };
        assert_eq!(render_expr_inline(&count_distinct, SqlDialect::Postgres).unwrap(), "count(DISTINCT \"x\")");
        assert_eq!(render_expr_inline(&count_distinct, SqlDialect::Sqlite).unwrap(), "count(DISTINCT \"x\")");
        assert_eq!(render_expr_inline(&count_distinct, SqlDialect::Mysql).unwrap(), "count(DISTINCT `x`)");

        // sum/avg/min/max(<col>) — identical spelling, only quoting differs.
        for (func, name) in [
            (AggFunc::Sum, "sum"),
            (AggFunc::Avg, "avg"),
            (AggFunc::Min, "min"),
            (AggFunc::Max, "max"),
        ] {
            let e = Expr::Agg {
                func,
                arg: Some(Box::new(Expr::col("x"))),
                delimiter: None,
                distinct: false,
            };
            assert_eq!(render_expr_inline(&e, SqlDialect::Postgres).unwrap(), format!("{name}(\"x\")"));
            assert_eq!(render_expr_inline(&e, SqlDialect::Sqlite).unwrap(), format!("{name}(\"x\")"));
            assert_eq!(render_expr_inline(&e, SqlDialect::Mysql).unwrap(), format!("{name}(`x`)"));
        }

        // The bound path renders the aggregate identically and binds no placeholders
        // (a ColRef arg is an identifier, not a bind).
        let mut ctx = BindCtx::new(SqlDialect::Postgres);
        assert_eq!(render_expr_bound(&count_distinct, &mut ctx).unwrap(), "count(DISTINCT \"x\")");
        assert_eq!(ctx.binds.len(), 0, "a ColRef aggregate arg is not a bind");
        assert_eq!(
            render_expr_bound(&count_star, &mut BindCtx::new(SqlDialect::Mysql)).unwrap(),
            "count(*)"
        );
    }

    #[test]
    fn pg_first_aggregates_render_postgres_sql_names_and_string_agg_delimiter() {
        use crate::model::expr::AggFunc;

        let string_agg = Expr::Agg {
            func: AggFunc::StringAgg,
            arg: Some(Box::new(Expr::col("name"))),
            delimiter: Some(Box::new(Expr::lit(IrScalar::Str(", ".to_string())))),
            distinct: false,
        };
        assert_eq!(
            render_expr_inline(&string_agg, SqlDialect::Postgres).unwrap(),
            "string_agg(\"name\", ', ')"
        );

        let string_agg_distinct = Expr::Agg {
            func: AggFunc::StringAgg,
            arg: Some(Box::new(Expr::col("name"))),
            delimiter: Some(Box::new(Expr::lit(IrScalar::Str("|".to_string())))),
            distinct: true,
        };
        assert_eq!(
            render_expr_inline(&string_agg_distinct, SqlDialect::Postgres).unwrap(),
            "string_agg(DISTINCT \"name\", '|')"
        );

        for (func, sql) in [
            (AggFunc::ArrayAgg, "array_agg(\"name\")"),
            (AggFunc::BoolAnd, "bool_and(\"name\")"),
            (AggFunc::BoolOr, "bool_or(\"name\")"),
        ] {
            let e = Expr::Agg {
                func,
                arg: Some(Box::new(Expr::col("name"))),
                delimiter: None,
                distinct: false,
            };
            assert_eq!(render_expr_inline(&e, SqlDialect::Postgres).unwrap(), sql);
        }
    }

    // ── identifier safety ───────────────────────────────────────────────────

    #[test]
    fn rejects_schema_qualified_table() {
        let err = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "other_schema.victims",
            &["a".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap_err();
        assert!(matches!(err, DmlError::InvalidIdentifier { what: "table", .. }), "{err:?}");
    }

    /// L1 self-defense: the PG `qualify_table` arm must not blindly trust
    /// the engine-supplied `project_schema`. A NUL byte — the one char that
    /// `"`-doubling cannot neutralise (PG rejects it inside an identifier) — is
    /// refused fail-closed with `DmlError::InvalidIdentifier { what: "schema" }`,
    /// not interpolated. RED before the `quote_schema` assertion landed (the old
    /// `format!` would have emitted a statement carrying the raw NUL).
    #[test]
    fn rejects_nul_in_project_schema_pg() {
        let err = assemble_insert(
            "app\0proj",
            SqlDialect::Postgres,
            "t",
            &["a".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap_err();
        assert!(matches!(err, DmlError::InvalidIdentifier { what: "schema", .. }), "{err:?}");
    }

    /// An empty schema is likewise refused fail-closed — `""` cannot name a real
    /// relation and an empty quoted ident (`""`) is degenerate.
    #[test]
    fn rejects_empty_project_schema_pg() {
        let err = assemble_insert(
            "",
            SqlDialect::Postgres,
            "t",
            &["a".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap_err();
        assert!(matches!(err, DmlError::InvalidIdentifier { what: "schema", .. }), "{err:?}");
    }

    /// The real Confined project schema is the app id — a UUIDv7 carrying `-`,
    /// which is NOT a bare `[A-Za-z_]…` ident. It MUST render (not be rejected):
    /// the prior over-strict `quote_ident` predicate would have broken every real
    /// deploy. `-` is render-safe, emitted verbatim inside the quoted schema.
    #[test]
    fn uuid_project_schema_renders_pg() {
        let a = assemble_insert(
            "019efd94-a4e0-7a82-8a08-95e1f906ca3f",
            SqlDialect::Postgres,
            "members",
            &["id".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"019efd94-a4e0-7a82-8a08-95e1f906ca3f\".\"members\" (\"id\") \
             VALUES ($1)"
        );
    }

    /// A hostile `"`-bearing schema cannot break out of the quoted identifier:
    /// it is SAFELY escaped (doubled `""`), not raw-interpolated and not
    /// (wrongly) rejected — the statement shape is unaltered, matching how every
    /// other engine seam quotes the schema.
    #[test]
    fn quote_bearing_project_schema_is_escaped_not_broken_out_pg() {
        let a = assemble_insert(
            "a\"; DROP--",
            SqlDialect::Postgres,
            "t",
            &["a".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap();
        assert_eq!(a.template, "INSERT INTO \"a\"\"; DROP--\".\"t\" (\"a\") VALUES ($1)");
    }

    #[test]
    fn rejects_injection_in_column() {
        let err = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "t",
            &["a\"); DROP TABLE users; --".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap_err();
        assert!(matches!(err, DmlError::InvalidIdentifier { what: "column", .. }), "{err:?}");
    }

    // ── insert: native binds, never interpolated ────────────────────────────

    #[test]
    fn insert_binds_all_values_pg() {
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![val(IrScalar::Int(200)), val(IrScalar::Str("ok".into()))]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"app_proj\".\"status_codes\" (\"code\", \"label\") VALUES ($1, $2)"
        );
        assert_eq!(a.binds, vec![BindValue::Int(200), BindValue::Text("ok".into())]);
    }

    #[test]
    fn insert_renders_fnsynth_value_without_bind_pg() {
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "events",
            &["created_at".into(), "id".into()],
            &[vec![IrValue::Expr(Expr::FnSynth {
                r#fn: SynthFn::Now,
                args: vec![],
            }), IrValue::Expr(Expr::FnSynth {
                r#fn: SynthFn::GenRandomUuid,
                args: vec![],
            })]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"app_proj\".\"events\" (\"created_at\", \"id\") VALUES (now(), gen_random_uuid())"
        );
        assert!(a.binds.is_empty(), "fnSynth insert value is DB-evaluated, not a bind");
    }

    #[test]
    fn insert_renders_fnsynth_value_without_bind_mysql() {
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Mysql,
            "events",
            &["created_at".into(), "id".into()],
            &[vec![IrValue::Expr(Expr::FnSynth {
                r#fn: SynthFn::Now,
                args: vec![],
            }), IrValue::Expr(Expr::FnSynth {
                r#fn: SynthFn::GenRandomUuid,
                args: vec![],
            })]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO `app_proj`.`events` (`created_at`, `id`) VALUES (CURRENT_TIMESTAMP(6), UUID())"
        );
        assert!(a.binds.is_empty(), "fnSynth insert value is DB-evaluated, not a bind");
    }

    #[test]
    fn insert_renders_fnsynth_value_without_bind_sqlite() {
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Sqlite,
            "events",
            &["created_at".into(), "id".into()],
            &[vec![IrValue::Expr(Expr::FnSynth {
                r#fn: SynthFn::Now,
                args: vec![],
            }), IrValue::Expr(Expr::FnSynth {
                r#fn: SynthFn::GenRandomUuid,
                args: vec![],
            })]],
            None,
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"events\" (\"created_at\", \"id\") VALUES (CURRENT_TIMESTAMP, lower(hex(randomblob(16))))"
        );
        assert!(a.binds.is_empty(), "fnSynth insert value is DB-evaluated, not a bind");
    }

    #[test]
    fn insert_uses_question_placeholders_on_sqlite() {
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Sqlite,
            "t",
            &["a".into(), "b".into()],
            &[vec![val(IrScalar::Int(1)), val(IrScalar::Null)]],
            None,
        )
        .unwrap();
        assert_eq!(a.template, "INSERT INTO \"t\" (\"a\", \"b\") VALUES (?1, ?2)");
        assert_eq!(a.binds, vec![BindValue::Int(1), BindValue::Null]);
    }

    #[test]
    fn insert_multi_row_continues_placeholder_counter() {
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "t",
            &["a".into()],
            &[vec![val(IrScalar::Int(1))], vec![val(IrScalar::Int(2))]],
            None,
        )
        .unwrap();
        assert_eq!(a.template, "INSERT INTO \"app_proj\".\"t\" (\"a\") VALUES ($1), ($2)");
        assert_eq!(a.binds, vec![BindValue::Int(1), BindValue::Int(2)]);
    }

    /// Bind-safety: a value full of SQL metacharacters cannot alter the statement
    /// shape — it is a single bind, the template is unchanged.
    #[test]
    fn insert_metacharacter_value_cannot_alter_shape() {
        let hostile = "x'); DROP TABLE users; --";
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "t",
            &["a".into()],
            &[vec![val(IrScalar::Str(hostile.into()))]],
            None,
        )
        .unwrap();
        // The template carries ONLY the placeholder; the hostile bytes are a bind.
        assert_eq!(a.template, "INSERT INTO \"app_proj\".\"t\" (\"a\") VALUES ($1)");
        assert!(!a.template.contains("DROP"), "metacharacters must not reach the template");
        assert_eq!(a.binds, vec![BindValue::Text(hostile.into())]);
    }

    #[test]
    fn insert_over_the_bind_param_ceiling_is_rejected() {
        // One column × (MAX_BIND_PARAMS + 1) rows assembles one bind per row,
        // overflowing the protocol parameter ceiling. Reject with a bounded error.
        let rows: Vec<Vec<IrValue>> =
            (0..=MAX_BIND_PARAMS as i64).map(|i| vec![val(IrScalar::Int(i))]).collect();
        let err = assemble_insert(SCHEMA, SqlDialect::Postgres, "t", &["a".into()], &rows, None)
            .unwrap_err();
        assert!(
            matches!(err, DmlError::TooManyBinds { count, max, .. } if count == MAX_BIND_PARAMS + 1 && max == MAX_BIND_PARAMS),
            "{err:?}"
        );
        // Exactly at the ceiling still assembles.
        let rows_ok: Vec<Vec<IrValue>> =
            (0..MAX_BIND_PARAMS as i64).map(|i| vec![val(IrScalar::Int(i))]).collect();
        assert!(assemble_insert(SCHEMA, SqlDialect::Postgres, "t", &["a".into()], &rows_ok, None).is_ok());
    }

    #[test]
    fn insert_ragged_row_rejected() {
        let err = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "t",
            &["a".into(), "b".into()],
            &[vec![val(IrScalar::Int(1))]],
            None,
        )
        .unwrap_err();
        assert!(matches!(err, DmlError::MalformedInsert { .. }), "{err:?}");
    }

    // ── onConflict: PG renders, SQLite hard error ────────────────────────────

    #[test]
    fn insert_on_conflict_renders_on_pg() {
        let oc = OnConflict {
            columns: vec!["code".into()],
            do_update: Some(BTreeMap::from([("label".to_string(), val(IrScalar::Str("dup".into())))])),
        };
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![val(IrScalar::Int(1)), val(IrScalar::Str("ok".into()))]],
            Some(&oc),
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"app_proj\".\"status_codes\" (\"code\", \"label\") VALUES ($1, $2) \
             ON CONFLICT (\"code\") DO UPDATE SET \"label\" = $3"
        );
        assert_eq!(
            a.binds,
            vec![BindValue::Int(1), BindValue::Text("ok".into()), BindValue::Text("dup".into())]
        );
    }

    #[test]
    fn insert_on_conflict_do_update_renders_fnsynth_without_bind_pg() {
        let oc = OnConflict {
            columns: vec!["code".into()],
            do_update: Some(BTreeMap::from([(
                "updated_at".to_string(),
                IrValue::Expr(Expr::FnSynth {
                    r#fn: SynthFn::Now,
                    args: vec![],
                }),
            )])),
        };
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![val(IrScalar::Int(1)), val(IrScalar::Str("ok".into()))]],
            Some(&oc),
        )
        .unwrap();
        assert_eq!(
            a.template,
            "INSERT INTO \"app_proj\".\"status_codes\" (\"code\", \"label\") VALUES ($1, $2) \
             ON CONFLICT (\"code\") DO UPDATE SET \"updated_at\" = now()"
        );
        assert_eq!(
            a.binds,
            vec![BindValue::Int(1), BindValue::Text("ok".into())],
            "fnSynth(now) in doUpdate must be DB-evaluated, not a bind"
        );
    }

    #[test]
    fn insert_on_conflict_do_nothing_pg() {
        let oc = OnConflict { columns: vec!["code".into()], do_update: None };
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "t",
            &["code".into()],
            &[vec![val(IrScalar::Int(1))]],
            Some(&oc),
        )
        .unwrap();
        assert!(a.template.ends_with("ON CONFLICT (\"code\") DO NOTHING"), "{}", a.template);
    }

    #[test]
    fn insert_on_conflict_rejected_on_sqlite() {
        let oc = OnConflict { columns: vec!["code".into()], do_update: None };
        let err = assemble_insert(
            SCHEMA,
            SqlDialect::Sqlite,
            "t",
            &["code".into()],
            &[vec![val(IrScalar::Int(1))]],
            Some(&oc),
        )
        .unwrap_err();
        assert!(matches!(err, DmlError::OnConflictNotPortable { .. }), "{err:?}");
    }

    // ── update: bound set + where, both dialects ─────────────────────────────

    #[test]
    fn update_binds_literal_in_set_and_where() {
        let set = BTreeMap::from([(
            "label".to_string(),
            dml_expr(Expr::FnCall {
                r#fn: ScalarFn::Coalesce,
                args: vec![Expr::col("label"), lit_str("unknown")],
            }),
        )]);
        let pred = Expr::BinOp {
            op: BinaryOp::Gt,
            lhs: Box::new(Expr::col("code")),
            rhs: Box::new(lit_int(0)),
        };
        let a = assemble_update(SCHEMA, SqlDialect::Postgres, "status_codes", &set, Some(&pred))
            .unwrap();
        assert_eq!(
            a.template,
            "UPDATE \"app_proj\".\"status_codes\" SET \"label\" = coalesce(\"label\", $1) \
             WHERE (\"code\" > $2)"
        );
        assert_eq!(a.binds, vec![BindValue::Text("unknown".into()), BindValue::Int(0)]);
    }

    #[test]
    fn update_portable_on_sqlite() {
        let set = BTreeMap::from([("a".to_string(), dml_expr(lit_int(5)))]);
        let a = assemble_update(SCHEMA, SqlDialect::Sqlite, "t", &set, None).unwrap();
        assert_eq!(a.template, "UPDATE \"t\" SET \"a\" = ?1");
        assert_eq!(a.binds, vec![BindValue::Int(5)]);
    }

    #[test]
    fn update_empty_set_rejected() {
        let err =
            assemble_update(SCHEMA, SqlDialect::Postgres, "t", &BTreeMap::new(), None).unwrap_err();
        assert!(matches!(err, DmlError::EmptySet { op: "update", .. }), "{err:?}");
    }

    // ── delete: mandatory where, both dialects ───────────────────────────────

    #[test]
    fn delete_binds_where_pg() {
        let pred = Expr::UnaryOp { op: UnaryOp::IsNull, operand: Box::new(Expr::col("code")) };
        let a = assemble_delete(SCHEMA, SqlDialect::Postgres, "t", &pred, None).unwrap();
        assert_eq!(a.template, "DELETE FROM \"app_proj\".\"t\" WHERE (\"code\" IS NULL)");
        assert!(a.binds.is_empty());
    }

    #[test]
    fn delete_with_limit_sqlite() {
        let pred = Expr::BinOp {
            op: BinaryOp::Lt,
            lhs: Box::new(Expr::col("code")),
            rhs: Box::new(lit_int(0)),
        };
        let a = assemble_delete(SCHEMA, SqlDialect::Sqlite, "t", &pred, Some(100)).unwrap();
        // The bundled rusqlite is built WITHOUT SQLITE_ENABLE_UPDATE_DELETE_LIMIT,
        // so `DELETE … LIMIT ?n` is a syntax error; the portable form is the
        // rowid subquery (the SQLite analog of the PG ctid form).
        assert_eq!(
            a.template,
            "DELETE FROM \"t\" WHERE rowid IN \
             (SELECT rowid FROM \"t\" WHERE (\"code\" < ?1) LIMIT ?2)"
        );
        assert_eq!(a.binds, vec![BindValue::Int(0), BindValue::Int(100)]);
    }

    #[test]
    fn delete_with_limit_pg() {
        // The PG ctid-subquery form is unchanged; pin it alongside the SQLite form
        // so the two portable lowerings stay structurally parallel.
        let pred = Expr::BinOp {
            op: BinaryOp::Lt,
            lhs: Box::new(Expr::col("code")),
            rhs: Box::new(lit_int(0)),
        };
        let a = assemble_delete(SCHEMA, SqlDialect::Postgres, "t", &pred, Some(100)).unwrap();
        assert_eq!(
            a.template,
            "DELETE FROM \"app_proj\".\"t\" WHERE ctid IN \
             (SELECT ctid FROM \"app_proj\".\"t\" WHERE (\"code\" < $1) LIMIT $2)"
        );
        assert_eq!(a.binds, vec![BindValue::Int(0), BindValue::Int(100)]);
    }

    // ── backfill: inline strings (PG path) ───────────────────────────────────

    #[test]
    fn backfill_renders_inline_set_and_filter() {
        let set = BTreeMap::from([(
            "label".to_string(),
            dml_expr(Expr::BinOp {
                op: BinaryOp::Concat,
                lhs: Box::new(Expr::col("code")),
                rhs: Box::new(lit_str("!")),
            }),
        )]);
        let filter = Expr::BinOp {
            op: BinaryOp::Gt,
            lhs: Box::new(Expr::col("code")),
            rhs: Box::new(lit_int(0)),
        };
        let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, Some(&filter)).unwrap();
        assert_eq!(c.set_clause, "\"label\" = (\"code\" || '!')");
        assert_eq!(c.filter.as_deref(), Some("(\"code\" > 0)"));
    }

    /// A string literal in a backfill is `''`-escaped inline (then guard-revalidated
    /// downstream); the quote cannot break out of the literal.
    #[test]
    fn backfill_inline_string_is_quote_escaped() {
        let set = BTreeMap::from([("a".to_string(), dml_expr(lit_str("O'Brien")))]);
        let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = 'O''Brien'");
    }

    #[test]
    fn backfill_empty_set_rejected() {
        let err = assemble_backfill_clauses(SqlDialect::Postgres, "t", &BTreeMap::new(), None)
            .unwrap_err();
        assert!(matches!(err, DmlError::EmptySet { op: "backfill", .. }), "{err:?}");
    }

    // ── splitPart lowering (pinned helper) ───────────────────────────────────

    fn split(col: &str, delim: &str, n: i64) -> Expr {
        Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col(col),
                Expr::lit(IrScalar::Str(delim.into())),
                Expr::lit(IrScalar::Int(n)),
            ],
        }
    }

    /// PG lowers splitPart to the native `split_part(col, 'd', n)` — verbatim.
    #[test]
    fn split_part_pg_native() {
        let set = BTreeMap::from([("first".to_string(), dml_expr(split("name", " ", 1)))]);
        let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"first\" = split_part(\"name\", ' ', 1)");
    }

    /// SQLite lowers splitPart to the pinned instr/substr unroll. n=1 is the base
    /// case (no inner walk). The exact string is pinned to the reference exhibit.
    #[test]
    fn split_part_sqlite_n1_unroll() {
        let set = BTreeMap::from([("first".to_string(), dml_expr(split("name", " ", 1)))]);
        let c = assemble_backfill_clauses(SqlDialect::Sqlite, "t", &set, None).unwrap();
        assert_eq!(
            c.set_clause,
            "\"first\" = substr((\"name\" || ' '), 1, instr((\"name\" || ' '), ' ') - 1)"
        );
    }

    /// SQLite n=2 unrolls one boundary walk — pinned to the reference exhibit.
    #[test]
    fn split_part_sqlite_n2_unroll() {
        let set = BTreeMap::from([("last".to_string(), dml_expr(split("name", " ", 2)))]);
        let c = assemble_backfill_clauses(SqlDialect::Sqlite, "t", &set, None).unwrap();
        // cur1 = substr((name||' '), instr((name||' '), ' ') + 1)
        // result = substr(cur1, 1, instr(cur1, ' ') - 1)
        assert_eq!(
            c.set_clause,
            "\"last\" = substr(substr((\"name\" || ' '), instr((\"name\" || ' '), ' ') + 1), \
             1, instr(substr((\"name\" || ' '), instr((\"name\" || ' '), ' ') + 1), ' ') - 1)"
        );
    }

    /// splitPart works in the one-shot (bound) path too — the column arg renders
    /// (binding nested literals); the delim/n are engine-pinned constants, NOT binds.
    #[test]
    fn split_part_one_shot_bound_pg() {
        let set = BTreeMap::from([("first".to_string(), dml_expr(split("name", ",", 1)))]);
        let a = assemble_update(SCHEMA, SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(a.template, "UPDATE \"app_proj\".\"t\" SET \"first\" = split_part(\"name\", ',', 1)");
        assert!(a.binds.is_empty(), "delim/n are pinned constants, not binds");
    }

    /// A single-quote delimiter is `''''`-escaped in the inline literal on both legs.
    #[test]
    fn split_part_quote_delim_escaped_sqlite() {
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", "'", 1)))]);
        let c = assemble_backfill_clauses(SqlDialect::Sqlite, "t", &set, None).unwrap();
        assert_eq!(
            c.set_clause,
            "\"a\" = substr((\"name\" || ''''), 1, instr((\"name\" || ''''), '''') - 1)"
        );
    }

    /// Renderer fail-closed backstop: an out-of-envelope splitPart (multi-char
    /// delim) that somehow reached the renderer is rejected ON SQLITE, never
    /// mis-built.
    #[test]
    fn split_part_renderer_rejects_out_of_envelope() {
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ", ", 1)))]);
        let err = assemble_backfill_clauses(SqlDialect::Sqlite, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ",", 9)))]);
        let err = assemble_backfill_clauses(SqlDialect::Sqlite, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
    }

    /// The documented `dialect_scope=PgOnly` escape for an
    /// out-of-envelope `c.fn.splitPart`. The validator ADMITS a
    /// multi-char delimiter and `n > 8` on a Postgres target; the renderer MUST
    /// therefore lower it to native `split_part(col, 'delim', n)` on PG, not
    /// hard-error. This is the missing companion to the load-only grammar test
    /// `out_of_envelope_split_part_pg_loads_sqlite_rejected`.
    #[test]
    fn split_part_out_of_envelope_renders_native_on_pg() {
        // multi-char delimiter — PG's split_part is multi-char-capable.
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ", ", 1)))]);
        let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = split_part(\"name\", ', ', 1)");

        // n beyond the SQLite unroll bound (9) — PG takes any positive n.
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ",", 9)))]);
        let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = split_part(\"name\", ',', 9)");

        // and the one-shot (bound) PG path too — delim/n stay pinned constants.
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ", ", 1)))]);
        let a = assemble_update(SCHEMA, SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(a.template, "UPDATE \"app_proj\".\"t\" SET \"a\" = split_part(\"name\", ', ', 1)");
        assert!(a.binds.is_empty(), "delim/n are pinned constants, not binds");
    }

    /// A non-ASCII (multibyte) delimiter is still PG-renderable (PG splits on the
    /// literal string); the single-ASCII byte gate is a SQLite-only envelope.
    #[test]
    fn split_part_non_ascii_delim_renders_on_pg() {
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", "→", 2)))]);
        let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = split_part(\"name\", '→', 2)");
        // …but rejected on the SQLite leg (out of the byte-wise envelope).
        let err = assemble_backfill_clauses(SqlDialect::Sqlite, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
    }

    /// PG still rejects a structurally-malformed splitPart (non-literal delim, a
    /// non-positive n, a non-string delim) — the PG path widens the ENVELOPE, not
    /// the grammar. These remain unrenderable on both dialects.
    #[test]
    fn split_part_pg_still_rejects_malformed() {
        // n = 0 (not a positive part index) — invalid on PG too.
        let set = BTreeMap::from([("a".to_string(), dml_expr(split("name", ",", 0)))]);
        let err = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
        // non-literal delim (a ColRef) — never renderable.
        let bad = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::ColRef { name: "name".into(), table: None },
                Expr::ColRef { name: "name".into(), table: None },
                lit_int(1),
            ],
        };
        let set = BTreeMap::from([("a".to_string(), dml_expr(bad))]);
        let err = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
    }

    /// The renderer's envelope bound MUST equal the validator's, so a node the
    /// validator admits the renderer can always lower.
    #[test]
    fn split_part_max_n_matches_validator() {
        assert_eq!(SPLIT_PART_MAX_N, crate::model::validate::SPLIT_PART_MAX_N);
    }
}
