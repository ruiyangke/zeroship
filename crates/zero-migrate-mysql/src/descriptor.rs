use zero_migrate_ir::backend::{
    BackendDescriptor, Capability, CapabilitySet, IdentifierLimit, Limits,
};
use zero_migrate_ir::dialect::MYSQL;

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

/// The `MySQL` backend descriptor.
pub static MYSQL_DESCRIPTOR: BackendDescriptor = BackendDescriptor {
    id: MYSQL,
    display_name: "MySQL",
    capabilities: MYSQL_CAPABILITIES,
    limits: Limits {
        identifier: IdentifierLimit::Characters(64),
    },
};
