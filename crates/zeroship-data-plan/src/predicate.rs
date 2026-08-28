//! The expression sub-grammar.
//!
//! SC-3 pins this level rather than leaving it to the first family that ports,
//! and the argument is worth restating because it is the reason this module is
//! the largest one here: filters, projections, ordering keys and search
//! predicates all compose the same comparison, logical, path and literal nodes,
//! so a shared node fixed wrongly by whoever ports first costs every family
//! after it, whereas a family that gets its own node shape wrong costs that
//! family a revision.
//!
//! # The three shapes that encode defects rather than preferences
//!
//! * **[`Predicate::IsNull`] is a distinct node, and no [`Operand`] can be
//!   null.** Nullness is not an operator here. That is what makes the
//!   `$in: [null]`-binds-the-empty-string defect unrepresentable rather than
//!   merely absent - see [`crate::literal`].
//! * **`And([])` is `TRUE` and `Or([])` is `FALSE`**, deliberately not the same
//!   constant, so a renderer does not get to invent a convention. Both are
//!   normalised to [`Predicate::Const`] by [`Predicate::canonical`], so the two
//!   spellings render identically.
//! * **Empty membership is not a `Membership`.** [`Predicate::membership`]
//!   simplifies it to a constant before a [`crate::LiteralSet`] exists, so
//!   `IN ()` - a `PostgreSQL` syntax error, which failed the entire query rather
//!   than returning nothing - has no representation.
//!
//! # What is deliberately NOT a node
//!
//! SC-3 lists a `Range { lhs, low, high, inclusive }` variant. It is a
//! constructor here ([`Predicate::range`]) that desugars to a conjunction of
//! two comparisons, because a separate node would give one logical plan two
//! spellings - `range(x, 1, 10)` and `and([x >= 1, x <= 10])` are the same
//! query - and the canonical-form property in [`crate::render`] rests on there
//! being exactly one. A node that a canonicaliser has to desugar anyway is a
//! node whose render arm is unreachable.

use crate::ident::Ident;
use crate::literal::{Literal, LiteralError, LiteralSet};
use crate::path::FieldPath;
use core::fmt;

/// A comparison operator. Null is not among them; see [`Predicate::IsNull`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
}

impl CompareOp {
    /// The operator that means the same thing with the operands exchanged.
    /// Used by [`Predicate::canonical`] so `1 < x` and `x > 1` are one plan.
    const fn mirrored(self) -> Self {
        match self {
            Self::Eq => Self::Eq,
            Self::Ne => Self::Ne,
            Self::Lt => Self::Gt,
            Self::Lte => Self::Gte,
            Self::Gt => Self::Lt,
            Self::Gte => Self::Lte,
        }
    }
}

/// Set membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MembershipOp {
    In,
    NotIn,
}

/// Pattern matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PatternOp {
    Like,
    NotLike,
    /// Case-insensitive. `PostgreSQL`-only; a backend without it must refuse
    /// rather than emulate, per SC-3's decision 2.
    ILike,
    NotILike,
}

/// Which endpoints a [`Predicate::range`] includes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RangeBounds {
    InclusiveBoth,
    ExclusiveBoth,
    LowInclusiveHighExclusive,
    LowExclusiveHighInclusive,
}

impl RangeBounds {
    const fn ops(self) -> (CompareOp, CompareOp) {
        match self {
            Self::InclusiveBoth => (CompareOp::Gte, CompareOp::Lte),
            Self::ExclusiveBoth => (CompareOp::Gt, CompareOp::Lt),
            Self::LowInclusiveHighExclusive => (CompareOp::Gte, CompareOp::Lt),
            Self::LowExclusiveHighInclusive => (CompareOp::Gt, CompareOp::Lte),
        }
    }
}

/// A `LIKE` pattern.
///
/// A validated newtype rather than a `String` for the same reason everything
/// else here is one: a bare string reaching a renderer is the shape this crate
/// exists to remove. The pattern is bound as a parameter, so its content is
/// never parsed as SQL - what this type fences is the NUL byte `PostgreSQL`
/// `text` cannot carry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextPattern(String);

impl TextPattern {
    /// # Errors
    ///
    /// [`PredicateError::NulByteInPattern`].
    pub fn new(raw: impl Into<String>) -> Result<Self, PredicateError> {
        let raw = raw.into();
        if raw.contains('\0') {
            return Err(PredicateError::NulByteInPattern);
        }
        Ok(Self(raw))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The single character that escapes a `LIKE` wildcard.
///
/// It is rendered as a **bind parameter** (`... LIKE $1 ESCAPE $2`), not
/// interpolated into the statement. That matters more than it looks: an escape
/// character interpolated as a quoted literal is the one place in a `LIKE`
/// lowering where caller-chosen text would otherwise reach the SQL string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EscapeChar(char);

impl EscapeChar {
    /// # Errors
    ///
    /// [`PredicateError::EscapeCharNotGraphic`] for anything outside the
    /// printable ASCII range. A control character or a multi-byte character
    /// here is never a deliberate choice, and a NUL would be refused by the
    /// driver rather than by us.
    pub const fn new(c: char) -> Result<Self, PredicateError> {
        if c.is_ascii_graphic() {
            Ok(Self(c))
        } else {
            Err(PredicateError::EscapeCharNotGraphic { character: c })
        }
    }

    #[must_use]
    pub const fn get(self) -> char {
        self.0
    }
}

/// An aggregate function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AggregateFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggregateFunc {
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::Count => "COUNT",
            Self::Sum => "SUM",
            Self::Avg => "AVG",
            Self::Min => "MIN",
            Self::Max => "MAX",
        }
    }
}

/// An aggregate over a column, or `COUNT(*)`.
///
/// An aggregate is legal only in a `HAVING` position or a projection; SC-3
/// names that as one of the invariants the shapes cannot enforce and that
/// therefore needs a test. It is enforced in [`crate::Select`]'s constructor,
/// with `aggregate_operand_in_a_filter_is_refused` covering it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AggregateRef {
    func: AggregateFunc,
    argument: Option<Ident>,
    distinct: bool,
}

impl AggregateRef {
    /// `COUNT(*)`.
    #[must_use]
    pub const fn count_rows() -> Self {
        Self {
            func: AggregateFunc::Count,
            argument: None,
            distinct: false,
        }
    }

    /// An aggregate over one column.
    ///
    /// # Errors
    ///
    /// [`PredicateError::DistinctCountOnly`] if `distinct` is set on anything
    /// but `COUNT`. `SUM(DISTINCT x)` and `AVG(DISTINCT x)` are legal SQL but
    /// almost never what a caller means, and this crate does not carry a node
    /// whose most likely use is a mistake.
    pub fn over(
        func: AggregateFunc,
        column: Ident,
        distinct: bool,
    ) -> Result<Self, PredicateError> {
        if distinct && func != AggregateFunc::Count {
            return Err(PredicateError::DistinctCountOnly { func });
        }
        Ok(Self {
            func,
            argument: Some(column),
            distinct,
        })
    }

    #[must_use]
    pub const fn func(&self) -> AggregateFunc {
        self.func
    }

    #[must_use]
    pub const fn argument(&self) -> Option<&Ident> {
        self.argument.as_ref()
    }

    #[must_use]
    pub const fn is_distinct(&self) -> bool {
        self.distinct
    }
}

/// One side of a comparison.
///
/// **The variant order is load-bearing.** The derived `Ord` is what
/// [`Predicate::canonical`] orients comparisons by, and `Lit` is deliberately
/// last: a literal therefore always sorts after a column or an aggregate, so
/// `1 = x` canonicalises to `x = 1` and `5 < COUNT(*)` to `COUNT(*) > 5`,
/// rather than the other way round. Reordering these variants is not a
/// cosmetic change - it rewrites every statement this crate emits, and with
/// them every prepared-statement cache key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operand {
    Path(FieldPath),
    Aggregate(AggregateRef),
    Lit(Literal),
}

impl Operand {
    /// Shorthand for a plain column reference.
    #[must_use]
    pub const fn column(name: Ident) -> Self {
        Self::Path(FieldPath::column(name))
    }

    /// Whether this operand, or anything below it, is an aggregate.
    #[must_use]
    pub const fn is_aggregate(&self) -> bool {
        matches!(self, Self::Aggregate(_))
    }
}

/// A boolean expression over a collection's rows.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Predicate {
    /// Conjunction. Empty renders `TRUE`.
    And(Vec<Self>),
    /// Disjunction. Empty renders `FALSE` - deliberately not `And`'s constant.
    Or(Vec<Self>),
    Not(Box<Self>),
    /// Neither side may be null: [`Literal`] has no null variant.
    Compare {
        lhs: Operand,
        op: CompareOp,
        rhs: Operand,
    },
    /// Set membership. The set is non-empty by construction.
    Membership {
        lhs: Operand,
        op: MembershipOp,
        set: LiteralSet,
    },
    Pattern {
        lhs: Operand,
        op: PatternOp,
        pattern: TextPattern,
        escape: Option<EscapeChar>,
    },
    /// The only way to test nullness. A node, not an operator.
    IsNull { operand: Operand, negated: bool },
    /// Produced by simplification. Authoring one is legal and harmless, but
    /// nothing in this crate needs to.
    Const(bool),
}

impl Predicate {
    /// The always-true predicate.
    #[must_use]
    pub const fn always() -> Self {
        Self::Const(true)
    }

    /// The never-true predicate.
    #[must_use]
    pub const fn never() -> Self {
        Self::Const(false)
    }

    /// A comparison, canonicalised.
    #[must_use]
    pub fn compare(lhs: Operand, op: CompareOp, rhs: Operand) -> Self {
        Self::Compare { lhs, op, rhs }.canonical()
    }

    /// A nullness test.
    #[must_use]
    pub const fn is_null(operand: Operand) -> Self {
        Self::IsNull {
            operand,
            negated: false,
        }
    }

    /// A not-null test.
    #[must_use]
    pub const fn is_not_null(operand: Operand) -> Self {
        Self::IsNull {
            operand,
            negated: true,
        }
    }

    /// A conjunction, canonicalised.
    #[must_use]
    pub fn and(children: Vec<Self>) -> Self {
        Self::And(children).canonical()
    }

    /// A disjunction, canonicalised.
    #[must_use]
    pub fn or(children: Vec<Self>) -> Self {
        Self::Or(children).canonical()
    }

    /// A negation, canonicalised.
    ///
    /// Named `negate` rather than `not` because an inherent `not` shadows
    /// `std::ops::Not::not` at every call site that has the trait in scope, and
    /// a reader cannot tell which one ran.
    #[must_use]
    pub fn negate(inner: Self) -> Self {
        Self::Not(Box::new(inner)).canonical()
    }

    /// A bounded range, desugared to two comparisons.
    ///
    /// See the module note: this is a constructor rather than a node so that
    /// one logical plan has one spelling.
    #[must_use]
    pub fn range(lhs: Operand, low: Operand, high: Operand, bounds: RangeBounds) -> Self {
        let (low_op, high_op) = bounds.ops();
        Self::and(vec![
            Self::compare(lhs.clone(), low_op, low),
            Self::compare(lhs, high_op, high),
        ])
    }

    /// Membership, with the null rule applied at construction.
    ///
    /// `members` is a list of **optional** literals, and that signature is the
    /// whole mechanism: a caller translating a wire array cannot get a null
    /// past this without the `None` arm being handled, because a null is not a
    /// [`Literal`] and never becomes one. The partition below reproduces the
    /// lowering `query.rs` arrived at after the defect
    /// (`query.rs:5386-5409` for `In`, `:5424-5444` for `NotIn`):
    ///
    /// | values | nulls | `In`                          | `NotIn`                              |
    /// | ------ | ----- | ----------------------------- | ------------------------------------ |
    /// | none   | none  | `FALSE`                       | `TRUE`                               |
    /// | none   | some  | `x IS NULL`                   | `x IS NOT NULL`                      |
    /// | some   | none  | `x IN (..)`                   | `x NOT IN (..)`                      |
    /// | some   | some  | `(x IN (..) OR x IS NULL)`    | `(x NOT IN (..) AND x IS NOT NULL)`  |
    ///
    /// The bottom row is **not** what SQL does natively - `x IN (NULL)` matches
    /// nothing on `PostgreSQL` - so lowering a null member literally would revert
    /// behaviour this project shipped and tested.
    ///
    /// # Errors
    ///
    /// [`LiteralError`] if the non-null members are heterogeneous or exceed
    /// [`crate::literal::MAX_MEMBERSHIP_LIST_LEN`].
    pub fn membership(
        lhs: Operand,
        op: MembershipOp,
        members: Vec<Option<Literal>>,
    ) -> Result<Self, LiteralError> {
        let saw_null = members.iter().any(Option::is_none);
        let values: Vec<Literal> = members.into_iter().flatten().collect();
        let negated = matches!(op, MembershipOp::NotIn);

        if values.is_empty() {
            return Ok(if saw_null {
                Self::IsNull {
                    operand: lhs,
                    negated,
                }
            } else {
                // `$in: []` matches nothing; `$nin: []` excludes nothing.
                Self::Const(negated)
            });
        }

        let set = LiteralSet::new(values)?;
        let membership = Self::Membership {
            lhs: lhs.clone(),
            op,
            set,
        };
        if !saw_null {
            return Ok(membership);
        }
        let null_test = Self::IsNull {
            operand: lhs,
            negated,
        };
        Ok(match op {
            MembershipOp::In => Self::or(vec![membership, null_test]),
            MembershipOp::NotIn => Self::and(vec![membership, null_test]),
        })
    }

    /// Whether this predicate mentions an aggregate anywhere.
    #[must_use]
    pub fn mentions_aggregate(&self) -> bool {
        match self {
            Self::And(children) | Self::Or(children) => {
                children.iter().any(Self::mentions_aggregate)
            }
            Self::Not(inner) => inner.mentions_aggregate(),
            Self::Compare { lhs, rhs, .. } => lhs.is_aggregate() || rhs.is_aggregate(),
            Self::Membership { lhs, .. }
            | Self::Pattern { lhs, .. }
            | Self::IsNull { operand: lhs, .. } => lhs.is_aggregate(),
            Self::Const(_) => false,
        }
    }

    /// The canonical form of this predicate.
    ///
    /// Two predicates that mean the same thing must produce the same SQL, or
    /// the driver's prepared-statement cache sees two entries for one logical
    /// operation and hits neither. Canonicalisation is what makes that true,
    /// and every transformation below preserves meaning:
    ///
    /// * **flatten** nested `And`/`Or` of the same connective (associativity),
    ///   so `And([And([a, b]), c])` and `And([a, b, c])` converge;
    /// * **absorb** constants - `And` drops `TRUE` and collapses on `FALSE`,
    ///   `Or` the dual;
    /// * **sort and deduplicate** children (commutativity and idempotence), so
    ///   `And([b, a])` and `And([a, b])` converge. SQL does not promise an
    ///   evaluation order for `AND` in the first place, so nothing observable
    ///   depends on the authored order;
    /// * **unwrap** a one-child connective and turn an empty one into its
    ///   identity - `And([])` to `TRUE`, `Or([])` to `FALSE`;
    /// * **push `Not` through** a constant, a double negation, and an
    ///   `IsNull`. The last is exact rather than approximate: `IS NULL` never
    ///   evaluates to NULL, so negating it is ordinary two-valued logic;
    /// * **orient comparisons** so the smaller operand leads, mirroring the
    ///   operator - `1 < x` becomes `x > 1`.
    ///
    /// Notably absent: no De Morgan rewriting and no reordering of anything
    /// order-sensitive. `ORDER BY` keys are never touched, here or anywhere.
    #[must_use]
    pub fn canonical(self) -> Self {
        match self {
            Self::And(children) => Self::canonical_connective(children, true),
            Self::Or(children) => Self::canonical_connective(children, false),
            Self::Not(inner) => match inner.canonical() {
                Self::Const(b) => Self::Const(!b),
                Self::Not(double) => *double,
                Self::IsNull { operand, negated } => Self::IsNull {
                    operand,
                    negated: !negated,
                },
                other => Self::Not(Box::new(other)),
            },
            Self::Compare { lhs, op, rhs } => {
                if rhs < lhs {
                    Self::Compare {
                        lhs: rhs,
                        op: op.mirrored(),
                        rhs: lhs,
                    }
                } else {
                    Self::Compare { lhs, op, rhs }
                }
            }
            // Already canonical: `LiteralSet` sorts and deduplicates at
            // construction, and the remaining variants have no reorderable
            // structure.
            other => other,
        }
    }

    /// The shared body of `And`/`Or` canonicalisation. `conjunction` selects
    /// which constant is the identity and which is absorbing.
    fn canonical_connective(children: Vec<Self>, conjunction: bool) -> Self {
        let identity = conjunction;
        let mut flat: Vec<Self> = Vec::with_capacity(children.len());
        for child in children {
            match child.canonical() {
                Self::And(inner) if conjunction => flat.extend(inner),
                Self::Or(inner) if !conjunction => flat.extend(inner),
                Self::Const(b) if b == identity => {}
                Self::Const(_) => return Self::Const(!identity),
                other => flat.push(other),
            }
        }
        flat.sort();
        flat.dedup();
        match flat.len() {
            0 => Self::Const(identity),
            1 => flat.pop().unwrap_or(Self::Const(identity)),
            _ => {
                if conjunction {
                    Self::And(flat)
                } else {
                    Self::Or(flat)
                }
            }
        }
    }
}

/// Why a predicate part was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PredicateError {
    NulByteInPattern,
    EscapeCharNotGraphic { character: char },
    DistinctCountOnly { func: AggregateFunc },
}

impl fmt::Display for PredicateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NulByteInPattern => {
                f.write_str("a LIKE pattern must not contain a NUL byte")
            }
            Self::EscapeCharNotGraphic { character } => write!(
                f,
                "a LIKE escape character must be printable ASCII, not '{}'",
                character.escape_debug()
            ),
            Self::DistinctCountOnly { func } => write!(
                f,
                "DISTINCT is supported only on COUNT, not on {}",
                func.as_sql()
            ),
        }
    }
}

impl std::error::Error for PredicateError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::IdentRole;

    fn col(name: &str) -> Operand {
        Operand::column(Ident::parse_as(name, IdentRole::Column).expect("valid column"))
    }

    fn int(v: i64) -> Operand {
        Operand::Lit(Literal::Int(v))
    }

    /// Flattening, sorting and deduplication must each be able to change
    /// something, or `canonical` is an identity function that every
    /// determinism test would pass vacuously.
    #[test]
    fn canonicalisation_can_actually_change_a_predicate() {
        let nested = Predicate::And(vec![
            Predicate::And(vec![
                Predicate::compare(col("b"), CompareOp::Eq, int(2)),
                Predicate::compare(col("a"), CompareOp::Eq, int(1)),
            ]),
            Predicate::compare(col("a"), CompareOp::Eq, int(1)),
        ]);
        let flat = Predicate::and(vec![
            Predicate::compare(col("a"), CompareOp::Eq, int(1)),
            Predicate::compare(col("b"), CompareOp::Eq, int(2)),
        ]);
        assert_ne!(nested, flat, "the two inputs must really differ");
        assert_eq!(
            nested.canonical(),
            flat,
            "nesting, order and a duplicate must all normalise away"
        );
    }

    #[test]
    fn empty_connectives_take_their_own_identity() {
        assert_eq!(Predicate::and(vec![]), Predicate::Const(true));
        assert_eq!(Predicate::or(vec![]), Predicate::Const(false));
    }

    #[test]
    fn a_comparison_is_oriented_so_the_column_leads() {
        let literal_first = Predicate::compare(int(1), CompareOp::Lt, col("a"));
        let column_first = Predicate::compare(col("a"), CompareOp::Gt, int(1));
        assert_eq!(literal_first, column_first);
    }

    #[test]
    fn negating_a_null_test_flips_the_node_rather_than_wrapping_it() {
        assert_eq!(
            Predicate::negate(Predicate::is_null(col("a"))),
            Predicate::is_not_null(col("a"))
        );
    }

    /// A range desugars to exactly the conjunction a caller could have written
    /// by hand, which is the property that lets the node be absent.
    #[test]
    fn a_range_is_the_same_plan_as_the_conjunction_it_desugars_to() {
        let range = Predicate::range(
            col("age"),
            int(18),
            int(65),
            RangeBounds::LowInclusiveHighExclusive,
        );
        let by_hand = Predicate::and(vec![
            Predicate::compare(col("age"), CompareOp::Gte, int(18)),
            Predicate::compare(col("age"), CompareOp::Lt, int(65)),
        ]);
        assert_eq!(range, by_hand);
    }
}
