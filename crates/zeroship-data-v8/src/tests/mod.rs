//! Adapter contracts exercised through JavaScript and the public runtime.
//! PostgreSQL and SQLite fixtures are required by ordinary package tests.
mod integration;
mod missing_role;
mod native_transaction;
pub(crate) mod parity;
#[path = "../../../../tests/fixtures/data/schema.rs"]
pub(crate) mod schema_fixture;
mod sqlite_integration;
pub(crate) mod support;

pub(crate) mod recording;
mod sqlite;
