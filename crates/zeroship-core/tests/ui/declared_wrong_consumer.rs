//! A declared key bound to one component, read with another's consumer token.
//!
//! The peer of `wrong_consumer.rs`, for the non-config population. The central
//! accessor takes a typed key AND a consumer; if the consumer were only a
//! label, this would compile and the read would be recorded against the wrong
//! component.
//!
//! The positive control is `a_read_site_carries_its_class_and_its_consumer` in
//! crates/zeroship-config-contract/tests/integration/declared_env.rs: same
//! macro, same key shape, matching consumer, and it compiles and records.

use zeroship_core::config::{zeroship_config, DeclaredEnvKey};

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

const CONTROL_ONLY: DeclaredEnvKey<String, FixtureControlConfigConsumer> =
    DeclaredEnvKey::external("SOME_EXTERNAL_NAME");

fn main() {
    let _ = zeroship_core::read_declared_env!(CONTROL_ONLY, FixtureWorkerConfigConsumer);
}
