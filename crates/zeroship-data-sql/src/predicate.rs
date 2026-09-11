//! Comparison and logical predicates shared by query plans.
//!
//! Null checks are distinct nodes. Canonicalization reduces empty conjunctions to
//! true, empty disjunctions to false, and empty membership to a constant. Ranges
//! become comparisons so equivalent predicates share a representation.
//!
//! Recursive consumers require a bounded tree. `depth` measures it iteratively,
//! and plan validation enforces `MAX_PREDICATE_DEPTH` before canonicalization or
//! rendering.

use crate::ident::Ident;
use crate::literal::{Literal, LiteralError, LiteralSet};
use crate::path::FieldPath;
use core::fmt;

/// Maximum predicate-tree depth accepted by plan validation.
/// Leaves have depth one; a connective adds to its deepest child's depth.
pub const MAX_PREDICATE_DEPTH: usize = 16;

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
    IsNull {
        operand: Operand,
        negated: bool,
    },
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

    /// How deep this tree is, with a leaf at 1.
    ///
    /// **Iterative, and that is the whole point.** This is the function a
    /// hostile tree is measured by, so it is the one function that must not
    /// recurse: a recursive depth check overflows on exactly the input it exists
    /// to refuse, and does so before it can return an error. The explicit stack
    /// below grows on the heap instead.
    #[must_use]
    pub fn depth(&self) -> usize {
        let mut deepest = 0_usize;
        let mut pending: Vec<(&Self, usize)> = vec![(self, 1)];
        while let Some((node, depth)) = pending.pop() {
            deepest = deepest.max(depth);
            match node {
                Self::And(children) | Self::Or(children) => {
                    pending.extend(children.iter().map(|child| (child, depth + 1)));
                }
                Self::Not(inner) => pending.push((inner.as_ref(), depth + 1)),
                // Every remaining variant is a leaf: an `Operand` cannot hold a
                // `Predicate`, so no other node contributes depth.
                Self::Compare { .. }
                | Self::Membership { .. }
                | Self::Pattern { .. }
                | Self::IsNull { .. }
                | Self::Const(_) => {}
            }
        }
        deepest
    }

    /// Refuse a tree deeper than [`MAX_PREDICATE_DEPTH`].
    ///
    /// # Errors
    ///
    /// [`PredicateError::TooDeep`].
    pub(crate) fn check_depth(&self) -> Result<(), PredicateError> {
        let depth = self.depth();
        if depth > MAX_PREDICATE_DEPTH {
            return Err(PredicateError::TooDeep { depth });
        }
        Ok(())
    }

    /// A conjunction, canonicalised.
    ///
    /// # Errors
    ///
    /// [`PredicateError::TooDeep`] past [`MAX_PREDICATE_DEPTH`]. The check runs
    /// **before** [`Predicate::canonical`], because canonicalisation is itself
    /// recursive - a depth check after it would be a check that never ran.
    ///
    /// Canonicalisation only ever flattens, so it cannot turn an accepted tree
    /// into a deeper one; checking the authored form is conservative in the safe
    /// direction.
    pub fn and(children: Vec<Self>) -> Result<Self, PredicateError> {
        let authored = Self::And(children);
        authored.check_depth()?;
        Ok(authored.canonical())
    }

    /// A disjunction, canonicalised.
    ///
    /// # Errors
    ///
    /// [`PredicateError::TooDeep`]; see [`Predicate::and`].
    pub fn or(children: Vec<Self>) -> Result<Self, PredicateError> {
        let authored = Self::Or(children);
        authored.check_depth()?;
        Ok(authored.canonical())
    }

    /// A negation, canonicalised.
    ///
    /// Named `negate` rather than `not` because an inherent `not` shadows
    /// `std::ops::Not::not` at every call site that has the trait in scope, and
    /// a reader cannot tell which one ran.
    ///
    /// # Errors
    ///
    /// [`PredicateError::TooDeep`]; see [`Predicate::and`].
    pub fn negate(inner: Self) -> Result<Self, PredicateError> {
        let authored = Self::Not(Box::new(inner));
        authored.check_depth()?;
        Ok(authored.canonical())
    }

    /// A bounded range, desugared to two comparisons.
    ///
    /// See the module note: this is a constructor rather than a node so that
    /// one logical plan has one spelling.
    ///
    /// Infallible, and provably so rather than by omission: the result is
    /// `And([Compare, Compare])`, whose depth is 2 whatever the operands are,
    /// because an [`Operand`] cannot hold a `Predicate`.
    #[must_use]
    pub fn range(lhs: Operand, low: Operand, high: Operand, bounds: RangeBounds) -> Self {
        let (low_op, high_op) = bounds.ops();
        Self::And(vec![
            Self::compare(lhs.clone(), low_op, low),
            Self::compare(lhs, high_op, high),
        ])
        .canonical()
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
        // Built from the variants rather than through `Self::or` / `Self::and`
        // so this stays infallible: the result is two leaves under one
        // connective, so its depth is 2 and the bound cannot be reached. Routing
        // it through the fallible constructors would add an error arm no input
        // can produce.
        Ok(match op {
            MembershipOp::In => Self::Or(vec![membership, null_test]).canonical(),
            MembershipOp::NotIn => Self::And(vec![membership, null_test]).canonical(),
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
    EscapeCharNotGraphic {
        character: char,
    },
    DistinctCountOnly {
        func: AggregateFunc,
    },
    /// The tree was deeper than [`MAX_PREDICATE_DEPTH`]. See the module note:
    /// depth, not width, is what turns a small request into an aborted worker.
    TooDeep {
        depth: usize,
    },
}

impl fmt::Display for PredicateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NulByteInPattern => f.write_str("a LIKE pattern must not contain a NUL byte"),
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
            Self::TooDeep { depth } => write!(
                f,
                "a predicate nested {depth} deep exceeds the maximum of \
                 {MAX_PREDICATE_DEPTH}"
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
        ])
        .expect("within the depth bound");
        assert_ne!(nested, flat, "the two inputs must really differ");
        assert_eq!(
            nested.canonical(),
            flat,
            "nesting, order and a duplicate must all normalise away"
        );
    }

    #[test]
    fn empty_connectives_take_their_own_identity() {
        assert_eq!(
            Predicate::and(vec![]).expect("shallow"),
            Predicate::Const(true)
        );
        assert_eq!(
            Predicate::or(vec![]).expect("shallow"),
            Predicate::Const(false)
        );
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
            Predicate::negate(Predicate::is_null(col("a"))).expect("shallow"),
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
        ])
        .expect("within the depth bound");
        assert_eq!(range, by_hand);
    }

    /// A conjunction of leaves is one level deeper than a leaf, and nesting
    /// through `Not` counts the same as nesting through a connective. The
    /// measurement has to be right before the bound built on it means anything.
    #[test]
    fn depth_counts_a_leaf_as_one_and_every_connective_as_one_more() {
        let leaf = Predicate::compare(col("a"), CompareOp::Eq, int(1));
        assert_eq!(leaf.depth(), 1);
        assert_eq!(Predicate::Const(true).depth(), 1);
        assert_eq!(Predicate::And(vec![leaf.clone()]).depth(), 2);
        assert_eq!(
            Predicate::Not(Box::new(Predicate::And(vec![leaf.clone()]))).depth(),
            3
        );
        // The DEEPEST child decides, not the first or the last one.
        let shallow = Predicate::And(vec![leaf.clone()]);
        let deep = Predicate::And(vec![Predicate::And(vec![leaf])]);
        assert_eq!(
            Predicate::Or(vec![shallow.clone(), deep.clone()]).depth(),
            4,
            "a deeper trailing child must win"
        );
        assert_eq!(
            Predicate::Or(vec![deep, shallow]).depth(),
            4,
            "a deeper leading child must win too"
        );
    }

    /// `range` and `membership` stay infallible, and this is the arm that keeps
    /// them honest: both are asserted to produce a tree well inside the bound,
    /// so the absence of an error arm is a fact about their output rather than
    /// an omission.
    #[test]
    fn the_infallible_constructors_cannot_approach_the_depth_bound() {
        let range = Predicate::range(col("age"), int(18), int(65), RangeBounds::InclusiveBoth);
        assert!(range.depth() <= 2, "range produced depth {}", range.depth());

        let with_null = Predicate::membership(
            col("status"),
            MembershipOp::In,
            vec![Some(Literal::Int(1)), None],
        )
        .expect("membership builds");
        assert!(
            with_null.depth() <= 2,
            "membership produced depth {}",
            with_null.depth()
        );
        // The margin is what makes the two assertions above meaningful rather
        // than a restatement of the bound.
        const { assert!(MAX_PREDICATE_DEPTH > 2) };
    }
}
