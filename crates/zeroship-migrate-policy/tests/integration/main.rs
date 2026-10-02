//! The policy crate's integration suites, linked into one test executable.
//!
//! Cargo links one executable per `tests/*.rs`; `Cargo.toml` sets `autotests =
//! false` and registers this file as the `integration` target, so a suite is
//! compiled only once it is declared here. Add `mod <name>;` with the file or its
//! tests never run.

mod admit_permutation;
mod compose_oracle;
mod loader;
mod seal;
