//! A declared key held in a runtime binding, not a constant.
//!
//! `read_declared_env!` copies the key's name, class and consumer into a linked
//! `static`, which is what makes the read enumerable. A `let` binding cannot be
//! read from a `static` initializer, so the compiler rejects it. Without this
//! the macro would appear to work while emitting a read site describing
//! whatever the binding happened to hold at expansion time - or, worse, would
//! have to fall back to not emitting one at all.
//!
//! The positive control is the same call with a `const` key in
//! tests/declared_env.rs.

use zeroship_config_contract::fixtures::FixtureControlConfigConsumer;
use zeroship_core::config::DeclaredEnvKey;

fn main() {
    let runtime_key: DeclaredEnvKey<String, FixtureControlConfigConsumer> =
        DeclaredEnvKey::external("SOME_EXTERNAL_NAME");
    let _ = zeroship_core::read_declared_env!(runtime_key, FixtureControlConfigConsumer);
}
