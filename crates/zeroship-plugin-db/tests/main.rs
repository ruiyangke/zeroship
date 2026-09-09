#![recursion_limit = "256"]

//! Integration contracts independent of the shared database fixtures.
//!
//! Ordinary package tests run this target, the shared fixtures in
//! `tests/test_helpers.rs`, and the process-isolated distributed CDC target.
//! Test builds enable integration helpers automatically and require PostgreSQL.
//!
//! With `autotests = false`, add each new test file to the appropriate entry
//! file's module list. Select a subset using its module path, for example:
//!
//! `cargo test -p zeroship-plugin-db --test main capability::`

mod audit_table_parity;
mod capability;
mod db_v8_class;
mod platform_fence;
mod subscription_finalizer;
