//! The BACKEND CONTRACT: what core asks a backend, and how a backend is named.
//!
//! Core never asks "which dialect is this". It asks "can you do this". The
//! vocabulary for that question is [`Capability`]; the answer is a
//! [`BackendDescriptor`], which also carries the backend's [`DialectId`], its
//! human-facing name, and its [`Limits`].
//!
//! # Why this lives in the leaf crate
//!
//! `docs/proposals/pluggable-backends.md` puts this contract in a separate
//! `zero-migrate-backend` crate. While the backend crates sit in-tree, a separate
//! contract crate would have exactly one consumer and one implementor, so the
//! contract lives here instead: this is already the bottom of the crate graph,
//! already the crate the engine, the guard, and the N-API addon all name, and
//! relocating these items is a `pub use` away.
//!
//! # What is NOT here
//!
//! The `Backend` trait itself (`introspect` / `render` / `execute`). Those
//! signatures name `SchemaModel`, `ChangeSet` and `ExecutionPlan`, which are
//! engine types; naming them here would invert the crate graph. This module
//! promotes IDENTITY and CAPABILITY to public vocabulary and nothing else.

use core::fmt;

use crate::dialect::{DialectId, DialectSet};

// ---------------------------------------------------------------------------
// Capability
// ---------------------------------------------------------------------------

/// A question CORE ASKS a backend. Never a vendor name.
///
/// Promoted from `zero_migrate::render::renderer`, where it was `pub(crate)`.
/// The promotion changed neither spelling nor meaning of any predicate it
/// carried over; membership has grown since, which is what the paragraph below
/// is about. (This doc used to pin a count of the promoted predicates. The count
/// was already wrong by four before this variant was added, and a stale number
/// in prose reads as authoritative - so the invariant is stated instead:
/// `ALL` is the membership, and the shipping census asserts against its length.)
///
/// Keep this enum CLOSED. Adding a capability is a core change and should be
/// rare; adding a BACKEND is not a core change at all. A backend that needs a
/// predicate nobody else has does not add one here - it keeps that fact private
/// to its own rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability {
    /// An identity column that is not (part of) the primary key.
    NonPkIdentity,
    /// `GENERATED ALWAYS AS (...) VIRTUAL` (as opposed to `STORED`).
    VirtualGeneratedColumn,
    /// DDL that names a schema other than the project schema.
    CrossSchemaDdl,
    /// A table-level `FOREIGN KEY` clause in `CREATE TABLE`.
    TableLevelForeignKey,
    /// A table-level `UNIQUE` clause in `CREATE TABLE`.
    TableLevelUnique,
    /// An index method other than btree.
    NonBtreeIndexMethod,
    /// A partial index `WHERE` predicate.
    PartialIndexPredicate,
    /// In-place `ALTER TABLE ... ALTER COLUMN` (as opposed to a table rebuild).
    NativeAlterColumn,
    /// `ALTER TABLE ... ADD CONSTRAINT`.
    AlterTableAddConstraint,
    /// `ALTER TABLE ... DROP CONSTRAINT`.
    AlterTableDropConstraint,
    /// `ALTER TABLE ... VALIDATE CONSTRAINT` (the `NOT VALID` adoption path).
    AlterTableValidateConstraint,
    /// An upsert clause on `INSERT`.
    InsertOnConflictClause,
    /// The privileged catalog-object family a backend renders through its own
    /// vendor-op renderer: namespaces, server extensions, roles and the grants
    /// over them, row-level security and its policies, stored functions and
    /// triggers - plus the audited raw-statement escape, which is gated with them
    /// because it is equally privileged rather than because it is an object.
    ///
    /// One question rather than eight because it is answered all-or-nothing: a
    /// backend either supplies a vendor-op renderer or it does not. That is a
    /// coarser grain than the rest of this enum and worth splitting if a backend
    /// ever arrives that has roles but no policies.
    PrivilegedCatalogObjects,
    /// Materialized views.
    MaterializedView,
    /// `CREATE OR REPLACE VIEW`.
    CreateOrReplaceView,
    /// A `TRUNCATE` trigger event.
    TriggerTruncateEvent,
    /// `FOR EACH STATEMENT` triggers.
    TriggerStatementForEach,
    /// `CREATE TRIGGER ... EXECUTE FUNCTION`.
    TriggerExecuteFunction,
    /// An inline trigger BODY (as opposed to a named function).
    TriggerBody,
    /// A materialized (catalog-level) enum type.
    MaterializedEnumType,
    /// A materialized (catalog-level) domain type.
    MaterializedDomainType,
    /// Standalone sequence objects.
    Sequence,
    /// Exclusion constraints.
    ExclusionConstraint,
    /// `COMMENT ON`.
    CommentOn,
    /// Index names that are unique per SCHEMA rather than per TABLE.
    SchemaWideIndexNames,
    /// DDL that participates in the surrounding transaction, so a failed step
    /// rolls back rather than leaving the catalog half-changed.
    ///
    /// The render layer needs this to set `MigrationFlags::transactional`, and
    /// it is the SAME fact the apply layer already asks by property as
    /// `Backend::ddl_is_transactional`. Before this capability existed the
    /// renderer asked it by NAME (comparing against the former closed enum's MySQL
    /// variant), so a fourth backend would have silently claimed transactional DDL.
    ///
    /// Note this is stricter than MySQL 8.0's ATOMIC DDL: atomic DDL makes a
    /// single DDL statement all-or-nothing, but an implicit COMMIT still
    /// brackets it, so it cannot roll back with the journal row. That is why
    /// MySQL answers NO here even on 8.0.
    TransactionalDdl,
    /// `DEFERRABLE` / `INITIALLY DEFERRED` constraint checking.
    DeferrableConstraint,
    /// A named `UNIQUE` constraint is a CATALOG OBJECT distinct from the index
    /// backing it, so dropping the constraint leaves any same-name index alone.
    ///
    /// Where this is NO the catalog collapses the two into one key object, and a
    /// snapshot that still carried a synthetic constraint row would report it
    /// missing on every re-introspection.
    UniqueConstraintDistinctFromIndex,
    /// An INTEGER PRIMARY KEY ALIASES the table's row id, so declaring one
    /// silently makes the column auto-generating.
    ///
    /// A storage-model fact, not a syntax one: core must refuse to INTRODUCE
    /// such a primary key, because the generation it turns on was never
    /// authored.
    IntegerPrimaryKeyRowidAlias,
    /// A partition is a RELATION IN ITS OWN RIGHT - it has a name the catalog
    /// resolves, it is created against a declared parent with declared bounds,
    /// and it can be attached to and detached from that parent as an
    /// independent table.
    ///
    /// # Why the question is about relations and not about "partitioning"
    ///
    /// Naming this `NativePartitioning` would make one of the three shipping
    /// answers a lie. MySQL HAS native partitioning: `PARTITION BY RANGE/LIST/
    /// HASH/KEY` is first-class, server-enforced, and older than PostgreSQL's
    /// declarative model. What MySQL does not have is a partition that is a
    /// RELATION: its partitions are storage divisions of one table, unnamed in
    /// the relation namespace, with no `CREATE TABLE ... PARTITION OF`, no
    /// `ATTACH PARTITION`, and no `DETACH` that yields a standalone table.
    /// `EXCHANGE PARTITION` swaps rows between a partition and a
    /// structurally-identical table; it is not the same operation and does not
    /// leave the partition behind as its own object.
    ///
    /// So MySQL answers NO here, and the NO is true rather than merely
    /// convenient: the engine's whole partition surface - `createPartition`,
    /// `attachPartition`, `detachPartition`, `dropPartition`, and a parent
    /// created by `createTable { partitionBy }` - is written in relations, and
    /// MySQL has nowhere to put one.
    ///
    /// # What answering YES commits a backend to
    ///
    /// All four `DdlEmitter` partition methods returning `Some`. Those methods
    /// are required with no default precisely so a backend states the complete
    /// boundary, and the render layer `expect`s them once this capability says
    /// yes - so a backend that claims the capability and refuses an emitter
    /// would panic mid-render rather than refuse cleanly. The shipping registry
    /// census
    /// (`crates/zero-migrate/tests/dialect_matrix/vendor_registry_owns_shipping_descriptors.rs`)
    /// holds the two answers together.
    ///
    /// # What answering NO means
    ///
    /// Not "partitions are refused". A backend that says NO still honours an
    /// author's affirmed `partitionBy.whenUnsupported = "collapse"`, which folds
    /// the children into the parent and mirrors the bounds as row predicates;
    /// only the unaffirmed case, and `attachPartition`/`detachPartition` - which
    /// have no collapsed spelling because there is no second relation to move -
    /// are refused.
    PartitionRelationDdl,
}

impl Capability {
    /// Every capability, in declaration order. The index into this slice is the
    /// bit position [`CapabilitySet`] uses.
    pub const ALL: &'static [Capability] = &[
        Capability::NonPkIdentity,
        Capability::VirtualGeneratedColumn,
        Capability::CrossSchemaDdl,
        Capability::TableLevelForeignKey,
        Capability::TableLevelUnique,
        Capability::NonBtreeIndexMethod,
        Capability::PartialIndexPredicate,
        Capability::NativeAlterColumn,
        Capability::AlterTableAddConstraint,
        Capability::AlterTableDropConstraint,
        Capability::AlterTableValidateConstraint,
        Capability::InsertOnConflictClause,
        Capability::PrivilegedCatalogObjects,
        Capability::MaterializedView,
        Capability::CreateOrReplaceView,
        Capability::TriggerTruncateEvent,
        Capability::TriggerStatementForEach,
        Capability::TriggerExecuteFunction,
        Capability::TriggerBody,
        Capability::MaterializedEnumType,
        Capability::MaterializedDomainType,
        Capability::Sequence,
        Capability::ExclusionConstraint,
        Capability::CommentOn,
        Capability::SchemaWideIndexNames,
        Capability::TransactionalDdl,
        Capability::DeferrableConstraint,
        Capability::UniqueConstraintDistinctFromIndex,
        Capability::IntegerPrimaryKeyRowidAlias,
        Capability::PartitionRelationDdl,
    ];

    /// This capability's bit position in a [`CapabilitySet`].
    #[must_use]
    const fn bit(self) -> u64 {
        1u64 << (self as u32)
    }
}

/// The capabilities one backend answers YES to.
///
/// A `u64` bitset over the CLOSED [`Capability`] enum. This is a fixed-width set
/// over a fixed vocabulary and is NOT the thing the eight-backend cap lived in -
/// that was [`DialectSet`], which is now unbounded. A static assertion below
/// keeps the vocabulary inside 64 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilitySet(u64);

const _: () = assert!(
    Capability::ALL.len() <= 64,
    "CapabilitySet is a u64 bitset; the Capability vocabulary outgrew it"
);

impl CapabilitySet {
    /// The set that answers NO to everything.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// This set plus `cap`. `const`, so a backend declares its capability set as
    /// an item-scope constant.
    #[must_use]
    pub const fn with(self, cap: Capability) -> Self {
        Self(self.0 | cap.bit())
    }

    /// Whether this backend answers YES to `cap`.
    #[must_use]
    pub const fn contains(self, cap: Capability) -> bool {
        self.0 & cap.bit() != 0
    }

    /// How many capabilities the set holds.
    #[must_use]
    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Whether the backend answers NO to everything.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// A catalog's cap on identifier length.
///
/// Bytes and CHARACTERS are distinguished because the two shipping caps differ
/// in unit, not only in magnitude, and the engine already depends on that
/// distinction: PostgreSQL truncates at 63 BYTES with only a `NOTICE`, so the
/// drop-side bound is enforced there and NOT on MySQL, whose 64 is a CHARACTER
/// count. Collapsing them to one number would either refuse a MySQL name that
/// legitimately exists or under-bound a PostgreSQL one. See
/// `crates/zero-migrate/tests/authored_identifier_lengths.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifierLimit {
    /// The catalog imposes no identifier cap (`SQLite`).
    Unbounded,
    /// Capped at N BYTES (PostgreSQL: `NAMEDATALEN - 1` = 63).
    Bytes(usize),
    /// Capped at N CHARACTERS (`MySQL`: 64).
    Characters(usize),
}

/// Non-boolean backend facts core needs. Unlike a [`Capability`] these are
/// QUANTITIES, so they cannot be answered yes/no.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The catalog's identifier-length cap.
    pub identifier: IdentifierLimit,
    /// Identifier prefixes this catalog reserves for its own objects, lowercase.
    ///
    /// A name starting with one of these is refused at declaration, because the
    /// catalog either already owns it or will collide with it. Declared here rather
    /// than listed in core for the reason every other backend fact is: core held
    /// `"pg_"` and `"sqlite_"` as literals in `schema::query`, which made two
    /// backends' catalog conventions part of the neutral name validator, and left a
    /// fourth backend's reservation with nowhere to go.
    ///
    /// Core checks a declared name against the union across every REGISTERED
    /// backend, not just the selected one - the same portability argument as the
    /// generated-identifier budget. A name that is legal on today's target and
    /// reserved on another is a re-targeting hazard, and it is cheaper to refuse it
    /// at declaration than to discover it at deploy.
    ///
    /// Empty is a legitimate answer: a backend may reserve nothing by prefix.
    pub reserved_identifier_prefixes: &'static [&'static str],
}

// ---------------------------------------------------------------------------
// BackendDescriptor
// ---------------------------------------------------------------------------

/// Everything core is allowed to know about a backend.
///
/// Core matches on NONE of it. It reads [`Self::id`] as an opaque key, asks
/// [`Self::capabilities`] yes/no questions, and reads [`Self::limits`] for the
/// quantities it must respect. [`Self::display_name`] is for humans and for the
/// duplicate-registration diagnostic; it is never a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendDescriptor {
    /// The opaque identity. The registry refuses two descriptors sharing one.
    pub id: DialectId,
    /// A human-facing name. NOT an alias and NOT a key.
    pub display_name: &'static str,
    /// The yes/no answers core asks for.
    pub capabilities: CapabilitySet,
    /// The quantities core must respect.
    pub limits: Limits,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Why a set of descriptors is not a registry.
///
/// Both arms name the offending registrant(s). A registry that resolved a
/// collision silently - last-one-wins - would let two backends quietly share
/// capability rows and dialect-table entries, which is strictly worse than the
/// closed enum this replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// A descriptor's id does not satisfy the id rule.
    MalformedId {
        /// The offending id, verbatim.
        id: &'static str,
        /// The registrant that declared it.
        display_name: &'static str,
        /// Its 0-based position in the registration list.
        index: usize,
    },
    /// Two descriptors claim the same id.
    DuplicateId {
        /// The contested id.
        id: &'static str,
        /// The registrant that claimed it first.
        first_display_name: &'static str,
        /// The first claimant's 0-based position.
        first_index: usize,
        /// The registrant that claimed it again.
        second_display_name: &'static str,
        /// The second claimant's 0-based position.
        second_index: usize,
    },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedId {
                id,
                display_name,
                index,
            } => write!(
                f,
                "backend {display_name:?} (entry {index}) declares a malformed dialect id {id:?}: \
                 an id must be lowercase ASCII matching [a-z][a-z0-9_]*, with no aliases and no \
                 display names"
            ),
            Self::DuplicateId {
                id,
                first_display_name,
                first_index,
                second_display_name,
                second_index,
            } => write!(
                f,
                "duplicate dialect id {id:?}: already registered by {first_display_name:?} \
                 (entry {first_index}), re-registered by {second_display_name:?} \
                 (entry {second_index}). Backend ids are identities, not labels; one of the two \
                 must change its id rather than the registry picking a winner"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

/// The set of backends a build ships, keyed by [`DialectId`].
///
/// Built once, fallibly. There is no `insert`: a registry that could grow after
/// validation would let a duplicate in behind the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRegistry {
    entries: Vec<&'static BackendDescriptor>,
}

impl BackendRegistry {
    /// Validate a registration list into a registry.
    ///
    /// # Errors
    ///
    /// [`RegistryError::MalformedId`] if any id breaks the id rule, and
    /// [`RegistryError::DuplicateId`] - naming BOTH registrants - if two
    /// descriptors claim one id. Never last-one-wins.
    pub fn build(descriptors: &[&'static BackendDescriptor]) -> Result<Self, RegistryError> {
        for (index, descriptor) in descriptors.iter().enumerate() {
            if !descriptor.id.is_well_formed() {
                return Err(RegistryError::MalformedId {
                    id: descriptor.id.as_str(),
                    display_name: descriptor.display_name,
                    index,
                });
            }
        }
        for (index, descriptor) in descriptors.iter().enumerate() {
            if let Some((first_index, first)) = descriptors[..index]
                .iter()
                .enumerate()
                .find(|(_, earlier)| earlier.id == descriptor.id)
            {
                return Err(RegistryError::DuplicateId {
                    id: descriptor.id.as_str(),
                    first_display_name: first.display_name,
                    first_index,
                    second_display_name: descriptor.display_name,
                    second_index: index,
                });
            }
        }
        Ok(Self {
            entries: descriptors.to_vec(),
        })
    }

    /// The descriptor filed under `id`, if this build has one.
    #[must_use]
    pub fn get(&self, id: &DialectId) -> Option<&'static BackendDescriptor> {
        self.entries.iter().copied().find(|d| &d.id == id)
    }

    /// Every registered id.
    #[must_use]
    pub fn ids(&self) -> DialectSet {
        DialectSet::from_ids(self.entries.iter().map(|d| d.id.clone()))
    }

    /// How many backends are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no backend is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The registered descriptors, in registration order.
    pub fn iter(&self) -> impl Iterator<Item = &'static BackendDescriptor> + '_ {
        self.entries.iter().copied()
    }
}
