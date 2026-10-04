//! A key declared for one binary, read with another binary's consumer token.
//!
//! The registry check `validate_contract` in
//! crates/zeroship-config-contract/src/contract.rs catches this after linking.
//! This proves the same mistake is also a type error, so it cannot reach a
//! running process.
//!
//! The key is a `const` on purpose. `read_config_env!` places the identity in a
//! linked `static`, so a `let` binding fails for an unrelated reason and would
//! make this fixture stop isolating the consumer mismatch. The positive control
//! is `the_matching_consumer_token_compiles_and_reads` in
//! crates/zeroship-config-contract/tests/integration/typed_accessor.rs: same
//! call, same key, only the consumer differs, and it compiles.

use zeroship_core::config::{zeroship_config, CanonicalName, EnvKey};

#[zeroship_config(binary = "zeroship-fixture-control", scope = "control")]
struct FixtureControlConfig {
    #[config(name = "control.port")]
    port: zeroship_core::config::Operational<u16>,
}

#[zeroship_config(binary = "zeroship-fixture-worker", scope = "worker")]
struct FixtureWorkerConfig {
    #[config(name = "worker.max_isolates")]
    max_isolates: zeroship_core::config::Operational<usize>,
}

const CONTROL_PORT: EnvKey<String, FixtureControlConfigConsumer> =
    EnvKey::from_static(CanonicalName::from_static("control.port"));

fn main() {
    let _ = zeroship_core::read_config_env!(CONTROL_PORT, FixtureWorkerConfigConsumer);
}
