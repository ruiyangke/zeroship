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
    DeclaredEnvKey::external("CONFIG_CONTRACT_FIXTURE_SHARED");

#[test]
fn every_shape_of_declared_read_registers_a_site() {
    // Mutation basis: delete any one of these calls and its name disappears
    // from DECLARED_ENV_READS, which is the property the gate depends on.
    // Does not cover: reads in code this binary does not link. A distributed
    // slice can only report what the linker saw, which is exactly why the
    // SOURCE gate exists alongside it.
    let _ = declared_env!(external, "CONFIG_CONTRACT_FIXTURE_PLAIN", TestHarness);
    let _ = declared_env_os!(test, "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_OS", TestHarness);
    let _ = read_declared_env!(SHARED_NAME, FixtureLibraryConsumer);
    let _ = zeroship_core::test_env!("ZEROSHIP_CONFIG_CONTRACT_FIXTURE_TEST");

    let names = DECLARED_ENV_READS
        .iter()
        .map(|read| read.name())
        .collect::<BTreeSet<_>>();
    for expected in [
        "CONFIG_CONTRACT_FIXTURE_PLAIN",
        "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_OS",
        "CONFIG_CONTRACT_FIXTURE_SHARED",
        "ZEROSHIP_CONFIG_CONTRACT_FIXTURE_TEST",
    ] {
        assert!(names.contains(expected), "{expected} was not registered");
    }
}

#[test]
fn a_read_site_carries_its_class_and_its_consumer() {
    let shared = DECLARED_ENV_READS
        .iter()
        .find(|read| read.name() == "CONFIG_CONTRACT_FIXTURE_SHARED")
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
fn every_linked_declared_read_has_a_usable_environment_name() {
    // The SOURCE gate checks key literals, which is the complete population
    // today because `declared_env!` accepts a literal only. This checks the
    // LINKED population instead, so a key built some other way - a `const fn`
    // that assembles a name, a future constructor - still cannot register a
    // name no shell can set.
    // Does not cover: names in code this binary does not link, and the reserved
    // snapshot marker, which is deliberately not a variable name.
    assert!(
        !DECLARED_ENV_READS.is_empty(),
        "no declared reads linked; this test would pass vacuously"
    );
    for read in DECLARED_ENV_READS {
        if read.name() == zeroship_core::config::PROCESS_ENV_SNAPSHOT {
            continue;
        }
        assert!(
            zeroship_core::config::is_valid_env_name(read.name()),
            "{:?} at {:?} is not a usable environment name",
            read.name(),
            read.location()
        );
        assert!(
            read.class() != EnvClass::External || !read.name().starts_with("ZEROSHIP_"),
            "{:?} at {:?} claims to be somebody else's contract but carries our \
             prefix",
            read.name(),
            read.location()
        );
    }
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

/// The `platform` class is a debt ledger, and this is what stops it growing.
///
/// `EnvClass::Platform` documents itself as a transitional class whose members
/// still owe a generated `#[zeroship_config]` declaration. A promise like that
/// in a doc comment is the kind of claim that reads as protection and is not:
/// nothing stopped the next conversion from reaching for it. So the count is
/// pinned. Raising the ceiling is a deliberate edit with a diff someone reviews.
///
/// It is a SOURCE census, not a linked one: `DECLARED_ENV_READS` only contains
/// what this test binary links, which would silently exclude most of the tree.
///
/// Does not cover: whether a `platform` entry is CORRECTLY classified. A name
/// that should have been `dev` sits inside the ceiling as comfortably as one
/// that could not be anything else.
#[test]
fn the_platform_class_census_only_shrinks() {
    use zeroship_config_contract::inventory::collect_tracked_rust_sources;
    use zeroship_config_contract::raw_env::collect_declared_keys;

    // Measured 2026-08-12, at the end of Step 4: 14 distinct names across 18
    // sites. Distinct NAMES, not sites, because the debt is one missing
    // declaration per identity however many places read it. Lower it as Step 3
    // and Step 6 give these settings real declarations; never raise it without
    // saying in the commit why a generated declaration was not possible.
    //
    // KNOWN BLIND SPOT, stated rather than papered over:
    // `ZEROSHIP_BLOB_UPLOAD_CONCURRENCY` is a fifteenth platform-class name.
    // It is declared inside the body of `resolve_s3_runtime!` in
    // `crates/zeroship-core/src/config/declared.rs`, and a `macro_rules!` body is tokens
    // rather than expressions, so this source census cannot see it. It is
    // counted here in prose and not in the number.
    const CEILING: usize = 14;

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");
    let sources = collect_tracked_rust_sources(root).expect("tracked Rust sources");
    assert!(
        sources.len() > 500,
        "only {} tracked files; the enumeration is broken and a low count would \
         prove nothing",
        sources.len()
    );

    let keys = collect_declared_keys(&sources).expect("declared key census");
    assert!(
        !keys.is_empty(),
        "no declared keys found at all; the census instrument is broken"
    );

    let platform = keys
        .iter()
        .filter(|key| key.class == "platform")
        .collect::<Vec<_>>();
    let names = platform
        .iter()
        .map(|key| key.name.clone())
        .collect::<BTreeSet<_>>();
    assert!(
        names.len() <= CEILING,
        "the platform class has grown to {} distinct names (ceiling {CEILING}) \
         across {} sites:\n{}",
        names.len(),
        platform.len(),
        platform
            .iter()
            .map(|key| format!("  {} ({})", key.name, key.file))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
