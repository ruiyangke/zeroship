//! The plan itself.
//!
//! # `DbPlan` is in-process only
//!
//! SC-3's decision 3: the moment a plan crosses a process boundary it is a wire
//! format, and a wire format needs versioning, compatibility rules and a
//! migration story - the exact burden the IR exists to avoid. Deriving
//! `Serialize` for a debug dump or a test fixture is a one-line change that
//! silently creates that obligation.
//!
//! Here the derive is not one line away, it is impossible: this crate declares
//! no dependencies at all, so `serde` is not in scope. The `Debug` output is
//! the only inspection path and is explicitly not a format anyone may parse.
//!
//! # What is NOT here
//!
//! SC-3 names six families: read, relation, write, search, unmask and effects.
//! **Only read is built**, and [`DbPlan`] has exactly one variant to say so
//! rather than five stubs that would each fix a shape by accident. The document
//! is explicit that per-family node spelling is deliberately left to each
//! family's port, because a family that gets its own shape wrong costs that
//! family a revision - it is only the *shared* core (identifiers, expressions,
//! projection, literals) that a first port must not be allowed to fix by
//! default. That shared core is what this crate is.
//!
//! # The scope boundary, re-derived
//!
//! SC-3 splits `query.rs` into DDL/schema rendering (not `DbPlan`) and runtime
//! query builders (`DbPlan` nodes), and that distinction is the one to follow.
//! Its line ranges are not: the file is now 12,104 lines and the ranges
//! describe a file about half that size. Nothing in this crate emits DDL,
//! because the runtime executes none - no `CREATE TABLE`, no index, no
//! constraint, no `ALTER`.

use crate::ident::Ident;
use crate::path::FieldPath;
use crate::predicate::{Predicate, MAX_PREDICATE_DEPTH};
use crate::projection::{Projection, ProjectionKind, ProjectionSource};
use core::fmt;

/// The largest page a single read may return.
///
/// Mirrors `MAX_QUERY_LIMIT` (`query.rs:595`). A limit is **not optional** on
/// [`Select`]: SC-3's parent design records that an omitted limit once emitted
/// no `LIMIT` clause at all and pulled an entire collection into the worker, so
/// the default here is the cap rather than absence, and "unbounded" has no
/// representation.
pub const MAX_ROW_LIMIT: i64 = 500;

/// The furthest a single read may skip. Mirrors `MAX_QUERY_OFFSET`
/// (`query.rs:596`).
pub const MAX_ROW_OFFSET: i64 = 10_000;

/// A bounded row limit. Defaults to [`MAX_ROW_LIMIT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowLimit(i64);

impl RowLimit {
    /// # Errors
    ///
    /// [`PlanError::LimitOutOfRange`] for anything outside `1..=MAX_ROW_LIMIT`.
    /// Zero is refused rather than treated as "no rows": a caller who wants no
    /// rows has a `Predicate::never()`, and `LIMIT 0` as an accident of
    /// arithmetic should say so.
    pub fn new(rows: i64) -> Result<Self, PlanError> {
        if (1..=MAX_ROW_LIMIT).contains(&rows) {
            Ok(Self(rows))
        } else {
            Err(PlanError::LimitOutOfRange { requested: rows })
        }
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl Default for RowLimit {
    fn default() -> Self {
        Self(MAX_ROW_LIMIT)
    }
}

/// A bounded row offset. Defaults to zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowOffset(i64);

impl RowOffset {
    /// # Errors
    ///
    /// [`PlanError::OffsetOutOfRange`] outside `0..=MAX_ROW_OFFSET`.
    pub fn new(rows: i64) -> Result<Self, PlanError> {
        if (0..=MAX_ROW_OFFSET).contains(&rows) {
            Ok(Self(rows))
        } else {
            Err(PlanError::OffsetOutOfRange { requested: rows })
        }
    }

    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    Ascending,
    Descending,
}

/// Where nulls sort.
///
/// Explicit, never implied, because the two backends disagree by default:
/// `PostgreSQL` sorts nulls last on an ascending order, `SQLite` sorts them first.
/// Leaving it implicit makes the dev tier and the production tier return pages
/// in a different order for the same plan, which is the kind of divergence that
/// is only ever found in production.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NullOrder {
    First,
    Last,
}

/// One `ORDER BY` key.
///
/// The operand is a [`FieldPath`], not an [`crate::Operand`], so an aggregate
/// or a literal cannot be a sort key - the first is a position this crate does
/// not support and the second is a constant sort that silently does nothing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderKey {
    pub path: FieldPath,
    pub direction: Direction,
    pub nulls: NullOrder,
}

/// A read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    namespace: Option<Ident>,
    collection: Ident,
    projection: Projection,
    distinct: bool,
    filter: Predicate,
    group_by: Vec<FieldPath>,
    having: Predicate,
    order_by: Vec<OrderKey>,
    limit: RowLimit,
    offset: RowOffset,
}

impl Select {
    /// Start building a read.
    #[must_use]
    pub fn builder(collection: Ident, projection: Projection) -> SelectBuilder {
        SelectBuilder {
            namespace: None,
            collection,
            projection,
            distinct: false,
            filter: Predicate::always(),
            group_by: Vec::new(),
            having: Predicate::always(),
            order_by: Vec::new(),
            limit: RowLimit::default(),
            offset: RowOffset::default(),
        }
    }

    #[must_use]
    pub const fn namespace(&self) -> Option<&Ident> {
        self.namespace.as_ref()
    }

    #[must_use]
    pub const fn collection(&self) -> &Ident {
        &self.collection
    }

    #[must_use]
    pub const fn projection(&self) -> &Projection {
        &self.projection
    }

    #[must_use]
    pub const fn is_distinct(&self) -> bool {
        self.distinct
    }

    #[must_use]
    pub const fn filter(&self) -> &Predicate {
        &self.filter
    }

    #[must_use]
    pub fn group_by(&self) -> &[FieldPath] {
        &self.group_by
    }

    #[must_use]
    pub const fn having(&self) -> &Predicate {
        &self.having
    }

    #[must_use]
    pub fn order_by(&self) -> &[OrderKey] {
        &self.order_by
    }

    #[must_use]
    pub const fn limit(&self) -> RowLimit {
        self.limit
    }

    #[must_use]
    pub const fn offset(&self) -> RowOffset {
        self.offset
    }
}

/// Builder for [`Select`]. Every invariant is checked in
/// [`SelectBuilder::build`], so a `Select` that exists is a `Select` that holds
/// together.
#[derive(Debug, Clone)]
pub struct SelectBuilder {
    namespace: Option<Ident>,
    collection: Ident,
    projection: Projection,
    distinct: bool,
    filter: Predicate,
    group_by: Vec<FieldPath>,
    having: Predicate,
    order_by: Vec<OrderKey>,
    limit: RowLimit,
    offset: RowOffset,
}

impl SelectBuilder {
    #[must_use]
    pub fn namespace(mut self, namespace: Ident) -> Self {
        self.namespace = Some(namespace);
        self
    }

    #[must_use]
    pub const fn distinct(mut self, distinct: bool) -> Self {
        self.distinct = distinct;
        self
    }

    #[must_use]
    pub fn filter(mut self, filter: Predicate) -> Self {
        self.filter = filter;
        self
    }

    #[must_use]
    pub fn group_by(mut self, keys: Vec<FieldPath>) -> Self {
        self.group_by = keys;
        self
    }

    #[must_use]
    pub fn having(mut self, having: Predicate) -> Self {
        self.having = having;
        self
    }

    #[must_use]
    pub fn order_by(mut self, keys: Vec<OrderKey>) -> Self {
        self.order_by = keys;
        self
    }

    #[must_use]
    pub const fn limit(mut self, limit: RowLimit) -> Self {
        self.limit = limit;
        self
    }

    #[must_use]
    pub const fn offset(mut self, offset: RowOffset) -> Self {
        self.offset = offset;
        self
    }

    /// Check the invariants the shapes cannot carry, and canonicalise.
    ///
    /// SC-3 names three invariants that are not expressible in the node shapes
    /// and therefore need checking rather than assuming. Two of them are here:
    ///
    /// * **an aggregate operand is legal only in a `HAVING` position.** An
    ///   aggregate in `WHERE` is a `PostgreSQL` error
    ///   (`aggregate functions are not allowed in WHERE`), and it is the kind
    ///   of mistake a builder makes when `HAVING` is spelled as "another
    ///   filter";
    /// * **a backend's refusal is a typed error**, which is
    ///   [`crate::render::postgres::RenderError`] rather than an empty result.
    ///
    /// The third - that a plan's parameter count equals the placeholders its
    /// lowering emits - belongs to rendering and is asserted there.
    ///
    /// # Errors
    ///
    /// [`PlanError`], naming the invariant that failed.
    pub fn build(self) -> Result<Select, PlanError> {
        // BEFORE `canonical()`, which recurses - and before `mentions_aggregate`
        // and the renderer, which also do. `Predicate`'s smart constructors
        // already refuse an over-deep tree, but its variants are public, so a
        // caller can assemble one by hand; this is the boundary every predicate
        // must cross to become executable, and it is where the bound is closed.
        for (position, predicate) in [("filter", &self.filter), ("having", &self.having)] {
            let depth = predicate.depth();
            if depth > MAX_PREDICATE_DEPTH {
                return Err(PlanError::PredicateTooDeep { position, depth });
            }
        }

        let filter = self.filter.canonical();
        let having = self.having.canonical();

        if filter.mentions_aggregate() {
            return Err(PlanError::AggregateOutsideHaving);
        }

        let aggregating = self.projection.kind() == ProjectionKind::Aggregate;
        let mut group_by = self.group_by;
        group_by.sort();
        group_by.dedup();

        if !aggregating && !group_by.is_empty() {
            return Err(PlanError::GroupByWithRowProjection);
        }
        if !aggregating && having != Predicate::always() {
            return Err(PlanError::HavingWithoutAggregate);
        }

        if aggregating {
            for field in self.projection.fields() {
                let key = match &field.source {
                    ProjectionSource::Aggregate(_) => continue,
                    ProjectionSource::Column(column) => FieldPath::column(column.clone()),
                    ProjectionSource::Path(path) => path.clone(),
                    // Refused by `Projection::aggregate` already; kept as an
                    // explicit arm so a future variant cannot slip through the
                    // grouping check by being added to the enum alone.
                    ProjectionSource::Stored { physical } => FieldPath::column(physical.clone()),
                    // Refused by `Projection::rows` and `Projection::aggregate`
                    // both, so no `Select` can carry one. Kept as an explicit
                    // arm for the same reason as the one above: a family added
                    // later must not slip past the grouping check by adding a
                    // variant alone.
                    ProjectionSource::SearchScalar(kind) => {
                        return Err(PlanError::SearchScalarInRead {
                            alias: kind.alias_str(),
                        })
                    }
                };
                if !group_by.contains(&key) {
                    return Err(PlanError::UngroupedProjectedField {
                        alias: field.alias.as_str().to_string(),
                    });
                }
            }
            for key in &self.order_by {
                if !group_by.contains(&key.path) {
                    return Err(PlanError::UngroupedOrderKey {
                        column: key.path.root().as_str().to_string(),
                    });
                }
            }
        }

        // A repeated sort key is a no-op, so removing duplicates is
        // meaning-preserving. The order of the keys that remain is NOT touched:
        // `ORDER BY a, b` and `ORDER BY b, a` are different queries, and this
        // is the one list in a plan where sorting would be a correctness bug
        // rather than a canonicalisation.
        let mut order_by: Vec<OrderKey> = Vec::with_capacity(self.order_by.len());
        for key in self.order_by {
            if order_by.iter().any(|seen| seen.path == key.path) {
                continue;
            }
            order_by.push(key);
        }

        Ok(Select {
            namespace: self.namespace,
            collection: self.collection,
            projection: self.projection,
            distinct: self.distinct,
            filter,
            group_by,
            having,
            order_by,
            limit: self.limit,
            offset: self.offset,
        })
    }
}

/// A runtime database operation.
///
/// Three families today: read, write, and search. See the module note on why
/// the other three are absent rather than stubbed.
///
/// **`insert` and `insertMany` share one variant**, because they share one
/// statement: `INSERT INTO t (a) VALUES ($1)` is what both produce for a single
/// row, and a second variant would give one logical plan two spellings. The
/// canonical-form property that lets a prepared statement be shared rests on
/// there being exactly one - the same argument that makes
/// [`crate::Predicate::range`] a constructor rather than a node.
///
/// **`search` and `near` likewise share one variant.** They differ in what they
/// rank by, which is a [`crate::SearchCriterion`], not in the statement's
/// shape: both project a synthetic distance, filter, order by that distance
/// ascending and bound the result. Today they are two functions
/// (`crates/zeroship-data-query-builder/src/compile.rs:4951` and `:5040`) that had drifted
/// into ordering by two different things - the distance *expression* on one and
/// the output *alias* on the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbPlan {
    Select(Select),
    /// One or more rows, one statement. See [`crate::write::Insert`].
    Insert(crate::write::Insert),
    Update(crate::write::Update),
    Delete(crate::write::Delete),
    /// A ranked search. See [`crate::search::Search`].
    Search(crate::search::Search),
}

/// Why a plan was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    LimitOutOfRange {
        requested: i64,
    },
    OffsetOutOfRange {
        requested: i64,
    },
    AggregateOutsideHaving,
    GroupByWithRowProjection,
    HavingWithoutAggregate,
    UngroupedProjectedField {
        alias: String,
    },
    UngroupedOrderKey {
        column: String,
    },
    /// A predicate nested past [`MAX_PREDICATE_DEPTH`]. `position` names which
    /// of the two a read carries, because the same text is legal in one and the
    /// error would otherwise send the reader to the wrong clause.
    PredicateTooDeep {
        position: &'static str,
        depth: usize,
    },
    /// A search's ranking scalar reached a read. Unreachable through
    /// [`Projection::rows`], which refuses it; this is the second fence.
    SearchScalarInRead {
        alias: &'static str,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitOutOfRange { requested } => write!(
                f,
                "a row limit of {requested} is outside 1..={MAX_ROW_LIMIT}"
            ),
            Self::OffsetOutOfRange { requested } => write!(
                f,
                "a row offset of {requested} is outside 0..={MAX_ROW_OFFSET}"
            ),
            Self::AggregateOutsideHaving => f.write_str(
                "an aggregate operand is legal only in a HAVING position, not in a filter",
            ),
            Self::GroupByWithRowProjection => f.write_str(
                "GROUP BY needs an aggregate projection: a row projection carries the \
                 platform fields, which are not grouping keys",
            ),
            Self::HavingWithoutAggregate => {
                f.write_str("HAVING needs an aggregate projection; use a filter instead")
            }
            Self::UngroupedProjectedField { alias } => write!(
                f,
                "'{alias}' is projected but neither aggregated nor grouped"
            ),
            Self::UngroupedOrderKey { column } => write!(
                f,
                "'{column}' orders a grouped read but is not a grouping key"
            ),
            Self::PredicateTooDeep { position, depth } => write!(
                f,
                "the {position} is nested {depth} deep, over the maximum of \
                 {MAX_PREDICATE_DEPTH}"
            ),
            Self::SearchScalarInRead { alias } => write!(
                f,
                "'{alias}' is a search's ranking scalar and has no operands in a read; \
                 build a Search"
            ),
        }
    }
}

impl std::error::Error for PlanError {}
