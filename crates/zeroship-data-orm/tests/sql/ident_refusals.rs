//! An identifier cannot be built from arbitrary text.
//!
//! Every arm here declares the number of vectors IT RULED ON and a floor that
//! number must clear, following the repository's gate discipline: a check that
//! examines nothing and a clean tree print the same thing.
//!
//! The compile-time half of this property is not here. It cannot be - a test
//! that fails to compile does not run - and it lives as `compile_fail`
//! doctests on the [`zeroship_data_orm::sql::ident`] module, which `cargo test`
//! executes as part of the doc-test target. Those cover the three ways a
//! validated newtype is usually laundered: the struct literal, the private
//! field, and a `From<String>`.

use zeroship_data_orm::sql::{Ident, IdentError, IdentRole};

const ALL_ROLES: [IdentRole; 7] = [
    IdentRole::Namespace,
    IdentRole::Collection,
    IdentRole::Column,
    IdentRole::StoredColumn,
    IdentRole::Alias,
    IdentRole::Constraint,
    IdentRole::Index,
];

/// `ALL_ROLES` fell a variant behind the enum and nothing said so.
///
/// `StoredColumn` was added to LOOSEN a fence and was the one role this sweep
/// never ran against, because the array's length is its own declaration and a
/// `[IdentRole; 6]` is as valid a Rust type after a seventh variant lands as
/// before. Length cannot notice a missing element.
///
/// A match can. Listing every variant makes a new one a non-exhaustive-pattern
/// COMPILE error here, so the next role cannot be added without this array being
/// looked at. It costs nothing at runtime and it cannot go vacuous, which the
/// `assert_eq!` on a literal could and did.
fn _all_roles_covers_the_enum(role: IdentRole) {
    match role {
        IdentRole::Namespace
        | IdentRole::Collection
        | IdentRole::Column
        | IdentRole::StoredColumn
        | IdentRole::Alias
        | IdentRole::Constraint
        | IdentRole::Index => {}
    }
}

/// The shape rules apply to every role, so injection has no role-shaped hole.
#[test]
fn no_role_accepts_text_that_is_not_an_identifier() {
    let vectors = [
        "users\"; DROP TABLE users; --",
        "users'; DELETE FROM users; --",
        "users; SELECT 1",
        "user name",
        "user-name",
        "user.name",
        "user(name)",
        "user\nname",
        "user\tname",
        "user\0name",
        "users/*x*/",
        "caf\u{e9}",
        "\u{5b57}\u{6bb5}",
        "",
        "1 OR 1=1",
        "*",
    ];
    let mut ruled_on = 0_usize;
    for role in ALL_ROLES {
        for vector in vectors {
            let outcome = Ident::parse_as(vector, role);
            assert!(
                outcome.is_err(),
                "role {role} accepted {vector:?}, which is not an identifier"
            );
            ruled_on += 1;
        }
    }
    assert_eq!(
        ruled_on,
        ALL_ROLES.len() * vectors.len(),
        "the loop did not rule on every (role, vector) pair"
    );
    assert!(
        ruled_on >= 90,
        "ruled on {ruled_on} pairs, too few to mean anything"
    );
    println!("ruled on {ruled_on} (role, vector) pairs");
}

/// A refusal that refused everything would pass the arm above and be useless.
/// This is its control: ordinary names must be accepted, in the roles where
/// they are ordinary.
#[test]
fn ordinary_names_are_accepted() {
    let vectors = ["users", "user_profiles", "a", "created_at", "x1", "A_B_9"];
    let mut ruled_on = 0_usize;
    for role in ALL_ROLES {
        for vector in vectors {
            assert!(
                Ident::parse_as(vector, role).is_ok(),
                "role {role} refused the ordinary name {vector:?}"
            );
            ruled_on += 1;
        }
    }
    assert_eq!(ruled_on, ALL_ROLES.len() * vectors.len());
    assert!(ruled_on >= 30, "ruled on {ruled_on} pairs");
    println!("ruled on {ruled_on} (role, vector) pairs");
}

/// The 63-byte boundary is inclusive. Postgres truncates rather than erroring,
/// so an over-long name is how two distinct fields alias to one column.
#[test]
fn the_length_fence_is_inclusive_at_63() {
    let at_limit = "a".repeat(63);
    let over = "a".repeat(64);
    assert!(Ident::parse_as(&at_limit, IdentRole::Column).is_ok());
    assert_eq!(
        Ident::parse_as(&over, IdentRole::Column),
        Err(IdentError::TooLong {
            role: IdentRole::Column,
            len: 64
        })
    );
    println!("ruled on 2 lengths");
}

/// TABLE half of the pair. The shared platform prefixes and the runtime copy of
/// each shipping backend's catalog prefix are both checked before rendering.
#[test]
fn the_table_fence_holds() {
    let refused = [
        "pg_class",
        "PG_CLASS",
        "pg_",
        "__zeroship_migrations",
        "__ZEROSHIP_x",
        "sqlite_master",
        "sqlite_sequence",
    ];
    let mut ruled_on = 0_usize;
    for name in refused {
        let outcome = Ident::parse_as(name, IdentRole::Collection);
        assert!(
            matches!(outcome, Err(IdentError::Reserved { .. })),
            "the table fence let {name:?} through: {outcome:?}"
        );
        ruled_on += 1;
    }
    // The control: a name that merely resembles a reserved one must pass, or
    // the fence is a blanket refusal wearing a table's clothes.
    //
    // The two `__zero_migrate` witnesses are ACCEPTED on purpose, and are here
    // rather than absent so this test rules in both directions. That prefix
    // fences an empty namespace: the engine's journal tables are
    // `__zeroship_schema_*`, and the one live object carrying the token is the
    // rebuild table, named `{table}__zero_migrate_rebuild` - a SUFFIX, which a
    // prefix list cannot cover.
    for name in [
        "page_views",
        "zeroship_apps",
        "__zs_internal",
        "sqlited",
        "pgx",
        "__zero_migrate_journal",
        "__ZERO_MIGRATE_x",
    ] {
        assert!(
            Ident::parse_as(name, IdentRole::Collection).is_ok(),
            "the table fence over-matched {name:?}"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 14);
    assert!(ruled_on >= 10, "ruled on {ruled_on} names");
    println!("ruled on {ruled_on} table names");
}

/// COLUMN half of the pair - the half a bulk move drops, leaving the survivor
/// to make the namespace look defended. Mirrors `RESERVED_NAMES`
/// (`query.rs:738-766`).
#[test]
fn the_column_fence_holds() {
    let refused = [
        "_distance",
        "_",
        "__zs_x",
        "__zeroship_migrations",
        "pg_attribute",
        "sqlite_master",
        "ssn_masked",
        "email_masked",
        "public",
        "pii",
        "spi",
        "phi",
        "pci",
        "internal",
    ];
    let mut ruled_on = 0_usize;
    for name in refused {
        let outcome = Ident::parse_as(name, IdentRole::Column);
        assert!(
            matches!(outcome, Err(IdentError::Reserved { .. })),
            "the column fence let {name:?} through: {outcome:?}"
        );
        ruled_on += 1;
    }
    for name in ["masked_ssn", "publication", "internal_id", "distance"] {
        assert!(
            Ident::parse_as(name, IdentRole::Column).is_ok(),
            "the column fence over-matched {name:?}"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 18);
    assert!(ruled_on >= 15, "ruled on {ruled_on} column names");
    println!("ruled on {ruled_on} column names");
}

/// The two fences are genuinely different, which is the whole reason the role
/// is an argument. If this arm ever passes with one table, one of the pair has
/// been dropped.
#[test]
fn the_table_and_column_fences_are_not_the_same_fence() {
    // Fenced as a column (the `_` prefix), fine as a table.
    assert!(Ident::parse_as("_scratch", IdentRole::Column).is_err());
    assert!(Ident::parse_as("_scratch", IdentRole::Collection).is_ok());
    // Fenced as a column (classification taxonomy), fine as a table.
    assert!(Ident::parse_as("pii", IdentRole::Column).is_err());
    assert!(Ident::parse_as("pii", IdentRole::Collection).is_ok());
    println!("ruled on 4 (name, role) pairs");
}

/// An alias may lead with `_`; a column may not. That asymmetry is why the
/// role exists at all - the platform's own synthetic result columns are spelled
/// `_distance`, and they are aliases.
#[test]
fn an_alias_may_carry_a_platform_underscore_name_that_a_column_may_not() {
    assert!(Ident::parse_as("_distance", IdentRole::Alias).is_ok());
    assert!(Ident::parse_as("_distance", IdentRole::Column).is_err());
    // The platform prefixes are still fenced on aliases.
    assert!(Ident::parse_as("__zs_leak", IdentRole::Alias).is_err());
    assert!(Ident::parse_as("__zeroship_leak", IdentRole::Alias).is_err());
    println!("ruled on 4 alias vectors");
}

/// The seven system fields are query keys, not forbidden words.
/// `db.users.find({ id: "..." })` is the canonical shape, and the declaration-
/// time reservation (`query.rs:867-878`) is a different call site this crate
/// does not have.
#[test]
fn the_assigned_field_names_are_referenceable_columns() {
    // Spelled locally on purpose. The claim under test is about the IDENTIFIER
    // FENCE - that these ordinary names are not reserved - and that needs a
    // witness of its own. A shared constant would make the test agree with
    // whatever the constant said.
    let mut ruled_on = 0_usize;
    for name in [
        "id",
        "created_at",
        "updated_at",
        "created_by",
        "updated_by",
        "version",
        "deleted_at",
    ] {
        assert!(
            Ident::parse_as(name, IdentRole::Column).is_ok(),
            "the platform field {name:?} is not referenceable as a column"
        );
        ruled_on += 1;
    }
    assert_eq!(ruled_on, 7);
    println!("ruled on {ruled_on} platform field names");
}

/// The refusal must not echo unvalidated text back to whoever reads it. The
/// charset check runs first, so only names that already passed it appear in a
/// message.
#[test]
fn an_illegal_character_refusal_does_not_echo_the_name() {
    let message = Ident::parse_as("user\u{7}name\u{5b57}", IdentRole::Column)
        .expect_err("must refuse")
        .to_string();
    assert!(
        !message.contains("username"),
        "the refusal echoed the offending name: {message}"
    );
    assert!(
        message.contains("\\u{7}"),
        "the refusal did not name the offending character in escaped form: {message}"
    );
    println!("ruled on 1 message");
}

/// The schema fence keeps the worker out of the platform's own namespace. The
/// invariant this serves is that state a separate service writes and the worker
/// only reads must not be nameable from a worker-built plan.
///
/// The witness is any `__zeroship`-prefixed name; the fence is
/// `Reservation::Prefix`, so no particular spelling is load-bearing. Do not use
/// the name of a platform system schema here - it reads as though that schema
/// exists. The fence guards live objects either way: `__zeroship_` is the prefix
/// of the migration journal, the unmask audit table and the workflow journal in
/// every app schema.
#[test]
fn the_namespace_fence_refuses_the_platform_schema() {
    let mut ruled_on = 0_usize;
    for name in [
        "pg_catalog",
        "information_schema",
        "__zeroship_reserved",
        "sqlite_temp",
    ] {
        assert!(
            Ident::parse_as(name, IdentRole::Namespace).is_err(),
            "the namespace fence let {name:?} through"
        );
        ruled_on += 1;
    }
    assert!(Ident::parse_as("app_7f3c1e", IdentRole::Namespace).is_ok());
    ruled_on += 1;
    assert_eq!(ruled_on, 5);
    println!("ruled on {ruled_on} namespaces");
}
