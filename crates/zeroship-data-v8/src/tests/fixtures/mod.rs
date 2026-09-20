//! Private adapter setup and observations through public backend interfaces.
#[path = "../../../../../tests/fixtures/data/mod.rs"]
mod data;
pub(crate) use data::*;
mod context;
#[path = "../../../../../tests/fixtures/data/schema.rs"]
pub(crate) mod schema;
pub(crate) use context::{
    SuppliedProjectKeysGuard, binding, install_cold_schema, install_schema, key_source,
    project_keys, reset_context, set_database_url, supply_app_bindings, supply_project_key,
};
pub(crate) mod parity;
pub(crate) mod recording;
pub(crate) mod sqlite;

/// The runtime descriptor document a harness hands a `Runtime`, naming the
/// database `harness_binding` resolved for this app as the PRIMARY.
///
/// The document and the supplied binding have to name the same database, or
/// the plugin refuses the descriptor it cannot resolve a binding for - which
/// is the point of the refusal, and makes a hand-written document in a test a
/// silent mismatch waiting to happen.
pub(crate) fn harness_descriptor_document(app_id: &str, schema: &str) -> String {
    let binding = harness_binding(app_id);
    let database = binding
        .database()
        .expect("a harness binding addresses a database")
        .as_str()
        .to_owned();
    zeroship_runtime::databases::RuntimeDatabases::single("main", &database, schema)
        .expect("a harness schema is valid JSON")
}

/// The same document as [`harness_descriptor_document`], parsed, for a test
/// that drives the plugin hook directly rather than through a `Runtime`.
pub(crate) fn harness_descriptor_value(
    app_id: &str,
    schema: &serde_json::Value,
) -> serde_json::Value {
    let document = harness_descriptor_document(app_id, &schema.to_string());
    serde_json::from_str(&document).expect("the harness document is valid JSON")
}
