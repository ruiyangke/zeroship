use zero_migrate_ir::backend::{
    BackendDescriptor, Capability, CapabilitySet, IdentifierLimit, Limits,
};
use zero_migrate_ir::dialect::SQLITE;

/// `SQLite`'s capability answers.
///
/// [`Capability::PartitionRelationDdl`] is absent because SQLite has no
/// partitioning of any kind — no relation-valued partitions and no storage
/// divisions either. All four `DdlEmitter` partition methods return `None`.
pub const SQLITE_CAPABILITIES: CapabilitySet = CapabilitySet::empty()
    .with(Capability::VirtualGeneratedColumn)
    .with(Capability::TableLevelForeignKey)
    .with(Capability::PartialIndexPredicate)
    .with(Capability::InsertOnConflictClause)
    .with(Capability::TriggerBody)
    .with(Capability::SchemaWideIndexNames)
    .with(Capability::TransactionalDdl)
    .with(Capability::DeferrableConstraint)
    .with(Capability::UniqueConstraintDistinctFromIndex)
    .with(Capability::IntegerPrimaryKeyRowidAlias);

/// The `SQLite` backend descriptor.
pub static SQLITE_DESCRIPTOR: BackendDescriptor = BackendDescriptor {
    id: SQLITE,
    display_name: "SQLite",
    capabilities: SQLITE_CAPABILITIES,
    limits: Limits {
        identifier: IdentifierLimit::Unbounded,
        // The internal schema namespace: `sqlite_master`, `sqlite_sequence`,
        // `sqlite_autoindex_*`. `CREATE TABLE sqlite_x` is refused by the server
        // outright ("object name reserved for internal use").
        reserved_identifier_prefixes: &["sqlite_"],
    },
};
