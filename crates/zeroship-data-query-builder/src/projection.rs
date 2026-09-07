//! What a read returns.
//!
//! # There is no `Star`
//!
//! "Never `SELECT *`" is a property of the type here, not a rule a renderer
//! remembers. [`ProjectionSource`] has no wildcard variant, so the shortest
//! path to a working query is still an explicit column list.
//!
//! # The platform-field union is in the constructor
//!
//! SC-3 states the narrowing rule as: *a narrowed projection is always unioned
//! with the required platform fields; narrowing can request fewer declared
//! fields, it can never drop a column the system needs to function* - and is
//! explicit that this must be enforced in the constructor rather than
//! remembered. [`Projection::rows`] does the union unconditionally, so a caller
//! cannot express a row projection without them.
//!
//! The reason the rule matters is that `id` is load-bearing in four independent
//! ways, and no one of them would be found by testing the others: the relation
//! stitch matches children to parents by it; the unmask handle plucks `row_pk`
//! from it; encrypted columns bind it into the AEAD tag, so without it the
//! ciphertext is undecryptable; and change events correlate on it
//! (`emit_for_rows` reads `row["id"]`, `crates/zeroship-data-engine/src/exec.rs`).
//! A narrowing implementation could plausibly be written, reviewed and shipped
//! against any one of those with the other three never exercised.
//!
//! # `Stored` is a node, and that is only half a guarantee
//!
//! A protected column is not selected alongside the column that holds its
//! stored form; it is *substituted*, emitting `"<physical>" AS "<logical>"`, and
//! the mask pass depends on exactly that aliasing. So the obvious implementation
//! of narrowing - filter the final column list down to what the caller asked for
//! - emits a bare logical name and returns **plaintext**: not fewer columns than
//! intended but the wrong value, silently, on precisely the columns marked as
//! needing protection.
//!
//! [`ProjectionSource::Stored`] makes that substitution a node rather than a
//! late string rewrite, and [`crate::render`] has no way to emit a bare logical
//! name for one. **What it cannot do is force the choice.** Deciding that a
//! column is protected requires the declared schema, which lives outside this
//! leaf crate, so a caller who builds `ProjectionSource::Column` for a protected
//! column still gets plaintext. The node makes the correct form expressible and
//! nameable; the schema-aware layer above is what must choose it. That gap is
//! real and is listed under what the types cannot enforce.
//!
//! This variant DERIVED its physical name until 2026-09-07, as `<col>_masked`,
//! at one site. The storage flip moved the platform's stored form to a
//! `__zs_raw__` prefix and stopped creating `<col>_masked` at all, so the
//! derivation outlived the layout it described and rendered against a column no
//! migration produces. Both names are now supplied by the caller, because where
//! a value is physically stored is the storage owner's fact, not a rule a
//! grammar can hold.

use crate::ident::{Ident, IdentRole};
use crate::path::FieldPath;
use crate::predicate::AggregateRef;
use core::fmt;

/// Why a field is in the list.
///
/// This answers a question the union rule states but does not resolve: whether
/// the platform fields are visible to the creator. `Platform` fields are added
/// by the planner and stripped before the row reaches user code **unless** the
/// declared schema names them too, in which case they arrive as `Declared` and
/// stay. Without the distinction the rule is ambiguous in a way that shows up
/// as either a leaked `deleted_at` or a missing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Exposure {
    /// The creator's schema names this field; it reaches user code.
    Declared,
    /// The planner added it because the system needs it; stripped before user
    /// code.
    Platform,
    /// Machinery only - never surfaced, whatever the schema says.
    Internal,
}

/// The synthetic scalar a search ranks by.
///
/// # This is a KIND, not an expression, and the distinction is load-bearing
///
/// The variant says only *which* scalar the row carries. Everything needed to
/// compute it (the column, the query vector, the metric, the point, the radius)
/// lives on the [`crate::SearchCriterion`] the plan already holds, and the
/// lowering reads it from there.
///
/// Carrying the operands here instead would put them in the plan **twice**, and
/// two copies of one fact is two spellings of one query: a projection whose
/// scalar disagreed with the criterion would render a statement that ranks by
/// one vector and filters by another. The canonical-form property rests on
/// there being exactly one place each fact lives.
///
/// The consequence is that a scalar is only meaningful inside a search, which
/// is enforced rather than documented: [`Projection::rows`] and
/// [`Projection::aggregate`] refuse one, so the only way a
/// [`ProjectionSource::SearchScalar`] exists is
/// [`crate::SearchBuilder::build`] installing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SearchScalarKind {
    /// Distance from the row's vector to the query vector, under the
    /// criterion's metric. Smaller is nearer, for every metric.
    VectorDistance,
    /// Distance in metres from the row's point to the query point.
    GeoDistanceMetres,
}

impl SearchScalarKind {
    /// The output name this scalar is projected under.
    ///
    /// These are the names the shipped builders emit - `_distance`
    /// (`crates/zeroship-schema/src/query.rs:5007`) and `_distance_m`
    /// (`:5075`) - and they are spelled at **exactly one site**, here, for the
    /// same reason the reservation tables split by role: a leading `_` is
    /// refused for a column ([`crate::IdentRole::Column`]) and permitted for an
    /// alias ([`crate::IdentRole::Alias`]), so the platform can name these and
    /// a creator cannot shadow them. A second site that spelled the name would
    /// be a second place that fence could be argued around.
    #[must_use]
    pub const fn alias_str(self) -> &'static str {
        match self {
            Self::VectorDistance => "_distance",
            Self::GeoDistanceMetres => "_distance_m",
        }
    }

    /// The alias as a validated identifier.
    ///
    /// Fallible rather than a constant because it goes through
    /// [`Ident::parse_as`] like every other identifier in this crate. It cannot
    /// fail in practice - both names are short ASCII that clear
    /// [`crate::IdentRole::Alias`]'s fences - and
    /// `the_search_scalar_aliases_are_legal_aliases_and_illegal_columns`
    /// asserts exactly that, so the day a fence changes is the day this says so
    /// instead of projecting a name the backend will reject.
    fn alias(self) -> Result<Ident, ProjectionError> {
        Ident::parse_as(self.alias_str(), IdentRole::Alias)
            .map_err(|source| ProjectionError::Alias { source })
    }
}

/// Where a projected value comes from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProjectionSource {
    Column(Ident),
    Path(FieldPath),
    /// `"<physical>" AS "<alias>"`, where the physical column is NOT the name
    /// the row is returned under.
    ///
    /// A distinct variant rather than a `Column` with an unequal alias, because
    /// two rules depend on being able to SEE the divergence: [`crate::render`]
    /// has no way to emit a bare `alias` for one, and [`Projection::aggregate`]
    /// refuses it as a grouping key. Collapsing it into `Column` would keep the
    /// rendering correct and silently drop the refusal.
    Stored { physical: Ident },
    Aggregate(AggregateRef),
    /// The scalar a search ranks by. Only reachable inside a
    /// [`crate::Search`]; see [`SearchScalarKind`].
    SearchScalar(SearchScalarKind),
}

/// One output column.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectedField {
    pub source: ProjectionSource,
    /// A first-class slot, not a rendering trick: mandatory aliasing for
    /// cross-collection column collisions needs somewhere to live once
    /// relations exist.
    pub alias: Ident,
    pub exposure: Exposure,
}

impl ProjectedField {
    /// A declared column projected under its own name.
    ///
    /// # Errors
    ///
    /// [`ProjectionError::Alias`] if the column name is not a legal alias.
    pub fn column(name: Ident) -> Result<Self, ProjectionError> {
        let alias = Ident::parse_as(name.as_str(), IdentRole::Alias)
            .map_err(|source| ProjectionError::Alias { source })?;
        Ok(Self {
            source: ProjectionSource::Column(name),
            alias,
            exposure: Exposure::Declared,
        })
    }

    /// A column the PLATFORM manages, projected under its own name.
    ///
    /// Identical to [`Self::column`] except for the exposure, which is what
    /// [`Projection::visible_aliases`] filters on: a platform field is selected
    /// and is not part of what the caller asked to see.
    ///
    /// WHICH fields those are is deliberately not a fact this crate holds. It
    /// kept its own list until 2026-09-07 and unioned it into every row
    /// projection; see [`Projection::rows`] for what that cost. The caller
    /// names them because the caller is the one that knows.
    ///
    /// # Errors
    ///
    /// [`ProjectionError::Alias`] if the column name is not a legal alias.
    pub fn platform(name: Ident) -> Result<Self, ProjectionError> {
        let alias = Ident::parse_as(name.as_str(), IdentRole::Alias)
            .map_err(|source| ProjectionError::Alias { source })?;
        Ok(Self {
            source: ProjectionSource::Column(name),
            alias,
            exposure: Exposure::Platform,
        })
    }

    /// A column whose PHYSICAL name differs from the LOGICAL name it is
    /// returned under: `"<physical>" AS "<logical>"`.
    ///
    /// This is the only expressible form of physical/logical divergence, and it
    /// is why [`Self::column`] is not enough on its own: that constructor
    /// derives the alias from the name, so it can never express a divergence.
    ///
    /// The caller supplies both names. Nothing here derives one from the other,
    /// which is deliberate: the previous constructor derived a `<col>_masked`
    /// sibling at exactly one site, and the storage flip stopped creating that
    /// column, so the derivation outlived the shape it described. A rule about
    /// where a value is physically stored belongs to whoever owns the storage
    /// layout, not to the grammar that renders a name.
    ///
    /// `physical` must have been parsed under [`crate::IdentRole::StoredColumn`],
    /// which is the role that permits the platform's own column prefixes while
    /// still refusing the backend catalogs and the classification names.
    ///
    /// # Errors
    ///
    /// [`ProjectionError::Alias`] if `logical` is not a legal alias.
    pub fn stored(physical: Ident, logical: Ident) -> Result<Self, ProjectionError> {
        let alias = Ident::parse_as(logical.as_str(), IdentRole::Alias)
            .map_err(|source| ProjectionError::Alias { source })?;
        Ok(Self {
            source: ProjectionSource::Stored { physical },
            alias,
            exposure: Exposure::Declared,
        })
    }

    /// An aggregate under an explicit alias.
    #[must_use]
    pub const fn aggregate(aggregate: AggregateRef, alias: Ident) -> Self {
        Self {
            source: ProjectionSource::Aggregate(aggregate),
            alias,
            exposure: Exposure::Declared,
        }
    }

    /// Whether this field aggregates.
    #[must_use]
    pub const fn is_aggregate(&self) -> bool {
        matches!(self.source, ProjectionSource::Aggregate(_))
    }

    /// Whether this field is a search's ranking scalar.
    #[must_use]
    pub const fn is_search_scalar(&self) -> bool {
        matches!(self.source, ProjectionSource::SearchScalar(_))
    }
}

/// Whether a projection returns rows or an aggregation.
///
/// The distinction has to exist because the platform-field union is correct for
/// one and invalid SQL for the other: unioning `id`, `created_at`, ... into a
/// grouped query projects ungrouped columns, which `PostgreSQL` refuses outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProjectionKind {
    Rows,
    Aggregate,
}

/// A non-empty, ordered list of output columns.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Projection {
    fields: Vec<ProjectedField>,
    kind: ProjectionKind,
}

impl Projection {
    /// A row projection: exactly the fields the caller supplies.
    ///
    /// # This unioned a list of platform fields until 2026-09-07
    ///
    /// It held its own copy of the seven system-field names and appended any the
    /// caller had not already declared. Two defects came from that, and both
    /// were silent. `distinct("role")` was unrepresentable, because the union
    /// widened a one-column projection back to eight. And an explicit
    /// `select: ["email"]` returned eight columns rather than one, so a caller
    /// asking to narrow got a row shape it did not ask for.
    ///
    /// The deeper problem is that the list was platform policy living in a
    /// grammar. Which fields a platform manages is not a property of SQL, this
    /// crate had no way to be told the answer, and its own copy was a duplicate
    /// of `zeroship-schema`'s `SYSTEM_FIELD_NAMES` that nothing kept in step -
    /// the constant's doc comment said one of the two had to go.
    ///
    /// So the caller supplies the whole list now, and marks the platform's own
    /// entries with [`ProjectedField::platform`] so [`Self::visible_aliases`]
    /// can still tell them apart. The schema-aware layer above knows which
    /// fields those are; this one does not and should not.
    ///
    /// # Errors
    ///
    /// [`ProjectionError::Empty`] if the list is empty. This refusal is NEW and
    /// it is not optional: the non-empty invariant used to be maintained by the
    /// union, which could not produce an empty projection, so removing the union
    /// removed the invariant with it. Without this arm an empty list renders
    /// `SELECT  FROM`, which is a syntax error the grammar exists to prevent.
    ///
    /// [`ProjectionError::DuplicateAlias`] if two fields share an alias - two
    /// columns arriving under one name is an ambiguous row, not a narrowing.
    /// [`ProjectionError::AggregateInRowProjection`] if a field aggregates.
    pub fn rows(declared: Vec<ProjectedField>) -> Result<Self, ProjectionError> {
        if declared.is_empty() {
            return Err(ProjectionError::Empty);
        }
        if let Some(field) = declared.iter().find(|f| f.is_aggregate()) {
            return Err(ProjectionError::AggregateInRowProjection {
                alias: field.alias.as_str().to_string(),
            });
        }
        Self::refuse_search_scalar(&declared)?;
        let fields = declared;
        Self::refuse_duplicate_aliases(&fields)?;
        Ok(Self {
            fields: Self::canonicalise(fields),
            kind: ProjectionKind::Rows,
        })
    }

    /// An aggregate projection. No platform-field union: see
    /// [`ProjectionKind`].
    ///
    /// # Errors
    ///
    /// [`ProjectionError::NoAggregate`] if nothing in the list aggregates -
    /// that is a row projection and should say so.
    /// [`ProjectionError::DuplicateAlias`] as above.
    /// [`ProjectionError::StoredInAggregate`], because a column whose physical
    /// name diverges from its logical one is the shape a protected value takes,
    /// and resolving its policy over a grouped result is SC-6's contract rather
    /// than something to guess at here.
    pub fn aggregate(fields: Vec<ProjectedField>) -> Result<Self, ProjectionError> {
        if !fields.iter().any(ProjectedField::is_aggregate) {
            return Err(ProjectionError::NoAggregate);
        }
        Self::refuse_search_scalar(&fields)?;
        if let Some(field) = fields
            .iter()
            .find(|f| matches!(f.source, ProjectionSource::Stored { .. }))
        {
            return Err(ProjectionError::StoredInAggregate {
                alias: field.alias.as_str().to_string(),
            });
        }
        Self::refuse_duplicate_aliases(&fields)?;
        Ok(Self {
            fields: Self::canonicalise(fields),
            kind: ProjectionKind::Aggregate,
        })
    }

    /// Sort by alias and nothing else.
    ///
    /// Sorting is safe because a result row is addressed by name, never by
    /// position, so the select-list order is not observable to a caller - and
    /// it is necessary because two callers who asked for the same columns in
    /// different orders must produce one SQL string, or the prepared-statement
    /// cache holds two entries for one query.
    fn canonicalise(mut fields: Vec<ProjectedField>) -> Vec<ProjectedField> {
        fields.sort_by(|a, b| a.alias.cmp(&b.alias));
        fields
    }

    /// Append a search's ranking scalar to a row projection.
    ///
    /// `pub(crate)` and called from exactly one place
    /// ([`crate::SearchBuilder::build`]), which is what makes
    /// [`ProjectionSource::SearchScalar`] unreachable outside a search. A
    /// scalar in a plain `SELECT` would have no criterion to read its operands
    /// from, so the renderer would have to either invent them or refuse - and a
    /// shape whose only two outcomes are "invent" and "refuse" is a shape that
    /// should not be constructible.
    ///
    /// # Errors
    ///
    /// [`ProjectionError::DuplicateAlias`] if the caller's declared fields
    /// already claim the scalar's alias. That is reachable: a creator schema
    /// cannot declare a `_`-prefixed **column**, but an internal-exposure field
    /// may be projected under a `_`-prefixed **alias**, and this is where the
    /// two would collide.
    pub(crate) fn with_search_scalar(
        mut self,
        kind: SearchScalarKind,
    ) -> Result<Self, ProjectionError> {
        let alias = kind.alias()?;
        self.fields.push(ProjectedField {
            source: ProjectionSource::SearchScalar(kind),
            alias,
            // Declared, not Platform: the creator asked for a ranked search and
            // the rank is the answer. A `Platform` exposure would project the
            // distance and then strip it before user code, which is the one
            // column a search exists to return.
            exposure: Exposure::Declared,
        });
        Self::refuse_duplicate_aliases(&self.fields)?;
        self.fields = Self::canonicalise(self.fields);
        Ok(self)
    }

    /// A caller may not spell a ranking scalar; see [`SearchScalarKind`].
    fn refuse_search_scalar(fields: &[ProjectedField]) -> Result<(), ProjectionError> {
        if let Some(field) = fields.iter().find(|f| f.is_search_scalar()) {
            return Err(ProjectionError::SearchScalarOutsideSearch {
                alias: field.alias.as_str().to_string(),
            });
        }
        Ok(())
    }

    fn refuse_duplicate_aliases(fields: &[ProjectedField]) -> Result<(), ProjectionError> {
        let mut seen: Vec<&str> = fields.iter().map(|f| f.alias.as_str()).collect();
        seen.sort_unstable();
        for pair in seen.windows(2) {
            if pair[0] == pair[1] {
                return Err(ProjectionError::DuplicateAlias {
                    alias: pair[0].to_string(),
                });
            }
        }
        Ok(())
    }

    /// The output columns, in canonical order.
    #[must_use]
    pub fn fields(&self) -> &[ProjectedField] {
        &self.fields
    }

    #[must_use]
    pub const fn kind(&self) -> ProjectionKind {
        self.kind
    }

    /// The aliases that reach user code: [`Exposure::Declared`] only.
    ///
    /// This is the other half of the union rule. Everything the planner added
    /// is projected and then stripped, so the SQL is correct and the creator
    /// sees what they asked for.
    #[must_use]
    pub fn visible_aliases(&self) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|f| f.exposure == Exposure::Declared)
            .map(|f| f.alias.as_str())
            .collect()
    }
}

/// Why a projection was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    Alias { source: crate::ident::IdentError },
    /// A row projection was given no fields at all.
    Empty,
    DuplicateAlias { alias: String },
    AggregateInRowProjection { alias: String },
    StoredInAggregate { alias: String },
    NoAggregate,
    /// A ranking scalar was offered to a projection that is not a search's.
    SearchScalarOutsideSearch { alias: String },
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Alias { source } => write!(f, "projection alias is invalid: {source}"),
            Self::Empty => write!(
                f,
                "a row projection needs at least one field; an empty list would render \
                 a select with no columns"
            ),
            Self::DuplicateAlias { alias } => write!(
                f,
                "two projected fields share the alias '{alias}'; the row would be ambiguous"
            ),
            Self::AggregateInRowProjection { alias } => write!(
                f,
                "'{alias}' aggregates, so this is not a row projection; use \
                 Projection::aggregate"
            ),
            Self::StoredInAggregate { alias } => write!(
                f,
                "'{alias}' is stored under a different physical name and cannot be a \
                 grouping key: its policy over a grouped result is not decided here"
            ),
            Self::NoAggregate => f.write_str(
                "an aggregate projection must contain at least one aggregate; use \
                 Projection::rows",
            ),
            Self::SearchScalarOutsideSearch { alias } => write!(
                f,
                "'{alias}' is a search's ranking scalar and has no operands outside one; \
                 build a Search, whose builder installs it"
            ),
        }
    }
}

impl std::error::Error for ProjectionError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical sort must be able to change something, or the determinism
    /// arm that rests on it passes vacuously. This is the in-crate half of the
    /// mutation proof; the render-level half lives in `crate::render`.
    #[test]
    fn the_projection_sort_can_actually_reorder() {
        let unsorted = vec![
            ProjectedField::column(Ident::parse_as("zeta", IdentRole::Column).expect("ident"))
                .expect("field"),
            ProjectedField::column(Ident::parse_as("alpha", IdentRole::Column).expect("ident"))
                .expect("field"),
        ];
        let before: Vec<String> = unsorted
            .iter()
            .map(|f| f.alias.as_str().to_string())
            .collect();
        let after: Vec<String> = Projection::canonicalise(unsorted)
            .iter()
            .map(|f| f.alias.as_str().to_string())
            .collect();
        assert_ne!(before, after, "the sort left an unsorted input untouched");
        assert_eq!(after[0], "alpha");
    }
}
