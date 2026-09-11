//! Runtime ORM, protection pipelines, and database execution.
//!
//! The public API is [`orm`]: deployment-bound database and collection handles,
//! Rust model mapping, and the prepared operations used by the V8 adapter.
//! [`crud`] applies system fields, masking, encryption and result decoding.
//! [`transaction`] and [`exec`] own transaction state and routed statements.
//! [`backend_handle`] registers drivers behind shared contracts; this crate has no
//! dependency on V8 or the worker adapter.

// The engine's async chains nest deeply - a CRUD pass awaiting the routed
// executor awaiting a pooled `compio-postgres` request - and rustc walks the
// whole chain when it computes a block's layout. The adapter carries the same
// attribute for the same reason; it is a compiler resource limit, not a
// correctness guard.
#![recursion_limit = "256"]

extern crate self as zeroship_data_orm;

zeroship_core::declare_env_consumer!(
    /// The ORM's environment reads.
    ///
    /// A LIBRARY consumer, so `target` is the cargo package: this crate is
    /// linked into `zeroship-data-v8`, which is itself linked into the worker
    /// AND the CLI's `zeroship serve` vector.
    pub DataOrmConsumer,
    target = "zeroship-data-orm",
    scope = "data_orm");

pub use zeroship_data_sql::{catalog, compile};
pub mod backend;
pub mod backend_handle;
pub mod backend_selection;
pub mod binding;
pub mod budgets;
pub mod capability;
pub mod cdc;
pub mod connection;
pub mod crud;
pub mod descriptor;
pub mod driver;
pub mod encryption;
pub mod error;
pub mod exec;
pub mod executor;
pub mod lock_policy;
pub mod masking;
pub mod metrics;
pub mod orm;
pub mod protection;
pub(crate) mod schema_cache;
pub mod search;
pub mod storage;
pub mod system_shape_charter;
pub mod transaction;
pub(crate) mod tx_lanes;
pub mod tx_route;
pub use connection::ConnectOptions;
pub use orm::{Collection, Database, Value};

/// Install a descriptor for an isolated test binding.
#[cfg(test)]
#[doc(hidden)]
pub fn cache_schema_for_tests(
    app_id: &str,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
) {
    cache_schema_for_deploy_for_tests(
        &zeroship_data_orm::binding::DbBinding::cold_start(app_id),
        collection,
        schema,
    );
}

/// Test helper: [`cache_schema_for_tests`] for an explicit binding, so a
/// fixture can install two deploys of one app and assert they do not see each
/// other's descriptor entries.
#[cfg(test)]
#[doc(hidden)]
pub fn cache_schema_for_deploy_for_tests(
    binding: &zeroship_data_orm::binding::DbBinding,
    collection: &str,
    schema: zeroship_data_sql::value::Value,
) {
    zeroship_data_orm::schema_cache::with_mut(|c| c.insert_one(binding, collection, schema));
}

// Test capture uses tracing-subscriber from dev-dependencies. Keep the module
// test-only so enabling test helpers in a dependent crate does not require it.
#[cfg(test)]
mod test_support;

#[cfg(test)]
pub(crate) fn reset_engine_for_tests() {
    tx_lanes::reset_for_tests();
    protection::mask_policy::reset_for_tests();
    protection::protection_floor::reset_for_tests();
    metrics::reset_for_tests();
    system_shape_charter::reset_for_tests();
    zeroship_data_orm::schema_cache::reset_for_tests();
}

#[cfg(test)]
pub mod fixtures;

pub mod orm_context;
pub use orm_context::OrmContext;

#[cfg(test)]
mod tx_lane_state_tests;

#[cfg(test)]
#[path = "../../../tests/fixtures/postgres/mod.rs"]
mod postgres_fixture;

#[cfg(test)]
mod live_tests;
#[cfg(test)]
use live_tests::{schema_fixture, support};
