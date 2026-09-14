//! A declared read whose name comes from a variable rather than a literal.
//!
//! Keeping the name at the call site makes declared environment reads explicit
//! during review, so `declared_env!` accepts a `literal` fragment only and a
//! non-literal is a macro-match failure.
//!
//! The positive control is the identical call with a string literal in
//! tests/declared_env.rs.

use zeroship_config_contract::fixtures::FixtureControlConfigConsumer;

fn main() {
    let name = "SOME_EXTERNAL_NAME";
    let _ = zeroship_core::declared_env!(external, name, FixtureControlConfigConsumer);
}
