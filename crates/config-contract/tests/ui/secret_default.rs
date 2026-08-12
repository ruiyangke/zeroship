#![allow(unused_imports)]

//! A compiled default for a secret would put credential material in the binary.

use zeroship_core::config::{zeroship_config, Secret};

#[zeroship_config(binary = "zeroship-fixture-bad", scope = "bad")]
struct BakedSecretConfig {
    #[config(name = "bad.token", default = "hunter2")]
    token: Secret<String>,
}

fn main() {}
