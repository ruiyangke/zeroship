//! Stream integration suites linked into one test executable.
//!
//! `autotests` is off, so a new `tests/integration/<name>.rs` is compiled by
//! nothing until it is declared here.

mod memory_roundtrip;
mod redpanda_roundtrip;
mod registry;
