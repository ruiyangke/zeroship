//! Configuration-contract suites linked into one test executable.
//!
//! Suites that register into a link-time registry keep their own target so
//! their registrations cannot leak into another suite's enumeration: see the
//! `declared_env`, `linked_registry` and `typed_accessor` stanzas in
//! Cargo.toml. `autotests` is off, so a new file is compiled by nothing until
//! it is declared either here or as a `[[test]]` entry.

mod compile_fail;
mod contract_fixtures;
mod metadata_contract;
mod overlay_sections;
mod real_registry;
mod workspace_lints;
