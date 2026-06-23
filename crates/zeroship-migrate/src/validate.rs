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
//! Rust validator here is the SOLE authoritative gate; the JS side runs an
//! optional best-effort structural hint over the SAME schemars schema.

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
        match expr {
            Expr::ColRef { name } => self.check_colref(name),
            Expr::Literal { .. } => Ok(()),
            Expr::BinOp { lhs, rhs, .. } => {
                self.walk(lhs)?;
                self.walk(rhs)
            }
            Expr::UnaryOp { operand, .. } => self.walk(operand),
            Expr::Case { branches, r#else } => {
                for CaseBranch { condition, result } in branches {
                    self.walk(condition)?;
                    self.walk(result)?;
                }
                if let Some(e) = r#else {
                    self.walk(e)?;
                }
                Ok(())
            }
            // FnCall is an allow-listed scalar by construction (the closed
            // ScalarFn enum) — only its args need recursion.
            Expr::FnCall { args, .. } => {
                for a in args {
                    self.walk(a)?;
                }
                Ok(())
            }
            Expr::FnSynth { r#fn, args } => self.check_synth(*r#fn, args),
            // Cast target is portable by the closed CastTarget enum (rule d);
            // recurse into the operand.
            Expr::Cast { operand, .. } => self.walk(operand),
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

    /// Rule (b): `c.fn.splitPart` envelope. `now`/`genRandomUuid`/`concatWs`
    /// recurse into their args.
    fn check_synth(&self, f: SynthFn, args: &[Expr]) -> Result<(), AuthoringError> {
        match f {
            SynthFn::SplitPart => {
                self.check_split_part(args)?;
                // The column arg (and any in-AST sub-expression) still validates.
                if let Some(col) = args.first() {
                    self.walk(col)?;
                }
                Ok(())
            }
            SynthFn::ConcatWs | SynthFn::Now | SynthFn::GenRandomUuid => {
                for a in args {
                    self.walk(a)?;
                }
                Ok(())
            }
        }
    }

    fn split_part_err(&self, reason: String) -> AuthoringError {
        // splitPart is PG-renderable but the out-of-envelope shape only fails on
        // the SQLite leg (§9) — the rejection is dialect=sqlite.
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
        // Shape: splitPart(col, delim, n) — exactly three args.
        if args.len() != 3 {
            return Err(self.split_part_err(format!(
                "c.fn.splitPart takes exactly (column, delim, n); got {} args",
                args.len()
            )));
        }
        // delim — a single-ASCII-character string Literal.
        match &args[1] {
            Expr::Literal { value } => match value {
                crate::ir::IrScalar::Str(s) => {
                    let bytes = s.as_bytes();
                    if bytes.len() != 1 || bytes[0] >= 0x80 {
                        return Err(self.split_part_err(format!(
                            "c.fn.splitPart delimiter must be a single ASCII character \
                             (one byte, code point < 0x80); got {s:?}"
                        )));
                    }
                }
                other => {
                    return Err(self.split_part_err(format!(
                        "c.fn.splitPart delimiter must be a single-ASCII string literal; \
                         got {other:?}"
                    )));
                }
            },
            other => {
                return Err(self.split_part_err(format!(
                    "c.fn.splitPart delimiter must be a literal (a runtime/computed \
                     delimiter is not portable); got {other:?}"
                )));
            }
        }
        // n — a positive integer Literal in 1..=8.
        match &args[2] {
            Expr::Literal { value } => match value {
                crate::ir::IrScalar::Int(n) => {
                    if *n < 1 {
                        return Err(self.split_part_err(format!(
                            "c.fn.splitPart part index n must be a positive integer; got {n}"
                        )));
                    }
                    if *n > SPLIT_PART_MAX_N {
                        return Err(self.split_part_err(format!(
                            "c.fn.splitPart part index n must be <= {SPLIT_PART_MAX_N} \
                             (the proven inline-unroll bound); got {n}"
                        )));
                    }
                }
                other => {
                    return Err(self.split_part_err(format!(
                        "c.fn.splitPart part index n must be a positive integer literal; \
                         got {other:?}"
                    )));
                }
            },
            other => {
                return Err(self.split_part_err(format!(
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
}
