//! The shipping vendor registry is the only authority for shipping descriptors.
//!
//! Capability rows used to be copied into the leaf IR crate, reached through both
//! the closed dialect enum's `descriptor()` method and a second `SHIPPING_DESCRIPTORS`
//! list. A fourth backend could not extend that closed bridge. The rows now live in their vendor
//! crates and this integration test checks the composition that can see all three.

use zero_migrate::{shipping_backends, Capability};
use zero_migrate_backend::registry::BackendVendor;
use zero_migrate_ir::ir::PartitionBounds;
use zero_migrate_mysql::DIALECT as MYSQL;
use zero_migrate_postgres::DIALECT as POSTGRES;
use zero_migrate_sqlite::DIALECT as SQLITE;

const REGISTERED_VENDOR_FLOOR: usize = 3;
const EXPECTED_VENDOR_IDS: &[&str] = &["postgres", "sqlite", "mysql"];

/// The vendors this file reaches through their OWN statics, which is the one
/// place naming a vendor crate is the design rather than a leak: this test is
/// the composition that can see all three.
fn shipping_vendors() -> [&'static BackendVendor; 3] {
    [
        &zero_migrate_postgres::VENDOR,
        &zero_migrate_sqlite::VENDOR,
        &zero_migrate_mysql::VENDOR,
    ]
}

/// The NEEDLE-LIVENESS floors for [`a_backends_two_partition_answers_cannot_disagree`],
/// and they are two because there are two ways for that census to hold while
/// measuring nothing.
///
/// If EVERY registered backend emitted partition-relation DDL, the refusing arm —
/// the entire reason the emitter methods return `Option` — would never execute. If
/// NONE did, the emitting arm would never execute and the census would be a
/// four-way `assert!(none.is_none())` that no capability answer could ever break.
///
/// Measured on the tree that introduced `Capability::PartitionRelationDdl`:
/// PostgreSQL emits, SQLite and MySQL refuse.
const VENDORS_WITH_PARTITION_RELATIONS_FLOOR: usize = 1;
const VENDORS_WITHOUT_PARTITION_RELATIONS_FLOOR: usize = 2;

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
        (Capability::PrivilegedCatalogObjects, [true, false, false]),
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
        (Capability::PartitionRelationDdl, [true, false, false]),
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

/// A backend's DECLARED partition posture and its EMITTED one cannot disagree.
///
/// # The failure this closes, which the type system cannot
///
/// `Capability::PartitionRelationDdl` and the four `DdlEmitter` partition methods
/// are the same fact asked at two altitudes. The emitter half is compiler-forced —
/// all four methods are required with no default, so a fourth backend cannot
/// inherit another vendor's partition grammar by omission. The capability half is
/// not, and cannot be: a `CapabilitySet` is opt-in by construction, so silence
/// there means NO for every one of its members.
///
/// That asymmetry makes exactly one direction dangerous, and it is not the one
/// silence protects. A backend that emits partition DDL but forgets the capability
/// merely gets its partitions collapsed — wrong, but quiet and recoverable. A
/// backend that CLAIMS the capability while an emitter returns `None` gets
/// `render::declarative`'s `.expect("selected backend supports partition-relation
/// DDL")`, which is a PANIC in the middle of rendering a migration, from a backend
/// whose only mistake was one line in a descriptor. Nothing else in the tree holds
/// those two answers together, so this does.
///
/// # Why this replaced a required trait method rather than joining it
///
/// A native-partitioning predicate on ValidationPolicy used to be a third spelling of
/// this same fact, consulted only by the authoring validator while `render/lower.rs`
/// asked the identical question as `self.dialect != POSTGRES`. Two spellings of one
/// fact drift; three is a promise to. The capability is now the single spelling —
/// the vocabulary BOTH layers already share — and this census anchors it to the
/// compiler-forced emitter answers, which is strictly more than the trait method
/// checked: it forced an ANSWER, never that the answer was TRUE.
#[test]
fn a_backends_two_partition_answers_cannot_disagree() {
    // Values only need to be well-formed; whether a backend spells them is the
    // backend's business, that it is ASKED is this test's.
    let bounds = PartitionBounds::Hash {
        modulus: 4,
        remainder: 0,
    };

    let mut emitting = 0usize;
    let mut refusing = 0usize;

    for vendor in shipping_vendors() {
        let id = &vendor.descriptor.id;
        let declared = vendor
            .descriptor
            .capabilities
            .contains(Capability::PartitionRelationDdl);
        let ddl = (vendor.ddl)("public");

        // All four, not a representative one: the emitter contract requires the
        // COMPLETE boundary, so a backend that spelled three of four would leave
        // the fourth's `.expect` live behind a capability that reads as satisfied.
        let emitted = [
            ddl.create_partition("child", "parent", &bounds).is_some(),
            ddl.attach_partition("parent", "child", &bounds).is_some(),
            ddl.detach_partition("parent", "child", false).is_some(),
            ddl.drop_partition("child", false).is_some(),
        ];

        for (method, spelled) in [
            "create_partition",
            "attach_partition",
            "detach_partition",
            "drop_partition",
        ]
        .into_iter()
        .zip(emitted)
        {
            assert_eq!(
                spelled,
                declared,
                "{id:?} declares PartitionRelationDdl = {declared} but its \
                 DdlEmitter::{method} {} — a backend that claims the capability and \
                 refuses an emitter panics in render::declarative, and one that emits \
                 without claiming it gets its partitions silently collapsed",
                if spelled { "emits" } else { "refuses" },
            );
        }

        if declared {
            emitting += 1;
        } else {
            refusing += 1;
        }
    }

    assert!(
        emitting >= VENDORS_WITH_PARTITION_RELATIONS_FLOOR,
        "{emitting} backend(s) emit partition-relation DDL, expected at least \
         {VENDORS_WITH_PARTITION_RELATIONS_FLOOR} — with none, this census is a \
         four-way `is_none()` no capability answer could break"
    );
    assert!(
        refusing >= VENDORS_WITHOUT_PARTITION_RELATIONS_FLOOR,
        "{refusing} backend(s) refuse partition-relation DDL, expected at least \
         {VENDORS_WITHOUT_PARTITION_RELATIONS_FLOOR} — with none, the refusing arm \
         that is the whole reason these methods return `Option` never executes"
    );
}
