//! Worker end-to-end suites linked into one test executable.
//!
//! Every suite drives the shipped `zeroship-worker` binary through
//! `CARGO_BIN_EXE_zeroship-worker`. `autotests` is off, so a new
//! `tests/e2e/<name>.rs` is compiled by nothing until it is declared here.

mod check_config;
mod peer_boot;
mod plaintext_peers_env_tier;
