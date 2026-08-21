use zero_migrate_ir::backend::{
    BackendDescriptor, Capability, CapabilitySet, IdentifierLimit, Limits,
};
use zero_migrate_ir::dialect::POSTGRES;

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
    .with(Capability::SchemaWideIndexNames)
    .with(Capability::TransactionalDdl)
    .with(Capability::DeferrableConstraint)
    .with(Capability::UniqueConstraintDistinctFromIndex);

/// The PostgreSQL backend descriptor.
pub static POSTGRES_DESCRIPTOR: BackendDescriptor = BackendDescriptor {
    id: POSTGRES,
    display_name: "PostgreSQL",
    capabilities: POSTGRES_CAPABILITIES,
    limits: Limits {
        // `NAMEDATALEN - 1`. Anything longer is truncated with only a NOTICE.
        identifier: IdentifierLimit::Bytes(63),
    },
};
