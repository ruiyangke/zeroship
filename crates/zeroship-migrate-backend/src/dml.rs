//! The creator-**DML assembler** + the closed-AST **expression renderer**.
//!
//! `IrAuthor::lower` compiles the DDL ops (`createTable`/`alter*`/...) into the
//! same `Migration` shape the declarative differ
//! emits. This module is the peer for the **DML** ops - `insert` / `update` /
//! `del` / `backfill` - and for the closed expression AST ([`zeroship_migrate_ir::expr::Expr`])
//! they carry in their `set` / `where` / `filter` positions.
//!
//! # Two rendering modes, one source of truth
//!
//! A migration value can reach the database two structurally-distinct ways, and
//! this module owns both:
//!
//! 1. **Parameterized one-shot DML** ([`assemble_insert_for_backend`] / [`assemble_update_for_backend`] /
//!    [`assemble_delete_for_backend`]). Every authored VALUE - an `insert` row scalar, an
//!    `update SET` literal, a `where`-predicate literal - is emitted as a NATIVE
//!    placeholder (`$n` on Postgres, `?n` on SQLite) carried on a
//!    `PlanStep::Dml` `binds` vector, NEVER
//!    string-interpolated. So a value containing a quote / semicolon / comment
//!    cannot change the *shape* of the statement on either backend (the
//!    bind-safety property). The expression renderer (`render_expr_bound`) walks
//!    the closed AST and appends a placeholder for each [`Expr::Literal`].
//!
//! 2. **Batched backfill** (`assemble_backfill_clauses_for_backend`). The existing
//!    `BackfillSpec` executor (PG `backfill.rs`)
//!    consumes a `set_clause` / `filter` SQL *string* (it assembles a windowed
//!    `UPDATE ... WHERE cursor > $last ... AND (<filter>)` and guard-checks the WHOLE
//!    statement). A backfill expression references the row's own columns and is
//!    paged, so it cannot carry positional binds the way a one-shot statement can.
//!    Here the renderer (`render_expr_inline_for_backend`) emits a SQL string in which a
//!    `Literal` is an INLINE SQL literal (numeric verbatim; a string single-quoted
//!    with `''` doubling - the canonical escape the guard's real-parser deny-list
//!    then re-validates). The assembled `UPDATE` is guard-checked by the executor
//!    before any batch runs, so the inline path inherits the same parse-time
//!    confinement the rest of the engine relies on.
//!
//! # Identifier safety
//!
//! Every identifier (table, column) is validated as a bare
//! `[A-Za-z_][A-Za-z0-9_]*` identifier and double-quoted with `"` doubling
//! (`quote_ident`). A schema-qualified or otherwise malformed identifier is
//! rejected at assemble time - an injection attempt through an identifier slot
//! cannot reach the database. On **Postgres** the table is qualified to the project
//! schema (`"schema"."table"`) so the resolved relation is always the project's
//! own; on **SQLite** the table lives in the connection's `main` database (the app
//! file) and is referenced UNqualified, matching the engine's SQLite DDL.
//!
//! # Conflict handling and backfills
//!
//! - `insert { onConflict }` renders exact `ON CONFLICT (columns)` semantics on
//!   PostgreSQL and SQLite. MySQL uses `ON DUPLICATE KEY UPDATE`, which cannot name
//!   one unique constraint. For `doUpdate`, the generated statement compares the
//!   incoming target values with the conflicting row before applying assignments,
//!   so a collision on another unique key cannot update the wrong row. A MySQL
//!   `doUpdate` therefore requires every target column in the inserted column list
//!   and does not permit assigning those target columns. A collision on another
//!   unique key raises an error. MySQL `doNothing` is refused because its native
//!   no-op update fires update triggers, while `INSERT IGNORE` suppresses unrelated
//!   errors; neither is equivalent to a targeted `DO NOTHING`.
//! - A **batched** `backfill` targets the `BackfillSpec` executor, PORTABLE on
//!   BOTH backends: PG via the
//!   writable-CTE windowed `UPDATE` (`backfill.rs`), SQLite via the batched
//!   per-batch-txn executor (`zeroship_migrate_sqlite::backend::backfill_sql`). The inline
//!   `set`/`filter` differ per dialect (the `c.fn.splitPart` lowering,
//!   NULL-skipping `concatWs`); the `BackfillSpec` shape is uniform.
//!
//! # What is SPELLING here, and what deliberately is not
//!
//! Per `docs/proposals/pluggable-backends.md`, the SQL SPELLING in this
//! module has moved to `render::backends`, reached through the `DmlRenderer`
//! trait: placeholders, inline string / decimal / bytes literals, `IN`-list
//! shape, regex match, date extraction, concatenation, `IS DISTINCT FROM`, the
//! `IS TRUE` / `IS FALSE` predicates, and the per-vendor scalar-call overrides.
//! Each moved body is character-for-character what stood here; the three
//! `Postgres | Sqlite` shared arms became one duplicated line in each of the two
//! modules, which is the price of the arms being per-vendor impls rather than a
//! match. (Plain text, not doc links: every name in this section is a private
//! item, and a link from this module's public docs to one is a rustdoc warning.)
//!
//! The remaining neutral helpers do not select a vendor:
//!
//! - **IR leg selection.** `select_dialect_leg` reads the wire-pinned
//!   `Expr::Dialectal { legs }` map by the backend's own `DialectId`. It cannot
//!   move; the shape is a checksum input.
//! - **Bind accumulation.** [`BindCtx`] is contract vocabulary now. It lets each
//!   required backend method append values while owning its conflict and limited
//!   delete grammar; core neither picks a shipping id nor supplies a fallback.
//!
//! # There is no placeholder seam here, and there never was one
//!
//! This section used to describe a shared `placeholder(dialect, n)` function the
//! one-shot assembler and a vendor's batched-backfill executor supposedly both called.
//! They never did: that function had zero callers anywhere and was deleted, and the
//! two paths agree because both end at the SAME resolved `DmlRenderer`, not because a
//! common function routes them. The distinction matters - the old wording made a
//! `renderer(dialect)` lookup nothing performed look like a dialect boundary, and it
//! was counted as one. See the tombstone above its old home below.
//!
//! A SQLite-named placeholder helper sat beside it with two real callers, both inside
//! one vendor crate. Two callers in one crate is that crate's helper; it is
//! `crate::dml::placeholder` in `zeroship-migrate-sqlite` now, and this module names no
//! backend's placeholder spelling.
//!
//! The transport-safe bind mirror
//! (`zeroship_migrate_sqlite::backend::actor::SqliteBind`) is the single
//! value-binding path the SQLite executor uses.

use std::collections::BTreeMap;

use crate::renderer::{Capability, DmlRenderer};
use zeroship_migrate_ir::dialect::DialectId;

use crate::step::BindValue;
use zeroship_migrate_ir::expr::{AggFunc, BinaryOp, Expr, ExtractField, ScalarFn, SynthFn, UnaryOp};
use zeroship_migrate_ir::ir::{IrScalar, IrValue};

/// A failure assembling a DML op into a statement (template + binds, or a backfill
/// spec). Distinct from the structural [`zeroship_migrate_ir::validate::AuthoringError`]
/// (which gates the expression AST *before* assembly): this carries the
/// assembler-level rejections: a malformed identifier, an empty insert, an
/// expression node the renderer cannot lower, or a MySQL conflict shape whose
/// target intent cannot be retained safely.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DmlError {
    /// An identifier (table / column) is not a bare `[A-Za-z_][A-Za-z0-9_]*`
    /// identifier - empty, schema-qualified, or containing characters outside the
    /// safe set. Rejected before any SQL is assembled.
    #[error(
        "invalid identifier for {what}: {value:?} (must be a bare [A-Za-z_][A-Za-z0-9_]* identifier)"
    )]
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
    /// An `update` / `backfill` whose `set` map is empty - nothing to assign.
    #[error(
        "malformed {op} into {table:?}: empty `set` (a transform must assign at least one column)"
    )]
    EmptySet {
        /// The op kind (`"update"` / `"backfill"`).
        op: &'static str,
        /// The target table.
        table: String,
    },
    /// A target with no native `DELETE ... LIMIT` lowers one through a subquery,
    /// which needs a live-catalog key that identifies one row exactly.
    ///
    /// A backend's implicit row identifier is not sufficient and no backend here may
    /// substitute one: SQLite's hidden `rowid` can be shadowed by a declared column of
    /// that name and is absent entirely from a `WITHOUT ROWID` table, and that
    /// unreliability is the general case, not a quirk. The refusal wants a key the
    /// CATALOG proves.
    #[error(
        "{dialect} limited delete from {table:?} requires a catalog-proven non-null PRIMARY KEY or full UNIQUE key"
    )]
    LimitedDeleteNeedsUniqueIdentity {
        /// The target that cannot render the limit, from its own identity.
        dialect: DialectId,
        /// The target table whose catalog snapshot had no safe identity.
        table: String,
    },
    /// The closed-AST expression renderer cannot lower a node (an unsupported /
    /// out-of-policy shape that the structural validator should have rejected
    /// first - this is the assembler's fail-closed backstop, never a silent
    /// emission). Carries a description of the offending node.
    #[error(
        "cannot render expression node ({0}); the structural validator must reject it before assembly"
    )]
    UnrenderableExpr(String),
    /// A `doUpdate` target column is absent from the inserted column list, on a
    /// target that cannot NAME a conflict target in its own grammar.
    ///
    /// Such a backend reconstructs the target match from the incoming row, so it needs
    /// the incoming VALUE of each target column to guard the update. A target column
    /// nobody inserted has no incoming value, so there is nothing to guard with.
    #[error(
        "{dialect} insert into {table:?} cannot safely apply `onConflict.doUpdate`: \
         target column {column:?} is not present in the inserted columns"
    )]
    ConflictTargetNotInserted {
        /// The target that refused it, from its own identity.
        dialect: DialectId,
        /// The target table.
        table: String,
        /// The target column without an incoming value.
        column: String,
    },
    /// A `doUpdate` assigns one of its OWN conflict-target columns, on a target whose
    /// duplicate-key assignments are evaluated in sequence.
    ///
    /// Changing a target value part-way through the list invalidates the target-match
    /// guard for every later assignment, so the render is refused rather than emitted
    /// with a guard that stops holding mid-statement.
    #[error(
        "{dialect} insert into {table:?} cannot safely apply `onConflict.doUpdate`: \
         assignment to target column {column:?} is not supported; update a \
         non-target column or split the migration into explicit steps"
    )]
    ConflictTargetAssigned {
        /// The target that refused it, from its own identity.
        dialect: DialectId,
        /// The target table.
        table: String,
        /// The conflict target column also present in `doUpdate`.
        column: String,
    },
    /// One assignment's right-hand side reads a column the same SET list also writes,
    /// on a target that evaluates a SET list SEQUENTIALLY rather than simultaneously.
    ///
    /// The authored meaning of a multi-column SET is that every RHS reads the ORIGINAL
    /// row. A backend that exposes an earlier assignment to a later one would compute
    /// a different answer without failing, so the render is refused. A SELF-reference
    /// stays legal on every target: its RHS is read before that column's sole
    /// assignment either way.
    #[error(
        "{dialect} {op} on {table:?} cannot preserve simultaneous SET semantics: \
         assignment to {column:?} reads assigned column {referenced_column:?}; \
         split the operation or compute the value without another assigned column"
    )]
    CrossAssignmentDependency {
        /// The target that refused it, from its own identity.
        dialect: DialectId,
        /// The authored operation (`update`, `backfill`, or `onConflict.doUpdate`).
        op: &'static str,
        /// The target table.
        table: String,
        /// The assignment whose RHS has the dependency.
        column: String,
        /// The other assigned column read by that RHS.
        referenced_column: String,
    },
    /// A target has no form that means exactly "on conflict, do nothing" for the
    /// authored conflict target, so an `onConflict` carrying no `doUpdate` is refused.
    ///
    /// `reason` is the REFUSING BACKEND'S own words for which of its near-misses fail
    /// and how. Core owns the shape of the refusal; naming the constructs would be core
    /// spelling one vendor's grammar for every target that ever hits this arm.
    #[error(
        "{dialect} cannot safely apply `onConflict` without a non-empty `doUpdate`: \
         {reason}"
    )]
    ConflictDoNothingNotExact {
        /// The target that refused it, from its own identity.
        dialect: DialectId,
        /// That target's own account of why its near-misses are not exact.
        reason: &'static str,
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
pub const MAX_BIND_PARAMS: usize = 65535;

/// Validate a bare SQL identifier and double-quote it for `dialect` (`"` -> `""` on
/// both shipping spellings). The ONLY identifier-emission path the assembler uses -
/// a schema-qualified / malformed name is rejected, so an injection through an
/// identifier slot cannot reach the DB. Bare-identifier validation mirrors
/// `crate::model::backfill::BackfillSpec`.
///
/// # There is no longer a PostgreSQL-pinned spelling of this, and that is the point
///
/// This function used to have two dialect-free wrappers, `quote_ident` and its
/// public-in-crate sibling `quote_bare_ident`, both of which passed
/// the PostgreSQL variant of the former closed dialect enum. They were safe only
/// for callers that were THEMSELVES
/// PostgreSQL-specific, and twice they were not: `render::backends::sqlite` quoted
/// all four of its identifier emissions with them, and after that was fixed
/// `render_sqlite_trigger_op` - then still in `render::lower`, since moved into
/// `render::backends::sqlite` - quoted all six of its trigger
/// identifiers with them. Both were correct SQL only because the two vendors spell
/// an identifier `"x"`, and both were hard blockers on extracting a
/// `zeroship-migrate-sqlite` crate that does not need `zeroship-migrate-postgres` AT
/// RUNTIME - a crate-extraction spike demonstrated the second one by rendering a
/// `createTrigger` from inside the extracted crate and getting PostgreSQL's marker
/// back.
///
/// With the trigger path routed through [`quote_bare_ident_for_backend`], rustc
/// reported both wrappers `never used`, so they are GONE rather than merely
/// unused: a dialect-free spelling that exists is a trap a future caller falls
/// into, and its absence is what makes "core hard-codes a dialect behind a
/// backend's back" unrepresentable here instead of merely absent today.
///
/// The fail-closed engine-identifier gate is separately exposed as
/// [`quote_ident_checked_for_backend`]. It also requires a backend explicitly;
/// there is no dialect-free spelling or default backend.
///
/// Pinned by `crates/zeroship-migrate/tests/dialect_matrix/sqlite_trigger_quoting_reaches_postgres.rs`.
pub fn quote_ident_for_backend(
    what: &'static str,
    ident: &str,
    backend: &dyn DmlRenderer,
) -> Result<String, DmlError> {
    let ok = !ident.is_empty()
        && ident.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(DmlError::InvalidIdentifier {
            what,
            value: ident.to_string(),
        });
    }
    Ok(escape_quote_ident_for_backend(ident, backend))
}

/// Public-in-crate wrapper for author-supplied bare identifiers. Trigger-body
/// rendering needs the same strict table/column/name gate as the DML assembler, and
/// must name the dialect it is rendering for - see [`quote_ident_for_backend`] for
/// why the dialect-free spelling of this was deleted rather than left unused.
pub fn quote_bare_ident_for_backend(
    what: &'static str,
    ident: &str,
    backend: &dyn DmlRenderer,
) -> Result<String, DmlError> {
    quote_ident_for_backend(what, ident, backend)
}

/// A render-seam rejection of an **engine-supplied identifier** (project schema,
/// migrator role, meta schema, ...) - the single fail-closed gate every engine
/// quoting seam routes through. Distinct from
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

/// The ONE canonical render seam for an **engine-supplied identifier** - the
/// project schema, the migrator role, the meta schema, a derived trigger name,
/// etc. Every engine quoting helper (`author` / `backfill` / `role` / `journal`
/// / `dml`) routes through this so all seams are **byte-identical** AND
/// **uniformly self-defending**.
///
/// Unlike a bare authored identifier ([`quote_ident_for_backend`]), an engine-supplied name
/// is NOT a bare `[A-Za-z_][A-Za-z0-9_]*` ident - under the Confined posture the
/// project schema is the app id (a `UUIDv7` carrying `-`). So it is emitted
/// escape-and-quote: double an embedded `"`, wrap in `"`.
///
/// The name is never author-supplied, but this seam still fails closed rather
/// than trust the caller: it refuses an empty string and any value carrying a
/// NUL byte - the one byte that `"`-doubling cannot neutralise (PG rejects NUL
/// inside an identifier outright). Everything else (including `"`) is rendered
/// safely by escaping, **byte-identically** to a bare
/// `format!("\"{}\"", x.replace('"', "\"\""))`.
/// There is deliberately no dialect-free wrapper. Vendor code passes its own
/// renderer, while engine code carrying a [`DialectId`]
/// resolves that registered backend through
/// `zeroship_migrate::render::dml::quote_ident_checked_for_dialect`.
pub fn quote_ident_checked_for_backend(
    ident: &str,
    backend: &dyn DmlRenderer,
) -> Result<String, IdentQuoteError> {
    if ident.is_empty() {
        return Err(IdentQuoteError {
            reason: "empty",
            value: ident.to_string(),
        });
    }
    if ident.contains('\0') {
        return Err(IdentQuoteError {
            reason: "contains NUL",
            value: ident.to_string(),
        });
    }
    Ok(escape_quote_ident_for_backend(ident, backend))
}

/* THERE IS NO UNROUTED RAW SPELLING PRIMITIVE IN THIS MODULE.
 *
 * A crate-private helper here that spelled `"x"` could be called by any module in
 * the crate without ever naming a vendor. Two of the three shipping dialects agree
 * on that spelling, so such a call is BYTE-CORRECT AND SILENT: no assertion about
 * emitted SQL can distinguish "SQLite quoted this" from "nobody quoted this and it
 * happened to look right". That is the whole defect class - an emission that
 * reaches NO renderer at all.
 *
 * The bytes have exactly one home, `crate::spelling::ansi_double_quote_ident`, and
 * a caller that wants them must pick a DOOR instead, because the door records the
 * vendor:
 *
 *   - EMIT for a named dialect  -> `escape_quote_ident_for_backend(x, d)`
 *   - constraint-definition normal form -> snapshot codec
 *
 * That rule was a privacy error while the primitive was module-private; across the
 * crate split it is a textual census instead, and the downgrade is recorded where
 * the primitive lives.
 */

/// EMIT an identifier in `dialect`'s own spelling, decided by that dialect's
/// backend rather than by a `format!` in core.
///
/// This is the door for anything that will be sent to a database. The other door,
/// the snapshot codec is for the normal form that is COMPARED rather than
/// executed; picking between them is the point of there being two.
pub fn escape_quote_ident_for_backend(ident: &str, backend: &dyn DmlRenderer) -> String {
    backend.quote_ident(ident)
}

/// Qualify a validated bare table name for the target dialect.
///
/// - **Postgres**: `"schema"."table"` - the project schema is engine-supplied
///   (never author-supplied) and the migrator's `search_path` is pinned to it, but
///   we qualify explicitly so the resolved relation is unambiguously the project's.
/// - **SQLite**: the table lives in the connection's `main` database (the app
///   file is `main`) - there is NO schema namespace, and a
///   `"schema"."table"` reference would resolve to a non-existent attached DB. So
///   the SQLite form is the BARE quoted table, matching the engine's UNqualified
///   SQLite DDL emission (the same property the createTable lowering relies on).
fn qualify_table(
    project_schema: &str,
    backend: &dyn DmlRenderer,
    table: &str,
) -> Result<String, DmlError> {
    backend.qualify_table(project_schema, table)
}

/// Map an [`IrScalar`] to a [`BindValue`] for native parameter binding - the
/// one-shot DML path. Safe `Int` and tagged `Int64` values become exact i64
/// binds; decimal strings carry through verbatim. Binary values stay binary on
/// SQLite; PostgreSQL and MySQL wrap a base64 text bind in a dialect decoder at
/// the placeholder site. Values are never inlined.
fn scalar_to_bind(s: &IrScalar) -> BindValue {
    match s {
        IrScalar::Null => BindValue::Null,
        IrScalar::Bool(b) => BindValue::Bool(*b),
        IrScalar::Int(i) | IrScalar::Int64(i) => BindValue::Int(*i),
        IrScalar::Decimal(d) => BindValue::Decimal(d.clone()),
        IrScalar::Str(s) => BindValue::Text(s.clone()),
        IrScalar::Bytes(bytes) => BindValue::Bytes(bytes.clone()),
    }
}

/* `pub fn placeholder(dialect, n)` USED TO LIVE HERE, and its doc claimed to be
 * "the SINGLE placeholder-emission point the one-shot assembler and the batched-
 * backfill SQLite executor both call". It was not: it had ZERO callers in
 * `crates/` or `packages/`, and had had none for as long as the two
 * places it named have existed. The one-shot assembler emits through
 * `BindCtx::push`, which asks its RESOLVED backend (`self.backend.placeholder(n)`);
 * the SQLite executor uses its own crate's helper, described below.
 *
 * It was `pub`, so the compiler could not report it unused, and it was counted as
 * one of the crate's dialect boundaries on the strength of that doc comment. It was
 * neither a boundary nor reachable - just a `renderer(dialect)` lookup that nothing
 * performed. 0.1.0 was never published and nothing outside this repo consumes the
 * crate, so deleting it costs nothing and stops the miscount recurring.
 *
 * A SQLite-named placeholder helper sat here too and its callers WERE real - both of
 * them inside `zeroship-migrate-sqlite`, one the `DmlRenderer::placeholder` impl and one
 * the batched backfill executor. Two callers in one vendor crate is that crate's
 * shared helper, not the contract's, so it is `crate::dml::placeholder` there now.
 */

/// Render a single inline SQL literal for the **backfill** string path
/// ([`render_expr_inline_for_backend`]). Numeric/bool literals print verbatim, except that an
/// exact decimal is quoted on SQLite to match its lossless TEXT storage. A string
/// is single-quoted with `''` doubling (the canonical SQL escape). The assembled
/// statement is then guard-checked by the real Postgres parser before any batch
/// runs, so a hostile literal cannot alter the statement shape past the parser.
/// NULL renders as the keyword. Bytes use native binary expressions on every
/// dialect so a backfill or column default never coerces them through text.
/// Render a SQL string literal using the canonical single-quote escape.
#[must_use]
pub fn sql_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Render an inline string without depending on a server's string-escape mode.
/// PostgreSQL and SQLite use standard quote doubling. MySQL uses a UTF-8 hex
/// literal so `NO_BACKSLASH_ESCAPES` (present or absent) cannot change either the
/// value or the statement shape.
pub fn inline_string_literal_for_backend(s: &str, backend: &dyn DmlRenderer) -> String {
    backend.inline_string_literal(s)
}

pub fn inline_literal_for_backend(
    s: &IrScalar,
    backend: &dyn DmlRenderer,
) -> Result<String, DmlError> {
    Ok(match s {
        IrScalar::Null => "NULL".to_string(),
        IrScalar::Bool(b) => {
            if *b {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        IrScalar::Int(i) | IrScalar::Int64(i) => i.to_string(),
        IrScalar::Decimal(d) => backend.inline_decimal_literal(d),
        IrScalar::Str(s) => backend.inline_string_literal(s),
        IrScalar::Bytes(bytes) => backend.inline_bytes_literal(bytes),
    })
}

/// Validate an in-list / regex text operand, then let the CALLER'S OWN backend
/// spell it.
///
/// It takes the backend rather than the former closed dialect enum because every
/// caller is a backend rendering for itself:
/// `backends/mysql.rs::render_regex_match` and
/// [`render_in_list_elem_portable`] below. Taking a dialect here meant core
/// resolving the registry to reach the vendor that had just called in - see
/// [`render_in_list_elem_portable`] for the whole shape. Core owns whether the
/// operand is LEGAL (non-empty, no NUL); the vendor owns how it is WRITTEN.
pub fn in_list_text_literal(
    s: &str,
    what: &'static str,
    backend: &dyn DmlRenderer,
) -> Result<String, DmlError> {
    if s.is_empty() {
        return Err(DmlError::UnrenderableExpr(format!(
            "{what} must be non-empty"
        )));
    }
    if s.contains('\0') {
        return Err(DmlError::UnrenderableExpr(format!(
            "{what} contains a NUL byte"
        )));
    }
    Ok(backend.inline_string_literal(s))
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
        IrScalar::Int(_) | IrScalar::Int64(_) | IrScalar::Decimal(_) => InListScalarKind::Number,
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

/// One in-list element in the spelling of the backend that asked, for the two
/// vendors whose in-list is a plain `IN (...)` over inline literals.
///
/// # It takes a BACKEND, and that is the whole point
///
/// This used to take `dialect` as the former closed dialect enum, and its only two callers -
/// `backends/sqlite.rs::render_in_list` and `backends/mysql.rs::render_in_list` -
/// handed it their own `DIALECT` const. Core then resolved that dialect back
/// through `crate::render::backends::renderer` to reach the very backend that had
/// called in. In the crate layout `docs/proposals/pluggable-backends.md` describes,
/// that reads
/// `zeroship-migrate-sqlite` -> core -> `zeroship-migrate-sqlite`: a dependency cycle in the
/// exact shape the crate split exists to remove, and one that emits byte-identical
/// SQL either way, so no behaviour test can see it. The backends now pass `self`,
/// and the round trip is gone.
///
/// # Why one backend keeps its own lookup-free helper instead of calling this
///
/// Not because it is exempt from the rule. PostgreSQL's in-list spellings are all
/// FIXED - `'x'::text` for a string, the decimal verbatim - so its element renderer
/// needs no vendor at all, and it is a private function in its own crate rather than
/// a `_pg`-suffixed one here. SQLite quotes decimals to match its lossless TEXT
/// storage and MySQL emits strings as a UTF-8 hex literal, so this helper genuinely
/// needs a vendor. It just needs the CALLER's, which the caller already is.
///
/// Pinned by `crates/zeroship-migrate/tests/dialect_matrix/dml_emitters_do_not_relookup_a_backend.rs`.
pub fn render_in_list_elem_portable(
    elem: &IrScalar,
    backend: &dyn DmlRenderer,
) -> Result<String, DmlError> {
    Ok(match elem {
        IrScalar::Str(s) => in_list_text_literal(s, "inList element", backend)?,
        IrScalar::Int(i) | IrScalar::Int64(i) => i.to_string(),
        IrScalar::Decimal(d) => backend.inline_decimal_literal(d),
        IrScalar::Bool(b) => {
            if *b {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
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
    backend: &dyn DmlRenderer,
) -> Result<String, DmlError> {
    if elems.is_empty() {
        return Ok(if negated { "TRUE" } else { "FALSE" }.to_string());
    }
    let kind = homogeneous_in_list_kind(elems)?.expect("non-empty list has kind");
    let joiner = if matches!(kind, InListScalarKind::Text) {
        ", "
    } else {
        ","
    };
    backend.render_in_list(expr, elems, negated, joiner)
}

/// The standard SQL keyword for an `EXTRACT` field.
///
/// A convenience a backend may CALL, not a decision made for it: which fields it
/// admits is its own `render_extract`, and a backend spelling a part differently
/// (SQLite reaches for `strftime`) simply does not use this.
#[must_use]
pub fn extract_field_name(field: ExtractField) -> &'static str {
    match field {
        ExtractField::Year => "year",
        ExtractField::Month => "month",
        ExtractField::Day => "day",
        ExtractField::Hour => "hour",
        ExtractField::Minute => "minute",
        ExtractField::Dow => "dow",
        ExtractField::Second => "second",
        ExtractField::Doy => "doy",
        ExtractField::Epoch => "epoch",
        ExtractField::Quarter => "quarter",
        ExtractField::Week => "week",
        ExtractField::Isodow => "isodow",
        ExtractField::Isoyear => "isoyear",
        ExtractField::Century => "century",
        ExtractField::Decade => "decade",
        ExtractField::Millennium => "millennium",
        ExtractField::Microseconds => "microseconds",
        ExtractField::Milliseconds => "milliseconds",
        ExtractField::Timezone => "timezone",
        ExtractField::TimezoneHour => "timezone_hour",
        ExtractField::TimezoneMinute => "timezone_minute",
    }
}

/// The SQL spelling of a binary operator (the method-to-node table). `Concat` is
/// `||` - the one place PG/SQLite NULL semantics agree.
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
fn render_binop(op: BinaryOp, l: &str, r: &str, backend: &dyn DmlRenderer) -> String {
    if matches!(op, BinaryOp::Concat) {
        backend.render_concat(l, r)
    } else {
        format!("({} {} {})", l, binary_op_sql(op), r)
    }
}

/// Render the portable `distinctFrom` NULL-safe inequality node, **dialect-aware**.
/// PG and SQLite both support the standard `IS DISTINCT FROM` operator directly.
/// MySQL has NO `IS DISTINCT FROM`, so the engine owns the lowering to
/// `NOT (<l> <=> <r>)` - `<=>` is MySQL's NULL-safe equality operator, so its
/// negation is exactly the "distinct from" (NULL-aware inequality) predicate.
fn render_distinct_from(l: &str, r: &str, backend: &dyn DmlRenderer) -> String {
    backend.render_distinct_from(l, r)
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
        // Portable scalar fns - identical spelling on PG/SQLite/MySQL.
        // `Mod` renders as the `%` OPERATOR, special-cased in `render_scalar_fn_call`
        // (SQLite has no `mod()` fn); this fallback name is never reached for it.
        ScalarFn::Mod => "mod",
        ScalarFn::Round => "round",
        ScalarFn::Floor => "floor",
        ScalarFn::Ceil => "ceil",
        ScalarFn::Substr => "substr",
        ScalarFn::Replace => "replace",
        // VENDOR scalars. `current_user` is a reserved keyword
        // rendered WITHOUT parens - the FnCall render arms special-case it; this
        // spelling is the fallback name.
        ScalarFn::CurrentSetting => "current_setting",
        ScalarFn::CurrentUser => "current_user",
    }
}

/// Render an allow-listed [`ScalarFn`] call from its already-rendered argument
/// fragments. Most are `<name>(<args>)`; the VENDOR `CurrentUser` is a bare
/// reserved keyword with NO parens (PG rejects `current_user()`).
fn render_scalar_fn_call(f: ScalarFn, args: &[String], backend: &dyn DmlRenderer) -> String {
    match f {
        ScalarFn::CurrentUser => "current_user".to_string(),
        // `mod` renders as the `%` OPERATOR - NOT a `mod(...)` call - because
        // SQLite has no `mod()` SQL function (`%` is universal on PG/SQLite/MySQL).
        // `args.join(" % ")` wrapped in parens is byte-identical to `(<a> % <b>)`
        // for the 2-arg case the builder produces, and never index-panics on a
        // malformed hand-crafted arity.
        ScalarFn::Mod => format!("({})", args.join(" % ")),
        // Every other spelling is the vendor's to answer. A backend returning
        // `None` takes the shared `<name>(<args>)` table below.
        _ => backend
            .render_scalar_fn_override(f, args)
            .unwrap_or_else(|| format!("{}({})", scalar_fn_sql(f), args.join(", "))),
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
/// `arg = None` (only `Count`) -> `count(*)`; `StringAgg` renders its
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
        None if matches!(f, AggFunc::ArrayAgg | AggFunc::BoolAnd | AggFunc::BoolOr) => Err(
            DmlError::UnrenderableExpr(format!("{} requires an argument", agg_fn_sql(f))),
        ),
        None => Ok(format!("{name}(*)")),
        Some(a) if distinct => Ok(format!("{name}(DISTINCT {a})")),
        Some(a) => Ok(format!("{name}({a})")),
    }
}

/// The portable cast-target SQL type per dialect. `bytes` is `BYTEA` on
/// PG / `BLOB` on SQLite; the rest share spelling.
fn cast_target_sql(
    target: zeroship_migrate_ir::expr::CastTarget,
    backend: &dyn DmlRenderer,
) -> &'static str {
    backend.cast_target(target)
}

/// Render a `c.fn.concatWs(delim, a, b, ...)` per dialect: PG `concat_ws`;
/// SQLite has no `concat_ws`, so it lowers to a NULL-skipping fold over `||` using
/// the pinned, portable shape. The args are already rendered fragments.
fn render_concat_ws(rendered: &[String], backend: &dyn DmlRenderer) -> String {
    backend.render_concat_ws(rendered)
}

/// Render the PINNED `c.fn.splitPart(col, d, n)` through the selected backend,
/// given the already-rendered `col_sql` fragment and the raw delimiter + `n` IR
/// args. The structural validator has already asked that backend for its
/// portability envelope. This shared seam enforces only the dialect-neutral
/// grammar as a fail-closed rendering backstop:
///
/// the delimiter must be a non-empty string literal and `n` a positive integer
/// literal - a non-literal delim/n, a non-string delim, an empty delim, or `n <= 0`
/// is unrenderable on every backend. Backend-specific lowering constraints stay
/// behind [`DmlRenderer::render_split_part`].
fn render_split_part(
    col_sql: &str,
    delim_arg: &Expr,
    n_arg: &Expr,
    backend: &dyn DmlRenderer,
) -> Result<String, DmlError> {
    // GRAMMAR (every backend): a string-literal delimiter and positive integer
    // literal n. Other shapes are unrenderable everywhere.
    let delim = match delim_arg {
        Expr::Literal {
            value: IrScalar::Str(s),
        } if !s.is_empty() => s.as_str(),
        Expr::Literal {
            value: IrScalar::Str(_),
        } => {
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
        Expr::Literal {
            value: IrScalar::Int(n),
        } if *n >= 1 => *n,
        Expr::Literal {
            value: IrScalar::Int(n),
        } => {
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

    backend.render_split_part(col_sql, delim, n)
}

/// Select the [`Expr::Dialectal`] leg to render for `dialect`: the
/// target dialect's OWN leg. Returns a borrow of the chosen leg. This is the one
/// leg-selection rule shared by both the bound
/// and inline render paths.
///
/// A `Dialectal` with no own leg for the target is
/// UNREACHABLE here because the engine's structural validator refuses it
/// per-target (`EXPR_NOT_PORTABLE`) before assembly - but the seam is fail-closed
/// defensively: it returns [`DmlError::UnrenderableExpr`] rather than silently
/// dropping the value.
///
/// # Why this compares ids instead of matching a variant
///
/// `dialect` is an OPEN [`DialectId`], and the leg map is keyed by that same
/// identity. A fourth backend therefore selects its own key without core naming
/// it or silently borrowing another backend's value.
pub fn select_dialect_leg(
    dialect: DialectId,
    legs: &BTreeMap<DialectId, Box<Expr>>,
) -> Result<&Expr, DmlError> {
    legs.get(&dialect).map(Box::as_ref).ok_or_else(|| {
        DmlError::UnrenderableExpr(format!(
            "dialect() has no leg for the {} target — the structural validator \
             must refuse this before assembly",
            dialect.as_str()
        ))
    })
}

/// Collect every column an expression reads AS RENDERED FOR `dialect`, in sorted
/// order.
///
/// The offline half of the `DropColumn` CHECK cascade: PostgreSQL drops a CHECK
/// whenever any column its expression references is dropped, so the fold has to
/// know that column set exactly. Reading it back out of the rendered SQL text is
/// not an option - `CHECK ((status <> 'qty'::text))` must NOT cascade on a `qty`
/// column, and `CHECK (((a)::text <> ''::text))` carries a bare `text` token that
/// is a cast type rather than a column. Walking the closed AST answers both
/// exactly.
///
/// `Expr::Dialectal` descends ONLY into the leg `select_dialect_leg` picks,
/// matching what [`render_expr_inline_for_backend`] actually emits. Unioning all legs would
/// attribute a PostgreSQL CHECK to a column that appears only in the SQLite or
/// MySQL leg and cascade away a constraint PostgreSQL kept.
///
/// Deliberately NOT shared with the `collect_col_refs` inside
/// `crate::render::lower::derived_check_constraint_name`, which unions every leg
/// because a derived constraint NAME has to stay stable across dialects. Same walk,
/// opposite requirement.
///
/// The match has no catch-all arm so a new [`Expr`] variant is a compile error here
/// rather than a silently missed column reference.
pub fn expr_column_refs_for_backend(
    expr: &Expr,
    backend: &dyn DmlRenderer,
) -> Result<Vec<String>, DmlError> {
    fn walk(
        expr: &Expr,
        backend: &dyn DmlRenderer,
        out: &mut std::collections::BTreeSet<String>,
    ) -> Result<(), DmlError> {
        match expr {
            Expr::ColRef { name, .. } => {
                out.insert(name.clone());
            }
            Expr::Literal { .. } | Expr::UuidV4 | Expr::UuidV7 | Expr::Interval { .. } => {}
            Expr::BinOp { lhs, rhs, .. } => {
                walk(lhs, backend, out)?;
                walk(rhs, backend, out)?;
            }
            Expr::UnaryOp { operand, .. }
            | Expr::Cast { operand, .. }
            | Expr::StorageSize { expr: operand }
            | Expr::Extract { from: operand, .. }
            | Expr::RegexMatch { expr: operand, .. }
            | Expr::InList { expr: operand, .. } => walk(operand, backend, out)?,
            Expr::Case { branches, r#else } => {
                for branch in branches {
                    walk(&branch.when, backend, out)?;
                    walk(&branch.then, backend, out)?;
                }
                if let Some(r#else) = r#else {
                    walk(r#else, backend, out)?;
                }
            }
            Expr::FnCall { args, .. } | Expr::FnSynth { args, .. } => {
                for arg in args {
                    walk(arg, backend, out)?;
                }
            }
            Expr::Between { operand, low, high } => {
                walk(operand, backend, out)?;
                walk(low, backend, out)?;
                walk(high, backend, out)?;
            }
            Expr::Like { operand, pattern } => {
                walk(operand, backend, out)?;
                walk(pattern, backend, out)?;
            }
            Expr::DistinctFrom { left, right } => {
                walk(left, backend, out)?;
                walk(right, backend, out)?;
            }
            Expr::Agg { arg, delimiter, .. } => {
                if let Some(arg) = arg {
                    walk(arg, backend, out)?;
                }
                if let Some(delimiter) = delimiter {
                    walk(delimiter, backend, out)?;
                }
            }
            Expr::Dialectal { legs } => {
                walk(select_dialect_leg(backend.dialect(), legs)?, backend, out)?;
            }
        }
        Ok(())
    }

    let mut out = std::collections::BTreeSet::new();
    walk(expr, backend, &mut out)?;
    Ok(out.into_iter().collect())
}

/// A bind accumulator carried through the parameterized render walk: it owns the
/// running placeholder counter (1-based, dialect-specific) and the ordered
/// [`BindValue`] list.
#[derive(Debug)]
pub struct BindCtx<'a> {
    /// The vendor this walk is rendering for.
    ///
    /// It used to carry `dialect` as the former closed dialect enum ALONGSIDE the
    /// backend, with a note saying `dialect` survived only because the sibling doors
    /// (`quote_ident_for_backend`, `inline_literal_for_backend`, ...) still took one and had
    /// callers outside `render::`. The crate split is the "later step" that note
    /// anticipated: those doors take a `&dyn DmlRenderer` now, so the second field
    /// had nothing left to answer and is gone. Where the walk genuinely needs the
    /// dialect - a capability question, a dialectal-leg selection - it reads
    /// [`DmlRenderer::dialect`].
    pub backend: &'a dyn DmlRenderer,
    /// The ordered binds accumulated by the walk.
    pub binds: Vec<BindValue>,
}

impl<'a> BindCtx<'a> {
    pub fn new(backend: &'a dyn DmlRenderer) -> Self {
        Self {
            backend,
            binds: Vec::new(),
        }
    }

    /// Append a bound scalar and return its dialect placeholder.
    pub fn push_bind(&mut self, b: BindValue) -> String {
        self.binds.push(b);
        self.backend.placeholder(self.binds.len())
    }

    /// Bind one typed IR scalar without losing binary values.
    ///
    /// The binary case is the VENDOR's to answer - which carrier the value
    /// travels in, and what (if anything) wraps the placeholder - so it goes
    /// through [`DmlRenderer::bind_bytes`]. This method used to make that choice
    /// itself, with a three-way `match` on `self.backend.dialect()` spelling
    /// `decode(.., 'base64')` / `FROM_BASE64(..)` / raw bytes in core.
    fn push_scalar(&mut self, value: &IrScalar) -> String {
        if let IrScalar::Bytes(bytes) = value {
            let backend = self.backend;
            return backend.bind_bytes(bytes, &mut |b| self.push_bind(b));
        }
        self.push_bind(scalar_to_bind(value))
    }
}

/// Render a closed-AST [`Expr`] to a parameterized SQL fragment, appending a
/// native bind for every [`Expr::Literal`] (the one-shot DML path). A `ColRef`
/// renders to its quoted identifier; a `Literal` to a placeholder; operators /
/// functions / casts to their SQL spelling. The bind safety property:
/// statement structure is fixed by the AST shape, never by a literal's content.
pub fn render_expr_bound(expr: &Expr, ctx: &mut BindCtx) -> Result<String, DmlError> {
    Ok(match expr {
        Expr::ColRef { name, table } => match table {
            // Qualified ref (`c("orders", "id")`): `<quoted table>.<quoted col>`,
            // both halves through the same per-dialect identifier quoting.
            Some(t) => format!(
                "{}.{}",
                quote_ident_for_backend("table", t, ctx.backend)?,
                quote_ident_for_backend("column", name, ctx.backend)?
            ),
            None => quote_ident_for_backend("column", name, ctx.backend)?,
        },
        Expr::Literal { value } => ctx.push_scalar(value),
        Expr::BinOp { op, lhs, rhs } => {
            let l = render_expr_bound(lhs, ctx)?;
            let r = render_expr_bound(rhs, ctx)?;
            render_binop(*op, &l, &r, ctx.backend)
        }
        Expr::UnaryOp { op, operand } => {
            let o = render_expr_bound(operand, ctx)?;
            render_unary(*op, &o, ctx.backend)
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
            render_scalar_fn_call(*r#fn, &rs, ctx.backend)
        }
        Expr::FnSynth { r#fn, args } => render_synth_bound(*r#fn, args, ctx)?,
        Expr::UuidV4 => ctx.backend.uuid_v4(),
        Expr::UuidV7 => ctx.backend.uuid_v7()?,
        Expr::Cast { operand, target } => {
            let o = render_expr_bound(operand, ctx)?;
            format!("CAST({o} AS {})", cast_target_sql(*target, ctx.backend))
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
            render_distinct_from(&l, &r, ctx.backend)
        }
        Expr::Agg {
            func,
            arg,
            delimiter,
            distinct,
        } => {
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
        Expr::InList {
            expr,
            elems,
            negated,
        } => {
            let e = render_expr_bound(expr, ctx)?;
            render_in_list(&e, elems, *negated, ctx.backend)?
        }
        Expr::RegexMatch { expr, pattern } => {
            let e = render_expr_bound(expr, ctx)?;
            ctx.backend.render_regex_match(&e, pattern)?
        }
        Expr::StorageSize { expr } => {
            let e = render_expr_bound(expr, ctx)?;
            ctx.backend.render_storage_size(&e)?
        }
        Expr::Extract { field, from } => {
            let e = render_expr_bound(from, ctx)?;
            ctx.backend.render_extract(*field, &e)?
        }
        Expr::Interval { duration } => ctx.backend.render_interval(duration)?,
        Expr::Dialectal { legs } => {
            let leg = select_dialect_leg(ctx.backend.dialect(), legs)?;
            render_expr_bound(leg, ctx)?
        }
    })
}

/// Render a `FnSynth` in the parameterized path. `concatWs` and `splitPart` lower
/// per dialect via the portable-helper renderers; `now` renders to the apply-time
/// DB scalar. Exact UUID generators are direct expression variants rather than
/// synthesized-function tokens.
fn render_synth_bound(f: SynthFn, args: &[Expr], ctx: &mut BindCtx) -> Result<String, DmlError> {
    match f {
        SynthFn::ConcatWs => {
            let mut rs = Vec::with_capacity(args.len());
            for a in args {
                rs.push(render_expr_bound(a, ctx)?);
            }
            Ok(render_concat_ws(&rs, ctx.backend))
        }
        SynthFn::Now => Ok(ctx.backend.synth_now()),
        SynthFn::SplitPart => {
            // splitPart(col, delim, n): the column arg may itself be a ColRef or an
            // in-AST sub-expression - render it (binding any nested Literals), then
            // extract the dialect-neutral literal grammar. The delim/n are
            // engine-pinned constants of the lowering, NOT binds.
            if args.len() != 3 {
                return Err(DmlError::UnrenderableExpr(format!(
                    "c.fn.splitPart takes exactly (column, delim, n); got {} args",
                    args.len()
                )));
            }
            let col_sql = render_expr_bound(&args[0], ctx)?;
            render_split_part(&col_sql, &args[1], &args[2], ctx.backend)
        }
    }
}

/// Render one assignment/row value through the bound-expression walk.
///
/// Required vendor conflict renderers use this neutral door so their values join
/// the caller's bind sequence without reproducing expression rendering.
pub fn render_value_bound(value: &IrValue, ctx: &mut BindCtx) -> Result<String, DmlError> {
    match value {
        IrValue::Scalar(s) => Ok(ctx.push_scalar(s)),
        IrValue::Expr(e) => render_expr_bound(e, ctx),
    }
}

/// Render a unary op around an already-rendered operand, dialect-aware.
fn render_unary(op: UnaryOp, operand: &str, backend: &dyn DmlRenderer) -> String {
    match op {
        UnaryOp::Not => format!("(NOT {operand})"),
        UnaryOp::IsNull => format!("({operand} IS NULL)"),
        UnaryOp::IsNotNull => format!("({operand} IS NOT NULL)"),
        // The boolean predicates are the vendor's to spell: SQLite has no native
        // boolean type (values are 0/1) and rejects `IS TRUE` / `IS FALSE` at apply.
        UnaryOp::IsTrue => backend.render_is_true(operand),
        UnaryOp::IsFalse => backend.render_is_false(operand),
    }
}

/// Render a closed-AST [`Expr`] to an INLINE SQL string (the backfill path). A
/// `ColRef` is its quoted identifier; a `Literal` is an inline SQL literal
/// ([`inline_literal_for_backend`], guard-revalidated downstream); operators / functions /
/// casts render the same SQL spelling as the bound path. NO binds - the backfill
/// executor pages the statement and cannot carry positional binds.
pub fn render_expr_inline_for_backend(
    expr: &Expr,
    backend: &dyn DmlRenderer,
) -> Result<String, DmlError> {
    render_expr_inline_with_col_for_backend(expr, backend, &|name| {
        quote_ident_for_backend("column", name, backend)
    })
}

pub fn render_value_inline_for_backend(
    value: &IrValue,
    backend: &dyn DmlRenderer,
) -> Result<String, DmlError> {
    match value {
        IrValue::Scalar(s) => inline_literal_for_backend(s, backend),
        IrValue::Expr(e) => render_expr_inline_for_backend(e, backend),
    }
}

/// The DOOR into the inline walk: it RESOLVES the backend once and hands it to
/// the recursive worker.
///
/// Its callers - in `render::lower`, `render::declarative`, `model::` and
/// `apply::` - hold an open [`DialectId`] and
/// resolve their backend from it. What the split buys is that the RECURSION
/// carries a resolved backend instead of re-deriving it from the dialect at each
/// node, which is the same arrangement [`BindCtx`] gives the bound walk.
pub fn render_expr_inline_with_col_for_backend<F>(
    expr: &Expr,
    backend: &dyn DmlRenderer,
    col_ref: &F,
) -> Result<String, DmlError>
where
    F: Fn(&str) -> Result<String, DmlError>,
{
    render_expr_inline_walk_for_backend(expr, backend, col_ref)
}

fn render_expr_inline_walk_for_backend<F>(
    expr: &Expr,
    backend: &dyn DmlRenderer,
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
                quote_ident_for_backend("table", t, backend)?,
                col_ref(name)?
            ),
            None => col_ref(name)?,
        },
        Expr::Literal { value } => inline_literal_for_backend(value, backend)?,
        Expr::BinOp { op, lhs, rhs } => {
            let l = render_expr_inline_walk_for_backend(lhs, backend, col_ref)?;
            let r = render_expr_inline_walk_for_backend(rhs, backend, col_ref)?;
            render_binop(*op, &l, &r, backend)
        }
        Expr::UnaryOp { op, operand } => render_unary(
            *op,
            &render_expr_inline_walk_for_backend(operand, backend, col_ref)?,
            backend,
        ),
        Expr::Case { branches, r#else } => {
            let mut s = String::from("CASE");
            for b in branches {
                let c = render_expr_inline_walk_for_backend(&b.when, backend, col_ref)?;
                let r = render_expr_inline_walk_for_backend(&b.then, backend, col_ref)?;
                s.push_str(&format!(" WHEN {c} THEN {r}"));
            }
            if let Some(e) = r#else {
                s.push_str(&format!(
                    " ELSE {}",
                    render_expr_inline_walk_for_backend(e, backend, col_ref)?
                ));
            }
            s.push_str(" END");
            s
        }
        Expr::FnCall { r#fn, args } => {
            let rs: Result<Vec<_>, _> = args
                .iter()
                .map(|a| render_expr_inline_walk_for_backend(a, backend, col_ref))
                .collect();
            render_scalar_fn_call(*r#fn, &rs?, backend)
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
                let col_sql = render_expr_inline_walk_for_backend(&args[0], backend, col_ref)?;
                render_split_part(&col_sql, &args[1], &args[2], backend)?
            }
            SynthFn::ConcatWs => {
                let rs: Result<Vec<_>, _> = args
                    .iter()
                    .map(|a| render_expr_inline_walk_for_backend(a, backend, col_ref))
                    .collect();
                render_concat_ws(&rs?, backend)
            }
            SynthFn::Now => backend.synth_now(),
        },
        Expr::UuidV4 => backend.uuid_v4(),
        Expr::UuidV7 => backend.uuid_v7()?,
        Expr::Cast { operand, target } => {
            format!(
                "CAST({} AS {})",
                render_expr_inline_walk_for_backend(operand, backend, col_ref)?,
                cast_target_sql(*target, backend)
            )
        }
        Expr::Between { operand, low, high } => {
            let o = render_expr_inline_walk_for_backend(operand, backend, col_ref)?;
            let lo = render_expr_inline_walk_for_backend(low, backend, col_ref)?;
            let hi = render_expr_inline_walk_for_backend(high, backend, col_ref)?;
            format!("({o} BETWEEN {lo} AND {hi})")
        }
        Expr::Like { operand, pattern } => {
            let o = render_expr_inline_walk_for_backend(operand, backend, col_ref)?;
            let p = render_expr_inline_walk_for_backend(pattern, backend, col_ref)?;
            format!("({o} LIKE {p})")
        }
        Expr::DistinctFrom { left, right } => {
            let l = render_expr_inline_walk_for_backend(left, backend, col_ref)?;
            let r = render_expr_inline_walk_for_backend(right, backend, col_ref)?;
            render_distinct_from(&l, &r, backend)
        }
        Expr::Agg {
            func,
            arg,
            delimiter,
            distinct,
        } => {
            let a = match arg {
                Some(e) => Some(render_expr_inline_walk_for_backend(e, backend, col_ref)?),
                None => None,
            };
            let d = match delimiter {
                Some(e) => Some(render_expr_inline_walk_for_backend(e, backend, col_ref)?),
                None => None,
            };
            render_agg(*func, a.as_deref(), d.as_deref(), *distinct)?
        }
        Expr::InList {
            expr,
            elems,
            negated,
        } => {
            let e = render_expr_inline_walk_for_backend(expr, backend, col_ref)?;
            render_in_list(&e, elems, *negated, backend)?
        }
        Expr::RegexMatch { expr, pattern } => {
            let e = render_expr_inline_walk_for_backend(expr, backend, col_ref)?;
            backend.render_regex_match(&e, pattern)?
        }
        Expr::StorageSize { expr } => {
            let e = render_expr_inline_walk_for_backend(expr, backend, col_ref)?;
            backend.render_storage_size(&e)?
        }
        Expr::Extract { field, from } => {
            let e = render_expr_inline_walk_for_backend(from, backend, col_ref)?;
            backend.render_extract(*field, &e)?
        }
        Expr::Interval { duration } => backend.render_interval(duration)?,
        Expr::Dialectal { legs } => {
            let leg = select_dialect_leg(backend.dialect(), legs)?;
            render_expr_inline_walk_for_backend(leg, backend, col_ref)?
        }
    })
}

/// Render a CLOSED [`Expr`] predicate to an inline SQL fragment in the CALLER'S
/// OWN spelling, for a DDL position that carries no binds.
///
/// The two positions in this tree are a policy `USING` / `WITH CHECK` clause and a
/// trigger `WHEN` clause. Both are part of a catalog DEFINITION rather than a
/// parameterized statement, so there is nowhere to bind and the inline renderer is
/// the right seam: a `ColRef` becomes the caller's quoted identifier and a `Literal`
/// its inline literal. A backend that guards its own emitted SQL re-parses the whole
/// statement afterwards, so the inline literals are re-validated before any apply.
///
/// # Errors
/// [`DmlError::UnrenderableExpr`] for an expression node that has no inline form
/// (e.g. a `bytes` literal).
/// THE TWO PINNED SPELLINGS COLLAPSED INTO ONE PARAMETERIZED DOOR, and that is a
/// simplification the split forced rather than a loss. The two vendor-named
/// predicate renderers this replaced had identical bodies apart from the dialect
/// literal, so each was a vendor writing its own name into a helper it then called
/// on itself.
/// A vendor now passes `self`: `zeroship-migrate-postgres` for the vendor
/// `CREATE POLICY` / `CREATE TRIGGER` clauses, `zeroship-migrate-sqlite` for its trigger
/// bodies. Neither can reach the other's spelling any more, which is the property
/// `sqlite_trigger_quoting_reaches_postgres.rs` exists to protect.
pub fn render_predicate(expr: &Expr, backend: &dyn DmlRenderer) -> Result<String, DmlError> {
    render_expr_inline_for_backend(expr, backend)
}

/// The structured `onConflict` facet of an `insert`.
#[derive(Debug, Clone, PartialEq)]
pub struct OnConflict {
    /// The conflict-target columns (`ON CONFLICT (cols)`).
    pub columns: Vec<String>,
    /// `Some` SET assignments => `DO UPDATE SET ...`; `None` => `DO NOTHING`.
    pub do_update: Option<BTreeMap<String, IrValue>>,
}

/// Neutral, prevalidated inputs to one backend's conflict-clause renderer.
#[derive(Debug, Clone, Copy)]
pub struct OnConflictRenderRequest<'a> {
    /// The authored bare table name, used in diagnostics.
    pub table: &'a str,
    /// The backend-qualified target table already emitted by this renderer.
    pub qualified_table: &'a str,
    /// The authored insert-column order.
    pub insert_columns: &'a [String],
    /// The authored structured conflict request.
    pub on_conflict: &'a OnConflict,
    /// Conflict-target columns quoted by this same backend, in authored order.
    pub quoted_target_columns: &'a [String],
}

/// Neutral inputs to one backend's row-limited DELETE renderer.
#[derive(Debug, Clone, Copy)]
pub struct LimitedDeleteRenderRequest<'a> {
    /// The authored bare table name, used in diagnostics.
    pub table: &'a str,
    /// The backend-qualified target table.
    pub qualified_table: &'a str,
    /// The already-rendered predicate body.
    pub rendered_where: &'a str,
    /// The authored row limit.
    pub limit: u64,
    /// A catalog-proven identity, when the backend needs one.
    pub catalog_identity_columns: Option<&'a [String]>,
}

/// The assembled one-shot DML statement: the placeholder template + ordered binds.
/// Fed straight into `PlanStep::Dml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledDml {
    /// The placeholder SQL (`$n`/`?n` - never an inlined value).
    pub template: String,
    /// The ordered native binds.
    pub binds: Vec<BindValue>,
}

/// Assemble an `insert` op into a parameterized one-shot statement. Every
/// value is a native bind. PostgreSQL and SQLite render an exact conflict target;
/// MySQL renders its safe native duplicate-key form.
///
/// # Errors
/// [`DmlError`] on a malformed identifier, an empty-or-ragged insert, or a MySQL
/// conflict shape that cannot retain target intent safely.
pub fn assemble_insert_for_backend(
    project_schema: &str,
    backend: &dyn DmlRenderer,
    table: &str,
    columns: &[String],
    rows: &[Vec<IrValue>],
    on_conflict: Option<&OnConflict>,
) -> Result<AssembledDml, DmlError> {
    let dialect = backend.dialect();
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
    let mut ctx = BindCtx::new(backend);
    let qtable = qualify_table(project_schema, ctx.backend, table)?;
    let qcols: Result<Vec<_>, _> = columns
        .iter()
        .map(|c| quote_ident_for_backend("column", c, backend))
        .collect();
    let qcols = qcols?;

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
        let placeholders: Result<Vec<String>, DmlError> = row
            .iter()
            .map(|v| render_value_bound(v, &mut ctx))
            .collect();
        let placeholders = placeholders?;
        value_groups.push(format!("({})", placeholders.join(", ")));
    }

    let mut template = format!(
        "INSERT INTO {qtable} ({}) VALUES {}",
        qcols.join(", "),
        value_groups.join(", ")
    );

    if let Some(oc) = on_conflict {
        if !backend.supports(Capability::InsertOnConflictClause) {
            return Err(DmlError::UnrenderableExpr(format!(
                "structured onConflict is unavailable for the {} target",
                dialect.as_str()
            )));
        }
        if oc.columns.is_empty() {
            return Err(DmlError::MalformedInsert {
                table: "<onConflict>".to_string(),
                reason: "onConflict carries no target columns".to_string(),
            });
        }
        let quoted_target_columns = oc
            .columns
            .iter()
            .map(|column| quote_ident_for_backend("column", column, backend))
            .collect::<Result<Vec<_>, _>>()?;
        template.push_str(&backend.render_on_conflict(
            OnConflictRenderRequest {
                table,
                qualified_table: &qtable,
                insert_columns: columns,
                on_conflict: oc,
                quoted_target_columns: &quoted_target_columns,
            },
            &mut ctx,
        )?);
    }

    if ctx.binds.len() > MAX_BIND_PARAMS {
        return Err(DmlError::TooManyBinds {
            table: table.to_string(),
            count: ctx.binds.len(),
            max: MAX_BIND_PARAMS,
        });
    }

    Ok(AssembledDml {
        template,
        binds: ctx.binds,
    })
}

/// Assemble a one-shot `update` op (no `batch`) into a parameterized statement.
/// `set` RHS values render through `render_value_bound` and the optional `where`
/// renders through `render_expr_bound`, so scalar set values and expression
/// literals are native binds. Portable on both backends.
///
/// # Errors
/// [`DmlError`] on a malformed identifier / empty `set` / an unrenderable node.
pub fn assemble_update_for_backend(
    project_schema: &str,
    backend: &dyn DmlRenderer,
    table: &str,
    set: &BTreeMap<String, IrValue>,
    r#where: Option<&Expr>,
) -> Result<AssembledDml, DmlError> {
    if set.is_empty() {
        return Err(DmlError::EmptySet {
            op: "update",
            table: table.to_string(),
        });
    }
    let mut ctx = BindCtx::new(backend);
    let qtable = qualify_table(project_schema, ctx.backend, table)?;
    backend.validate_assignment_semantics("update", table, set)?;
    // BTreeMap => deterministic, canonical assignment order.
    let mut assigns = Vec::with_capacity(set.len());
    for (col, rhs) in set {
        let qc = quote_ident_for_backend("column", col, backend)?;
        let r = render_value_bound(rhs, &mut ctx)?;
        assigns.push(format!("{qc} = {r}"));
    }
    let mut template = format!("UPDATE {qtable} SET {}", assigns.join(", "));
    if let Some(pred) = r#where {
        let w = render_expr_bound(pred, &mut ctx)?;
        template.push_str(&format!(" WHERE {w}"));
    }
    Ok(AssembledDml {
        template,
        binds: ctx.binds,
    })
}

/// Assemble a `del` op into a parameterized `DELETE`. PostgreSQL limited deletes
/// identify a physical row with `(tableoid, ctid)`, including when the target is
/// a partitioned parent. MySQL uses its native limit clause. A SQLite limited
/// delete is rejected here because this schema-blind entry cannot prove a safe
/// row identity; the catalog-aware IR lower uses the internal identity-bearing
/// entry after introspection. The limit-free form is a plain delete everywhere.
///
/// # Errors
/// [`DmlError`] on a malformed identifier, an unrenderable predicate, or a
/// SQLite limit without catalog identity facts.
pub fn assemble_delete_for_backend(
    project_schema: &str,
    backend: &dyn DmlRenderer,
    table: &str,
    r#where: &Expr,
    limit: Option<u64>,
) -> Result<AssembledDml, DmlError> {
    assemble_delete_with_catalog_identity_for_backend(
        project_schema,
        backend,
        table,
        r#where,
        limit,
        None,
    )
}

/// Catalog-aware delete assembly used by the guarded IR lower. The SQLite
/// identity columns must be a non-null PRIMARY KEY or complete non-partial UNIQUE
/// key recovered from the live table snapshot.
pub fn assemble_delete_with_catalog_identity_for_backend(
    project_schema: &str,
    backend: &dyn DmlRenderer,
    table: &str,
    r#where: &Expr,
    limit: Option<u64>,
    catalog_identity_columns: Option<&[String]>,
) -> Result<AssembledDml, DmlError> {
    let mut ctx = BindCtx::new(backend);
    let qtable = qualify_table(project_schema, ctx.backend, table)?;
    let w = render_expr_bound(r#where, &mut ctx)?;
    let template = match limit {
        None => format!("DELETE FROM {qtable} WHERE {w}"),
        Some(limit) => backend.render_limited_delete(
            LimitedDeleteRenderRequest {
                table,
                qualified_table: &qtable,
                rendered_where: &w,
                limit,
                catalog_identity_columns,
            },
            &mut ctx,
        )?,
    };
    Ok(AssembledDml {
        template,
        binds: ctx.binds,
    })
}

/// The rendered backfill clauses (the SQL strings the
/// `BackfillSpec` carries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillClauses {
    /// The `SET` body (e.g. `"normalized" = lower("raw")`).
    pub set_clause: String,
    /// The optional extra `WHERE` conjunct.
    pub filter: Option<String>,
}

/// Assemble a `backfill` op's `set` / `filter` into the inline SQL strings the
/// `BackfillSpec` executor consumes. Renders for
/// EITHER dialect: the inline transform is dialect-rendered (the
/// `c.fn.splitPart` lowering, NULL-skipping `concatWs`), and the PG (`backfill.rs`)
/// or SQLite (`zeroship_migrate_sqlite::backend::backfill_sql`) executor consumes the result.
///
/// # Errors
/// [`DmlError`] on a malformed identifier / empty `set` / an unrenderable node.
pub fn assemble_backfill_clauses_for_backend(
    backend: &dyn DmlRenderer,
    table: &str,
    set: &BTreeMap<String, IrValue>,
    filter: Option<&Expr>,
) -> Result<BackfillClauses, DmlError> {
    assemble_backfill_clauses_inner_for_backend(backend, table, set, filter, false)
}

/// Assemble the ordinary portion of a backfill that also carries apply-engine
/// per-row assignments. The ordinary map may be empty because the executor will
/// append the separately carried generator assignments at batch time.
pub fn assemble_backfill_clauses_allow_empty_for_backend(
    backend: &dyn DmlRenderer,
    table: &str,
    set: &BTreeMap<String, IrValue>,
    filter: Option<&Expr>,
) -> Result<BackfillClauses, DmlError> {
    assemble_backfill_clauses_inner_for_backend(backend, table, set, filter, true)
}

fn assemble_backfill_clauses_inner_for_backend(
    backend: &dyn DmlRenderer,
    table: &str,
    set: &BTreeMap<String, IrValue>,
    filter: Option<&Expr>,
    allow_empty: bool,
) -> Result<BackfillClauses, DmlError> {
    if set.is_empty() && !allow_empty {
        return Err(DmlError::EmptySet {
            op: "backfill",
            table: table.to_string(),
        });
    }
    backend.validate_assignment_semantics("backfill", table, set)?;
    // BTreeMap => canonical order.
    let mut assigns = Vec::with_capacity(set.len());
    for (col, rhs) in set {
        let qc = quote_ident_for_backend("column", col, backend)?;
        let r = render_value_inline_for_backend(rhs, backend)?;
        assigns.push(format!("{qc} = {r}"));
    }
    let set_clause = assigns.join(", ");
    let filter = match filter {
        Some(f) => Some(render_expr_inline_for_backend(f, backend)?),
        None => None,
    };
    Ok(BackfillClauses { set_clause, filter })
}
