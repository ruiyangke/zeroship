#![allow(unused_imports)]

//! A declaration whose canonical identity is not the ASCII lowercase grammar.
//!
//! config-macros unit-tests `validate_canonical` directly; this proves the
//! rejection is actually wired into the attribute a declaration uses.

use zeroship_core::config::{zeroship_config, Operational};

#[zeroship_config(binary = "zeroship-fixture-bad", scope = "bad")]
struct BadConfig {
    #[config(name = "Control.Port")]
    port: Operational<u16>,
}

fn main() {}
