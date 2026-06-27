//! The CLOSED expression AST (design §3.3.1 / §3.3.1.1).
//!
//! Every expression position in the `op.*` IR — a DML `set` value, a `where`,
//! an `addCheck` body, a partial-index `where:` — is a node of this **closed**
//! AST, constructed in JS by the fluent `(c) => Expr` builder and serialized to
//! the `.ir.json` as data. **It is NEVER parsed from text** — there is no lexer,
//! no Pratt parser, no `libpg_query`, and therefore no Rust-vs-JS parser drift
//! and no differential fuzzer (HIGH-1 dissolved, §3.3.1.1). Validation is a
//! purely STRUCTURAL allow-list check over this enum ([`crate::validate`]).
//!
//! The variants are exactly:
//!
//! `ColRef | Literal | BinOp | UnaryOp | Case | FnCall(allow-listed) | FnSynth |
//! Cast`.
//!
//! # Why a closed enum, internally tagged
//!
//! - schemars derives the discriminated-union JSON Schema the JS builder targets
//!   (one `$defs/Expr` `oneOf`, each branch pinning `properties.node.const`).
//! - serde deserialize REJECTS any node tag outside the closed set — a
//!   hand-crafted `.ir.json` carrying an unknown node simply fails to parse
//!   (`UNSUPPORTED { kind: "expr" }` at load), there is no "unknown function"
//!   parse path because there is no text to parse.
//! - The numeric domain of a [`Literal`](Expr::Literal) is the constrained
//!   [`IrScalar`](crate::ir::IrScalar) — a fractional/exponential/`>=2^53` value
//!   is rejected at DESERIALIZE before any checksum runs (§2.5).
//!
//! NB: the per-dialect *rendering* of an `Expr` is the engine's job (Wave C /
//! PR6a) — this module is the data + (with [`crate::validate`]) the structural
//! gate. Nothing here renders SQL.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::ir::IrScalar;

/// A binary operator admitted in the closed AST (§3.3.1 method↔node table).
///
/// Camel/lower-cased on the wire so the JS builder emits the same tokens
/// (`{"node":"binOp","op":"eq", …}`). The set is closed: comparison, boolean,
/// arithmetic, and string concatenation (`||`, the one place PG/SQLite NULL
/// semantics agree — §3.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BinaryOp {
    /// `=`
    Eq,
    /// `<>` / `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// boolean `AND`
    And,
    /// boolean `OR`
    Or,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `||` string concatenation (NULL-propagating on BOTH backends).
    Concat,
}

/// A unary operator admitted in the closed AST (§3.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum UnaryOp {
    /// boolean `NOT`
    Not,
    /// `IS NULL`
    IsNull,
    /// `IS NOT NULL`
    IsNotNull,
    /// `IS TRUE`
    IsTrue,
    /// `IS FALSE`
    IsFalse,
}

/// The allow-listed *named* scalar functions (`c.fn.*` that are NOT engine-
/// synthesized `FnSynth`). CLOSED — a function outside this set has no builder
/// method and no AST variant (§3.3.1.1(a)). These are the provably-identical
/// cross-dialect scalars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ScalarFn {
    /// `coalesce(...)`
    Coalesce,
    /// `nullif(a, b)`
    Nullif,
    /// `lower(e)`
    Lower,
    /// `upper(e)`
    Upper,
    /// `trim(e)`
    Trim,
    /// `length(e)`
    Length,
    /// `abs(e)`
    Abs,
    /// **VENDOR** — `current_setting('<name>', <missingOk>)` (vendor spec §2.10).
    /// A PG GUC read needed by the RLS policy predicates (`0025`'s
    /// `current_setting('zeroship.tenant_app', true)`). Pure, side-effect-free; it
    /// is PG-only and lowers only on PG (the containing vendor op is `PgOnly`). A
    /// closed-AST `FnCall` node — NOT a raw escape.
    CurrentSetting,
    /// **VENDOR** — `current_user` (vendor spec §2.10). A nullary identity scalar;
    /// renders WITHOUT parentheses (it is a reserved keyword, not a function call).
    CurrentUser,
}

/// The engine-SYNTHESIZED helpers (`FnSynth`) whose per-dialect lowering the
/// engine pins (§9). CLOSED. `splitPart` is admitted only within its pinned
/// single-ASCII-delimiter + positive-literal-`n` envelope (validated structurally
/// — §3.3.1.1(b)); `concatWs` is the NULL-skipping join; `now`/`genRandomUuid`
/// are apply-time DB-evaluated scalars (the structured replacement for a frozen
/// `Date.now()` / UUID literal, §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum SynthFn {
    /// NULL-skipping `concat_ws` (PG) / `coalesce`-folded `||` (SQLite).
    ConcatWs,
    /// `split_part` (PG) / pinned `instr`/`substr` unroll (SQLite), in-envelope only.
    SplitPart,
    /// `now()` / current timestamp, evaluated at apply time.
    Now,
    /// `gen_random_uuid()`, evaluated at apply time.
    GenRandomUuid,
}

/// The closed portable cast-target set (§3.3.1). A non-portable cast target is
/// rejected (`UNSUPPORTED { kind: "expr" }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CastTarget {
    /// `text`
    Text,
    /// `integer`
    Integer,
    /// `real`
    Real,
    /// `boolean`
    Boolean,
    /// `blob` (`BYTEA` on PG)
    Blob,
    /// `uuid` (PG-native `uuid`; `text` on SQLite, which has no uuid type).
    /// Needed for the VENDOR policy predicates — the 0025 tenant-isolation
    /// policy casts `current_setting('zeroship.tenant_app', true)::uuid`, so a
    /// faithful port of `pg_get_expr(polqual)` requires the real `::uuid` cast,
    /// not a `::text` substitute (vendor spec §2.10 / §5.3).
    Uuid,
}

/// The CLOSED expression AST node (§3.3.1). Internally tagged on `"node"`,
/// camel-cased (`{"node":"colRef","name":"first"}`). NO `untagged`, NO `flatten`
/// — same discipline as [`Op`](crate::ir::Op), so schemars derives a clean
/// discriminated union and serde rejects any out-of-set node tag at deserialize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "node", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Expr {
    /// A column reference (`c("name")`). The name is a plain string, resolved
    /// against the enclosing op's single target table at apply/render time
    /// (§3.3.1.1(c)) — never `tsc`-bound to the live schema (§3.3).
    ColRef {
        /// The column name (plain string).
        name: String,
    },
    /// A typed scalar literal (a bare JS value auto-wrapped by a fluent operator
    /// method). Carries an [`IrScalar`] so the numeric domain is enforced at
    /// deserialize (§2.5) and the value folds into the checksum (§2.4 point 3).
    Literal {
        /// The typed scalar value.
        value: IrScalar,
    },
    /// A binary operation (`lhs <op> rhs`).
    BinOp {
        /// The operator.
        op: BinaryOp,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
    },
    /// A unary operation (`<op> operand` / `operand <op>`).
    UnaryOp {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        operand: Box<Expr>,
    },
    /// A searched `CASE` (`c.fn.case([[cond, val], …], else?)`). Each branch is
    /// `(condition, result)`; both halves are themselves closed-AST nodes.
    Case {
        /// `(condition, result)` branches, in order.
        branches: Vec<CaseBranch>,
        /// Optional `ELSE` result.
        #[serde(rename = "else", skip_serializing_if = "Option::is_none")]
        r#else: Option<Box<Expr>>,
    },
    /// An allow-listed named scalar function call (§3.3.1.1(a)).
    FnCall {
        /// The function (allow-listed).
        r#fn: ScalarFn,
        /// The argument expressions.
        args: Vec<Expr>,
    },
    /// An engine-SYNTHESIZED helper call (§9). The per-dialect lowering is the
    /// engine's; the author never sees the rendered form.
    FnSynth {
        /// The synthesized function.
        r#fn: SynthFn,
        /// The argument expressions (a `splitPart`'s `delim`/`n` are `Literal`
        /// args, validated in-envelope structurally — §3.3.1.1(b)).
        args: Vec<Expr>,
    },
    /// A cast to a portable type (`.cast("integer")`).
    Cast {
        /// The expression being cast.
        operand: Box<Expr>,
        /// The portable target type.
        target: CastTarget,
    },
}

/// One `(condition, result)` branch of a [`Expr::Case`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaseBranch {
    /// The branch condition (a boolean closed-AST node).
    pub condition: Expr,
    /// The branch result.
    pub result: Expr,
}

impl Expr {
    /// A `ColRef` convenience constructor (tests / IrAuthor).
    #[must_use]
    pub fn col(name: impl Into<String>) -> Self {
        Expr::ColRef { name: name.into() }
    }

    /// A `Literal` convenience constructor.
    #[must_use]
    pub fn lit(value: IrScalar) -> Self {
        Expr::Literal { value }
    }
}
