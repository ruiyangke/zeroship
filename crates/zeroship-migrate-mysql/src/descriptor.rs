use zeroship_migrate_ir::backend::{
    BackendDescriptor, Capability, CapabilitySet, IdentifierLimit, Limits,
};

use crate::DIALECT;

/// `MySQL`'s capability answers.
///
/// # The one absence worth stating out loud
///
/// [`Capability::PartitionRelationDdl`] is NOT here, and that is not the same
/// claim as "MySQL cannot partition". MySQL's `PARTITION BY RANGE/LIST/HASH/KEY`
/// is first-class and predates PostgreSQL's declarative model. What MySQL has no
/// spelling for is a partition that is a RELATION - there is no
/// `CREATE TABLE ... PARTITION OF`, no `ATTACH PARTITION`, and no `DETACH` that
/// leaves a standalone table behind, because a MySQL partition is a storage
/// division of one table and never appears in the relation namespace.
/// `EXCHANGE PARTITION` swaps rows with a structurally-identical table; it moves
/// data, not catalog identity.
///
/// The engine's partition surface is written in relations, so this backend has
/// nowhere to put one and its four `DdlEmitter` partition methods all return
/// `None`. The NO is a fact about the catalog, not a gap in this crate.
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
    id: DIALECT,
    display_name: "MySQL",
    capabilities: MYSQL_CAPABILITIES,
    limits: Limits {
        identifier: IdentifierLimit::Characters(64),
        // MySQL keeps its catalog in NAMED schemas (`mysql`, `information_schema`,
        // `performance_schema`, `sys`) rather than behind an identifier prefix, so
        // there is no prefix to reserve. Empty is the answer, not an omission.
        reserved_identifier_prefixes: &[],
    },
};
