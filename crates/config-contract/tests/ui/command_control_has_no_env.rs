#![allow(unused_imports)]

//! A command control reaches no environment tier.
//!
//! `resolve_control` takes no overlay and the generated carrier carries no clap
//! `env`, so the only way to give `--check-config` an environment source would
//! be a hand-written typed read. That read needs an `EnvKey` for a canonical
//! name this consumer never declares as env-supplied, and the registry rejects
//! it as an undeclared read site. What the compiler stops here is the earlier
//! mistake: naming a wrapper the attribute does not implement.

use zeroship_core::config::{zeroship_config, CommandEnv};

#[zeroship_config(binary = "zeroship-fixture-bad", scope = "bad")]
struct EnvBackedCommandConfig {
    #[config(name = "check_config")]
    check_config: CommandEnv<bool>,
}

fn main() {}
