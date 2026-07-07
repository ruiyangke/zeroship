//! The STRUCTURAL expression-AST validator + the structured-error envelope
//! (design §3.3.1.1 / §8.8).
//!
//! The closed expression AST ([`crate::model::expr::Expr`]) is **constructed in JS and
//! serialized to IR — never parsed from text** (§3.3.1.1). So validation is a
//! purely STRUCTURAL allow-list walk over the deserialized tree:
//!
//! - **(a)** every node is in the allow-listed set — the serde deserializer
//!   already rejects an unknown node *tag* (`UNSUPPORTED { kind: "expr" }` at
//!   load); this walk additionally rejects the structural shapes that *are*
//!   well-typed nodes but out of policy (an out-of-envelope `FnSynth(splitPart)`,
//!   a non-portable cast target).
//! - **(b)** `c.fn.splitPart` args are in-envelope — `delim` is a single ASCII
//!   character `Literal` (one byte, code point `< 0x80`), `n` is a positive
//!   integer `Literal` with `1 ≤ n ≤ 8`, and the column arg is a `ColRef` /
//!   in-AST sub-expression.
//! - **(c)** every `ColRef` resolves to a column on the ENCLOSING target table —
//!   an apply/render-time check scoped to the single target table of the
//!   enclosing op (§3.3.1.1(c)). A cross-table reference is impossible by
//!   construction (`c` is single-table-scoped — §3.3.1), and any reference to a
//!   column not on the target table is a hard error (injection defense + the
//!   capability boundary).
//! - **(d)** a `Cast` target is a portable type — guaranteed by the closed
//!   [`crate::model::expr::CastTarget`] enum, so this is structurally total.
//!
//! There is **NO lexer, NO Pratt/precedence parser, NO `libpg_query`, NO
//! differential fuzzer** — HIGH-1 is dissolved, not mitigated (§3.3.1.1). The
//! Rust validator here is the authoritative STRUCTURAL gate (checks (a), (b),
//! (d) — node allow-list, `FnSynth` arity/envelope, portable cast target); the
//! JS side runs an optional best-effort structural hint over the SAME schemars
//! schema. Rule (c) — `ColRef` resolution against the live target table — runs
//! at the apply/render seam (§3.3.1.1(c) is an apply-time check): at IR load the
//! live column set is generally unknown for the DML ops, `setColumnType`,
//! `addConstraint` and `createIndex`, so those positions validate
//! [`TargetScope::structural_only`] here and the seam re-runs the walk with a
//! resolved column set. A self-contained `createTable` DOES resolve (c) against
//! its own declared columns at load.
//!
//! LAYERING EXCEPTION (A3): raw view-body validation calls the guard's read-only
//! body scanner after the structural `SELECT` checks. That scanner is real
//! deny-list security logic, so moving it down into `model` would put guard policy
//! in the data layer. Until a separate analysis pass above `model` + `guard`
//! walks raw view bodies, this is the one deliberate `model -> guard` edge.

use crate::model::expr::{AggFunc, CaseBranch, Duration, Expr, ScalarFn, SynthFn};
use crate::model::profile::{AuthorPrimaryKeyPolicy, PolicyProfile};
use pg_query::protobuf::node::Node as NodeEnum;

// ── Canonical authoring-time error codes (§8.8) ─────────────────────────────
// The taxonomy new validators add their code to. The op-vs-expr distinction is
// carried as the `kind` field on `UNSUPPORTED`, not two top-level codes.

/// An op or expression node the engine cannot render on EITHER dialect — carries
/// `kind: "op" | "expr"` (§8.8). For PR1 the validator emits the `"expr"` kind.
pub const CODE_UNSUPPORTED: &str = "UNSUPPORTED";
/// An expression that is *expressible* but out of its portable envelope (e.g. an
/// out-of-envelope `c.fn.splitPart`) — kept distinct from `UNSUPPORTED` because
/// the remedy differs ("stay in-envelope or accept `PgOnly`"), §3.3.1.1/§9.
pub const CODE_EXPR_NOT_PORTABLE: &str = "EXPR_NOT_PORTABLE";
/// A `dialect_scope = PgOnly` artifact deployed against a SQLite target (§2.4.1).
pub const CODE_DIALECT_SCOPE_PGONLY: &str = "DIALECT_SCOPE_PGONLY";
/// An op-function called outside an active recorder (§3.1) — emitted JS-side.
pub const CODE_OP_OUTSIDE_RECORDER: &str = "OP_OUTSIDE_RECORDER";
/// An op is structurally valid JSON but carries an internally inconsistent shape.
pub const CODE_OP_INVALID: &str = "OP_INVALID";
/// **PR10** — an op naming a `schema` the active [`SchemaScope`](crate::model::policy::SchemaScope)
/// does not permit (§2.7). The Confined creator profile pins the project schema:
/// an explicit `schema != project_schema` is REFUSED at validate-time, fail-closed,
/// BEFORE lower — additional and EARLIER than the migrator-role 42501 + the
/// parse-guard cross-schema denial (which stay unchanged). The Platform profile
/// permits only its allow-list. The friendly remedy is "drop the qualifier or name
/// the project schema".
pub const CODE_CROSS_SCHEMA: &str = "CROSS_SCHEMA";
/// **PR10** — a `schema` qualifier that is not a safe bare SQL identifier (§2.7):
/// empty, not alpha/`_`-leading, or carrying a non-`[A-Za-z0-9_]` char. The schema
/// is an author-controlled identifier the engine double-quotes; this rejects an
/// injection-shaped value (`"; DROP …`, embedded quote) at validate-time, before
/// it can reach the render seam.
pub const CODE_INVALID_SCHEMA_IDENT: &str = "INVALID_SCHEMA_IDENT";
/// **PR10** — an existence guard whose DIRECTION is illegal for the op variant
/// (§2.7): an `ifExists` on a create*/add* op, or an `ifNotExists` on a
/// drop*/rename/alter op. A structured authoring error, not a render-time blow-up.
pub const CODE_GUARD_DIRECTION: &str = "GUARD_DIRECTION";
/// **Migration-first P2a (§4)** — a `t.id({prefix})` `id_prefix` that is not a
/// valid typed-id prefix (charset / length) or is in the reserved-prefix
/// deny-list (`usr`, …). The IR's threat model is a hand-crafted `.ir.json`, so a
/// malformed/reserved prefix is a fail-closed VALIDATE error, not a render-time
/// surprise (it would otherwise mint ids colliding with platform `usr_…` ids).
pub const CODE_INVALID_ID_PREFIX: &str = "INVALID_ID_PREFIX";
/// **Migration-first P2a (§4)** — a `vector_metric` carried on a column that is
/// NOT a `ColType::Vector`. The metric is structurally bounded by the closed
/// [`crate::model::ir::VectorMetric`] enum at deserialize; this is the co-occurrence
/// rule (the metric is meaningless without a vector type, and would otherwise be
/// a silent dead field a hand-crafted artifact could ride in on).
pub const CODE_VECTOR_METRIC_MISPLACED: &str = "VECTOR_METRIC_MISPLACED";
/// A column carries mutually-exclusive facets (`default` + `generated`,
/// `identity` + `generated`, etc.).
pub const CODE_COLUMN_FACET_CONFLICT: &str = "COLUMN_FACET_CONFLICT";
/// A column/domain default is structurally valid but invalid for the declared
/// column type (for example `{}` on `text[]`).
pub const CODE_COLUMN_DEFAULT_TYPE: &str = "COLUMN_DEFAULT_TYPE";
/// A volatile expression node appeared in a SQL context that requires immutable
/// expressions (index expression/predicate, generated column, or CHECK).
pub const CODE_IMMUTABLE_CONTEXT_VOLATILE: &str = "IMMUTABLE_CONTEXT_VOLATILE";
/// An aggregate expression appeared in a scalar context (index expression/
/// predicate, generated column, CHECK, or column DEFAULT).
pub const CODE_AGGREGATE_IN_SCALAR_CONTEXT: &str = "AGGREGATE_IN_SCALAR_CONTEXT";
/// A sequence carries a semantically invalid option (`increment = 0`,
/// `cache < 1`, or `minValue > maxValue`).
pub const CODE_SEQUENCE_OPTION_INVALID: &str = "SEQUENCE_OPTION_INVALID";
/// **VENDOR (`@zeroship/migrate`)** — a privileged vendor op (role/grant/RLS/
/// policy/trigger/function/extension/schema/`pgRaw`) whose required
/// [`VendorCapability`](crate::model::capability::VendorCapability) is NOT granted by the
/// active capability set (vendor spec §3.2). The Confined creator/AI posture
/// grants NO vendor capability, so EVERY vendor op is refused fail-closed at
/// validate, BEFORE lower — the #1 invariant (gate 1). The redundant lower gate
/// (gate 2 — the rendered SQL hits the Confined deny-list) means a future refactor
/// that drops this gate still fails closed.
pub const CODE_VENDOR_OP_DENIED: &str = "VENDOR_OP_DENIED";
/// A `pgRaw` op must carry a non-empty audit reason for using the raw SQL escape.
pub const CODE_PGRAW_REASON_REQUIRED: &str = "PGRAW_REASON_REQUIRED";
/// A resolved `createTable.primaryKey` is structurally invalid: empty, duplicated,
/// or naming a column absent from the resolved table.
pub const CODE_PRIMARY_KEY_INVALID: &str = "PRIMARY_KEY_INVALID";
/// A resolved `createTable` violates the active profile's table-shape policy.
pub const CODE_TABLE_SHAPE_POLICY: &str = "TABLE_SHAPE_POLICY";
/// A dialect target cannot realize an authored construct and no P12 affirmation
/// authorizes a transparent-degradable leg.
pub const CODE_DIALECT_UNSUPPORTED: &str = "DIALECT_UNSUPPORTED";
/// Partition rule 1: unique-enforcing entries on a partitioned table must cover
/// all partition key columns.
pub const CODE_PARTITION_KEY_COVERAGE: &str = "PARTITION_KEY_COVERAGE";
/// Partition rule 2: collapse-affirmed bound sets must be total.
pub const CODE_PARTITION_BOUNDS_NOT_TOTAL: &str = "PARTITION_BOUNDS_NOT_TOTAL";
/// Partition rule 2: v1 range collapse only supports a single partition key.
pub const CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED: &str =
    "PARTITION_COMPOSITE_KEY_UNSUPPORTED";
/// Partition rule 2: collapse predicates require two-valued, non-null keys.
pub const CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE: &str =
    "PARTITION_KEY_NULLABLE_UNDER_COLLAPSE";
/// Partition rule 3: sibling bounds must be PG-well-formed.
pub const CODE_PARTITION_BOUNDS_ILL_FORMED: &str = "PARTITION_BOUNDS_ILL_FORMED";
/// Partition tier split: hash child drops have no portable collapse predicate.
pub const CODE_PARTITION_HASH_DROP_UNDERIVABLE: &str = "PARTITION_HASH_DROP_UNDERIVABLE";

/// The MAX byte length a `t.id({prefix})` prefix may carry (P2a §4). Mirrors the
/// typed_id convention (`crates/core/src/typed_id.rs`: `usr`/`app`/`ses` are 3
/// chars; the auto-derivation in `plugin-db`'s `system_fields_pass` caps at 4 for
/// collection-derived prefixes). A hand-authored prefix is bounded to the SAME 4
/// so the minted `<prefix>_<22 base62>` typed-id keeps the compact platform shape.
pub const MAX_ID_PREFIX_LEN: usize = 4;

/// The dialect a structured rejection pertains to (§8.8 `dialect` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Postgres.
    Postgres,
    /// SQLite.
    Sqlite,
    /// MySQL.
    Mysql,
}

impl Dialect {
    /// The lower-case wire spelling used in the structured payload.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::Postgres => "postgres",
            Dialect::Sqlite => "sqlite",
            Dialect::Mysql => "mysql",
        }
    }
}

/// The `UNSUPPORTED { kind }` discriminant (§8.8) — an internal op-vs-expr
/// distinction carried as a field, not two top-level codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedKind {
    /// An unsupported OP.
    Op,
    /// An unsupported EXPRESSION node.
    Expr,
    /// A generated VIRTUAL column on a dialect that only supports STORED.
    VirtualColumn,
    /// An identity column placement/type that has no sound target-dialect render.
    Identity,
}

impl UnsupportedKind {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            UnsupportedKind::Op => "op",
            UnsupportedKind::Expr => "expr",
            UnsupportedKind::VirtualColumn => "virtualColumn",
            UnsupportedKind::Identity => "identity",
        }
    }
}

/// The machine-readable authoring-time rejection envelope (§8.8).
///
/// The human-facing rendering leads with [`suggested_fix`](AuthoringError::suggested_fix)
/// (the field that unblocks the author/AI loop); `code` is secondary metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoringError {
    /// Stable machine-actionable code (one of the `CODE_*` consts).
    pub code: String,
    /// For [`CODE_UNSUPPORTED`], the op-vs-expr kind; `None` for codes where the
    /// distinction does not apply.
    pub kind: Option<UnsupportedKind>,
    /// The 0-based index of the offending op in the migration's op list.
    pub op_index: usize,
    /// The `.ts` source-map location (e.g. `migrations/0007_split.ts:9`), if the
    /// recorder attributed one. PR1 carries it through from the validator caller.
    pub ts_location: Option<String>,
    /// The dialect the rejection pertains to.
    pub dialect: Dialect,
    /// A precise human-readable reason.
    pub reason: String,
    /// A concrete remedy the AI loop can act on (leads the human rendering).
    pub suggested_fix: Option<String>,
}

impl AuthoringError {
    /// Serialize to the canonical structured-error JSON object (§8.8), with
    /// `suggested_fix` FIRST so a human rendering leads with it.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if let Some(fix) = &self.suggested_fix {
            map.insert("suggested_fix".into(), serde_json::Value::String(fix.clone()));
        }
        map.insert("code".into(), serde_json::Value::String(self.code.clone()));
        if let Some(kind) = self.kind {
            map.insert("kind".into(), serde_json::Value::String(kind.as_str().into()));
        }
        map.insert("op_index".into(), serde_json::Value::from(self.op_index));
        if let Some(loc) = &self.ts_location {
            map.insert("ts_location".into(), serde_json::Value::String(loc.clone()));
        }
        map.insert("dialect".into(), serde_json::Value::String(self.dialect.as_str().into()));
        map.insert("reason".into(), serde_json::Value::String(self.reason.clone()));
        serde_json::Value::Object(map)
    }
}

impl std::fmt::Display for AuthoringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Lead with suggested_fix (the unblocking field), then code metadata.
        if let Some(fix) = &self.suggested_fix {
            write!(f, "{fix} [")?;
        } else {
            write!(f, "[")?;
        }
        write!(f, "{}", self.code)?;
        if let Some(kind) = self.kind {
            write!(f, " kind={}", kind.as_str())?;
        }
        write!(
            f,
            " op_index={} dialect={}]: {}",
            self.op_index,
            self.dialect.as_str(),
            self.reason
        )
    }
}

impl std::error::Error for AuthoringError {}

/// The MAX literal part index `c.fn.splitPart` admits (the O(2ⁿ) inline-unroll
/// bound, §9 — `~17 KB` at `n=8`).
pub const SPLIT_PART_MAX_N: i64 = 8;

/// The single-target-table scope a transform validates against (§3.3.1.1(c)).
///
/// Carries the target table name + its valid column set. PR1's caller (the
/// validator / `IrAuthor` render seam) supplies the columns it resolved for the
/// enclosing op's table (from the op's own `createTable` columns, or — at
/// apply/render time — from the live table). When `columns` is `None`, the
/// `ColRef`-resolution check (c) is SKIPPED (the caller could not resolve the
/// live schema yet — structural checks (a),(b),(d) still run).
#[derive(Debug, Clone)]
pub struct TargetScope<'a> {
    /// The enclosing op's single target table.
    pub table: &'a str,
    /// The valid columns on that table, or `None` to skip the (c) resolution.
    pub columns: Option<&'a [String]>,
}

impl<'a> TargetScope<'a> {
    /// A scope that resolves `ColRef`s against the given column set.
    #[must_use]
    pub fn new(table: &'a str, columns: &'a [String]) -> Self {
        Self { table, columns: Some(columns) }
    }

    /// A scope that does NOT resolve `ColRef`s (structural-only validation).
    #[must_use]
    pub fn structural_only(table: &'a str) -> Self {
        Self { table, columns: None }
    }
}

/// Validate one expression-AST tree structurally for a `target_dialect` against a
/// `scope`. Returns the first [`AuthoringError`] or `Ok(())`.
///
/// `op_index` / `ts_location` are stamped onto any emitted error so the AI loop
/// gets the §8.8 payload.
///
/// # Errors
/// Returns an [`AuthoringError`] for an out-of-envelope `splitPart` (b), a
/// `ColRef` to a column not on the target table (c), or — defensively — a node
/// the structural policy rejects (a). Allow-listed in-policy nodes validate.
pub fn validate_expr(
    expr: &Expr,
    target_dialect: Dialect,
    scope: &TargetScope<'_>,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    let ctx = Ctx { target_dialect, scope, op_index, ts_location };
    ctx.walk(expr)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExprVolatility {
    Immutable,
    Stable,
    Volatile,
}

fn scalar_fn_volatility(f: ScalarFn) -> ExprVolatility {
    match f {
        ScalarFn::CurrentSetting | ScalarFn::CurrentUser => ExprVolatility::Stable,
        ScalarFn::Coalesce
        | ScalarFn::Nullif
        | ScalarFn::Lower
        | ScalarFn::Upper
        | ScalarFn::Trim
        | ScalarFn::Length
        | ScalarFn::Abs
        | ScalarFn::Mod
        | ScalarFn::Round
        | ScalarFn::Floor
        | ScalarFn::Ceil
        | ScalarFn::Substr
        | ScalarFn::Replace => ExprVolatility::Immutable,
    }
}

fn synth_fn_volatility(f: SynthFn) -> ExprVolatility {
    match f {
        SynthFn::Now | SynthFn::GenRandomUuid => ExprVolatility::Volatile,
        SynthFn::ConcatWs | SynthFn::SplitPart => ExprVolatility::Immutable,
    }
}

fn scalar_fn_name(f: ScalarFn) -> &'static str {
    match f {
        ScalarFn::Coalesce => "coalesce",
        ScalarFn::Nullif => "nullif",
        ScalarFn::Lower => "lower",
        ScalarFn::Upper => "upper",
        ScalarFn::Trim => "trim",
        ScalarFn::Length => "length",
        ScalarFn::Abs => "abs",
        ScalarFn::Mod => "mod",
        ScalarFn::Round => "round",
        ScalarFn::Floor => "floor",
        ScalarFn::Ceil => "ceil",
        ScalarFn::Substr => "substr",
        ScalarFn::Replace => "replace",
        ScalarFn::CurrentSetting => "currentSetting",
        ScalarFn::CurrentUser => "currentUser",
    }
}

fn synth_fn_name(f: SynthFn) -> &'static str {
    match f {
        SynthFn::ConcatWs => "concatWs",
        SynthFn::SplitPart => "splitPart",
        SynthFn::Now => "now",
        SynthFn::GenRandomUuid => "genRandomUuid",
    }
}

fn agg_func_name(f: AggFunc) -> &'static str {
    match f {
        AggFunc::Count => "count",
        AggFunc::Sum => "sum",
        AggFunc::Avg => "avg",
        AggFunc::Min => "min",
        AggFunc::Max => "max",
    }
}

fn first_aggregate(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::ColRef { .. } | Expr::Literal { .. } | Expr::PgInterval { .. } => None,
        Expr::BinOp { lhs, rhs, .. } => first_aggregate(lhs).or_else(|| first_aggregate(rhs)),
        Expr::UnaryOp { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::PgRegexMatch { expr: operand, .. }
        | Expr::PgColumnSize { expr: operand }
        | Expr::Extract { from: operand, .. }
        | Expr::PgExtract { from: operand, .. } => first_aggregate(operand),
        Expr::Case { branches, r#else } => branches
            .iter()
            .find_map(|CaseBranch { when, then }| {
                first_aggregate(when).or_else(|| first_aggregate(then))
            })
            .or_else(|| r#else.as_deref().and_then(first_aggregate)),
        Expr::FnCall { args, .. } | Expr::FnSynth { args, .. } => {
            args.iter().find_map(first_aggregate)
        }
        Expr::Between { operand, low, high } => first_aggregate(operand)
            .or_else(|| first_aggregate(low))
            .or_else(|| first_aggregate(high)),
        Expr::Like { operand, pattern } => {
            first_aggregate(operand).or_else(|| first_aggregate(pattern))
        }
        Expr::DistinctFrom { left, right } => {
            first_aggregate(left).or_else(|| first_aggregate(right))
        }
        Expr::Agg { func, .. } => Some(agg_func_name(*func)),
        Expr::InList { expr, .. } => first_aggregate(expr),
        Expr::Dialectal { default, pg, sqlite, mysql } => [
            default.as_deref(),
            pg.as_deref(),
            sqlite.as_deref(),
            mysql.as_deref(),
        ]
        .into_iter()
        .flatten()
        .find_map(first_aggregate),
    }
}

fn first_volatile_function(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::ColRef { .. } | Expr::Literal { .. } | Expr::PgInterval { .. } => None,
        Expr::BinOp { lhs, rhs, .. } => {
            first_volatile_function(lhs).or_else(|| first_volatile_function(rhs))
        }
        Expr::UnaryOp { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::PgRegexMatch { expr: operand, .. }
        | Expr::PgColumnSize { expr: operand }
        | Expr::Extract { from: operand, .. }
        | Expr::PgExtract { from: operand, .. } => first_volatile_function(operand),
        Expr::Case { branches, r#else } => branches
            .iter()
            .find_map(|CaseBranch { when, then }| {
                first_volatile_function(when).or_else(|| first_volatile_function(then))
            })
            .or_else(|| r#else.as_deref().and_then(first_volatile_function)),
        Expr::FnCall { r#fn, args } => {
            if scalar_fn_volatility(*r#fn) == ExprVolatility::Volatile {
                return Some(scalar_fn_name(*r#fn));
            }
            args.iter().find_map(first_volatile_function)
        }
        Expr::FnSynth { r#fn, args } => {
            if synth_fn_volatility(*r#fn) == ExprVolatility::Volatile {
                return Some(synth_fn_name(*r#fn));
            }
            args.iter().find_map(first_volatile_function)
        }
        Expr::Between { operand, low, high } => first_volatile_function(operand)
            .or_else(|| first_volatile_function(low))
            .or_else(|| first_volatile_function(high)),
        Expr::Like { operand, pattern } => {
            first_volatile_function(operand).or_else(|| first_volatile_function(pattern))
        }
        Expr::DistinctFrom { left, right } => {
            first_volatile_function(left).or_else(|| first_volatile_function(right))
        }
        Expr::Agg { arg, .. } => arg.as_deref().and_then(first_volatile_function),
        Expr::InList { expr, .. } => first_volatile_function(expr),
        Expr::Dialectal { default, pg, sqlite, mysql } => {
            [
                default.as_deref(),
                pg.as_deref(),
                sqlite.as_deref(),
                mysql.as_deref(),
            ]
            .into_iter()
            .flatten()
            .find_map(first_volatile_function)
        }
    }
}

fn validate_immutable_expr_context(
    expr: &Expr,
    context: &str,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    validate_no_aggregate_expr_context(expr, context, target_dialect, op_index, ts_location)?;
    let Some(function_name) = first_volatile_function(expr) else {
        return Ok(());
    };
    Err(AuthoringError {
        code: CODE_IMMUTABLE_CONTEXT_VOLATILE.to_string(),
        kind: Some(UnsupportedKind::Expr),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason: format!(
            "{context} requires an immutable expression, but contains volatile function {function_name}()"
        ),
        suggested_fix: Some(format!(
            "remove {function_name}() from the {context}; use it only in defaults, DML values, or another non-immutable expression context"
        )),
    })
}

fn validate_no_aggregate_expr_context(
    expr: &Expr,
    context: &str,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    let Some(aggregate_name) = first_aggregate(expr) else {
        return Ok(());
    };
    Err(AuthoringError {
        code: CODE_AGGREGATE_IN_SCALAR_CONTEXT.to_string(),
        kind: Some(UnsupportedKind::Expr),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason: format!(
            "{context} requires a scalar expression, but contains aggregate {aggregate_name}()"
        ),
        suggested_fix: Some(format!(
            "remove {aggregate_name}() from the {context}; use aggregates only in \
             grouped SELECT/HAVING expression contexts"
        )),
    })
}

/// Walk an entire [`MigrationIr`](crate::model::ir::MigrationIr) and validate EVERY
/// embedded expression-AST node against `target_dialect` — the §3.3.1.1 "the
/// Rust validator is the authoritative STRUCTURAL gate" obligation made
/// operative. Checks (a)/(b)/(d) run at load for every Expr slot; check (c)
/// (`ColRef` resolution) runs here only for a self-contained `createTable`, and
/// otherwise at the apply/render seam (see the module note).
///
/// This is the walker that enumerates each [`Op`](crate::model::ir::Op) variant's
/// expression positions and calls [`validate_expr`] per node with the enclosing
/// op's index + single target table as scope:
///
/// - `createTable` — each `IrIndex` element expression, each `IrIndex.where`
///   partial-index predicate + each `Check` constraint `expr` (scoped to the
///   table's own declared columns, so rule (c) `ColRef` resolution runs against
///   them).
/// - `createIndex` — each index element expression + the `where` partial-index
///   predicate (closed AST since the property-A fix).
/// - `setColumnType` — the `using` cast expression (closed AST since the
///   property-A fix).
/// - `addConstraint` — a `Check` constraint `expr`.
/// - `update` — every `set` RHS + the optional `where`.
/// - `delete` — the mandatory `where`.
/// - `backfill` — every `set` RHS + the optional `filter`.
///
/// Ops with no expression slot (e.g. `dropTable`, `addColumn`, `insert`) walk to
/// `Ok(())`. For the DML ops (`update`/`delete`/`backfill`) and `setColumnType`
/// the live-schema column set is generally not known at IR-load time, so the
/// scope is [`TargetScope::structural_only`] — the structural checks (a),(b),(d)
/// still run; the apply/render seam (a later wave) re-runs the walk with a
/// resolved column set to enforce (c). A `createTable` is self-contained, so its
/// embedded predicates ARE resolved against the table's own columns here.
///
/// Returns the FIRST [`AuthoringError`] encountered, or `Ok(())`.
///
/// `ts_locations`, when supplied, maps a 0-based op index to its `.ts` source
/// location for the §8.8 payload; a missing entry yields `None`.
///
/// # Errors
/// Returns the first [`AuthoringError`] any embedded expression produces.
pub fn validate_ir(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    validate_ir_scoped(
        ir,
        target_dialect,
        ts_locations,
        None,
        &PolicyProfile::confined(),
    )
}

/// **PR10** — [`validate_ir`] threaded with the active schema confinement scope
/// (§2.7). `schema_scope`:
/// - `None` ⇒ omitted/default public capability: no project schema is known, so
///   cross-schema checks are not applied, but vendor capabilities stay confined.
/// - `Some(SchemaScope::Single(project_schema))` ⇒ the **Confined** creator
///   profile: an explicit `schema != project_schema` is REFUSED fail-closed
///   ([`CODE_CROSS_SCHEMA`]).
/// - `Some(SchemaScope::Allowlist([...]))` ⇒ the **Platform** profile: an explicit
///   `schema` must be a member of the allow-list.
/// - `Some(SchemaScope::Unconfined)` ⇒ the explicit **Trusted** operator profile:
///   no cross-schema confinement and full vendor capability.
///
/// # Errors
/// The first [`AuthoringError`] any op produces (cross-schema, invalid schema ident,
/// illegal guard direction, or an embedded-expression rejection).
pub fn validate_ir_scoped(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
    schema_scope: Option<&crate::model::policy::SchemaScope>,
    policy_profile: &PolicyProfile,
) -> Result<(), AuthoringError> {
    for (op_index, op) in ir.ops.iter().enumerate() {
        let ts = ts_locations.get(op_index).and_then(Option::as_deref);
        validate_op_scoped(
            op,
            target_dialect,
            op_index,
            ts,
            schema_scope,
            policy_profile,
        )?;
    }
    validate_partition_recording(ir, target_dialect, ts_locations)?;
    Ok(())
}

#[derive(Debug, Clone)]
struct PartitionUniqueEntry {
    op_index: usize,
    label: &'static str,
    columns: Vec<String>,
}

#[derive(Debug, Clone)]
struct PartitionParentFold {
    op_index: usize,
    spec: crate::model::ir::PartitionSpec,
    not_null_columns: std::collections::BTreeSet<String>,
    unique_entries: Vec<PartitionUniqueEntry>,
    children: std::collections::BTreeMap<String, (usize, crate::model::ir::PartitionBounds)>,
}

fn partition_error(
    code: &'static str,
    op_index: usize,
    ts_locations: &[Option<String>],
    dialect: Dialect,
    reason: impl Into<String>,
    suggested_fix: impl Into<String>,
) -> AuthoringError {
    AuthoringError {
        code: code.to_string(),
        kind: None,
        op_index,
        ts_location: ts_locations.get(op_index).cloned().flatten(),
        dialect,
        reason: reason.into(),
        suggested_fix: Some(suggested_fix.into()),
    }
}

fn partition_spec_label(spec: &crate::model::ir::PartitionSpec) -> &'static str {
    match spec {
        crate::model::ir::PartitionSpec::Range { .. } => "range",
        crate::model::ir::PartitionSpec::List { .. } => "list",
        crate::model::ir::PartitionSpec::Hash { .. } => "hash",
    }
}

fn index_column_names(index_columns: &[crate::model::ir::IndexElement]) -> Vec<String> {
    index_columns
        .iter()
        .filter_map(|element| match element {
            crate::model::ir::IndexElement::Column { name, .. } => Some(name.clone()),
            crate::model::ir::IndexElement::Expr { .. } => None,
        })
        .collect()
}

fn exclusion_column_names(elements: &[crate::model::ir::ExclusionElement]) -> Vec<String> {
    elements
        .iter()
        .filter_map(|element| match &element.target {
            crate::model::ir::ColumnOrExpr::Column { name } => Some(name.clone()),
            crate::model::ir::ColumnOrExpr::Expr { .. } => None,
        })
        .collect()
}

fn partition_bound_key(value: &crate::model::ir::PartitionBoundValue) -> String {
    match value {
        crate::model::ir::PartitionBoundValue::String { value } => format!("s:{value}"),
        crate::model::ir::PartitionBoundValue::Int { value } => format!("i:{}", value.get()),
        crate::model::ir::PartitionBoundValue::MinValue => "min".to_string(),
        crate::model::ir::PartitionBoundValue::MaxValue => "max".to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PartitionComparableBound<'a> {
    Min,
    Int(i64),
    String(&'a str),
    Max,
}

fn comparable_bound(
    value: &crate::model::ir::PartitionBoundValue,
) -> PartitionComparableBound<'_> {
    match value {
        crate::model::ir::PartitionBoundValue::String { value } => {
            PartitionComparableBound::String(value)
        }
        crate::model::ir::PartitionBoundValue::Int { value } => {
            PartitionComparableBound::Int(value.get())
        }
        crate::model::ir::PartitionBoundValue::MinValue => PartitionComparableBound::Min,
        crate::model::ir::PartitionBoundValue::MaxValue => PartitionComparableBound::Max,
    }
}

fn compare_bound_tuple(
    lhs: &[crate::model::ir::PartitionBoundValue],
    rhs: &[crate::model::ir::PartitionBoundValue],
) -> Option<std::cmp::Ordering> {
    if lhs.len() != rhs.len() {
        return None;
    }
    for (l, r) in lhs.iter().zip(rhs) {
        let l = comparable_bound(l);
        let r = comparable_bound(r);
        match (l, r) {
            (PartitionComparableBound::Int(_), PartitionComparableBound::String(_))
            | (PartitionComparableBound::String(_), PartitionComparableBound::Int(_)) => {
                return None;
            }
            _ => {}
        }
        let ord = l.cmp(&r);
        if !ord.is_eq() {
            return Some(ord);
        }
    }
    Some(std::cmp::Ordering::Equal)
}

fn hash_gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

fn hash_lcm(a: u128, b: u128) -> Option<u128> {
    if a == 0 || b == 0 {
        return None;
    }
    a.checked_div(hash_gcd(a, b))?.checked_mul(b)
}

fn validate_partition_recording(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    use crate::model::ir::{IrConstraintKind, Op, PartitionSpec};

    let mut parents: std::collections::BTreeMap<String, PartitionParentFold> =
        std::collections::BTreeMap::new();

    for (op_index, op) in ir.ops.iter().enumerate() {
        match op {
            Op::CreateTable {
                name,
                columns,
                primary_key,
                constraints,
                indexes,
                partition_by: Some(spec),
                ..
            } => {
                let mut not_null_columns = std::collections::BTreeSet::new();
                for column in columns {
                    if column.nullable == Some(false) {
                        not_null_columns.insert(column.name.clone());
                    }
                }
                let mut unique_entries = Vec::new();
                if let Some(pk) = primary_key {
                    for column in pk {
                        not_null_columns.insert(column.clone());
                    }
                    unique_entries.push(PartitionUniqueEntry {
                        op_index,
                        label: "primary key",
                        columns: pk.clone(),
                    });
                }
                for column in columns {
                    if column.unique.unwrap_or(false) {
                        unique_entries.push(PartitionUniqueEntry {
                            op_index,
                            label: "column unique",
                            columns: vec![column.name.clone()],
                        });
                    }
                }
                for constraint in constraints {
                    match &constraint.kind {
                        IrConstraintKind::Unique { columns } => {
                            unique_entries.push(PartitionUniqueEntry {
                                op_index,
                                label: "unique constraint",
                                columns: columns.clone(),
                            });
                        }
                        IrConstraintKind::Exclusion { elements, .. } => {
                            unique_entries.push(PartitionUniqueEntry {
                                op_index,
                                label: "exclusion constraint",
                                columns: exclusion_column_names(elements),
                            });
                        }
                        _ => {}
                    }
                }
                for index in indexes {
                    if index.unique.unwrap_or(false) {
                        unique_entries.push(PartitionUniqueEntry {
                            op_index,
                            label: "unique index",
                            columns: index_column_names(&index.columns),
                        });
                    }
                }
                parents.insert(
                    name.clone(),
                    PartitionParentFold {
                        op_index,
                        spec: spec.clone(),
                        not_null_columns,
                        unique_entries,
                        children: std::collections::BTreeMap::new(),
                    },
                );
            }
            Op::CreateTable { name, .. } => {
                parents.remove(name);
            }
            Op::DropTable { table, .. } => {
                parents.remove(table);
            }
            Op::RenameTable { table, to, .. } => {
                if let Some(parent) = parents.remove(table) {
                    parents.insert(to.clone(), parent);
                }
            }
            Op::CreatePartition { name, of, bounds, .. } => {
                if let Some(parent) = parents.get_mut(of) {
                    parent.children.insert(name.clone(), (op_index, bounds.clone()));
                } else if !matches!(target_dialect, Dialect::Postgres) {
                    return Err(partition_error(
                        CODE_DIALECT_UNSUPPORTED,
                        op_index,
                        ts_locations,
                        target_dialect,
                        format!(
                            "createPartition {name:?} targets parent {of:?}, but this recording does not contain a collapse-affirmed partitioned parent to authorize the no-DDL leg"
                        ),
                        "record the partitioned parent with partitionBy.whenUnsupported: \"collapse\" in the same fold, or target Postgres for native partition DDL",
                    ));
                }
            }
            Op::AttachPartition { parent, name, bound, .. } => {
                if let Some(parent) = parents.get_mut(parent) {
                    parent.children.insert(name.clone(), (op_index, bound.clone()));
                } else if !matches!(target_dialect, Dialect::Postgres) {
                    return Err(partition_error(
                        CODE_DIALECT_UNSUPPORTED,
                        op_index,
                        ts_locations,
                        target_dialect,
                        format!(
                            "attachPartition {name:?} targets parent {parent:?}, but attachPartition is PostgreSQL-only"
                        ),
                        "target Postgres for native partition attach",
                    ));
                }
            }
            Op::DropPartition { parent, name, .. } => {
                if let Some(parent_state) = parents.get(parent) {
                    if parent_state.spec.collapse()
                        && parent_state
                            .children
                            .get(name)
                            .is_some_and(|(_, bounds)| matches!(bounds, crate::model::ir::PartitionBounds::Hash { .. }))
                    {
                        return Err(partition_error(
                            CODE_PARTITION_HASH_DROP_UNDERIVABLE,
                            op_index,
                            ts_locations,
                            target_dialect,
                            format!(
                                "dropping hash partition {name:?} from collapse-affirmed parent {parent:?} has no portable row predicate"
                            ),
                            "omit partitionBy.whenUnsupported for PG-only hash repartitioning, or avoid dropping hash children under collapse",
                        ));
                    }
                }
                if let Some(parent_state) = parents.get_mut(parent) {
                    parent_state.children.remove(name);
                }
            }
            Op::SetColumnNotNull { table, column, .. } => {
                if let Some(parent) = parents.get_mut(table) {
                    parent.not_null_columns.insert(column.clone());
                }
            }
            Op::DropColumnNotNull { table, column, .. } => {
                if let Some(parent) = parents.get_mut(table) {
                    parent.not_null_columns.remove(column);
                }
            }
            Op::AddConstraint { table, constraint, .. } => {
                if let Some(parent) = parents.get_mut(table) {
                    match &constraint.kind {
                        IrConstraintKind::Unique { columns } => {
                            parent.unique_entries.push(PartitionUniqueEntry {
                                op_index,
                                label: "unique constraint",
                                columns: columns.clone(),
                            });
                        }
                        IrConstraintKind::Exclusion { elements, .. } => {
                            parent.unique_entries.push(PartitionUniqueEntry {
                                op_index,
                                label: "exclusion constraint",
                                columns: exclusion_column_names(elements),
                            });
                        }
                        _ => {}
                    }
                }
            }
            Op::CreateIndex {
                table,
                columns,
                unique,
                ..
            } if unique.unwrap_or(false) => {
                if let Some(parent) = parents.get_mut(table) {
                    parent.unique_entries.push(PartitionUniqueEntry {
                        op_index,
                        label: "unique index",
                        columns: index_column_names(columns),
                    });
                }
            }
            _ => {}
        }
    }

    for (table, parent) in &parents {
        let key_columns = parent.spec.columns();
        for entry in &parent.unique_entries {
            let cols: std::collections::BTreeSet<&str> =
                entry.columns.iter().map(String::as_str).collect();
            if let Some(missing) = key_columns.iter().find(|key| !cols.contains(key.as_str())) {
                return Err(partition_error(
                    CODE_PARTITION_KEY_COVERAGE,
                    entry.op_index,
                    ts_locations,
                    target_dialect,
                    format!(
                        "partitioned table {table:?} has a {} that does not include partition key column {missing:?}",
                        entry.label
                    ),
                    "include every partition key column in each primary key, unique constraint, unique index, and exclusion constraint on the partitioned table",
                ));
            }
        }

        validate_partition_bounds_well_formed(table, parent, target_dialect, ts_locations)?;

        if parent.spec.collapse() {
            if matches!(parent.spec, PartitionSpec::Range { .. }) && key_columns.len() != 1 {
                return Err(partition_error(
                    CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED,
                    parent.op_index,
                    ts_locations,
                    target_dialect,
                    format!(
                        "collapse-affirmed range partitioning on table {table:?} has {} partition key columns; v1 collapse supports exactly one",
                        key_columns.len()
                    ),
                    "use a single range partition key for collapse, or omit whenUnsupported and target Postgres only",
                ));
            }
            for key in key_columns {
                if !parent.not_null_columns.contains(key) {
                    return Err(partition_error(
                        CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE,
                        parent.op_index,
                        ts_locations,
                        target_dialect,
                        format!(
                            "collapse-affirmed partitioned table {table:?} has nullable partition key column {key:?}"
                        ),
                        "mark every partition key column notNull, or omit whenUnsupported and target Postgres only",
                    ));
                }
            }
            validate_partition_bounds_total(table, parent, target_dialect, ts_locations)?;
        }
    }

    Ok(())
}

fn validate_partition_bounds_well_formed(
    table: &str,
    parent: &PartitionParentFold,
    dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    use crate::model::ir::{PartitionBounds, PartitionSpec};

    match &parent.spec {
        PartitionSpec::Range { columns, .. } => {
            let mut ranges: Vec<(
                usize,
                &[crate::model::ir::PartitionBoundValue],
                &[crate::model::ir::PartitionBoundValue],
            )> = Vec::new();
            for (_name, (op_index, bounds)) in &parent.children {
                match bounds {
                    PartitionBounds::Range { from, to } => {
                        if from.len() != columns.len() || to.len() != columns.len() {
                            return Err(partition_error(
                                CODE_PARTITION_BOUNDS_ILL_FORMED,
                                *op_index,
                                ts_locations,
                                dialect,
                                format!(
                                    "range partition child on table {table:?} has bound arity from={} to={} for {} partition key columns",
                                    from.len(),
                                    to.len(),
                                    columns.len()
                                ),
                                "make each range bound tuple match the partition key arity",
                            ));
                        }
                        if !matches!(
                            compare_bound_tuple(from, to),
                            Some(std::cmp::Ordering::Less)
                        ) {
                            return Err(partition_error(
                                CODE_PARTITION_BOUNDS_ILL_FORMED,
                                *op_index,
                                ts_locations,
                                dialect,
                                format!(
                                    "range partition child on table {table:?} has an empty, reversed, or incomparable FROM/TO bound"
                                ),
                                "use non-empty range bounds with comparable value kinds and FROM < TO",
                            ));
                        }
                        ranges.push((*op_index, from, to));
                    }
                    PartitionBounds::Default => {}
                    _ => {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!("range partitioned table {table:?} has a non-range child bound"),
                            "use range bounds or a default child under a range-partitioned parent",
                        ));
                    }
                }
            }
            for i in 0..ranges.len() {
                for j in (i + 1)..ranges.len() {
                    let (_, a_from, a_to) = ranges[i];
                    let (b_op, b_from, b_to) = ranges[j];
                    let overlaps = matches!(
                        compare_bound_tuple(a_from, b_to),
                        Some(std::cmp::Ordering::Less)
                    ) && matches!(
                        compare_bound_tuple(b_from, a_to),
                        Some(std::cmp::Ordering::Less)
                    );
                    if overlaps {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            b_op,
                            ts_locations,
                            dialect,
                            format!("range partition bounds on table {table:?} overlap"),
                            "make sibling range partition bounds pairwise non-overlapping",
                        ));
                    }
                }
            }
        }
        PartitionSpec::List { .. } => {
            let mut seen = std::collections::BTreeSet::new();
            for (_name, (op_index, bounds)) in &parent.children {
                match bounds {
                    PartitionBounds::List { values } => {
                        for value in values {
                            let key = partition_bound_key(value);
                            if !seen.insert(key) {
                                return Err(partition_error(
                                    CODE_PARTITION_BOUNDS_ILL_FORMED,
                                    *op_index,
                                    ts_locations,
                                    dialect,
                                    format!(
                                        "list partition value {} appears more than once on table {table:?}",
                                        partition_bound_key(value)
                                    ),
                                    "ensure each list-bound value appears at most once across all sibling partitions",
                                ));
                            }
                        }
                    }
                    PartitionBounds::Default => {}
                    _ => {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!("list partitioned table {table:?} has a non-list child bound"),
                            "use list bounds or a default child under a list-partitioned parent",
                        ));
                    }
                }
            }
        }
        PartitionSpec::Hash { .. } => {
            let mut classes = Vec::new();
            for (_name, (op_index, bounds)) in &parent.children {
                match bounds {
                    PartitionBounds::Default => {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!("hash partitioned table {table:?} cannot have a default child"),
                            "remove the default child from hash partitioning and use modulus/remainder bounds",
                        ));
                    }
                    PartitionBounds::Hash { modulus, remainder } => {
                    if *modulus == 0 || *remainder >= *modulus {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!(
                                "hash partition on table {table:?} has modulus {modulus} and remainder {remainder}; remainder must be less than a non-zero modulus"
                            ),
                            "use hash bounds with modulus > 0 and remainder < modulus",
                        ));
                    }
                    classes.push((*op_index, u128::from(*modulus), u128::from(*remainder)));
                    }
                    _ => {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!("hash partitioned table {table:?} has a non-hash child bound"),
                            "use modulus/remainder bounds under a hash-partitioned parent",
                        ));
                    }
                }
            }
            for i in 0..classes.len() {
                for j in (i + 1)..classes.len() {
                    let (op_index, m1, r1) = classes[i];
                    let (op_index2, m2, r2) = classes[j];
                    let (small_m, small_r, large_m, large_r, err_op) = if m1 <= m2 {
                        (m1, r1, m2, r2, op_index2)
                    } else {
                        (m2, r2, m1, r1, op_index)
                    };
                    if large_m % small_m != 0 {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            err_op,
                            ts_locations,
                            dialect,
                            format!(
                                "hash partition moduli {m1} and {m2} on table {table:?} are not comparable by divisibility"
                            ),
                            "use hash partition moduli where every pair is comparable by divisibility",
                        ));
                    }
                    if large_r % small_m == small_r {
                        return Err(partition_error(
                            CODE_PARTITION_BOUNDS_ILL_FORMED,
                            err_op,
                            ts_locations,
                            dialect,
                            format!(
                                "hash partition congruence classes ({m1},{r1}) and ({m2},{r2}) overlap on table {table:?}"
                            ),
                            "use non-overlapping hash remainder classes",
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_partition_bounds_total(
    table: &str,
    parent: &PartitionParentFold,
    dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    use crate::model::ir::{PartitionBounds, PartitionSpec};

    match &parent.spec {
        PartitionSpec::Range { .. } | PartitionSpec::List { .. } => {
            if !parent
                .children
                .values()
                .any(|(_, bounds)| matches!(bounds, PartitionBounds::Default))
            {
                return Err(partition_error(
                    CODE_PARTITION_BOUNDS_NOT_TOTAL,
                    parent.op_index,
                    ts_locations,
                    dialect,
                    format!(
                        "collapse-affirmed {} partitioned table {table:?} has no default child",
                        partition_spec_label(&parent.spec)
                    ),
                    "add a .partition(...).create({ default: true }) child, or omit whenUnsupported and target Postgres only",
                ));
            }
        }
        PartitionSpec::Hash { .. } => {
            let mut lcm = 1_u128;
            let mut classes = Vec::new();
            for (_name, (op_index, bounds)) in &parent.children {
                if let PartitionBounds::Hash { modulus, remainder } = bounds {
                    lcm = hash_lcm(lcm, u128::from(*modulus)).ok_or_else(|| {
                        partition_error(
                            CODE_PARTITION_BOUNDS_NOT_TOTAL,
                            *op_index,
                            ts_locations,
                            dialect,
                            format!(
                                "hash partition modulus set on table {table:?} overflows the validator's exact lcm arithmetic"
                            ),
                            "use smaller hash moduli or avoid collapse affirmation for this hash partition set",
                        )
                    })?;
                    classes.push((u128::from(*modulus), u128::from(*remainder)));
                }
            }
            let covered: u128 = classes.iter().map(|(m, _)| lcm / *m).sum();
            if covered != lcm {
                return Err(partition_error(
                    CODE_PARTITION_BOUNDS_NOT_TOTAL,
                    parent.op_index,
                    ts_locations,
                    dialect,
                    format!(
                        "collapse-affirmed hash partitioned table {table:?} covers {covered} of {lcm} residue classes"
                    ),
                    "declare hash children whose modulus/remainder classes cover every residue in 0..lcm(moduli)-1",
                ));
            }
        }
    }
    Ok(())
}

/// Validate every expression slot of a single [`Op`](crate::model::ir::Op) at
/// `op_index`. The per-variant Expr enumeration the SOLE-gate property needs;
/// see [`validate_ir`] for the slot map.
///
/// # Errors
/// Returns the first [`AuthoringError`] any embedded expression produces.
pub fn validate_op(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    // The bare entry keeps the Trusted posture (no cross-schema confinement); the
    // schema-ident + guard-direction checks still run (trust-independent).
    validate_op_scoped(
        op,
        target_dialect,
        op_index,
        ts_location,
        None,
        &PolicyProfile::confined(),
    )
}

/// **PR10** — [`validate_op`] threaded with the active
/// [`SchemaScope`](crate::model::policy::SchemaScope) (§2.7). Runs the schema/guard gate
/// FIRST, then the per-op expression-slot checks.
///
/// # Errors
/// Returns the first [`AuthoringError`] the gate or any embedded expression produces.
pub fn validate_op_scoped(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
    policy_profile: &PolicyProfile,
) -> Result<(), AuthoringError> {
    use crate::model::ir::{
        ColumnOrExpr, IndexElement, IrConstraintKind, Op, TriggerAction, ViewQuery,
    };

    // **PR10** — schema confinement + guard-direction gate, BEFORE any expression
    // walk. Fail-closed: a Confined cross-schema op never reaches lower.
    validate_op_schema_and_guard(op, target_dialect, op_index, ts_location, schema_scope)?;

    // **VENDOR (`@zeroship/migrate`)** — the capability-composition gate (vendor
    // spec §3.2 gate 1), BEFORE any expression walk. A privileged vendor op is
    // refused fail-closed when (a) the target is SQLite (every vendor op is
    // `PgOnly`, §4.3), or (b) the active capability set — derived from the threaded
    // [`SchemaScope`] — does not GRANT the op's required capability. The Confined
    // creator/AI posture (`Single` scope) grants nothing, so every vendor op dies
    // here; Platform/Trusted (`Allowlist`/`Unconfined`) grant the operator preset.
    validate_vendor_op(op, target_dialect, op_index, ts_location, schema_scope)?;
    validate_create_table_primary_key_policy(
        op,
        target_dialect,
        op_index,
        ts_location,
        policy_profile,
    )?;
    validate_op_support(op, target_dialect, op_index, ts_location)?;
    validate_sequence_options(op, target_dialect, op_index, ts_location)?;
    validate_function_type_refs(op, target_dialect, op_index, ts_location)?;

    // Constraint-embedded expressions validate against the given table scope.
    let check_constraint =
        |kind: &IrConstraintKind, scope: &TargetScope<'_>| -> Result<(), AuthoringError> {
            match kind {
                IrConstraintKind::Check { expr, .. } => {
                    validate_expr(expr, target_dialect, scope, op_index, ts_location)?;
                    validate_immutable_expr_context(
                        expr,
                        "CHECK constraint",
                        target_dialect,
                        op_index,
                        ts_location,
                    )?;
                }
                IrConstraintKind::Exclusion { elements, where_predicate, .. } => {
                    for element in elements {
                        match &element.target {
                            ColumnOrExpr::Column { name } => {
                                let col = crate::model::expr::Expr::ColRef {
                                    name: name.clone(),
                                    table: None,
                                };
                                validate_expr(&col, target_dialect, scope, op_index, ts_location)?;
                            }
                            ColumnOrExpr::Expr { expr } => {
                                validate_expr(expr, target_dialect, scope, op_index, ts_location)?;
                            }
                        }
                    }
                    if let Some(pred) = where_predicate {
                        validate_expr(pred, target_dialect, scope, op_index, ts_location)?;
                    }
                }
                _ => {}
            }
            Ok(())
        };

    let check_index_element =
        |element: &IndexElement, scope: &TargetScope<'_>| -> Result<(), AuthoringError> {
            match element {
                IndexElement::Column { name, .. } => {
                    let col = crate::model::expr::Expr::ColRef { name: name.clone(), table: None };
                    validate_expr(&col, target_dialect, scope, op_index, ts_location)?;
                }
                IndexElement::Expr { expr } => {
                    validate_expr(expr, target_dialect, scope, op_index, ts_location)?;
                    validate_immutable_expr_context(
                        expr,
                        "index expression",
                        target_dialect,
                        op_index,
                        ts_location,
                    )?;
                }
            }
            Ok(())
        };

    match op {
        Op::CreateTable { name, columns, primary_key, constraints, indexes, .. } => {
            // A resolved createTable is self-contained: ColRefs resolve against
            // the op's explicit columns. Confined record/build paths stamp the
            // seven system fields before checksum; Platform paths do not.
            let cols: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
            let scope = TargetScope::new(name, &cols);
            for ix in indexes {
                for element in &ix.columns {
                    check_index_element(element, &scope)?;
                }
                if let Some(pred) = &ix.r#where {
                    validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
                    validate_immutable_expr_context(
                        pred,
                        "index predicate",
                        target_dialect,
                        op_index,
                        ts_location,
                    )?;
                }
            }
            for c in constraints {
                check_constraint(&c.kind, &scope)?;
            }
            let pk_cols = primary_key.as_deref();
            // **Migration-first P2a (§4)** — the per-column declared-only facets
            // (`id_prefix` / `vector_metric`) carry validate-time bounds: the IR's
            // threat model is a hand-crafted `.ir.json`, so a malformed/reserved
            // prefix or a misplaced metric is refused fail-closed BEFORE lower /
            // checksum, never deferred to a render surprise.
            for col in columns {
                if let Some(generated) = &col.generated {
                    validate_expr(
                        &generated.expr,
                        target_dialect,
                        &scope,
                        op_index,
                        ts_location,
                    )?;
                    validate_immutable_expr_context(
                        &generated.expr,
                        "generated column expression",
                        target_dialect,
                        op_index,
                        ts_location,
                    )?;
                }
                validate_column_facets(col, target_dialect, op_index, ts_location)?;
                validate_identity_placement(
                    col,
                    target_dialect,
                    pk_cols,
                    false,
                    op_index,
                    ts_location,
                )?;
            }
            Ok(())
        }
        Op::SetTableOptions { .. } => Ok(()),
        Op::CreateIndex { table, columns, r#where, .. } => {
            // The index elements and partial-index predicate. The live column set
            // is not known at load (the table pre-exists), so structural-only here.
            let scope = TargetScope::structural_only(table);
            for element in columns {
                check_index_element(element, &scope)?;
            }
            if let Some(pred) = r#where {
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
                validate_immutable_expr_context(
                    pred,
                    "index predicate",
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            Ok(())
        }
        Op::SetColumnType { table, to_type, using, .. } => {
            validate_col_type_position(
                to_type,
                "setColumnType.toType",
                false,
                target_dialect,
                op_index,
                ts_location,
            )?;
            if let Some(cast) = using {
                let scope = TargetScope::structural_only(table);
                validate_expr(cast, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        Op::AddConstraint { table, constraint, .. } => {
            let scope = TargetScope::structural_only(table);
            check_constraint(&constraint.kind, &scope)
        }
        Op::CreateDomain { as_type, check, default, .. } => {
            validate_col_type_position(
                as_type,
                "createDomain.as",
                true,
                target_dialect,
                op_index,
                ts_location,
            )?;
            if let Some(default) = default {
                validate_default_for_type(
                    "createDomain.default",
                    as_type,
                    default,
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            if let Some(check) = check {
                let cols = vec!["VALUE".to_string()];
                let scope = TargetScope::new("domain", &cols);
                validate_expr(check, target_dialect, &scope, op_index, ts_location)?;
                validate_immutable_expr_context(
                    check,
                    "CHECK constraint",
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            Ok(())
        }
        Op::Update { table, set, r#where, .. } => {
            let scope = TargetScope::structural_only(table);
            for value in set.values() {
                if let crate::model::ir::IrValue::Expr(expr) = value {
                    validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                }
            }
            if let Some(pred) = r#where {
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        Op::Delete { table, r#where, .. } => {
            let scope = TargetScope::structural_only(table);
            validate_expr(r#where, target_dialect, &scope, op_index, ts_location)
        }
        Op::Backfill { table, set, filter, .. } => {
            let scope = TargetScope::structural_only(table);
            for value in set.values() {
                if let crate::model::ir::IrValue::Expr(expr) = value {
                    validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                }
            }
            if let Some(pred) = filter {
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        Op::Insert { table, rows, on_conflict, .. } => {
            let scope = TargetScope::structural_only(table);
            for row in rows {
                for value in row {
                    if let crate::model::ir::IrValue::Expr(expr) = value {
                        validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                    }
                }
            }
            if let Some(on_conflict) = on_conflict {
                if let Some(do_update) = &on_conflict.do_update {
                    for value in do_update.values() {
                        if let crate::model::ir::IrValue::Expr(expr) = value {
                            validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                        }
                    }
                }
            }
            Ok(())
        }
        Op::DropIndex { name, table, .. } => {
            // §8.6 fail-closed (HIGH): a DropIndex carries an index `name` and an
            // OPTIONAL owning-table hint. The ownership gate
            // ([`crate::model::load::enforce_ir_ownership`]) checks the op's TARGET
            // TABLE — but a bare-name DropIndex (`table: None`) has no
            // ownership-checkable target, so the gate would SKIP it, letting a
            // hostile `.ir.json` `{op:"dropIndex", name:"<other_app_index>"}` drop
            // ANOTHER app's index cross-tenant. Until a name→owning-table registry
            // resolver exists, we refuse a bare-name DropIndex fail-closed: the
            // author must carry the owning-table hint, which makes the drop
            // ownership-checkable. (A name-only drop is also intrinsically
            // dialect-ambiguous on PG, where an index lives in a schema, not a
            // table.) An `UNSUPPORTED { kind: "op" }` so the AI/author loop's
            // remedy is "carry the owning table".
            if table.is_none() {
                return Err(AuthoringError {
                    code: CODE_UNSUPPORTED.to_string(),
                    kind: Some(UnsupportedKind::Op),
                    op_index,
                    ts_location: ts_location.map(str::to_string),
                    dialect: target_dialect,
                    reason: format!(
                        "dropIndex of {name:?} omits its owning table, so the §8.6 \
                         ownership check cannot resolve the index's owner — a \
                         bare-name index drop is refused fail-closed (it would let a \
                         migration drop another app's index by name)"
                    ),
                    suggested_fix: Some(format!(
                        "name the owning table, e.g. op.dropIndex({name:?}, {{ table: \
                         \"<owning_table>\" }}), so the drop is ownership-checked"
                    )),
                });
            }
            Ok(())
        }
        // **#173** — AddColumn carries the same per-column declared facets
        // (`vector_metric` / standalone `mask`) `createTable` columns do, so it gets the
        // SAME fail-closed facet validation: a `vector_metric` on a non-vector added
        // column is refused [`CODE_VECTOR_METRIC_MISPLACED`] BEFORE lower (mask/kind are
        // already structurally bounded by their closed enums at deserialize). Build a
        // synthetic single-column `IrColumn` view and route it through the shared
        // [`validate_column_facets`]. (`id_prefix` cannot reach here — `Op::AddColumn` has
        // no slot; the recorder fail-closes it — so the prefix arm of the validator is a
        // no-op for this view.)
        Op::AddColumn {
            table,
            column,
            ty,
            nullable,
            default,
            vector_metric,
            case_sensitive,
            mask,
            generated,
            identity,
            ..
        } => {
            if let Some(generated) = generated {
                let scope = TargetScope::structural_only(table);
                validate_expr(
                    &generated.expr,
                    target_dialect,
                    &scope,
                    op_index,
                    ts_location,
                )?;
                validate_immutable_expr_context(
                    &generated.expr,
                    "generated column expression",
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            let view = crate::model::ir::IrColumn {
                name: column.clone(),
                ty: ty.clone(),
                nullable: *nullable,
                default: default.clone(),
                unique: None,
                id_prefix: None,
                vector_metric: *vector_metric,
                case_sensitive: *case_sensitive,
                mask: *mask,
                generated: generated.clone(),
                identity: *identity,
            };
            validate_column_facets(&view, target_dialect, op_index, ts_location)?;
            validate_identity_placement(
                &view,
                target_dialect,
                None,
                true,
                op_index,
                ts_location,
            )
        }
        // VENDOR — a `createPolicy`'s `USING`/`WITH CHECK` predicates are CLOSED
        // `(c) => Expr` ASTs (vendor spec §2.4): validate them STRUCTURALLY (the
        // (a)/(b)/(d) checks) against the policy's target table. The live column set
        // is unknown at load (the table pre-exists), so structural-only here.
        Op::CreatePolicy { table, using, with_check, .. } => {
            let scope = TargetScope::structural_only(table);
            validate_expr(using, target_dialect, &scope, op_index, ts_location)?;
            if let Some(wc) = with_check {
                validate_expr(wc, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        // CROSS-DIALECT CORE — trigger `WHEN` + body statements are CLOSED ASTs.
        // Dialect-impossible actions/facets are refused per facet, not by a
        // whole-construct vendor gate.
        Op::CreateTrigger { table, events, for_each, when, action, .. } => {
            validate_trigger_dialect(
                events,
                *for_each,
                action,
                target_dialect,
                op_index,
                ts_location,
            )?;
            if let Some(w) = when {
                let scope = TargetScope::structural_only(table);
                validate_expr(w, target_dialect, &scope, op_index, ts_location)?;
            }
            if let TriggerAction::Body { statements } = action {
                for stmt in statements {
                    validate_trigger_stmt(
                        stmt,
                        table,
                        target_dialect,
                        op_index,
                        ts_location,
                        schema_scope,
                    )?;
                }
            }
            Ok(())
        }
        // CROSS-DIALECT CORE views. A structured view body is the closed SelectAst
        // subset and needs no vendor capability. A raw body is operator-gated above,
        // then asserted to be exactly one read-only SELECT and re-scanned with the
        // function-body deny-list before admission.
        Op::CreateView { query, .. } => {
            match query {
                ViewQuery::Structured { select } => {
                    validate_select_ast(
                        select,
                        target_dialect,
                        op_index,
                        ts_location,
                        schema_scope,
                    )?;
                }
                ViewQuery::Raw { sql } => {
                    validate_raw_view_body_sql(
                        sql,
                        target_dialect,
                        op_index,
                        ts_location,
                        schema_scope,
                    )?;
                }
            }
            Ok(())
        }
        Op::PgRaw { reason, .. } if reason.trim().is_empty() => Err(AuthoringError {
            code: CODE_PGRAW_REASON_REQUIRED.to_string(),
            kind: Some(UnsupportedKind::Op),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: "pgRaw requires a non-empty reason for auditability".to_string(),
            suggested_fix: Some(
                "pass pg.raw({ sql, reason }) with a short explanation for why raw SQL is required"
                    .to_string(),
            ),
        }),
        Op::CreateRole { superuser, if_not_exists, .. }
            if superuser.unwrap_or(false) && if_not_exists.unwrap_or(false) =>
        {
            Err(AuthoringError {
                code: CODE_UNSUPPORTED.to_string(),
                kind: Some(UnsupportedKind::Op),
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: "createRole cannot combine superuser:true with ifNotExists:true; \
                         the idempotent form requires a PL/pgSQL DO wrapper and SUPERUSER \
                         must never be hidden inside an opaque body"
                    .to_string(),
                suggested_fix: Some(
                    "remove superuser:true; zeroship Platform migrations may create bounded \
                     roles, but must not mint Postgres superusers"
                        .to_string(),
                ),
            })
        }
        // Ops with no embedded expression slot. (`RenameTable` carries only its
        // old/new table NAMES — no Expr — so the schema-ident + guard-direction
        // gate in `validate_op_schema_and_guard` above is the whole check, and the
        // render-time `quote_ident` is the injection-safe identifier seam.) The
        // remaining VENDOR ops carry no embedded Expr — their privileged payload is
        // closed sub-enums (`Privilege`/`TriggerTiming`/…) or the §3-gated raw
        // `body`/`sql` strings (parse-scanned by the guard deny-list at lower).
        Op::RenameColumn { ty, .. } => validate_col_type_position(
            ty,
            "renameColumn.type",
            false,
            target_dialect,
            op_index,
            ts_location,
        ),
        Op::SetColumnDefault { value, .. } => {
            if let crate::model::ir::IrDefault::Expr { expr } = value {
                validate_default_expr(
                    "setColumnDefault.value",
                    expr,
                    target_dialect,
                    op_index,
                    ts_location,
                )?;
            }
            Ok(())
        }
        Op::SetRls { enabled, forced, .. } if enabled.is_none() && forced.is_none() => {
            Err(AuthoringError {
                code: CODE_OP_INVALID.to_string(),
                kind: Some(UnsupportedKind::Op),
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: "setRls needs at least one of { enabled, forced }".to_string(),
                suggested_fix: Some(
                    "set enabled, forced, or both on the setRls op".to_string(),
                ),
            })
        }
        Op::DropTable { .. }
        | Op::CreatePartition { .. }
        | Op::AttachPartition { .. }
        | Op::DetachPartition { .. }
        | Op::DropPartition { .. }
        | Op::RenameTable { .. }
        | Op::DropColumn { .. }
        | Op::SetColumnNotNull { .. }
        | Op::DropColumnNotNull { .. }
        | Op::DropColumnDefault { .. }
        | Op::DropConstraint { .. }
        // ValidateConstraint carries no embedded Expr; its PG-only dialect refusal
        // runs in the op-level `error_from_decision` gate above.
        | Op::ValidateConstraint { .. }
        | Op::CreateEnum { .. }
        | Op::DropEnum { .. }
        | Op::DropDomain { .. }
        | Op::CreateSequence { .. }
        | Op::AlterSequence { .. }
        | Op::DropSequence { .. }
        | Op::Comment { .. }
        | Op::CreateSchema { .. }
        | Op::DropSchema { .. }
        | Op::CreateExtension { .. }
        | Op::DropExtension { .. }
        | Op::CreateRole { .. }
        | Op::AlterRole { .. }
        | Op::DropRole { .. }
        | Op::DropOwnedBy { .. }
        | Op::Grant { .. }
        | Op::Revoke { .. }
        | Op::SetRls { .. }
        | Op::DropPolicy { .. }
        | Op::DropTrigger { .. }
        | Op::DropView { .. }
        | Op::CreateFunction { .. }
        | Op::DropFunction { .. }
        | Op::PgRaw { .. } => Ok(()),
    }
}

fn validate_create_table_primary_key_policy(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    policy_profile: &PolicyProfile,
) -> Result<(), AuthoringError> {
    use crate::model::ir::Op;

    let Op::CreateTable {
        name,
        columns,
        primary_key,
        indexes,
        ..
    } = op
    else {
        return Ok(());
    };

    let err = |code: &str, reason: String, suggested_fix: String| AuthoringError {
        code: code.to_string(),
        kind: None,
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(suggested_fix),
    };

    if let Some(pk_columns) = primary_key {
        if pk_columns.is_empty() {
            return Err(err(
                CODE_PRIMARY_KEY_INVALID,
                format!("createTable {name:?} declares an empty primaryKey"),
                "omit primaryKey for no primary key, or name one or more table columns".to_string(),
            ));
        }

        let mut seen = std::collections::BTreeSet::new();
        let table_columns = columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        for column in pk_columns {
            if !seen.insert(column.as_str()) {
                return Err(err(
                    CODE_PRIMARY_KEY_INVALID,
                    format!(
                        "createTable {name:?} primaryKey names column {column:?} more than once"
                    ),
                    "remove duplicate primaryKey columns".to_string(),
                ));
            }
            if !table_columns.contains(column.as_str()) {
                return Err(err(
                    CODE_PRIMARY_KEY_INVALID,
                    format!(
                        "createTable {name:?} primaryKey names column {column:?}, but that column \
                         is absent from the resolved table"
                    ),
                    "name only columns present in the resolved createTable columns".to_string(),
                ));
            }
        }
    }

    if matches!(
        policy_profile.system_shape.author_primary_key,
        AuthorPrimaryKeyPolicy::Allow
    ) {
        return Ok(());
    }

    let matches_profile = crate::model::table_shape::resolved_create_table_matches_profile(
        columns,
        primary_key,
        indexes,
        policy_profile,
    )
    .map_err(|source| {
        err(
            CODE_TABLE_SHAPE_POLICY,
            format!(
                "createTable {name:?} could not validate against the active table-shape profile: \
                 {source}"
            ),
            "fix the migration policy profile or regenerate the resolved IR".to_string(),
        )
    })?;

    if !matches_profile {
        return Err(err(
            CODE_TABLE_SHAPE_POLICY,
            format!(
                "createTable {name:?} violates the active table-shape profile: \
                 author_primary_key is forbid, so the resolved table must carry the profile \
                 system columns, system indexes, and primaryKey [\"id\"]"
            ),
            "regenerate this migration under the confined profile, or apply it with a profile whose system_shape.author_primary_key is allow".to_string(),
        ));
    }

    Ok(())
}

fn validate_op_support(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::{
        IndexElement, IndexMethod, IrConstraintKind, IrDefault, Op, TriggerAction, TriggerEvent,
        TriggerStmt,
    };
    use crate::model::support::{Feature, Support, SupportDecision};

    fn error_from_decision(
        decision: SupportDecision,
        kind: UnsupportedKind,
        target_dialect: Dialect,
        op_index: usize,
        ts_location: Option<&str>,
    ) -> Option<AuthoringError> {
        let SupportDecision::Unsupported { code, reason } = decision else {
            return None;
        };
        let suggested_fix = match kind {
            UnsupportedKind::Expr => {
                "remove the expression-bearing option for now, or defer this migration until the expression/default renderer lands"
            }
            _ => {
                "remove this unsupported shape, or target a dialect/op shape the current engine declares supported"
            }
        };
        Some(AuthoringError {
            code: code.to_string(),
            kind: Some(kind),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: reason.to_string(),
            suggested_fix: Some(suggested_fix.to_string()),
        })
    }

    fn feature_kind(feature: Feature) -> UnsupportedKind {
        match feature {
            Feature::TableLevelCheck | Feature::AlterColumnUsing => UnsupportedKind::Expr,
            _ => UnsupportedKind::Op,
        }
    }

    fn op_kind(op: &Op) -> UnsupportedKind {
        match op {
            Op::CreateTable { columns, .. } if columns.iter().any(|col| col.identity.is_some()) => {
                UnsupportedKind::Identity
            }
            Op::AddColumn {
                identity: Some(_), ..
            } => UnsupportedKind::Identity,
            Op::SetColumnType { using: Some(_), .. } => UnsupportedKind::Expr,
            Op::AddConstraint {
                constraint,
                ..
            } if matches!(constraint.kind, IrConstraintKind::Check { .. }) => {
                UnsupportedKind::Expr
            }
            _ => UnsupportedKind::Op,
        }
    }

    fn check_feature(
        support: &Support,
        feature: Feature,
        target_dialect: Dialect,
        op_index: usize,
        ts_location: Option<&str>,
    ) -> Result<(), AuthoringError> {
        let Some(feature_support) = support.features.iter().find(|decl| decl.feature == feature)
        else {
            return Ok(());
        };
        if let Some(err) = error_from_decision(
            feature_support.decision(target_dialect),
            feature_kind(feature),
            target_dialect,
            op_index,
            ts_location,
        ) {
            return Err(err);
        }
        Ok(())
    }

    fn default_is_nextval(default: Option<&IrDefault>) -> bool {
        matches!(default, Some(IrDefault::Nextval { .. }))
    }

    fn non_btree_index_method(using: Option<IndexMethod>) -> bool {
        !matches!(using, None | Some(IndexMethod::Btree))
    }

    fn with_storage_params(with: &Option<crate::model::ir::IndexStorageParams>) -> bool {
        with.as_ref().is_some_and(|params| !params.is_empty())
    }

    fn index_elements_have_opclass(columns: &[IndexElement]) -> bool {
        columns.iter().any(|element| {
            matches!(element, IndexElement::Column { opclass: Some(_), .. })
        })
    }

    fn index_elements_have_collation(columns: &[IndexElement]) -> bool {
        columns.iter().any(|element| {
            matches!(element, IndexElement::Column { collation: Some(_), .. })
        })
    }

    fn constraint_kind_not_valid(kind: &IrConstraintKind) -> bool {
        matches!(
            kind,
            IrConstraintKind::Fk { not_valid: Some(true), .. }
                | IrConstraintKind::Check { not_valid: Some(true), .. }
        )
    }

    fn fk_features(
        columns: &[String],
        references_columns: &[String],
        mut check: impl FnMut(Feature) -> Result<(), AuthoringError>,
    ) -> Result<(), AuthoringError> {
        if columns.is_empty() {
            check(Feature::ForeignKeyNoLocalColumn)?;
        } else if columns.len() != 1 {
            check(Feature::CompositeForeignKey)?;
        }
        if !(references_columns.is_empty()
            || (references_columns.len() == 1 && references_columns[0] == "id"))
        {
            check(Feature::NonIdForeignKey)?;
        }
        Ok(())
    }

    let fk_deferrable_consistency =
        |deferrable: &Option<bool>, initially_deferred: &Option<bool>| {
            if *initially_deferred == Some(true) && *deferrable != Some(true) {
                return Err(AuthoringError {
                    code: CODE_OP_INVALID.to_string(),
                    kind: None,
                    op_index,
                    ts_location: ts_location.map(str::to_string),
                    dialect: target_dialect,
                    reason: "initiallyDeferred requires deferrable".to_string(),
                    suggested_fix: Some(
                        "set deferrable: true when initiallyDeferred is true, or omit initiallyDeferred"
                            .to_string(),
                    ),
                });
            }
            Ok(())
        };

    let support = op.support();
    match op {
        Op::CreateTable {
            name,
            partition_by: Some(partition_by),
            ..
        } if !matches!(target_dialect, Dialect::Postgres) && !partition_by.collapse() => {
            return Err(AuthoringError {
                code: CODE_DIALECT_UNSUPPORTED.to_string(),
                kind: Some(UnsupportedKind::Op),
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: format!(
                    "partitioned table {name:?} is native only on Postgres unless partitionBy.whenUnsupported is affirmed as \"collapse\""
                ),
                suggested_fix: Some(
                    "add partitionBy.whenUnsupported: \"collapse\" and satisfy the partition collapse validation rules, or target Postgres only"
                        .to_string(),
                ),
            });
        }
        _ => {}
    }
    if let Some(err) = error_from_decision(
        support.decision(target_dialect),
        op_kind(op),
        target_dialect,
        op_index,
        ts_location,
    ) {
        return Err(err);
    }

    let mut check = |feature| {
        check_feature(&support, feature, target_dialect, op_index, ts_location)
    };

    match op {
        Op::CreateTable {
            columns,
            partition_by,
            constraints,
            indexes,
            ..
        } => {
            if partition_by.is_some() {
                check(Feature::PartitionDdl)?;
            }
            if columns
                .iter()
                .any(|col| default_is_nextval(col.default.as_ref()))
            {
                check(Feature::SequenceDefault)?;
            }
            for constraint in constraints {
                // `NOT VALID` is meaningless at create-time (there are no existing
                // rows to defer, and PostgreSQL rejects `NOT VALID` in `CREATE TABLE`).
                // Refuse it fail-closed on the create-time inline constraint so a
                // hand-crafted IR cannot smuggle it into a silently-dropped slot; it
                // is only authorable via addForeignKey/addCheck (ALTER TABLE ADD
                // CONSTRAINT).
                if constraint_kind_not_valid(&constraint.kind) {
                    return Err(AuthoringError {
                        code: CODE_OP_INVALID.to_string(),
                        kind: None,
                        op_index,
                        ts_location: ts_location.map(str::to_string),
                        dialect: target_dialect,
                        reason: "notValid is only valid on addForeignKey/addCheck (ALTER TABLE ADD CONSTRAINT); a create-time constraint cannot be NOT VALID".to_string(),
                        suggested_fix: Some(
                            "drop notValid from the create() constraint, or add the constraint after createTable via addForeignKey/addCheck with { notValid: true }".to_string(),
                        ),
                    });
                }
                match &constraint.kind {
                    IrConstraintKind::Check { .. } => check(Feature::TableLevelCheck)?,
                    IrConstraintKind::Fk {
                        columns,
                        references_columns,
                        deferrable,
                        initially_deferred,
                        ..
                    } => {
                        check(Feature::TableLevelForeignKey)?;
                        fk_features(columns, references_columns, &mut check)?;
                        fk_deferrable_consistency(deferrable, initially_deferred)?;
                    }
                    IrConstraintKind::Unique { .. } => check(Feature::TableLevelUnique)?,
                    IrConstraintKind::Exclusion { .. } => check(Feature::ExclusionConstraint)?,
                }
            }
            for index in indexes {
                if index
                    .columns
                    .iter()
                    .any(|element| matches!(element, IndexElement::Expr { .. }))
                {
                    check(Feature::ExpressionIndex)?;
                }
                if index.r#where.is_some() {
                    check(Feature::PartialIndex)?;
                }
                if !index.include.is_empty() {
                    check(Feature::IndexInclude)?;
                }
                if with_storage_params(&index.with) {
                    check(Feature::IndexStorageParams)?;
                }
                if index.only.unwrap_or(false) {
                    check(Feature::IndexOnly)?;
                }
                if index.nulls_not_distinct.unwrap_or(false) {
                    check(Feature::IndexNullsNotDistinct)?;
                }
                if index_elements_have_opclass(&index.columns) {
                    check(Feature::IndexOpclass)?;
                }
                if index_elements_have_collation(&index.columns) {
                    check(Feature::IndexCollation)?;
                }
                if non_btree_index_method(index.using) {
                    check(Feature::NonBtreeIndexMethod)?;
                }
            }
        }
        Op::CreatePartition { .. }
        | Op::AttachPartition { .. }
        | Op::DetachPartition { .. }
        | Op::DropPartition { .. } => check(Feature::PartitionDdl)?,
        Op::AddColumn { default, .. } => {
            if default_is_nextval(default.as_ref()) {
                check(Feature::SequenceDefault)?;
            }
        }
        Op::SetColumnType { using: Some(_), .. } => check(Feature::AlterColumnUsing)?,
        Op::SetColumnDefault {
            value: IrDefault::Nextval { .. },
            ..
        } => check(Feature::SequenceDefault)?,
        Op::CreateIndex {
            columns,
            using,
            r#where,
            include,
            with,
            only,
            nulls_not_distinct,
            ..
        } => {
            if columns
                .iter()
                .any(|element| matches!(element, IndexElement::Expr { .. }))
            {
                check(Feature::ExpressionIndex)?;
            }
            if r#where.is_some() {
                check(Feature::PartialIndex)?;
            }
            if !include.is_empty() {
                check(Feature::IndexInclude)?;
            }
            if with_storage_params(with) {
                check(Feature::IndexStorageParams)?;
            }
            if only.unwrap_or(false) {
                check(Feature::IndexOnly)?;
            }
            if nulls_not_distinct.unwrap_or(false) {
                check(Feature::IndexNullsNotDistinct)?;
            }
            if index_elements_have_opclass(columns) {
                check(Feature::IndexOpclass)?;
            }
            if index_elements_have_collation(columns) {
                check(Feature::IndexCollation)?;
            }
            if non_btree_index_method(*using) {
                check(Feature::NonBtreeIndexMethod)?;
            }
        }
        Op::RenameColumn {
            existence_guard: Some(_),
            ..
        } => check(Feature::RenameColumnGuard)?,
        Op::AddConstraint { constraint, .. } => match &constraint.kind {
            IrConstraintKind::Fk {
                columns,
                references_columns,
                deferrable,
                initially_deferred,
                not_valid,
                ..
            } => {
                fk_features(columns, references_columns, &mut check)?;
                fk_deferrable_consistency(deferrable, initially_deferred)?;
                if *not_valid == Some(true) {
                    check(Feature::ConstraintNotValid)?;
                }
            }
            IrConstraintKind::Check { not_valid, .. } => {
                check(Feature::TableLevelCheck)?;
                if *not_valid == Some(true) {
                    check(Feature::ConstraintNotValid)?;
                }
            }
            IrConstraintKind::Exclusion { .. } => check(Feature::ExclusionConstraint)?,
            IrConstraintKind::Unique { .. } => {}
        },
        Op::Insert {
            on_conflict: Some(_),
            ..
        } => check(Feature::InsertOnConflict)?,
        Op::CreateView {
            query,
            replace,
            materialized,
            ..
        } => {
            if matches!(query, crate::model::ir::ViewQuery::Raw { .. }) {
                check(Feature::RawViewBody)?;
            }
            if materialized.unwrap_or(false) {
                check(Feature::MaterializedView)?;
                if replace.unwrap_or(false) {
                    check(Feature::CreateOrReplaceMaterializedView)?;
                }
            }
        }
        Op::DropView { materialized, .. } if materialized.unwrap_or(false) => {
            check(Feature::MaterializedView)?;
        }
        Op::CreateTrigger {
            timing,
            events,
            for_each,
            action,
            when,
            ..
        } => {
            if events.len() > 1 {
                check(Feature::TriggerMultipleEvents)?;
            }
            if events
                .iter()
                .any(|event| matches!(event, TriggerEvent::Truncate))
            {
                check(Feature::TriggerTruncateEvent)?;
            }
            if matches!(timing, crate::model::ir::TriggerTiming::InsteadOf) {
                check(Feature::TriggerInsteadOfTiming)?;
            }
            if matches!(for_each, crate::model::ir::ForEach::Statement) {
                check(Feature::TriggerStatementForEach)?;
            }
            if when.is_some() {
                check(Feature::TriggerWhen)?;
            }
            match action {
                TriggerAction::ExecuteFunction { .. } => check(Feature::TriggerExecuteFunction)?,
                TriggerAction::Body { statements } => {
                    check(Feature::TriggerBody)?;
                    if statements.iter().any(|stmt| {
                        matches!(
                            stmt,
                            TriggerStmt::Raise {
                                level: crate::model::ir::RaiseLevel::Ignore,
                                ..
                            }
                        )
                    }) {
                        check(Feature::TriggerRaiseIgnore)?;
                    }
                }
            }
        }
        Op::Comment { .. } => check(Feature::Comment)?,
        Op::CreateSequence { .. } | Op::AlterSequence { .. } | Op::DropSequence { .. } => {
            check(Feature::Sequence)?;
        }
        Op::PgRaw { .. } => check(Feature::RawSql)?,
        _ => {}
    }

    Ok(())
}

/// **VENDOR (`@zeroship/migrate`)** — the capability-composition gate (vendor
/// spec §3.2 gate 1). For every VENDOR [`Op`](crate::model::ir::Op) variant:
///
/// 1. **SQLite refusal** — every vendor op is `dialect_scope = PgOnly` (no SQLite
///    analogue, §4.3); a SQLite target is refused [`CODE_UNSUPPORTED`] `{kind:"op"}`
///    at load, never silently skipped.
/// 2. **Capability gate** — the active
///    [`VendorCapabilities`](crate::model::capability::VendorCapabilities), derived from the
///    threaded [`SchemaScope`](crate::model::policy::SchemaScope), must GRANT the op's
///    required [`VendorCapability`](crate::model::capability::VendorCapability). The
///    Confined `Single` scope grants nothing ⇒ every vendor op is
///    [`CODE_VENDOR_OP_DENIED`]. The gate keys on the CAPABILITY FLAG
///    (`caps.grants(cap)`), not on a hard-coded profile name.
///
/// A non-vendor op is a no-op here.
fn validate_vendor_op(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    let caps = op.vendor_capabilities();
    if caps.is_empty() {
        return Ok(()); // portable-core op — not gated here.
    };

    // (1) SQLite — every vendor op except RawViewBody is PgOnly. Refuse
    // fail-closed at load. RawViewBody is a raw surface but not PgOnly; SQLite can
    // create plain views from a SELECT body.
    if matches!(target_dialect, Dialect::Sqlite)
        && caps
            .iter()
            .any(|cap| !matches!(cap, crate::model::capability::VendorCapability::RawViewBody))
    {
        let cap = caps
            .iter()
            .find(|cap| !matches!(cap, crate::model::capability::VendorCapability::RawViewBody))
            .copied()
            .expect("non-raw-view cap exists");
        let (reason, fix) = if matches!(cap, crate::model::capability::VendorCapability::MaterializedView)
        {
            (
                "materializedView: SQLite has no materialized views; materialized:true is PostgreSQL-only"
                    .to_string(),
                "drop materialized:true for SQLite, or target Postgres for this view".to_string(),
            )
        } else {
            (
                format!(
                    "the @zeroship/migrate vendor op (capability {:?}) is Postgres-only — \
                     roles/grants/RLS/partitions/policies/triggers/functions/extensions/schemas/pgRaw have \
                     no SQLite analogue (PgOnly)",
                    cap.as_token()
                ),
                "vendor primitives target Postgres only — deploy this migration against a \
                 Postgres backend, or remove the privileged Postgres op"
                    .to_string(),
            )
        };
        return Err(AuthoringError {
            code: CODE_UNSUPPORTED.to_string(),
            kind: Some(UnsupportedKind::Op),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason,
            suggested_fix: Some(fix),
        });
    }

    // (2) The capability-composition gate. Derive the active capability set from the
    // threaded scope (the operator-gated, non-spoofable trust signal) and key on the
    // capability FLAG — never a hard-coded profile name.
    let caps = crate::model::capability::VendorCapabilities::from_scope(schema_scope);
    for cap in op.vendor_capabilities() {
        if !caps.grants(cap) {
            return Err(AuthoringError {
                code: CODE_VENDOR_OP_DENIED.to_string(),
                kind: None,
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: format!(
                    "vendor PG primitive (op capability {:?}) requires the {} capability, which \
                     the active (Confined creator) capability set does not grant — the privileged \
                     @zeroship/migrate primitives are unreachable from a confined migration by \
                     construction (vendor spec §3.2)",
                    cap.as_token(),
                    cap.flag_name(),
                ),
                suggested_fix: Some(format!(
                    "author this privileged migration under the operator/platform capability set \
                     (which composes {}), not the confined creator profile",
                    cap.flag_name(),
                )),
            });
        }
    }
    Ok(())
}

fn validate_function_type_refs(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    let reject = |slot: &'static str, value: &str| AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason: format!(
            "{slot} must be a conservative PostgreSQL type reference (bare or \
             schema-qualified name with optional precision and [] suffixes), not \
             a SQL fragment: {value:?}"
        ),
        suggested_fix: Some(
            "use a type like text, int[], numeric(10,2), or myschema.mytype; \
             function attributes such as SECURITY DEFINER must be explicit \
             structured fields, not smuggled through a type string"
                .to_string(),
        ),
    };

    match op {
        crate::model::ir::Op::CreateFunction { args, returns, .. } => {
            if !crate::model::ir::is_valid_pg_type_ref(returns) {
                return Err(reject("createFunction.returns", returns));
            }
            if let Some(args) = args {
                for arg in args {
                    if !crate::model::ir::is_valid_pg_type_ref(&arg.ty) {
                        return Err(reject("createFunction.args[].type", &arg.ty));
                    }
                }
            }
        }
        crate::model::ir::Op::DropFunction { arg_types: Some(arg_types), .. } => {
            for ty in arg_types {
                if !crate::model::ir::is_valid_pg_type_ref(ty) {
                    return Err(reject("dropFunction.argTypes[]", ty));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn view_body_error(
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    reason: String,
    suggested_fix: &'static str,
) -> AuthoringError {
    AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(suggested_fix.to_string()),
    }
}

/// Validate a raw view body before it is admitted by `ViewQuery::Raw`.
///
/// The raw surface is deliberately narrow: it must be exactly one
/// top-level `SELECT` (no DDL/DML utility statement, no semicolon-chained second
/// statement, no `SELECT INTO`) and then it is fed through the same body
/// reparse/string-literal/token deny-list used for function bodies.
pub(crate) fn validate_raw_view_body_sql(
    sql: &str,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    let parsed = pg_query::parse(sql).map_err(|e| {
        view_body_error(
            target_dialect,
            op_index,
            ts_location,
            format!("raw viewBody SQL must parse as exactly one top-level SELECT: {e}"),
            "rewrite the view body as a single SELECT, or use the structured SelectAst builder",
        )
    })?;
    if parsed.protobuf.stmts.len() != 1 {
        return Err(view_body_error(
            target_dialect,
            op_index,
            ts_location,
            format!(
                "raw viewBody SQL must contain exactly one top-level SELECT statement; parsed {} statements",
                parsed.protobuf.stmts.len()
            ),
            "remove semicolon-chained statements from the view body",
        ));
    }
    let stmt = parsed
        .protobuf
        .stmts
        .first()
        .and_then(|raw| raw.stmt.as_ref())
        .and_then(|stmt| stmt.node.as_ref());
    let Some(NodeEnum::SelectStmt(select)) = stmt else {
        return Err(view_body_error(
            target_dialect,
            op_index,
            ts_location,
            "raw viewBody SQL must be a single top-level SELECT; DDL, DML, COPY, and utility statements are refused".to_string(),
            "rewrite the view body as a SELECT, or use the structured SelectAst builder",
        ));
    };
    if select.into_clause.is_some() {
        return Err(view_body_error(
            target_dialect,
            op_index,
            ts_location,
            "raw viewBody SQL uses SELECT INTO, which creates a table and is not a read-only view body".to_string(),
            "drop the INTO clause; a view body must be read-only",
        ));
    }
    // LAYERING EXCEPTION (A3): keep the deny-list scanner in `guard`; duplicating
    // or moving that security policy into `model` would be the worse boundary.
    crate::guard::check_raw_view_body_text(sql, sql, schema_scope).map_err(|e| {
        view_body_error(
            target_dialect,
            op_index,
            ts_location,
            format!("raw viewBody SQL failed the read-only body scanner: {e}"),
            "remove host/file/network/dynamic-SQL escape tokens from the view body",
        )
    })?;
    Ok(())
}

fn validate_table_ref(
    table: &crate::model::ir::TableRef,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    if let Some(schema) = table.schema.as_deref() {
        if !is_safe_schema_ident(schema) {
            return Err(AuthoringError {
                code: CODE_INVALID_SCHEMA_IDENT.to_string(),
                kind: None,
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: format!(
                    "view SELECT table reference names schema {schema:?}, which is not a safe bare SQL identifier"
                ),
                suggested_fix: Some("use a plain identifier for the table schema".to_string()),
            });
        }
        if let Some(scope) = schema_scope {
            if !scope.permits(schema) {
                return Err(AuthoringError {
                    code: CODE_CROSS_SCHEMA.to_string(),
                    kind: None,
                    op_index,
                    ts_location: ts_location.map(str::to_string),
                    dialect: target_dialect,
                    reason: format!(
                        "view SELECT table reference names schema {schema:?}, which the active schema scope does not permit"
                    ),
                    suggested_fix: Some(
                        "drop the table schema qualifier or use a schema permitted by the active capability scope"
                            .to_string(),
                    ),
                });
            }
        }
    }
    Ok(())
}

fn validate_select_ast(
    select: &crate::model::ir::SelectAst,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::{OrderItem, SelectItem};

    validate_table_ref(&select.from, target_dialect, op_index, ts_location, schema_scope)?;
    let scope = TargetScope::structural_only(&select.from.name);

    for item in &select.projection {
        if let SelectItem::Expr { expr, .. } = item {
            validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
        }
    }
    for join in &select.joins {
        validate_table_ref(&join.table, target_dialect, op_index, ts_location, schema_scope)?;
        validate_expr(&join.on, target_dialect, &scope, op_index, ts_location)?;
    }
    if let Some(pred) = &select.r#where {
        validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
    }
    if let Some(order_by) = &select.order_by {
        for item in order_by {
            if let OrderItem::Expr { expr, .. } = item {
                validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
            }
        }
    }
    Ok(())
}

/// **PR10** — validate an op's `schema` qualifier + existence-guard direction
/// (§2.7), BEFORE the per-op expression-slot checks. Three fail-closed checks:
///
/// 1. **Schema identifier safety** — if a `schema` is present it MUST be a safe bare
///    identifier ([`is_safe_schema_ident`], mirroring `dml.rs`'s `quote_ident`
///    shape); an injection-shaped value is rejected ([`CODE_INVALID_SCHEMA_IDENT`])
///    REGARDLESS of profile (the engine double-quotes it, but a fail-closed
///    validate-time reject is the defense the names-are-strings stance needs).
/// 2. **Cross-schema confinement** — under a `Some(scope)` (Confined/Platform) an
///    explicit `schema` the scope does not `permit` is refused
    ///    ([`CODE_CROSS_SCHEMA`]). Absent schema, or a permitted one, passes.
    ///    `SchemaScope::Unconfined` skips this for the explicit Trusted operator
    ///    profile; `None` means default public validation without vendor capabilities.
/// 3. **Existence-guard direction** — a guard whose direction is illegal for the op
///    variant is refused ([`CODE_GUARD_DIRECTION`]).
fn validate_op_schema_and_guard(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    let mk = |code: &str, reason: String, fix: String| AuthoringError {
        code: code.to_string(),
        kind: None,
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(fix),
    };

    let check_schema = |schema: &str, what: &str| -> Result<(), AuthoringError> {
        if !is_safe_schema_ident(schema) {
            return Err(mk(
                CODE_INVALID_SCHEMA_IDENT,
                format!(
                    "{what} schema qualifier {schema:?} is not a safe bare SQL identifier \
                     (must be non-empty, start with a letter or '_', and contain only \
                     letters, digits, or '_')"
                ),
                "use a plain identifier for the schema, e.g. schema: \"app2\"".to_string(),
            ));
        }
        if let Some(scope) = schema_scope {
            if !scope.permits(schema) {
                let (reason, fix) = match scope {
                    crate::model::policy::SchemaScope::Single(project) => (
                        format!(
                            "this migration is CONFINED to its project schema {project:?}, but \
                             {what} names a different schema {schema:?} — a cross-schema \
                             migration is refused fail-closed (the creator profile pins the \
                             project schema; the migrator role would also reject it, but this \
                             is the earlier, friendlier gate)"
                        ),
                        format!(
                            "drop the schema qualifier (it defaults to {project:?}) or set \
                             schema: {project:?}"
                        ),
                    ),
                    crate::model::policy::SchemaScope::Allowlist(allowed) => (
                        format!(
                            "{what} names schema {schema:?}, which is not in the permitted \
                             platform schema allow-list {allowed:?}"
                        ),
                        format!("name one of the permitted schemas {allowed:?}"),
                    ),
                    crate::model::policy::SchemaScope::Unconfined => (
                        format!(
                            "internal error: unconfined operator scope unexpectedly refused \
                             schema {schema:?}"
                        ),
                        "report this migrate engine bug".to_string(),
                    ),
                };
                return Err(mk(CODE_CROSS_SCHEMA, reason, fix));
            }
        }
        Ok(())
    };

    // (1) + (2) — the top-level schema qualifier.
    if let Some(schema) = op.schema() {
        check_schema(schema, "op")?;
    }

    // Review #5 LOW-6: GRANT/REVOKE table targets carry an inner schema that is
    // not `Op::schema()`. Surface it to the same validate-time allowlist gate so
    // an out-of-scope table grant is refused before lower/render.
    match op {
        crate::model::ir::Op::Grant {
            on: crate::model::ir::GrantTarget::Table { schema: Some(schema), .. },
            ..
        }
        | crate::model::ir::Op::Revoke {
            on: crate::model::ir::GrantTarget::Table { schema: Some(schema), .. },
            ..
        } => {
            check_schema(schema, "grant table target")?;
        }
        _ => {}
    }

    // (3) — the existence-guard direction.
    if let Some(guard) = op.existence_guard() {
        match op.legal_existence_guard() {
            Some(legal) if legal == guard => {}
            Some(_) => {
                let (got, want, family) = match guard {
                    crate::model::ir::ExistenceGuard::IfExists => {
                        ("ifExists", "ifNotExists", "create*/add*")
                    }
                    crate::model::ir::ExistenceGuard::IfNotExists => {
                        ("ifNotExists", "ifExists", "drop*/rename/alter")
                    }
                };
                return Err(mk(
                    CODE_GUARD_DIRECTION,
                    format!(
                        "existence guard {got:?} is not legal on this op (the {family} family \
                         takes {want:?})"
                    ),
                    format!("use {want:?} on this op, or drop the guard"),
                ));
            }
            None => {
                // A DML op carries no guard slot, so `existence_guard()` is `None`
                // there and this arm is unreachable; defensively refuse.
                return Err(mk(
                    CODE_GUARD_DIRECTION,
                    "this op admits no existence guard".to_string(),
                    "remove the existence guard from this op".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn sequence_option_error(
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    reason: String,
    suggested_fix: String,
) -> AuthoringError {
    AuthoringError {
        code: CODE_SEQUENCE_OPTION_INVALID.to_string(),
        kind: None,
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(suggested_fix),
    }
}

fn validate_sequence_numeric_options(
    increment: Option<crate::model::ir::SafeI64>,
    min_value: &Option<Option<crate::model::ir::SafeI64>>,
    max_value: &Option<Option<crate::model::ir::SafeI64>>,
    cache: Option<crate::model::ir::SafeU64>,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    if matches!(increment, Some(n) if n.get() == 0) {
        return Err(sequence_option_error(
            target_dialect,
            op_index,
            ts_location,
            "sequence increment must not be 0".to_string(),
            "use a non-zero sequence increment".to_string(),
        ));
    }
    if matches!(cache, Some(n) if n.get() < 1) {
        return Err(sequence_option_error(
            target_dialect,
            op_index,
            ts_location,
            "sequence cache must be at least 1".to_string(),
            "set cache to 1 or a larger integer".to_string(),
        ));
    }
    if let (Some(Some(min)), Some(Some(max))) = (min_value, max_value) {
        if min.get() > max.get() {
            return Err(sequence_option_error(
                target_dialect,
                op_index,
                ts_location,
                format!(
                    "sequence minValue ({}) must be less than or equal to maxValue ({})",
                    min.get(),
                    max.get()
                ),
                "set minValue <= maxValue, or use null to request the PostgreSQL default bound"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_sequence_options(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::Op;
    match op {
        Op::CreateSequence {
            increment,
            min_value,
            max_value,
            cache,
            ..
        }
        | Op::AlterSequence {
            increment,
            min_value,
            max_value,
            cache,
            ..
        } => validate_sequence_numeric_options(
            *increment,
            min_value,
            max_value,
            *cache,
            target_dialect,
            op_index,
            ts_location,
        ),
        _ => Ok(()),
    }
}

fn unsupported_trigger(
    kind: &'static str,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    reason: String,
    suggested_fix: String,
) -> AuthoringError {
    AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason: format!("{kind}: {reason}"),
        suggested_fix: Some(suggested_fix),
    }
}

fn validate_trigger_dialect(
    events: &[crate::model::ir::TriggerEvent],
    for_each: crate::model::ir::ForEach,
    action: &crate::model::ir::TriggerAction,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    match (target_dialect, action) {
        (Dialect::Postgres, crate::model::ir::TriggerAction::Body { .. }) => {
            return Err(unsupported_trigger(
                "triggerBody",
                target_dialect,
                op_index,
                ts_location,
                "Postgres triggers must execute a named trigger function; the closed inline body form renders only on SQLite".to_string(),
                "use action: { kind: \"executeFunction\", name: \"...\" } and create the trigger function separately".to_string(),
            ));
        }
        (Dialect::Sqlite | Dialect::Mysql, crate::model::ir::TriggerAction::ExecuteFunction { .. }) => {
            let dialect_name = target_dialect.as_str();
            return Err(unsupported_trigger(
                "executeFunction",
                target_dialect,
                op_index,
                ts_location,
                format!("{dialect_name} has no CREATE TRIGGER EXECUTE FUNCTION form"),
                format!("use action: {{ kind: \"body\", statements: [...] }} for {dialect_name} triggers"),
            ));
        }
        _ => {}
    }

    if matches!(target_dialect, Dialect::Sqlite | Dialect::Mysql)
        && events.iter().any(|e| matches!(e, crate::model::ir::TriggerEvent::Truncate))
    {
        let dialect_name = target_dialect.as_str();
        return Err(unsupported_trigger(
            "triggerEventTruncate",
            target_dialect,
            op_index,
            ts_location,
            format!("{dialect_name} has no TRUNCATE trigger event"),
            format!("remove the truncate event for {dialect_name}, or target Postgres for this trigger"),
        ));
    }

    if matches!(target_dialect, Dialect::Mysql) && events.len() > 1 {
        return Err(unsupported_trigger(
            "triggerMultipleEvents",
            target_dialect,
            op_index,
            ts_location,
            "MySQL CREATE TRIGGER accepts exactly one trigger event".to_string(),
            "split this into one trigger per event when targeting MySQL".to_string(),
        ));
    }

    if matches!(target_dialect, Dialect::Sqlite | Dialect::Mysql)
        && matches!(for_each, crate::model::ir::ForEach::Statement)
    {
        let dialect_name = target_dialect.as_str();
        return Err(unsupported_trigger(
            "forEachStatement",
            target_dialect,
            op_index,
            ts_location,
            format!("{dialect_name} triggers are row-level only"),
            format!("use forEach: \"row\" for {dialect_name}, or target Postgres for statement-level triggers"),
        ));
    }

    Ok(())
}

fn validate_trigger_stmt(
    stmt: &crate::model::ir::TriggerStmt,
    outer_table: &str,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::model::policy::SchemaScope>,
) -> Result<(), AuthoringError> {
    let validate_schema = |schema: Option<&str>| -> Result<(), AuthoringError> {
        let Some(schema) = schema else {
            return Ok(());
        };
        if !is_safe_schema_ident(schema) {
            return Err(AuthoringError {
                code: CODE_INVALID_SCHEMA_IDENT.to_string(),
                kind: None,
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: format!(
                    "trigger body statement schema qualifier {schema:?} is not a safe bare SQL identifier"
                ),
                suggested_fix: Some("use a plain schema identifier or omit the nested schema qualifier".to_string()),
            });
        }
        if let Some(scope) = schema_scope {
            if !scope.permits(schema) {
                return Err(AuthoringError {
                    code: CODE_CROSS_SCHEMA.to_string(),
                    kind: None,
                    op_index,
                    ts_location: ts_location.map(str::to_string),
                    dialect: target_dialect,
                    reason: format!(
                        "trigger body statement names schema {schema:?}, which is outside the active schema scope"
                    ),
                    suggested_fix: Some(
                        "omit the nested schema qualifier or use the permitted project schema".to_string(),
                    ),
                });
            }
        }
        Ok(())
    };

    match stmt {
        crate::model::ir::TriggerStmt::Insert { table, rows, schema, .. } => {
            validate_schema(schema.as_deref())?;
            let scope = TargetScope::structural_only(table);
            for row in rows {
                for value in row {
                    if let crate::model::ir::IrValue::Expr(expr) = value {
                        validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                    }
                }
            }
            Ok(())
        }
        crate::model::ir::TriggerStmt::Update { table, set, r#where, schema } => {
            validate_schema(schema.as_deref())?;
            let scope = TargetScope::structural_only(table);
            for value in set.values() {
                if let crate::model::ir::IrValue::Expr(expr) = value {
                    validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
                }
            }
            if let Some(pred) = r#where {
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        crate::model::ir::TriggerStmt::Delete { table, r#where, schema, .. } => {
            validate_schema(schema.as_deref())?;
            let scope = TargetScope::structural_only(table);
            validate_expr(r#where, target_dialect, &scope, op_index, ts_location)
        }
        crate::model::ir::TriggerStmt::Select { expr } => {
            let scope = TargetScope::structural_only(outer_table);
            validate_expr(expr, target_dialect, &scope, op_index, ts_location)
        }
        crate::model::ir::TriggerStmt::Raise { errcode, .. } => {
            if let Some(code) = errcode {
                let valid = code.len() == 5 && code.chars().all(|c| c.is_ascii_alphanumeric());
                if !valid {
                    return Err(AuthoringError {
                        code: CODE_UNSUPPORTED.to_string(),
                        kind: Some(UnsupportedKind::Op),
                        op_index,
                        ts_location: ts_location.map(str::to_string),
                        dialect: target_dialect,
                        reason: format!(
                            "raise errcode {code:?} is not a five-character SQLSTATE token"
                        ),
                        suggested_fix: Some(
                            "use a five-character SQLSTATE such as \"P0001\", or omit errcode"
                                .to_string(),
                        ),
                    });
                }
            }
            Ok(())
        }
    }
}

/// **PR10** — a safe bare SQL identifier for a schema qualifier (§2.7): non-empty,
/// alpha/`_`-leading, all chars `[A-Za-z0-9_]`. Mirrors `dml.rs`'s `quote_ident`
/// shape so the validate-time gate and the emitter's double-quoting agree.
#[must_use]
fn is_safe_schema_ident(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn validate_default_for_type(
    position: &str,
    ty: &crate::model::ir::ColType,
    default: &crate::model::ir::IrDefault,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::{ColType, EmptyContainerKind, IrDefault};

    if let IrDefault::Expr { expr } = default {
        validate_default_expr(position, expr, target_dialect, op_index, ts_location)?;
        return Ok(());
    }

    if let IrDefault::Nextval { .. } = default {
        if !matches!(target_dialect, Dialect::Postgres) {
            return Err(AuthoringError {
                code: CODE_UNSUPPORTED.to_string(),
                kind: Some(UnsupportedKind::Op),
                op_index,
                ts_location: ts_location.map(str::to_string),
                dialect: target_dialect,
                reason: format!(
                    "{position} declares a nextval sequence default, but standalone \
                     sequences and nextval defaults are PostgreSQL-only"
                ),
                suggested_fix: Some(
                    "target PostgreSQL, use an identity/auto-increment shape for this dialect, or remove `.default(nextval(...))`"
                        .to_string(),
                ),
            });
        }
        if matches!(ty, ColType::Int | ColType::BigInt | ColType::SmallInt) {
            return Ok(());
        }
        return Err(AuthoringError {
            code: CODE_COLUMN_DEFAULT_TYPE.to_string(),
            kind: None,
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: format!(
                "{position} declares a nextval sequence default on type {ty:?}; \
                 nextval defaults require an integer column"
            ),
            suggested_fix: Some(
                "use nextval only on int, bigInt, or smallInt columns, or remove `.default(nextval(...))`"
                    .to_string(),
            ),
        });
    }

    if let IrDefault::Json { .. } = default {
        if matches!(ty, ColType::Json) {
            return Ok(());
        }
        return Err(AuthoringError {
            code: CODE_COLUMN_DEFAULT_TYPE.to_string(),
            kind: None,
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: format!(
                "{position} declares a JSON value default on type {ty:?}; \
                 JSON value defaults are valid only on json columns in v1"
            ),
            suggested_fix: Some(
                "use this default only on json columns, or remove the non-empty object/array default"
                    .to_string(),
            ),
        });
    }

    let IrDefault::Container { kind } = default else {
        return Ok(());
    };
    let ok = matches!(
        (kind, ty),
        (EmptyContainerKind::Object, ColType::Json)
            | (EmptyContainerKind::Array, ColType::Json | ColType::TextArray)
    );
    if ok {
        return Ok(());
    }

    let expected = match kind {
        EmptyContainerKind::Object => "json",
        EmptyContainerKind::Array => "json or textArray",
    };
    Err(AuthoringError {
        code: CODE_COLUMN_DEFAULT_TYPE.to_string(),
        kind: None,
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason: format!(
            "{position} declares an empty {kind:?} container default on type {ty:?}; \
             empty object defaults require json, and empty array defaults require \
             json or textArray"
        ),
        suggested_fix: Some(format!(
            "use this default only on {expected} columns, or remove `.default({{}})` / `.default([])`"
        )),
    })
}

fn validate_default_expr(
    position: &str,
    expr: &Expr,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    let scope = TargetScope::structural_only(position);
    validate_expr(expr, target_dialect, &scope, op_index, ts_location)?;
    validate_no_aggregate_expr_context(expr, position, target_dialect, op_index, ts_location)?;

    fn mk_err(
        reason: String,
        target_dialect: Dialect,
        op_index: usize,
        ts_location: Option<&str>,
    ) -> AuthoringError {
        AuthoringError {
            code: CODE_OP_INVALID.to_string(),
            kind: Some(UnsupportedKind::Expr),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason,
            suggested_fix: Some(
                "use only literals, CASE, immutable scalar helpers, and now()/genRandomUuid() in column defaults"
                    .to_string(),
            ),
        }
    }

    fn walk(
        expr: &Expr,
        target_dialect: Dialect,
        op_index: usize,
        ts_location: Option<&str>,
    ) -> Result<(), AuthoringError> {
        match expr {
            Expr::ColRef { .. } => Err(mk_err(
                "a column default cannot reference a column".to_string(),
                target_dialect,
                op_index,
                ts_location,
            )),
            Expr::Agg { .. } => Err(mk_err(
                "a column default cannot use an aggregate".to_string(),
                target_dialect,
                op_index,
                ts_location,
            )),
            Expr::FnCall { r#fn, args } => {
                if matches!(r#fn, ScalarFn::CurrentSetting | ScalarFn::CurrentUser) {
                    return Err(mk_err(
                        "a column default cannot use volatile or vendor-only functions".to_string(),
                        target_dialect,
                        op_index,
                        ts_location,
                    ));
                }
                for arg in args {
                    walk(arg, target_dialect, op_index, ts_location)?;
                }
                Ok(())
            }
            Expr::PgRegexMatch { .. }
            | Expr::PgColumnSize { .. }
            | Expr::PgExtract { .. }
            | Expr::PgInterval { .. }
            | Expr::Dialectal { .. } => Err(mk_err(
                "a column default cannot use volatile, dialect-specific, or vendor-only expression nodes"
                    .to_string(),
                target_dialect,
                op_index,
                ts_location,
            )),
            Expr::Extract { .. } => Err(mk_err(
                "a column default cannot use an EXTRACT expression".to_string(),
                target_dialect,
                op_index,
                ts_location,
            )),
            Expr::Literal { .. } => Ok(()),
            Expr::BinOp { lhs, rhs, .. } => {
                walk(lhs, target_dialect, op_index, ts_location)?;
                walk(rhs, target_dialect, op_index, ts_location)
            }
            Expr::UnaryOp { operand, .. } => walk(operand, target_dialect, op_index, ts_location),
            Expr::Case { branches, r#else } => {
                for CaseBranch { when, then } in branches {
                    walk(when, target_dialect, op_index, ts_location)?;
                    walk(then, target_dialect, op_index, ts_location)?;
                }
                if let Some(expr) = r#else {
                    walk(expr, target_dialect, op_index, ts_location)?;
                }
                Ok(())
            }
            Expr::FnSynth { args, .. } => {
                for arg in args {
                    walk(arg, target_dialect, op_index, ts_location)?;
                }
                Ok(())
            }
            Expr::Cast { operand, .. } => walk(operand, target_dialect, op_index, ts_location),
            Expr::Between { operand, low, high } => {
                walk(operand, target_dialect, op_index, ts_location)?;
                walk(low, target_dialect, op_index, ts_location)?;
                walk(high, target_dialect, op_index, ts_location)
            }
            Expr::Like { operand, pattern } => {
                walk(operand, target_dialect, op_index, ts_location)?;
                walk(pattern, target_dialect, op_index, ts_location)
            }
            Expr::DistinctFrom { left, right } => {
                walk(left, target_dialect, op_index, ts_location)?;
                walk(right, target_dialect, op_index, ts_location)
            }
            Expr::InList { expr, .. } => walk(expr, target_dialect, op_index, ts_location),
        }
    }

    walk(expr, target_dialect, op_index, ts_location)
}

/// **Migration-first P2a (§4)** — validate one [`IrColumn`](crate::model::ir::IrColumn)'s
/// declared-only facets (`id_prefix` / `vector_metric`) against their bounds.
///
/// Two fail-closed checks, with the IR's hand-crafted-`.ir.json` threat model in
/// mind (the closed-enum + `deny_unknown_fields` design):
///
/// 1. **`id_prefix`** — must be a valid typed-id prefix: the SAME `^[a-z][a-z0-9_]*$`
///    charset rule + reserved-prefix deny-list (`usr`, …) the runtime enforces via
///    [`zeroship_schema::query::validate_id_prefix`] (the SINGLE source of truth,
///    mirroring `crates/core/src/typed_id.rs` + `system_fields_pass`'s
///    `RESERVED_AUTO_PREFIXES`), PLUS a [`MAX_ID_PREFIX_LEN`] length bound so a
///    hand-authored prefix keeps the compact `<prefix>_<22 base62>` typed-id shape.
///    A reserved/malformed/over-long prefix is [`CODE_INVALID_ID_PREFIX`], refused
///    BEFORE lower — never a render-time surprise minting colliding `usr_…` ids.
/// 2. **`vector_metric`** — structurally bounded by the closed
///    [`crate::model::ir::VectorMetric`] enum at deserialize; the only authoring error
///    left is CO-OCCURRENCE: a metric carried on a non-`Vector` column is
///    meaningless (the opclass has no vector to apply to) and is refused
///    ([`CODE_VECTOR_METRIC_MISPLACED`]) so a hand-crafted artifact cannot ride a
///    dead field in.
///
/// # Errors
/// [`CODE_INVALID_ID_PREFIX`] / [`CODE_VECTOR_METRIC_MISPLACED`] as above.
fn validate_column_facets(
    col: &crate::model::ir::IrColumn,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    validate_col_type_position(
        &col.ty,
        "column.type",
        false,
        target_dialect,
        op_index,
        ts_location,
    )?;

    let mk = |code: &str, reason: String, fix: String| AuthoringError {
        code: code.to_string(),
        kind: None,
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(fix),
    };
    let unsupported = |kind: UnsupportedKind, reason: String, fix: String| AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(kind),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(fix),
    };

    if col.generated.is_some() && col.default.is_some() {
        return Err(mk(
            CODE_COLUMN_FACET_CONFLICT,
            format!(
                "column {:?} is generated and also declares a default; generated columns \
                 cannot have DEFAULT values",
                col.name
            ),
            "remove either `.generated(...)` or `.default(...)` from the column".to_string(),
        ));
    }
    if col.identity.is_some() && col.default.is_some() {
        return Err(mk(
            CODE_COLUMN_FACET_CONFLICT,
            format!(
                "column {:?} is an identity column and also declares a default; identity \
                 columns cannot have DEFAULT values",
                col.name
            ),
            "remove either `.identity(...)` or `.default(...)` from the column".to_string(),
        ));
    }
    if col.identity.is_some() && col.generated.is_some() {
        return Err(mk(
            CODE_COLUMN_FACET_CONFLICT,
            format!(
                "column {:?} declares both identity and generated facets; SQL identity \
                 and generated/computed columns are mutually exclusive",
                col.name
            ),
            "remove either `.identity(...)` or `.generated(...)` from the column".to_string(),
        ));
    }

    if let Some(default) = &col.default {
        validate_default_for_type(
            &format!("column {:?}.default", col.name),
            &col.ty,
            default,
            target_dialect,
            op_index,
            ts_location,
        )?;
    }

    if matches!(target_dialect, Dialect::Postgres)
        && matches!(col.generated.as_ref(), Some(generated) if !generated.stored)
    {
        return Err(unsupported(
            UnsupportedKind::VirtualColumn,
            format!(
                "column {:?} requests a VIRTUAL generated column, but Postgres supports \
                 generated columns only as STORED",
                col.name
            ),
            "use `.generated(expr)` / `{ virtual: false }` for Postgres, or target SQLite"
                .to_string(),
        ));
    }

    if col.identity.is_some()
        && !matches!(
            col.ty,
            crate::model::ir::ColType::SmallInt
                | crate::model::ir::ColType::Int
                | crate::model::ir::ColType::BigInt
        )
    {
        return Err(unsupported(
            UnsupportedKind::Identity,
            format!(
                "column {:?} declares identity on a non-integer type; identity is only \
                 supported on smallInt/int/bigInt columns",
                col.name
            ),
            "declare the column as `t.smallInt().identity(...)`, `t.int().identity(...)`, \
             or `t.bigInt().identity(...)`"
                .to_string(),
        ));
    }

    if let Some(prefix) = &col.id_prefix {
        // Charset + reserved deny-list — the runtime's single source of truth.
        if let Err(e) = zeroship_schema::query::validate_id_prefix(prefix) {
            return Err(mk(
                CODE_INVALID_ID_PREFIX,
                format!(
                    "column {:?} declares an invalid t.id() prefix {prefix:?}: {e}",
                    col.name
                ),
                "use a prefix matching ^[a-z][a-z0-9_]*$ that is not platform-reserved \
                 (e.g. \"post\", \"org\")"
                    .to_string(),
            ));
        }
        // Length bound — keep the compact typed-id shape (charset already checked).
        if prefix.len() > MAX_ID_PREFIX_LEN {
            return Err(mk(
                CODE_INVALID_ID_PREFIX,
                format!(
                    "column {:?} declares a t.id() prefix {prefix:?} of {} bytes; the \
                     maximum is {MAX_ID_PREFIX_LEN} (a typed-id prefix is kept short so \
                     the minted `<prefix>_<22 base62>` id stays compact)",
                    col.name,
                    prefix.len()
                ),
                format!("shorten the prefix to at most {MAX_ID_PREFIX_LEN} characters"),
            ));
        }
    }

    if col.vector_metric.is_some() && !matches!(col.ty, crate::model::ir::ColType::Vector { .. }) {
        return Err(mk(
            CODE_VECTOR_METRIC_MISPLACED,
            format!(
                "column {:?} carries a vector_metric but is not a vector column; a \
                 distance metric only applies to a t.vector(n) column",
                col.name
            ),
            "drop the metric, or declare the column as t.vector(n, { metric })".to_string(),
        ));
    }

    if matches!(col.case_sensitive, Some(false))
        && !matches!(col.ty, crate::model::ir::ColType::Text)
    {
        return Err(mk(
            CODE_UNSUPPORTED,
            format!(
                "column {:?} declares caseSensitive:false but is not a text column; \
                 caseSensitive:false is only valid on a text column",
                col.name
            ),
            "drop the caseSensitive facet, or declare the column as t.text({ caseSensitive: false })"
                .to_string(),
        ));
    }

    Ok(())
}

fn validate_col_type_position(
    ty: &crate::model::ir::ColType,
    position: &'static str,
    allow_pg_domain_date_base: bool,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::ColType;

    fn contains_date(ty: &ColType) -> bool {
        match ty {
            ColType::Date => true,
            ColType::Encrypted { of } => contains_date(of),
            _ => false,
        }
    }

    if matches!(ty, ColType::Char { length: 0 }) {
        return Err(AuthoringError {
            code: CODE_UNSUPPORTED.to_string(),
            kind: Some(UnsupportedKind::Op),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: format!("{position} uses `char(0)`; fixed-length char requires a positive length"),
            suggested_fix: Some("use `t.char(1)` or larger".to_string()),
        });
    }

    if !contains_date(ty) {
        return Ok(());
    }
    if allow_pg_domain_date_base && target_dialect == Dialect::Postgres && matches!(ty, ColType::Date)
    {
        return Ok(());
    }
    Err(AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(UnsupportedKind::Op),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason: format!(
            "{position} uses the narrow `date` type token, which Slice C admits only \
             as a PostgreSQL createDomain base type"
        ),
        suggested_fix: Some(
            "use `timestamp` for ordinary columns, or use `date` only as \
             domain(name).create({ as: \"date\", ... }) on PostgreSQL"
                .to_string(),
        ),
    })
}

fn validate_identity_placement(
    col: &crate::model::ir::IrColumn,
    target_dialect: Dialect,
    pk_cols: Option<&[String]>,
    is_add_column: bool,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    let Some(identity) = col.identity else {
        return Ok(());
    };
    if !matches!(target_dialect, Dialect::Sqlite | Dialect::Mysql) {
        return Ok(());
    }
    let err = |reason: String| AuthoringError {
        code: CODE_UNSUPPORTED.to_string(),
        kind: Some(UnsupportedKind::Identity),
        op_index,
        ts_location: ts_location.map(str::to_string),
        dialect: target_dialect,
        reason,
        suggested_fix: Some(
            "use identity only on the sole integer primary key for this dialect, or remove \
             `.identity(...)`"
                .to_string(),
        ),
    };
    if identity.always {
        return Err(err(
            "identity({ always: true }) is PostgreSQL-only; SQLite/MySQL support \
             only identity({ always: false }) / autoIncrement() on the sole integer \
             primary key"
                .to_string(),
        ));
    }
    if is_add_column {
        return Err(err(
            "autoIncrement identity: non-PK identity has no sound target-dialect \
             render; SQLite AUTOINCREMENT and MySQL AUTO_INCREMENT are only sound \
             on the sole integer primary key"
                .to_string(),
        ));
    }
    let Some(pk_cols) = pk_cols else {
        return Err(err(format!(
            "autoIncrement identity: column {:?} is not the declared primary key; \
             non-PK identity has no sound target-dialect render",
            col.name
        )));
    };
    if pk_cols.len() == 1 && pk_cols[0] == col.name {
        return Ok(());
    }
    Err(err(format!(
        "autoIncrement identity: column {:?} is part of {:?}, but this dialect's \
         identity is only sound for the sole integer primary key",
        col.name, pk_cols
    )))
}

/// **Apply/render-seam ColRef resolution (rule (c), MED).** Re-run the
/// expression-AST walk for the ops whose live-schema column set was NOT known at
/// IR-load time — the DML ops (`update`/`delete`/`backfill`) and `setColumnType`
/// — now that the render/apply seam HAS the live columns. For each such op whose
/// target table appears in `live_columns`, the embedded predicates / set RHS /
/// cast are re-validated with a **RESOLVING** [`TargetScope`], so an unresolved
/// `ColRef` is rejected with the structured [`AuthoringError`] (rule (c)) at apply
/// — NOT as an opaque raw DB error mid-statement.
///
/// `live_columns` maps a target table → its live column names (system fields
/// included). An op whose table is absent from the map keeps the structural-only
/// scope (the (c) check is skipped — the caller could not resolve that table).
/// Non-DML / non-`setColumnType` ops are revalidated structurally (a),(b),(d)
/// — harmless and keeps the walk total.
///
/// This is the seam the design (`validate_ir` doc, "the apply/render seam re-runs
/// the walk with a resolved column set to enforce (c)") names. In PR1 the DML /
/// `setColumnType` LOWER is still deferred (`IrAuthor::lower` returns
/// `UnsupportedOp`), so this resolution is exercised as a stand-alone seam +
/// regression; once DML lowering lands (PR6a) the apply path calls this BEFORE
/// rendering the DML statement.
///
/// # Errors
/// The first [`AuthoringError`] any embedded expression produces — incl. a rule
/// (c) `ColRef`-resolution failure now that the column set is known.
pub fn validate_ir_resolved(
    ir: &crate::model::ir::MigrationIr,
    target_dialect: Dialect,
    live_columns: &std::collections::BTreeMap<String, Vec<String>>,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    for (op_index, op) in ir.ops.iter().enumerate() {
        let ts = ts_locations.get(op_index).and_then(Option::as_deref);
        validate_op_resolved(op, target_dialect, live_columns, op_index, ts)?;
    }
    validate_partition_recording(ir, target_dialect, ts_locations)?;
    Ok(())
}

/// **Single-op apply/render-seam ColRef resolution (rule (c), MED).** The per-op
/// peer of [`validate_ir_resolved`]: re-run the expression-AST walk for ONE op with
/// a RESOLVING [`TargetScope`] when its target table's live column set is known.
///
/// This is the seam the DML LOWER calls ([`crate::render::lower::IrAuthor::lower_dml_op`]):
/// at lower/apply the live schema HAS been introspected, so each DML op
/// (`update`/`delete`/`backfill`) / `setColumnType` resolves its embedded
/// `ColRef`s against the live target-table columns BEFORE the SQL template is
/// assembled. A `ColRef` to a column NOT on the enclosing target table (or a
/// synthesized cross-table reference) is rejected with the structured
/// [`AuthoringError`] (`UNSUPPORTED { kind: "expr" }`, rule (c)) at apply — NOT as
/// an opaque raw DB `column does not exist` error mid-statement (§3.3.1.1(c) "at
/// apply/render time").
///
/// `live_columns` maps a target table → its live column names (system fields
/// included). An op whose table is ABSENT from the map keeps the structural-only
/// scope (the (c) check is skipped — the caller could not resolve that table; the
/// (a)/(b)/(d) structural checks still run). A non-DML / non-`setColumnType` op
/// re-runs the structural [`validate_op`] (harmless; keeps the walk total).
///
/// # Errors
/// The first [`AuthoringError`] the op's embedded expressions produce — incl. a
/// rule (c) `ColRef`-resolution failure now that the column set is known.
pub fn validate_op_resolved(
    op: &crate::model::ir::Op,
    target_dialect: Dialect,
    live_columns: &std::collections::BTreeMap<String, Vec<String>>,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::model::ir::Op;
    let ts = ts_location;
    validate_op_support(op, target_dialect, op_index, ts)?;
    // The op's target table (for the DML / setColumnType ops we resolve).
    let resolved_scope = |table: &str| -> Option<Vec<String>> { live_columns.get(table).cloned() };
    match op {
        Op::Update { table, set, r#where, .. } => {
            if let Some(cols) = resolved_scope(table) {
                let scope = TargetScope::new(table, &cols);
                for value in set.values() {
                    if let crate::model::ir::IrValue::Expr(expr) = value {
                        validate_expr(expr, target_dialect, &scope, op_index, ts)?;
                    }
                }
                if let Some(pred) = r#where {
                    validate_expr(pred, target_dialect, &scope, op_index, ts)?;
                }
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        Op::Delete { table, r#where, .. } => {
            if let Some(cols) = resolved_scope(table) {
                let scope = TargetScope::new(table, &cols);
                validate_expr(r#where, target_dialect, &scope, op_index, ts)?;
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        Op::Backfill { table, set, filter, .. } => {
            if let Some(cols) = resolved_scope(table) {
                let scope = TargetScope::new(table, &cols);
                for value in set.values() {
                    if let crate::model::ir::IrValue::Expr(expr) = value {
                        validate_expr(expr, target_dialect, &scope, op_index, ts)?;
                    }
                }
                if let Some(pred) = filter {
                    validate_expr(pred, target_dialect, &scope, op_index, ts)?;
                }
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        Op::SetColumnType { table, to_type, using, .. } => {
            validate_col_type_position(
                to_type,
                "setColumnType.toType",
                false,
                target_dialect,
                op_index,
                ts,
            )?;
            if let (Some(cols), Some(cast)) = (resolved_scope(table), using) {
                let scope = TargetScope::new(table, &cols);
                validate_expr(cast, target_dialect, &scope, op_index, ts)?;
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        // SA-18: insert row cells and `on_conflict.do_update` values can carry a
        // closed Expr (a DB-evaluated synth scalar or `DO UPDATE SET n = n + 1`).
        // When the target table resolves, walk every `IrValue::Expr` through a real
        // resolving `TargetScope` so a ColRef to a non-existent column is rejected
        // here, not as an opaque mid-statement DB error — symmetric with the
        // Update/Delete/Backfill/SetColumnType arms above.
        Op::Insert { table, rows, on_conflict, .. } => {
            if let Some(cols) = resolved_scope(table) {
                let scope = TargetScope::new(table, &cols);
                for row in rows {
                    for cell in row {
                        if let crate::model::ir::IrValue::Expr(e) = cell {
                            validate_expr(e, target_dialect, &scope, op_index, ts)?;
                        }
                    }
                }
                if let Some(do_update) = on_conflict.as_ref().and_then(|oc| oc.do_update.as_ref()) {
                    for v in do_update.values() {
                        if let crate::model::ir::IrValue::Expr(e) = v {
                            validate_expr(e, target_dialect, &scope, op_index, ts)?;
                        }
                    }
                }
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        // Every other op: revalidate structurally (its own scope is already
        // resolved or has no Expr slot).
        other => validate_op(other, target_dialect, op_index, ts)?,
    }
    Ok(())
}

struct Ctx<'a> {
    target_dialect: Dialect,
    scope: &'a TargetScope<'a>,
    op_index: usize,
    ts_location: Option<&'a str>,
}

impl Ctx<'_> {
    fn err(
        &self,
        code: &str,
        kind: Option<UnsupportedKind>,
        dialect: Dialect,
        reason: String,
        suggested_fix: Option<String>,
    ) -> AuthoringError {
        AuthoringError {
            code: code.to_string(),
            kind,
            op_index: self.op_index,
            ts_location: self.ts_location.map(str::to_string),
            dialect,
            reason,
            suggested_fix,
        }
    }

    /// The maximum expression-AST nesting [`Ctx::walk`] will descend before
    /// refusing the tree as a [`CODE_UNSUPPORTED`] DoS guard. The bound is OWNED
    /// by the validator (an explicit counter, below) rather than left implicit to
    /// `serde_json`'s compile-time `recursion_limit` (~128) at deserialize: a
    /// future switch to a streaming/custom deserializer, or a raised serde limit,
    /// would otherwise silently expose a stack-overflow on a deeply-nested hostile
    /// `.ir.json`. `128` is comfortably below any realistic legitimate nesting and
    /// matches serde's own default so it never narrows the accepted set in
    /// practice. If a deserializer ever admits deeper trees, THIS bound — not an
    /// upstream default — is what protects the recursive walk.
    const MAX_EXPR_DEPTH: u32 = 128;

    fn walk(&self, expr: &Expr) -> Result<(), AuthoringError> {
        self.walk_depth(expr, 0)
    }

    fn walk_depth(&self, expr: &Expr, depth: u32) -> Result<(), AuthoringError> {
        if depth >= Self::MAX_EXPR_DEPTH {
            return Err(self.err(
                CODE_UNSUPPORTED,
                Some(UnsupportedKind::Expr),
                self.target_dialect,
                format!(
                    "expression nesting exceeds the maximum supported depth ({}); \
                     flatten the expression",
                    Self::MAX_EXPR_DEPTH
                ),
                Some("reduce the nesting depth of this expression".to_string()),
            ));
        }
        let d = depth + 1;
        match expr {
            // Unqualified ref: resolve against the enclosing single target table
            // (rule (c)). Qualified ref (`c("t","col")`, §3.4): the full
            // "qualified-ref table must be in the FROM set" scope check
            // (`QUALIFIED_REF_UNKNOWN_TABLE`) is coupled with the Phase-2 view/FROM
            // builder; for this additive slice accept the qualified form
            // structurally (lenient pass — see design §3.4).
            Expr::ColRef { name, table } => match table {
                Some(_) => Ok(()),
                None => self.check_colref(name),
            },
            Expr::Literal { .. } => Ok(()),
            Expr::BinOp { lhs, rhs, .. } => {
                self.walk_depth(lhs, d)?;
                self.walk_depth(rhs, d)
            }
            Expr::UnaryOp { operand, .. } => self.walk_depth(operand, d),
            Expr::Case { branches, r#else } => {
                for CaseBranch { when, then } in branches {
                    self.walk_depth(when, d)?;
                    self.walk_depth(then, d)?;
                }
                if let Some(e) = r#else {
                    self.walk_depth(e, d)?;
                }
                Ok(())
            }
            // FnCall is an allow-listed scalar by the closed ScalarFn enum, but
            // two members are PG-only VENDOR scalars (vendor spec §2.10):
            // `current_setting` / `current_user` render as PG built-ins with no
            // faithful SQLite/MySQL form, so they must be gated off the portable
            // core exactly like the other PG-only expr nodes below — otherwise a
            // portable op carrying them validates clean and breaks at apply.
            Expr::FnCall { r#fn, args } => {
                if matches!(r#fn, ScalarFn::CurrentSetting | ScalarFn::CurrentUser) {
                    self.check_pg_only_expr("current_setting / current_user")?;
                }
                for a in args {
                    self.walk_depth(a, d)?;
                }
                Ok(())
            }
            Expr::FnSynth { r#fn, args } => self.check_synth(*r#fn, args, d),
            // Cast target is portable by the closed CastTarget enum (rule d);
            // recurse into the operand.
            Expr::Cast { operand, .. } => self.walk_depth(operand, d),
            // Portable predicate nodes (§3.4): between/like/distinctFrom render on
            // ALL three dialects (the engine owns distinctFrom's per-dialect
            // lowering), so there is NO dialect gate — just recurse structurally.
            Expr::Between { operand, low, high } => {
                self.walk_depth(operand, d)?;
                self.walk_depth(low, d)?;
                self.walk_depth(high, d)
            }
            Expr::Like { operand, pattern } => {
                self.walk_depth(operand, d)?;
                self.walk_depth(pattern, d)
            }
            Expr::DistinctFrom { left, right } => {
                self.walk_depth(left, d)?;
                self.walk_depth(right, d)
            }
            // Portable aggregate node (§3.4/§3.6): count/sum/avg/min/max render
            // identically on all three dialects, so there is NO dialect gate. The
            // "aggregate only valid in a grouped/SELECT context" check
            // (`AGG_POSITION_INVALID`) is coupled with the Phase-2 view/select
            // builder — this additive slice accepts the node STRUCTURALLY, just
            // recursing into the optional argument (`count(*)` has none).
            Expr::Agg { func: _, arg, distinct: _ } => match arg {
                Some(e) => self.walk_depth(e, d),
                None => Ok(()),
            },
            Expr::InList { expr, elems, negated: _ } => self.check_in_list(expr, elems, d),
            Expr::PgRegexMatch { expr, pattern } => self.check_pg_regex_match(expr, pattern, d),
            Expr::PgColumnSize { expr } => {
                self.check_pg_only_expr("pg_column_size")?;
                self.walk_depth(expr, d)
            }
            Expr::Extract { field: _, from } => self.walk_depth(from, d),
            Expr::PgExtract { field: _, from } => {
                self.check_pg_only_expr("PG EXTRACT")?;
                self.walk_depth(from, d)
            }
            Expr::PgInterval { duration } => {
                self.check_pg_only_expr("PG interval literal")?;
                self.check_duration(duration)
            }
            // The one Layer-2 portability escape (§3.4): a per-dialect value
            // divergence. Structurally validate EVERY present leg (dialect-
            // neutral), then apply the per-TARGET scope math (own leg OR default).
            Expr::Dialectal { default, pg, sqlite, mysql } => {
                self.check_dialectal(default, pg, sqlite, mysql, d)
            }
        }
    }

    /// Validate an [`Expr::Dialectal`] — the `dialect({ default?, pg?, sqlite?,
    /// mysql? })` Layer-2 escape (design §3.4). Three checks, in order:
    ///
    /// 1. **At least one leg** — a legless `dialect({})` is malformed on EVERY
    ///    target (dialect-neutral [`CODE_UNSUPPORTED`]).
    /// 2. **Recurse into every present leg** structurally, regardless of the
    ///    target dialect — an unresolved `ColRef` / malformed nested node in ANY
    ///    leg must reject (dialect-neutral, mirroring `check_synth`). Runs before
    ///    the scope check so a precise per-node error surfaces rather than being
    ///    masked by the coverage refusal.
    /// 3. **Scope math, per-TARGET** — the target must be covered by either its
    ///    OWN leg or a `default`; else refuse fail-closed with
    ///    [`CODE_EXPR_NOT_PORTABLE`]. This is per-target: a `dialect()` missing
    ///    the sqlite leg (no default) is fine targeting PG, refused targeting
    ///    SQLite/MySQL.
    ///
    /// RATCHET (P11 / §3.4): each leg is one of the four ratcheted budget
    /// counters. The budget mechanism is a later phase (not yet built); the
    /// per-leg count is wired in when it lands. Deferred — not gated here.
    fn check_dialectal(
        &self,
        default: &Option<Box<Expr>>,
        pg: &Option<Box<Expr>>,
        sqlite: &Option<Box<Expr>>,
        mysql: &Option<Box<Expr>>,
        depth: u32,
    ) -> Result<(), AuthoringError> {
        // (1) at least one leg.
        if default.is_none() && pg.is_none() && sqlite.is_none() && mysql.is_none() {
            return Err(self.err(
                CODE_UNSUPPORTED,
                Some(UnsupportedKind::Expr),
                self.target_dialect,
                "dialect({}) carries no legs; a per-dialect value escape must \
                 provide at least one of default/pg/sqlite/mysql"
                    .to_string(),
                Some("supply at least one dialect leg (or a default)".to_string()),
            ));
        }
        // (2) recurse into EVERY present leg. The structural checks remain the
        // same, but PG-only portability gates must be judged against the leg that
        // could render them: pg as PG, sqlite as SQLite, mysql as MySQL. The
        // default leg is required to be portable because it may cover any target.
        if let Some(leg) = default {
            self.walk_depth_portable_default(leg, depth)?;
        }
        if let Some(leg) = pg {
            self.walk_depth_as(Dialect::Postgres, leg, depth)?;
        }
        if let Some(leg) = sqlite {
            self.walk_depth_as(Dialect::Sqlite, leg, depth)?;
        }
        if let Some(leg) = mysql {
            self.walk_depth_as(Dialect::Mysql, leg, depth)?;
        }
        // (3) SCOPE MATH, per-TARGET: own leg OR default covers this dialect.
        let own_present = match self.target_dialect {
            Dialect::Postgres => pg.is_some(),
            Dialect::Sqlite => sqlite.is_some(),
            Dialect::Mysql => mysql.is_some(),
        };
        if own_present || default.is_some() {
            return Ok(());
        }
        Err(self.err(
            CODE_EXPR_NOT_PORTABLE,
            Some(UnsupportedKind::Expr),
            self.target_dialect,
            format!(
                "dialect() has no leg for the {} target and no default leg; the \
                 per-dialect divergence does not cover this dialect",
                self.target_dialect.as_str()
            ),
            Some(format!(
                "add a {} leg or a default leg to the dialect() escape",
                self.target_dialect.as_str()
            )),
        ))
    }

    fn with_target_dialect(&self, target_dialect: Dialect) -> Ctx<'_> {
        Ctx {
            target_dialect,
            scope: self.scope,
            op_index: self.op_index,
            ts_location: self.ts_location,
        }
    }

    fn walk_depth_as(
        &self,
        target_dialect: Dialect,
        expr: &Expr,
        depth: u32,
    ) -> Result<(), AuthoringError> {
        self.with_target_dialect(target_dialect).walk_depth(expr, depth)
    }

    fn walk_depth_portable_default(
        &self,
        expr: &Expr,
        depth: u32,
    ) -> Result<(), AuthoringError> {
        for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            self.walk_depth_as(dialect, expr, depth)?;
        }
        Ok(())
    }

    /// Rule (c): a `ColRef` must resolve to a column on the enclosing target
    /// table. Cross-table is impossible by construction (no `c("t.col")`), so
    /// the only failure is a name not on the target table.
    fn check_colref(&self, name: &str) -> Result<(), AuthoringError> {
        let Some(cols) = self.scope.columns else {
            return Ok(()); // structural-only mode: skip resolution
        };
        if cols.iter().any(|c| c == name) {
            return Ok(());
        }
        Err(self.err(
            CODE_UNSUPPORTED,
            Some(UnsupportedKind::Expr),
            self.target_dialect,
            format!(
                "column {name:?} does not resolve on the enclosing target table {:?}; \
                 a transform may only reference columns of its own target table \
                 (cross-table references are not expressible)",
                self.scope.table
            ),
            Some(format!(
                "reference a column that exists on {:?}, or split the migration so \
                 the value comes from this table",
                self.scope.table
            )),
        ))
    }

    /// Rule (b): the `FnSynth` arity/shape backstop. Each synth helper has a
    /// pinned argument shape; an out-of-shape call is rejected STRUCTURALLY here
    /// — independent of the (per-dialect) render seam — so a hostile/buggy
    /// `.ir.json` carrying e.g. `FnSynth{fn:now, args:[…]}` or a zero-arg
    /// `concatWs` cannot pass the structural gate and defer the blow-up to
    /// rendering. After the shape check each variant recurses into its args.
    fn check_synth(&self, f: SynthFn, args: &[Expr], depth: u32) -> Result<(), AuthoringError> {
        match f {
            SynthFn::SplitPart => {
                // (1) ARITY first — the grammar/envelope checks index args[1]/args[2].
                //     Wrong arity is malformed on BOTH dialects → CODE_UNSUPPORTED.
                if args.len() != 3 {
                    return Err(self.malformed_synth_err(format!(
                        "c.fn.splitPart takes exactly (column, delim, n); got {} args",
                        args.len()
                    )));
                }
                // (2) Structurally walk EVERY arg FIRST, regardless of target_dialect:
                //     rule (c) `ColRef` resolution + the nested-synth backstop are
                //     dialect-NEUTRAL and must cover all three slots on BOTH dialects.
                //     This runs BEFORE the grammar check so an UNRESOLVED ColRef / a
                //     malformed nested synth in the delim/n slot surfaces as the precise
                //     CODE_UNSUPPORTED (item-4), rather than being masked by the
                //     grammar's CODE_EXPR_NOT_PORTABLE. (A ColRef to an EXISTENT column
                //     resolves here, then the grammar check below rejects it as a
                //     non-literal delim/n.)
                for a in args {
                    self.walk_depth(a, depth)?;
                }
                // (3) GRAMMAR (dialect-neutral) + ENVELOPE (SQLite-only).
                self.check_split_part(args)?;
                Ok(())
            }
            // now()/gen_random_uuid() are NULLARY apply-time scalars: no args.
            // A non-nullary call is genuinely MALFORMED — `now()`/`gen_random_uuid()`
            // are nullary on BOTH dialects — so it is an unconditional
            // CODE_UNSUPPORTED, never a dialect-gated portability reject (MED-1).
            SynthFn::Now | SynthFn::GenRandomUuid => {
                if !args.is_empty() {
                    return Err(self.malformed_synth_err(format!(
                        "c.fn.{} takes no arguments; got {}",
                        match f {
                            SynthFn::Now => "now",
                            SynthFn::GenRandomUuid => "genRandomUuid",
                            _ => unreachable!(),
                        },
                        args.len()
                    )));
                }
                Ok(())
            }
            // concatWs(delim, value, …): a delimiter + at least one value. Fewer
            // than two args is a genuinely-malformed join on EITHER dialect →
            // unconditional CODE_UNSUPPORTED (MED-1).
            SynthFn::ConcatWs => {
                if args.len() < 2 {
                    return Err(self.malformed_synth_err(format!(
                        "c.fn.concatWs takes a delimiter plus at least one value \
                         (>=2 args); got {}",
                        args.len()
                    )));
                }
                // The SQLite render lowers concatWs to a NULL-skipping `||`-fold with
                // a `substr(fold, length(delim)+1)` head-trim that strips the leading
                // delimiter (`render_concat_ws`). That head-trim is only correct when
                // the delimiter is a FIXED Literal; a computed/runtime delimiter (a
                // ColRef, a nested synth, …) would make the prefix length unknowable
                // and silently corrupt the result. PG's `concat_ws` takes any
                // expression delimiter, so — exactly like the splitPart delim gate —
                // a non-literal delimiter loads on PG and is a HARD reject only on a
                // SQLite target. Mirror the splitPart structural gate so a hand-crafted
                // IR cannot slip a non-literal delimiter past and defer the corruption
                // to render.
                if self.target_dialect == Dialect::Sqlite
                    && !matches!(args[0], Expr::Literal { .. })
                {
                    return Err(self.concat_ws_delim_envelope_err(format!(
                        "c.fn.concatWs delimiter must be a literal on SQLite (a \
                         runtime/computed delimiter is not portable — the NULL-skip \
                         head-trim needs a fixed delimiter length); got {:?}",
                        args[0]
                    )));
                }
                for a in args {
                    self.walk_depth(a, depth)?;
                }
                Ok(())
            }
        }
    }

    /// A genuinely-MALFORMED synth-helper call — a shape broken on BOTH dialects
    /// (`now(arg)`, `genRandomUuid(args)`, `concatWs` with <2 args, `splitPart`
    /// with the wrong arity). This is NOT a portability boundary: there is no
    /// dialect on which it renders, so it is an unconditional
    /// [`CODE_UNSUPPORTED`] (`kind:"expr"`), independent of `target_dialect`
    /// (MED-1). Distinct from [`Self::split_part_envelope_err`], which is the
    /// PG-renderable-but-SQLite-unsupported portability reject.
    fn malformed_synth_err(&self, reason: String) -> AuthoringError {
        self.err(
            CODE_UNSUPPORTED,
            Some(UnsupportedKind::Expr),
            // The shape is broken regardless of target; report the current target
            // so the payload's `dialect` field is faithful to the deploy.
            self.target_dialect,
            reason,
            Some(
                "call the synth helper with its pinned argument shape \
                 (now/genRandomUuid take no args; concatWs takes a delimiter + \
                 >=1 value; splitPart takes exactly (column, delim, n))"
                    .to_string(),
            ),
        )
    }

    /// A splitPart **portability-boundary** reject: the call is well-formed and
    /// PG-renderable (`split_part` accepts it), but OUT of the pinned SQLite
    /// envelope (§9). It is therefore a hard error ONLY on the SQLite leg and
    /// loads fine on a Postgres target — the §2.4.1 loads-on-PG/rejected-on-SQLite
    /// verdict. The caller must only reach this when `target_dialect == Sqlite`.
    fn split_part_envelope_err(&self, reason: String) -> AuthoringError {
        self.err(
            CODE_EXPR_NOT_PORTABLE,
            None,
            Dialect::Sqlite,
            reason,
            Some(
                "use a single-ASCII delimiter with 1<=n<=8, restructure to stay \
                 in-envelope (split into <=8 parts), or mark the migration PG-only \
                 (dialect_scope=PgOnly)"
                    .to_string(),
            ),
        )
    }

    /// A splitPart **grammar** reject: the call's argument SHAPE is broken on EVERY
    /// dialect — the delimiter is not a string literal, or the part index is not a
    /// positive integer literal. Unlike [`Self::split_part_envelope_err`] (the
    /// PG-renderable-but-SQLite-out-of-envelope verdict, SQLite-only), the renderer
    /// enforces this same grammar fail-closed on BOTH dialects, so the validator
    /// rejects it regardless of `target_dialect` — and stamps the *current* target so
    /// the payload's `dialect` is faithful to the deploy. CODE_EXPR_NOT_PORTABLE (the
    /// §8.8 structured envelope), the AI loop's primary structured-feedback signal.
    fn split_part_grammar_err(&self, reason: String) -> AuthoringError {
        self.err(
            CODE_EXPR_NOT_PORTABLE,
            None,
            self.target_dialect,
            reason,
            Some(
                "pass a single-ASCII string LITERAL delimiter and a positive integer \
                 LITERAL part index (a runtime/computed delim or n is not portable)"
                    .to_string(),
            ),
        )
    }

    /// A concatWs **portability-boundary** reject: the call is well-formed and
    /// PG-renderable (`concat_ws` takes any expression delimiter), but the SQLite
    /// lowering's literal-delimiter head-trim assumption is violated by a
    /// non-literal delimiter. Like [`Self::split_part_envelope_err`], it is a hard
    /// error ONLY on the SQLite leg; the caller only reaches it when
    /// `target_dialect == Sqlite`.
    fn concat_ws_delim_envelope_err(&self, reason: String) -> AuthoringError {
        self.err(
            CODE_EXPR_NOT_PORTABLE,
            None,
            Dialect::Sqlite,
            reason,
            Some(
                "pass a string literal as the concatWs delimiter, or mark the \
                 migration PG-only (dialect_scope=PgOnly)"
                    .to_string(),
            ),
        )
    }

    /// A Tier-PG expression node. These are not portable-envelope misses: they are
    /// PostgreSQL-only value nodes, so SQLite/MySQL validation refuses them as
    /// `UNSUPPORTED { kind:"expr" }` before rendering.
    fn check_pg_only_expr(&self, name: &'static str) -> Result<(), AuthoringError> {
        // Read the PG-only verdict off the generated dialect vocabulary (a
        // `pg = portable, else = unsupported` disposition) rather than a bespoke
        // `== Postgres` dialect arm — the same `Disposition::is_supported` reading
        // `Op::support` uses when assembling per-dialect support cells.
        if crate::model::support::pg_only_expr_disposition(self.target_dialect).is_supported() {
            return Ok(());
        }
        Err(self.err(
            CODE_UNSUPPORTED,
            Some(UnsupportedKind::Expr),
            self.target_dialect,
            format!(
                "{name} is a PostgreSQL-only expression node and has no \
                 SQLite/MySQL renderer"
            ),
            Some(
                "use this node only in a PostgreSQL-targeted migration, or rewrite \
                 the predicate using portable expression nodes"
                    .to_string(),
            ),
        ))
    }

    fn check_pg_text_literal(&self, value: &str, what: &str) -> Result<(), AuthoringError> {
        if value.is_empty() {
            return Err(self.err(
                CODE_UNSUPPORTED,
                Some(UnsupportedKind::Expr),
                self.target_dialect,
                format!("{what} must be a non-empty text literal"),
                Some(format!("pass a non-empty string literal for {what}")),
            ));
        }
        if value.contains('\0') {
            return Err(self.err(
                CODE_UNSUPPORTED,
                Some(UnsupportedKind::Expr),
                self.target_dialect,
                format!("{what} must not contain a NUL byte"),
                Some(format!("remove the NUL byte from {what}")),
            ));
        }
        Ok(())
    }

    fn check_in_list(
        &self,
        expr: &Expr,
        elems: &[crate::model::ir::IrScalar],
        depth: u32,
    ) -> Result<(), AuthoringError> {
        #[derive(Clone, Copy, Eq, PartialEq)]
        enum ElemKind {
            Text,
            Number,
            Bool,
            Null,
        }

        self.walk_depth(expr, depth)?;
        let mut first_kind = None;
        for (idx, elem) in elems.iter().enumerate() {
            let kind = match elem {
                crate::model::ir::IrScalar::Str(s) => {
                    self.check_pg_text_literal(s, "inList element")?;
                    ElemKind::Text
                }
                crate::model::ir::IrScalar::Int(_) | crate::model::ir::IrScalar::Decimal(_) => {
                    ElemKind::Number
                }
                crate::model::ir::IrScalar::Bool(_) => ElemKind::Bool,
                crate::model::ir::IrScalar::Null => ElemKind::Null,
                crate::model::ir::IrScalar::Bytes(_) => {
                    return Err(self.err(
                        CODE_UNSUPPORTED,
                        Some(UnsupportedKind::Expr),
                        self.target_dialect,
                        format!(
                            "inList element {idx} must be string, number, boolean, or null; bytes are not allowed"
                        ),
                        Some("remove byte values from the in() / notIn() list".to_string()),
                    ));
                }
            };
            if let Some(first) = first_kind {
                if kind != first {
                    return Err(self.err(
                        CODE_UNSUPPORTED,
                        Some(UnsupportedKind::Expr),
                        self.target_dialect,
                        "inList elements must be homogeneous".to_string(),
                        Some(
                            "use one scalar kind per in() / notIn() list, or split the predicate"
                                .to_string(),
                        ),
                    ));
                }
            } else {
                first_kind = Some(kind);
            }
        }
        Ok(())
    }

    fn check_pg_regex_match(
        &self,
        expr: &Expr,
        pattern: &str,
        depth: u32,
    ) -> Result<(), AuthoringError> {
        self.check_pg_only_expr("PG regex match")?;
        self.walk_depth(expr, depth)?;
        self.check_pg_text_literal(pattern, "PG regex pattern")
    }

    fn check_duration(&self, duration: &Duration) -> Result<(), AuthoringError> {
        if duration.is_empty() {
            return Err(self.err(
                CODE_UNSUPPORTED,
                Some(UnsupportedKind::Expr),
                self.target_dialect,
                "PG interval duration must include at least one field".to_string(),
                Some("use a structured duration such as {\"minutes\":1}".to_string()),
            ));
        }
        Ok(())
    }

    fn check_split_part(&self, args: &[Expr]) -> Result<(), AuthoringError> {
        // Shape: splitPart(col, delim, n) — exactly three args. The WRONG ARITY is
        // broken on BOTH dialects (`split_part` is ternary on PG too), so it is an
        // unconditional CODE_UNSUPPORTED, NOT a dialect-gated envelope reject
        // (MED-1). The caller (`check_synth`) already checks arity before the arg
        // walk; this is a defensive guard so the args[1]/args[2] indexing below
        // cannot panic if `check_split_part` is ever reached on a non-ternary call.
        if args.len() != 3 {
            return Err(self.malformed_synth_err(format!(
                "c.fn.splitPart takes exactly (column, delim, n); got {} args",
                args.len()
            )));
        }
        // ── GRAMMAR (dialect-NEUTRAL) — enforced on EVERY target, BEFORE the
        //    dialect early-return. The renderer (dml.rs render_split_part) requires a
        //    STRING-LITERAL delim and a POSITIVE-INTEGER-LITERAL n fail-closed on BOTH
        //    dialects, so a grammar-broken node is renderable on neither; the
        //    validator (the AI loop's structured-feedback signal, §3.3.1.1) rejects it
        //    here rather than deferring to render time. We capture the validated
        //    string/int so the SQLite ENVELOPE checks below need not re-match.
        let delim = match &args[1] {
            Expr::Literal { value: crate::model::ir::IrScalar::Str(s) } => s,
            Expr::Literal { value: other } => {
                return Err(self.split_part_grammar_err(format!(
                    "c.fn.splitPart delimiter must be a string literal; got {other:?}"
                )));
            }
            other => {
                return Err(self.split_part_grammar_err(format!(
                    "c.fn.splitPart delimiter must be a literal (a runtime/computed \
                     delimiter is not portable); got {other:?}"
                )));
            }
        };
        let n = match &args[2] {
            Expr::Literal { value: crate::model::ir::IrScalar::Int(n) } => {
                if *n < 1 {
                    return Err(self.split_part_grammar_err(format!(
                        "c.fn.splitPart part index n must be a positive integer; got {n}"
                    )));
                }
                *n
            }
            Expr::Literal { value: other } => {
                return Err(self.split_part_grammar_err(format!(
                    "c.fn.splitPart part index n must be a positive integer literal; \
                     got {other:?}"
                )));
            }
            other => {
                return Err(self.split_part_grammar_err(format!(
                    "c.fn.splitPart part index n must be a literal positive integer \
                     (a runtime n is not portable); got {other:?}"
                )));
            }
        };

        // ── ENVELOPE (SQLite-only) — the grammar-valid node is renderable on
        //    Postgres but a multi-char/non-ASCII delim or n>8 is out of the pinned
        //    SQLite envelope (§9). On a POSTGRES target the node loads fine; only a
        //    SQLITE target rejects it (§2.4.1).
        if self.target_dialect == Dialect::Postgres {
            return Ok(());
        }
        // delim — a single ASCII character (one byte, code point < 0x80).
        let bytes = delim.as_bytes();
        if bytes.len() != 1 || bytes[0] >= 0x80 {
            return Err(self.split_part_envelope_err(format!(
                "c.fn.splitPart delimiter must be a single ASCII character \
                 (one byte, code point < 0x80); got {delim:?}"
            )));
        }
        // n — within the proven inline-unroll bound.
        if n > SPLIT_PART_MAX_N {
            return Err(self.split_part_envelope_err(format!(
                "c.fn.splitPart part index n must be <= {SPLIT_PART_MAX_N} \
                 (the proven inline-unroll bound); got {n}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::expr::{
        BinaryOp, CastTarget, Expr, ExtractField, PgExtractField, ScalarFn, SynthFn, UnaryOp,
    };
    use crate::model::ir::{IndexElement, IrScalar, IrValue};

    fn cols() -> Vec<String> {
        vec!["name".into(), "first".into(), "last".into(), "total".into(), "active".into()]
    }

    fn scope<'a>(table: &'a str, cols: &'a [String]) -> TargetScope<'a> {
        TargetScope::new(table, cols)
    }

    // ── DoS guard: explicit walk depth bound (code-critic LOW) ──────────────
    // The validator OWNS the recursion bound (Ctx::MAX_EXPR_DEPTH), not an
    // implicit serde_json::recursion_limit. Build the AST in Rust (bypassing
    // serde entirely, exactly as a future streaming/custom deserializer or a
    // raised serde limit would) and assert the walker still refuses an
    // over-deep tree as CODE_UNSUPPORTED rather than recursing to a stack
    // overflow.

    /// Wrap `inner` in `depth` nested `UnaryOp::Not` nodes.
    fn nest_not(depth: u32, inner: Expr) -> Expr {
        let mut e = inner;
        for _ in 0..depth {
            e = Expr::UnaryOp { op: UnaryOp::Not, operand: Box::new(e) };
        }
        e
    }

    #[test]
    fn walk_refuses_over_deep_expression_as_unsupported() {
        let c = cols();
        let sc = scope("users", &c);
        // Comfortably past the bound — would stack-overflow a naive walker.
        let deep = nest_not(Ctx::MAX_EXPR_DEPTH + 50, Expr::col("name"));
        let err = validate_expr(&deep, Dialect::Postgres, &sc, 0, None)
            .expect_err("an over-deep expression must be refused, not recursed");
        assert_eq!(err.code, CODE_UNSUPPORTED, "got: {err}");
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert!(
            err.reason.contains("nesting"),
            "the error must name the depth bound, got: {}",
            err.reason
        );
    }

    #[test]
    fn walk_accepts_expression_within_the_depth_bound() {
        let c = cols();
        let sc = scope("users", &c);
        // A legitimately-shallow tree (well under the bound) still validates —
        // the bound never narrows the realistic accepted set.
        let ok = nest_not(Ctx::MAX_EXPR_DEPTH - 2, Expr::col("name"));
        assert!(
            validate_expr(&ok, Dialect::Postgres, &sc, 0, None).is_ok(),
            "a tree within the depth bound must validate"
        );
    }

    #[test]
    fn current_setting_and_current_user_are_pg_only_rejected_off_postgres() {
        // Regression: current_setting / current_user are PG-only VENDOR scalars
        // (they render as PG built-ins with no SQLite/MySQL form). A portable op
        // carrying them must be REFUSED at validate on SQLite/MySQL — not sail
        // through and break at apply.
        let c = cols();
        let sc = scope("users", &c);
        for f in [ScalarFn::CurrentUser, ScalarFn::CurrentSetting] {
            let e = Expr::FnCall { r#fn: f, args: vec![] };
            assert!(
                validate_expr(&e, Dialect::Postgres, &sc, 0, None).is_ok(),
                "{f:?} must validate on Postgres"
            );
            for d in [Dialect::Sqlite, Dialect::Mysql] {
                let err = validate_expr(&e, d, &sc, 0, None)
                    .expect_err("a PG-only vendor scalar must be refused off Postgres");
                assert_eq!(err.code, CODE_UNSUPPORTED, "{f:?} on {d:?}: {err}");
                assert_eq!(err.kind, Some(UnsupportedKind::Expr));
            }
        }
    }

    // ── (a) every allow-listed node validates ──────────────────────────────

    #[test]
    fn all_allow_listed_nodes_validate() {
        let c = cols();
        let sc = scope("users", &c);
        // A representative tree using each node kind.
        let e = Expr::BinOp {
            op: BinaryOp::And,
            lhs: Box::new(Expr::UnaryOp {
                op: UnaryOp::IsNotNull,
                operand: Box::new(Expr::col("name")),
            }),
            rhs: Box::new(Expr::BinOp {
                op: BinaryOp::Gt,
                lhs: Box::new(Expr::Cast {
                    operand: Box::new(Expr::FnCall {
                        r#fn: ScalarFn::Length,
                        args: vec![Expr::col("name")],
                    }),
                    target: CastTarget::Int,
                }),
                rhs: Box::new(Expr::lit(IrScalar::Int(0))),
            }),
        };
        assert!(validate_expr(&e, Dialect::Sqlite, &sc, 0, None).is_ok());

        // Case + FnCall(coalesce) + concat.
        let case = Expr::Case {
            branches: vec![CaseBranch {
                when: Expr::UnaryOp {
                    op: UnaryOp::IsNull,
                    operand: Box::new(Expr::col("first")),
                },
                then: Expr::lit(IrScalar::Str("none".into())),
            }],
            r#else: Some(Box::new(Expr::FnCall {
                r#fn: ScalarFn::Coalesce,
                args: vec![Expr::col("first"), Expr::lit(IrScalar::Str("".into()))],
            })),
        };
        assert!(validate_expr(&case, Dialect::Postgres, &sc, 1, None).is_ok());
    }

    fn in_list(expr: Expr, elems: Vec<&str>) -> Expr {
        Expr::InList {
            expr: Box::new(expr),
            elems: elems.into_iter().map(|s| IrScalar::Str(s.to_string())).collect(),
            negated: false,
        }
    }

    fn not_in_list(expr: Expr, elems: Vec<&str>) -> Expr {
        Expr::InList {
            expr: Box::new(expr),
            elems: elems.into_iter().map(|s| IrScalar::Str(s.to_string())).collect(),
            negated: true,
        }
    }

    #[test]
    fn pg_only_expr_nodes_validate_on_pg() {
        let c = cols();
        let sc = scope("users", &c);
        for e in [
            Expr::PgRegexMatch {
                expr: Box::new(Expr::col("name")),
                pattern: "^[a-z]+$".to_string(),
            },
            Expr::BinOp {
                op: BinaryOp::Le,
                lhs: Box::new(Expr::PgColumnSize { expr: Box::new(Expr::col("name")) }),
                rhs: Box::new(Expr::lit(IrScalar::Int(8192))),
            },
            Expr::PgExtract {
                field: PgExtractField::Epoch,
                from: Box::new(Expr::col("total")),
            },
        ] {
            validate_expr(&e, Dialect::Postgres, &sc, 0, None)
                .unwrap_or_else(|err| panic!("PG-only expression must validate on PG: {err}"));
        }
    }

    #[test]
    fn portable_predicate_and_extract_nodes_validate_on_all_three_dialects() {
        // between / like / distinctFrom / inList / extract are PORTABLE (§3.4):
        // they render on all three dialects (the engine owns each per-dialect
        // lowering), so the walk accepts them with NO dialect gate — including on
        // SQLite/MySQL, exactly where the PG-only nodes are refused.
        let c = cols();
        let sc = scope("users", &c);
        let nodes = [
            Expr::Between {
                operand: Box::new(Expr::col("total")),
                low: Box::new(Expr::lit(IrScalar::Int(0))),
                high: Box::new(Expr::lit(IrScalar::Int(100))),
            },
            Expr::Like {
                operand: Box::new(Expr::col("name")),
                pattern: Box::new(Expr::lit(IrScalar::Str("A%".into()))),
            },
            Expr::DistinctFrom {
                left: Box::new(Expr::col("first")),
                right: Box::new(Expr::col("last")),
            },
            in_list(Expr::col("name"), vec!["active", "past_due"]),
            not_in_list(Expr::col("name"), vec!["suspended"]),
            in_list(Expr::col("name"), vec![]),
            Expr::InList {
                expr: Box::new(Expr::col("total")),
                elems: vec![IrScalar::Int(200), IrScalar::Int(404), IrScalar::Int(500)],
                negated: false,
            },
            Expr::InList {
                expr: Box::new(Expr::col("active")),
                elems: vec![IrScalar::Bool(true), IrScalar::Bool(false)],
                negated: false,
            },
            Expr::Extract { field: ExtractField::Year, from: Box::new(Expr::col("total")) },
            Expr::Extract { field: ExtractField::Month, from: Box::new(Expr::col("total")) },
            Expr::Extract { field: ExtractField::Day, from: Box::new(Expr::col("total")) },
            Expr::Extract { field: ExtractField::Hour, from: Box::new(Expr::col("total")) },
            Expr::Extract { field: ExtractField::Minute, from: Box::new(Expr::col("total")) },
            Expr::Extract { field: ExtractField::Dow, from: Box::new(Expr::col("total")) },
        ];
        for e in &nodes {
            for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
                validate_expr(e, d, &sc, 0, None).unwrap_or_else(|err| {
                    panic!("portable predicate/extract must validate on {d:?}: {err}")
                });
            }
        }
    }

    #[test]
    fn portable_aggregate_nodes_validate_on_all_three_dialects() {
        use crate::model::expr::AggFunc;
        // count(*) / count(DISTINCT col) / sum/avg/min/max(col) are PORTABLE (§3.4/
        // §3.6): byte-identical SQL on PG/SQLite/MySQL, so the walk accepts them with
        // NO dialect gate. (The grouped/SELECT position check is a Phase-2 concern;
        // this slice accepts the node structurally, recursing into the arg.)
        let c = cols();
        let sc = scope("users", &c);
        let nodes = [
            Expr::Agg { func: AggFunc::Count, arg: None, distinct: false },
            Expr::Agg {
                func: AggFunc::Count,
                arg: Some(Box::new(Expr::col("total"))),
                distinct: true,
            },
            Expr::Agg { func: AggFunc::Sum, arg: Some(Box::new(Expr::col("total"))), distinct: false },
            Expr::Agg { func: AggFunc::Avg, arg: Some(Box::new(Expr::col("total"))), distinct: false },
            Expr::Agg { func: AggFunc::Min, arg: Some(Box::new(Expr::col("total"))), distinct: false },
            Expr::Agg { func: AggFunc::Max, arg: Some(Box::new(Expr::col("total"))), distinct: false },
        ];
        for e in &nodes {
            for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
                validate_expr(e, d, &sc, 0, None)
                    .unwrap_or_else(|err| panic!("portable aggregate must validate on {d:?}: {err}"));
            }
        }
        // A bogus column inside the aggregate arg is still caught by the recursive
        // colref check (the node isn't a blind accept).
        let bad = Expr::Agg {
            func: AggFunc::Sum,
            arg: Some(Box::new(Expr::col("does_not_exist"))),
            distinct: false,
        };
        assert!(
            validate_expr(&bad, Dialect::Postgres, &sc, 0, None).is_err(),
            "aggregate must still validate its argument's column ref"
        );
    }

    #[test]
    fn portable_scalar_fns_validate_on_all_three_dialects() {
        // mod / round / floor / ceil / substr / replace are PORTABLE ScalarFns
        // (§3.4): identical spelling on PG/SQLite/MySQL (mod renders as the `%`
        // operator), so the walk accepts them with NO dialect gate — unlike the
        // PG-only currentSetting/currentUser vendor scalars.
        let c = cols();
        let sc = scope("users", &c);
        let nodes = [
            Expr::FnCall {
                r#fn: ScalarFn::Mod,
                args: vec![Expr::col("total"), Expr::lit(IrScalar::Int(3))],
            },
            Expr::FnCall { r#fn: ScalarFn::Round, args: vec![Expr::col("total")] },
            Expr::FnCall {
                r#fn: ScalarFn::Round,
                args: vec![Expr::col("total"), Expr::lit(IrScalar::Int(2))],
            },
            Expr::FnCall { r#fn: ScalarFn::Floor, args: vec![Expr::col("total")] },
            Expr::FnCall { r#fn: ScalarFn::Ceil, args: vec![Expr::col("total")] },
            Expr::FnCall {
                r#fn: ScalarFn::Substr,
                args: vec![
                    Expr::col("name"),
                    Expr::lit(IrScalar::Int(1)),
                    Expr::lit(IrScalar::Int(3)),
                ],
            },
            Expr::FnCall {
                r#fn: ScalarFn::Replace,
                args: vec![
                    Expr::col("name"),
                    Expr::lit(IrScalar::Str("a".into())),
                    Expr::lit(IrScalar::Str("b".into())),
                ],
            },
        ];
        for e in &nodes {
            for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
                validate_expr(e, d, &sc, 0, None).unwrap_or_else(|err| {
                    panic!("portable scalar fn must validate on {d:?}: {err}")
                });
            }
        }
    }

    #[test]
    fn pg_only_expr_nodes_reject_on_sqlite_and_mysql() {
        let c = cols();
        let sc = scope("users", &c);
        for e in [
            Expr::PgRegexMatch {
                expr: Box::new(Expr::col("name")),
                pattern: "^[a-z]+$".to_string(),
            },
            Expr::PgColumnSize { expr: Box::new(Expr::col("name")) },
            Expr::PgExtract {
                field: PgExtractField::Epoch,
                from: Box::new(Expr::col("total")),
            },
        ] {
            for d in [Dialect::Sqlite, Dialect::Mysql] {
                let err = validate_expr(&e, d, &sc, 0, None)
                    .expect_err("PG-only expression must reject on non-PG");
                assert_eq!(err.code, CODE_UNSUPPORTED);
                assert_eq!(err.kind, Some(UnsupportedKind::Expr));
                assert_eq!(err.dialect, d);
                assert!(err.reason.contains("PostgreSQL-only"), "got: {err}");
            }
        }
    }

    #[test]
    fn text_literal_shapes_are_checked() {
        let c = cols();
        let sc = scope("users", &c);
        let empty_membership = in_list(Expr::col("name"), vec![]);
        for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            validate_expr(&empty_membership, d, &sc, 0, None)
                .unwrap_or_else(|err| panic!("empty inList must validate on {d:?}: {err}"));
        }

        let nul_elem = in_list(Expr::col("name"), vec!["ok", "bad\0value"]);
        let err = validate_expr(&nul_elem, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert!(err.reason.contains("NUL"));

        let mixed_elem = Expr::InList {
            expr: Box::new(Expr::col("name")),
            elems: vec![IrScalar::Str("ok".into()), IrScalar::Int(200)],
            negated: false,
        };
        let err = validate_expr(&mixed_elem, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert!(err.reason.contains("homogeneous"));

        let bytes_elem = Expr::InList {
            expr: Box::new(Expr::col("name")),
            elems: vec![IrScalar::Bytes(vec![1, 2, 3])],
            negated: false,
        };
        let err = validate_expr(&bytes_elem, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert!(err.reason.contains("bytes are not allowed"));

        let empty_pattern = Expr::PgRegexMatch {
            expr: Box::new(Expr::col("name")),
            pattern: String::new(),
        };
        let err = validate_expr(&empty_pattern, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert!(err.reason.contains("non-empty"));
    }

    // ── (b) splitPart envelope ─────────────────────────────────────────────

    fn split(delim: &str, n: i64) -> Expr {
        Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::lit(IrScalar::Str(delim.into())),
                Expr::lit(IrScalar::Int(n)),
            ],
        }
    }

    #[test]
    fn split_part_in_envelope_validates() {
        let c = cols();
        let sc = scope("users", &c);
        for n in 1..=SPLIT_PART_MAX_N {
            assert!(
                validate_expr(&split(" ", n), Dialect::Sqlite, &sc, 0, None).is_ok(),
                "n={n} single-ASCII delim must be in-envelope"
            );
        }
    }

    #[test]
    fn split_part_multichar_delim_rejected() {
        let c = cols();
        let sc = scope("users", &c);
        let err = validate_expr(&split(", ", 1), Dialect::Sqlite, &sc, 2, Some("m.ts:9"))
            .unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
        assert_eq!(err.op_index, 2);
        assert_eq!(err.dialect, Dialect::Sqlite);
        assert_eq!(err.ts_location.as_deref(), Some("m.ts:9"));
        assert!(err.suggested_fix.is_some());
        // The structured payload leads with suggested_fix.
        let json = err.to_json();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.keys().next().unwrap(), "suggested_fix");
        assert_eq!(obj["code"], CODE_EXPR_NOT_PORTABLE);
    }

    // ── MED-1: the splitPart envelope verdict is DIALECT-GATED ──────────────
    // An OUT-OF-ENVELOPE-but-PG-renderable c.fn.splitPart (multi-char delim,
    // n>8, …) is renderable on Postgres (`split_part` accepts it) and only a
    // hard reject on the SQLite leg (§2.4.1/§9). The SAME node must therefore
    // validate OK on a Postgres target and be EXPR_NOT_PORTABLE on a SQLite
    // target. RED before check_split_part branches on target_dialect.

    #[test]
    fn out_of_envelope_split_part_loads_on_pg_rejected_on_sqlite() {
        let c = cols();
        let sc = scope("users", &c);
        // The §2.4.1 loads-on-PG / rejected-on-SQLite fixture: multi-char delim.
        let node = split(", ", 1);
        assert!(
            validate_expr(&node, Dialect::Postgres, &sc, 0, None).is_ok(),
            "an out-of-envelope-but-PG-renderable splitPart must VALIDATE on a Postgres target"
        );
        let err = validate_expr(&node, Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_EXPR_NOT_PORTABLE,
            "the same node must be EXPR_NOT_PORTABLE on a SQLite target"
        );
        assert_eq!(err.dialect, Dialect::Sqlite);

        // Likewise n>8 and a non-ASCII delim: PG-renderable, SQLite-rejected.
        for node in [split(" ", 9), split("·", 1)] {
            assert!(
                validate_expr(&node, Dialect::Postgres, &sc, 0, None).is_ok(),
                "out-of-envelope splitPart loads on PG"
            );
            assert_eq!(
                validate_expr(&node, Dialect::Sqlite, &sc, 0, None).unwrap_err().code,
                CODE_EXPR_NOT_PORTABLE
            );
        }
    }

    // ── LOW (PR6b code-critic): the GRAMMAR is dialect-NEUTRAL ──────────────
    // A grammar-broken splitPart — a NON-literal / non-string delim, or a
    // non-literal / non-positive-int n — is not renderable on EITHER dialect (the
    // renderer enforces the same grammar fail-closed on PG and SQLite). The
    // validator (the AI loop's primary structured-feedback signal, §3.3.1.1) must
    // therefore reject it on a Postgres target too, BEFORE the dialect early-return —
    // not defer the only rejection to render time. RED before check_split_part lifts
    // the grammar checks above the `if Postgres { return Ok(()) }`.
    #[test]
    fn grammar_broken_split_part_rejected_on_pg_too() {
        let c = cols();
        let sc = scope("users", &c);

        // (1) delim is a COLUMN REFERENCE (a runtime/computed delimiter) — not a
        //     string literal. Grammar-broken on BOTH dialects.
        let runtime_delim = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![Expr::col("name"), Expr::col("first"), Expr::lit(IrScalar::Int(1))],
        };
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            let err = validate_expr(&runtime_delim, d, &sc, 0, None).unwrap_err();
            assert_eq!(
                err.code, CODE_EXPR_NOT_PORTABLE,
                "a non-literal delim must reject on {d:?} (grammar is dialect-neutral)"
            );
        }

        // (2) delim is a NON-STRING literal (an integer). Grammar-broken on both.
        let int_delim = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::lit(IrScalar::Int(7)),
                Expr::lit(IrScalar::Int(1)),
            ],
        };
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            assert_eq!(
                validate_expr(&int_delim, d, &sc, 0, None).unwrap_err().code,
                CODE_EXPR_NOT_PORTABLE,
                "a non-string-literal delim must reject on {d:?}"
            );
        }

        // (3) n is a COLUMN REFERENCE (a runtime n) — not a literal. Both dialects.
        let runtime_n = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![Expr::col("name"), Expr::lit(IrScalar::Str(",".into())), Expr::col("total")],
        };
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            assert_eq!(
                validate_expr(&runtime_n, d, &sc, 0, None).unwrap_err().code,
                CODE_EXPR_NOT_PORTABLE,
                "a non-literal n must reject on {d:?}"
            );
        }

        // (4) n is a non-POSITIVE integer literal (n<1) — grammar-broken on both.
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            assert_eq!(
                validate_expr(&split(",", 0), d, &sc, 0, None).unwrap_err().code,
                CODE_EXPR_NOT_PORTABLE,
                "n<1 must reject on {d:?}"
            );
        }

        // GUARD: a grammar-VALID but out-of-ENVELOPE node (multi-char string-literal
        // delim, or n>8) is still PG-renderable — the envelope stays SQLite-gated.
        assert!(
            validate_expr(&split(", ", 1), Dialect::Postgres, &sc, 0, None).is_ok(),
            "a multi-char STRING-LITERAL delim is grammar-valid → still loads on PG"
        );
        assert!(
            validate_expr(&split(",", 9), Dialect::Postgres, &sc, 0, None).is_ok(),
            "n>8 is grammar-valid (positive int literal) → still loads on PG"
        );
    }

    #[test]
    fn malformed_split_part_arity_is_unconditional_unsupported() {
        // A genuinely-MALFORMED splitPart — wrong arity (not exactly 3 args) — is
        // broken on BOTH dialects (`split_part` is ternary on PG too), so it is
        // an unconditional CODE_UNSUPPORTED, NOT a dialect-gated portability
        // reject. Rejected on PG AND SQLite.
        let c = cols();
        let sc = scope("users", &c);
        let two_arg = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![Expr::col("name"), Expr::lit(IrScalar::Str(" ".into()))],
        };
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            let err = validate_expr(&two_arg, d, &sc, 0, None).unwrap_err();
            assert_eq!(err.code, CODE_UNSUPPORTED, "wrong arity is broken on both dialects ({d:?})");
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
    }

    #[test]
    fn split_part_non_ascii_delim_rejected() {
        let c = cols();
        let sc = scope("users", &c);
        let err = validate_expr(&split("·", 1), Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }

    #[test]
    fn split_part_n_out_of_range_rejected() {
        let c = cols();
        let sc = scope("users", &c);
        for n in [0_i64, -1, 9, 100] {
            let err = validate_expr(&split(" ", n), Dialect::Sqlite, &sc, 0, None).unwrap_err();
            assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE, "n={n} must reject");
        }
        // n=8 is the boundary that PASSES.
        assert!(validate_expr(&split(" ", 8), Dialect::Sqlite, &sc, 0, None).is_ok());
    }

    // ── (b') the remaining SynthFn arities — structural backstop ───────────
    // now/genRandomUuid take ZERO args; concatWs takes >=2 (a delimiter + >=1
    // value). Independent of the (not-yet-existing) render seam, the validator
    // is the structural backstop. RED before the check_synth arity fix.

    fn synth(f: SynthFn, args: Vec<Expr>) -> Expr {
        Expr::FnSynth { r#fn: f, args }
    }

    #[test]
    fn now_with_args_is_rejected() {
        // MED-1: now(arg) is a genuinely-MALFORMED synth — `now()` is nullary on
        // BOTH dialects — so it is an unconditional CODE_UNSUPPORTED, on PG AND
        // SQLite (not a dialect-gated portability reject).
        let sc = TargetScope::structural_only("t");
        let e = synth(SynthFn::Now, vec![Expr::lit(IrScalar::Int(1))]);
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            let err = validate_expr(&e, d, &sc, 0, None).unwrap_err();
            assert_eq!(err.code, CODE_UNSUPPORTED, "now(arg) is broken on both dialects ({d:?})");
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
        // zero-arg form passes on both.
        assert!(validate_expr(&synth(SynthFn::Now, vec![]), Dialect::Postgres, &sc, 0, None).is_ok());
        assert!(validate_expr(&synth(SynthFn::Now, vec![]), Dialect::Sqlite, &sc, 0, None).is_ok());
    }

    #[test]
    fn gen_random_uuid_with_args_is_rejected() {
        // MED-1: genRandomUuid(args) is genuinely malformed → unconditional
        // CODE_UNSUPPORTED on both dialects.
        let sc = TargetScope::structural_only("t");
        let e = synth(SynthFn::GenRandomUuid, vec![Expr::col("x"), Expr::col("y")]);
        for d in [Dialect::Postgres, Dialect::Sqlite] {
            let err = validate_expr(&e, d, &sc, 0, None).unwrap_err();
            assert_eq!(err.code, CODE_UNSUPPORTED);
        }
        assert!(
            validate_expr(&synth(SynthFn::GenRandomUuid, vec![]), Dialect::Postgres, &sc, 0, None)
                .is_ok()
        );
    }

    #[test]
    fn concat_ws_arity_is_enforced() {
        // MED-1: concatWs with <2 args is genuinely malformed (no valid join on
        // EITHER dialect) → unconditional CODE_UNSUPPORTED on PG and SQLite.
        let c = cols();
        let sc = scope("users", &c);
        // 0 args and 1 arg (delimiter only, no values) are out of shape.
        for bad in [vec![], vec![Expr::lit(IrScalar::Str(",".into()))]] {
            for d in [Dialect::Postgres, Dialect::Sqlite] {
                let err = validate_expr(&synth(SynthFn::ConcatWs, bad.clone()), d, &sc, 0, None)
                    .unwrap_err();
                assert_eq!(err.code, CODE_UNSUPPORTED, "concatWs needs delim + >=1 value ({d:?})");
            }
        }
        // delim + 1 value is the minimum valid shape; the value still recurses.
        let ok = synth(
            SynthFn::ConcatWs,
            vec![Expr::lit(IrScalar::Str(",".into())), Expr::col("name")],
        );
        assert!(validate_expr(&ok, Dialect::Sqlite, &sc, 0, None).is_ok());
    }

    #[test]
    fn concat_ws_non_literal_delim_rejected_on_sqlite_loads_on_pg() {
        // LOW — the SQLite render's NULL-skip head-trim (`substr(fold,
        // length(delim)+1)`) is only correct for a FIXED literal delimiter. A
        // non-literal delimiter (here a ColRef to an existing column, so rule (c)
        // is satisfied and the ONLY objection is the literal-delim gate) must be a
        // HARD reject on SQLite and load fine on PG (`concat_ws` takes any expr),
        // mirroring the splitPart delim-literal gate.
        let c = cols();
        let sc = scope("users", &c);
        let e = synth(
            SynthFn::ConcatWs,
            vec![Expr::col("name"), Expr::col("first")],
        );
        // PG: a non-literal delimiter is fine.
        assert!(
            validate_expr(&e, Dialect::Postgres, &sc, 0, None).is_ok(),
            "a non-literal concatWs delimiter must LOAD on a Postgres target"
        );
        // SQLite: the structural literal-delim gate rejects it.
        let err = validate_expr(&e, Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_EXPR_NOT_PORTABLE,
            "a non-literal concatWs delimiter must reject on SQLite (literal-delim gate); got: {err}"
        );
    }

    #[test]
    fn concat_ws_recurses_into_a_bad_nested_value() {
        // The arity gate must not short-circuit recursion: a nested
        // out-of-envelope splitPart inside a well-shaped concatWs still rejects.
        let c = cols();
        let sc = scope("users", &c);
        let e = synth(
            SynthFn::ConcatWs,
            vec![Expr::lit(IrScalar::Str(",".into())), split(", ", 1)],
        );
        let err = validate_expr(&e, Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }

    // ── the Layer-2 dialect() per-dialect value escape (§3.4) ────────────────

    fn dialectal(
        default: Option<Expr>,
        pg: Option<Expr>,
        sqlite: Option<Expr>,
        mysql: Option<Expr>,
    ) -> Expr {
        Expr::Dialectal {
            default: default.map(Box::new),
            pg: pg.map(Box::new),
            sqlite: sqlite.map(Box::new),
            mysql: mysql.map(Box::new),
        }
    }

    #[test]
    fn dialectal_missing_leg_no_default_accepted_on_own_target_refused_off_target() {
        // dialect({ pg: A }) — no default. Its covered set is exactly {pg}: it is
        // ACCEPTED targeting PG (its own leg), REFUSED targeting SQLite/MySQL
        // (neither own leg nor default) — the per-TARGET scope math.
        let sc = TargetScope::structural_only("t");
        let e = dialectal(None, Some(Expr::lit(IrScalar::Str("A".into()))), None, None);

        assert!(
            validate_expr(&e, Dialect::Postgres, &sc, 0, None).is_ok(),
            "a pg-only dialect() covers the PG target"
        );
        for d in [Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_expr(&e, d, &sc, 0, None).unwrap_err();
            assert_eq!(
                err.code, CODE_EXPR_NOT_PORTABLE,
                "a pg-only dialect() must refuse the {d:?} target (no own leg, no default); got: {err}"
            );
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
    }

    #[test]
    fn dialectal_default_covers_every_off_target() {
        // dialect({ default: D, pg: A }) covers ALL dialects: PG via its own leg,
        // SQLite/MySQL via the default. Accepted on every target.
        let sc = TargetScope::structural_only("t");
        let e = dialectal(
            Some(Expr::lit(IrScalar::Int(0))),
            Some(Expr::lit(IrScalar::Str("A".into()))),
            None,
            None,
        );
        for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            assert!(
                validate_expr(&e, d, &sc, 0, None).is_ok(),
                "a default leg covers the {d:?} target"
            );
        }
    }

    #[test]
    fn dialectal_pg_vendor_node_in_pg_leg_validates_on_all_covered_targets() {
        // Regression: the PG-only gate must validate each dialect() leg as the
        // dialect that owns that leg. A PG-vendor node in the pg leg is fine even
        // while validating a SQLite/MySQL target, because those targets render
        // their own portable legs and never render the pg leg.
        let c = cols();
        let sc = scope("users", &c);
        let e = dialectal(
            None,
            Some(Expr::PgColumnSize { expr: Box::new(Expr::col("name")) }),
            Some(Expr::col("name")),
            Some(Expr::col("name")),
        );
        for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            validate_expr(&e, d, &sc, 0, None).unwrap_or_else(|err| {
                panic!("pgColumnSize in the pg leg must validate on covered {d:?}: {err}")
            });
        }
    }

    #[test]
    fn dialectal_pg_vendor_node_in_pg_leg_does_not_cover_missing_mysql_leg() {
        // The per-leg PG-only fix must not weaken the existing coverage rule:
        // pg+sqlite with no default still cannot target MySQL.
        let c = cols();
        let sc = scope("users", &c);
        let e = dialectal(
            None,
            Some(Expr::PgColumnSize { expr: Box::new(Expr::col("name")) }),
            Some(Expr::col("name")),
            None,
        );
        assert!(validate_expr(&e, Dialect::Postgres, &sc, 0, None).is_ok());
        assert!(validate_expr(&e, Dialect::Sqlite, &sc, 0, None).is_ok());
        let err = validate_expr(&e, Dialect::Mysql, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_EXPR_NOT_PORTABLE,
            "a dialect() with no mysql/default leg must still refuse MySQL; got: {err}"
        );
    }

    #[test]
    fn dialectal_default_leg_must_remain_portable() {
        // `default` is not a vendor bucket. It may be selected for any target, so
        // a PG-only node in default is refused even when the current target is PG.
        let c = cols();
        let sc = scope("users", &c);
        let e = dialectal(
            Some(Expr::PgColumnSize { expr: Box::new(Expr::col("name")) }),
            None,
            Some(Expr::col("name")),
            Some(Expr::col("name")),
        );
        for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_expr(&e, d, &sc, 0, None).unwrap_err();
            assert_eq!(
                err.code, CODE_UNSUPPORTED,
                "a PG-only node in default must be refused on {d:?}; got: {err}"
            );
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
    }

    #[test]
    fn dialectal_with_no_legs_is_refused_on_every_target() {
        // dialect({}) — zero legs — is malformed on EVERY target (dialect-neutral
        // CODE_UNSUPPORTED), enforced at validate (serde deserializes the empty
        // node, the structural gate refuses it).
        let sc = TargetScope::structural_only("t");
        let e = dialectal(None, None, None, None);
        for d in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_expr(&e, d, &sc, 0, None).unwrap_err();
            assert_eq!(err.code, CODE_UNSUPPORTED, "legless dialect() refused on {d:?}; got: {err}");
            assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        }
    }

    #[test]
    fn dialectal_recurses_into_every_present_leg() {
        // The scope check must not short-circuit recursion: a malformed nested
        // node in ANY leg rejects, dialect-neutrally, even on a target the leg
        // does not select. Here an unresolved ColRef sits in the (unselected)
        // mysql leg while targeting PG.
        let c = cols();
        let sc = scope("users", &c);
        let e = dialectal(
            Some(Expr::lit(IrScalar::Int(0))),
            Some(Expr::col("name")),
            None,
            Some(Expr::col("ghost")), // not a column on `users`
        );
        let err = validate_expr(&e, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_UNSUPPORTED,
            "an unresolved ColRef in ANY leg must reject (rule c), even off-target; got: {err}"
        );
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }

    #[test]
    fn split_part_non_literal_args_rejected() {
        let c = cols();
        let sc = scope("users", &c);
        // Non-literal delimiter (a column ref) is not portable.
        let e = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![Expr::col("name"), Expr::col("first"), Expr::lit(IrScalar::Int(1))],
        };
        let err = validate_expr(&e, Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }

    // ── item-4 regression: rule (c) ColRef resolution must cover EVERY splitPart
    // arg, on PG too. check_split_part returns Ok early on a Postgres target
    // (the envelope is PG-renderable); but the structural ColRef-resolution walk
    // (rule c) must STILL run over args[1]/args[2]. Before the fix, check_synth
    // recursed only args.first() (the column), so a ColRef to a nonexistent
    // column hidden in the delim/n slot slipped past on PG and deferred the
    // failure to render/execute. RED before walking every arg unconditionally.

    #[test]
    fn split_part_colref_in_delim_slot_rejected_on_pg() {
        let c = cols();
        let sc = scope("users", &c);
        // delim slot is a ColRef to a column NOT on `users` — rule (c) must fire,
        // even on a Postgres target (the structural resolution is dialect-neutral).
        let e = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::col("nonexistent"),
                Expr::lit(IrScalar::Int(1)),
            ],
        };
        let err = validate_expr(&e, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_UNSUPPORTED,
            "an unresolved ColRef in the delim slot must reject on PG (rule c), got: {err}"
        );
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }

    #[test]
    fn split_part_colref_in_n_slot_rejected_on_pg() {
        let c = cols();
        let sc = scope("users", &c);
        // n slot is a ColRef to a nonexistent column — rule (c), on PG.
        let e = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![
                Expr::col("name"),
                Expr::lit(IrScalar::Str(" ".into())),
                Expr::col("ghost"),
            ],
        };
        let err = validate_expr(&e, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED, "got: {err}");
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }

    #[test]
    fn split_part_nested_bad_synth_in_delim_slot_rejected_on_pg() {
        // A NESTED out-of-... no: on PG the inner splitPart envelope is fine, but
        // a nested splitPart with WRONG ARITY (malformed on both dialects) hidden
        // in the delim slot must still be reached by the walk on PG.
        let c = cols();
        let sc = scope("users", &c);
        let bad_inner = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![Expr::col("name")], // arity 1 → malformed on both dialects
        };
        let e = Expr::FnSynth {
            r#fn: SynthFn::SplitPart,
            args: vec![Expr::col("name"), bad_inner, Expr::lit(IrScalar::Int(1))],
        };
        let err = validate_expr(&e, Dialect::Postgres, &sc, 0, None).unwrap_err();
        assert_eq!(
            err.code, CODE_UNSUPPORTED,
            "a malformed nested splitPart in the delim slot must be reached on PG, got: {err}"
        );
    }

    #[test]
    fn validate_ir_rejects_split_part_colref_in_n_slot_on_pg() {
        // The production-path proof: drive a hostile IR through validate_ir on a
        // Postgres target. A createTable Check whose splitPart hides a ColRef to a
        // nonexistent column in the n slot must reject (rule c), not pass on PG.
        let ir = ir_with(vec![Op::CreateTable {
            name: "users".into(),
            columns: vec![IrColumn {
                name: "first".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
            primary_key: None,
            constraints: vec![IrConstraint {
                name: None,
                kind: IrConstraintKind::Check {
                    expr: Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::FnSynth {
                            r#fn: SynthFn::SplitPart,
                            args: vec![
                                Expr::col("first"),
                                Expr::lit(IrScalar::Str(" ".into())),
                                Expr::col("ghost"), // not a column of users
                            ],
                        }),
                    },
                
                    not_valid: None,
                },
            }],
            indexes: vec![],

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED, "got: {err}");
        assert_eq!(err.op_index, 0);
    }

    // ── (c) ColRef resolution against the target table ─────────────────────

    #[test]
    fn colref_on_target_table_validates() {
        let c = cols();
        let sc = scope("users", &c);
        assert!(validate_expr(&Expr::col("name"), Dialect::Postgres, &sc, 0, None).is_ok());
    }

    #[test]
    fn colref_not_on_target_table_rejected() {
        let c = cols();
        let sc = scope("users", &c);
        let err =
            validate_expr(&Expr::col("nope"), Dialect::Postgres, &sc, 3, Some("m.ts:4")).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert_eq!(err.op_index, 3);
        assert!(err.reason.contains("cross-table") || err.reason.contains("does not resolve"));
    }

    #[test]
    fn synthesized_cross_table_reference_is_rejected() {
        // A node a buggy/malicious builder might synthesize: a ColRef carrying a
        // qualified "other.col" name. `c` is single-table-scoped, so "other.col"
        // is not a column on `users` → rejected (cross-table is not expressible).
        let c = cols();
        let sc = scope("users", &c);
        let err = validate_expr(
            &Expr::col("customers.name"),
            Dialect::Postgres,
            &sc,
            0,
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }

    #[test]
    fn structural_only_scope_skips_colref_resolution() {
        let sc = TargetScope::structural_only("users");
        // A col not in any set still validates structurally (resolution deferred).
        assert!(validate_expr(&Expr::col("anything"), Dialect::Sqlite, &sc, 0, None).is_ok());
        // …but an out-of-envelope splitPart STILL rejects (structural).
        let err = validate_expr(&split(", ", 1), Dialect::Sqlite, &sc, 0, None).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }

    // ── validate_ir / validate_op — the SOLE-gate walker over a whole IR ────
    //
    // These pin the §3.3.1.1 obligation that the validator is actually INVOKED
    // over every embedded Expr slot of every Op (the walker did not exist
    // before the code-critic MED fix).

    use crate::model::ir::{
        ColType, IrColumn, IrConstraint, IrConstraintKind, IrIndex, MigrationIr, Op,
        PartitionBoundValue, PartitionBounds, PartitionSpec, SafeI64,
    };
    use std::collections::BTreeMap;

    fn ir_with(ops: Vec<Op>) -> MigrationIr {
        MigrationIr {
            ir_version: 1,
            name: "n".into(),
            owner_app: String::new(),
            ops,
            flags: Default::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        }
    }

    fn op_json(json: &str) -> Op {
        serde_json::from_str(json).expect("test op JSON")
    }

    fn validate_ir_platform(
        ir: &MigrationIr,
        dialect: Dialect,
    ) -> Result<(), AuthoringError> {
        validate_ir_scoped(ir, dialect, &[], None, &PolicyProfile::platform())
    }

    fn part_col(name: &str, ty: ColType, not_null: bool) -> IrColumn {
        IrColumn {
            name: name.into(),
            ty,
            nullable: not_null.then_some(false),
            default: None,
            unique: None,
            id_prefix: None,
            vector_metric: None,
            case_sensitive: None,
            mask: None,
            generated: None,
            identity: None,
        }
    }

    fn idx_col(name: &str) -> IndexElement {
        IndexElement::Column {
            name: name.into(),
            order: None,
            opclass: None,
            collation: None,
        }
    }

    fn unique_idx(columns: &[&str]) -> IrIndex {
        IrIndex {
            name: None,
            columns: columns.iter().map(|name| idx_col(name)).collect(),
            unique: Some(true),
            using: None,
            r#where: None,
            include: Vec::new(),
            with: None,
            only: None,
            nulls_not_distinct: None,
        }
    }

    fn safe_i(value: i64) -> PartitionBoundValue {
        PartitionBoundValue::Int {
            value: SafeI64::new(value).expect("test partition bound is JS-safe"),
        }
    }

    fn str_b(value: &str) -> PartitionBoundValue {
        PartitionBoundValue::String { value: value.into() }
    }

    fn create_parent(
        name: &str,
        spec: PartitionSpec,
        columns: Vec<IrColumn>,
        primary_key: Option<&[&str]>,
        constraints: Vec<IrConstraint>,
        indexes: Vec<IrIndex>,
    ) -> Op {
        Op::CreateTable {
            name: name.into(),
            columns,
            primary_key: primary_key.map(|cols| cols.iter().map(|col| (*col).into()).collect()),
            constraints,
            indexes,
            partition_by: Some(spec),
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn create_part(name: &str, of: &str, bounds: PartitionBounds) -> Op {
        Op::CreatePartition {
            name: name.into(),
            of: of.into(),
            bounds,
            schema: None,
            existence_guard: None,
        }
    }

    fn drop_part(parent: &str, name: &str) -> Op {
        Op::DropPartition {
            parent: parent.into(),
            name: name.into(),
            schema: None,
            existence_guard: None,
            cascade: None,
        }
    }

    #[test]
    fn partitioned_table_without_collapse_is_dialect_unsupported_off_postgres() {
        let ir = ir_with(vec![create_parent(
            "events",
            PartitionSpec::Range { columns: vec!["ts".into()], collapse: false },
            vec![part_col("ts", ColType::Timestamp, true)],
            None,
            vec![],
            vec![],
        )]);

        assert!(validate_ir_platform(&ir, Dialect::Postgres).is_ok());
        let err = validate_ir_platform(&ir, Dialect::Sqlite)
            .expect_err("non-affirmed partitioning must fail closed off Postgres");
        assert_eq!(err.code, CODE_DIALECT_UNSUPPORTED, "got: {err}");
    }

    #[test]
    fn partition_key_coverage_refuses_non_covering_unique_and_accepts_covering() {
        let base_cols = || {
            vec![
                part_col("tenant_id", ColType::Uuid, true),
                part_col("ts", ColType::Timestamp, true),
            ]
        };
        let spec = || PartitionSpec::Range { columns: vec!["ts".into()], collapse: false };

        let bad = ir_with(vec![create_parent(
            "events",
            spec(),
            base_cols(),
            None,
            vec![],
            vec![unique_idx(&["tenant_id"])],
        )]);
        let err = validate_ir_platform(&bad, Dialect::Postgres)
            .expect_err("unique indexes on partitioned parents must cover the key");
        assert_eq!(err.code, CODE_PARTITION_KEY_COVERAGE, "got: {err}");

        let ok = ir_with(vec![create_parent(
            "events",
            spec(),
            base_cols(),
            None,
            vec![],
            vec![unique_idx(&["tenant_id", "ts"])],
        )]);
        assert!(validate_ir_platform(&ok, Dialect::Postgres).is_ok());
    }

    #[test]
    fn collapse_requires_total_range_list_and_hash_bounds() {
        let range_missing_default = ir_with(vec![
            create_parent(
                "events",
                PartitionSpec::Range { columns: vec!["ts".into()], collapse: true },
                vec![part_col("ts", ColType::Timestamp, true)],
                None,
                vec![],
                vec![],
            ),
            create_part(
                "events_0",
                "events",
                PartitionBounds::Range {
                    from: vec![safe_i(0)],
                    to: vec![safe_i(10)],
                },
            ),
        ]);
        let err = validate_ir_platform(&range_missing_default, Dialect::Postgres)
            .expect_err("collapse range without default must refuse");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_NOT_TOTAL, "got: {err}");

        let list_missing_default = ir_with(vec![
            create_parent(
                "orders",
                PartitionSpec::List { columns: vec!["region".into()], collapse: true },
                vec![part_col("region", ColType::Text, true)],
                None,
                vec![],
                vec![],
            ),
            create_part(
                "orders_us",
                "orders",
                PartitionBounds::List { values: vec![str_b("US")] },
            ),
        ]);
        let err = validate_ir_platform(&list_missing_default, Dialect::Postgres)
            .expect_err("collapse list without default must refuse");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_NOT_TOTAL, "got: {err}");

        let hash_partial = ir_with(vec![
            create_parent(
                "sessions",
                PartitionSpec::Hash { columns: vec!["tenant_id".into()], collapse: true },
                vec![part_col("tenant_id", ColType::Uuid, true)],
                None,
                vec![],
                vec![],
            ),
            create_part(
                "sessions_0",
                "sessions",
                PartitionBounds::Hash { modulus: 2, remainder: 0 },
            ),
        ]);
        let err = validate_ir_platform(&hash_partial, Dialect::Postgres)
            .expect_err("collapse hash must cover every residue");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_NOT_TOTAL, "got: {err}");

        let hash_total = ir_with(vec![
            create_parent(
                "sessions",
                PartitionSpec::Hash { columns: vec!["tenant_id".into()], collapse: true },
                vec![part_col("tenant_id", ColType::Uuid, true)],
                None,
                vec![],
                vec![],
            ),
            create_part(
                "sessions_0",
                "sessions",
                PartitionBounds::Hash { modulus: 2, remainder: 0 },
            ),
            create_part(
                "sessions_1",
                "sessions",
                PartitionBounds::Hash { modulus: 2, remainder: 1 },
            ),
        ]);
        assert!(validate_ir_platform(&hash_total, Dialect::Postgres).is_ok());
    }

    #[test]
    fn collapse_hash_child_drop_is_underivable_but_pg_only_hash_drop_is_valid() {
        let parent = |collapse| {
            create_parent(
                "sessions",
                PartitionSpec::Hash { columns: vec!["tenant_id".into()], collapse },
                vec![part_col("tenant_id", ColType::Uuid, true)],
                None,
                vec![],
                vec![],
            )
        };
        let child_0 = || {
            create_part(
                "sessions_0",
                "sessions",
                PartitionBounds::Hash { modulus: 2, remainder: 0 },
            )
        };
        let child_1 = || {
            create_part(
                "sessions_1",
                "sessions",
                PartitionBounds::Hash { modulus: 2, remainder: 1 },
            )
        };

        let collapse_drop = ir_with(vec![
            parent(true),
            child_0(),
            child_1(),
            drop_part("sessions", "sessions_0"),
        ]);
        for dialect in [Dialect::Postgres, Dialect::Sqlite, Dialect::Mysql] {
            let err = validate_ir_platform(&collapse_drop, dialect)
                .expect_err("collapse hash child drop must be recording-level underivable");
            assert_eq!(err.code, CODE_PARTITION_HASH_DROP_UNDERIVABLE, "got: {err}");
        }

        let pg_only_drop =
            ir_with(vec![parent(false), child_0(), child_1(), drop_part("sessions", "sessions_0")]);
        assert!(validate_ir_platform(&pg_only_drop, Dialect::Postgres).is_ok());
    }

    #[test]
    fn collapse_refuses_composite_range_key() {
        let ir = ir_with(vec![
            create_parent(
                "events",
                PartitionSpec::Range {
                    columns: vec!["tenant_id".into(), "ts".into()],
                    collapse: true,
                },
                vec![
                    part_col("tenant_id", ColType::Uuid, true),
                    part_col("ts", ColType::Timestamp, true),
                ],
                None,
                vec![],
                vec![],
            ),
            create_part("events_default", "events", PartitionBounds::Default),
        ]);

        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("range collapse v1 supports one key column");
        assert_eq!(err.code, CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED, "got: {err}");
    }

    #[test]
    fn collapse_refuses_nullable_key_and_later_drop_not_null() {
        let nullable = ir_with(vec![
            create_parent(
                "events",
                PartitionSpec::Range { columns: vec!["ts".into()], collapse: true },
                vec![part_col("ts", ColType::Timestamp, false)],
                None,
                vec![],
                vec![],
            ),
            create_part("events_default", "events", PartitionBounds::Default),
        ]);
        let err = validate_ir_platform(&nullable, Dialect::Postgres)
            .expect_err("collapse partition keys must be not null");
        assert_eq!(err.code, CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE, "got: {err}");

        let dropped_later = ir_with(vec![
            create_parent(
                "events",
                PartitionSpec::Range { columns: vec!["ts".into()], collapse: true },
                vec![part_col("ts", ColType::Timestamp, true)],
                None,
                vec![],
                vec![],
            ),
            create_part("events_default", "events", PartitionBounds::Default),
            Op::DropColumnNotNull {
                table: "events".into(),
                column: "ts".into(),
                schema: None,
                existence_guard: None,
            },
        ]);
        let err = validate_ir_platform(&dropped_later, Dialect::Postgres)
            .expect_err("later dropNotNull on a collapse key must refuse");
        assert_eq!(err.code, CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE, "got: {err}");
    }

    #[test]
    fn partition_bounds_refuse_overlapping_range_and_accept_disjoint() {
        let parent = || {
            create_parent(
                "events",
                PartitionSpec::Range { columns: vec!["bucket".into()], collapse: false },
                vec![part_col("bucket", ColType::Int, true)],
                None,
                vec![],
                vec![],
            )
        };
        let range = |name: &str, from: i64, to: i64| {
            create_part(
                name,
                "events",
                PartitionBounds::Range {
                    from: vec![safe_i(from)],
                    to: vec![safe_i(to)],
                },
            )
        };

        let bad = ir_with(vec![parent(), range("events_a", 0, 10), range("events_b", 5, 20)]);
        let err = validate_ir_platform(&bad, Dialect::Postgres)
            .expect_err("overlapping range siblings must refuse");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_ILL_FORMED, "got: {err}");

        let ok = ir_with(vec![parent(), range("events_a", 0, 10), range("events_b", 10, 20)]);
        assert!(validate_ir_platform(&ok, Dialect::Postgres).is_ok());
    }

    #[test]
    fn partition_bounds_refuse_duplicate_list_value_and_accept_unique() {
        let parent = || {
            create_parent(
                "orders",
                PartitionSpec::List { columns: vec!["region".into()], collapse: false },
                vec![part_col("region", ColType::Text, true)],
                None,
                vec![],
                vec![],
            )
        };

        let bad = ir_with(vec![
            parent(),
            create_part(
                "orders_a",
                "orders",
                PartitionBounds::List { values: vec![str_b("US"), str_b("US")] },
            ),
        ]);
        let err = validate_ir_platform(&bad, Dialect::Postgres)
            .expect_err("duplicate list values must refuse");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_ILL_FORMED, "got: {err}");

        let ok = ir_with(vec![
            parent(),
            create_part(
                "orders_us",
                "orders",
                PartitionBounds::List { values: vec![str_b("US")] },
            ),
            create_part(
                "orders_eu",
                "orders",
                PartitionBounds::List { values: vec![str_b("EU")] },
            ),
        ]);
        assert!(validate_ir_platform(&ok, Dialect::Postgres).is_ok());
    }

    #[test]
    fn partition_bounds_refuse_non_factor_chain_hash_and_accept_factor_chain() {
        let parent = || {
            create_parent(
                "sessions",
                PartitionSpec::Hash { columns: vec!["tenant_id".into()], collapse: false },
                vec![part_col("tenant_id", ColType::Uuid, true)],
                None,
                vec![],
                vec![],
            )
        };

        let bad = ir_with(vec![
            parent(),
            create_part(
                "sessions_2_0",
                "sessions",
                PartitionBounds::Hash { modulus: 2, remainder: 0 },
            ),
            create_part(
                "sessions_3_1",
                "sessions",
                PartitionBounds::Hash { modulus: 3, remainder: 1 },
            ),
        ]);
        let err = validate_ir_platform(&bad, Dialect::Postgres)
            .expect_err("hash moduli must be comparable by divisibility");
        assert_eq!(err.code, CODE_PARTITION_BOUNDS_ILL_FORMED, "got: {err}");

        let ok = ir_with(vec![
            parent(),
            create_part(
                "sessions_2_0",
                "sessions",
                PartitionBounds::Hash { modulus: 2, remainder: 0 },
            ),
            create_part(
                "sessions_4_1",
                "sessions",
                PartitionBounds::Hash { modulus: 4, remainder: 1 },
            ),
        ]);
        assert!(validate_ir_platform(&ok, Dialect::Postgres).is_ok());
    }

    // ── PR10: schema confinement + guard direction + schema-ident safety ────────

    /// CONFINED — an explicit `schema != project_schema` is REFUSED fail-closed at
    /// validate-time with the structured `CROSS_SCHEMA` code (§2.7). RED before the
    /// gate (the op would have lowered cross-schema). An op whose schema EQUALS the
    /// project schema, or omits it, passes.
    #[test]
    fn confined_cross_schema_op_is_refused_at_validate() {
        use crate::model::policy::SchemaScope;
        let cross = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("other_app".into()),
            existence_guard: None,
        }]);
        let scope = SchemaScope::Single("app_a".into());
        let err = validate_ir_scoped(
            &cross,
            Dialect::Postgres,
            &[],
            Some(&scope),
            &PolicyProfile::confined(),
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_CROSS_SCHEMA, "got: {err}");

        // schema == project schema (case-insensitive) passes.
        let same = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("APP_A".into()),
            existence_guard: None,
        }]);
        assert!(validate_ir_scoped(
            &same,
            Dialect::Postgres,
            &[],
            Some(&scope),
            &PolicyProfile::confined(),
        )
        .is_ok());

        // Absent schema passes.
        let none = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: None,
        }]);
        assert!(validate_ir_scoped(
            &none,
            Dialect::Postgres,
            &[],
            Some(&scope),
            &PolicyProfile::confined(),
        )
        .is_ok());
    }

    /// Defaulted public validation (`None` scope) has no project schema available,
    /// so it honors any schema for non-vendor ops; PLATFORM (`Allowlist`) refuses a
    /// schema outside its allow-list (§2.7).
    #[test]
    fn trusted_honors_any_schema_platform_gates_to_allowlist() {
        use crate::model::policy::SchemaScope;
        let foreign = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("anything".into()),
            existence_guard: None,
        }]);
        // Defaulted public validation: permitted for non-vendor schema qualifiers.
        assert!(validate_ir_scoped(
            &foreign,
            Dialect::Postgres,
            &[],
            None,
            &PolicyProfile::confined(),
        )
        .is_ok());
        // Platform allow-list excluding "anything": refused.
        let scope = SchemaScope::Allowlist(vec!["zeroship".into(), "public".into()]);
        let err = validate_ir_scoped(
            &foreign,
            Dialect::Postgres,
            &[],
            Some(&scope),
            &PolicyProfile::platform(),
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_CROSS_SCHEMA);
        // A schema IN the allow-list passes.
        let ok = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("zeroship".into()),
            existence_guard: None,
        }]);
        assert!(validate_ir_scoped(
            &ok,
            Dialect::Postgres,
            &[],
            Some(&scope),
            &PolicyProfile::platform(),
        )
        .is_ok());
    }

    /// A `schema` qualifier that is not a safe bare identifier (injection-shaped) is
    /// REFUSED with `INVALID_SCHEMA_IDENT` — REGARDLESS of profile (§2.7). RED before
    /// `is_safe_schema_ident` guards the author-controlled identifier position.
    #[test]
    fn injection_shaped_schema_ident_is_refused() {
        for bad in ["a\"; DROP TABLE x;--", "1bad", "has space", "", "a-b"] {
            let ir = ir_with(vec![Op::DropTable {
                table: "t".into(),
                cascade: None,
                schema: Some(bad.into()),
                existence_guard: None,
            }]);
            // Even defaulted public validation (None scope) rejects an injection-shaped ident.
            let err = validate_ir_scoped(
                &ir,
                Dialect::Postgres,
                &[],
                None,
                &PolicyProfile::confined(),
            )
            .unwrap_err();
            assert_eq!(err.code, CODE_INVALID_SCHEMA_IDENT, "schema {bad:?} got: {err}");
        }
    }

    /// A guard whose DIRECTION is illegal for the op variant is an authoring error
    /// (`GUARD_DIRECTION`): `ifExists` on a create*/add* op, `ifNotExists` on a
    /// drop*/rename op (§2.7). RED before the legal-direction check.
    #[test]
    fn wrong_direction_existence_guard_is_an_authoring_error() {
        // ifExists on createTable — illegal.
        let bad_create = ir_with(vec![Op::CreateTable {
            name: "t".into(),
            columns: vec![],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: Some(crate::model::ir::ExistenceGuard::IfExists),
        }]);
        let err = validate_ir_scoped(
            &bad_create,
            Dialect::Postgres,
            &[],
            None,
            &PolicyProfile::platform(),
        )
        .unwrap_err();
        assert_eq!(err.code, CODE_GUARD_DIRECTION, "got: {err}");

        // ifNotExists on dropTable — illegal.
        let bad_drop = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: Some(crate::model::ir::ExistenceGuard::IfNotExists),
        }]);
        let err2 = validate_ir_scoped(
            &bad_drop,
            Dialect::Postgres,
            &[],
            None,
            &PolicyProfile::confined(),
        )
        .unwrap_err();
        assert_eq!(err2.code, CODE_GUARD_DIRECTION);

        // The LEGAL directions pass.
        let ok_create = ir_with(vec![Op::CreateTable {
            name: "t".into(),
            columns: vec![],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: Some(crate::model::ir::ExistenceGuard::IfNotExists),
        }]);
        assert!(validate_ir_scoped(
            &ok_create,
            Dialect::Postgres,
            &[],
            None,
            &PolicyProfile::platform(),
        )
        .is_ok());
    }

    #[test]
    fn platform_profile_accepts_create_table_composite_primary_key() {
        let ir = ir_with(vec![op_json(
            r#"{
              "op": "createTable",
              "name": "memberships",
              "columns": [
                { "name": "account_id", "type": "uuid", "nullable": false },
                { "name": "team", "type": "text", "nullable": false }
              ],
              "primaryKey": ["account_id", "team"],
              "constraints": [],
              "indexes": []
            }"#,
        )]);

        validate_ir_scoped(
            &ir,
            Dialect::Postgres,
            &[],
            None,
            &PolicyProfile::platform(),
        )
        .expect("platform profile accepts author-owned composite createTable primaryKey");
    }

    #[test]
    fn platform_profile_accepts_create_table_null_primary_key() {
        let ir = ir_with(vec![op_json(
            r#"{
              "op": "createTable",
              "name": "events",
              "columns": [
                { "name": "stream", "type": "text", "nullable": false },
                { "name": "payload", "type": "json", "nullable": false }
              ],
              "primaryKey": null,
              "constraints": [],
              "indexes": []
            }"#,
        )]);

        validate_ir_scoped(
            &ir,
            Dialect::Postgres,
            &[],
            None,
            &PolicyProfile::platform(),
        )
        .expect("platform profile accepts no primary key");
    }

    #[test]
    fn confined_profile_refuses_create_table_author_primary_key() {
        let ir = ir_with(vec![op_json(
            r#"{
              "op": "createTable",
              "name": "memberships",
              "columns": [
                { "name": "account_id", "type": "uuid", "nullable": false },
                { "name": "team", "type": "text", "nullable": false }
              ],
              "primaryKey": ["account_id", "team"],
              "constraints": [],
              "indexes": []
            }"#,
        )]);

        let err = validate_ir_scoped(
            &ir,
            Dialect::Postgres,
            &[],
            None,
            &PolicyProfile::confined(),
        )
        .expect_err("confined profile must refuse author-owned createTable primaryKey");
        assert_eq!(err.code, CODE_TABLE_SHAPE_POLICY, "got: {err}");
        assert!(
            err.reason.contains("author_primary_key") && err.reason.contains("primaryKey"),
            "policy refusal should name the profile gate, got: {err}"
        );
    }

    #[test]
    fn confined_profile_accepts_resolved_system_shape_create_table() {
        let raw = ir_with(vec![op_json(
            r#"{
              "op": "createTable",
              "name": "users",
              "columns": [
                { "name": "email", "type": "text", "nullable": false }
              ],
              "constraints": [],
              "indexes": []
            }"#,
        )]);
        let confined = PolicyProfile::confined();
        let resolved = crate::model::table_shape::resolve_create_table_policy(&raw, &confined)
            .expect("confined table-shape resolution succeeds");

        validate_ir_scoped(
            &resolved,
            Dialect::Postgres,
            &[],
            None,
            &confined,
        )
        .expect("resolved confined system shape remains valid");
    }

    // (E) MED — ColRef resolution at the apply/render seam. At LOAD the DML scope
    // is structural-only (the live column set is unknown), so an unresolved ColRef
    // PASSES the load walk. At APPLY, `validate_ir_resolved` re-runs the walk with
    // the resolved live columns and REJECTS a ColRef that does not resolve — with
    // the structured (c) error, NOT a raw DB error.
    #[test]
    fn validate_ir_resolved_rejects_unresolved_colref_in_update_set() {
        use std::collections::BTreeMap;
        // An update whose SET RHS references `ghost` — a column that does NOT exist
        // on the live `users` table.
        let ir = ir_with(vec![Op::Update {
            table: "users".into(),
            set: [("name".to_string(), IrValue::Expr(Expr::col("ghost")))]
                .into_iter()
                .collect(),
            r#where: None,
            schema: None,
        }]);

        // At LOAD: structural-only scope ⇒ the unresolved ColRef is NOT caught.
        assert!(
            validate_ir(&ir, Dialect::Postgres, &[]).is_ok(),
            "load-time validation is structural-only for DML (column set unknown)"
        );

        // At APPLY: resolve against the live columns of `users` (no `ghost`).
        let mut live: BTreeMap<String, Vec<String>> = BTreeMap::new();
        live.insert("users".to_string(), vec!["id".to_string(), "name".to_string()]);
        let err = validate_ir_resolved(&ir, Dialect::Postgres, &live, &[])
            .expect_err("an unresolved ColRef must be rejected at the resolved apply seam");
        assert_eq!(err.code, CODE_UNSUPPORTED, "rule (c) failure is structured, not a raw DB error");
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_resolved_rejects_unresolved_colref_in_insert_on_conflict_do_update() {
        use crate::model::ir::{IrOnConflict, IrValue};
        use std::collections::BTreeMap;
        // SA-18: an insert whose ON CONFLICT DO UPDATE assigns an Expr that
        // references `ghost` — a column that does NOT exist on live `users`.
        let mut do_update: BTreeMap<String, IrValue> = BTreeMap::new();
        do_update.insert("name".to_string(), IrValue::Expr(Expr::col("ghost")));
        let ir = ir_with(vec![Op::Insert {
            table: "users".into(),
            columns: vec!["name".into()],
            rows: vec![vec![IrValue::Scalar(crate::model::ir::IrScalar::Str("x".into()))]],
            on_conflict: Some(IrOnConflict { columns: vec!["id".into()], do_update: Some(do_update) }),
            schema: None,
        }]);

        // At LOAD: structural-only ⇒ the unresolved ColRef is NOT caught (this is
        // the asymmetry SA-18 closes — pre-fix the resolved seam also missed it).
        assert!(validate_ir(&ir, Dialect::Postgres, &[]).is_ok());

        // At APPLY: resolve against the live columns of `users` (no `ghost`).
        let mut live: BTreeMap<String, Vec<String>> = BTreeMap::new();
        live.insert("users".to_string(), vec!["id".to_string(), "name".to_string()]);
        let err = validate_ir_resolved(&ir, Dialect::Postgres, &live, &[])
            .expect_err("an unresolved ColRef in DO UPDATE must be rejected at the resolved seam");
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_resolved_accepts_resolvable_colref_in_update_set() {
        use std::collections::BTreeMap;
        // The SAME shape but the ColRef references a column that DOES exist.
        let ir = ir_with(vec![Op::Update {
            table: "users".into(),
            set: [("name".to_string(), IrValue::Expr(Expr::col("name")))]
                .into_iter()
                .collect(),
            r#where: None,
            schema: None,
        }]);
        let mut live: BTreeMap<String, Vec<String>> = BTreeMap::new();
        live.insert("users".to_string(), vec!["id".to_string(), "name".to_string()]);
        assert!(
            validate_ir_resolved(&ir, Dialect::Postgres, &live, &[]).is_ok(),
            "a ColRef that resolves to a live column passes the apply-seam (c) check"
        );
    }

    #[test]
    fn validate_ir_passes_a_clean_migration() {
        let ir = ir_with(vec![
            Op::CreateTable {
                name: "users".into(),
                columns: vec![
                    IrColumn { name: "first".into(), ty: ColType::Text, nullable: None, default: None, unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
                    IrColumn { name: "total".into(), ty: ColType::Int, nullable: None, default: None, unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None },
                ],
                primary_key: None,
                constraints: vec![],
                indexes: vec![IrIndex {
                    name: None,
                    columns: vec![IndexElement::Column {
                        name: "first".into(),
                        order: None,
                        opclass: None,
                        collation: None,
                    }],
                    unique: None,
                    using: None,
                    r#where: Some(Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::col("first")),
                    }),
                include: Vec::new(),
                with: None,
                only: None,
                nulls_not_distinct: None,
                }],

            partition_by: None,

            runtime_options: Default::default(),
                schema: None,
                existence_guard: None,
            },
            Op::Delete {
                table: "users".into(),
                r#where: Expr::lit(IrScalar::Bool(true)),
                limit: None,
                schema: None,
            },
        ]);
        assert!(validate_ir_platform(&ir, Dialect::Postgres).is_ok());
        assert!(validate_ir_platform(&ir, Dialect::Sqlite).is_ok());
    }

    #[test]
    fn validate_ir_rejects_initially_deferred_without_deferrable() {
        let create_table = op_json(
            r#"{
                "op":"createTable",
                "name":"orders",
                "columns":[{"name":"user_id","type":"text"}],
                "constraints":[{
                    "name":"orders_user_fk",
                    "kind":{
                        "kind":"fk",
                        "columns":["user_id"],
                        "referencesTable":"users",
                        "referencesColumns":["id"],
                        "initiallyDeferred":true
                    }
                }]
            }"#,
        );
        let add_constraint = op_json(
            r#"{
                "op":"addConstraint",
                "table":"orders",
                "constraint":{
                    "name":"orders_user_fk",
                    "kind":{
                        "kind":"fk",
                        "columns":["user_id"],
                        "referencesTable":"users",
                        "referencesColumns":["id"],
                        "initiallyDeferred":true
                    }
                }
            }"#,
        );

        for op in [create_table, add_constraint] {
            let ir = ir_with(vec![op]);
            let err = validate_ir_platform(&ir, Dialect::Postgres)
                .expect_err("initiallyDeferred without deferrable must be rejected");
            assert_eq!(err.code, CODE_OP_INVALID);
            assert_eq!(err.reason, "initiallyDeferred requires deferrable");
        }
    }

    #[test]
    fn validate_ir_rejects_sequence_increment_zero() {
        let ir = ir_with(vec![op_json(r#"{"op":"createSequence","name":"s","increment":0}"#)]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_SEQUENCE_OPTION_INVALID);
        assert!(err.reason.contains("increment"));
    }

    #[test]
    fn validate_ir_rejects_sequence_cache_zero() {
        let ir = ir_with(vec![op_json(r#"{"op":"alterSequence","name":"s","cache":0}"#)]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_SEQUENCE_OPTION_INVALID);
        assert!(err.reason.contains("cache"));
    }

    #[test]
    fn validate_ir_rejects_sequence_min_greater_than_max() {
        let ir = ir_with(vec![op_json(
            r#"{"op":"createSequence","name":"s","minValue":10,"maxValue":9}"#,
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_SEQUENCE_OPTION_INVALID);
        assert!(err.reason.contains("minValue"));
    }

    #[test]
    fn validate_ir_create_table_partial_index_resolves_system_fields_in_scope() {
        // The profile resolver materializes the seven platform system fields
        // before validation/lowering. A legitimate soft-delete partial-unique index
        // `WHERE deleted_at IS NULL` references the resolved column and MUST
        // resolve in rule (c) scope, not be rejected.
        let ir = ir_with(vec![Op::CreateTable {
            name: "users".into(),
            columns: vec![IrColumn {
                name: "first".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![IrIndex {
                name: None,
                columns: vec![IndexElement::Column {
                    name: "first".into(),
                    order: None,
                    opclass: None,
                    collation: None,
                }],
                unique: Some(true),
                using: None,
                // the canonical soft-delete partial-unique predicate
                r#where: Some(Expr::UnaryOp {
                    op: UnaryOp::IsNull,
                    operand: Box::new(Expr::col("deleted_at")),
                }),
            include: Vec::new(),
            with: None,
            only: None,
            nulls_not_distinct: None,
            }],

        partition_by: None,

        runtime_options: Default::default(),
            schema: None,
            existence_guard: None,
        }]);
        let ir = crate::model::table_shape::resolve_create_table_policy(
            &ir,
            &crate::model::profile::PolicyProfile::confined(),
        )
        .expect("resolve confined table shape");
        assert!(
            validate_ir(&ir, Dialect::Postgres, &[]).is_ok(),
            "a partial index on `deleted_at` must resolve system fields (PG)"
        );
        assert!(
            validate_ir(&ir, Dialect::Sqlite, &[]).is_ok(),
            "a partial index on `deleted_at` must resolve system fields (SQLite)"
        );
    }

    #[test]
    fn validate_ir_create_table_still_rejects_truly_unknown_column() {
        // The system-field union must NOT loosen the gate for a genuinely unknown
        // column — `ghost` is neither declared nor a system field.
        let ir = ir_with(vec![Op::CreateTable {
            name: "users".into(),
            columns: vec![IrColumn {
                name: "first".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
            primary_key: None,
            constraints: vec![IrConstraint {
                name: None,
                kind: IrConstraintKind::Check {
                    expr: Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::col("ghost")),
                    },
                
                    not_valid: None,
                },
            }],
            indexes: vec![],

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
    }

    #[test]
    fn validate_ir_rejects_check_colref_to_nonexistent_column() {
        // A createTable whose Check references a column NOT on the table — rule
        // (c). The walker resolves the createTable's own columns, so this fails.
        let ir = ir_with(vec![Op::CreateTable {
            name: "users".into(),
            columns: vec![IrColumn {
                name: "first".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, case_sensitive: None, vector_metric: None, mask: None, generated: None, identity: None }],
            primary_key: None,
            constraints: vec![IrConstraint {
                name: None,
                kind: IrConstraintKind::Check {
                    expr: Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::col("ghost")),
                    },
                
                    not_valid: None,
                },
            }],
            indexes: vec![],

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir_platform(&ir, Dialect::Postgres).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_rejects_out_of_envelope_split_part_in_update_set() {
        // The Update is the SECOND op — the walker must stamp op_index = 1, and
        // it must reach the `set` RHS (the splitPart) to reject it.
        let mut set = BTreeMap::new();
        set.insert("name".to_string(), IrValue::Expr(split(", ", 1))); // multi-char delim
        let ir = ir_with(vec![
            Op::DropColumn {
                table: "t".into(),
                column: "x".into(),
                schema: None,
                existence_guard: None,
            },
            Op::Update { table: "users".into(), set, r#where: None, schema: None },
        ]);
        let ts = vec![None, Some("m.ts:9".to_string())];
        let err = validate_ir(&ir, Dialect::Sqlite, &ts).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
        assert_eq!(err.op_index, 1, "the walker must stamp the enclosing op's index");
        assert_eq!(err.ts_location.as_deref(), Some("m.ts:9"));
    }

    #[test]
    fn validate_ir_walks_create_index_where_predicate() {
        // The property-A fix made createIndex.where a closed Expr — the walker
        // must now reach it. An out-of-envelope splitPart there must reject.
        let ir = ir_with(vec![Op::CreateIndex {
            table: "users".into(),
            columns: vec![IndexElement::Column {
                name: "a".into(),
                order: None,
                opclass: None,
                collation: None,
            }],
            name: None,
            unique: None,
            using: None,
            r#where: Some(split(", ", 1)),

        include: Vec::new(),
        with: None,
        only: None,
        nulls_not_distinct: None,
        concurrently: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Sqlite, &[]).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_rejects_aggregate_count_in_index_predicate() {
        // J2b regression: moving aggregates to ExprChain methods makes aggregate
        // nodes type-reachable in immutable/scalar slots. The Rust validator is
        // the authoritative backstop; before this check, this createIndex.where
        // node passed validate cleanly.
        let ir = ir_with(vec![Op::CreateIndex {
            table: "users".into(),
            columns: vec![IndexElement::Column {
                name: "a".into(),
                order: None,
                opclass: None,
                collation: None,
            }],
            name: None,
            unique: None,
            using: None,
            r#where: Some(Expr::Agg {
                func: AggFunc::Count,
                arg: None,
                distinct: false,
            }),
            include: Vec::new(),
            with: None,
            only: None,
            nulls_not_distinct: None,
            concurrently: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Postgres, &[]).unwrap_err();
        assert_eq!(err.code, CODE_AGGREGATE_IN_SCALAR_CONTEXT);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert_eq!(err.op_index, 0);
        assert!(
            err.reason.contains("count()"),
            "reason names offending aggregate: {err}"
        );
        assert!(
            err.reason.contains("index predicate"),
            "reason names scalar context: {err}"
        );
    }

    #[test]
    fn validate_ir_rejects_volatile_now_in_index_predicate() {
        // J3 regression: moving now()/genRandomUuid() to top-level imports makes
        // volatile nodes type-reachable in immutable slots. The Rust validator is
        // the authoritative backstop; before this check, this createIndex.where
        // node passed validate cleanly.
        let ir = ir_with(vec![Op::CreateIndex {
            table: "users".into(),
            columns: vec![IndexElement::Column {
                name: "a".into(),
                order: None,
                opclass: None,
                collation: None,
            }],
            name: None,
            unique: None,
            using: None,
            r#where: Some(Expr::FnSynth {
                r#fn: SynthFn::Now,
                args: vec![],
            }),

        include: Vec::new(),
        with: None,
        only: None,
        nulls_not_distinct: None,
        concurrently: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Postgres, &[]).unwrap_err();
        assert_eq!(err.code, CODE_IMMUTABLE_CONTEXT_VOLATILE);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert_eq!(err.op_index, 0);
        assert!(err.reason.contains("now()"), "reason names offending function: {err}");
        assert!(
            err.reason.contains("index predicate"),
            "reason names immutable context: {err}"
        );
    }

    #[test]
    fn validate_ir_refuses_set_column_type_using_until_expr_renderer_lands() {
        let ir = ir_with(vec![Op::SetColumnType {
            table: "users".into(),
            column: "a".into(),
            to_type: ColType::Int,
            using: Some(split(", ", 1)),
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Sqlite, &[]).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert!(err.reason.contains("setColumnType.using"));
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_walks_backfill_filter_and_set() {
        let mut set = BTreeMap::new();
        set.insert("name".to_string(), IrValue::Expr(Expr::col("first"))); // fine structurally
        let ir = ir_with(vec![Op::Backfill {
            table: "users".into(),
            cursor_column: "id".into(),
            batch_size: serde_json::from_str("100").unwrap(),
            set,
            filter: Some(split(", ", 1)), // out-of-envelope → reject
            name: "bf".into(),
            schema: None,
        }]);
        let err = validate_ir(&ir, Dialect::Sqlite, &[]).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }

    // ── PR5 — the names-stay-strings BINDING corollary (§3.3) ───────────────
    //
    // This is the apply-time HALF of the PR5 guarantee. The OTHER half lives in
    // the JS type-level suite (`sdks/migrate/tests/types/type-tests.ts`): a
    // migration whose table/column NAMES are plain strings type-checks cleanly
    // EVEN WHEN those names are not in the current `@zeroship/db` schema (the
    // anti-rot guarantee — names are NOT live-schema-bound, so an immutable
    // historical migration never rots as the schema evolves).
    //
    // The corollary this test pins: because tsc CANNOT see the name (it is a
    // plain string), a migration that references a NON-EXISTENT column must fail
    // at APPLY — never silently mis-apply — with the STRUCTURED error. Load is
    // structural-only (the name is accepted, mirroring tsc accepting the string),
    // and the resolved apply seam is the SOLE place a bad name is caught.
    #[test]
    fn pr5_nonexistent_column_name_fails_at_apply_not_at_load_with_structured_error() {
        use std::collections::BTreeMap;

        // A migration whose `where` and `set` reference `column_that_was_dropped`
        // — a plain-string name the JS DSL type-checks (it is NOT live-schema-
        // bound) and that does NOT exist on the live `users` table.
        let ir = ir_with(vec![Op::Update {
            table: "users".into(),
            set: [(
                "name".to_string(),
                IrValue::Expr(Expr::col("column_that_was_dropped")),
            )]
                .into_iter()
                .collect(),
            r#where: Some(Expr::BinOp {
                op: BinaryOp::Eq,
                lhs: Box::new(Expr::col("column_that_was_dropped")),
                rhs: Box::new(Expr::lit(IrScalar::Int(1))),
            }),
            schema: None,
        }]);

        // LOAD-time (the tsc-analog): structural-only — the plain-string name is
        // ACCEPTED, exactly as tsc accepts the string literal. NOT rejected here.
        assert!(
            validate_ir(&ir, Dialect::Postgres, &[]).is_ok(),
            "a plain-string column name is accepted at load (the tsc-analog), never name-bound"
        );

        // APPLY-time (resolved against the REAL live columns): the missing name is
        // the SOLE place it is caught — with the STRUCTURED `UNSUPPORTED { expr }`
        // error, not a raw DB \"column does not exist\" surprise.
        let mut live: BTreeMap<String, Vec<String>> = BTreeMap::new();
        live.insert("users".to_string(), vec!["id".to_string(), "name".to_string()]);
        let err = validate_ir_resolved(&ir, Dialect::Postgres, &live, &[])
            .expect_err("a non-existent column name must FAIL at the resolved apply seam");
        assert_eq!(
            err.code, CODE_UNSUPPORTED,
            "the apply-time name reject is structured (the §8.8 envelope), not a raw DB error"
        );
        assert_eq!(
            err.kind,
            Some(UnsupportedKind::Expr),
            "an unknown column is a rule-(c) expr-kind capability-boundary reject"
        );
        assert_eq!(err.op_index, 0, "the structured error attributes the failing op");
    }

    // ── Migration-first P2a — column-facet validate-time bounds (§4) ─────────
    // RED before the `validate_column_facets` wiring: a hand-crafted `.ir.json`
    // carrying a malformed/reserved/over-long id_prefix or a misplaced metric would
    // have passed validate and deferred the blow-up to render / mint colliding ids.

    use crate::model::ir::{EmptyContainerKind, IrJsonValue, VectorMetric};

    /// Build a createTable Op with a single `id` column carrying `id_prefix`.
    fn create_with_id_prefix(prefix: &str) -> Op {
        Op::CreateTable {
            name: "things".into(),
            columns: vec![IrColumn {
                name: "id".into(),
                ty: ColType::Uuid,
                nullable: None,
                default: None,
                unique: None,
                id_prefix: Some(prefix.to_string()),
                case_sensitive: None,
                vector_metric: None, mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn create_with_default(ty: ColType, default: crate::model::ir::IrDefault) -> Op {
        Op::CreateTable {
            name: "docs".into(),
            columns: vec![IrColumn {
                name: "body".into(),
                ty,
                nullable: None,
                default: Some(default),
                unique: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],
            partition_by: None,
            runtime_options: None,
            schema: None,
            existence_guard: None,
        }
    }

    fn json_value_default() -> crate::model::ir::IrDefault {
        crate::model::ir::IrDefault::Json {
            value: IrJsonValue::Object(
                [("a".to_string(), IrJsonValue::Int(1))]
                    .into_iter()
                    .collect(),
            ),
        }
    }

    #[test]
    fn p2a_create_table_accepts_a_valid_id_prefix() {
        let ir = ir_with(vec![create_with_id_prefix("post")]);
        assert!(
            validate_ir_platform(&ir, Dialect::Postgres).is_ok(),
            "a well-formed, unreserved, in-length id prefix must validate"
        );
    }

    #[test]
    fn p2a_create_table_rejects_a_reserved_id_prefix() {
        // `usr` is the platform user-id prefix (RESERVED_ID_PREFIXES); a creator
        // prefix that collides with it would mint ids colliding with platform users.
        let ir = ir_with(vec![create_with_id_prefix("usr")]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("a reserved id prefix must be refused at validate, fail-closed");
        assert_eq!(err.code, CODE_INVALID_ID_PREFIX, "got: {err}");
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn p2a_create_table_rejects_a_malformed_id_prefix() {
        // An upper-case / non-`[a-z0-9_]` prefix is not a valid typed-id segment.
        let ir = ir_with(vec![create_with_id_prefix("Po-st")]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("a malformed id prefix must be refused at validate");
        assert_eq!(err.code, CODE_INVALID_ID_PREFIX, "got: {err}");
    }

    #[test]
    fn p2a_create_table_rejects_an_over_long_id_prefix() {
        // Charset-valid but longer than MAX_ID_PREFIX_LEN — refused so the minted
        // `<prefix>_<22 base62>` typed-id keeps the compact platform shape.
        let ir = ir_with(vec![create_with_id_prefix("toolong")]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("an over-long id prefix must be refused at validate");
        assert_eq!(err.code, CODE_INVALID_ID_PREFIX, "got: {err}");
        assert!(err.reason.contains("maximum"), "the error names the length bound: {err}");
    }

    #[test]
    fn p2a_create_table_rejects_vector_metric_on_non_vector_column() {
        // A metric on a non-Vector column is the co-occurrence violation — the
        // closed enum already bounds the metric token at deserialize; this catches a
        // dead metric a hand-crafted artifact rides in on a text column.
        let ir = ir_with(vec![Op::CreateTable {
            name: "docs".into(),
            columns: vec![IrColumn {
                name: "body".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: Some(VectorMetric::Cosine),
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("a vector_metric on a non-vector column must be refused");
        assert_eq!(err.code, CODE_VECTOR_METRIC_MISPLACED, "got: {err}");
    }

    #[test]
    fn case_sensitive_false_rejects_non_text_columns() {
        for ty in [ColType::Int, ColType::Json] {
            let ir = ir_with(vec![Op::CreateTable {
                name: "docs".into(),
                columns: vec![IrColumn {
                    name: "body".into(),
                    ty,
                    nullable: None,
                    default: None,
                    unique: None,
                    id_prefix: None,
                    case_sensitive: Some(false),
                    vector_metric: None,
                    mask: None,
                    generated: None,
                    identity: None,
                }],
                primary_key: None,
                constraints: vec![],
                indexes: vec![],
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            }]);
            let err = validate_ir_platform(&ir, Dialect::Postgres)
                .expect_err("caseSensitive:false on a non-text column must be refused");
            assert_eq!(err.code, CODE_UNSUPPORTED, "got: {err}");
            assert!(
                err.reason.contains("caseSensitive:false is only valid on a text column"),
                "error should explain the text-only bound: {err}"
            );
        }
    }

    #[test]
    fn container_default_object_on_text_array_is_rejected() {
        let ir = ir_with(vec![create_with_default(
            ColType::TextArray,
            crate::model::ir::IrDefault::Container { kind: EmptyContainerKind::Object },
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("empty object defaults are valid only on json columns");
        assert_eq!(err.code, CODE_COLUMN_DEFAULT_TYPE, "got: {err}");
        assert!(
            err.reason.contains("empty object defaults require json"),
            "error should explain the allowed type: {err}"
        );
    }

    #[test]
    fn container_default_array_on_int_is_rejected() {
        let ir = ir_with(vec![create_with_default(
            ColType::Int,
            crate::model::ir::IrDefault::Container { kind: EmptyContainerKind::Array },
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("empty array defaults are valid only on json/textArray columns");
        assert_eq!(err.code, CODE_COLUMN_DEFAULT_TYPE, "got: {err}");
        assert!(
            err.reason.contains("empty array defaults require json or textArray"),
            "error should explain the allowed types: {err}"
        );
    }

    #[test]
    fn json_value_default_on_int_is_rejected() {
        let ir = ir_with(vec![create_with_default(ColType::Int, json_value_default())]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("JSON value defaults are valid only on json columns");
        assert_eq!(err.code, CODE_COLUMN_DEFAULT_TYPE, "got: {err}");
        assert!(
            err.reason.contains("JSON value defaults are valid only on json columns"),
            "error should explain the json-only bound: {err}"
        );
    }

    #[test]
    fn json_value_default_on_text_array_is_rejected() {
        let ir = ir_with(vec![create_with_default(
            ColType::TextArray,
            json_value_default(),
        )]);
        let err = validate_ir_platform(&ir, Dialect::Postgres)
            .expect_err("JSON value defaults are valid only on json columns");
        assert_eq!(err.code, CODE_COLUMN_DEFAULT_TYPE, "got: {err}");
        assert!(
            err.reason.contains("JSON value defaults are valid only on json columns"),
            "error should explain the json-only bound: {err}"
        );
    }

    #[test]
    fn p2a_create_table_accepts_vector_metric_on_a_vector_column() {
        let ir = ir_with(vec![Op::CreateTable {
            name: "docs".into(),
            columns: vec![IrColumn {
                name: "embedding".into(),
                ty: ColType::Vector { vector: 1536 },
                nullable: None,
                default: None,
                unique: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: Some(VectorMetric::Cosine),
                mask: None,
                generated: None,
                identity: None,
            }],
            primary_key: None,
            constraints: vec![],
            indexes: vec![],

        partition_by: None,

        runtime_options: None,
            schema: None,
            existence_guard: None,
        }]);
        assert!(
            validate_ir_platform(&ir, Dialect::Postgres).is_ok(),
            "a metric on a t.vector(n) column is the legitimate co-occurrence"
        );
    }
}
