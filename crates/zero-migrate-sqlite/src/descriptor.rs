use zero_migrate_ir::backend::{
    BackendDescriptor, Capability, CapabilitySet, IdentifierLimit, Limits,
};
use zero_migrate_ir::dialect::SQLITE;

/// `SQLite`'s capability answers.
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
    },
};
