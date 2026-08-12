//! The positive control for `tests/ui/wrong_consumer.rs`.
//!
//! This lives in its own integration-test binary ON PURPOSE. It is the only
//! place in the test suite that names a read site by hand, and while it sat in
//! linked_registry.rs it registered a second `control.port` Env site into that
//! binary's `CONFIG_READ_SITES`. That silently weakened the independence claim
//! there ("enumerated without this crate naming a read site") and forced a
//! `dedup` that hid double registration. Separate binaries, separate registries.

use zeroship_config_contract::fixtures::FixtureControlConfigConsumer;
use zeroship_core::config::{CanonicalName, EnvKey};

const CONTROL_PORT: EnvKey<String, FixtureControlConfigConsumer> =
    EnvKey::from_static(CanonicalName::from_static("control.port"));

#[test]
fn the_matching_consumer_token_compiles_and_reads() {
    // Same macro, same const key, same shape as tests/ui/wrong_consumer.rs. The
    // ONLY difference is that the consumer marker matches the key's. That
    // partner is what separates "the type check discriminates" from "the
    // fixture failed for some unrelated reason".
    // Does not cover: the value itself. This asserts the call type-checks and
    // reports absence, not any particular environment content.
    let read = zeroship_core::read_config_env!(CONTROL_PORT, FixtureControlConfigConsumer);
    assert!(read.is_ok());
}
