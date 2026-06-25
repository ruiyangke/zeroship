//! The creator-**DML assembler** + the closed-AST **expression renderer** (design
//! §PR6a / §2.3 / §3.3.1.1 / §9).
//!
//! `IrAuthor::lower` compiles the DDL ops (`createTable`/`alter*`/…) into the
//! same [`Migration`](crate::migration::Migration) shape the declarative differ
//! emits. This module is the peer for the **DML** ops — `insert` / `update` /
//! `del` / `backfill` — and for the closed expression AST ([`crate::expr::Expr`])
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
//!    [`PlanStep::Dml`](crate::plan::PlanStep::Dml) `binds` vector, NEVER
//!    string-interpolated. So a value containing a quote / semicolon / comment
//!    cannot change the *shape* of the statement on either backend (§2.3.2 the
//!    bind-safety property). The expression renderer ([`render_expr_bound`]) walks
//!    the closed AST and appends a placeholder for each [`Expr::Literal`].
//!
//! 2. **Batched backfill** ([`assemble_backfill`]). The existing
//!    [`BackfillSpec`](crate::backfill::BackfillSpec) executor (PG `backfill.rs`)
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
//! file, §2.5.1) and is referenced UNqualified, matching the engine's SQLite DDL.
//!
//! # Portability boundary (§9)
//!
//! - `insert { onConflict }` renders natively on **Postgres**; on **SQLite** it is
//!   a hard authoring error ([`DmlError::OnConflictNotPortable`], surfaced as
//!   `dialect_scope = PgOnly` / `UNSUPPORTED { kind: "op" }`) — there is NO raw
//!   route (property A) and we never silently drop the conflict clause.
//! - A **batched** `backfill` (and an `update { batch }`) targets the
//!   `BackfillSpec` executor, PORTABLE on BOTH backends since PR6b: PG via the
//!   writable-CTE windowed `UPDATE` (`backfill.rs`), SQLite via the batched
//!   per-batch-txn executor (`backend_sqlite::backfill_sql`, §2.3.1). The inline
//!   `set`/`filter` differ per dialect (the §9 `c.fn.splitPart` lowering,
//!   NULL-skipping `concatWs`); the `BackfillSpec` shape is uniform.
//!
//! # The shared SQLite-DML module seam
//!
//! The SQLite numbered `?n` placeholder emission lives in [`sqlite_placeholder`]
//! (called through [`placeholder`]), the SINGLE place the one-shot DML assembler
//! (here, PR6a) and the PR6b batched-backfill SQLite executor both emit positional
//! placeholders — so the two PRs never fork a divergent copy of the `?n`-binding
//! logic (§PR6a "ONE SQLite-DML-assembly module"). The transport-safe bind mirror
//! ([`crate::backend_sqlite::actor::SqliteBind`]) is likewise the single
//! value-binding path the SQLite executor uses.

use std::collections::BTreeMap;

use zeroship_schema::query::SqlDialect;

use crate::expr::{BinaryOp, Expr, ScalarFn, SynthFn, UnaryOp};
use crate::ir::IrScalar;
use crate::plan::BindValue;

/// A failure assembling a DML op into a statement (template + binds, or a backfill
/// spec). Distinct from the structural [`crate::validate::AuthoringError`]
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
    /// (`dialect_scope = PgOnly`), not a silently-dropped conflict clause (§9).
    #[error(
        "insert into {table:?} carries `onConflict`, which is PostgreSQL-only — SQLite \
         has no portable upsert and there is no raw route; restructure as separate \
         insert + update, or mark the migration dialect_scope=PgOnly (PG-only)"
    )]
    OnConflictNotPortable {
        /// The target table.
        table: String,
    },
}

/// Validate a bare SQL identifier and double-quote it (`"` → `""`). The ONLY
/// identifier-emission path the assembler uses — a schema-qualified / malformed
/// name is rejected, so an injection through an identifier slot cannot reach the
/// DB. Bare-identifier validation mirrors [`crate::backfill::BackfillSpec`].
fn quote_ident(what: &'static str, ident: &str) -> Result<String, DmlError> {
    let ok = !ident.is_empty()
        && ident.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(DmlError::InvalidIdentifier { what, value: ident.to_string() });
    }
    Ok(format!("\"{}\"", ident.replace('"', "\"\"")))
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
    if ident.is_empty() {
        return Err(IdentQuoteError { reason: "empty", value: ident.to_string() });
    }
    if ident.contains('\0') {
        return Err(IdentQuoteError { reason: "contains NUL", value: ident.to_string() });
    }
    Ok(format!("\"{}\"", ident.replace('"', "\"\"")))
}

/// Qualify a validated bare table name for the target dialect.
///
/// - **Postgres**: `"schema"."table"` — the project schema is engine-supplied
///   (never author-supplied) and the migrator's `search_path` is pinned to it, but
///   we qualify explicitly so the resolved relation is unambiguously the project's.
/// - **SQLite**: the table lives in the connection's `main` database (the app
///   file is `main`, §2.5.1) — there is NO schema namespace, and a
///   `"schema"."table"` reference would resolve to a non-existent attached DB. So
///   the SQLite form is the BARE quoted table, matching the engine's UNqualified
///   SQLite DDL emission (the same property the createTable lowering relies on).
fn qualify_table(
    project_schema: &str,
    dialect: SqlDialect,
    table: &str,
) -> Result<String, DmlError> {
    let t = quote_ident("table", table)?;
    match dialect {
        // L1 self-defense (PR10b): the project schema is engine-supplied (never
        // author-supplied) on every current path — under the Confined posture it
        // is the app id (a UUIDv7, e.g. `019efd94-…`, which carries `-`), so it
        // is NOT a bare `[A-Za-z_][A-Za-z0-9_]*` identifier and MUST NOT go
        // through `quote_ident` (that would reject every real deploy). It is
        // emitted exactly as the rest of the engine emits the schema —
        // escape-and-quote (`author::quote_ident` / `backfill` / `role`):
        // `"<schema with "" doubled>"`. The render seam still does not blindly
        // trust the caller: `quote_ident_checked` (the ONE shared engine seam)
        // fails closed on the bytes that double-quote escaping cannot neutralise
        // (empty / NUL, which PG rejects in an identifier outright). For a
        // quote-free schema the output is byte-identical to the prior `format!`,
        // so the dml goldens stay green.
        SqlDialect::Postgres => Ok(format!(
            "{}.{}",
            quote_ident_checked(project_schema).map_err(|e| DmlError::InvalidIdentifier {
                what: "schema",
                value: e.value,
            })?,
            t
        )),
        SqlDialect::Sqlite => Ok(t),
    }
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
        // already round-trips Bytes through canonical base64, §2.5.)
        IrScalar::Bytes(b) => {
            use base64::Engine as _;
            BindValue::Text(base64::engine::general_purpose::STANDARD.encode(b))
        }
    }
}

/// The dialect-specific positional placeholder for the `n`-th (1-based) bind.
/// Postgres uses `$n`; SQLite uses `?n` (the numbered form, so the binds stay
/// positional and reusable). This is the SINGLE placeholder-emission point the
/// one-shot assembler and the PR6b batched-backfill SQLite executor both call —
/// the shared SQLite-DML seam (§PR6a). Postgres routes through the same fn for one
/// consistent counter.
#[must_use]
pub fn placeholder(dialect: SqlDialect, n: usize) -> String {
    match dialect {
        SqlDialect::Postgres => format!("${n}"),
        SqlDialect::Sqlite => sqlite_placeholder(n),
    }
}

/// The SQLite numbered placeholder (`?n`) — factored out as the shared-module
/// entry the PR6b batched-backfill SQLite executor reuses for per-batch statement
/// assembly, so the two PRs bind values through ONE path (§PR6a).
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
fn inline_literal(s: &IrScalar) -> Result<String, DmlError> {
    Ok(match s {
        IrScalar::Null => "NULL".to_string(),
        IrScalar::Bool(b) => if *b { "TRUE".to_string() } else { "FALSE".to_string() },
        IrScalar::Int(i) => i.to_string(),
        IrScalar::Decimal(d) => d.clone(),
        IrScalar::Str(s) => format!("'{}'", s.replace('\'', "''")),
        IrScalar::Bytes(_) => {
            return Err(DmlError::UnrenderableExpr(
                "a bytes literal is not inline-renderable in a backfill transform \
                 (use a one-shot update with a native bind for byte values)"
                    .to_string(),
            ));
        }
    })
}

/// The SQL spelling of a binary operator (§3.3.1 method↔node table). `Concat` is
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

/// The SQL spelling of an allow-listed named scalar function (§3.3.1.1(a)). These
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
    }
}

/// The portable cast-target SQL type per dialect (§3.3.1). `blob` is `BYTEA` on
/// PG / `BLOB` on SQLite; the rest share spelling.
fn cast_target_sql(target: crate::expr::CastTarget, dialect: SqlDialect) -> &'static str {
    use crate::expr::CastTarget;
    match (target, dialect) {
        (CastTarget::Text, _) => "text",
        (CastTarget::Integer, _) => "integer",
        (CastTarget::Real, _) => "real",
        (CastTarget::Boolean, SqlDialect::Postgres) => "boolean",
        // SQLite has no boolean type; a `cast(x as boolean)` is not portable and
        // must be rejected upstream — but keep the spelling total here behind the
        // structural validator's gate. SQLite stores booleans as integers.
        (CastTarget::Boolean, SqlDialect::Sqlite) => "integer",
        (CastTarget::Blob, SqlDialect::Postgres) => "bytea",
        (CastTarget::Blob, SqlDialect::Sqlite) => "blob",
    }
}

/// Render a `c.fn.concatWs(delim, a, b, …)` per dialect (§9): PG `concat_ws`;
/// SQLite has no `concat_ws`, so it lowers to a NULL-skipping fold over `||` using
/// the pinned, portable shape. The args are already rendered fragments.
fn render_concat_ws(rendered: &[String], dialect: SqlDialect) -> String {
    // rendered[0] is the delimiter; rendered[1..] are the values.
    match dialect {
        SqlDialect::Postgres => format!("concat_ws({})", rendered.join(", ")),
        SqlDialect::Sqlite => {
            // NULL-skipping join: coalesce each value with '' joined by the
            // delimiter would re-introduce empty fields; the pinned SQLite shape
            // for concat_ws is a fold that skips NULLs. We use the standard
            // equivalent: trim away the delimiter that a leading NULL would leave.
            // For the bounded value count we emit the explicit
            // `substr(<acc>, len(delim)+1)` head-trim of a `||`-fold where each
            // value contributes `delim || value` only when not NULL.
            let delim = &rendered[0];
            let values = &rendered[1..];
            // acc = '' ; for each v: acc = acc || (case when v is null then '' else delim||v end)
            // then strip the single leading delim.
            let mut fold = String::from("''");
            for v in values {
                fold = format!(
                    "({fold} || (CASE WHEN ({v}) IS NULL THEN '' ELSE ({delim}) || ({v}) END))"
                );
            }
            // Strip the leading delimiter (length of the delim literal). Using
            // substr with instr-free fixed-prefix removal is only correct when the
            // delim is a fixed literal; concatWs's delim is a Literal by the op
            // shape, so this holds. We strip `length(delim)` leading chars.
            format!("substr({fold}, length({delim}) + 1)")
        }
    }
}

/// The MAX literal part index `c.fn.splitPart` admits — the O(2ⁿ) inline-unroll
/// bound (§9, `~17 KB` at `n=8`). MUST equal
/// [`crate::validate::SPLIT_PART_MAX_N`] — the validator gates the envelope and
/// this renderer assumes it; a fixture pins their equality.
const SPLIT_PART_MAX_N: i64 = crate::validate::SPLIT_PART_MAX_N;

/// Render the PINNED `c.fn.splitPart(col, d, n)` per dialect (§9), given the
/// already-rendered `col_sql` fragment and the raw delimiter + `n` IR args. The
/// renderer is **dialect-aware** because the portability ENVELOPE is dialect-gated
/// (mirroring `validate::check_split_part`, which returns early on a Postgres
/// target): PG's native `split_part` is multi-char-delimiter-capable and takes any
/// positive `n`, so on a Postgres target a `dialect_scope=PgOnly` out-of-envelope
/// splitPart (§2.4.1/§9) is a first-class, renderable node — NOT a hard error.
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
///   exactly the boundaries PG's character-wise `split_part` does (§9 byte-identity
///   proof). Append a sentinel delimiter (`cur₀ = col || 'd'`) so every token is
///   delimiter-terminated, then walk the boundary to literal depth `n`:
///   `curᵢ = substr(curᵢ₋₁, instr(curᵢ₋₁, 'd') + 1)` for `i = 1 … n−1`, and the
///   result is `substr(cur_{n-1}, 1, instr(cur_{n-1}, 'd') − 1)`. The unroll
///   references `curᵢ₋₁` twice per level, so it grows O(2ⁿ) (~17 KB at `n=8`) — the
///   reason `n` is capped at 8. The delimiter is emitted as a single-quoted SQL
///   literal (`''`-escaped), NOT a bind: it is an engine-pinned constant of the
///   pinned expression, and the authorizer (with `instr` allow-listed, §9) vets the
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

    match dialect {
        SqlDialect::Postgres => {
            // PG's split_part is multi-char-capable and takes any positive n — the
            // full delimiter string, '-escaped, rendered verbatim. This is the
            // dialect_scope=PgOnly escape (§2.4.1/§9): the envelope does NOT apply.
            let d = format!("'{}'", delim.replace('\'', "''"));
            Ok(format!("split_part({col_sql}, {d}, {n})"))
        }
        SqlDialect::Sqlite => {
            // ENVELOPE (SQLite only): single-ASCII-byte delim, 1 ≤ n ≤ MAX_N.
            let bytes = delim.as_bytes();
            if bytes.len() != 1 || bytes[0] >= 0x80 {
                return Err(DmlError::UnrenderableExpr(format!(
                    "c.fn.splitPart delimiter must be a single ASCII character (one byte, \
                     code point < 0x80) to lower portably on SQLite; got {delim:?}"
                )));
            }
            if n > SPLIT_PART_MAX_N {
                return Err(DmlError::UnrenderableExpr(format!(
                    "c.fn.splitPart part index n must be in 1..={SPLIT_PART_MAX_N} \
                     (the proven inline-unroll bound) to lower portably on SQLite; got {n}"
                )));
            }
            let dc = char::from(bytes[0]);
            // Single-ASCII delimiter as an inline SQL string literal (`'` → `''`).
            let d = if dc == '\'' { "''''".to_string() } else { format!("'{dc}'") };
            // cur₀ = (col || 'd') — the sentinel-terminated string.
            let mut cur = format!("({col_sql} || {d})");
            // curᵢ = substr(curᵢ₋₁, instr(curᵢ₋₁, 'd') + 1), i = 1 … n−1.
            for _ in 1..n {
                cur = format!("substr({cur}, instr({cur}, {d}) + 1)");
            }
            // result = substr(cur_{n-1}, 1, instr(cur_{n-1}, 'd') − 1).
            Ok(format!("substr({cur}, 1, instr({cur}, {d}) - 1)"))
        }
    }
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
        Expr::ColRef { name } => quote_ident("column", name)?,
        Expr::Literal { value } => ctx.push_bind(scalar_to_bind(value)),
        Expr::BinOp { op, lhs, rhs } => {
            let l = render_expr_bound(lhs, ctx)?;
            let r = render_expr_bound(rhs, ctx)?;
            format!("({} {} {})", l, binary_op_sql(*op), r)
        }
        Expr::UnaryOp { op, operand } => {
            let o = render_expr_bound(operand, ctx)?;
            render_unary(*op, &o)
        }
        Expr::Case { branches, r#else } => {
            let mut s = String::from("CASE");
            for b in branches {
                let c = render_expr_bound(&b.condition, ctx)?;
                let r = render_expr_bound(&b.result, ctx)?;
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
            format!("{}({})", scalar_fn_sql(*r#fn), rs.join(", "))
        }
        Expr::FnSynth { r#fn, args } => render_synth_bound(*r#fn, args, ctx)?,
        Expr::Cast { operand, target } => {
            let o = render_expr_bound(operand, ctx)?;
            format!("CAST({o} AS {})", cast_target_sql(*target, ctx.dialect))
        }
    })
}

/// Render a `FnSynth` in the parameterized path. `concatWs` lowers per dialect;
/// `splitPart` lowering is the PR6b portable-helper wave (§9) — until then a
/// `splitPart` in a one-shot DML position is a fail-closed unrenderable. `now` /
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
        SynthFn::Now => Ok(match ctx.dialect {
            SqlDialect::Postgres => "now()".to_string(),
            SqlDialect::Sqlite => "CURRENT_TIMESTAMP".to_string(),
        }),
        SynthFn::GenRandomUuid => Ok(match ctx.dialect {
            SqlDialect::Postgres => "gen_random_uuid()".to_string(),
            // SQLite has no native UUID; the portable shape is a hex-randomblob
            // composition. Pinned, deterministic SHAPE (not value).
            SqlDialect::Sqlite => "lower(hex(randomblob(16)))".to_string(),
        }),
        SynthFn::SplitPart => {
            // splitPart(col, delim, n): the column arg may itself be a ColRef or an
            // in-AST sub-expression — render it (binding any nested Literals), then
            // extract the pinned single-ASCII delim + literal n envelope (§9). The
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

/// Render a unary op around an already-rendered operand.
fn render_unary(op: UnaryOp, operand: &str) -> String {
    match op {
        UnaryOp::Not => format!("(NOT {operand})"),
        UnaryOp::IsNull => format!("({operand} IS NULL)"),
        UnaryOp::IsNotNull => format!("({operand} IS NOT NULL)"),
        UnaryOp::IsTrue => format!("({operand} IS TRUE)"),
        UnaryOp::IsFalse => format!("({operand} IS FALSE)"),
    }
}

/// Render a closed-AST [`Expr`] to an INLINE SQL string (the backfill path). A
/// `ColRef` is its quoted identifier; a `Literal` is an inline SQL literal
/// ([`inline_literal`], guard-revalidated downstream); operators / functions /
/// casts render the same SQL spelling as the bound path. NO binds — the backfill
/// executor pages the statement and cannot carry positional binds.
fn render_expr_inline(expr: &Expr, dialect: SqlDialect) -> Result<String, DmlError> {
    Ok(match expr {
        Expr::ColRef { name } => quote_ident("column", name)?,
        Expr::Literal { value } => inline_literal(value)?,
        Expr::BinOp { op, lhs, rhs } => {
            let l = render_expr_inline(lhs, dialect)?;
            let r = render_expr_inline(rhs, dialect)?;
            format!("({} {} {})", l, binary_op_sql(*op), r)
        }
        Expr::UnaryOp { op, operand } => {
            render_unary(*op, &render_expr_inline(operand, dialect)?)
        }
        Expr::Case { branches, r#else } => {
            let mut s = String::from("CASE");
            for b in branches {
                let c = render_expr_inline(&b.condition, dialect)?;
                let r = render_expr_inline(&b.result, dialect)?;
                s.push_str(&format!(" WHEN {c} THEN {r}"));
            }
            if let Some(e) = r#else {
                s.push_str(&format!(" ELSE {}", render_expr_inline(e, dialect)?));
            }
            s.push_str(" END");
            s
        }
        Expr::FnCall { r#fn, args } => {
            let rs: Result<Vec<_>, _> =
                args.iter().map(|a| render_expr_inline(a, dialect)).collect();
            format!("{}({})", scalar_fn_sql(*r#fn), rs?.join(", "))
        }
        Expr::FnSynth { r#fn, args } => match r#fn {
            SynthFn::SplitPart => {
                // The column arg renders inline; the delim/n are engine-pinned
                // constants of the §9 lowering, extracted raw (NOT inline-rendered
                // as generic literals). The backfill (inline) path is exactly where
                // the §3.1 hero split lands.
                if args.len() != 3 {
                    return Err(DmlError::UnrenderableExpr(format!(
                        "c.fn.splitPart takes exactly (column, delim, n); got {} args",
                        args.len()
                    )));
                }
                let col_sql = render_expr_inline(&args[0], dialect)?;
                render_split_part(&col_sql, &args[1], &args[2], dialect)?
            }
            SynthFn::ConcatWs => {
                let rs: Result<Vec<_>, _> =
                    args.iter().map(|a| render_expr_inline(a, dialect)).collect();
                render_concat_ws(&rs?, dialect)
            }
            SynthFn::Now => match dialect {
                SqlDialect::Postgres => "now()".to_string(),
                SqlDialect::Sqlite => "CURRENT_TIMESTAMP".to_string(),
            },
            SynthFn::GenRandomUuid => match dialect {
                SqlDialect::Postgres => "gen_random_uuid()".to_string(),
                SqlDialect::Sqlite => "lower(hex(randomblob(16)))".to_string(),
            },
        },
        Expr::Cast { operand, target } => {
            format!(
                "CAST({} AS {})",
                render_expr_inline(operand, dialect)?,
                cast_target_sql(*target, dialect)
            )
        }
    })
}

/// The `onConflict` facet of an `insert` (§3.4 / §9). PG-only.
#[derive(Debug, Clone, PartialEq)]
pub struct OnConflict {
    /// The conflict-target columns (`ON CONFLICT (cols)`).
    pub columns: Vec<String>,
    /// `Some` SET assignments ⇒ `DO UPDATE SET …`; `None` ⇒ `DO NOTHING`.
    pub do_update: Option<BTreeMap<String, IrScalar>>,
}

/// The assembled one-shot DML statement: the placeholder template + ordered binds.
/// Fed straight into [`PlanStep::Dml`](crate::plan::PlanStep::Dml).
#[derive(Debug, Clone, PartialEq)]
pub struct AssembledDml {
    /// The placeholder SQL (`$n`/`?n` — never an inlined value).
    pub template: String,
    /// The ordered native binds.
    pub binds: Vec<BindValue>,
}

/// Assemble an `insert` op into a parameterized one-shot statement (§2.3.2). Every
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
    rows: &[Vec<IrScalar>],
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
        columns.iter().map(|c| quote_ident("column", c)).collect();
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
        let placeholders: Vec<String> =
            row.iter().map(|s| ctx.push_bind(scalar_to_bind(s))).collect();
        value_groups.push(format!("({})", placeholders.join(", ")));
    }

    let mut template = format!(
        "INSERT INTO {qtable} ({}) VALUES {}",
        qcols.join(", "),
        value_groups.join(", ")
    );

    if let Some(oc) = on_conflict {
        if dialect == SqlDialect::Sqlite {
            return Err(DmlError::OnConflictNotPortable { table: table.to_string() });
        }
        template.push_str(&render_on_conflict(oc, &mut ctx)?);
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
        oc.columns.iter().map(|c| quote_ident("column", c)).collect();
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
                let qc = quote_ident("column", col)?;
                let ph = ctx.push_bind(scalar_to_bind(val));
                assigns.push(format!("{qc} = {ph}"));
            }
            Ok(format!(" {target} DO UPDATE SET {}", assigns.join(", ")))
        }
    }
}

/// Assemble a one-shot `update` op (no `batch`) into a parameterized statement.
/// `set` RHS + the optional `where` render through [`render_expr_bound`], so every
/// literal is a native bind. Portable on both backends.
///
/// # Errors
/// [`DmlError`] on a malformed identifier / empty `set` / an unrenderable node.
pub fn assemble_update(
    project_schema: &str,
    dialect: SqlDialect,
    table: &str,
    set: &BTreeMap<String, Expr>,
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
        let qc = quote_ident("column", col)?;
        let r = render_expr_bound(rhs, &mut ctx)?;
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
    };
    Ok(AssembledDml { template, binds: ctx.binds })
}

/// The rendered backfill clauses (the SQL strings the [`BackfillSpec`] carries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillClauses {
    /// The `SET` body (e.g. `"normalized" = lower("raw")`).
    pub set_clause: String,
    /// The optional extra `WHERE` conjunct.
    pub filter: Option<String>,
}

/// Assemble a `backfill` op's `set` / `filter` into the inline SQL strings the
/// [`BackfillSpec`](crate::backfill::BackfillSpec) executor consumes. Renders for
/// EITHER dialect (PR6b): the inline transform is dialect-rendered (the §9
/// `c.fn.splitPart` lowering, NULL-skipping `concatWs`), and the PG (`backfill.rs`)
/// or SQLite (`backend_sqlite::backfill_sql`) executor consumes the result.
///
/// # Errors
/// [`DmlError`] on a malformed identifier / empty `set` / an unrenderable node.
pub fn assemble_backfill_clauses(
    dialect: SqlDialect,
    table: &str,
    set: &BTreeMap<String, Expr>,
    filter: Option<&Expr>,
) -> Result<BackfillClauses, DmlError> {
    if set.is_empty() {
        return Err(DmlError::EmptySet { op: "backfill", table: table.to_string() });
    }
    // BTreeMap ⇒ canonical order.
    let mut assigns = Vec::with_capacity(set.len());
    for (col, rhs) in set {
        let qc = quote_ident("column", col)?;
        let r = render_expr_inline(rhs, dialect)?;
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
    use crate::expr::Expr;

    const SCHEMA: &str = "app_proj";

    // ---- #150: the ONE shared engine identifier seam --------------------------

    /// **#150** — `quote_ident_checked` fails CLOSED on the two bytes `"`-doubling
    /// cannot neutralise: an empty string and a NUL byte. RED before #150: the peer
    /// seams (`author`/`backfill`/`role`/`journal`) had NO such guard — they would
    /// have ACCEPTED a NUL and emitted `"a\0b"`.
    #[test]
    fn quote_ident_checked_fails_closed_on_empty_and_nul() {
        assert!(quote_ident_checked("").is_err(), "empty must fail closed");
        assert!(quote_ident_checked("a\0b").is_err(), "NUL must fail closed");
        assert_eq!(quote_ident_checked("").unwrap_err().reason, "empty");
        assert_eq!(quote_ident_checked("a\0b").unwrap_err().reason, "contains NUL");
    }

    /// **#150** — for any non-empty / non-NUL identifier the output is
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

    /// **#150** — the four engine peer seams (`author`/`backfill`/`role`/`journal`)
    /// now all route through `quote_ident_checked`, so they emit BYTE-IDENTICAL
    /// output for the same quote-bearing schema (the "uniform render seam"
    /// requirement). The peers wrap the shared helper, so comparing each to the
    /// canonical helper proves the uniformity for all five.
    #[test]
    fn all_engine_seams_render_uniformly() {
        let schema = "ap\"p"; // a quote-bearing engine schema
        let canonical = quote_ident_checked(schema).unwrap();
        // author (infallible-on-valid wrapper) — maps to its own error on failure.
        assert_eq!(crate::author::quote_ident_for_test(schema).unwrap(), canonical);
        assert_eq!(crate::backfill::quote_ident_for_test(schema).unwrap(), canonical);
        assert_eq!(crate::role::quote_ident_for_test(schema).unwrap(), canonical);
        assert_eq!(crate::journal::quote_ident_for_test(schema).unwrap(), canonical);
        // …and they fail closed uniformly on a NUL too.
        assert!(crate::author::quote_ident_for_test("a\0b").is_err());
        assert!(crate::backfill::quote_ident_for_test("a\0b").is_err());
        assert!(crate::role::quote_ident_for_test("a\0b").is_err());
        assert!(crate::journal::quote_ident_for_test("a\0b").is_err());
    }

    fn lit_str(s: &str) -> Expr {
        Expr::lit(IrScalar::Str(s.to_string()))
    }
    fn lit_int(i: i64) -> Expr {
        Expr::lit(IrScalar::Int(i))
    }

    // ── identifier safety ───────────────────────────────────────────────────

    #[test]
    fn rejects_schema_qualified_table() {
        let err = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "other_schema.victims",
            &["a".into()],
            &[vec![IrScalar::Int(1)]],
            None,
        )
        .unwrap_err();
        assert!(matches!(err, DmlError::InvalidIdentifier { what: "table", .. }), "{err:?}");
    }

    /// L1 self-defense (PR10b): the PG `qualify_table` arm must not blindly trust
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
            &[vec![IrScalar::Int(1)]],
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
            &[vec![IrScalar::Int(1)]],
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
            &[vec![IrScalar::Int(1)]],
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
            &[vec![IrScalar::Int(1)]],
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
            &[vec![IrScalar::Int(1)]],
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
            &[vec![IrScalar::Int(200), IrScalar::Str("ok".into())]],
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
    fn insert_uses_question_placeholders_on_sqlite() {
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Sqlite,
            "t",
            &["a".into(), "b".into()],
            &[vec![IrScalar::Int(1), IrScalar::Null]],
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
            &[vec![IrScalar::Int(1)], vec![IrScalar::Int(2)]],
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
            &[vec![IrScalar::Str(hostile.into())]],
            None,
        )
        .unwrap();
        // The template carries ONLY the placeholder; the hostile bytes are a bind.
        assert_eq!(a.template, "INSERT INTO \"app_proj\".\"t\" (\"a\") VALUES ($1)");
        assert!(!a.template.contains("DROP"), "metacharacters must not reach the template");
        assert_eq!(a.binds, vec![BindValue::Text(hostile.into())]);
    }

    #[test]
    fn insert_ragged_row_rejected() {
        let err = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "t",
            &["a".into(), "b".into()],
            &[vec![IrScalar::Int(1)]],
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
            do_update: Some(BTreeMap::from([("label".to_string(), IrScalar::Str("dup".into()))])),
        };
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "status_codes",
            &["code".into(), "label".into()],
            &[vec![IrScalar::Int(1), IrScalar::Str("ok".into())]],
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
    fn insert_on_conflict_do_nothing_pg() {
        let oc = OnConflict { columns: vec!["code".into()], do_update: None };
        let a = assemble_insert(
            SCHEMA,
            SqlDialect::Postgres,
            "t",
            &["code".into()],
            &[vec![IrScalar::Int(1)]],
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
            &[vec![IrScalar::Int(1)]],
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
            Expr::FnCall {
                r#fn: ScalarFn::Coalesce,
                args: vec![Expr::col("label"), lit_str("unknown")],
            },
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
        let set = BTreeMap::from([("a".to_string(), lit_int(5))]);
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
            Expr::BinOp {
                op: BinaryOp::Concat,
                lhs: Box::new(Expr::col("code")),
                rhs: Box::new(lit_str("!")),
            },
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
        let set = BTreeMap::from([("a".to_string(), lit_str("O'Brien"))]);
        let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = 'O''Brien'");
    }

    #[test]
    fn backfill_empty_set_rejected() {
        let err = assemble_backfill_clauses(SqlDialect::Postgres, "t", &BTreeMap::new(), None)
            .unwrap_err();
        assert!(matches!(err, DmlError::EmptySet { op: "backfill", .. }), "{err:?}");
    }

    // ── splitPart lowering (§9 pinned helper) ────────────────────────────────

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
        let set = BTreeMap::from([("first".to_string(), split("name", " ", 1))]);
        let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"first\" = split_part(\"name\", ' ', 1)");
    }

    /// SQLite lowers splitPart to the pinned instr/substr unroll. n=1 is the base
    /// case (no inner walk). The exact string is pinned to the §9 exhibit.
    #[test]
    fn split_part_sqlite_n1_unroll() {
        let set = BTreeMap::from([("first".to_string(), split("name", " ", 1))]);
        let c = assemble_backfill_clauses(SqlDialect::Sqlite, "t", &set, None).unwrap();
        assert_eq!(
            c.set_clause,
            "\"first\" = substr((\"name\" || ' '), 1, instr((\"name\" || ' '), ' ') - 1)"
        );
    }

    /// SQLite n=2 unrolls one boundary walk — pinned to the §9 exhibit.
    #[test]
    fn split_part_sqlite_n2_unroll() {
        let set = BTreeMap::from([("last".to_string(), split("name", " ", 2))]);
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
        let set = BTreeMap::from([("first".to_string(), split("name", ",", 1))]);
        let a = assemble_update(SCHEMA, SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(a.template, "UPDATE \"app_proj\".\"t\" SET \"first\" = split_part(\"name\", ',', 1)");
        assert!(a.binds.is_empty(), "delim/n are pinned constants, not binds");
    }

    /// A single-quote delimiter is `''''`-escaped in the inline literal on both legs.
    #[test]
    fn split_part_quote_delim_escaped_sqlite() {
        let set = BTreeMap::from([("a".to_string(), split("name", "'", 1))]);
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
        let set = BTreeMap::from([("a".to_string(), split("name", ", ", 1))]);
        let err = assemble_backfill_clauses(SqlDialect::Sqlite, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
        let set = BTreeMap::from([("a".to_string(), split("name", ",", 9))]);
        let err = assemble_backfill_clauses(SqlDialect::Sqlite, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
    }

    /// HIGH (PR6b code-critic): the documented `dialect_scope=PgOnly` escape for an
    /// out-of-envelope `c.fn.splitPart` (§2.4.1/§9). The validator ADMITS a
    /// multi-char delimiter and `n > 8` on a Postgres target; the renderer MUST
    /// therefore lower it to native `split_part(col, 'delim', n)` on PG, not
    /// hard-error. This is the missing companion to the load-only grammar test
    /// `out_of_envelope_split_part_pg_loads_sqlite_rejected`.
    #[test]
    fn split_part_out_of_envelope_renders_native_on_pg() {
        // multi-char delimiter — PG's split_part is multi-char-capable.
        let set = BTreeMap::from([("a".to_string(), split("name", ", ", 1))]);
        let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = split_part(\"name\", ', ', 1)");

        // n beyond the SQLite unroll bound (9) — PG takes any positive n.
        let set = BTreeMap::from([("a".to_string(), split("name", ",", 9))]);
        let c = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(c.set_clause, "\"a\" = split_part(\"name\", ',', 9)");

        // and the one-shot (bound) PG path too — delim/n stay pinned constants.
        let set = BTreeMap::from([("a".to_string(), split("name", ", ", 1))]);
        let a = assemble_update(SCHEMA, SqlDialect::Postgres, "t", &set, None).unwrap();
        assert_eq!(a.template, "UPDATE \"app_proj\".\"t\" SET \"a\" = split_part(\"name\", ', ', 1)");
        assert!(a.binds.is_empty(), "delim/n are pinned constants, not binds");
    }

    /// A non-ASCII (multibyte) delimiter is still PG-renderable (PG splits on the
    /// literal string); the single-ASCII byte gate is a SQLite-only envelope.
    #[test]
    fn split_part_non_ascii_delim_renders_on_pg() {
        let set = BTreeMap::from([("a".to_string(), split("name", "→", 2))]);
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
        let set = BTreeMap::from([("a".to_string(), split("name", ",", 0))]);
        let err = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
        // non-literal delim (a ColRef) — never renderable.
        let bad = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::ColRef { name: "name".into() },
                Expr::ColRef { name: "name".into() },
                lit_int(1),
            ],
        };
        let set = BTreeMap::from([("a".to_string(), bad)]);
        let err = assemble_backfill_clauses(SqlDialect::Postgres, "t", &set, None).unwrap_err();
        assert!(matches!(err, DmlError::UnrenderableExpr(_)), "{err:?}");
    }

    /// The renderer's envelope bound MUST equal the validator's, so a node the
    /// validator admits the renderer can always lower.
    #[test]
    fn split_part_max_n_matches_validator() {
        assert_eq!(SPLIT_PART_MAX_N, crate::validate::SPLIT_PART_MAX_N);
    }
}
