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

/// The dialect a structured rejection pertains to (§8.8 `dialect` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Postgres.
    Postgres,
    /// SQLite.
    Sqlite,
}

impl Dialect {
    /// The lower-case wire spelling used in the structured payload.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::Postgres => "postgres",
            Dialect::Sqlite => "sqlite",
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
}

impl UnsupportedKind {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            UnsupportedKind::Op => "op",
            UnsupportedKind::Expr => "expr",
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
    for (op_index, op) in ir.ops.iter().enumerate() {
        let ts = ts_locations.get(op_index).and_then(Option::as_deref);
        validate_op(op, target_dialect, op_index, ts)?;
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
    use crate::ir::{IrConstraintKind, Op};

    // A `Check` constraint's expr validates against the given scope.
    let check_constraint =
        |kind: &IrConstraintKind, scope: &TargetScope<'_>| -> Result<(), AuthoringError> {
            if let IrConstraintKind::Check { expr } = kind {
                validate_expr(expr, target_dialect, scope, op_index, ts_location)?;
            }
            Ok(())
        };

    match op {
        Op::CreateTable { name, columns, constraints, indexes } => {
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
        Op::AddConstraint { table, constraint } => {
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
        // Ops with no embedded expression slot.
        Op::DropTable { .. }
        | Op::AddColumn { .. }
        | Op::DropColumn { .. }
        | Op::AlterColumnNullability { .. }
        | Op::RenameColumn { .. }
        | Op::DropConstraint { .. }
        | Op::Insert { .. } => Ok(()),
    }
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
                self.check_split_part(args)?;
                // Structurally walk EVERY arg, regardless of target_dialect. The
                // envelope checks in `check_split_part` return early on a Postgres
                // target (the delim/n LITERAL shape is a SQLite-only envelope), but
                // rule (c) — `ColRef` resolution — and the structural backstop are
                // dialect-NEUTRAL and must cover all three slots on BOTH dialects.
                // Recursing only `args.first()` (the column) let a `ColRef` to a
                // nonexistent column — or a malformed nested synth — hide in the
                // delim/n slot and slip past on PG, deferring the failure to
                // render/execute (item-4). A `Literal` slot recurses to a no-op,
                // so this is idempotent with the envelope literal checks.
                for a in args {
                    self.walk_depth(a, depth)?;
                }
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

    fn check_split_part(&self, args: &[Expr]) -> Result<(), AuthoringError> {
        // Shape: splitPart(col, delim, n) — exactly three args. The WRONG ARITY is
        // broken on BOTH dialects (`split_part` is ternary on PG too), so it is an
        // unconditional CODE_UNSUPPORTED, NOT a dialect-gated envelope reject
        // (MED-1). This is checked first, regardless of target_dialect.
        if args.len() != 3 {
            return Err(self.malformed_synth_err(format!(
                "c.fn.splitPart takes exactly (column, delim, n); got {} args",
                args.len()
            )));
        }
        // The remaining checks are the PORTABILITY ENVELOPE: a multi-char/non-ASCII
        // delim, n outside 1..=8, or a non-literal delim/n is renderable on
        // Postgres but out of the pinned SQLite envelope (§9). On a POSTGRES
        // target the node loads fine; only a SQLITE target rejects it (§2.4.1).
        if self.target_dialect == Dialect::Postgres {
            return Ok(());
        }
        // delim — a single-ASCII-character string Literal (SQLite envelope).
        match &args[1] {
            Expr::Literal { value } => match value {
                crate::ir::IrScalar::Str(s) => {
                    let bytes = s.as_bytes();
                    if bytes.len() != 1 || bytes[0] >= 0x80 {
                        return Err(self.split_part_envelope_err(format!(
                            "c.fn.splitPart delimiter must be a single ASCII character \
                             (one byte, code point < 0x80); got {s:?}"
                        )));
                    }
                }
                other => {
                    return Err(self.split_part_envelope_err(format!(
                        "c.fn.splitPart delimiter must be a single-ASCII string literal; \
                         got {other:?}"
                    )));
                }
            },
            other => {
                return Err(self.split_part_envelope_err(format!(
                    "c.fn.splitPart delimiter must be a literal (a runtime/computed \
                     delimiter is not portable); got {other:?}"
                )));
            }
        }
        // n — a positive integer Literal in 1..=8 (SQLite envelope).
        match &args[2] {
            Expr::Literal { value } => match value {
                crate::ir::IrScalar::Int(n) => {
                    if *n < 1 {
                        return Err(self.split_part_envelope_err(format!(
                            "c.fn.splitPart part index n must be a positive integer; got {n}"
                        )));
                    }
                    if *n > SPLIT_PART_MAX_N {
                        return Err(self.split_part_envelope_err(format!(
                            "c.fn.splitPart part index n must be <= {SPLIT_PART_MAX_N} \
                             (the proven inline-unroll bound); got {n}"
                        )));
                    }
                }
                other => {
                    return Err(self.split_part_envelope_err(format!(
                        "c.fn.splitPart part index n must be a positive integer literal; \
                         got {other:?}"
                    )));
                }
            },
            other => {
                return Err(self.split_part_envelope_err(format!(
                    "c.fn.splitPart part index n must be a literal positive integer \
                     (a runtime n is not portable); got {other:?}"
                )));
            }
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
                unique: None,
            }],
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

    #[test]
    fn validate_ir_passes_a_clean_migration() {
        let ir = ir_with(vec![
            Op::CreateTable {
                name: "users".into(),
                columns: vec![
                    IrColumn { name: "first".into(), ty: ColType::Text, nullable: None, default: None, unique: None },
                    IrColumn { name: "total".into(), ty: ColType::Int, nullable: None, default: None, unique: None },
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
            },
            Op::Delete {
                table: "users".into(),
                r#where: Expr::lit(IrScalar::Bool(true)),
                limit: None,
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
                unique: None,
            }],
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
                unique: None,
            }],
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
                unique: None,
            }],
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
            Op::DropColumn { table: "t".into(), column: "x".into(), if_exists: None },
            Op::Update { table: "users".into(), set, r#where: None, batch: None },
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
        }]);
        let err = validate_ir(&ir, Dialect::Sqlite, &[]).unwrap_err();
        assert_eq!(err.code, CODE_EXPR_NOT_PORTABLE);
    }
}
