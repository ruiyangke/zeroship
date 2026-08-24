//! The registry is where a backend id stops being trusted and starts being
//! checked, and where the eight-backend cap has to be gone.
//!
//! Three separate claims live here and they fail for different reasons:
//!
//! 1. **The cap is gone.** A registry of NINE backends must hold nine. The old
//!    `DialectSet(u8)` could not represent them: three bits were spoken for and
//!    the other five had no ids to be spoken for BY, so nine registrants
//!    collapsed to three. This is a REPRESENTATION failure, not a compile
//!    failure — the registry builds, it just cannot say what it holds.
//! 2. **A duplicate id is refused, naming both registrants.** Never
//!    last-one-wins: two backends silently sharing capability rows is worse than
//!    the closed enum this replaces.
//! 3. **The over-refusal control.** Every distinct, well-formed id must still
//!    register. A refusal that also refuses the working case is a regression.

use zero_migrate_ir::backend::{
    BackendDescriptor, BackendRegistry, CapabilitySet, IdentifierLimit, Limits, RegistryError,
};
use zero_migrate_ir::dialect::DialectId;

const NEUTRAL_LIMITS: Limits = Limits {
    identifier: IdentifierLimit::Unbounded,
    reserved_identifier_prefixes: &[],
};

const fn descriptor(id: &'static str, display_name: &'static str) -> BackendDescriptor {
    BackendDescriptor {
        id: DialectId::new(id),
        display_name,
        capabilities: CapabilitySet::empty(),
        limits: NEUTRAL_LIMITS,
    }
}

// Nine distinct, well-formed ids. Nine, not eight: eight would still fit the
// bitset that used to back the set and would prove nothing.
static B1: BackendDescriptor = descriptor("postgres", "PostgreSQL");
static B2: BackendDescriptor = descriptor("sqlite", "SQLite");
static B3: BackendDescriptor = descriptor("mysql", "MySQL");
static B4: BackendDescriptor = descriptor("duckdb", "DuckDB");
static B5: BackendDescriptor = descriptor("cockroachdb", "CockroachDB");
static B6: BackendDescriptor = descriptor("clickhouse", "ClickHouse");
static B7: BackendDescriptor = descriptor("mariadb", "MariaDB");
static B8: BackendDescriptor = descriptor("libsql", "libSQL");
static B9: BackendDescriptor = descriptor("spanner", "Cloud Spanner");

const NINE: &[&BackendDescriptor] = &[&B1, &B2, &B3, &B4, &B5, &B6, &B7, &B8, &B9];

/// THE CAP TEST. Nine registered backends must be nine retrievable backends.
#[test]
fn a_registry_holds_more_than_eight_backends() {
    let registry = BackendRegistry::build(NINE).expect("nine distinct well-formed ids register");

    assert_eq!(registry.len(), 9, "every registrant must be kept");

    let ids = registry.ids();
    assert_eq!(
        ids.len(),
        9,
        "the id SET must represent all nine; it reported {:?}",
        ids.iter().map(|id| id.as_str()).collect::<Vec<_>>()
    );

    for descriptor in NINE {
        assert!(
            ids.contains_id(&descriptor.id),
            "{} is registered but the id set does not contain it",
            descriptor.id
        );
        assert_eq!(
            registry.get(&descriptor.id).map(|d| d.display_name),
            Some(descriptor.display_name),
            "{} must resolve back to its own descriptor",
            descriptor.id
        );
    }
}

/// The set must not merely COUNT nine — it must distinguish them. A set that
/// answered "yes" to everything would pass the count assertion above.
#[test]
fn an_unregistered_id_is_not_a_member() {
    let registry = BackendRegistry::build(NINE).expect("nine distinct well-formed ids register");
    let ids = registry.ids();

    for absent in ["oracle", "db2", "informix"] {
        let id = DialectId::new(absent);
        assert!(
            !ids.contains_id(&id),
            "{absent} was never registered and must not be a member"
        );
        assert!(registry.get(&id).is_none(), "{absent} must not resolve");
    }
}

// ---------------------------------------------------------------------------
// The refusal, and its over-refusal control
// ---------------------------------------------------------------------------

#[test]
fn a_duplicate_id_is_refused_naming_both_registrants() {
    static IMPOSTOR: BackendDescriptor = descriptor("postgres", "Postgres Compatible");
    let err = BackendRegistry::build(&[&B1, &B2, &IMPOSTOR])
        .expect_err("two backends claiming one id is not a registry");

    match err {
        RegistryError::DuplicateId {
            id,
            first_display_name,
            first_index,
            second_display_name,
            second_index,
        } => {
            assert_eq!(id, "postgres");
            assert_eq!((first_display_name, first_index), ("PostgreSQL", 0));
            assert_eq!(
                (second_display_name, second_index),
                ("Postgres Compatible", 2)
            );
        }
        other @ RegistryError::MalformedId { .. } => {
            panic!("expected a duplicate-id refusal, got {other:?}")
        }
    }

    // The diagnostic must name BOTH, not just the loser.
    let rendered = BackendRegistry::build(&[&B1, &B2, &IMPOSTOR])
        .expect_err("still refused")
        .to_string();
    assert!(rendered.contains("PostgreSQL"), "{rendered}");
    assert!(rendered.contains("Postgres Compatible"), "{rendered}");
    assert!(rendered.contains("postgres"), "{rendered}");
}

static BAD_UPPERCASE: BackendDescriptor = descriptor("Postgres", "Uppercase");
static BAD_LEADING_DIGIT: BackendDescriptor = descriptor("1st", "Leading digit");
static BAD_LEADING_UNDERSCORE: BackendDescriptor = descriptor("_pg", "Leading underscore");
static BAD_DASH: BackendDescriptor = descriptor("cockroach-db", "Dash");
static BAD_EMPTY: BackendDescriptor = descriptor("", "Empty");

#[test]
fn a_malformed_id_is_refused_at_registration() {
    for bad in [
        &BAD_UPPERCASE,
        &BAD_LEADING_DIGIT,
        &BAD_LEADING_UNDERSCORE,
        &BAD_DASH,
        &BAD_EMPTY,
    ] {
        let err = BackendRegistry::build(&[&B1, bad])
            .expect_err("a malformed id must be refused at REGISTRATION rather than trusted");
        match err {
            RegistryError::MalformedId {
                id,
                display_name,
                index,
            } => {
                assert_eq!(id, bad.id.as_str());
                assert_eq!(display_name, bad.display_name);
                assert_eq!(index, 1);
            }
            other @ RegistryError::DuplicateId { .. } => {
                panic!("expected a malformed-id refusal for {bad:?}, got {other:?}")
            }
        }
    }
}

/// OVER-REFUSAL CONTROL. The refusals above must not refuse the working case.
#[test]
fn every_distinct_well_formed_id_still_registers() {
    // Ids that LOOK adjacent but are distinct must all coexist.
    static NEAR1: BackendDescriptor = descriptor("postgres", "PostgreSQL");
    static NEAR2: BackendDescriptor = descriptor("postgres_xl", "Postgres-XL");
    static NEAR3: BackendDescriptor = descriptor("postgres2", "PostgreSQL 2");
    static NEAR4: BackendDescriptor = descriptor("p", "P");
    let registry = BackendRegistry::build(&[&NEAR1, &NEAR2, &NEAR3, &NEAR4])
        .expect("distinct ids that share a prefix are distinct backends");
    assert_eq!(registry.len(), 4);
    assert_eq!(registry.ids().len(), 4);
}
