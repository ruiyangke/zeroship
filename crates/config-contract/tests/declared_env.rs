//! The declared (non-config) read path, exercised from OUTSIDE `zeroship-core`.
//!
//! It has its own binary for the same reason `typed_accessor.rs` does: it links
//! `DECLARED_ENV_READS` entries, and mixing those into a registry test would
//! make that test's enumeration depend on this file's contents.
//!
//! Every assertion here is about the RECORD, not about any variable's value.
//! Whether `PATH` is set is the operating system's business; whether a read of
//! it is enumerable is this contract's.

use std::collections::BTreeSet;

use zeroship_core::config::{
    ConfigConsumer, DeclaredEnvKey, EnvClass, TestHarness, DECLARED_ENV_READS,
};
use zeroship_core::{declare_env_consumer, declared_env, declared_env_os, read_declared_env};

declare_env_consumer!(
    /// Stand-in for a library crate's marker.
    FixtureLibraryConsumer,
    target = "zeroship-config-contract",
    scope = "fixture"
);

const SHARED_NAME: DeclaredEnvKey<String, FixtureLibraryConsumer> =
    DeclaredEnvKey::external("ZEROSHIP_CONFIG_CONTRACT_FIXTURE_SHARED");

#[test]
fn every_shape_of_declared_read_registers_a_site() {
    // Mutation basis: delete any one of these calls and its name disappears
    // from DECLARED_ENV_READS, which is the property the gate depends on.
    // Does not cover: reads in code this binary does not link. A distributed
    // slice can only report what the linker saw, which is exactly why the
    // SOURCE gate exists alongside it.
    let _ = declared_env!(external, "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_PLAIN", TestHarness);
    let _ = declared_env_os!(test, "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_OS", TestHarness);
    let _ = read_declared_env!(SHARED_NAME, FixtureLibraryConsumer);
    let _ = zeroship_core::test_env!("ZEROSHIP_CONFIG_CONTRACT_FIXTURE_TEST");

    let names = DECLARED_ENV_READS
        .iter()
        .map(|read| read.name())
        .collect::<BTreeSet<_>>();
    for expected in [
        "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_PLAIN",
        "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_OS",
        "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_SHARED",
        "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_TEST",
    ] {
        assert!(names.contains(expected), "{expected} was not registered");
    }
}

#[test]
fn a_read_site_carries_its_class_and_its_consumer() {
    let shared = DECLARED_ENV_READS
        .iter()
        .find(|read| read.name() == "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_SHARED")
        .expect("the explicit-key read is registered");

    assert_eq!(shared.class(), EnvClass::External);
    assert_eq!(
        shared.consumer().target(),
        FixtureLibraryConsumer::BINARY,
        "a declared read must name the component that performed it"
    );
    let (file, line, _column) = shared.location();
    assert!(file.ends_with("declared_env.rs"), "recorded file was {file}");
    assert!(line > 0);

    // The one-variable partner: same macro, same shape, different consumer
    // marker. If the consumer column were ignored, these two would agree.
    let test_class = DECLARED_ENV_READS
        .iter()
        .find(|read| read.name() == "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_TEST")
        .expect("the test-class read is registered");
    assert_eq!(test_class.class(), EnvClass::Test);
    assert_eq!(test_class.consumer().target(), TestHarness::BINARY);
    assert_ne!(test_class.consumer().target(), shared.consumer().target());
}

#[test]
fn a_declared_read_reports_absence_rather_than_a_value() {
    // Does not cover: the value of a variable that IS set. This asserts the
    // absent arm, which is the only one a test can assert without arranging
    // process-wide state that other tests in this binary would observe.
    let absent: Option<String> = declared_env!(
        test,
        "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_DEFINITELY_UNSET",
        TestHarness
    );
    assert_eq!(absent, None);
}
