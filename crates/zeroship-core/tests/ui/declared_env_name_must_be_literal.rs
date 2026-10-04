//! A declared read whose name comes from a variable rather than a literal.
//!
//! Keeping the name at the call site makes declared environment reads explicit
//! during review, so `declared_env!` accepts a `literal` fragment only and a
//! non-literal is a macro-match failure.
//!
//! The positive control is the identical call with a string literal in
//! `every_shape_of_declared_read_registers_a_site` in
//! crates/zeroship-config-contract/tests/integration/declared_env.rs.

use zeroship_core::config::zeroship_config;

#[zeroship_config(binary = "zeroship-fixture-control", scope = "control")]
struct FixtureControlConfig {
    #[config(name = "control.port")]
    port: zeroship_core::config::Operational<u16>,
}

fn main() {
    let name = "SOME_EXTERNAL_NAME";
    let _ = zeroship_core::declared_env!(external, name, FixtureControlConfigConsumer);
}
