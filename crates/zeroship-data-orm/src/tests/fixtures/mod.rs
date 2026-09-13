//! Private setup for database and implementation tests in this crate.
#[path = "../../../../../tests/fixtures/data/mod.rs"]
mod data;
pub(crate) use data::*;
mod host;
#[path = "../../../../../tests/fixtures/data/schema.rs"]
pub(crate) mod schema;
pub(crate) use host::Host;
mod database;
pub(crate) use database::DatabaseFixture;
pub(crate) mod events;
mod state;
pub(crate) use state::{
    cache_schema, cache_schema_for_deploy, generated_schema, native_fields, reset_engine,
};
mod unit;
pub(crate) use unit::{unit_backend, unit_route};
