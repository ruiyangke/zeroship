//! A declared read whose name comes from a variable rather than a literal.
//!
//! The source gate lifts key literals out of the syntax tree to check their
//! spelling and their class. A name behind a binding would be declared to the
//! type system and invisible to that audit, so `declared_env!` accepts a
//! `literal` fragment only and a non-literal is a macro-match failure.
//!
//! The positive control is the identical call with a string literal in
//! tests/declared_env.rs.

use zeroship_config_contract::fixtures::FixtureControlConfigConsumer;

fn main() {
    let name = "SOME_EXTERNAL_NAME";
    let _ = zeroship_core::declared_env!(external, name, FixtureControlConfigConsumer);
}
