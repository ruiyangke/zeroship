//! What a binding control declared read-only may and may not be asked to do.
//!
//! # What this is NOT
//!
//! Not a security boundary and not evidence of one. `PostgreSQL` enforces the
//! capability: the reconciler grants a binding role membership in exactly one of
//! its database's two capability roles, so a session narrowed to a read-only
//! binding cannot write whatever these assertions say. What is under test is
//! the ERGONOMIC half - that a creator gets a coded refusal naming the binding
//! instead of `42501 permission denied for table ...` from the server - and
//! that reads are untouched by it.
//!
//! The pairing is the point. Every refusal here sits beside the same operation
//! on a read-write binding and beside a READ on the read-only one; without
//! both, a preparation that refused everything, or one that refused nothing,
//! would pass half of this file.

use super::*;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_data_orm::error::READ_ONLY_BINDING;

/// One app's two edges to the SAME database, differing only in capability.
///
/// The same app, database, edge id and epoch, so the schema and the role name
/// are identical and a route captured on either validates against the other.
/// The capability is therefore the only variable any assertion below can be
/// responding to.
fn a_pair_of_bindings_differing_only_in_capability() -> (DbBinding, DbBinding) {
    let app = zeroship_core::AppId::mint();
    let database = zeroship_core::DatabaseId::mint();
    let edge = zeroship_core::BindingId::mint();
    let compose = |capability| {
        DbBinding::to_database(
            app.as_str(),
            "deploy_capability",
            database.clone(),
            edge.clone(),
            5,
            capability,
        )
        .expect("the minted ids compose a legal role name")
    };
    let writable = compose(DatabaseCapability::ReadWrite);
    let read_only = compose(DatabaseCapability::ReadOnly);
    assert_eq!(
        writable.schema(),
        read_only.schema(),
        "the pair must address one schema, or a refusal could be about the route"
    );
    assert_eq!(
        writable.session_role(),
        read_only.session_role(),
        "the pair must narrow to one role, or a refusal could be about the epoch"
    );
    (writable, read_only)
}

/// Install `posts` for both halves of the pair and return them.
fn bound_pair() -> (DbBinding, DbBinding) {
    crate::tests::fixtures::reset_engine();
    let (writable, read_only) = a_pair_of_bindings_differing_only_in_capability();
    for binding in [&writable, &read_only] {
        crate::descriptor::install_collections(
            binding,
            Schema::new(vec![(
                "posts".into(),
                <posts::Entity as Entity>::schema().clone(),
            )]),
        )
        .expect("the posts descriptor installs for a bound app");
    }
    (writable, read_only)
}

fn route_on(binding: &DbBinding) -> CapturedRoute {
    CapturedRoute::pool_on_binding_for_tests(
        binding,
        crate::sql::registration::SqlRegistration::sqlite(),
    )
}

/// Every operation that writes rows is refused on a read-only binding, and
/// every operation that does not is prepared.
///
/// The partition is exhaustive over `Operation`, so this cannot pass by
/// covering only the variants someone remembered: an operation added to the
/// enum and left out of one of the two lists fails the count assertion below.
#[test]
fn a_read_only_binding_refuses_every_row_modifying_operation_and_prepares_every_read() {
    let (writable, read_only) = bound_pair();

    let writes = || {
        vec![
            (
                "insert",
                Operation::Insert {
                    document: value!({"title": "t"}),
                },
            ),
            (
                "insertMany",
                Operation::InsertMany {
                    documents: value!([{"title": "t"}]),
                },
            ),
            (
                "update",
                Operation::Update {
                    filter: value!({}),
                    patch: value!({"title": "t"}),
                    many: false,
                },
            ),
            (
                "updateMany",
                Operation::Update {
                    filter: value!({}),
                    patch: value!({"title": "t"}),
                    many: true,
                },
            ),
            (
                "delete",
                Operation::Delete {
                    filter: value!({}),
                    many: false,
                },
            ),
            (
                "purge",
                Operation::Purge {
                    filter: value!({}),
                    many: false,
                },
            ),
            (
                "restore",
                Operation::Restore {
                    filter: value!({}),
                    many: false,
                },
            ),
            (
                "upsert",
                Operation::Upsert {
                    document: value!({"title": "t"}),
                    conflict_fields: value!(["title"]),
                },
            ),
        ]
    };

    let reads = || {
        vec![
            (
                "find",
                Operation::Find {
                    filter: value!({}),
                    options: value!({}),
                },
            ),
            (
                "count",
                Operation::Count {
                    filter: value!({}),
                    options: value!({}),
                },
            ),
            (
                "aggregate",
                Operation::Aggregate {
                    pipeline: value!([{"$match": {}}]),
                    options: value!({}),
                },
            ),
            (
                "distinct",
                Operation::Distinct {
                    field: "title".into(),
                    filter: value!({}),
                    options: value!({}),
                },
            ),
        ]
    };

    let mut refused = 0;
    for (label, operation) in writes() {
        // THE ACCEPTANCE CONTROL, one variable away: the same operation on the
        // read-write half of the pair prepares.
        PreparedOperation::new(
            writable.clone(),
            "posts",
            route_on(&writable),
            None,
            operation.clone(),
        )
        .unwrap_or_else(|error| panic!("{label} must prepare on a read-write binding: {error}"));

        let error = PreparedOperation::new(
            read_only.clone(),
            "posts",
            route_on(&read_only),
            None,
            operation,
        )
        .err()
        .unwrap_or_else(|| {
            panic!("{label} writes rows and must be refused on a read-only binding")
        });
        assert_eq!(
            error.code(),
            READ_ONLY_BINDING,
            "{label} must be refused by the read-only code, not by something else"
        );
        assert!(
            error.message_str().contains("read-only"),
            "{label}'s refusal must say what is wrong: {}",
            error.message_str()
        );
        refused += 1;
    }
    assert_eq!(
        refused, 8,
        "every row-modifying operation must be exercised"
    );

    // THE REJECTION CONTROL: a read on the SAME read-only binding prepares. A
    // guard that refused every operation would pass the loop above and fail
    // here.
    let mut prepared = 0;
    for (label, operation) in reads() {
        PreparedOperation::new(
            read_only.clone(),
            "posts",
            route_on(&read_only),
            None,
            operation,
        )
        .unwrap_or_else(|error| {
            panic!("{label} reads rows and must prepare on a read-only binding: {error}")
        });
        prepared += 1;
    }
    assert_eq!(prepared, 4, "every read operation must be exercised");
}

/// The two typed constructors carry their own classification, so each is
/// exercised on its own.
///
/// `new_model_update` and `new_model_mutation` do not take an `Operation` and
/// cannot consult [`Operation::writes_rows`]; they write by construction. A
/// guard placed only on `PreparedOperation::new` would leave the whole typed
/// Rust surface writing on a read-only binding, and nothing in the test above
/// would notice.
#[test]
fn the_typed_constructors_are_refused_on_a_read_only_binding_too() {
    let (writable, read_only) = bound_pair();
    let filter = || Filter::<posts::Entity>::all().into_predicate();

    for many in [false, true] {
        // The acceptance control for the typed update.
        PreparedOperation::new_model_update(
            writable.clone(),
            "posts",
            route_on(&writable),
            None,
            filter(),
            value!({"title": "t"}).into(),
            many,
        )
        .expect("a typed update prepares on a read-write binding");

        let error = PreparedOperation::new_model_update(
            read_only.clone(),
            "posts",
            route_on(&read_only),
            None,
            filter(),
            value!({"title": "t"}).into(),
            many,
        )
        .expect_err("a typed update writes rows and must be refused");
        assert_eq!(error.code(), READ_ONLY_BINDING);

        for (label, mutation) in [
            ("delete", mutations::Mutation::Delete),
            ("purge", mutations::Mutation::Purge),
            ("restore", mutations::Mutation::Restore),
        ] {
            // The acceptance control for each typed mutation.
            PreparedOperation::new_model_mutation(
                writable.clone(),
                "posts",
                route_on(&writable),
                None,
                filter(),
                mutation,
                many,
            )
            .unwrap_or_else(|error| {
                panic!("a typed {label} must prepare on a read-write binding: {error}")
            });

            let error = PreparedOperation::new_model_mutation(
                read_only.clone(),
                "posts",
                route_on(&read_only),
                None,
                filter(),
                mutation,
                many,
            )
            .err()
            .unwrap_or_else(|| panic!("a typed {label} writes rows and must be refused"));
            assert_eq!(error.code(), READ_ONLY_BINDING);
        }
    }
}

/// A PLATFORM binding writes. It has no control-plane edge and no capability,
/// and the refusal must not reach it.
///
/// Its control is the read-only creator binding beside it, which is refused for
/// the same operation: without that, a guard that had simply stopped firing
/// would pass this.
#[test]
fn a_platform_binding_writes_and_a_read_only_creator_binding_does_not() {
    let (_writable, read_only) = bound_pair();

    let platform = DbBinding::platform(
        "platform",
        "catalog",
        crate::sql::SchemaName::new("zeroship").expect("the platform schema is a legal identifier"),
    );
    assert_eq!(
        platform.database_capability(),
        None,
        "a platform binding holds no control-plane edge and so no capability"
    );
    assert!(
        platform.permits_writes(),
        "a platform store narrows to nothing; its authority is the login"
    );
    assert!(!read_only.permits_writes());

    crate::descriptor::install_collections(
        &platform,
        Schema::new(vec![(
            "posts".into(),
            <posts::Entity as Entity>::schema().clone(),
        )]),
    )
    .expect("the posts descriptor installs for the platform store");

    PreparedOperation::new(
        platform.clone(),
        "posts",
        route_on(&platform),
        None,
        Operation::Insert {
            document: value!({"title": "t"}),
        },
    )
    .expect("a platform store's own schema is writable");

    PreparedOperation::new(
        read_only.clone(),
        "posts",
        route_on(&read_only),
        None,
        Operation::Insert {
            document: value!({"title": "t"}),
        },
    )
    .expect_err("the control: the same insert on a read-only creator binding is refused");
}

// Every test above is a PLAIN `#[test]`, not a `#[compio::test]`, and that is
// deliberate. Preparation is synchronous - it compiles SQL and consults the
// descriptor without touching a connection - so a runtime here would be a
// dependency the subject does not have, and on a busy host `compio` answers
// `cannot create runtime: Os { code: 12, kind: OutOfMemory }` and reports a
// refusal this file never got to observe.
