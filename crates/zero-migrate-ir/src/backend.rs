//! The BACKEND CONTRACT: what core asks a backend, and how a backend is named.
//!
//! Core never asks "which dialect is this". It asks "can you do this". The
//! vocabulary for that question is [`Capability`]; the answer is a
//! [`BackendDescriptor`], which also carries the backend's [`DialectId`], its
//! human-facing name, and its [`Limits`].
//!
//! # Why this lives in the leaf crate
//!
//! `docs/proposals/pluggable-backends.md` places this contract in a future
//! `zero-migrate-backend` crate. Until the backend crates are extracted (steps 3
//! and 4 of that proposal) a separate contract crate would have exactly one
//! consumer and one implementor, so the contract lives here instead: this is
//! already the bottom of the crate graph, already the crate the engine, the
//! guard, and the N-API addon all name, and moving these items later is a
//! `pub use` away.
//!
//! # What is NOT here
//!
//! The `Backend` trait itself (`introspect` / `render` / `execute`). Those
//! signatures name `SchemaModel`, `ChangeSet` and `ExecutionPlan`, which are
//! engine types; naming them here would invert the crate graph. Step 2 promotes
//! IDENTITY and CAPABILITY to public vocabulary and nothing else.

use core::fmt;

use crate::dialect::{DialectId, DialectSet, SqlDialect, MYSQL, POSTGRES, SQLITE};

// ---------------------------------------------------------------------------
// Capability
// ---------------------------------------------------------------------------

/// A question CORE ASKS a backend. Never a vendor name.
///
/// Promoted from `zero_migrate::render::renderer` (where it was `pub(crate)`)
/// unchanged in spirit and unchanged in membership: the same 25 predicates, the
/// same spellings, the same meanings.
///
/// Keep this enum CLOSED. Adding a capability is a core change and should be
/// rare; adding a BACKEND is not a core change at all. A backend that needs a
/// predicate nobody else has does not add one here — it keeps that fact private
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
    /// The PostgreSQL-only vendor object family (extensions, roles, policies, …).
    PostgresVendorPrimitives,
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
        Capability::PostgresVendorPrimitives,
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
/// over a fixed vocabulary and is NOT the thing the eight-backend cap lived in —
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// PostgreSQL's capability answers.
pub const POSTGRES_CAPABILITIES: CapabilitySet = CapabilitySet::empty()
    .with(Capability::NonPkIdentity)
    .with(Capability::CrossSchemaDdl)
    .with(Capability::TableLevelForeignKey)
    .with(Capability::TableLevelUnique)
    .with(Capability::NonBtreeIndexMethod)
    .with(Capability::PartialIndexPredicate)
    .with(Capability::NativeAlterColumn)
    .with(Capability::AlterTableAddConstraint)
    .with(Capability::AlterTableDropConstraint)
    .with(Capability::AlterTableValidateConstraint)
    .with(Capability::InsertOnConflictClause)
    .with(Capability::PostgresVendorPrimitives)
    .with(Capability::MaterializedView)
    .with(Capability::CreateOrReplaceView)
    .with(Capability::TriggerTruncateEvent)
    .with(Capability::TriggerStatementForEach)
    .with(Capability::TriggerExecuteFunction)
    .with(Capability::MaterializedEnumType)
    .with(Capability::MaterializedDomainType)
    .with(Capability::Sequence)
    .with(Capability::ExclusionConstraint)
    .with(Capability::CommentOn)
    .with(Capability::SchemaWideIndexNames);

/// `SQLite`'s capability answers.
pub const SQLITE_CAPABILITIES: CapabilitySet = CapabilitySet::empty()
    .with(Capability::VirtualGeneratedColumn)
    .with(Capability::TableLevelForeignKey)
    .with(Capability::PartialIndexPredicate)
    .with(Capability::InsertOnConflictClause)
    .with(Capability::TriggerBody)
    .with(Capability::SchemaWideIndexNames);

/// `MySQL`'s capability answers.
pub const MYSQL_CAPABILITIES: CapabilitySet = CapabilitySet::empty()
    .with(Capability::VirtualGeneratedColumn)
    .with(Capability::CrossSchemaDdl)
    .with(Capability::TableLevelForeignKey)
    .with(Capability::TableLevelUnique)
    .with(Capability::NativeAlterColumn)
    .with(Capability::AlterTableAddConstraint)
    .with(Capability::AlterTableDropConstraint)
    .with(Capability::InsertOnConflictClause)
    .with(Capability::CreateOrReplaceView)
    .with(Capability::TriggerBody);

/// The PostgreSQL backend descriptor.
pub const POSTGRES_DESCRIPTOR: BackendDescriptor = BackendDescriptor {
    id: POSTGRES,
    display_name: "PostgreSQL",
    capabilities: POSTGRES_CAPABILITIES,
    limits: Limits {
        // `NAMEDATALEN - 1`. Anything longer is truncated with only a NOTICE.
        identifier: IdentifierLimit::Bytes(63),
    },
};

/// The `SQLite` backend descriptor.
pub const SQLITE_DESCRIPTOR: BackendDescriptor = BackendDescriptor {
    id: SQLITE,
    display_name: "SQLite",
    capabilities: SQLITE_CAPABILITIES,
    limits: Limits {
        identifier: IdentifierLimit::Unbounded,
    },
};

/// The `MySQL` backend descriptor.
pub const MYSQL_DESCRIPTOR: BackendDescriptor = BackendDescriptor {
    id: MYSQL,
    display_name: "MySQL",
    capabilities: MYSQL_CAPABILITIES,
    limits: Limits {
        identifier: IdentifierLimit::Characters(64),
    },
};

/// Every backend this build ships, in registration order.
pub const SHIPPING_DESCRIPTORS: &[&BackendDescriptor] =
    &[&POSTGRES_DESCRIPTOR, &SQLITE_DESCRIPTOR, &MYSQL_DESCRIPTOR];

impl SqlDialect {
    /// The descriptor for this closed variant.
    ///
    /// The bridge that lets engine code still holding a `SqlDialect` ask a
    /// capability QUESTION instead of matching on a vendor. Once the backend
    /// crates exist this direction disappears with the enum.
    #[must_use]
    pub const fn descriptor(self) -> &'static BackendDescriptor {
        match self {
            Self::Postgres => &POSTGRES_DESCRIPTOR,
            Self::Sqlite => &SQLITE_DESCRIPTOR,
            Self::Mysql => &MYSQL_DESCRIPTOR,
        }
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Why a set of descriptors is not a registry.
///
/// Both arms name the offending registrant(s). A registry that resolved a
/// collision silently — last-one-wins — would let two backends quietly share
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
    /// [`RegistryError::DuplicateId`] — naming BOTH registrants — if two
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

    /// The registry of every backend this build ships.
    ///
    /// # Panics
    ///
    /// Never in a shipped build: the shipping descriptors are constants and the
    /// `shipping_registry_builds` test proves they satisfy the rule. The
    /// `expect` is here so a future backend added with a bad or colliding id
    /// fails loudly at first use rather than being silently dropped.
    #[must_use]
    pub fn shipping() -> Self {
        Self::build(SHIPPING_DESCRIPTORS).expect("the shipping descriptors satisfy the id rule")
    }

    /// The descriptor filed under `id`, if this build has one.
    #[must_use]
    pub fn get(&self, id: DialectId) -> Option<&'static BackendDescriptor> {
        self.entries.iter().copied().find(|d| d.id == id)
    }

    /// Every registered id.
    #[must_use]
    pub fn ids(&self) -> DialectSet {
        DialectSet::from_ids(self.entries.iter().map(|d| d.id))
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
