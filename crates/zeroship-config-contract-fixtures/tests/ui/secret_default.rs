//! A compiled default for a secret would put credential material in the binary.

use zeroship_core::config::zeroship_config;

#[zeroship_config(binary = "zeroship-fixture-bad", scope = "bad")]
struct BakedSecretConfig {
    #[config(name = "bad.token", default = "hunter2")]
    token: zeroship_core::config::Secret<String>,
}

fn main() {}
