//! A declared key bound to one component, read with another's consumer token.
//!
//! The peer of `wrong_consumer.rs`, for the non-config population. Section 4.5
//! requires that the central accessor take a typed key AND a consumer; if the
//! consumer were only a label, this would compile and the read would be
//! recorded against the wrong component.
//!
//! The positive control is `a_read_site_carries_its_class_and_its_consumer` in
//! tests/declared_env.rs: same macro, same key shape, matching consumer, and it
//! compiles and records.

use zeroship_config_contract::fixtures::{FixtureControlConfigConsumer, FixtureWorkerConfigConsumer};
use zeroship_core::config::DeclaredEnvKey;

const CONTROL_ONLY: DeclaredEnvKey<String, FixtureControlConfigConsumer> =
    DeclaredEnvKey::external("SOME_EXTERNAL_NAME");

fn main() {
    let _ = zeroship_core::read_declared_env!(CONTROL_ONLY, FixtureWorkerConfigConsumer);
}
