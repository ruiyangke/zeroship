//! The CLI end-to-end suites, linked into one test executable.
//!
//! Every suite drives the shipped `zeroship` binary through
//! `CARGO_BIN_EXE_zeroship`. `autotests` is off, so a new
//! `tests/e2e/<name>.rs` is compiled by nothing until `tests/e2e/mod.rs`
//! declares it.

mod e2e;
