#![recursion_limit = "256"]
//! Adapter contracts exercised through JavaScript and the public runtime.
//! PostgreSQL and SQLite fixtures are required by ordinary package tests.
mod parity;
#[path = "../../../tests/fixtures/data/schema.rs"]
mod schema_fixture;
mod support;
mod integration;
mod sqlite_integration;
mod native_transaction;
mod missing_role;
