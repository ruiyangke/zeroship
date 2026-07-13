//! The CLOSED expression AST.
//!
//! Every expression position in the `op.*` IR — a DML `set` value, a `where`,
//! an `addCheck` body, a partial-index `where:` — is a node of this **closed**
//! AST, constructed in JS by the fluent `(c) => Expr` builder and serialized to
//! the `.ir.json` as data. **It is NEVER parsed from text** — there is no lexer,
//! no Pratt parser, no `libpg_query`, and therefore no Rust-vs-JS parser drift
//! and no differential fuzzer. Validation is a
//! purely STRUCTURAL allow-list check over this enum ([`crate::model::validate`]).
//!
//! The variants are exactly:
//!
//! `ColRef | Literal | BinOp | UnaryOp | Case | FnCall(allow-listed) | FnSynth |
//! Cast | Between | Like | DistinctFrom | Agg | InList | PgRegexMatch |
//! PgColumnSize | Extract | PgExtract | PgInterval | Dialectal`.
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
//!   is rejected at DESERIALIZE before any checksum runs.
//!
//! NB: the per-dialect *rendering* of an `Expr` is the engine's job — this module
//! is the data + (with [`crate::model::validate`]) the structural
//! gate. Nothing here renders SQL.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ir::IrScalar;

/// A binary operator admitted in the closed AST (method↔node table).
///
/// Camel/lower-cased on the wire so the JS builder emits the same tokens
/// (`{"node":"binOp","op":"eq", …}`). The set is closed: comparison, boolean,
/// arithmetic, and string concatenation (`||`, the one place PG/SQLite NULL
/// semantics agree).
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

/// A unary operator admitted in the closed AST.
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
/// method and no AST variant. These are the provably-identical
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
    /// `(<a> % <b>)` — integer/numeric modulo. Rendered as the `%` OPERATOR (not a
    /// `mod(...)` call) because SQLite exposes `%` but has NO `mod()` SQL function;
    /// the `%` spelling is identical on PG, SQLite, and MySQL, so this stays a
    /// dialect-NEUTRAL `ScalarFn` (special-cased at the render seam).
    Mod,
    /// `round(<x>)` / `round(<x>, <n>)` — portable rounding. Identical spelling on
    /// PG, SQLite, and MySQL. Optional second (precision) argument.
    Round,
    /// `floor(<x>)` — portable floor. `floor()` exists on PG, MySQL, and SQLite
    /// (≥3.35). Identical spelling.
    Floor,
    /// `ceil(<x>)` — portable ceiling. `ceil()` exists on PG, SQLite (≥3.35), and
    /// MySQL (where `CEIL` is an alias of `CEILING`). Identical spelling.
    Ceil,
    /// `substr(<s>, <start>[, <len>])` — portable substring. `substr()` exists on
    /// PG, SQLite, and MySQL (where `SUBSTR` is an alias of `SUBSTRING`). Identical
    /// spelling. 1-based `start`; optional `len`.
    Substr,
    /// `replace(<s>, <from>, <to>)` — portable string replace. `replace()` exists
    /// on PG, SQLite, and MySQL with identical spelling and semantics.
    Replace,
    /// **VENDOR** — `current_setting('<name>', <missingOk>)`.
    /// A PG GUC read needed by the RLS policy predicates (e.g. a tenant-isolation
    /// policy's `current_setting('app.tenant_id', true)`). Pure, side-effect-free; it
    /// is PG-only and lowers only on PG (the containing vendor op is `PgOnly`). A
    /// closed-AST `FnCall` node — NOT a raw escape.
    CurrentSetting,
    /// **VENDOR** — `current_user`. A nullary identity scalar;
    /// renders WITHOUT parentheses (it is a reserved keyword, not a function call).
    CurrentUser,
}

/// The engine-SYNTHESIZED helpers (`FnSynth`) whose per-dialect lowering the
/// engine pins. CLOSED. `splitPart` is admitted only within its pinned
/// single-ASCII-delimiter + positive-literal-`n` envelope (validated structurally);
/// `concatWs` is the NULL-skipping join; `now`/`genRandomUuid`
/// are apply-time DB-evaluated scalars (the structured replacement for a frozen
/// `Date.now()` / UUID literal).
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

/// The closed cast-target set, aligned to scalar `ColType` tokens. A
/// non-portable cast target is rejected (`UNSUPPORTED { kind: "expr" }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CastTarget {
    /// `text`
    Text,
    /// `int` (`INTEGER` on SQL backends)
    Int,
    /// `real`
    Real,
    /// `boolean`
    Boolean,
    /// `bytes` (`BYTEA` on PG)
    Bytes,
    /// `uuid` (PG-native `uuid`; `text` on SQLite, which has no uuid type).
    /// Needed for the VENDOR policy predicates — a tenant-isolation
    /// policy casts `current_setting('app.tenant_id', true)::uuid`, so a
    /// faithful port of `pg_get_expr(polqual)` requires the real `::uuid` cast,
    /// not a `::text` substitute.
    Uuid,
}

/// CLOSED portable field set for SQL `EXTRACT(<field> FROM <expr>)`.
///
/// Each admitted field has a live three-dialect proof and a faithful renderer on
/// PostgreSQL, SQLite, and MySQL. Fields with PostgreSQL-only semantics live in
/// [`PgExtractField`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ExtractField {
    /// Calendar year.
    Year,
    /// Month-of-year, 1-12.
    Month,
    /// Day-of-month field.
    Day,
    /// Hour-of-day, 0-23.
    Hour,
    /// Minute-of-hour, 0-59.
    Minute,
    /// Day-of-week, 0=Sunday through 6=Saturday.
    Dow,
}

/// CLOSED PostgreSQL-only field set for `EXTRACT(<field> FROM <expr>)`.
///
/// These fields either have no portable SQLite/MySQL analogue or have semantics
/// that diverge under the mandated portable renderers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PgExtractField {
    /// Seconds including fractional seconds on PostgreSQL.
    Second,
    /// Day-of-year.
    Doy,
    /// Seconds since 1970-01-01 00:00:00 UTC.
    Epoch,
    /// Calendar quarter.
    Quarter,
    /// ISO week number.
    Week,
    /// ISO day-of-week, 1=Monday through 7=Sunday.
    Isodow,
    /// ISO week-numbering year.
    Isoyear,
    /// Century.
    Century,
    /// Decade.
    Decade,
    /// Millennium.
    Millennium,
    /// Seconds field including fractional microseconds.
    Microseconds,
    /// Seconds field including fractional milliseconds.
    Milliseconds,
    /// Time-zone offset in seconds.
    Timezone,
    /// Time-zone offset hour.
    TimezoneHour,
    /// Time-zone offset minute.
    TimezoneMinute,
}

/// The CLOSED set of PORTABLE aggregate functions (`c.agg.*`).
///
/// `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` are byte-identical standard SQL on PostgreSQL,
/// SQLite, and MySQL (only the surrounding identifier quoting differs), so there
/// is NO dialect gate — an [`Expr::Agg`] validates and renders on all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum AggFunc {
    /// `count(...)` (or `count(*)` when the [`Expr::Agg`] `arg` is `None`).
    Count,
    /// `sum(<arg>)`.
    Sum,
    /// `avg(<arg>)`.
    Avg,
    /// `min(<arg>)`.
    Min,
    /// `max(<arg>)`.
    Max,
    /// `string_agg(<arg>, <delimiter>)` (PostgreSQL-first).
    StringAgg,
    /// `array_agg(<arg>)` (PostgreSQL-first).
    ArrayAgg,
    /// `bool_and(<arg>)` (PostgreSQL-first).
    BoolAnd,
    /// `bool_or(<arg>)` (PostgreSQL-first).
    BoolOr,
}

/// `skip_serializing_if` predicate: a `false` bool emits NOTHING on the wire, so a
/// non-`distinct` [`Expr::Agg`] serializes byte-minimally. The
/// bool `default`s to `false` on deserialize, so the round-trip is faithful.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// The CLOSED expression AST node. Internally tagged on `"node"`,
/// camel-cased (`{"node":"colRef","name":"first"}`). NO `untagged`, NO `flatten`
/// — same discipline as [`Op`](crate::ir::Op), so schemars derives a clean
/// discriminated union and serde rejects any out-of-set node tag at deserialize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(
    description = "The CLOSED expression AST node. Internally tagged on `\"node\"`,\ncamel-cased (`{\"node\":\"colRef\",\"name\":\"first\"}`). NO `untagged`, NO `flatten`\n— same discipline as [`Op`](crate::ir::Op), so schemars derives a clean\ndiscriminated union and serde rejects any out-of-set node tag at deserialize."
)]
#[serde(tag = "node", rename_all = "camelCase", rename_all_fields = "camelCase", deny_unknown_fields)]
pub enum Expr {
    /// A column reference (`c("name")`). The name is a plain string, resolved
    /// against the enclosing op's single target table at apply/render time
    /// — never `tsc`-bound to the live schema.
    ColRef {
        /// The column name (plain string).
        name: String,
        /// Optional qualifying table/alias (`c("orders", "customer_id")` →
        /// `table: Some("orders")`). Present only for the two-arg qualified form
        /// (the join-ON fix). An unqualified `c("col")` leaves this `None`
        /// and, via `skip_serializing_if`, serializes byte-identically to the
        /// pre-qualification wire shape — additive, no `ir_version` bump.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        table: Option<String>,
    },
    /// A typed scalar literal (a bare JS value auto-wrapped by a fluent operator
    /// method). Carries an [`IrScalar`] so the numeric domain is enforced at
    /// deserialize and the value folds into the checksum.
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
    /// A searched `CASE` (`c.case({ branches: [{ when, then }], else? })`). Each branch is
    /// `(when, then)`; both halves are themselves closed-AST nodes.
    Case {
        /// `(when, then)` branches, in order.
        branches: Vec<CaseBranch>,
        /// Optional `ELSE` result.
        #[serde(rename = "else", skip_serializing_if = "Option::is_none")]
        r#else: Option<Box<Expr>>,
    },
    /// An allow-listed named scalar function call.
    FnCall {
        /// The function (allow-listed).
        r#fn: ScalarFn,
        /// The argument expressions.
        args: Vec<Expr>,
    },
    /// An engine-SYNTHESIZED helper call. The per-dialect lowering is the
    /// engine's; the author never sees the rendered form.
    FnSynth {
        /// The synthesized function.
        r#fn: SynthFn,
        /// The argument expressions (a `splitPart`'s `delim`/`n` are `Literal`
        /// args, validated in-envelope structurally).
        args: Vec<Expr>,
    },
    /// A cast to a portable type (`.cast({ to: "int" })`).
    Cast {
        /// The expression being cast.
        operand: Box<Expr>,
        /// The portable target type.
        target: CastTarget,
    },
    /// A **portable** inclusive range test (`c("x").between(low, high)`) rendered
    /// exactly as `(<operand> BETWEEN <low> AND <high>)` — IDENTICAL SQL on PG,
    /// SQLite, and MySQL (standard SQL, inclusive on both ends).
    Between {
        /// The expression under test.
        operand: Box<Expr>,
        /// The inclusive lower bound.
        low: Box<Expr>,
        /// The inclusive upper bound.
        high: Box<Expr>,
    },
    /// A **portable** `LIKE` pattern match (`c("x").like(pattern)`) rendered exactly
    /// as `(<operand> LIKE <pattern>)` — the SAME syntax on PG, SQLite, and MySQL.
    ///
    /// NOTE: LIKE *case-sensitivity* semantics differ per dialect — PG is
    /// case-sensitive, SQLite is ASCII-case-insensitive by default, and MySQL is
    /// collation-dependent — so a dialect-uniform portability PROOF is a
    /// claiming-phase obligation. This adds the node + the
    /// faithful syntax render only; it does not yet claim cross-dialect parity.
    Like {
        /// The expression under test.
        operand: Box<Expr>,
        /// The LIKE pattern expression.
        pattern: Box<Expr>,
    },
    /// A **portable** NULL-safe inequality (`c("x").distinctFrom(y)`). The
    /// per-dialect lowering is the engine's job (this is the point): PG + SQLite
    /// render the standard `(<left> IS DISTINCT FROM <right>)`; MySQL has NO
    /// `IS DISTINCT FROM` operator, so it lowers to `(NOT (<left> <=> <right>))` —
    /// `<=>` is MySQL's NULL-safe equality, so its negation is exactly
    /// "distinct from" (NULL-aware inequality).
    DistinctFrom {
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// A **PORTABLE** aggregate function application (`c.agg.count()`,
    /// `c.agg.sum(e)`, `c.agg.count(e, { distinct: true })`).
    ///
    /// `COUNT`/`SUM`/`AVG`/`MIN`/`MAX` render byte-identically on PG, SQLite, and
    /// MySQL (only identifier quoting differs), so there is NO dialect gate. `arg:
    /// None` with `func: Count` is `COUNT(*)`; a present `arg` renders
    /// `<func>(<arg>)`, and `distinct` inserts `DISTINCT`
    /// (`count(DISTINCT <arg>)`).
    ///
    /// NB: the "an aggregate is only legal in a grouped/SELECT context" check is
    /// coupled with the view/select builder (`AGG_POSITION_INVALID`) — this
    /// additive slice accepts the node STRUCTURALLY only.
    Agg {
        /// The aggregate function.
        func: AggFunc,
        /// The single argument expression. `None` + `func: Count` = `COUNT(*)`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arg: Option<Box<Expr>>,
        /// Optional second argument for `StringAgg` only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delimiter: Option<Box<Expr>>,
        /// `DISTINCT` inside the aggregate (`count(DISTINCT <arg>)`). Skipped on the
        /// wire when `false` so the node serializes byte-minimally.
        #[serde(default, skip_serializing_if = "is_false")]
        distinct: bool,
    },
    /// A **portable** scalar-list membership predicate (`c("x").in([...])` /
    /// `c("x").notIn([...])`). PostgreSQL renders in the pg_dump-faithful
    /// `= ANY (ARRAY[...])` / `<> ALL (ARRAY[...])` shape; text elements keep an
    /// explicit `::text` cast. SQLite and MySQL render `IN (...)` / `NOT IN (...)`.
    /// Empty lists are defined:
    /// `IN []` renders false, `NOT IN []` renders true.
    InList {
        /// The expression tested against the literal scalar list.
        expr: Box<Expr>,
        /// Homogeneous scalar elements. Text elements serialize as plain JSON
        /// strings and PostgreSQL renders each as a safe string literal with an
        /// explicit `::text` cast to match pg_dump's CHECK/domain shape.
        elems: Vec<IrScalar>,
        /// `false` => membership (`IN` / `= ANY`); `true` => non-membership
        /// (`NOT IN` / `<> ALL`).
        negated: bool,
    },
    /// **PG-ONLY** regex match rendered exactly as
    /// `(<expr> ~ '<pattern>'::text)`.
    PgRegexMatch {
        /// The value expression to match.
        expr: Box<Expr>,
        /// The regex pattern. It is always rendered as a SQL string literal; never
        /// concatenated as free-form SQL.
        pattern: String,
    },
    /// **PG-ONLY** scalar expression `pg_column_size(<expr>)`.
    PgColumnSize {
        /// The expression whose on-disk size Postgres should measure.
        expr: Box<Expr>,
    },
    /// **PORTABLE** scalar expression extracting a date/time part whose numeric
    /// semantics are identical on PostgreSQL, SQLite, and MySQL.
    Extract {
        /// Closed EXTRACT field.
        field: ExtractField,
        /// Source expression.
        from: Box<Expr>,
    },
    /// **PG-ONLY** scalar expression `EXTRACT(<field> FROM <expr>)`.
    PgExtract {
        /// Closed PostgreSQL EXTRACT field.
        field: PgExtractField,
        /// Source expression.
        from: Box<Expr>,
    },
    /// **PG-ONLY** structured interval literal rendered as `INTERVAL '<parts>'`.
    PgInterval {
        /// Structured duration fields. Fields serialize in canonical order
        /// (`years`, `months`, `days`, `hours`, `minutes`, `seconds`) and absent
        /// fields are omitted from the wire.
        duration: Duration,
    },
    /// **The one Layer-2 portability escape** — a
    /// per-dialect VALUE divergence. Each present leg is a full [`Expr`]; the
    /// engine renders the leg matching the render's TARGET dialect — the
    /// dialect's own leg if present, else `default`. This is `dialect({ default?,
    /// pg?, sqlite?, mysql? })` in the builder (e.g. `default(dialect({ pg:
    /// c.fn.genRandomUuid(), sqlite: …, mysql: c.fn.uuid() }))`).
    ///
    /// The node tag on the wire is `"dialect"` (`#[serde(rename)]`); the four
    /// legs serialize in declaration order (`default, pg, sqlite, mysql`) — the
    /// canonical leg order the checksum folds. A leg that is `None` is skipped on
    /// the wire (`skip_serializing_if`), so a two-leg divergence is byte-minimal.
    ///
    /// **Scope math (validate, [`crate::model::validate`]).** The covered dialect
    /// set is `{legs present} ∪ {all dialects if default present}`; a target with
    /// NEITHER its own leg NOR a `default` is REFUSED fail-closed
    /// (`EXPR_NOT_PORTABLE`). This is a per-TARGET check: a `dialect()` missing
    /// the `sqlite` leg with no `default` is fine when targeting PG, refused when
    /// targeting SQLite. At least one leg must be present — a legless `dialect({})`
    /// is malformed on every target (`UNSUPPORTED`), enforced at validate (all
    /// four fields are `serde(default)` so an empty node deserializes, then the
    /// structural gate refuses it).
    ///
    /// **RATCHET OBLIGATION.** The design counts each
    /// `dialect()` leg as one of the four ratcheted budget counters. That budget
    /// / baseline mechanism is a LATER phase and is NOT YET BUILT (there is no
    /// baseline file in-tree). When it lands, the per-leg count of this node must
    /// be wired into it. Deferred by design — this additive slice does not gate
    /// on it.
    #[serde(rename = "dialect")]
    Dialectal {
        /// The fallback leg, rendered for any target dialect that has no explicit
        /// own leg. Its presence makes the covered set ALL dialects.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<Box<Expr>>,
        /// The PostgreSQL leg.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pg: Option<Box<Expr>>,
        /// The SQLite leg.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sqlite: Option<Box<Expr>>,
        /// The MySQL leg.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mysql: Option<Box<Expr>>,
    },
}

/// Structured duration value for PostgreSQL interval literals and future portable
/// date shifts. All fields are optional integers; validation requires at least
/// one present field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Duration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub years: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub months: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seconds: Option<i64>,
}

impl Duration {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.years.is_none()
            && self.months.is_none()
            && self.days.is_none()
            && self.hours.is_none()
            && self.minutes.is_none()
            && self.seconds.is_none()
    }
}

/// One `(when, then)` branch of a [`Expr::Case`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaseBranch {
    /// The `WHEN` predicate (a boolean closed-AST node).
    pub when: Expr,
    /// The `THEN` result.
    pub then: Expr,
}

impl Expr {
    /// A `ColRef` convenience constructor (tests / IrAuthor).
    #[must_use]
    pub fn col(name: impl Into<String>) -> Self {
        Expr::ColRef {
            name: name.into(),
            table: None,
        }
    }

    /// A qualified `ColRef` convenience constructor (`c("orders", "id")`).
    #[must_use]
    pub fn col_qualified(table: impl Into<String>, name: impl Into<String>) -> Self {
        Expr::ColRef {
            name: name.into(),
            table: Some(table.into()),
        }
    }

    /// A `Literal` convenience constructor.
    #[must_use]
    pub fn lit(value: IrScalar) -> Self {
        Expr::Literal { value }
    }
}
