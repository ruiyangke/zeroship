#![allow(unused_imports)]

//! A flag control's default is false and an optional control's is None.
//!
//! Spelling either out invites a `default = true` that quietly inverts a safety
//! control, so the attribute refuses both rather than honouring one.

use std::path::PathBuf;

use zeroship_core::config::{zeroship_config, BootstrapControl};

#[zeroship_config(binary = "zeroship-fixture-bad", scope = "bad")]
struct DefaultedControlConfig {
    #[config(shared = NO_CONFIG, default = true)]
    no_config: BootstrapControl<bool>,
    #[config(shared = CONFIG)]
    config: BootstrapControl<Option<PathBuf>>,
}

fn main() {}
