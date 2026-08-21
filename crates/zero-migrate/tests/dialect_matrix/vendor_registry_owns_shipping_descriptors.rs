//! The shipping vendor registry is the only authority for shipping descriptors.
//!
//! Capability rows used to be copied into the leaf IR crate, reached through both
//! `SqlDialect::descriptor()` and a second `SHIPPING_DESCRIPTORS` list. A fourth
//! backend could not extend that closed bridge. The rows now live in their vendor
//! crates and this integration test checks the composition that can see all three.

use zero_migrate::{shipping_backends, Capability, MYSQL, POSTGRES, SQLITE};

const REGISTERED_VENDOR_FLOOR: usize = 3;
const EXPECTED_VENDOR_IDS: &[&str] = &["postgres", "sqlite", "mysql"];

#[test]
fn the_shipping_registry_is_the_only_descriptor_list() {
    let registry = shipping_backends();
    assert!(
        registry.len() >= REGISTERED_VENDOR_FLOOR,
        "the vendor registry holds {} backend(s), expected at least {REGISTERED_VENDOR_FLOOR}",
        registry.len()
    );
    let found: Vec<&str> = registry.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(found, EXPECTED_VENDOR_IDS);

    for descriptor in registry.iter() {
        assert!(descriptor.id.is_well_formed());
        assert_eq!(registry.get(&descriptor.id), Some(descriptor));
    }

    assert!(std::ptr::eq(
        registry.get(&POSTGRES).expect("PostgreSQL is registered"),
        zero_migrate_postgres::VENDOR.descriptor,
    ));
    assert!(std::ptr::eq(
        registry.get(&SQLITE).expect("SQLite is registered"),
        zero_migrate_sqlite::VENDOR.descriptor,
    ));
    assert!(std::ptr::eq(
        registry.get(&MYSQL).expect("MySQL is registered"),
        zero_migrate_mysql::VENDOR.descriptor,
    ));
}

#[test]
fn every_shipping_capability_answer_is_pinned() {
    let registry = shipping_backends();
    let postgres = registry.get(&POSTGRES).expect("PostgreSQL is registered");
    let sqlite = registry.get(&SQLITE).expect("SQLite is registered");
    let mysql = registry.get(&MYSQL).expect("MySQL is registered");

    let expected = [
        (Capability::NonPkIdentity, [true, false, false]),
        (Capability::VirtualGeneratedColumn, [false, true, true]),
        (Capability::CrossSchemaDdl, [true, false, true]),
        (Capability::TableLevelForeignKey, [true, true, true]),
        (Capability::TableLevelUnique, [true, false, true]),
        (Capability::NonBtreeIndexMethod, [true, false, false]),
        (Capability::PartialIndexPredicate, [true, true, false]),
        (Capability::NativeAlterColumn, [true, false, true]),
        (Capability::AlterTableAddConstraint, [true, false, true]),
        (Capability::AlterTableDropConstraint, [true, false, true]),
        (
            Capability::AlterTableValidateConstraint,
            [true, false, false],
        ),
        (Capability::InsertOnConflictClause, [true, true, true]),
        (Capability::PostgresVendorPrimitives, [true, false, false]),
        (Capability::MaterializedView, [true, false, false]),
        (Capability::CreateOrReplaceView, [true, false, true]),
        (Capability::TriggerTruncateEvent, [true, false, false]),
        (Capability::TriggerStatementForEach, [true, false, false]),
        (Capability::TriggerExecuteFunction, [true, false, false]),
        (Capability::TriggerBody, [false, true, true]),
        (Capability::MaterializedEnumType, [true, false, false]),
        (Capability::MaterializedDomainType, [true, false, false]),
        (Capability::Sequence, [true, false, false]),
        (Capability::ExclusionConstraint, [true, false, false]),
        (Capability::CommentOn, [true, false, false]),
        (Capability::SchemaWideIndexNames, [true, true, false]),
        (Capability::TransactionalDdl, [true, true, false]),
        (Capability::DeferrableConstraint, [true, true, false]),
        (
            Capability::UniqueConstraintDistinctFromIndex,
            [true, true, false],
        ),
        (
            Capability::IntegerPrimaryKeyRowidAlias,
            [false, true, false],
        ),
    ];

    assert_eq!(expected.len(), Capability::ALL.len());
    for (capability, answers) in expected {
        assert_eq!(
            [
                postgres.capabilities.contains(capability),
                sqlite.capabilities.contains(capability),
                mysql.capabilities.contains(capability),
            ],
            answers,
            "unexpected {capability:?} capability row"
        );
    }
}
