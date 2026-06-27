//! The STRUCTURAL expression-AST validator + the structured-error envelope
//! (design §3.3.1.1 / §8.8).
//!
//! The closed expression AST ([`crate::expr::Expr`]) is **constructed in JS and
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
//!   [`crate::expr::CastTarget`] enum, so this is structurally total.
//!
//! There is **NO lexer, NO Pratt/precedence parser, NO `libpg_query`, NO
//! differential fuzzer** — HIGH-1 is dissolved, not mitigated (§3.3.1.1). The
//! Rust validator here is the authoritative STRUCTURAL gate (checks (a), (b),
//! (d) — node allow-list, `FnSynth` arity/envelope, portable cast target); the
//! JS side runs an optional best-effort structural hint over the SAME schemars
//! schema. Rule (c) — `ColRef` resolution against the live target table — runs
//! at the apply/render seam (§3.3.1.1(c) is an apply-time check): at IR load the
//! live column set is generally unknown for the DML ops, `alterColumnType`,
//! `addConstraint` and `createIndex`, so those positions validate
//! [`TargetScope::structural_only`] here and the seam re-runs the walk with a
//! resolved column set. A self-contained `createTable` DOES resolve (c) against
//! its own declared columns at load.

use crate::expr::{CaseBranch, Expr, SynthFn};
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
/// **PR10** — an op naming a `schema` the active [`SchemaScope`](crate::guard::SchemaScope)
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
/// [`crate::ir::VectorMetric`] enum at deserialize; this is the co-occurrence
/// rule (the metric is meaningless without a vector type, and would otherwise be
/// a silent dead field a hand-crafted artifact could ride in on).
pub const CODE_VECTOR_METRIC_MISPLACED: &str = "VECTOR_METRIC_MISPLACED";
/// A column carries mutually-exclusive facets (`default` + `generated`,
/// `identity` + `generated`, etc.).
pub const CODE_COLUMN_FACET_CONFLICT: &str = "COLUMN_FACET_CONFLICT";
/// **VENDOR (`@zeroship/migrate/pg`)** — a privileged vendor op (role/grant/RLS/
/// policy/trigger/function/extension/schema/`pg.sql`) whose required
/// [`VendorCapability`](crate::capability::VendorCapability) is NOT granted by the
/// active capability set (vendor spec §3.2). The Confined creator/AI posture
/// grants NO vendor capability, so EVERY vendor op is refused fail-closed at
/// validate, BEFORE lower — the #1 invariant (gate 1). The redundant lower gate
/// (gate 2 — the rendered SQL hits the Confined deny-list) means a future refactor
/// that drops this gate still fails closed.
pub const CODE_VENDOR_OP_DENIED: &str = "VENDOR_OP_DENIED";

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

/// Walk an entire [`MigrationIr`](crate::ir::MigrationIr) and validate EVERY
/// embedded expression-AST node against `target_dialect` — the §3.3.1.1 "the
/// Rust validator is the authoritative STRUCTURAL gate" obligation made
/// operative. Checks (a)/(b)/(d) run at load for every Expr slot; check (c)
/// (`ColRef` resolution) runs here only for a self-contained `createTable`, and
/// otherwise at the apply/render seam (see the module note).
///
/// This is the walker that enumerates each [`Op`](crate::ir::Op) variant's
/// expression positions and calls [`validate_expr`] per node with the enclosing
/// op's index + single target table as scope:
///
/// - `createTable` — each `IrIndex.where` partial-index predicate + each
///   `Check` constraint `expr` (scoped to the table's own declared columns, so
///   rule (c) `ColRef` resolution runs against them).
/// - `createIndex` — the `where` partial-index predicate (closed AST since the
///   property-A fix).
/// - `alterColumnType` — the `using` cast expression (closed AST since the
///   property-A fix).
/// - `addConstraint` — a `Check` constraint `expr`.
/// - `update` — every `set` RHS + the optional `where`.
/// - `delete` — the mandatory `where`.
/// - `backfill` — every `set` RHS + the optional `filter`.
///
/// Ops with no expression slot (e.g. `dropTable`, `addColumn`, `insert`) walk to
/// `Ok(())`. For the DML ops (`update`/`delete`/`backfill`) and `alterColumnType`
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
    ir: &crate::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    validate_ir_scoped(ir, target_dialect, ts_locations, None)
}

/// **PR10** — [`validate_ir`] threaded with the active schema confinement scope
/// (§2.7). `schema_scope`:
/// - `None` ⇒ the **Trusted** posture (the public dbmate-like CLI): NO cross-schema
///   confinement (the operator owns the DB). The legal-direction + schema-ident
///   checks STILL run (they are trust-independent safety/authoring checks).
/// - `Some(SchemaScope::Single(project_schema))` ⇒ the **Confined** creator
///   profile: an explicit `schema != project_schema` is REFUSED fail-closed
///   ([`CODE_CROSS_SCHEMA`]).
/// - `Some(SchemaScope::Allowlist([...]))` ⇒ the **Platform** profile: an explicit
///   `schema` must be a member of the allow-list.
///
/// # Errors
/// The first [`AuthoringError`] any op produces (cross-schema, invalid schema ident,
/// illegal guard direction, or an embedded-expression rejection).
pub fn validate_ir_scoped(
    ir: &crate::ir::MigrationIr,
    target_dialect: Dialect,
    ts_locations: &[Option<String>],
    schema_scope: Option<&crate::guard::SchemaScope>,
) -> Result<(), AuthoringError> {
    for (op_index, op) in ir.ops.iter().enumerate() {
        let ts = ts_locations.get(op_index).and_then(Option::as_deref);
        validate_op_scoped(op, target_dialect, op_index, ts, schema_scope)?;
    }
    Ok(())
}

/// Validate every expression slot of a single [`Op`](crate::ir::Op) at
/// `op_index`. The per-variant Expr enumeration the SOLE-gate property needs;
/// see [`validate_ir`] for the slot map.
///
/// # Errors
/// Returns the first [`AuthoringError`] any embedded expression produces.
pub fn validate_op(
    op: &crate::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    // The bare entry keeps the Trusted posture (no cross-schema confinement); the
    // schema-ident + guard-direction checks still run (trust-independent).
    validate_op_scoped(op, target_dialect, op_index, ts_location, None)
}

/// **PR10** — [`validate_op`] threaded with the active
/// [`SchemaScope`](crate::guard::SchemaScope) (§2.7). Runs the schema/guard gate
/// FIRST, then the per-op expression-slot checks.
///
/// # Errors
/// Returns the first [`AuthoringError`] the gate or any embedded expression produces.
pub fn validate_op_scoped(
    op: &crate::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::guard::SchemaScope>,
) -> Result<(), AuthoringError> {
    use crate::ir::{IrConstraintKind, Op, TriggerAction, ViewQuery};

    // **PR10** — schema confinement + guard-direction gate, BEFORE any expression
    // walk. Fail-closed: a Confined cross-schema op never reaches lower.
    validate_op_schema_and_guard(op, target_dialect, op_index, ts_location, schema_scope)?;

    // **VENDOR (`@zeroship/migrate/pg`)** — the capability-composition gate (vendor
    // spec §3.2 gate 1), BEFORE any expression walk. A privileged vendor op is
    // refused fail-closed when (a) the target is SQLite (every vendor op is
    // `PgOnly`, §4.3), or (b) the active capability set — derived from the threaded
    // [`SchemaScope`] — does not GRANT the op's required capability. The Confined
    // creator/AI posture (`Single` scope) grants nothing, so every vendor op dies
    // here; Platform/Trusted (`Allowlist`/`None`) grant the operator preset.
    validate_vendor_op(op, target_dialect, op_index, ts_location, schema_scope)?;

    // A `Check` constraint's expr validates against the given scope.
    let check_constraint =
        |kind: &IrConstraintKind, scope: &TargetScope<'_>| -> Result<(), AuthoringError> {
            if let IrConstraintKind::Check { expr } = kind {
                validate_expr(expr, target_dialect, scope, op_index, ts_location)?;
            }
            Ok(())
        };

    match op {
        Op::CreateTable { name, columns, constraints, indexes, .. } => {
            // A createTable is self-contained: resolve ColRefs against its own
            // declared columns PLUS the seven platform system fields the engine
            // auto-injects at lower/render time (`declarative::SYSTEM_FIELD_NAMES`).
            // A legitimate Check or partial-index predicate referencing a system
            // field — e.g. the canonical soft-delete partial-unique index
            // `WHERE deleted_at IS NULL`, or a Check on `id`/`created_at` — must
            // resolve, not be falsely rejected (rule (c) is enforceable here).
            let mut cols: Vec<String> =
                crate::declarative::SYSTEM_FIELD_NAMES.iter().map(|s| (*s).to_string()).collect();
            cols.extend(columns.iter().map(|c| c.name.clone()));
            let scope = TargetScope::new(name, &cols);
            for ix in indexes {
                if let Some(pred) = &ix.r#where {
                    validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
                }
            }
            for c in constraints {
                check_constraint(&c.kind, &scope)?;
            }
            let pk_cols = constraints.iter().find_map(|c| match &c.kind {
                IrConstraintKind::Pk { columns } => Some(columns.as_slice()),
                _ => None,
            });
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
        Op::CreateIndex { table, r#where, .. } => {
            // The partial-index predicate. The live column set is not known at
            // load (the table pre-exists), so structural-only here.
            if let Some(pred) = r#where {
                let scope = TargetScope::structural_only(table);
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        Op::AlterColumnType { table, using, .. } => {
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
        Op::Update { table, set, r#where, .. } => {
            let scope = TargetScope::structural_only(table);
            for rhs in set.values() {
                validate_expr(rhs, target_dialect, &scope, op_index, ts_location)?;
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
            for rhs in set.values() {
                validate_expr(rhs, target_dialect, &scope, op_index, ts_location)?;
            }
            if let Some(pred) = filter {
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        Op::DropIndex { name, table, .. } => {
            // §8.6 fail-closed (HIGH): a DropIndex carries an index `name` and an
            // OPTIONAL owning-table hint. The ownership gate
            // ([`crate::ir_load::enforce_ir_ownership`]) checks the op's TARGET
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
            }
            let view = crate::ir::IrColumn {
                name: column.clone(),
                ty: ty.clone(),
                nullable: *nullable,
                default: default.clone(),
                unique: None,
                id_prefix: None,
                vector_metric: *vector_metric,
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
        Op::PgRaw { binds, .. } if !binds.is_empty() => Err(AuthoringError {
            code: CODE_UNSUPPORTED.to_string(),
            kind: Some(UnsupportedKind::Op),
            op_index,
            ts_location: ts_location.map(str::to_string),
            dialect: target_dialect,
            reason: format!(
                "pgRaw carries {} bind value(s), but the current vendor/raw DDL path \
                 executes SQL as a batch statement and does not bind parameters. \
                 Non-empty PgRaw.binds are rejected until a parameterized PgRaw plan \
                 step exists.",
                binds.len()
            ),
            suggested_fix: Some(
                "remove interpolation from pg.sql for now, or wait for the \
                 parameterized PgRaw executor path; do not inline untrusted values"
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
        Op::DropTable { .. }
        | Op::RenameTable { .. }
        | Op::DropColumn { .. }
        | Op::AlterColumnNullability { .. }
        | Op::RenameColumn { .. }
        | Op::DropConstraint { .. }
        | Op::Insert { .. }
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
        | Op::EnableRls { .. }
        | Op::ForceRls { .. }
        | Op::DisableRls { .. }
        | Op::NoForceRls { .. }
        | Op::DropPolicy { .. }
        | Op::DropTrigger { .. }
        | Op::DropView { .. }
        | Op::CreateFunction { .. }
        | Op::DropFunction { .. }
        | Op::PgRaw { .. } => Ok(()),
    }
}

/// **VENDOR (`@zeroship/migrate/pg`)** — the capability-composition gate (vendor
/// spec §3.2 gate 1). For every VENDOR [`Op`](crate::ir::Op) variant:
///
/// 1. **SQLite refusal** — every vendor op is `dialect_scope = PgOnly` (no SQLite
///    analogue, §4.3); a SQLite target is refused [`CODE_UNSUPPORTED`] `{kind:"op"}`
///    at load, never silently skipped.
/// 2. **Capability gate** — the active
///    [`VendorCapabilities`](crate::capability::VendorCapabilities), derived from the
///    threaded [`SchemaScope`](crate::guard::SchemaScope), must GRANT the op's
///    required [`VendorCapability`](crate::capability::VendorCapability). The
///    Confined `Single` scope grants nothing ⇒ every vendor op is
///    [`CODE_VENDOR_OP_DENIED`]. The gate keys on the CAPABILITY FLAG
///    (`caps.grants(cap)`), not on a hard-coded profile name.
///
/// A non-vendor op is a no-op here.
fn validate_vendor_op(
    op: &crate::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::guard::SchemaScope>,
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
            .any(|cap| !matches!(cap, crate::capability::VendorCapability::RawViewBody))
    {
        let cap = caps
            .iter()
            .find(|cap| !matches!(cap, crate::capability::VendorCapability::RawViewBody))
            .copied()
            .expect("non-raw-view cap exists");
        let (reason, fix) = if matches!(cap, crate::capability::VendorCapability::MaterializedView)
        {
            (
                "materializedView: SQLite has no materialized views; materialized:true is PostgreSQL-only"
                    .to_string(),
                "drop materialized:true for SQLite, or target Postgres for this view".to_string(),
            )
        } else {
            (
                format!(
                    "the @zeroship/migrate/pg vendor op (capability {:?}) is Postgres-only — \
                     roles/grants/RLS/policies/triggers/functions/extensions/schemas/pg.sql have \
                     no SQLite analogue (PgOnly)",
                    cap.as_token()
                ),
                "vendor primitives target Postgres only — deploy this migration against a \
                 Postgres backend, or remove the @zeroship/migrate/pg op"
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
    let caps = crate::capability::VendorCapabilities::from_scope(schema_scope);
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
                     @zeroship/migrate/pg primitives are unreachable from a confined migration by \
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
/// The raw surface is deliberately narrower than `pg.sql`: it must be exactly one
/// top-level `SELECT` (no DDL/DML utility statement, no semicolon-chained second
/// statement, no `SELECT INTO`) and then it is fed through the same body
/// reparse/string-literal/token deny-list used for function bodies.
pub(crate) fn validate_raw_view_body_sql(
    sql: &str,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::guard::SchemaScope>,
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
    table: &crate::ir::TableRef,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::guard::SchemaScope>,
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
    select: &crate::ir::SelectAst,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::guard::SchemaScope>,
) -> Result<(), AuthoringError> {
    use crate::ir::{OrderItem, SelectItem};

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
///    ([`CODE_CROSS_SCHEMA`]). Absent schema, or a permitted one, passes. Trusted
///    (`None`) skips this — no confinement.
/// 3. **Existence-guard direction** — a guard whose direction is illegal for the op
///    variant is refused ([`CODE_GUARD_DIRECTION`]).
fn validate_op_schema_and_guard(
    op: &crate::ir::Op,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::guard::SchemaScope>,
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

    // (1) + (2) — the schema qualifier.
    if let Some(schema) = op.schema() {
        if !is_safe_schema_ident(schema) {
            return Err(mk(
                CODE_INVALID_SCHEMA_IDENT,
                format!(
                    "schema qualifier {schema:?} is not a safe bare SQL identifier \
                     (must be non-empty, start with a letter or '_', and contain only \
                     letters, digits, or '_')"
                ),
                "use a plain identifier for the schema, e.g. schema: \"app2\"".to_string(),
            ));
        }
        if let Some(scope) = schema_scope {
            if !scope.permits(schema) {
                let (reason, fix) = match scope {
                    crate::guard::SchemaScope::Single(project) => (
                        format!(
                            "this migration is CONFINED to its project schema {project:?}, \
                             but op names a different schema {schema:?} — a cross-schema \
                             migration is refused fail-closed (the creator profile pins the \
                             project schema; the migrator role would also reject it, but this \
                             is the earlier, friendlier gate)"
                        ),
                        format!(
                            "drop the schema qualifier (it defaults to {project:?}) or set \
                             schema: {project:?}"
                        ),
                    ),
                    crate::guard::SchemaScope::Allowlist(allowed) => (
                        format!(
                            "op names schema {schema:?}, which is not in the permitted \
                             platform schema allow-list {allowed:?}"
                        ),
                        format!("name one of the permitted schemas {allowed:?}"),
                    ),
                };
                return Err(mk(CODE_CROSS_SCHEMA, reason, fix));
            }
        }
    }

    // (3) — the existence-guard direction.
    if let Some(guard) = op.existence_guard() {
        match op.legal_existence_guard() {
            Some(legal) if legal == guard => {}
            Some(_) => {
                let (got, want, family) = match guard {
                    crate::ir::ExistenceGuard::IfExists => {
                        ("ifExists", "ifNotExists", "create*/add*")
                    }
                    crate::ir::ExistenceGuard::IfNotExists => {
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
    events: &[crate::ir::TriggerEvent],
    for_each: crate::ir::ForEach,
    action: &crate::ir::TriggerAction,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    match (target_dialect, action) {
        (Dialect::Postgres, crate::ir::TriggerAction::Body { .. }) => {
            return Err(unsupported_trigger(
                "triggerBody",
                target_dialect,
                op_index,
                ts_location,
                "Postgres triggers must execute a named trigger function; the closed inline body form renders only on SQLite".to_string(),
                "use action: { kind: \"executeFunction\", name: \"...\" } and create the trigger function separately".to_string(),
            ));
        }
        (Dialect::Sqlite | Dialect::Mysql, crate::ir::TriggerAction::ExecuteFunction { .. }) => {
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
        && events.iter().any(|e| matches!(e, crate::ir::TriggerEvent::Truncate))
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
        && matches!(for_each, crate::ir::ForEach::Statement)
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
    stmt: &crate::ir::TriggerStmt,
    outer_table: &str,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
    schema_scope: Option<&crate::guard::SchemaScope>,
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
        crate::ir::TriggerStmt::Insert { schema, .. } => validate_schema(schema.as_deref()),
        crate::ir::TriggerStmt::Update { table, set, r#where, schema } => {
            validate_schema(schema.as_deref())?;
            let scope = TargetScope::structural_only(table);
            for rhs in set.values() {
                validate_expr(rhs, target_dialect, &scope, op_index, ts_location)?;
            }
            if let Some(pred) = r#where {
                validate_expr(pred, target_dialect, &scope, op_index, ts_location)?;
            }
            Ok(())
        }
        crate::ir::TriggerStmt::Delete { table, r#where, schema, .. } => {
            validate_schema(schema.as_deref())?;
            let scope = TargetScope::structural_only(table);
            validate_expr(r#where, target_dialect, &scope, op_index, ts_location)
        }
        crate::ir::TriggerStmt::Select { expr } => {
            let scope = TargetScope::structural_only(outer_table);
            validate_expr(expr, target_dialect, &scope, op_index, ts_location)
        }
        crate::ir::TriggerStmt::Raise { errcode, .. } => {
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

/// **Migration-first P2a (§4)** — validate one [`IrColumn`](crate::ir::IrColumn)'s
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
///    [`crate::ir::VectorMetric`] enum at deserialize; the only authoring error
///    left is CO-OCCURRENCE: a metric carried on a non-`Vector` column is
///    meaningless (the opclass has no vector to apply to) and is refused
///    ([`CODE_VECTOR_METRIC_MISPLACED`]) so a hand-crafted artifact cannot ride a
///    dead field in.
///
/// # Errors
/// [`CODE_INVALID_ID_PREFIX`] / [`CODE_VECTOR_METRIC_MISPLACED`] as above.
fn validate_column_facets(
    col: &crate::ir::IrColumn,
    target_dialect: Dialect,
    op_index: usize,
    ts_location: Option<&str>,
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

    if col.identity.is_some() && !matches!(col.ty, crate::ir::ColType::Int | crate::ir::ColType::BigInt)
    {
        return Err(unsupported(
            UnsupportedKind::Identity,
            format!(
                "column {:?} declares identity on a non-integer type; identity is only \
                 supported on int/bigInt columns",
                col.name
            ),
            "declare the column as `t.integer().identity(...)` or `t.bigInt().identity(...)`"
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

    if col.vector_metric.is_some() && !matches!(col.ty, crate::ir::ColType::Vector { .. }) {
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

    Ok(())
}

fn validate_identity_placement(
    col: &crate::ir::IrColumn,
    target_dialect: Dialect,
    pk_cols: Option<&[String]>,
    is_add_column: bool,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    if col.identity.is_none() || !matches!(target_dialect, Dialect::Sqlite | Dialect::Mysql) {
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
    if is_add_column {
        return Err(err(
            "identity: non-PK identity has no sound target-dialect render; SQLite \
             AUTOINCREMENT and MySQL AUTO_INCREMENT are only sound on an inline \
             integer primary key"
                .to_string(),
        ));
    }
    let Some(pk_cols) = pk_cols else {
        return Err(err(format!(
            "identity: column {:?} is not the declared primary key; non-PK identity \
             has no sound target-dialect render",
            col.name
        )));
    };
    if pk_cols.len() == 1 && pk_cols[0] == col.name {
        return Ok(());
    }
    Err(err(format!(
        "identity: column {:?} is part of {:?}, but this dialect's identity is only \
         sound for the sole integer primary key",
        col.name, pk_cols
    )))
}

/// **Apply/render-seam ColRef resolution (rule (c), MED).** Re-run the
/// expression-AST walk for the ops whose live-schema column set was NOT known at
/// IR-load time — the DML ops (`update`/`delete`/`backfill`) and `alterColumnType`
/// — now that the render/apply seam HAS the live columns. For each such op whose
/// target table appears in `live_columns`, the embedded predicates / set RHS /
/// cast are re-validated with a **RESOLVING** [`TargetScope`], so an unresolved
/// `ColRef` is rejected with the structured [`AuthoringError`] (rule (c)) at apply
/// — NOT as an opaque raw DB error mid-statement.
///
/// `live_columns` maps a target table → its live column names (system fields
/// included). An op whose table is absent from the map keeps the structural-only
/// scope (the (c) check is skipped — the caller could not resolve that table).
/// Non-DML / non-`alterColumnType` ops are revalidated structurally (a),(b),(d)
/// — harmless and keeps the walk total.
///
/// This is the seam the design (`validate_ir` doc, "the apply/render seam re-runs
/// the walk with a resolved column set to enforce (c)") names. In PR1 the DML /
/// `alterColumnType` LOWER is still deferred (`IrAuthor::lower` returns
/// `UnsupportedOp`), so this resolution is exercised as a stand-alone seam +
/// regression; once DML lowering lands (PR6a) the apply path calls this BEFORE
/// rendering the DML statement.
///
/// # Errors
/// The first [`AuthoringError`] any embedded expression produces — incl. a rule
/// (c) `ColRef`-resolution failure now that the column set is known.
pub fn validate_ir_resolved(
    ir: &crate::ir::MigrationIr,
    target_dialect: Dialect,
    live_columns: &std::collections::BTreeMap<String, Vec<String>>,
    ts_locations: &[Option<String>],
) -> Result<(), AuthoringError> {
    for (op_index, op) in ir.ops.iter().enumerate() {
        let ts = ts_locations.get(op_index).and_then(Option::as_deref);
        validate_op_resolved(op, target_dialect, live_columns, op_index, ts)?;
    }
    Ok(())
}

/// **Single-op apply/render-seam ColRef resolution (rule (c), MED).** The per-op
/// peer of [`validate_ir_resolved`]: re-run the expression-AST walk for ONE op with
/// a RESOLVING [`TargetScope`] when its target table's live column set is known.
///
/// This is the seam the DML LOWER calls ([`crate::ir_author::IrAuthor::lower_dml_op`]):
/// at lower/apply the live schema HAS been introspected, so each DML op
/// (`update`/`delete`/`backfill`) / `alterColumnType` resolves its embedded
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
/// (a)/(b)/(d) structural checks still run). A non-DML / non-`alterColumnType` op
/// re-runs the structural [`validate_op`] (harmless; keeps the walk total).
///
/// # Errors
/// The first [`AuthoringError`] the op's embedded expressions produce — incl. a
/// rule (c) `ColRef`-resolution failure now that the column set is known.
pub fn validate_op_resolved(
    op: &crate::ir::Op,
    target_dialect: Dialect,
    live_columns: &std::collections::BTreeMap<String, Vec<String>>,
    op_index: usize,
    ts_location: Option<&str>,
) -> Result<(), AuthoringError> {
    use crate::ir::Op;
    let ts = ts_location;
    // The op's target table (for the DML / alterColumnType ops we resolve).
    let resolved_scope = |table: &str| -> Option<Vec<String>> { live_columns.get(table).cloned() };
    match op {
        Op::Update { table, set, r#where, .. } => {
            if let Some(cols) = resolved_scope(table) {
                let scope = TargetScope::new(table, &cols);
                for rhs in set.values() {
                    validate_expr(rhs, target_dialect, &scope, op_index, ts)?;
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
                for rhs in set.values() {
                    validate_expr(rhs, target_dialect, &scope, op_index, ts)?;
                }
                if let Some(pred) = filter {
                    validate_expr(pred, target_dialect, &scope, op_index, ts)?;
                }
            } else {
                validate_op(op, target_dialect, op_index, ts)?;
            }
        }
        Op::AlterColumnType { table, using, .. } => {
            if let (Some(cols), Some(cast)) = (resolved_scope(table), using) {
                let scope = TargetScope::new(table, &cols);
                validate_expr(cast, target_dialect, &scope, op_index, ts)?;
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
            Expr::ColRef { name } => self.check_colref(name),
            Expr::Literal { .. } => Ok(()),
            Expr::BinOp { lhs, rhs, .. } => {
                self.walk_depth(lhs, d)?;
                self.walk_depth(rhs, d)
            }
            Expr::UnaryOp { operand, .. } => self.walk_depth(operand, d),
            Expr::Case { branches, r#else } => {
                for CaseBranch { condition, result } in branches {
                    self.walk_depth(condition, d)?;
                    self.walk_depth(result, d)?;
                }
                if let Some(e) = r#else {
                    self.walk_depth(e, d)?;
                }
                Ok(())
            }
            // FnCall is an allow-listed scalar by construction (the closed
            // ScalarFn enum) — only its args need recursion.
            Expr::FnCall { args, .. } => {
                for a in args {
                    self.walk_depth(a, d)?;
                }
                Ok(())
            }
            Expr::FnSynth { r#fn, args } => self.check_synth(*r#fn, args, d),
            // Cast target is portable by the closed CastTarget enum (rule d);
            // recurse into the operand.
            Expr::Cast { operand, .. } => self.walk_depth(operand, d),
        }
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
            Expr::Literal { value: crate::ir::IrScalar::Str(s) } => s,
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
            Expr::Literal { value: crate::ir::IrScalar::Int(n) } => {
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
    use crate::expr::{BinaryOp, CastTarget, Expr, ScalarFn, SynthFn, UnaryOp};
    use crate::ir::IrScalar;

    fn cols() -> Vec<String> {
        vec!["name".into(), "first".into(), "last".into(), "total".into()]
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
                    target: CastTarget::Integer,
                }),
                rhs: Box::new(Expr::lit(IrScalar::Int(0))),
            }),
        };
        assert!(validate_expr(&e, Dialect::Sqlite, &sc, 0, None).is_ok());

        // Case + FnCall(coalesce) + concat.
        let case = Expr::Case {
            branches: vec![CaseBranch {
                condition: Expr::UnaryOp {
                    op: UnaryOp::IsNull,
                    operand: Box::new(Expr::col("first")),
                },
                result: Expr::lit(IrScalar::Str("none".into())),
            }],
            r#else: Some(Box::new(Expr::FnCall {
                r#fn: ScalarFn::Coalesce,
                args: vec![Expr::col("first"), Expr::lit(IrScalar::Str("".into()))],
            })),
        };
        assert!(validate_expr(&case, Dialect::Postgres, &sc, 1, None).is_ok());
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
                unique: None, id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None }],
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
                },
            }],
            indexes: vec![],
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Postgres, &[]).unwrap_err();
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

    use crate::ir::{
        ColType, IrColumn, IrConstraint, IrConstraintKind, IrIndex, MigrationIr, Op,
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

    // ── PR10: schema confinement + guard direction + schema-ident safety ────────

    /// CONFINED — an explicit `schema != project_schema` is REFUSED fail-closed at
    /// validate-time with the structured `CROSS_SCHEMA` code (§2.7). RED before the
    /// gate (the op would have lowered cross-schema). An op whose schema EQUALS the
    /// project schema, or omits it, passes.
    #[test]
    fn confined_cross_schema_op_is_refused_at_validate() {
        use crate::guard::SchemaScope;
        let cross = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("other_app".into()),
            existence_guard: None,
        }]);
        let scope = SchemaScope::Single("app_a".into());
        let err =
            validate_ir_scoped(&cross, Dialect::Postgres, &[], Some(&scope)).unwrap_err();
        assert_eq!(err.code, CODE_CROSS_SCHEMA, "got: {err}");

        // schema == project schema (case-insensitive) passes.
        let same = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("APP_A".into()),
            existence_guard: None,
        }]);
        assert!(validate_ir_scoped(&same, Dialect::Postgres, &[], Some(&scope)).is_ok());

        // Absent schema passes.
        let none = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: None,
        }]);
        assert!(validate_ir_scoped(&none, Dialect::Postgres, &[], Some(&scope)).is_ok());
    }

    /// TRUSTED (`None` scope) honors any schema — no cross-schema confinement — but
    /// PLATFORM (`Allowlist`) refuses a schema outside its allow-list (§2.7).
    #[test]
    fn trusted_honors_any_schema_platform_gates_to_allowlist() {
        use crate::guard::SchemaScope;
        let foreign = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("anything".into()),
            existence_guard: None,
        }]);
        // Trusted: permitted.
        assert!(validate_ir_scoped(&foreign, Dialect::Postgres, &[], None).is_ok());
        // Platform allow-list excluding "anything": refused.
        let scope = SchemaScope::Allowlist(vec!["zeroship".into(), "public".into()]);
        let err =
            validate_ir_scoped(&foreign, Dialect::Postgres, &[], Some(&scope)).unwrap_err();
        assert_eq!(err.code, CODE_CROSS_SCHEMA);
        // A schema IN the allow-list passes.
        let ok = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: Some("zeroship".into()),
            existence_guard: None,
        }]);
        assert!(validate_ir_scoped(&ok, Dialect::Postgres, &[], Some(&scope)).is_ok());
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
            // Even Trusted (None scope) rejects an injection-shaped ident.
            let err = validate_ir_scoped(&ir, Dialect::Postgres, &[], None).unwrap_err();
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
            constraints: vec![],
            indexes: vec![],
            schema: None,
            existence_guard: Some(crate::ir::ExistenceGuard::IfExists),
        }]);
        let err = validate_ir_scoped(&bad_create, Dialect::Postgres, &[], None).unwrap_err();
        assert_eq!(err.code, CODE_GUARD_DIRECTION, "got: {err}");

        // ifNotExists on dropTable — illegal.
        let bad_drop = ir_with(vec![Op::DropTable {
            table: "t".into(),
            cascade: None,
            schema: None,
            existence_guard: Some(crate::ir::ExistenceGuard::IfNotExists),
        }]);
        let err2 = validate_ir_scoped(&bad_drop, Dialect::Postgres, &[], None).unwrap_err();
        assert_eq!(err2.code, CODE_GUARD_DIRECTION);

        // The LEGAL directions pass.
        let ok_create = ir_with(vec![Op::CreateTable {
            name: "t".into(),
            columns: vec![],
            constraints: vec![],
            indexes: vec![],
            schema: None,
            existence_guard: Some(crate::ir::ExistenceGuard::IfNotExists),
        }]);
        assert!(validate_ir_scoped(&ok_create, Dialect::Postgres, &[], None).is_ok());
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
            set: [("name".to_string(), Expr::col("ghost"))].into_iter().collect(),
            r#where: None,
            batch: None,
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
    fn validate_ir_resolved_accepts_resolvable_colref_in_update_set() {
        use std::collections::BTreeMap;
        // The SAME shape but the ColRef references a column that DOES exist.
        let ir = ir_with(vec![Op::Update {
            table: "users".into(),
            set: [("name".to_string(), Expr::col("name"))].into_iter().collect(),
            r#where: None,
            batch: None,
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
                    IrColumn { name: "first".into(), ty: ColType::Text, nullable: None, default: None, unique: None, id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None },
                    IrColumn { name: "total".into(), ty: ColType::Int, nullable: None, default: None, unique: None, id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None },
                ],
                constraints: vec![IrConstraint {
                    name: None,
                    kind: IrConstraintKind::Check {
                        // references `total`, which IS a column of users → ok
                        expr: Expr::UnaryOp {
                            op: UnaryOp::IsNotNull,
                            operand: Box::new(Expr::col("total")),
                        },
                    },
                }],
                indexes: vec![IrIndex {
                    name: None,
                    columns: vec!["first".into()],
                    unique: None,
                    using: None,
                    r#where: Some(Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::col("first")),
                    }),
                }],
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
        assert!(validate_ir(&ir, Dialect::Postgres, &[]).is_ok());
        assert!(validate_ir(&ir, Dialect::Sqlite, &[]).is_ok());
    }

    #[test]
    fn validate_ir_create_table_resolves_system_fields_in_scope() {
        // MED: createTable auto-injects the seven platform system fields at
        // lower/render time. A legitimate soft-delete partial-unique index
        // `WHERE deleted_at IS NULL` and a Check referencing `id` reference those
        // system fields — they MUST resolve in rule (c) scope, not be rejected.
        let ir = ir_with(vec![Op::CreateTable {
            name: "users".into(),
            columns: vec![IrColumn {
                name: "first".into(),
                ty: ColType::Text,
                nullable: None,
                default: None,
                unique: None, id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None }],
            constraints: vec![IrConstraint {
                name: None,
                kind: IrConstraintKind::Check {
                    // references `id`, a system field → must resolve
                    expr: Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::col("id")),
                    },
                },
            }],
            indexes: vec![IrIndex {
                name: None,
                columns: vec!["first".into()],
                unique: Some(true),
                using: None,
                // the canonical soft-delete partial-unique predicate
                r#where: Some(Expr::UnaryOp {
                    op: UnaryOp::IsNull,
                    operand: Box::new(Expr::col("deleted_at")),
                }),
            }],
            schema: None,
            existence_guard: None,
        }]);
        assert!(
            validate_ir(&ir, Dialect::Postgres, &[]).is_ok(),
            "a Check on `id` + partial index on `deleted_at` must resolve system fields (PG)"
        );
        assert!(
            validate_ir(&ir, Dialect::Sqlite, &[]).is_ok(),
            "a Check on `id` + partial index on `deleted_at` must resolve system fields (SQLite)"
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
                unique: None, id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None }],
            constraints: vec![IrConstraint {
                name: None,
                kind: IrConstraintKind::Check {
                    expr: Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::col("ghost")),
                    },
                },
            }],
            indexes: vec![],
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Postgres, &[]).unwrap_err();
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
                unique: None, id_prefix: None, vector_metric: None, mask: None, generated: None, identity: None }],
            constraints: vec![IrConstraint {
                name: None,
                kind: IrConstraintKind::Check {
                    expr: Expr::UnaryOp {
                        op: UnaryOp::IsNotNull,
                        operand: Box::new(Expr::col("ghost")),
                    },
                },
            }],
            indexes: vec![],
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Postgres, &[]).unwrap_err();
        assert_eq!(err.code, CODE_UNSUPPORTED);
        assert_eq!(err.kind, Some(UnsupportedKind::Expr));
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_rejects_out_of_envelope_split_part_in_update_set() {
        // The Update is the SECOND op — the walker must stamp op_index = 1, and
        // it must reach the `set` RHS (the splitPart) to reject it.
        let mut set = BTreeMap::new();
        set.insert("name".to_string(), split(", ", 1)); // multi-char delim
        let ir = ir_with(vec![
            Op::DropColumn {
                table: "t".into(),
                column: "x".into(),
                schema: None,
                existence_guard: None,
            },
            Op::Update { table: "users".into(), set, r#where: None, batch: None, schema: None },
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
            columns: vec!["a".into()],
            name: None,
            unique: None,
            using: None,
            r#where: Some(split(", ", 1)),
            concurrently: None,
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Sqlite, &[]).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_walks_alter_column_type_using_cast() {
        // The property-A fix made alterColumnType.using a closed Expr — the
        // walker must reach it. A splitPart cast operand with a bad delim rejects.
        let ir = ir_with(vec![Op::AlterColumnType {
            table: "users".into(),
            column: "a".into(),
            ty: ColType::Int,
            using: Some(split(", ", 1)),
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Sqlite, &[]).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn validate_ir_walks_backfill_filter_and_set() {
        let mut set = BTreeMap::new();
        set.insert("name".to_string(), Expr::col("first")); // fine structurally
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
            set: [("name".to_string(), Expr::col("column_that_was_dropped"))]
                .into_iter()
                .collect(),
            r#where: Some(Expr::BinOp {
                op: BinaryOp::Eq,
                lhs: Box::new(Expr::col("column_that_was_dropped")),
                rhs: Box::new(Expr::lit(IrScalar::Int(1))),
            }),
            batch: None,
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

    use crate::ir::VectorMetric;

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
                vector_metric: None, mask: None,
                generated: None,
                identity: None,
            }],
            constraints: vec![],
            indexes: vec![],
            schema: None,
            existence_guard: None,
        }
    }

    #[test]
    fn p2a_create_table_accepts_a_valid_id_prefix() {
        let ir = ir_with(vec![create_with_id_prefix("post")]);
        assert!(
            validate_ir(&ir, Dialect::Postgres, &[]).is_ok(),
            "a well-formed, unreserved, in-length id prefix must validate"
        );
    }

    #[test]
    fn p2a_create_table_rejects_a_reserved_id_prefix() {
        // `usr` is the platform user-id prefix (RESERVED_ID_PREFIXES); a creator
        // prefix that collides with it would mint ids colliding with platform users.
        let ir = ir_with(vec![create_with_id_prefix("usr")]);
        let err = validate_ir(&ir, Dialect::Postgres, &[])
            .expect_err("a reserved id prefix must be refused at validate, fail-closed");
        assert_eq!(err.code, CODE_INVALID_ID_PREFIX, "got: {err}");
        assert_eq!(err.op_index, 0);
    }

    #[test]
    fn p2a_create_table_rejects_a_malformed_id_prefix() {
        // An upper-case / non-`[a-z0-9_]` prefix is not a valid typed-id segment.
        let ir = ir_with(vec![create_with_id_prefix("Po-st")]);
        let err = validate_ir(&ir, Dialect::Postgres, &[])
            .expect_err("a malformed id prefix must be refused at validate");
        assert_eq!(err.code, CODE_INVALID_ID_PREFIX, "got: {err}");
    }

    #[test]
    fn p2a_create_table_rejects_an_over_long_id_prefix() {
        // Charset-valid but longer than MAX_ID_PREFIX_LEN — refused so the minted
        // `<prefix>_<22 base62>` typed-id keeps the compact platform shape.
        let ir = ir_with(vec![create_with_id_prefix("toolong")]);
        let err = validate_ir(&ir, Dialect::Postgres, &[])
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
                vector_metric: Some(VectorMetric::Cosine),
                mask: None,
                generated: None,
                identity: None,
            }],
            constraints: vec![],
            indexes: vec![],
            schema: None,
            existence_guard: None,
        }]);
        let err = validate_ir(&ir, Dialect::Postgres, &[])
            .expect_err("a vector_metric on a non-vector column must be refused");
        assert_eq!(err.code, CODE_VECTOR_METRIC_MISPLACED, "got: {err}");
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
                vector_metric: Some(VectorMetric::Cosine),
                mask: None,
                generated: None,
                identity: None,
            }],
            constraints: vec![],
            indexes: vec![],
            schema: None,
            existence_guard: None,
        }]);
        assert!(
            validate_ir(&ir, Dialect::Postgres, &[]).is_ok(),
            "a metric on a t.vector(n) column is the legitimate co-occurrence"
        );
    }
}
