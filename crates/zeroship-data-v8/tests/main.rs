#![recursion_limit = "256"]

//! Integration contracts independent of the shared database fixtures.
//!
//! Ordinary package tests run this target, the shared fixtures in
//! `src/tests/`, and the process-isolated distributed CDC target.
//! Database fixtures are private source modules; PostgreSQL is required.
//!
//! With `autotests = false`, add each new test file to the appropriate entry
//! file's module list. Select a subset using its module path, for example:
//!
//! `cargo test -p zeroship-data-v8 --test main capability::`

mod audit_table_parity;
mod capability;
mod subscription_finalizer;
