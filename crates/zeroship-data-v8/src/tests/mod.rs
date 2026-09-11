//! Adapter contracts exercised through JavaScript and the public runtime.
//! PostgreSQL and SQLite fixtures are required by ordinary package tests.
pub(crate) mod fixtures;
mod integration;
mod missing_role;
mod native_transaction;
pub(crate) mod parity;
mod sqlite_integration;
