//! CLI end-to-end suites linked into one test executable.
//!
//! Every suite drives the shipped `zeroship` binary through
//! `CARGO_BIN_EXE_zeroship`. `autotests` is off, so a new
//! `tests/e2e/<name>.rs` is compiled by nothing until it is declared here.

mod child_output;
mod deploy_test;
mod dev_init_test;
mod login_test;
mod parent_death_test;
mod parent_death_unix_test;
mod parent_death_windows_test;
mod secrets_test;
mod serve_test;
// The suite's host trusts a port only once `/proc` shows its own child holds
// the listening socket, and `/proc` is Linux's.
#[cfg(target_os = "linux")]
mod workflow_local;
