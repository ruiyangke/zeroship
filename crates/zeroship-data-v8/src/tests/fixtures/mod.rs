//! Private adapter setup and observations through public backend interfaces.
#[path = "../../../../../tests/fixtures/data/mod.rs"]
mod data;
pub(crate) use data::*;
mod context;
#[path = "../../../../../tests/fixtures/data/schema.rs"]
pub(crate) mod schema;
pub(crate) use context::{
    SuppliedProjectKeysGuard, binding, install_cold_schema, install_schema, key_source,
    reset_context, set_database_url, supply_project_key,
};
pub(crate) mod recording;
pub(crate) mod sqlite;
