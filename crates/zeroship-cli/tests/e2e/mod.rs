//! CLI end-to-end suites linked into one test executable.
//!
//! Every suite drives the shipped `zeroship` binary through
//! `CARGO_BIN_EXE_zeroship`. `autotests` is off, so a new
//! `tests/e2e/<name>.rs` is compiled by nothing until it is declared here.

mod deploy_test;
mod dev_init_test;
mod login_test;
mod parent_death_test;
mod secrets_test;
mod workflow_local;
