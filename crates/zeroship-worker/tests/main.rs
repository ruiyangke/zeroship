//! The worker end-to-end suites, linked into one test executable.
//!
//! Every suite drives the shipped `zeroship-worker` binary through
//! `CARGO_BIN_EXE_zeroship-worker`. `autotests` is off, so a new
//! `tests/e2e/<name>.rs` is compiled by nothing until `tests/e2e/mod.rs`
//! declares it.

mod e2e;
