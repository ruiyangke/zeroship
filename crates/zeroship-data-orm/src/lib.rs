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

pub mod orm_context;
pub use orm_context::OrmContext;

#[cfg(test)]
mod tx_lane_state_tests;

#[cfg(test)]
mod tests;
