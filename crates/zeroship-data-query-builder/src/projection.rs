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
//! # `MaskedSibling` is a node, and that is only half a guarantee
//!
//! The read path does not select a masked column alongside its sibling; it
//! *substitutes*, emitting `"ssn_masked" AS "ssn"`, and the mask pass depends on
//! exactly that aliasing. So the obvious implementation of narrowing - filter
//! the final column list down to what the caller asked for - emits a bare
//! `"ssn"` and returns **plaintext**: not fewer columns than intended but the
//! wrong value, silently, on precisely the columns marked as needing
//! protection.
//!
//! [`ProjectionSource::MaskedSibling`] makes that substitution a node rather
//! than a late string rewrite, and [`crate::render`] has no way to emit a bare
//! parent name for one. **What it cannot do is force the choice.** Deciding
//! that `ssn` is masked requires the declared schema, which lives outside this
//! leaf crate, so a caller who builds `ProjectionSource::Column(ssn)` for a
//! masked column still gets plaintext. The node makes the correct form
//! expressible and nameable; the schema-aware layer above is what must choose
//! it. That gap is real and is listed under what the types cannot enforce.

use crate::ident::{Ident, IdentRole};
use crate::path::FieldPath;
use crate::predicate::AggregateRef;
use core::fmt;

/// The seven platform-managed system fields, unioned into every row
/// projection.
///
/// Mirrors `SYSTEM_FIELD_NAMES` (`query.rs:723-731`). Re-stated rather than
/// imported for the same reason as the identifier fences: this crate takes no
/// dependency on `zeroship-schema`. When SC-3's port lands, one of the two
/// copies must be deleted rather than both maintained.
pub const PLATFORM_FIELD_NAMES: &[&str] = &[
    "id",
    "created_at",
    "updated_at",
    "created_by",
    "updated_by",
    "version",
    "deleted_at",
];

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
    /// same reason [`Ident::masked_sibling_of`] exists: a leading `_` is
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
    /// `"<sibling>" AS "<alias>"`. The parent column is **not** read, which is
    /// also what keeps key resolution off the default read path.
    MaskedSibling { parent: Ident, sibling: Ident },
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

    /// A masked column: the sibling is read and aliased back to the parent's
    /// name, so `row[col]` holds the masked string and there is no
    /// `<col>_masked` key.
    ///
    /// The sibling name is **derived**, not supplied. A caller cannot pass one,
    /// because `<col>_masked` is refused as a column identifier - which is what
    /// stops a creator declaring it - so there has to be exactly one site that
    /// forms the name, and it is [`Ident::masked_sibling_of`].
    ///
    /// # Errors
    ///
    /// [`ProjectionError::Alias`] if the parent name is not a legal alias, or
    /// [`ProjectionError::MaskedSiblingName`] if the derived sibling would
    /// overflow the identifier limit.
    pub fn masked(parent: Ident) -> Result<Self, ProjectionError> {
        let alias = Ident::parse_as(parent.as_str(), IdentRole::Alias)
            .map_err(|source| ProjectionError::Alias { source })?;
        let sibling = Ident::masked_sibling_of(&parent)
            .map_err(|source| ProjectionError::MaskedSiblingName { source })?;
        Ok(Self {
            source: ProjectionSource::MaskedSibling { parent, sibling },
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
    /// A row projection: the caller's declared fields, unioned with every
    /// platform field.
    ///
    /// A field the caller already declared **wins** over the platform entry of
    /// the same name, so a schema that names `deleted_at` gets it with
    /// [`Exposure::Declared`] and it reaches user code.
    ///
    /// An empty `declared` list is legal and yields the platform fields alone;
    /// the non-empty invariant holds after the union, which is where it
    /// matters.
    ///
    /// # Errors
    ///
    /// [`ProjectionError::DuplicateAlias`] if two declared fields share an
    /// alias - two columns arriving under one name is an ambiguous row, not a
    /// narrowing. [`ProjectionError::AggregateInRowProjection`] if a declared
    /// field aggregates.
    pub fn rows(declared: Vec<ProjectedField>) -> Result<Self, ProjectionError> {
        if let Some(field) = declared.iter().find(|f| f.is_aggregate()) {
            return Err(ProjectionError::AggregateInRowProjection {
                alias: field.alias.as_str().to_string(),
            });
        }
        Self::refuse_search_scalar(&declared)?;
        let mut fields = declared;
        Self::refuse_duplicate_aliases(&fields)?;
        for name in PLATFORM_FIELD_NAMES {
            if fields.iter().any(|f| f.alias.as_str() == *name) {
                continue;
            }
            // Infallible in practice - the seven names are ASCII, short, and
            // hit no reservation - but built through the same validating
            // constructor as everything else rather than bypassing it. If one
            // of them ever stopped being a legal identifier, that is a fact
            // worth an error rather than a silent omission.
            let column = Ident::parse_as(name, IdentRole::Column)
                .map_err(|source| ProjectionError::Alias { source })?;
            let alias = Ident::parse_as(name, IdentRole::Alias)
                .map_err(|source| ProjectionError::Alias { source })?;
            fields.push(ProjectedField {
                source: ProjectionSource::Column(column),
                alias,
                exposure: Exposure::Platform,
            });
        }
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
    /// [`ProjectionError::MaskedSiblingInAggregate`], because a masked column
    /// as a grouping key would need its mask policy resolved over a grouped
    /// result, which is SC-6's contract rather than something to guess at here.
    pub fn aggregate(fields: Vec<ProjectedField>) -> Result<Self, ProjectionError> {
        if !fields.iter().any(ProjectedField::is_aggregate) {
            return Err(ProjectionError::NoAggregate);
        }
        Self::refuse_search_scalar(&fields)?;
        if let Some(field) = fields
            .iter()
            .find(|f| matches!(f.source, ProjectionSource::MaskedSibling { .. }))
        {
            return Err(ProjectionError::MaskedSiblingInAggregate {
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

    /// Whether any field carries a masked substitution.
    #[must_use]
    pub fn has_masked_field(&self) -> bool {
        self.fields
            .iter()
            .any(|f| matches!(f.source, ProjectionSource::MaskedSibling { .. }))
    }
}

/// Why a projection was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    Alias { source: crate::ident::IdentError },
    /// The derived `<col>_masked` name would overflow the identifier limit.
    MaskedSiblingName { source: crate::ident::IdentError },
    DuplicateAlias { alias: String },
    AggregateInRowProjection { alias: String },
    MaskedSiblingInAggregate { alias: String },
    NoAggregate,
    /// A ranking scalar was offered to a projection that is not a search's.
    SearchScalarOutsideSearch { alias: String },
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Alias { source } => write!(f, "projection alias is invalid: {source}"),
            Self::MaskedSiblingName { source } => write!(
                f,
                "the masked sibling column cannot be named: {source}. This crate does \
                 not reproduce the migration side's identifier cap, because a second \
                 implementation of it would project a column that does not exist"
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
            Self::MaskedSiblingInAggregate { alias } => write!(
                f,
                "'{alias}' is a masked column and cannot be a grouping key: its mask \
                 policy over a grouped result is not decided here"
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
