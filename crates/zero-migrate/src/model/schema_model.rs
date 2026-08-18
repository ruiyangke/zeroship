//! The NEUTRAL schema model, and the vendor side table that keeps it neutral.
//!
//! This is step 2 of `docs/proposals/single-fold-and-effects.md` section G, shared with
//! `docs/proposals/pluggable-backends.md` step 2. **No consumer moves here and no fold
//! is written here.** What lands is the TYPE and the COMPARATORS, plus the lossless
//! bridge to today's [`crate::model::snapshot`] types that makes both falsifiable
//! against a live server.
//!
//! ## The one rule this module exists to enforce
//!
//! Every type below DERIVES [`PartialEq`]. Structural equality means all fields equal,
//! with no exceptions and nothing to maintain. That is the whole point, and it is a
//! reversal of the failure direction rather than a refactor:
//!
//! * **Today** a field added to [`crate::model::snapshot::ColumnSnapshot`] is SILENTLY
//!   IGNORED by comparison, because `ColumnSnapshot::eq` is a hand-written inclusion
//!   list of ten fields out of twenty-one. Nobody is told. The measurement is in
//!   `tests/structural_equality_field_sensitivity.rs`, which points one property at both
//!   types: ELEVEN of `ColumnSnapshot`'s fields can differ while `==` reports equal.
//! * **After** a field added to [`Column`] is COMPARED by default, so the same mistake
//!   is noisy instead of invisible, and a consumer that wants it ignored has to say so
//!   in a NAMED comparator with a reason next to the field.
//!
//! ## Why "equal" is not one question
//!
//! `ColumnSnapshot::eq`'s exclusion list is consulted implicitly by every consumer, and
//! the consumers are not asking the same thing. Measured across `crates/*/src` with
//! `cfg(test)` excluded, four distinct questions ride these impls today:
//!
//! 1. **table shape** - "are these the same table?" ([`table_shape_identity`], built on
//!    [`column_shape_identity`], [`index_shape_identity`] and
//!    [`constraint_shape_identity`]). This is what `TableSnapshot::eq` answers, and its
//!    four production consumers are all in `render/declarative.rs`.
//! 2. **index pairing** - "is this live index the same index as this declared one, under
//!    a possibly DIFFERENT NAME?" ([`index_pairing_identity`]). Shared by the migration
//!    differ and the drift pass through `pair_indexes`, which is the one place two
//!    consumers already agree by construction rather than by convention.
//! 3. **pure-rename equivalence** - "is the ONLY difference between live and desired
//!    this one column rename?", which `render::declarative::pure_sqlite_column_rename`
//!    asks through `TableSnapshot::eq` ([`rename_equivalence_identity`]).
//! 4. **structural drift** - "would a user call this a change to their schema?"
//!    ([`drift_identity`]). This one does NOT go through `PartialEq` at all;
//!    `apply/drift.rs` rolls its own field-by-field pass, and it compares a STRICTLY
//!    LARGER set than `ColumnSnapshot::eq` does.
//!
//! Question 4 refutes the premise `docs/proposals/single-fold-and-effects.md` section D
//! works from, which is that `drift_identity` is the comparator `ColumnSnapshot::eq`
//! should be extracted into. It is not: `apply/drift.rs` contains no use of `PartialEq`
//! on `ColumnSnapshot`, `IndexSnapshot`, `ConstraintSnapshot`, `TableSnapshot` or
//! `ViewSnapshot`, and `ColumnSnapshot::eq` has exactly ONE production consumer -
//! `TableSnapshot::eq` at `model/snapshot.rs:1279`. The two definitions of "the same
//! column" already differ by a field, and naming them apart is what stops the next
//! reader assuming they are one.
//!
//! Question 3 is the one that has already BLOCKED A FIX. `docs/review-log.md:28291-28297`
//! records that `ConstraintSnapshot`'s `PartialEq` compares `definition`, so following a
//! column rename into a constraint definition inside `sqlite_rename_rebuild` would have
//! flipped `preserve_stored_shape` off and stopped the catalog path replaying SQLite's
//! own stored body. Whether you may fix a bug currently depends on an exclusion list
//! written for an unrelated reason. Naming the comparators separately is what makes that
//! dependency a declared input instead of an accident.
//!
//! ## Why the vendor fields LEAVE the model
//!
//! [`VendorFacts`] is a side table, not a field on [`Column`]. The distinction is
//! load-bearing: [`drift_identity`] cannot compare
//! `mysql_physical_type` because [`Column`] does not HAVE a `mysql_physical_type`. The
//! comparison is not excluded by discipline, it is unreachable by construction, which is
//! the only form of "vendor facts are compared only by code that knows the vendor" that
//! survives the next person in a hurry.
//!
//! The shape - `BTreeMap` keyed by object, absent meaning "this producer did not look" -
//! is not new here. [`crate::model::snapshot::SchemaSnapshot::table_rls`] already argues
//! for exactly it: "It also makes the dialect question vanish - engines with no
//! row-level security leave BOTH sides empty, so they cannot drift."
//!
//! ## What this does NOT claim
//!
//! The model below is bounded to TABLES, and to the catalog half of the proposal's
//! section C. It is not yet richer than [`crate::model::snapshot::TableSnapshot`], and
//! `tests/schema_model_god_object_bound.rs` measures precisely how much it would have to
//! grow to also carry `render::declarative::FieldDescriptor`, rather than asserting that
//! it could.

use std::collections::BTreeMap;

use crate::model::ir::{
    IdentityCol, IndexSortOrder, IndexStorageParams, PartitionSpec, TableRuntimeOptions,
    ValueFormat,
};
use crate::model::snapshot::{
    canonical_index_sort_order, index_predicates_canonically_eq, ColumnCollationSnapshot,
    ColumnSnapshot, ConstraintSnapshot, GeneratedColumnSnapshot, GeneratedKindSnapshot,
    IdDefaultSnapshot, IndexElementSnapshot, IndexSnapshot, MysqlPhysicalType,
    MysqlTextStorageSnapshot, TableSnapshot,
};

// ---------------------------------------------------------------------------
// Keys into the vendor side table
// ---------------------------------------------------------------------------

/// Identifies one table in the model.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TableKey {
    /// Table name, as the model keys it.
    pub table: String,
}

impl TableKey {
    /// Key for `table`.
    #[must_use]
    pub fn new(table: impl Into<String>) -> Self {
        Self {
            table: table.into(),
        }
    }
}

/// Identifies one column of one table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ColumnKey {
    /// Table name.
    pub table: String,
    /// Column name.
    pub column: String,
}

impl ColumnKey {
    /// Key for `table`.`column`.
    #[must_use]
    pub fn new(table: impl Into<String>, column: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            column: column.into(),
        }
    }
}

/// Identifies one index of one table.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexKey {
    /// Table name.
    pub table: String,
    /// Index name.
    pub index: String,
}

impl IndexKey {
    /// Key for the index named `index` on `table`.
    #[must_use]
    pub fn new(table: impl Into<String>, index: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            index: index.into(),
        }
    }
}

/// Identifies one ordered key element of one index.
///
/// Positional rather than by name because an element may be an expression, which has no
/// name, and because element ORDER is part of an index's identity anyway.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexElementKey {
    /// Table name.
    pub table: String,
    /// Index name.
    pub index: String,
    /// Zero-based position in the index's ordered element list.
    pub position: usize,
}

impl IndexElementKey {
    /// Key for element `position` of the index named `index` on `table`.
    #[must_use]
    pub fn new(table: impl Into<String>, index: impl Into<String>, position: usize) -> Self {
        Self {
            table: table.into(),
            index: index.into(),
            position,
        }
    }
}

// ---------------------------------------------------------------------------
// VendorFacts
// ---------------------------------------------------------------------------

/// Every fact ONE vendor's catalog states and the neutral model therefore may not carry.
///
/// One flat map per fact family rather than one opaque blob per object. The blob is what
/// `docs/proposals/pluggable-backends.md:376-379` sketches, and it is the right shape
/// ONCE a backend registry exists to own the blob's type; until then a typed map keeps
/// the round-trip in `SchemaModel::from_tables` / [`SchemaModel::to_tables`] provable and
/// keeps this struct exhaustively destructurable, which is what
/// `tests/schema_model_field_routing.rs` needs to prove nothing was dropped.
///
/// ABSENT means the producer did not look, and absent-on-both-sides is the state every
/// engine that does not have the concept is in - so two backends that never populate a
/// family cannot drift on it. That reasoning is copied deliberately from
/// [`crate::model::snapshot::SchemaSnapshot::table_rls`], which already chose a
/// side map over a field for the same reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VendorFacts {
    /// SQLite: whether this column is the exact rowid-alias shape
    /// (`INTEGER PRIMARY KEY`).
    ///
    /// This one is why [`drift_identity`] alone cannot reproduce today's verdict.
    /// `ColumnSnapshot::eq` COMPARES `sqlite_rowid`, so a comparator that only sees the
    /// neutral column would stop reporting an out-of-band rowid flip. The neutral and
    /// vendor halves are therefore combined explicitly by
    /// [`SchemaModel::column_drift_identity`], which is the dependency the old exclusion
    /// list kept implicit.
    pub sqlite_rowid: BTreeMap<ColumnKey, bool>,
    /// SQLite: the verbatim `sqlite_master.sql` body for this table.
    pub sqlite_stored_create_sql: BTreeMap<TableKey, String>,
    /// MySQL and SQLite: whether the live catalog carries the engine's own UUID
    /// spelling CHECK for this column. PostgreSQL never sets it.
    pub catalog_uuid_format_check: BTreeMap<ColumnKey, bool>,
    /// MySQL: `EXTRA` contains `DEFAULT_GENERATED`, distinguishing an expression
    /// default from a scalar literal MySQL has stripped the quotes from.
    pub mysql_default_generated: BTreeMap<ColumnKey, bool>,
    /// MySQL: exact character-set and collation identity.
    pub mysql_text_storage: BTreeMap<ColumnKey, MysqlTextStorageSnapshot>,
    /// MySQL: parsed physical type identity, which the portable `data_type` cannot
    /// carry because `mysql_canonical_type` folds every `varchar(n)` to `text`.
    pub mysql_physical_type: BTreeMap<ColumnKey, MysqlPhysicalType>,
    /// PostgreSQL: `ON ONLY` on a partitioned parent's index.
    pub pg_index_only: BTreeMap<IndexKey, bool>,
    /// PostgreSQL: the ANN operator class for an `ivfflat`/`hnsw` index.
    pub pg_index_opclass: BTreeMap<IndexKey, String>,
    /// PostgreSQL 15+: `NULLS NOT DISTINCT` on a UNIQUE index.
    pub pg_index_nulls_not_distinct: BTreeMap<IndexKey, bool>,
    /// PostgreSQL: per-element operator class (`text_pattern_ops`).
    pub pg_index_element_opclass: BTreeMap<IndexElementKey, String>,
    /// PostgreSQL: per-element collation (`"C"`).
    pub pg_index_element_collation: BTreeMap<IndexElementKey, String>,
}

impl VendorFacts {
    /// Merge `other` into `self`, later entries winning. Used when folding several
    /// tables into one model.
    pub fn absorb(&mut self, other: Self) {
        // EXHAUSTIVE, no `..`: a new fact family breaks this line until it is merged.
        let Self {
            sqlite_rowid,
            sqlite_stored_create_sql,
            catalog_uuid_format_check,
            mysql_default_generated,
            mysql_text_storage,
            mysql_physical_type,
            pg_index_only,
            pg_index_opclass,
            pg_index_nulls_not_distinct,
            pg_index_element_opclass,
            pg_index_element_collation,
        } = other;
        self.sqlite_rowid.extend(sqlite_rowid);
        self.sqlite_stored_create_sql
            .extend(sqlite_stored_create_sql);
        self.catalog_uuid_format_check
            .extend(catalog_uuid_format_check);
        self.mysql_default_generated.extend(mysql_default_generated);
        self.mysql_text_storage.extend(mysql_text_storage);
        self.mysql_physical_type.extend(mysql_physical_type);
        self.pg_index_only.extend(pg_index_only);
        self.pg_index_opclass.extend(pg_index_opclass);
        self.pg_index_nulls_not_distinct
            .extend(pg_index_nulls_not_distinct);
        self.pg_index_element_opclass
            .extend(pg_index_element_opclass);
        self.pg_index_element_collation
            .extend(pg_index_element_collation);
    }

    /// The VENDOR half of TABLE-SHAPE identity for one column: the vendor term
    /// `ColumnSnapshot::eq` compares today.
    ///
    /// Exactly one family participates, `sqlite_rowid`, and it participates because
    /// `ColumnSnapshot::eq` compares it - not because a vendor-neutral argument says it
    /// should. **This is the term that proves the side table cannot be a pure
    /// subtraction.** Move `sqlite_rowid` out of the neutral column and stop there, and
    /// an out-of-band rowid/AUTOINCREMENT flip silently stops being a difference. The
    /// neutral and vendor halves are therefore recombined explicitly, by
    /// [`SchemaModel::column_shape_identity`], which is the dependency the old exclusion
    /// list kept implicit.
    ///
    /// The other three column families are excluded, with the reasons their own field
    /// docs give in [`crate::model::snapshot`]:
    ///
    /// * `catalog_uuid_format_check` - introspection-only; author-built desired
    ///   snapshots always leave it `false`, so comparing it reports permanent phantom
    ///   drift on every UUID column.
    /// * `mysql_default_generated` - introspection metadata retained only for
    ///   expected-driven ID-default classification, not an independently comparable
    ///   portable facet.
    /// * `mysql_text_storage` - the portable schema surface records collation INTENT,
    ///   not a server-default MySQL collation name.
    /// * `mysql_physical_type` - NOT part of table shape. It IS part of drift, and it
    ///   is compared by [`Self::column_drift_identity`] below. Its field doc already
    ///   asks for exactly this split: "folding it into the general equality would
    ///   change what every consumer of `ColumnSnapshot` equality means by 'the same
    ///   column' [...] when only a MySQL-aware comparator should be asking."
    #[must_use]
    pub fn column_shape_identity(&self, left: &ColumnKey, right: &ColumnKey) -> bool {
        self.sqlite_rowid.get(left).copied().unwrap_or(false)
            == self.sqlite_rowid.get(right).copied().unwrap_or(false)
    }

    /// The VENDOR half of STRUCTURAL DRIFT for one column.
    ///
    /// A DIFFERENT set from [`Self::column_shape_identity`], and that is the finding
    /// rather than a design choice: `apply::drift::column_data_types_eq`
    /// (`apply/drift.rs:2799-2840`) reads `mysql_physical_type` on both sides and
    /// `sqlite_rowid`, while `ColumnSnapshot::eq` reads `sqlite_rowid` only. Two
    /// definitions of "the same column" already exist in the tree; naming both is what
    /// stops the next reader assuming there is one.
    ///
    /// `mysql_physical_type` declines when EITHER side is absent, exactly as
    /// `column_data_types_eq` does, because an author-built desired snapshot has not
    /// derived one and accusing it of a type change would be a phantom.
    /// `MysqlPhysicalType::Unknown` is deliberately not resolved here: its own doc
    /// requires each consumer to decide what an unmodelled type means for it, and a
    /// DIFFER must refuse to report a difference it cannot establish, so an `Unknown`
    /// on either side declines too.
    #[must_use]
    pub fn column_drift_identity(&self, left: &ColumnKey, right: &ColumnKey) -> bool {
        if !self.column_shape_identity(left, right) {
            return false;
        }
        match (
            self.mysql_physical_type.get(left),
            self.mysql_physical_type.get(right),
        ) {
            (Some(MysqlPhysicalType::Unknown { .. }), _)
            | (_, Some(MysqlPhysicalType::Unknown { .. })) => true,
            (Some(left), Some(right)) => left == right,
            _ => true,
        }
    }

    /// The VENDOR half of index PAIRING, of index SHAPE, and of index DRIFT - one
    /// function, because measurement says all three vendor answers are the same.
    ///
    /// Always `true`, and that is a MEASURED statement rather than a stub. All three
    /// PostgreSQL index families here are emission-only and excluded from
    /// `IndexSnapshot`'s equality today, each for a reason recorded in its field doc:
    /// `only` because `pg_get_indexdef` renders `ON ONLY` for every index on a
    /// partitioned parent whether or not it was written (measured on PostgreSQL 18.4,
    /// `docs/review-log.md:9412-9432`), `opclass` because live introspection cannot
    /// recover it cheaply, and `nulls_not_distinct` because recovery is out of scope.
    ///
    /// It is a function rather than a constant so the site exists to be changed, and so
    /// `tests/schema_model_field_routing.rs` has somewhere to route the three families.
    #[must_use]
    pub fn index_identity(&self, _left: &IndexKey, _right: &IndexKey) -> bool {
        true
    }

    /// The VENDOR half of TABLE-SHAPE identity for one table.
    ///
    /// Always `true`. `sqlite_stored_create_sql` is introspection-only
    /// (`sqlite_master.sql`), `None` on PostgreSQL and on author-built desired
    /// snapshots, and `TableSnapshot::eq` excludes it today for exactly that reason.
    #[must_use]
    pub fn table_identity(&self, _left: &TableKey, _right: &TableKey) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// The neutral model
// ---------------------------------------------------------------------------

/// One column, with every vendor-specific fact removed to [`VendorFacts`].
///
/// DERIVES [`PartialEq`]. A new field is compared by default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// The portable SQL data type.
    pub data_type: String,
    /// `true` if the column is nullable.
    pub nullable: bool,
    /// The `DEFAULT` clause expression to emit at CREATE / ADD COLUMN.
    pub default: Option<String>,
    /// Dialect-rendered type spelling to use in DDL instead of deriving from
    /// `data_type`.
    ///
    /// The VALUE is dialect-rendered; the FIELD is neutral, because every backend has a
    /// spelling to emit. That is the cut [`VendorFacts`] draws: a family lands there
    /// when only one vendor HAS the concept, not when the value happens to be rendered
    /// for one.
    pub ddl_type_override: Option<String>,
    /// Column-level CHECK clauses to append at the use-site.
    pub inline_checks: Vec<String>,
    /// A generated/computed column expression plus whether it is STORED or VIRTUAL.
    pub generated: Option<GeneratedColumnSnapshot>,
    /// The STRUCTURAL generated facet, beside the rendered expression above.
    pub generated_kind: Option<GeneratedKindSnapshot>,
    /// SQL identity / portable auto-increment facet.
    pub identity: Option<IdentityCol>,
    /// A locally enforced TypeID/ULID format CHECK recovered from the catalog.
    pub value_format: Option<ValueFormat>,
    /// Semantic drift key for a default on an ID-bearing column.
    pub id_default: Option<IdDefaultSnapshot>,
    /// `Some(false)` means this logical text column is case-insensitive.
    pub case_sensitive: Option<bool>,
    /// Exact non-default catalog collation identity.
    pub collation: Option<ColumnCollationSnapshot>,
    /// The inline encryption sentinel to append after this column's type.
    pub encryption_sentinel: Option<String>,
    /// The `COMMENT ON COLUMN` sentinel body for this column.
    pub comment_sentinel: Option<String>,
    /// User-authored catalog comment on this column.
    pub comment: Option<String>,
}

/// One ordered key element of an index, with the PostgreSQL per-element facets removed
/// to [`VendorFacts`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexElement {
    /// Plain column key.
    Column {
        /// Column name.
        name: String,
        /// Optional per-column sort order. `None` is canonical ASC/default.
        order: Option<IndexSortOrder>,
    },
    /// Expression key.
    Expr(String),
}

/// One index, with the PostgreSQL emission-only facets removed to [`VendorFacts`].
///
/// DERIVES [`PartialEq`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// Index name.
    pub name: String,
    /// `true` if it enforces uniqueness.
    pub unique: bool,
    /// The KEY columns the index covers, in index order.
    pub columns: Vec<String>,
    /// Ordered key elements, including both plain columns and expression keys.
    pub elements: Vec<IndexElement>,
    /// The index ACCESS METHOD.
    pub access_method: String,
    /// Partial-index predicate text, when present.
    pub predicate: Option<String>,
    /// Non-key covering columns (`INCLUDE (...)`).
    pub include: Vec<String>,
    /// Typed storage parameters (`WITH (...)`).
    pub with: Option<IndexStorageParams>,
    /// User-authored catalog comment on this index.
    pub comment: Option<String>,
    /// Provenance-only local columns read by this index's rendered-SQL sites.
    pub expr_cascade_columns: Option<Vec<String>>,
}

/// One constraint. DERIVES [`PartialEq`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    /// Constraint name.
    pub name: String,
    /// The constraint type: `PRIMARY KEY`, `FOREIGN KEY`, `UNIQUE`, `CHECK`, `EXCLUDE`.
    pub kind: String,
    /// The full constraint definition.
    pub definition: String,
    /// User-authored catalog comment on this constraint.
    pub comment: Option<String>,
    /// Provenance-only local columns whose drop cascades this constraint away.
    pub cascade_columns: Option<Vec<String>>,
}

/// One table. DERIVES [`PartialEq`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    /// Columns, ordered by name.
    pub columns: Vec<Column>,
    /// Indexes, ordered by name.
    pub indexes: Vec<Index>,
    /// Constraints, ordered by name.
    pub constraints: Vec<Constraint>,
    /// Runtime-visible collection options.
    pub runtime_options: TableRuntimeOptions,
    /// Partitioning strategy for a partitioned table parent.
    pub partition_by: Option<PartitionSpec>,
    /// User-authored catalog comment on this table.
    pub comment: Option<String>,
}

/// The neutral schema model plus the vendor side table.
///
/// DERIVES [`PartialEq`]: two models are equal when every table AND every vendor fact
/// is equal. There is no exclusion list on this type and there is nothing to maintain.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchemaModel {
    /// Tables, keyed and ordered by name.
    pub tables: BTreeMap<String, Table>,
    /// Vendor facts, keyed by object. NOT reachable from [`Table`] or [`Column`].
    pub vendor: VendorFacts,
}

// ---------------------------------------------------------------------------
// The bridge to today's snapshot types
// ---------------------------------------------------------------------------

impl Column {
    /// Split one [`ColumnSnapshot`] into its neutral half and its vendor facts.
    #[must_use]
    pub fn from_snapshot(table: &str, snapshot: &ColumnSnapshot, vendor: &mut VendorFacts) -> Self {
        let key = ColumnKey::new(table, &snapshot.name);
        vendor
            .sqlite_rowid
            .insert(key.clone(), snapshot.sqlite_rowid);
        vendor
            .catalog_uuid_format_check
            .insert(key.clone(), snapshot.catalog_uuid_format_check);
        if let Some(generated) = snapshot.mysql_default_generated {
            vendor
                .mysql_default_generated
                .insert(key.clone(), generated);
        }
        if let Some(storage) = snapshot.mysql_text_storage.clone() {
            vendor.mysql_text_storage.insert(key.clone(), storage);
        }
        if let Some(physical) = snapshot.mysql_physical_type.clone() {
            vendor.mysql_physical_type.insert(key, physical);
        }
        Self {
            name: snapshot.name.clone(),
            data_type: snapshot.data_type.clone(),
            nullable: snapshot.nullable,
            default: snapshot.default.clone(),
            ddl_type_override: snapshot.ddl_type_override.clone(),
            inline_checks: snapshot.inline_checks.clone(),
            generated: snapshot.generated.clone(),
            generated_kind: snapshot.generated_kind,
            identity: snapshot.identity,
            value_format: snapshot.value_format.clone(),
            id_default: snapshot.id_default.clone(),
            case_sensitive: snapshot.case_sensitive,
            collation: snapshot.collation.clone(),
            encryption_sentinel: snapshot.encryption_sentinel.clone(),
            comment_sentinel: snapshot.comment_sentinel.clone(),
            comment: snapshot.comment.clone(),
        }
    }

    /// Rejoin the neutral half with its vendor facts. The inverse of
    /// [`Self::from_snapshot`], asserted byte-exact by
    /// `tests/schema_model_roundtrip_*`.
    #[must_use]
    pub fn to_snapshot(&self, table: &str, vendor: &VendorFacts) -> ColumnSnapshot {
        let key = ColumnKey::new(table, &self.name);
        ColumnSnapshot {
            name: self.name.clone(),
            data_type: self.data_type.clone(),
            nullable: self.nullable,
            default: self.default.clone(),
            ddl_type_override: self.ddl_type_override.clone(),
            inline_checks: self.inline_checks.clone(),
            generated: self.generated.clone(),
            generated_kind: self.generated_kind,
            identity: self.identity,
            sqlite_rowid: vendor.sqlite_rowid.get(&key).copied().unwrap_or(false),
            value_format: self.value_format.clone(),
            catalog_uuid_format_check: vendor
                .catalog_uuid_format_check
                .get(&key)
                .copied()
                .unwrap_or(false),
            id_default: self.id_default.clone(),
            mysql_default_generated: vendor.mysql_default_generated.get(&key).copied(),
            case_sensitive: self.case_sensitive,
            collation: self.collation.clone(),
            mysql_text_storage: vendor.mysql_text_storage.get(&key).cloned(),
            mysql_physical_type: vendor.mysql_physical_type.get(&key).cloned(),
            encryption_sentinel: self.encryption_sentinel.clone(),
            comment_sentinel: self.comment_sentinel.clone(),
            comment: self.comment.clone(),
        }
    }
}

impl Index {
    /// Split one [`IndexSnapshot`] into its neutral half and its vendor facts.
    #[must_use]
    pub fn from_snapshot(table: &str, snapshot: &IndexSnapshot, vendor: &mut VendorFacts) -> Self {
        let key = IndexKey::new(table, &snapshot.name);
        vendor.pg_index_only.insert(key.clone(), snapshot.only);
        vendor
            .pg_index_nulls_not_distinct
            .insert(key.clone(), snapshot.nulls_not_distinct);
        if let Some(opclass) = snapshot.opclass.clone() {
            vendor.pg_index_opclass.insert(key, opclass);
        }

        let elements = snapshot
            .elements
            .iter()
            .enumerate()
            .map(|(position, element)| match element {
                IndexElementSnapshot::Column {
                    name,
                    order,
                    opclass,
                    collation,
                } => {
                    let element_key = IndexElementKey::new(table, &snapshot.name, position);
                    if let Some(opclass) = opclass.clone() {
                        vendor
                            .pg_index_element_opclass
                            .insert(element_key.clone(), opclass);
                    }
                    if let Some(collation) = collation.clone() {
                        vendor
                            .pg_index_element_collation
                            .insert(element_key, collation);
                    }
                    IndexElement::Column {
                        name: name.clone(),
                        order: *order,
                    }
                }
                IndexElementSnapshot::Expr(expr) => IndexElement::Expr(expr.clone()),
            })
            .collect();

        Self {
            name: snapshot.name.clone(),
            unique: snapshot.unique,
            columns: snapshot.columns.clone(),
            elements,
            access_method: snapshot.access_method.clone(),
            predicate: snapshot.predicate.clone(),
            include: snapshot.include.clone(),
            with: snapshot.with.clone(),
            comment: snapshot.comment.clone(),
            expr_cascade_columns: snapshot.expr_cascade_columns.clone(),
        }
    }

    /// Rejoin the neutral half with its vendor facts.
    #[must_use]
    pub fn to_snapshot(&self, table: &str, vendor: &VendorFacts) -> IndexSnapshot {
        let key = IndexKey::new(table, &self.name);
        IndexSnapshot {
            name: self.name.clone(),
            unique: self.unique,
            columns: self.columns.clone(),
            elements: self
                .elements
                .iter()
                .enumerate()
                .map(|(position, element)| match element {
                    IndexElement::Column { name, order } => {
                        let element_key = IndexElementKey::new(table, &self.name, position);
                        IndexElementSnapshot::Column {
                            name: name.clone(),
                            order: *order,
                            opclass: vendor.pg_index_element_opclass.get(&element_key).cloned(),
                            collation: vendor.pg_index_element_collation.get(&element_key).cloned(),
                        }
                    }
                    IndexElement::Expr(expr) => IndexElementSnapshot::Expr(expr.clone()),
                })
                .collect(),
            access_method: self.access_method.clone(),
            predicate: self.predicate.clone(),
            include: self.include.clone(),
            with: self.with.clone(),
            only: vendor.pg_index_only.get(&key).copied().unwrap_or(false),
            opclass: vendor.pg_index_opclass.get(&key).cloned(),
            nulls_not_distinct: vendor
                .pg_index_nulls_not_distinct
                .get(&key)
                .copied()
                .unwrap_or(false),
            comment: self.comment.clone(),
            expr_cascade_columns: self.expr_cascade_columns.clone(),
        }
    }
}

impl Constraint {
    /// Split one [`ConstraintSnapshot`] into the neutral model. A constraint carries no
    /// vendor fact today, which is why this takes no [`VendorFacts`].
    #[must_use]
    pub fn from_snapshot(snapshot: &ConstraintSnapshot) -> Self {
        Self {
            name: snapshot.name.clone(),
            kind: snapshot.kind.clone(),
            definition: snapshot.definition.clone(),
            comment: snapshot.comment.clone(),
            cascade_columns: snapshot.cascade_columns.clone(),
        }
    }

    /// Rejoin. The inverse of [`Self::from_snapshot`].
    #[must_use]
    pub fn to_snapshot(&self) -> ConstraintSnapshot {
        ConstraintSnapshot {
            name: self.name.clone(),
            kind: self.kind.clone(),
            definition: self.definition.clone(),
            comment: self.comment.clone(),
            cascade_columns: self.cascade_columns.clone(),
        }
    }
}

impl Table {
    /// Split one [`TableSnapshot`] into its neutral half and its vendor facts.
    #[must_use]
    pub fn from_snapshot(name: &str, snapshot: &TableSnapshot, vendor: &mut VendorFacts) -> Self {
        if let Some(stored) = snapshot.stored_create_sql.clone() {
            vendor
                .sqlite_stored_create_sql
                .insert(TableKey::new(name), stored);
        }
        Self {
            columns: snapshot
                .columns
                .iter()
                .map(|column| Column::from_snapshot(name, column, vendor))
                .collect(),
            indexes: snapshot
                .indexes
                .iter()
                .map(|index| Index::from_snapshot(name, index, vendor))
                .collect(),
            constraints: snapshot
                .constraints
                .iter()
                .map(Constraint::from_snapshot)
                .collect(),
            runtime_options: snapshot.runtime_options.clone(),
            partition_by: snapshot.partition_by.clone(),
            comment: snapshot.comment.clone(),
        }
    }

    /// Rejoin the neutral half with its vendor facts.
    #[must_use]
    pub fn to_snapshot(&self, name: &str, vendor: &VendorFacts) -> TableSnapshot {
        TableSnapshot {
            columns: self
                .columns
                .iter()
                .map(|column| column.to_snapshot(name, vendor))
                .collect(),
            indexes: self
                .indexes
                .iter()
                .map(|index| index.to_snapshot(name, vendor))
                .collect(),
            constraints: self
                .constraints
                .iter()
                .map(Constraint::to_snapshot)
                .collect(),
            runtime_options: self.runtime_options.clone(),
            partition_by: self.partition_by.clone(),
            comment: self.comment.clone(),
            stored_create_sql: vendor
                .sqlite_stored_create_sql
                .get(&TableKey::new(name))
                .cloned(),
        }
    }
}

impl SchemaModel {
    /// Build a model from today's table snapshots.
    #[must_use]
    pub fn from_tables(tables: &BTreeMap<String, TableSnapshot>) -> Self {
        let mut vendor = VendorFacts::default();
        let tables = tables
            .iter()
            .map(|(name, table)| (name.clone(), Table::from_snapshot(name, table, &mut vendor)))
            .collect();
        Self { tables, vendor }
    }

    /// Project the model back to today's table snapshots.
    #[must_use]
    pub fn to_tables(&self) -> BTreeMap<String, TableSnapshot> {
        self.tables
            .iter()
            .map(|(name, table)| (name.clone(), table.to_snapshot(name, &self.vendor)))
            .collect()
    }

    /// The FULL table-shape verdict for one column: the neutral half AND the vendor
    /// half, combined here and nowhere else.
    ///
    /// The combination is the point. Today it is implicit - `ColumnSnapshot::eq` compares
    /// `sqlite_rowid` alongside nine neutral fields, so every consumer of column equality
    /// silently inherits a SQLite fact. Here the caller can SEE that the verdict has two
    /// inputs, and a backend that needs another vendor term adds it to
    /// [`VendorFacts::column_shape_identity`] without touching [`column_shape_identity`]
    /// or any other consumer.
    ///
    /// Both sides are keyed separately so a RENAMED column can still be compared - the
    /// key is `(table, column-name)` and a rename changes it.
    #[must_use]
    pub fn column_shape_identity(
        &self,
        left_table: &str,
        left: &Column,
        right_table: &str,
        right: &Column,
    ) -> bool {
        column_shape_identity(left, right)
            && self.vendor.column_shape_identity(
                &ColumnKey::new(left_table, &left.name),
                &ColumnKey::new(right_table, &right.name),
            )
    }

    /// The FULL structural-drift verdict for one column. Strictly stronger than
    /// [`Self::column_shape_identity`] on both halves: it adds `generated_kind`
    /// neutrally and `mysql_physical_type` vendor-side.
    #[must_use]
    pub fn column_drift_identity(
        &self,
        left_table: &str,
        left: &Column,
        right_table: &str,
        right: &Column,
    ) -> bool {
        drift_identity(left, right)
            && self.vendor.column_drift_identity(
                &ColumnKey::new(left_table, &left.name),
                &ColumnKey::new(right_table, &right.name),
            )
    }

    /// The FULL table-shape verdict for one table: [`table_shape_identity`] over the
    /// neutral halves, each column recombined with its vendor facts, and the table's own
    /// vendor term.
    ///
    /// This is the function `tests/schema_model_comparator_equivalence_pg.rs` proves
    /// equal to `TableSnapshot::eq` on live data, and therefore the one a consumer would
    /// move onto in step 4.
    #[must_use]
    pub fn table_shape_identity(
        &self,
        left_table: &str,
        left: &Table,
        right_table: &str,
        right: &Table,
    ) -> bool {
        table_shape_identity(left, right)
            && left.columns.len() == right.columns.len()
            && left
                .columns
                .iter()
                .zip(&right.columns)
                .all(|(a, b)| self.column_shape_identity(left_table, a, right_table, b))
            && left.indexes.len() == right.indexes.len()
            && left.indexes.iter().zip(&right.indexes).all(|(a, b)| {
                self.vendor.index_identity(
                    &IndexKey::new(left_table, &a.name),
                    &IndexKey::new(right_table, &b.name),
                )
            })
            && self
                .vendor
                .table_identity(&TableKey::new(left_table), &TableKey::new(right_table))
    }
}

// ---------------------------------------------------------------------------
// The named comparators
// ---------------------------------------------------------------------------
//
// Each one is named after the QUESTION it answers, measured from the call sites, not
// after the impl it was extracted from. Every one destructures its input with no `..`,
// so a field added to the neutral model is a compile error in EVERY comparator until it
// is either compared or explicitly ignored with a reason.
//
// The measurement that fixed the names is in the module doc and is worth restating
// here, because it contradicts what `docs/proposals/single-fold-and-effects.md` section
// D asked for. That section proposes `drift_identity(&Column, &Column)` as the
// replacement for `ColumnSnapshot::eq`'s exclusion list, on the premise that structural
// drift is what reads it. Measured across `crates/*/src` with `cfg(test)` excluded, that
// premise is FALSE in both directions:
//
//   * `apply/drift.rs` contains NO use of `PartialEq` on `ColumnSnapshot`,
//     `IndexSnapshot`, `ConstraintSnapshot`, `TableSnapshot` or `ViewSnapshot`. Its
//     column pass matches by name into a `BTreeMap` and then compares named fields by
//     hand (`diff_attrs`, `apply/drift.rs:2846-3141`).
//   * `ColumnSnapshot::eq` has exactly ONE production consumer, and it is
//     `TableSnapshot::eq` (`model/snapshot.rs:1279`). No production code compares two
//     columns directly.
//
// So `ColumnSnapshot::eq` is not the drift comparator; it is the TABLE-SHAPE
// comparator, and its four real consumers are all in `render/declarative.rs`. Extracting
// it under the name `drift_identity` would have taught the next reader something false.
// Both questions are named below, and `drift_identity` is defined from what
// `apply::drift` actually compares - which is STRICTLY STRONGER, one field apart.

/// **Column shape identity**: are these the same column for the purposes of deciding
/// whether two TABLES are the same table?
///
/// The neutral half of `ColumnSnapshot::eq`. The vendor half is
/// [`VendorFacts::column_shape_identity`]; combine them with
/// [`SchemaModel::column_shape_identity`], which is what
/// `tests/schema_model_comparator_equivalence_pg.rs` and its MySQL sibling prove answers
/// identically to `ColumnSnapshot::eq` on snapshots taken from live servers through
/// engine-emitted SQL.
///
/// ## What it ignores, and why - the list an exclusion comment could never be
///
/// Each entry moved here from the field's own doc in [`crate::model::snapshot`]. They
/// are one shape: EMISSION METADATA that only one side of a comparison ever populates,
/// so comparing it reports a difference on a database that never changed.
///
/// * `default` - PostgreSQL normalises a stored default (`'{}'` -> `'{}'::jsonb`,
///   `NOW()` -> `now()`), so a byte compare of the authored default against the
///   introspected one phantom-drifts. The comparable projection is `id_default`, which
///   IS compared.
/// * `ddl_type_override` - emission-only spelling for a named type reference. The
///   introspectable `data_type` is compared instead.
/// * `inline_checks` - emission-only. Live introspection tracks table constraints
///   separately; only recognised ID-format CHECKs project into `value_format`, which IS
///   compared.
/// * `generated` - the RENDERED expression. Live introspection does not carry it into
///   the structural snapshot. The comparable half is `generated_kind`, which THIS
///   comparator also ignores but [`drift_identity`] does not - see there.
/// * `generated_kind` - see [`drift_identity`].
/// * `encryption_sentinel` and `comment_sentinel` - emission-only sentinels. Only
///   `desired_snapshot` populates the first; PostgreSQL introspection classifies
///   matching catalog comments into the second rather than into the user-facing
///   `comment`, which IS compared.
///
/// Two of those exclusions are LOAD-BEARING for a shipped fix rather than merely
/// defensible, and that is the fact section D of the proposal is about.
/// `docs/review-log.md:28481-28486`: `rename_column_in_generated_columns` and
/// `rename_column_in_inline_checks` write into the DESIRED snapshot from
/// `sqlite_rename_rebuild`, and "that is safe only because `ColumnSnapshot`'s
/// `PartialEq` excludes both fields". The mirror case is the bug that stayed unfixed for
/// a commit: `ConstraintSnapshot`'s equality does NOT exclude `definition`, so the same
/// rewrite one field over would have flipped `preserve_stored_shape` off. See
/// [`rename_equivalence_identity`], which is the comparator that dependency actually
/// runs through.
#[must_use]
pub fn column_shape_identity(left: &Column, right: &Column) -> bool {
    // EXHAUSTIVE, no `..`: a new `Column` field is a COMPILE ERROR here until it is
    // either compared below or bound to an `_ignored_*` name with a reason above. The
    // derived `PartialEq` already made the DEFAULT "compared"; this makes the OPT-OUT
    // explicit too.
    let Column {
        name,
        data_type,
        nullable,
        default: _ignored_default,
        ddl_type_override: _ignored_ddl_type_override,
        inline_checks: _ignored_inline_checks,
        generated: _ignored_generated,
        generated_kind: _ignored_generated_kind,
        identity,
        value_format,
        id_default,
        case_sensitive,
        collation,
        encryption_sentinel: _ignored_encryption_sentinel,
        comment_sentinel: _ignored_comment_sentinel,
        comment,
    } = left;

    *name == right.name
        && *data_type == right.data_type
        && *nullable == right.nullable
        && *identity == right.identity
        && *value_format == right.value_format
        && *id_default == right.id_default
        && *case_sensitive == right.case_sensitive
        && *collation == right.collation
        && *comment == right.comment
}

/// **Column drift identity**: would a user call the difference between these two columns
/// a change to their schema?
///
/// A DIFFERENT question from [`column_shape_identity`], and today a different ANSWER.
/// This is the neutral field set `apply::drift::diff_attrs` compares
/// (`apply/drift.rs:2901-3011`), and it is [`column_shape_identity`] plus exactly one
/// field: `generated_kind`, compared at `apply/drift.rs:2919` through
/// `comparable_generated_column`.
///
/// So `column_shape_identity(a, b) == true` does NOT imply the drift pass is quiet, and
/// `drift_identity(a, b) => column_shape_identity(a, b)` is the implication that DOES
/// hold. `tests/schema_model_comparator_equivalence_pg.rs` asserts the implication in
/// both directions: it must hold, AND there must exist a real column pair where the two
/// disagree, so "these are two questions" is a measurement and not a claim.
///
/// This divergence is exactly what section D predicted. `generated_kind`'s own field doc
/// declines to join `ColumnSnapshot::eq` because "adding it would change what every
/// consumer of column equality means by 'the same column' [...] when only the drift
/// comparator is asking". With the comparators named, that sentence stops being a reason
/// to decline and becomes a routing instruction.
///
/// The vendor half is [`VendorFacts::column_drift_identity`], which adds
/// `mysql_physical_type`. `default` is NOT here: `apply::drift` compares it only through
/// the dialect-gated `comparable_column_default` (`apply/drift.rs:2515-2538`), so it
/// belongs to a backend rather than to the neutral model, and this comparator declining
/// to guess is the whole point of the dialect boundary.
#[must_use]
pub fn drift_identity(left: &Column, right: &Column) -> bool {
    // EXHAUSTIVE, no `..`.
    let Column {
        name: _routed_by_shape_identity,
        data_type: _routed_by_shape_identity_2,
        nullable: _routed_by_shape_identity_3,
        default: _ignored_default_dialect_gated,
        ddl_type_override: _ignored_ddl_type_override,
        inline_checks: _ignored_inline_checks,
        generated: _ignored_generated_rendered,
        generated_kind,
        identity: _routed_by_shape_identity_4,
        value_format: _routed_by_shape_identity_5,
        id_default: _routed_by_shape_identity_6,
        case_sensitive: _routed_by_shape_identity_7,
        collation: _routed_by_shape_identity_8,
        encryption_sentinel: _ignored_encryption_sentinel,
        comment_sentinel: _ignored_comment_sentinel,
        comment: _routed_by_shape_identity_9,
    } = left;

    column_shape_identity(left, right)
        // `None` on either side means THIS PRODUCER DID NOT LOOK, so it declines rather
        // than accusing MySQL or SQLite of having dropped a generated column they never
        // modelled - the rule `comparable_generated_column` already enforces.
        && match (generated_kind, &right.generated_kind) {
            (Some(left), Some(right)) => left == right,
            _ => true,
        }
}

/// **Index pairing identity**: is this live index the same index as this declared one,
/// under a possibly DIFFERENT NAME?
///
/// The extraction of `IndexSnapshot::same_definition_except_name`. Measured, this is the
/// one comparator that already has two consumers agreeing through a shared function:
/// `render::declarative::pair_indexes` (`declarative.rs:4560`) adopts an alias-matched
/// live index only if it holds, and `apply::drift::diff_indexes` (`drift.rs:3627`)
/// delegates to that same `pair_indexes`, so drift and the plan cannot disagree about
/// WHICH live index a desired one meant. `diff_with_known_fk_targets`
/// (`declarative.rs:6461`) asks the same question through the sibling
/// `definition_differences_except_name`, for the refusal message.
///
/// It ignores the NAME, which is the whole point, and it ignores `expr_cascade_columns`
/// because that is provenance rather than identity - the offline producer records only
/// the expression sites while the PostgreSQL introspector's `pg_depend` read returns a
/// superset, so comparing them reports a difference on every index that has one.
///
/// `only`, `opclass` and `nulls_not_distinct` do not appear because they are not
/// reachable: they live in [`VendorFacts`]. That is the improvement over the impl this
/// replaces, where all three were fields of `IndexSnapshot` that a comparator had to
/// remember not to read - and where `only` was read by mistake for as long as nothing
/// authored it (`docs/review-log.md:9391-9411`).
#[must_use]
pub fn index_pairing_identity(left: &Index, right: &Index) -> bool {
    // EXHAUSTIVE, no `..`.
    let Index {
        name: _ignored_name_this_is_the_point,
        unique,
        columns,
        elements,
        access_method,
        predicate,
        include,
        with,
        comment,
        expr_cascade_columns: _ignored_expr_cascade_columns_provenance,
    } = left;

    *unique == right.unique
        && *columns == right.columns
        && index_elements_identity(elements, &right.elements)
        && *access_method == right.access_method
        && index_predicates_canonically_eq(predicate.as_deref(), right.predicate.as_deref())
        && *include == right.include
        && *with == right.with
        && *comment == right.comment
}

/// **Index shape identity**: pairing identity PLUS the name.
///
/// The extraction of `IndexSnapshot::eq`. The name is part of table shape because a
/// differently named index is a different catalog object; it is NOT part of pairing
/// because pairing exists precisely to survive a name derivation change. One line apart,
/// and today one impl - `IndexSnapshot::eq` is literally
/// `self.name == other.name && self.same_definition_except_name(other)`.
#[must_use]
pub fn index_shape_identity(left: &Index, right: &Index) -> bool {
    left.name == right.name && index_pairing_identity(left, right)
}

/// Canonical element comparison: DESC is preserved, ASC and absent are the same thing,
/// and expression text goes through the shared canonicaliser so quoting and whitespace
/// cannot fake a difference.
fn index_elements_identity(left: &[IndexElement], right: &[IndexElement]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| match (a, b) {
            (
                IndexElement::Column {
                    name: a,
                    order: order_a,
                },
                IndexElement::Column {
                    name: b,
                    order: order_b,
                },
            ) => {
                a == b
                    && canonical_index_sort_order(*order_a) == canonical_index_sort_order(*order_b)
            }
            (IndexElement::Expr(a), IndexElement::Expr(b)) => {
                index_predicates_canonically_eq(Some(a), Some(b))
            }
            _ => false,
        })
}

/// **Constraint shape identity.**
///
/// The extraction of `ConstraintSnapshot::eq`. Compares `definition` for EVERY kind
/// including `CHECK`, and the divergence from
/// `apply::drift::constraint_definition_is_comparable` - which skips a CHECK body
/// because PostgreSQL deparses one from the parse tree rather than echoing what was
/// written - is deliberate and argued at length on the field's own doc. Generalising the
/// differ's PG-shaped concession into this comparator would export it to every consumer.
///
/// `cascade_columns` is ignored because it is provenance, not identity: the fold-derived
/// and `conkey`-derived lists legitimately differ, and comparing them would report a
/// difference on constraints that are structurally identical.
#[must_use]
pub fn constraint_shape_identity(left: &Constraint, right: &Constraint) -> bool {
    // EXHAUSTIVE, no `..`.
    let Constraint {
        name,
        kind,
        definition,
        comment,
        cascade_columns: _ignored_cascade_columns_provenance,
    } = left;

    *name == right.name
        && *kind == right.kind
        && *definition == right.definition
        && *comment == right.comment
}

/// **Table shape identity**: are these the same table?
///
/// The neutral half of `TableSnapshot::eq`, whose four production consumers are all in
/// `render/declarative.rs`: `DesiredSchema`'s own equality (`:3450`),
/// `desired_snapshot_second_pass`'s `ConflictingDeclaration` check (`:4128`),
/// `pure_sqlite_column_rename` (`:4261`), and `enforce_ownership`'s "did anything
/// actually change" gate (`:6645`, `:6647`).
///
/// `runtime_options` is ignored because live catalog introspection cannot recover it, so
/// the offline fold is its only authority and comparing it would report a difference
/// against every live snapshot. `stored_create_sql` does not appear because it is not
/// reachable: it lives in [`VendorFacts`].
#[must_use]
pub fn table_shape_identity(left: &Table, right: &Table) -> bool {
    // EXHAUSTIVE, no `..`.
    let Table {
        columns,
        indexes,
        constraints,
        runtime_options: _ignored_runtime_options_not_introspectable,
        partition_by,
        comment,
    } = left;

    columns.len() == right.columns.len()
        && columns
            .iter()
            .zip(&right.columns)
            .all(|(a, b)| column_shape_identity(a, b))
        && indexes.len() == right.indexes.len()
        && indexes
            .iter()
            .zip(&right.indexes)
            .all(|(a, b)| index_shape_identity(a, b))
        && constraints.len() == right.constraints.len()
        && constraints
            .iter()
            .zip(&right.constraints)
            .all(|(a, b)| constraint_shape_identity(a, b))
        && *partition_by == right.partition_by
        && *comment == right.comment
}

/// **Pure-rename equivalence**: after applying a candidate rename to the live table, is
/// it now indistinguishable from the desired one - so the SQLite rebuild may replay
/// `sqlite_master`'s stored `CREATE TABLE` verbatim instead of re-rendering it?
///
/// The question `render::declarative::pure_sqlite_column_rename` (`declarative.rs:4261`)
/// asks. Today it asks it through `TableSnapshot::eq`, so its answer is
/// [`table_shape_identity`] and this function is defined as that - deliberately, because
/// this step preserves behaviour exactly and
/// `tests/schema_model_comparator_equivalence_pg.rs` proves it does.
///
/// **Naming it is the deliverable, not changing it.** Section D of the proposal shows
/// this is the comparator that has already BLOCKED A FIX: because
/// `ConstraintSnapshot::eq` compares `definition`, following a column rename into a
/// constraint definition inside `sqlite_rename_rebuild` would have made the desired table
/// unequal to the renamed live one, flipped `preserve_stored_shape` off, and stopped the
/// catalog path replaying SQLite's own stored body
/// (`docs/review-log.md:28291-28297`, `28481-28487`). The rewrite had to be moved a layer
/// down into `render_create_table_sqlite_rebuild` instead.
///
/// So the coupling is real and it stays. What changes is that it is now a DECLARED INPUT
/// of one named function with the consequence written next to it, instead of a fact you
/// discover from a failing SQLite rebuild. The next person who wants to follow a rename
/// into a new carrier can read here what depends on their choice - and, because this is
/// a separate function from [`table_shape_identity`], can change one without the other
/// the moment the two answers need to differ.
#[must_use]
pub fn rename_equivalence_identity(left: &Table, right: &Table) -> bool {
    table_shape_identity(left, right)
}
