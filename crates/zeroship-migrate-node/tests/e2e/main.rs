//! The migrate-node end-to-end suites, linked into one test executable.
//!
//! These spawn the real CLI through a Node host and build the shipped addon, so
//! they live in their own `e2e` target rather than the `integration` binary.

mod platform_corpus;
mod shipped_symbol_shape;
