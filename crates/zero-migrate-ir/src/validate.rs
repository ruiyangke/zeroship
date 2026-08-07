//! The STRUCTURAL expression-AST validator + the structured-error envelope.
//!
//! The closed expression AST ([`crate::expr::Expr`]) is **constructed in JS and
//! serialized to IR — never parsed from text**. So validation is a
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
//!   enclosing op. A cross-table reference is impossible by
//!   construction (`c` is single-table-scoped), and any reference to a
//!   column not on the target table is a hard error (injection defense + the
//!   capability boundary).
//! - **(d)** a `Cast` target is a portable type — guaranteed by the closed
//!   [`crate::expr::CastTarget`] enum, so this is structurally total.
//!
//! There is **NO lexer, NO Pratt/precedence parser, NO `libpg_query`, NO
//! differential fuzzer** — the parser-drift risk is dissolved, not mitigated. The
//! Rust validator here is the authoritative STRUCTURAL gate (checks (a), (b),
//! (d) — node allow-list, `FnSynth` arity/envelope, portable cast target); the
//! JS side runs an optional best-effort structural hint over the SAME schemars
//! schema. Rule (c) — `ColRef` resolution against the live target table — runs
//! at the apply/render seam (an apply-time check): at IR load the
//! live column set is generally unknown for the DML ops, `setColumnType`,
//! `addConstraint` and `createIndex`, so those positions validate
//! [`TargetScope::structural_only`] here and the seam re-runs the walk with a
//! resolved column set. A self-contained `createTable` DOES resolve (c) against
//! its own declared columns at load.
//!
//! LAYERING EXCEPTION: raw view-body validation calls the guard's read-only
//! body scanner after the structural `SELECT` checks. That scanner is real
//! deny-list security logic, so moving it down into `model` would put guard policy
//! in the data layer. Until a separate analysis pass above `model` + `guard`
//! walks raw view bodies, this is the one deliberate `model -> guard` edge.

use crate::expr::{AggFunc, CaseBranch, Duration, Expr, ScalarFn, SynthFn};
use crate::ir::AlterPrimaryKeyAction;

// ── Canonical authoring-time error codes ────────────────────────────────────
// The taxonomy new validators add their code to. The op-vs-expr distinction is
// carried as the `kind` field on `UNSUPPORTED`, not two top-level codes.

/// An op or expression node the engine cannot render on EITHER dialect — carries
/// `kind: "op" | "expr"`. The validator emits the `"expr"` kind.
pub const CODE_UNSUPPORTED: &str = "UNSUPPORTED";
/// An expression that is *expressible* but out of its portable envelope (e.g. an
/// out-of-envelope `c.fn.splitPart`) — kept distinct from `UNSUPPORTED` because
/// the remedy differs ("stay in-envelope or accept `PgOnly`").
pub const CODE_EXPR_NOT_PORTABLE: &str = "EXPR_NOT_PORTABLE";
/// A `dialect_scope = PgOnly` artifact deployed against a `SQLite` target.
pub const CODE_DIALECT_SCOPE_PGONLY: &str = "DIALECT_SCOPE_PGONLY";
/// An op-function called outside an active recorder — emitted JS-side.
pub const CODE_OP_OUTSIDE_RECORDER: &str = "OP_OUTSIDE_RECORDER";
/// An op is structurally valid JSON but carries an internally inconsistent shape.
pub const CODE_OP_INVALID: &str = "OP_INVALID";
/// An op naming a `schema` the active [`SchemaScope`](crate::policy::SchemaScope)
/// does not permit. The Confined creator profile pins the project schema:
/// an explicit `schema != project_schema` is REFUSED at validate-time, fail-closed,
/// BEFORE lower — additional and EARLIER than the migrator-role 42501 + the
/// parse-guard cross-schema denial (which stay unchanged). The Platform profile
/// permits only its allow-list. The friendly remedy is "drop the qualifier or name
/// the project schema".
pub const CODE_CROSS_SCHEMA: &str = "CROSS_SCHEMA";
/// A `schema` qualifier that is not a safe bare SQL identifier:
/// empty, not alpha/`_`-leading, or carrying a non-`[A-Za-z0-9_]` char. The schema
/// is an author-controlled identifier the engine double-quotes; this rejects an
/// injection-shaped value (`"; DROP …`, embedded quote) at validate-time, before
/// it can reach the render seam.
pub const CODE_INVALID_SCHEMA_IDENT: &str = "INVALID_SCHEMA_IDENT";
/// An existence guard whose DIRECTION is illegal for the op variant:
/// an `ifExists` on a create*/add* op, or an `ifNotExists` on a
/// drop*/rename/alter op. A structured authoring error, not a render-time blow-up.
pub const CODE_GUARD_DIRECTION: &str = "GUARD_DIRECTION";
/// A legacy internal platform `id_prefix` that is not a valid base62-UUIDv7 ID
/// prefix (charset / length) or is in the reserved-prefix
/// deny-list (`usr`, …). The IR's threat model is a hand-crafted IR envelope, so a
/// malformed/reserved prefix is a fail-closed VALIDATE error, not a render-time
/// surprise (it would otherwise mint ids colliding with platform `usr_…` ids).
pub const CODE_INVALID_ID_PREFIX: &str = "INVALID_ID_PREFIX";
/// A TypeID value format carries a prefix that violates the TypeID 0.3 prefix
/// grammar or its 63-byte bound.
pub const CODE_INVALID_TYPE_ID_PREFIX: &str = "INVALID_TYPE_ID_PREFIX";
/// A `vector_metric` carried on a column that is
/// NOT a `ColType::Vector`. The metric is structurally bounded by the closed
/// [`crate::ir::VectorMetric`] enum at deserialize; this is the co-occurrence
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
/// **VENDOR (`zero-migrate`)** — a privileged vendor op (role/grant/RLS/
/// policy/trigger/function/extension/schema/`pgRaw`) whose required
/// [`VendorCapability`](crate::capability::VendorCapability) is NOT granted by the
/// active capability set. The Confined creator/AI posture
/// grants NO vendor capability, so EVERY vendor op is refused fail-closed at
/// validate, BEFORE lower — the first gate. The redundant lower gate
/// (the rendered SQL hits the Confined deny-list) means a future refactor
/// that drops this gate still fails closed.
pub const CODE_VENDOR_OP_DENIED: &str = "VENDOR_OP_DENIED";
/// A `pgRaw` op must carry a non-empty audit reason for using the raw SQL escape.
pub const CODE_PGRAW_REASON_REQUIRED: &str = "PGRAW_REASON_REQUIRED";
/// A primary-key tuple is structurally invalid. This covers both a resolved
/// `createTable.primaryKey` and an explicit [`AlterPrimaryKeyAction`]: empty or
/// duplicated ordered tuples, a no-op replacement, or an invalid declared
/// identity-removal tuple.
pub const CODE_PRIMARY_KEY_INVALID: &str = "PRIMARY_KEY_INVALID";
/// A resolved `createTable` violates the active profile's table-shape policy.
pub const CODE_TABLE_SHAPE_POLICY: &str = "TABLE_SHAPE_POLICY";
/// A dialect target cannot realize an authored construct and no affirmation
/// authorizes a transparent-degradable leg.
pub const CODE_DIALECT_UNSUPPORTED: &str = "DIALECT_UNSUPPORTED";
/// Partition rule 1: unique-enforcing entries on a partitioned table must cover
/// all partition key columns.
pub const CODE_PARTITION_KEY_COVERAGE: &str = "PARTITION_KEY_COVERAGE";
/// Partition rule 2: collapse-affirmed bound sets must be total.
pub const CODE_PARTITION_BOUNDS_NOT_TOTAL: &str = "PARTITION_BOUNDS_NOT_TOTAL";
/// Partition rule 2: v1 range collapse only supports a single partition key.
pub const CODE_PARTITION_COMPOSITE_KEY_UNSUPPORTED: &str = "PARTITION_COMPOSITE_KEY_UNSUPPORTED";
/// Partition rule 2: collapse predicates require two-valued, non-null keys.
pub const CODE_PARTITION_KEY_NULLABLE_UNDER_COLLAPSE: &str =
    "PARTITION_KEY_NULLABLE_UNDER_COLLAPSE";
/// Partition rule 3: sibling bounds must be PG-well-formed.
pub const CODE_PARTITION_BOUNDS_ILL_FORMED: &str = "PARTITION_BOUNDS_ILL_FORMED";
/// Partition tier split: hash child drops have no portable collapse predicate.
pub const CODE_PARTITION_HASH_DROP_UNDERIVABLE: &str = "PARTITION_HASH_DROP_UNDERIVABLE";

/// The maximum byte length a legacy internal platform-ID prefix may carry.
/// Mirrors the internal convention (`usr`/`app`/`ses` are 3
/// chars; the auto-derivation in `plugin-db`'s `system_fields_pass` caps at 4 for
/// collection-derived prefixes). A hand-authored prefix is bounded to the SAME 4
/// so the minted `<prefix>_<22 base62 UUIDv7>` value keeps the compact platform
/// shape. This bound is unrelated to the public TypeID prefix grammar.
pub const MAX_ID_PREFIX_LEN: usize = 4;

/// Validate the dialect-neutral structural contract of an explicit primary-key
/// lifecycle action.
///
/// Live-schema prerequisites (the exact current key, candidate uniqueness,
/// identity facets, and inbound foreign keys) deliberately do not belong here;
/// apply checks them while holding the target-specific lock. This helper only
/// rejects malformed hand-authored IR that cannot represent the public
/// `OrderedColumns` contract faithfully:
///
/// - every ordered tuple is non-empty, names only non-empty columns, and has no
///   duplicate column;
/// - replacement changes the ordered tuple (so it cannot be used as a disguised
///   identity-only synchronization operation);
/// - a present `dropIdentityFrom` tuple is itself ordered/non-empty/unique and is
///   a subset of the exact expected current primary key.
///
/// # Errors
/// Returns a stable human-readable reason suitable for wrapping in an
/// [`AuthoringError`] carrying [`CODE_PRIMARY_KEY_INVALID`].
pub fn validate_alter_primary_key_action(action: &AlterPrimaryKeyAction) -> Result<(), String> {
    fn validate_ordered_columns(columns: &[String], field: &str) -> Result<(), String> {
        if columns.is_empty() {
            return Err(format!("{field} must be a non-empty ordered column tuple"));
        }

        let mut seen = std::collections::BTreeSet::new();
        for (position, column) in columns.iter().enumerate() {
            if column.is_empty() {
                return Err(format!(
                    "{field}[{position}] must be a non-empty column name"
                ));
            }
            if !seen.insert(column.as_str()) {
                return Err(format!("{field} names column {column:?} more than once"));
            }
        }
        Ok(())
    }

    let validate_drop_identity =
        |columns: Option<&[String]>, expected_columns: &[String]| -> Result<(), String> {
            let Some(columns) = columns else {
                return Ok(());
            };
            validate_ordered_columns(columns, "dropIdentityFrom")?;
            let expected = expected_columns
                .iter()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            for column in columns {
                if !expected.contains(column.as_str()) {
                    return Err(format!(
                        "dropIdentityFrom names column {column:?}, which is not in expectedColumns"
                    ));
                }
            }
            Ok(())
        };

    match action {
        AlterPrimaryKeyAction::Add { columns } => validate_ordered_columns(columns, "columns"),
        AlterPrimaryKeyAction::Replace {
            expected_columns,
            columns,
            drop_identity_from,
        } => {
            validate_ordered_columns(expected_columns, "expectedColumns")?;
            validate_ordered_columns(columns, "columns")?;
            if expected_columns == columns {
                return Err(
                    "replace columns must differ from expectedColumns; use replace only to change the primary-key tuple"
                        .to_string(),
                );
            }
            validate_drop_identity(drop_identity_from.as_deref(), expected_columns)
        }
        AlterPrimaryKeyAction::Drop {
            expected_columns,
            drop_identity_from,
        } => {
            validate_ordered_columns(expected_columns, "expectedColumns")?;
            validate_drop_identity(drop_identity_from.as_deref(), expected_columns)
        }
    }
}

#[cfg(test)]
mod alter_primary_key_tests {
    use super::*;

    fn strings(columns: &[&str]) -> Vec<String> {
        columns.iter().map(|column| (*column).to_string()).collect()
    }

    #[test]
    fn lifecycle_action_accepts_exact_ordered_shapes() {
        for action in [
            AlterPrimaryKeyAction::Add {
                columns: strings(&["tenant_id", "order_id"]),
            },
            AlterPrimaryKeyAction::Replace {
                expected_columns: strings(&["id"]),
                columns: strings(&["tenant_id", "order_id"]),
                drop_identity_from: Some(strings(&["id"])),
            },
            AlterPrimaryKeyAction::Replace {
                expected_columns: strings(&["tenant_id", "order_id"]),
                columns: strings(&["id"]),
                drop_identity_from: None,
            },
            AlterPrimaryKeyAction::Drop {
                expected_columns: strings(&["id"]),
                drop_identity_from: Some(strings(&["id"])),
            },
        ] {
            validate_alter_primary_key_action(&action)
                .unwrap_or_else(|error| panic!("valid action {action:?} was rejected: {error}"));
        }
    }

    #[test]
    fn lifecycle_action_rejects_malformed_ordered_tuples() {
        for (action, expected_reason) in [
            (
                AlterPrimaryKeyAction::Add { columns: vec![] },
                "columns must be a non-empty",
            ),
            (
                AlterPrimaryKeyAction::Add {
                    columns: strings(&["id", "id"]),
                },
                "more than once",
            ),
            (
                AlterPrimaryKeyAction::Replace {
                    expected_columns: strings(&["id"]),
                    columns: strings(&[""]),
                    drop_identity_from: None,
                },
                "must be a non-empty column name",
            ),
            (
                AlterPrimaryKeyAction::Drop {
                    expected_columns: strings(&["id"]),
                    drop_identity_from: Some(vec![]),
                },
                "dropIdentityFrom must be a non-empty",
            ),
        ] {
            let error = validate_alter_primary_key_action(&action).unwrap_err();
            assert!(
                error.contains(expected_reason),
                "{action:?} returned unexpected reason {error:?}"
            );
        }
    }

    #[test]
    fn replace_rejects_noop_and_identity_columns_outside_expected_key() {
        let noop = AlterPrimaryKeyAction::Replace {
            expected_columns: strings(&["tenant_id", "order_id"]),
            columns: strings(&["tenant_id", "order_id"]),
            drop_identity_from: None,
        };
        assert!(validate_alter_primary_key_action(&noop)
            .unwrap_err()
            .contains("must differ"));

        for action in [
            AlterPrimaryKeyAction::Replace {
                expected_columns: strings(&["id"]),
                columns: strings(&["tenant_id", "order_id"]),
                drop_identity_from: Some(strings(&["other"])),
            },
            AlterPrimaryKeyAction::Drop {
                expected_columns: strings(&["id"]),
                drop_identity_from: Some(strings(&["other"])),
            },
        ] {
            assert!(
                validate_alter_primary_key_action(&action)
                    .unwrap_err()
                    .contains("not in expectedColumns"),
                "{action:?} must reject an unrelated identity column"
            );
        }
    }
}

/// The dialect a structured rejection pertains to (the `dialect` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Postgres.
    Postgres,
    /// `SQLite`.
    Sqlite,
    /// `MySQL`.
    Mysql,
}

impl Dialect {
    /// The lower-case wire spelling used in the structured payload.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Sqlite => "sqlite",
            Self::Mysql => "mysql",
        }
    }
}

/// The `UNSUPPORTED { kind }` discriminant — an internal op-vs-expr
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
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Op => "op",
            Self::Expr => "expr",
            Self::VirtualColumn => "virtualColumn",
            Self::Identity => "identity",
        }
    }
}

/// The machine-readable authoring-time rejection envelope.
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
    /// recorder attributed one. Carried through from the validator caller.
    pub ts_location: Option<String>,
    /// The dialect the rejection pertains to.
    pub dialect: Dialect,
    /// A precise human-readable reason.
    pub reason: String,
    /// A concrete remedy the AI loop can act on (leads the human rendering).
    pub suggested_fix: Option<String>,
}

impl AuthoringError {
    /// Serialize to the canonical structured-error JSON object, with
    /// `suggested_fix` FIRST so a human rendering leads with it.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        if let Some(fix) = &self.suggested_fix {
            map.insert(
                "suggested_fix".into(),
                serde_json::Value::String(fix.clone()),
            );
        }
        map.insert("code".into(), serde_json::Value::String(self.code.clone()));
        if let Some(kind) = self.kind {
            map.insert(
                "kind".into(),
                serde_json::Value::String(kind.as_str().into()),
            );
        }
        map.insert("op_index".into(), serde_json::Value::from(self.op_index));
        if let Some(loc) = &self.ts_location {
            map.insert("ts_location".into(), serde_json::Value::String(loc.clone()));
        }
        map.insert(
            "dialect".into(),
            serde_json::Value::String(self.dialect.as_str().into()),
        );
        map.insert(
            "reason".into(),
            serde_json::Value::String(self.reason.clone()),
        );
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
/// bound — `~17 KB` at `n=8`).
pub const SPLIT_PART_MAX_N: i64 = 8;

/// The single-target-table scope a transform validates against.
///
/// Carries the target table name + its valid column set. The caller (the
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
    pub const fn new(table: &'a str, columns: &'a [String]) -> Self {
        Self {
            table,
            columns: Some(columns),
        }
    }

    /// A scope that does NOT resolve `ColRef`s (structural-only validation).
    #[must_use]
    pub const fn structural_only(table: &'a str) -> Self {
        Self {
            table,
            columns: None,
        }
    }
}

/// Validate one expression-AST tree structurally for a `target_dialect` against a
/// `scope`. Returns the first [`AuthoringError`] or `Ok(())`.
///
/// `op_index` / `ts_location` are stamped onto any emitted error so the AI loop
/// gets the structured-error payload.
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
    let ctx = Ctx {
        target_dialect,
        scope,
        op_index,
        ts_location,
    };
    ctx.walk(expr)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExprVolatility {
    Immutable,
    Stable,
    Volatile,
}

const fn scalar_fn_volatility(f: ScalarFn) -> ExprVolatility {
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

const fn synth_fn_volatility(f: SynthFn) -> ExprVolatility {
    match f {
        SynthFn::Now => ExprVolatility::Volatile,
        SynthFn::ConcatWs | SynthFn::SplitPart => ExprVolatility::Immutable,
    }
}

const fn scalar_fn_name(f: ScalarFn) -> &'static str {
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

const fn synth_fn_name(f: SynthFn) -> &'static str {
    match f {
        SynthFn::ConcatWs => "concatWs",
        SynthFn::SplitPart => "splitPart",
        SynthFn::Now => "now",
    }
}

const fn agg_func_name(f: AggFunc) -> &'static str {
    match f {
        AggFunc::Count => "count",
        AggFunc::Sum => "sum",
        AggFunc::Avg => "avg",
        AggFunc::Min => "min",
        AggFunc::Max => "max",
        AggFunc::StringAgg => "stringAgg",
        AggFunc::ArrayAgg => "arrayAgg",
        AggFunc::BoolAnd => "boolAnd",
        AggFunc::BoolOr => "boolOr",
    }
}

const fn agg_func_is_pg_first(f: AggFunc) -> bool {
    matches!(
        f,
        AggFunc::StringAgg | AggFunc::ArrayAgg | AggFunc::BoolAnd | AggFunc::BoolOr
    )
}

fn first_aggregate(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::ColRef { .. }
        | Expr::Literal { .. }
        | Expr::UuidV4
        | Expr::UuidV7
        | Expr::PgInterval { .. } => None,
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
        Expr::Agg {
            func,
            arg,
            delimiter,
            ..
        } => arg
            .as_deref()
            .and_then(first_aggregate)
            .or_else(|| delimiter.as_deref().and_then(first_aggregate))
            .or_else(|| Some(agg_func_name(*func))),
        Expr::InList { expr, .. } => first_aggregate(expr),
        Expr::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => [
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
        Expr::UuidV4 => Some("uuidV4"),
        Expr::UuidV7 => Some("uuidV7"),
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
        Expr::Agg { arg, delimiter, .. } => arg
            .as_deref()
            .and_then(first_volatile_function)
            .or_else(|| delimiter.as_deref().and_then(first_volatile_function)),
        Expr::InList { expr, .. } => first_volatile_function(expr),
        Expr::Dialectal {
            default,
            pg,
            sqlite,
            mysql,
        } => [
            default.as_deref(),
            pg.as_deref(),
            sqlite.as_deref(),
            mysql.as_deref(),
        ]
        .into_iter()
        .flatten()
        .find_map(first_volatile_function),
    }
}

pub fn validate_immutable_expr_context(
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

pub fn validate_no_aggregate_expr_context(
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

// ── The structural expression-AST walker (policy-free) ─────────────────────

/// The maximum expression-AST nesting [`validate_expr`]'s walker will descend
/// before refusing the tree as a [`CODE_UNSUPPORTED`] `DoS` guard. The bound is
/// OWNED by the validator (an explicit counter) rather than left implicit to
/// `serde_json`'s compile-time `recursion_limit` (~128) at deserialize: a future
/// switch to a streaming/custom deserializer, or a raised serde limit, would
/// otherwise silently expose a stack-overflow on a deeply-nested hostile
/// IR envelope. `128` is comfortably below any realistic legitimate nesting and
/// matches serde's own default so it never narrows the accepted set in practice.
pub const MAX_EXPR_DEPTH: u32 = 128;

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

    fn walk(&self, expr: &Expr) -> Result<(), AuthoringError> {
        self.walk_depth(expr, 0)
    }

    fn walk_depth(&self, expr: &Expr, depth: u32) -> Result<(), AuthoringError> {
        if depth >= MAX_EXPR_DEPTH {
            return Err(self.err(
                CODE_UNSUPPORTED,
                Some(UnsupportedKind::Expr),
                self.target_dialect,
                format!(
                    "expression nesting exceeds the maximum supported depth ({MAX_EXPR_DEPTH}); \
                     flatten the expression"
                ),
                Some("reduce the nesting depth of this expression".to_string()),
            ));
        }
        let d = depth + 1;
        match expr {
            // Unqualified ref: resolve against the enclosing single target table
            // (rule (c)). Qualified ref (`c("t","col")`): the full
            // "qualified-ref table must be in the FROM set" scope check
            // (`QUALIFIED_REF_UNKNOWN_TABLE`) is coupled with the view/FROM
            // builder; for this additive slice accept the qualified form
            // structurally (lenient pass).
            Expr::ColRef { name, table } => match table {
                Some(_) => Ok(()),
                None => self.check_colref(name),
            },
            Expr::Literal { .. } | Expr::UuidV4 => Ok(()),
            Expr::UuidV7 => self.check_uuid_v7_generation(),
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
            // two members are PG-only VENDOR scalars:
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
            // Portable predicate nodes: between/like/distinctFrom render on
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
            // Aggregate node: count/sum/avg/min/max render on all three
            // dialects. The long-tail aggregate variants are PostgreSQL-first and
            // fail closed off-PG unless wrapped in dialect({...}).
            Expr::Agg {
                func,
                arg,
                delimiter,
                distinct: _,
            } => self.check_agg(*func, arg.as_deref(), delimiter.as_deref(), d),
            Expr::InList {
                expr,
                elems,
                negated: _,
            } => self.check_in_list(expr, elems, d),
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
            // The one Layer-2 portability escape: a per-dialect value
            // divergence. Structurally validate EVERY present leg (dialect-
            // neutral), then apply the per-TARGET scope math (own leg OR default).
            Expr::Dialectal {
                default,
                pg,
                sqlite,
                mysql,
            } => self.check_dialectal(default, pg, sqlite, mysql, d),
        }
    }

    /// Validate an [`Expr::Dialectal`] — the `dialect({ default?, pg?, sqlite?,
    /// mysql? })` Layer-2 escape. Three checks, in order:
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
    /// RATCHET: each leg is one of the four ratcheted budget
    /// counters. The budget mechanism does not exist, so the per-leg count has
    /// nothing to feed and this is not gated here.
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

    const fn with_target_dialect(&self, target_dialect: Dialect) -> Ctx<'_> {
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
        self.with_target_dialect(target_dialect)
            .walk_depth(expr, depth)
    }

    fn walk_depth_portable_default(&self, expr: &Expr, depth: u32) -> Result<(), AuthoringError> {
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
    /// IR envelope carrying e.g. `FnSynth{fn:now, args:[…]}` or a zero-arg
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
            // now() is a NULLARY apply-time scalar: no args. A non-nullary call
            // is genuinely MALFORMED on every dialect, so it is an unconditional
            // CODE_UNSUPPORTED, never a dialect-gated portability reject.
            SynthFn::Now => {
                if !args.is_empty() {
                    return Err(self.malformed_synth_err(format!(
                        "c.fn.{} takes no arguments; got {}",
                        match f {
                            SynthFn::Now => "now",
                            _ => unreachable!(),
                        },
                        args.len()
                    )));
                }
                Ok(())
            }
            // concatWs(delim, value, …): a delimiter + at least one value. Fewer
            // than two args is a genuinely-malformed join on EITHER dialect →
            // unconditional CODE_UNSUPPORTED.
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
    /// (`now(arg)`, `concatWs` with <2 args, `splitPart`
    /// with the wrong arity). This is NOT a portability boundary: there is no
    /// dialect on which it renders, so it is an unconditional
    /// [`CODE_UNSUPPORTED`] (`kind:"expr"`), independent of `target_dialect`.
    /// Distinct from [`Self::split_part_envelope_err`], which is the
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
                 (now takes no args; concatWs takes a delimiter + \
                 >=1 value; splitPart takes exactly (column, delim, n))"
                    .to_string(),
            ),
        )
    }

    /// A splitPart **portability-boundary** reject: the call is well-formed and
    /// PG-renderable (`split_part` accepts it), but OUT of the pinned `SQLite`
    /// envelope. It is therefore a hard error ONLY on the `SQLite` leg and
    /// loads fine on a Postgres target — the loads-on-PG/rejected-on-SQLite
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
    /// the payload's `dialect` is faithful to the deploy. `CODE_EXPR_NOT_PORTABLE` (the
    /// structured envelope), the AI loop's primary structured-feedback signal.
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
    /// PG-renderable (`concat_ws` takes any expression delimiter), but the `SQLite`
    /// lowering's literal-delimiter head-trim assumption is violated by a
    /// non-literal delimiter. Like [`Self::split_part_envelope_err`], it is a hard
    /// error ONLY on the `SQLite` leg; the caller only reaches it when
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
        if matches!(self.target_dialect, Dialect::Postgres) {
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

    /// Database UUIDv7 generation currently has one exact lowering:
    /// PostgreSQL 18+'s native `uuidv7()`. MySQL and SQLite must fail before
    /// rendering rather than substituting UUIDv1, UUIDv4, or an engine-side
    /// value. PostgreSQL server-version validation remains an apply capability
    /// concern because the structural IR validator has no live server version.
    fn check_uuid_v7_generation(&self) -> Result<(), AuthoringError> {
        if matches!(self.target_dialect, Dialect::Postgres) {
            return Ok(());
        }
        Err(self.err(
            CODE_EXPR_NOT_PORTABLE,
            None,
            self.target_dialect,
            "uuidV7 database generation requires PostgreSQL 18+; MySQL and SQLite have no exact database UUIDv7 generator"
                .to_string(),
            Some(
                "use an externally supplied UUIDv7 on this target, or use uuidV4() when database-generated random UUIDs are acceptable"
                    .to_string(),
            ),
        ))
    }

    fn check_pg_or_mysql_expr(&self, name: &'static str) -> Result<(), AuthoringError> {
        if matches!(self.target_dialect, Dialect::Postgres | Dialect::Mysql) {
            return Ok(());
        }
        Err(self.err(
            CODE_DIALECT_UNSUPPORTED,
            Some(UnsupportedKind::Expr),
            self.target_dialect,
            format!("{name} is supported on PostgreSQL and MySQL, but SQLite has no stock REGEXP"),
            Some(
                "use dialect({ pg: ..., sqlite: ..., mysql: ... }) to provide an explicit \
                 SQLite leg, or avoid regex on SQLite"
                    .to_string(),
            ),
        ))
    }

    fn check_pg_first_aggregate(&self, name: &'static str) -> Result<(), AuthoringError> {
        if matches!(self.target_dialect, Dialect::Postgres) {
            return Ok(());
        }
        Err(self.err(
            CODE_DIALECT_UNSUPPORTED,
            Some(UnsupportedKind::Expr),
            self.target_dialect,
            format!("{name} aggregate is PostgreSQL-first and has no native SQLite/MySQL renderer"),
            Some(
                "wrap this aggregate in dialect({ pg: ..., sqlite: ..., mysql: ... }) with \
                 explicit non-Postgres legs, or target Postgres only"
                    .to_string(),
            ),
        ))
    }

    fn check_agg(
        &self,
        func: AggFunc,
        arg: Option<&Expr>,
        delimiter: Option<&Expr>,
        depth: u32,
    ) -> Result<(), AuthoringError> {
        if agg_func_is_pg_first(func) {
            self.check_pg_first_aggregate(agg_func_name(func))?;
        }
        if let Some(arg) = arg {
            self.walk_depth(arg, depth)?;
        } else if !matches!(func, AggFunc::Count) {
            return Err(self.err(
                CODE_UNSUPPORTED,
                Some(UnsupportedKind::Expr),
                self.target_dialect,
                format!("{} aggregate requires an argument", agg_func_name(func)),
                Some(
                    "call the aggregate as a receiver chain method, e.g. col(\"x\").arrayAgg()"
                        .to_string(),
                ),
            ));
        }
        match (func, delimiter) {
            (AggFunc::StringAgg, Some(delimiter)) => self.walk_depth(delimiter, depth),
            (AggFunc::StringAgg, None) => Err(self.err(
                CODE_UNSUPPORTED,
                Some(UnsupportedKind::Expr),
                self.target_dialect,
                "stringAgg aggregate requires a delimiter".to_string(),
                Some(
                    "call col(\"x\").stringAgg(delimiter) with a string or expression delimiter"
                        .to_string(),
                ),
            )),
            (_, Some(_)) => Err(self.err(
                CODE_UNSUPPORTED,
                Some(UnsupportedKind::Expr),
                self.target_dialect,
                "aggregate delimiter is only valid for stringAgg".to_string(),
                Some("remove delimiter unless func is stringAgg".to_string()),
            )),
            (_, None) => Ok(()),
        }
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
        elems: &[crate::ir::IrScalar],
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
                crate::ir::IrScalar::Str(s) => {
                    self.check_pg_text_literal(s, "inList element")?;
                    ElemKind::Text
                }
                crate::ir::IrScalar::Int(_)
                | crate::ir::IrScalar::Int64(_)
                | crate::ir::IrScalar::Decimal(_) => ElemKind::Number,
                crate::ir::IrScalar::Bool(_) => ElemKind::Bool,
                crate::ir::IrScalar::Null => ElemKind::Null,
                crate::ir::IrScalar::Bytes(_) => {
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
        self.check_pg_or_mysql_expr("regex match")?;
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
        // unconditional CODE_UNSUPPORTED, NOT a dialect-gated envelope reject.
        // The caller (`check_synth`) already checks arity before the arg
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
        //    validator (the AI loop's structured-feedback signal) rejects it
        //    here rather than deferring to render time. We capture the validated
        //    string/int so the SQLite ENVELOPE checks below need not re-match.
        let delim = match &args[1] {
            Expr::Literal {
                value: crate::ir::IrScalar::Str(s),
            } => s,
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
            Expr::Literal {
                value: crate::ir::IrScalar::Int(n),
            } => {
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
        //    SQLite envelope. On a POSTGRES target the node loads fine; only a
        //    SQLITE target rejects it.
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
