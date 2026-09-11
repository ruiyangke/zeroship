//! The search family: nearest-neighbour over a vector column, and
//! within-radius over a geographic point.
//!
//! # Two kinds, and only two
//!
//! SC-3 names the family as "vector and spatial - **and only those two**", and
//! records why the third is absent: full-text search was deleted, and "the IR
//! must not carry a family for a feature that no longer exists". That is
//! checked rather than trusted - `to_tsquery`, `tsvector`, `ts_rank`,
//! `plainto_tsquery`, `websearch_to_tsquery`, `fts5` and `bm25` appear nowhere
//! in `crates/zeroship-data-sql/src/compile.rs`,
//! `crates/zeroship-data-v8/src/` or `sdks/db/src/` (swept 2026-08-28); the
//! only occurrences in the tree are in `docs/archive/` and in the removal notes
//! at `crates/zeroship-migrate-core/src/render/declarative.rs:1824-1831`.
//!
//! The one surviving trace is a reservation: `_score` is fenced as a synthetic
//! result column and **nothing emits it**. This module does not resurrect it.
//!
//! # What the shared grammar must NOT learn
//!
//! The two backends do not merely spell this family differently - they compute
//! it differently, and the difference is not a dialect's punctuation:
//!
//! | | `PostgreSQL` | `SQLite` |
//! | --- | --- | --- |
//! | vector | `"col" <=> $1::vector`, an operator on the base table | a scalar distance function on the base BLOB column |
//! | geo | `ST_Distance(col, ST_MakePoint($1,$2)::geography)` | a haversine computed in Rust over a BLOB, sorted in the worker |
//!
//! Neither shape is expressible as the other with a different operator string,
//! so **no operator, function name or join shape appears in this module.** A
//! [`SearchCriterion`] says *what is being ranked and by what measure*; how a
//! backend computes that is entirely inside that backend's lowering
//! ([`crate::render::postgres::render_search`] is the only one written).
//!
//! This is the answer to the question SC-3's decision 2 poses for this family.
//! A shared node carrying `<=>` would make the `SQLite` arm either a lie or a
//! refusal of a query it can serve; a shared node carrying "cosine distance"
//! leaves both backends free and neither privileged.
//!
//! # How a one-backend capability is represented
//!
//! [`VectorMetric::InnerProduct`] exists on `pgvector` (`vector_ip_ops`) and
//! does not exist in `vec0`, which offers cosine and L2 only.
//!
//! The IR represents that absence by **representing the metric and letting the
//! backend refuse it**, not by omitting the variant and not by rendering it to
//! nothing. The distinction is the whole point:
//!
//! * a variant `SQLite` **refuses** is a typed error naming the metric, from
//!   `SQLite`'s own module - which is what
//!   `crates/zeroship-data-orm/src/backend/sqlite/vector.rs:61`
//!   (`reject_inner_product`) already does, returning
//!   `vector_unsupported_metric`;
//! * a variant that **renders to nothing** would return the rows a cosine
//!   search would have returned, ranked by the wrong measure, with no error -
//!   the silent-emulation failure decision 2 rejects by name.
//!
//! So the metric is not narrowed to the intersection of the two backends. A
//! plan that `PostgreSQL` can serve stays expressible, and the dev tier says so
//! instead of guessing.
//!
//! # The bound that three layers currently disagree about
//!
//! `k` is capped in three places today and the three numbers differ:
//!
//! * the SDK validates `1..=1000` (`sdks/db/src/collection/vector-geo.ts:22-35`,
//!   error `INVALID_K`);
//! * the `PostgreSQL` builder refuses over `MAX_SEARCH_LIMIT = 500`
//!   (`crates/zeroship-data-sql/src/compile.rs:597`, enforced at `:4964`);
//! * the `SQLite` arm has **no cap at all** - `build_vector_search_sql` never
//!   calls the validator and formats `k` straight into the statement
//!   (`crates/zeroship-data-orm/src/backend/sqlite/vector.rs:117`).
//!
//! A `k` of 750 is therefore refused in production and served in dev. Here
//! there is one bound, it is [`crate::RowLimit`] - the same type and the same
//! ceiling of 500 the read family already carries, because `MAX_SEARCH_LIMIT`
//! and `MAX_QUERY_LIMIT` are the same number - and it has **no absent value**,
//! so the two different caller-side defaults (`k` defaults to 10 at
//! `crates/zeroship-data-orm/src/crud/mod.rs:2138`, `near.limit` defaults to
//! 100 at `crates/zeroship-data-sql/src/compile.rs:5058`) have nowhere to live.

use crate::ident::Ident;
use crate::literal::{Finite, LiteralError, QueryVector};
use crate::plan::{OrderKey, PlanError, RowLimit};
use crate::predicate::{MAX_PREDICATE_DEPTH, Predicate};
use crate::projection::{Projection, ProjectionError, ProjectionKind, SearchScalarKind};
use core::fmt;

/// The furthest a within-radius search may reach, in metres.
///
/// The antipodal distance on the WGS-84 sphere: `pi * 6_378_137` m, the
/// greatest geodesic separation two points on Earth can have. A larger radius
/// is not a wider search - every row already qualifies - so it is a caller
/// error rather than a bound worth honouring.
pub const MAX_RADIUS_METRES: f64 = 20_037_508.342_789_244;

/// How the distance between two vectors is measured.
///
/// A closed set of three, matching `pgvector`'s three opclasses. **No operator
/// string appears here**; see the module note.
///
/// All three are ordered *smaller is better*, which is why
/// [`Self::InnerProduct`] is the **negative** inner product rather than the
/// inner product: it is what lets one ascending sort rank every metric
/// (`crates/zeroship-data-sql/src/descriptors.rs:31-35`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VectorMetric {
    Cosine,
    L2,
    /// Negative inner product. `PostgreSQL` only; `SQLite` refuses it - see the
    /// module note on why that is a refusal and not an omission.
    InnerProduct,
}

impl VectorMetric {
    /// The metric's name, for a backend's refusal message.
    ///
    /// Deliberately a *name*, not an operator: a backend that wants to refuse
    /// this metric needs to say which one it refused, and a backend that serves
    /// it spells the operator in its own lowering.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::L2 => "l2",
            Self::InnerProduct => "innerProduct",
        }
    }
}

/// A point on the Earth, in degrees.
///
/// The fields are private and the constructor validates, because the two
/// numbers are trivially transposable and neither the type system nor
/// `PostGIS` will catch it: `ST_MakePoint` takes `(x, y)` = `(lng, lat)`, the
/// inverse of the `{lat, lng}` order the SDK and the Rust trait use
/// (`crates/zeroship-data-sql/src/compile.rs:5030-5037`). A transposed pair inside
/// both valid ranges - `(45, 45)` - is a different place on Earth and no error
/// anywhere.
///
/// Naming the accessors [`Self::latitude`] and [`Self::longitude`] in full is
/// the other half: a lowering that writes them in the wrong order has to write
/// two words that say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GeoPoint {
    latitude: Finite,
    longitude: Finite,
}

impl GeoPoint {
    /// Build a point, refusing anything off the globe.
    ///
    /// # Errors
    ///
    /// [`SearchError::LatitudeOutOfRange`] outside `-90..=90`,
    /// [`SearchError::LongitudeOutOfRange`] outside `-180..=180`, and
    /// [`SearchError::Literal`] wrapping [`LiteralError::NonFiniteFloat`] for
    /// NaN or an infinity - which the range checks would otherwise pass, since
    /// every comparison against NaN is false.
    pub fn new(latitude: f64, longitude: f64) -> Result<Self, SearchError> {
        let latitude = Finite::new(latitude).map_err(SearchError::Literal)?;
        let longitude = Finite::new(longitude).map_err(SearchError::Literal)?;
        if !(-90.0..=90.0).contains(&latitude.get()) {
            return Err(SearchError::LatitudeOutOfRange { degrees: latitude });
        }
        if !(-180.0..=180.0).contains(&longitude.get()) {
            return Err(SearchError::LongitudeOutOfRange { degrees: longitude });
        }
        Ok(Self {
            latitude,
            longitude,
        })
    }

    #[must_use]
    pub const fn latitude(self) -> Finite {
        self.latitude
    }

    #[must_use]
    pub const fn longitude(self) -> Finite {
        self.longitude
    }
}

/// A search radius, in metres.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RadiusMetres(Finite);

impl RadiusMetres {
    /// # Errors
    ///
    /// [`SearchError::RadiusOutOfRange`] for a radius that is not positive or
    /// exceeds [`MAX_RADIUS_METRES`]. Zero is refused rather than treated as
    /// "exactly here": floating-point coordinates make an exact-equality
    /// spatial match a query that returns nothing for a reason no caller can
    /// see, and a caller who wants one row has a filter on the primary key.
    pub fn new(metres: f64) -> Result<Self, SearchError> {
        let metres = Finite::new(metres).map_err(SearchError::Literal)?;
        if metres.get() <= 0.0 || metres.get() > MAX_RADIUS_METRES {
            return Err(SearchError::RadiusOutOfRange { metres });
        }
        Ok(Self(metres))
    }

    #[must_use]
    pub const fn get(self) -> Finite {
        self.0
    }
}

/// What a search ranks by, and over which column.
///
/// The two variants are the two kinds SC-3 names. Each carries the column being
/// searched and the thing being searched *for*, and nothing about how a backend
/// computes the answer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SearchCriterion {
    /// Rank rows by the distance from `column`'s vector to `query`, under
    /// `metric`, nearest first.
    Vector {
        column: Ident,
        query: QueryVector,
        metric: VectorMetric,
    },
    /// Keep the rows whose `column` lies within `radius` of `point`, nearest
    /// first.
    ///
    /// The radius is **part of the criterion, not a filter**. It has to be:
    /// a backend serves it from the same index that produces the ordering
    /// (`ST_DWithin` and `ST_Distance` over one `GiST` index), and expressing it
    /// as a [`Predicate`] would need a distance operand the shared grammar
    /// deliberately does not have.
    Geo {
        column: Ident,
        point: GeoPoint,
        radius: RadiusMetres,
    },
}

impl SearchCriterion {
    /// The column being searched.
    #[must_use]
    pub const fn column(&self) -> &Ident {
        match self {
            Self::Vector { column, .. } | Self::Geo { column, .. } => column,
        }
    }

    /// The scalar this criterion produces, which is also what it orders by.
    #[must_use]
    pub const fn scalar(&self) -> SearchScalarKind {
        match self {
            Self::Vector { .. } => SearchScalarKind::VectorDistance,
            Self::Geo { .. } => SearchScalarKind::GeoDistanceMetres,
        }
    }
}

/// A ranked search over one collection.
///
/// # The ranking key is structural, and that is what makes the plan canonical
///
/// SC-3 lists an expression-free `ORDER BY` as cost 1 and says this family's
/// port must "give that scalar an ordering-visible slot ... or the restriction
/// has to be revisited". The slot is
/// [`crate::ProjectionSource::SearchScalar`], and the ordering is **not a
/// caller-supplied key**: the lowering always ranks by the criterion's scalar,
/// ascending, first.
///
/// That is a canonicalisation decision, not a convenience. If the distance key
/// were spellable, `search(v, k)` and `search(v, k).sortBy(_distance)` would be
/// two plans for one query, and the prepared-statement cache would hold two
/// entries. Here there is exactly one spelling because there is no way to write
/// the other.
///
/// The shipped builders already carry the seam this closes: the vector arm
/// orders by re-emitting the distance expression
/// (`crates/zeroship-data-sql/src/compile.rs:5013`) while the geo arm orders by the
/// output alias (`:5082`). Two spellings of one concept, in two functions
/// forty lines apart.
///
/// # Tiebreaks are caller-supplied, and they are not decoration
///
/// [`SearchBuilder::tiebreak`] appends keys **after** the distance key. Without
/// them, rows at equal distance come back in whatever order the engine
/// produced, and the two backends do not agree: `PostgreSQL` returns the
/// index's order and `SQLite` returns the order the worker's sort left them in
/// (`crates/zeroship-data-orm/src/backend/sqlite/mod.rs:1524-1532` sorts in
/// Rust). Equal distances are not exotic - two rows holding the same embedding
/// tie exactly - and the existing `PostgreSQL` test says so in its own comment,
/// asserting set membership rather than order because "pgvector distance ties
/// between FP-close vectors can re-order across builds"
/// (`crates/zeroship-data-orm/src/tests/postgres/search.rs`).
///
/// A tiebreak carries its own [`crate::NullOrder`], for the reason
/// [`crate::OrderKey`] does: the two backends' defaults differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    namespace: Option<Ident>,
    collection: Ident,
    criterion: SearchCriterion,
    /// Carries the scalar field; installed by the builder, never by a caller.
    projection: Projection,
    filter: Predicate,
    tiebreak: Vec<OrderKey>,
    limit: RowLimit,
}

impl Search {
    /// Start building a search.
    ///
    /// `projection` is a **row** projection built the ordinary way; the builder
    /// appends the criterion's scalar itself, so a caller neither spells the
    /// synthetic alias nor can omit it.
    #[must_use]
    pub fn builder(
        collection: Ident,
        criterion: SearchCriterion,
        projection: Projection,
    ) -> SearchBuilder {
        SearchBuilder {
            namespace: None,
            collection,
            criterion,
            projection,
            filter: Predicate::always(),
            tiebreak: Vec::new(),
            limit: RowLimit::default(),
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
    pub const fn criterion(&self) -> &SearchCriterion {
        &self.criterion
    }

    /// The output columns, including the ranking scalar.
    #[must_use]
    pub const fn projection(&self) -> &Projection {
        &self.projection
    }

    #[must_use]
    pub const fn filter(&self) -> &Predicate {
        &self.filter
    }

    /// The sort keys that follow the distance key. Possibly empty.
    #[must_use]
    pub fn tiebreak(&self) -> &[OrderKey] {
        &self.tiebreak
    }

    #[must_use]
    pub const fn limit(&self) -> RowLimit {
        self.limit
    }
}

/// Builder for [`Search`]. Every invariant is checked in
/// [`SearchBuilder::build`].
#[derive(Debug, Clone)]
pub struct SearchBuilder {
    namespace: Option<Ident>,
    collection: Ident,
    criterion: SearchCriterion,
    projection: Projection,
    filter: Predicate,
    tiebreak: Vec<OrderKey>,
    limit: RowLimit,
}

impl SearchBuilder {
    #[must_use]
    pub fn namespace(mut self, namespace: Ident) -> Self {
        self.namespace = Some(namespace);
        self
    }

    #[must_use]
    pub fn filter(mut self, filter: Predicate) -> Self {
        self.filter = filter;
        self
    }

    /// Sort keys applied **after** the distance key.
    #[must_use]
    pub fn tiebreak(mut self, keys: Vec<OrderKey>) -> Self {
        self.tiebreak = keys;
        self
    }

    /// How many rows to return. This is `k`; see the module note on the three
    /// ceilings it replaces.
    #[must_use]
    pub const fn limit(mut self, limit: RowLimit) -> Self {
        self.limit = limit;
        self
    }

    /// Check the invariants and canonicalise.
    ///
    /// # Errors
    ///
    /// [`SearchError`], naming the invariant that failed.
    pub fn build(self) -> Result<Search, SearchError> {
        // BEFORE `canonical()`, `mentions_aggregate()` and the renderer, all of
        // which recurse. The search family adds no recursive shape of its own -
        // a `SearchCriterion` cannot contain another one, a `QueryVector` is a
        // flat list, and the tiebreak keys are `FieldPath`s already bounded by
        // `MAX_PATH_SEGMENTS` - so the filter is the whole exposure, exactly as
        // it is for a write.
        let depth = self.filter.depth();
        if depth > MAX_PREDICATE_DEPTH {
            return Err(SearchError::FilterTooDeep { depth });
        }
        let filter = self.filter.canonical();
        if filter.mentions_aggregate() {
            return Err(SearchError::AggregateInFilter);
        }

        // A search returns rows. An aggregate projection would need a GROUP BY
        // over a per-row distance, which is not a grouping key and not a query
        // this family serves.
        if self.projection.kind() != ProjectionKind::Rows {
            return Err(SearchError::AggregateProjection);
        }

        let scalar = self.criterion.scalar();

        // The one site that installs the scalar. A caller cannot reach
        // `ProjectionSource::SearchScalar` through `Projection::rows`, which
        // refuses it, so this is the only way one exists - and it is therefore
        // the only place the synthetic alias is spelled.
        let projection = self
            .projection
            .with_search_scalar(scalar)
            .map_err(SearchError::Projection)?;

        // A tiebreak may not name the scalar. It is already the primary key of
        // the sort, so a second mention is either redundant or a contradiction,
        // and permitting it would give one ordering two spellings - the exact
        // property the distance key is unspellable to protect.
        //
        // The comparison is against the alias this crate DERIVED, not against a
        // string literal, so renaming the scalar moves both sides together.
        for key in &self.tiebreak {
            if key.path.root().as_str() == scalar.alias_str() {
                return Err(SearchError::TiebreakNamesTheScalar {
                    alias: scalar.alias_str(),
                });
            }
        }

        // Same rule as `SelectBuilder::build`: a repeated key is a no-op, and
        // the order of the keys that survive is NOT sorted, because
        // `ORDER BY a, b` and `ORDER BY b, a` are different queries.
        let mut tiebreak: Vec<OrderKey> = Vec::with_capacity(self.tiebreak.len());
        for key in self.tiebreak {
            if tiebreak.iter().any(|seen| seen.path == key.path) {
                continue;
            }
            tiebreak.push(key);
        }

        Ok(Search {
            namespace: self.namespace,
            collection: self.collection,
            criterion: self.criterion,
            projection,
            filter,
            tiebreak,
            limit: self.limit,
        })
    }
}

/// Why a search was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// A coordinate or a radius was NaN, infinite, or otherwise not a value.
    Literal(LiteralError),
    /// The offending numbers are [`Finite`], not `f64`, so this error can carry
    /// the total order the rest of the crate relies on. A NaN latitude is not a
    /// range failure and never reaches these variants - it is refused one step
    /// earlier as [`SearchError::Literal`], because every comparison against
    /// NaN is false and a range check alone would pass it.
    LatitudeOutOfRange {
        degrees: Finite,
    },
    LongitudeOutOfRange {
        degrees: Finite,
    },
    RadiusOutOfRange {
        metres: Finite,
    },
    /// The filter was nested past [`MAX_PREDICATE_DEPTH`].
    FilterTooDeep {
        depth: usize,
    },
    /// An aggregate operand appeared in a search's filter.
    AggregateInFilter,
    /// An aggregate projection was offered to a search.
    AggregateProjection,
    /// A tiebreak key named the ranking scalar.
    TiebreakNamesTheScalar {
        alias: &'static str,
    },
    /// The scalar could not be added to the projection.
    Projection(ProjectionError),
    /// A limit outside the shared row bound. Re-exported from [`PlanError`] so
    /// a caller building a `RowLimit` for a search sees the same refusal a read
    /// gets.
    Limit(PlanError),
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(source) => write!(f, "a search value is invalid: {source}"),
            Self::LatitudeOutOfRange { degrees } => write!(
                f,
                "a latitude of {} is outside -90..=90; note that ST_MakePoint takes \
                 (longitude, latitude), so a transposed pair is a common cause",
                degrees.get()
            ),
            Self::LongitudeOutOfRange { degrees } => {
                write!(f, "a longitude of {} is outside -180..=180", degrees.get())
            }
            Self::RadiusOutOfRange { metres } => write!(
                f,
                "a radius of {} m is not in 0 < r <= {MAX_RADIUS_METRES}, the antipodal \
                 distance; a larger radius selects every row rather than searching a \
                 wider one",
                metres.get()
            ),
            Self::FilterTooDeep { depth } => write!(
                f,
                "the filter is nested {depth} deep, over the maximum of \
                 {MAX_PREDICATE_DEPTH}"
            ),
            Self::AggregateInFilter => f.write_str(
                "an aggregate operand is legal only in a HAVING position, not in a \
                 search's filter",
            ),
            Self::AggregateProjection => f.write_str(
                "a search returns ranked rows, so its projection cannot aggregate: the \
                 ranking scalar is per-row and is not a grouping key",
            ),
            Self::TiebreakNamesTheScalar { alias } => write!(
                f,
                "'{alias}' is the ranking key a search already orders by; a tiebreak \
                 naming it would give one ordering two spellings"
            ),
            Self::Projection(source) => {
                write!(f, "a search projection is invalid: {source}")
            }
            Self::Limit(source) => write!(f, "a search bound is invalid: {source}"),
        }
    }
}

impl std::error::Error for SearchError {}
