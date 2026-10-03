//! The configuration-contract suites, linked into one test executable.
//!
//! `autotests` is off, so a new `tests/integration/<name>.rs` is compiled by
//! nothing until `tests/integration/mod.rs` declares it. Suites that register
//! into a link-time registry keep their own target so their registrations
//! cannot leak into another suite's enumeration: see the `declared_env`,
//! `linked_registry` and `typed_accessor` stanzas in Cargo.toml.

mod integration;
