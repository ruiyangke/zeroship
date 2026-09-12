//! Values. Every one of them becomes a bind parameter; none of them becomes
//! SQL text.
//!
//! # There is no null
//!
//! [`Literal`] has no `Null` variant. A null is not a
//! [`Literal`], so it cannot be an [`crate::sql::Operand`], so it cannot be a
//! comparison's right-hand side. Nullness is expressible only as
//! `Predicate::IsNull`, and the conversion boundary
//! ([`Literal::from_optional`]) forces the caller to say which of the two they
//! meant.

use core::fmt;

/// The largest membership list accepted by the query grammar.
pub const MAX_MEMBERSHIP_LIST_LEN: usize = 100;

/// A finite `f64`.
///
/// Wrapping the float is what lets [`Literal`] carry a total order, which
/// canonical predicate construction needs and which `f64` cannot
/// provide. Non-finite values are refused at construction rather than ordered
/// arbitrarily: a NaN parameter compares equal to nothing, including itself, so
/// a filter carrying one returns no rows for a reason no test would explain.
#[derive(Debug, Clone, Copy)]
pub struct Finite(f64);

impl Finite {
    /// Wrap a float, refusing NaN and the infinities.
    ///
    /// # Errors
    ///
    /// [`LiteralError::NonFiniteFloat`] if `value` is NaN or infinite.
    pub const fn new(value: f64) -> Result<Self, LiteralError> {
        if value.is_finite() {
            Ok(Self(value))
        } else {
            Err(LiteralError::NonFiniteFloat)
        }
    }

    /// The wrapped value.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

// Hand-written rather than derived: `f64` has no `Eq`/`Ord`, and the ordering
// the canonical form needs must be TOTAL. `total_cmp` provides that, and
// because non-finite values are refused above, the only value pair it orders
// differently from `PartialOrd` is `-0.0` against `0.0`. Those two compare
// EQUAL under `==` but not under `total_cmp`, so `Eq` is defined from
// `total_cmp` to keep `Ord` and `Eq` consistent - an inconsistency there is
// undefined behaviour for `BTreeMap` and friends, not merely surprising.
impl PartialEq for Finite {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == core::cmp::Ordering::Equal
    }
}

impl Eq for Finite {}

impl PartialOrd for Finite {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Finite {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl core::hash::Hash for Finite {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

/// A finite `f32`, for the elements of a [`QueryVector`].
///
/// The same argument as [`Finite`] and the same hand-written impls, at the
/// width the value actually has. An embedding is `f32` at every layer that
/// carries it - `VectorIndex::vector_search` takes `&[f32]`
/// (`crates/zeroship-data-orm/src/backend/mod.rs`) and `vec0` reads a
/// buffer of `f32` (`backend/sqlite/vector.rs:127`) - so widening to `f64` here
/// and narrowing again at the driver would make the plan's value and the bound
/// value different numbers for no gain.
#[derive(Debug, Clone, Copy)]
pub struct Finite32(f32);

impl Finite32 {
    /// Wrap a float, refusing NaN and the infinities.
    ///
    /// # Errors
    ///
    /// [`LiteralError::NonFiniteFloat`] if `value` is NaN or infinite.
    pub const fn new(value: f32) -> Result<Self, LiteralError> {
        if value.is_finite() {
            Ok(Self(value))
        } else {
            Err(LiteralError::NonFiniteFloat)
        }
    }

    /// The wrapped value.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

// Hand-written for the reason given on `Finite`: `f32` has no `Eq`/`Ord`, and
// a canonical plan needs a TOTAL one.
impl PartialEq for Finite32 {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == core::cmp::Ordering::Equal
    }
}

impl Eq for Finite32 {}

impl PartialOrd for Finite32 {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Finite32 {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl core::hash::Hash for Finite32 {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

/// The most dimensions a query vector may carry.
pub const MAX_VECTOR_DIMS: usize = 16_000;

/// A query vector: non-empty, finite in every element, and bounded in width.
///
/// The compiler preserves component order and binds a native array. Database
/// codecs and search compilers own its physical representation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct QueryVector(Vec<Finite32>);

impl QueryVector {
    /// Wrap a query vector.
    ///
    /// # Errors
    ///
    /// [`LiteralError::EmptyQueryVector`] for a zero-dimension vector - the
    /// `vector` type has no such value and the distance would be undefined
    /// rather than zero. [`LiteralError::NonFiniteFloat`] if any element is NaN
    /// or infinite: `pgvector` refuses both outright, and a NaN distance sorts
    /// arbitrarily under `ORDER BY`, which is a ranking that silently means
    /// nothing. [`LiteralError::QueryVectorTooWide`] past
    /// [`MAX_VECTOR_DIMS`].
    pub fn new(values: &[f32]) -> Result<Self, LiteralError> {
        if values.is_empty() {
            return Err(LiteralError::EmptyQueryVector);
        }
        if values.len() > MAX_VECTOR_DIMS {
            return Err(LiteralError::QueryVectorTooWide { dims: values.len() });
        }
        let mut out = Vec::with_capacity(values.len());
        for value in values {
            out.push(Finite32::new(*value)?);
        }
        Ok(Self(out))
    }

    /// The elements, in the order given.
    ///
    /// **Not sorted, and never will be.** Every other list in this crate is
    /// canonicalised by sorting; a vector's element order *is* its meaning, so
    /// sorting one would silently change which rows a search returns. It is
    /// called out here because the surrounding convention makes the omission
    /// look like one.
    #[must_use]
    pub fn elements(&self) -> &[Finite32] {
        &self.0
    }

    /// How many dimensions. Never zero.
    #[must_use]
    pub const fn dims(&self) -> usize {
        self.0.len()
    }
}

/// A bind parameter value.
///
/// The variant set is deliberately small. It covers what the runtime query
/// builders bind today and nothing else; a type this crate cannot represent is
/// a refusal at the boundary, not a `Text` fallback carrying a hand-rolled
/// encoding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Literal {
    Bool(bool),
    Int(i64),
    Float(Finite),
    Text(String),
    /// Encoded JSON, distinct from an ordinary text parameter.
    Json(String),
    Bytes(Vec<u8>),
    /// An embedding, bound whole. See [`QueryVector`].
    Vector(QueryVector),
}

impl Literal {
    /// A text value.
    ///
    /// # Errors
    ///
    /// [`LiteralError::NulByteInText`] if the string contains a NUL. `PostgreSQL`
    /// `text` cannot hold one, so binding it fails in the driver with an
    /// encoding error that names neither the column nor the filter; refusing
    /// here turns that into a typed error at the point the value entered.
    pub fn text(value: impl Into<String>) -> Result<Self, LiteralError> {
        let value = value.into();
        if value.contains('\0') {
            return Err(LiteralError::NulByteInText);
        }
        Ok(Self::Text(value))
    }

    /// A float value.
    ///
    /// # Errors
    ///
    /// [`LiteralError::NonFiniteFloat`] for NaN or an infinity.
    pub fn float(value: f64) -> Result<Self, LiteralError> {
        Finite::new(value).map(Self::Float)
    }

    /// The conversion boundary a caller crossing from a nullable source must
    /// use.
    ///
    /// `None` is not a [`Literal`] and never becomes one. The signature is the
    /// point: a caller holding "a value that might be null" cannot get a
    /// `Literal` out of this without handling the `None` arm, and the only
    /// thing they can do with it is build a `Predicate::IsNull`.
    #[must_use]
    pub const fn from_optional(value: Option<Self>) -> Option<Self> {
        value
    }

    /// The logical type name, used for the homogeneity message on
    /// [`LiteralSet`] and in refusals.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::Text(_) => "text",
            Self::Json(_) => "json",
            Self::Bytes(_) => "bytes",
            Self::Vector(_) => "vector",
        }
    }
}

/// A non-empty, homogeneous, bounded set of values for a membership test.
///
/// Its invariants travel with the type rather than being rechecked by each caller:
///
/// * **Non-empty.** [`crate::sql::Predicate::membership`] simplifies an empty
///   input to a constant before a set is built.
/// * **Homogeneous.** A set mixing `Int` and `Text` binds parameters of two
///   types into one `IN` list, where `PostgreSQL` resolves a single type for the
///   whole list and the mismatch surfaces as a cast error at execution.
/// * **Bounded** by [`MAX_MEMBERSHIP_LIST_LEN`].
///
/// The values are stored sorted and deduplicated. Membership is a set test, so
/// neither transformation changes the result, and both are needed for the
/// stable statement property: equivalent sets produce one SQL shape.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LiteralSet(Vec<Literal>);

impl LiteralSet {
    /// Build a set.
    ///
    /// # Errors
    ///
    /// [`LiteralError::EmptyLiteralSet`], [`LiteralError::HeterogeneousSet`],
    /// or [`LiteralError::MembershipListTooLong`].
    pub fn new(values: Vec<Literal>) -> Result<Self, LiteralError> {
        let Some(first) = values.first() else {
            return Err(LiteralError::EmptyLiteralSet);
        };
        let expected = first.type_name();
        if let Some(odd) = values.iter().find(|v| v.type_name() != expected) {
            return Err(LiteralError::HeterogeneousSet {
                expected,
                found: odd.type_name(),
            });
        }
        // The cap is checked on the AUTHORED length, before deduplication. A
        // caller who sends 500 copies of one value has still sent 500 values,
        // and silently accepting that because it collapses to one would make
        // the bound depend on the data rather than on the request.
        if values.len() > MAX_MEMBERSHIP_LIST_LEN {
            return Err(LiteralError::MembershipListTooLong { len: values.len() });
        }
        let mut values = values;
        values.sort();
        values.dedup();
        Ok(Self(values))
    }

    /// The values, sorted and deduplicated.
    #[must_use]
    pub fn values(&self) -> &[Literal] {
        &self.0
    }

    /// How many values the set holds. Never zero.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// Always `false`. Present because `clippy::len_without_is_empty` asks for
    /// it, and it documents the invariant while it is there.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }
}

/// Why a value or a set was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiteralError {
    /// NaN or an infinity was offered as a float.
    NonFiniteFloat,
    /// A text value carried a NUL byte.
    NulByteInText,
    /// A membership set had no members. Not representable: see
    /// [`crate::sql::Predicate::membership`], which simplifies that case away
    /// before a set is built.
    EmptyLiteralSet,
    /// A membership set mixed logical types.
    HeterogeneousSet {
        expected: &'static str,
        found: &'static str,
    },
    /// A membership set exceeded [`MAX_MEMBERSHIP_LIST_LEN`].
    MembershipListTooLong { len: usize },
    /// A query vector with no dimensions.
    EmptyQueryVector,
    /// A query vector wider than [`MAX_VECTOR_DIMS`].
    QueryVectorTooWide { dims: usize },
}

impl fmt::Display for LiteralError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteFloat => f.write_str(
                "a float parameter must be finite: NaN compares equal to nothing, \
                 including itself",
            ),
            Self::NulByteInText => f.write_str("a text parameter must not contain a NUL byte"),
            Self::EmptyLiteralSet => f.write_str(
                "a membership set must not be empty; empty membership is a constant, \
                 not an IN list",
            ),
            Self::HeterogeneousSet { expected, found } => write!(
                f,
                "a membership set must be homogeneous: expected {expected}, found {found}"
            ),
            Self::MembershipListTooLong { len } => write!(
                f,
                "a membership set of {len} values exceeds the maximum of \
                 {MAX_MEMBERSHIP_LIST_LEN}"
            ),
            Self::EmptyQueryVector => f.write_str(
                "a query vector must have at least one dimension; the distance to a \
                 zero-dimension vector is undefined, not zero",
            ),
            Self::QueryVectorTooWide { dims } => write!(
                f,
                "a query vector of {dims} dimensions exceeds the maximum of \
                 {MAX_VECTOR_DIMS}, which is the widest `vector` column pgvector will \
                 create"
            ),
        }
    }
}

impl std::error::Error for LiteralError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The total order keeps canonical predicate construction deterministic.
    /// `-0.0` and `0.0` are the pair
    /// that separates `total_cmp` from `PartialOrd`.
    #[test]
    fn the_float_order_is_total_and_consistent_with_eq() {
        let neg = Finite::new(-0.0).expect("finite");
        let pos = Finite::new(0.0).expect("finite");
        assert_ne!(
            neg, pos,
            "Eq must agree with Ord, which separates the zeroes"
        );
        assert!(neg < pos);
        assert_eq!(neg.cmp(&neg), core::cmp::Ordering::Equal);
    }

    #[test]
    fn non_finite_floats_are_refused() {
        assert_eq!(Finite::new(f64::NAN), Err(LiteralError::NonFiniteFloat));
        assert_eq!(
            Finite::new(f64::INFINITY),
            Err(LiteralError::NonFiniteFloat)
        );
        assert_eq!(
            Finite::new(f64::NEG_INFINITY),
            Err(LiteralError::NonFiniteFloat)
        );
    }
}
