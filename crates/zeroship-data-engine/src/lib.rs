//! The data plane's ENGINE tier.
//!
//! # What lives here
//!
//! Everything between the per-isolate composition root and the two drivers:
//!
//! - [`crud`] - the read and write pipelines and their passes (system fields,
//!   bytes, encryption, masking, unmask).
//! - [`transaction`] - the SC-1 protocol: reducer, driver, frames, deadlines.
//! - [`exec`] - the routed executor every statement funnels through, and the
//!   single point that emits the raw usage metrics in [`metrics`].
//! - [`tx_lanes`] - the per-isolate transaction lane owner (claims, parked
//!   sessions, pending emits, reducers).
//! - [`tx_route`] - the tx-vs-pool routing decision, frozen at the V8 dispatch
//!   frame and carried into the spawned future.
//! - [`backend_handle`] / [`backend`] - the dispatch enum over both vendors and
//!   the prelude that names their traits.
//! - [`auth`] - per-app PostgreSQL role provisioning.
//!
//! # What is deliberately absent
//!
//! **Any name from `zeroship-plugin-db`.** The adapter owns the V8 classes, the
//! per-isolate `context`, the CDC modules and the service lifecycle; this crate
//! is below it, so it cannot name it. That is enforced by the manifest, not by
//! review - `zeroship-plugin-db` appears in neither dependency table, so
//! `zeroship_plugin_db::…` here is E0433.
//!
//! Where a helper here genuinely needed adapter state, the state is a
//! PARAMETER: [`exec::ambient_route_for_tests`] takes the backend,
//! [`backend_selection::new_sqlite_backend`] takes the key source, and
//! [`transaction::probe::begin`] takes the handle. The caller that owns the
//! context does the lookup, which is the same correction
//! `PostgresBackend::new` and `open_sqlite_backend` already took one tier down.
//!
//! # How `crate::` paths still resolve
//!
//! This crate re-exports the same neutral modules `zeroship-plugin-db`'s
//! `lib.rs` does - `query`, `diff`, `broker`, `read_set`, `encryption`,
//! `budgets`, `lock_policy` - so every `crate::query::…` / `crate::broker::…`
//! reference in a moved file resolves to exactly the item it resolved to
//! before, and the adapter re-exports THIS crate's modules for the same reason.
//! Two crates, one vocabulary, zero call-site churn.

// The engine's async chains nest deeply - a CRUD pass awaiting the routed
// executor awaiting a pooled `compio-postgres` request - and rustc walks the
// whole chain when it computes a block's layout. The adapter carries the same
// attribute for the same reason; it is a compiler resource limit, not a
// correctness guard.
#![recursion_limit = "256"]

zeroship_core::declare_env_consumer!(
    /// The engine tier's own environment reads.
    ///
    /// A LIBRARY consumer, so `target` is the cargo package: this crate is
    /// linked into `zeroship-plugin-db`, which is itself linked into the worker
    /// AND the CLI's `zeroship serve` vector.
    pub DataEngineConsumer,
    target = "zeroship-data-engine",
    scope = "data_engine");

// ---------------------------------------------------------------------------
// The shared vocabulary, re-exported so `crate::…` means the same thing here as
// it does in the adapter.
// ---------------------------------------------------------------------------
// The DDL/DML builders + `QueryError` + `SqlDialect`, in the leaf crate
// `zeroship-schema`.
pub use zeroship_schema::query;
// The diff classifier and the vendor-neutral schema metadata (`MaskMeta`,
// `EncryptionMeta`, `MaskKind`, `Classification`, `LiveSchema`, `ColumnInfo`)
// the mask and encryption passes read off a descriptor.
pub use zeroship_schema::diff;
// The process-wide change broker. Two tiers publish into it - this one on local
// mutation (`exec::emit_local`) and CDC from the WAL - which is why it sits in
// `zeroship-data-core`, below both.
pub use zeroship_data_core::broker;
// The normalised read-set predicate the broker evaluates.
pub use zeroship_data_core::read_set;
// Cross-backend column encryption: `KeyStore`, `LocalKeySource`, the AEAD.
pub use zeroship_data_core::encryption;
// DB-1 execution budgets, named by the PG session SQL renderer AND by
// `transaction/driver.rs`'s cross-backend protocol deadline.
pub use zeroship_data_core::budgets;
// The advisory-lock retry policy both vendors call.
pub use zeroship_data_core::lock_policy;

// ---------------------------------------------------------------------------
// The engine's own modules.
// ---------------------------------------------------------------------------
// Per-app PostgreSQL role provisioning. `bootstrap` is the ENGINE-tier file;
// `mod.rs` carries only the `APP_ROLE_TEMPLATE` anchor it needs and `util` is
// its test-gated helper subtree, so the directory travels as a unit.
pub mod auth;
// The dispatch prelude: `BackendHandle`, both vendors' backends, and the trait
// surface every pass is written against. It was `zeroship-plugin-db`'s
// `backend/mod.rs`, untiered and contested (#170), and the contest was over the
// CDC names in its rustdoc and one `#[cfg(test)]` conformance assertion. The
// assertion moved to `change_stream_pg.rs`, where the fact it pins lives; what
// is left re-exports data-core, both vendors and `zeroship-schema`, plus this
// crate's own `BackendHandle`. Every one of those is at or below this tier.
pub mod backend;
// The per-isolate dispatch enum, separated from `backend/mod.rs` so this cut
// moved a file rather than a definition.
pub mod backend_handle;
// Concrete backend composition: the engine supplies the consumer-side ports the
// process broker implements.
pub mod backend_selection;
// The read and write pipelines.
pub mod crud;
// THE schema authority for the data plane: the runtime descriptor this isolate
// was built from. One resolution function, no `Option`, no catalog read.
pub mod descriptor;
// The routed executor.
pub mod exec;
// Raw usage metrics and the single emit point. The metric NAMES are a billing
// contract shared with `zeroship-metering` and the control plane's pricing
// catalog, not a detail of whichever module happens to run the statement.
pub mod metrics;
// The operator charter the worker parses once at construction.
pub mod system_shape_charter;
// The SC-1 transaction protocol.
pub mod transaction;
// The per-isolate transaction lane owner.
pub mod tx_lanes;
// The tx-vs-pool routing decision.
pub mod tx_route;

/// Reset every piece of ENGINE state this thread owns, plus the descriptor
/// store the engine reads.
///
/// The adapter's `reset_context_for_tests` calls this and then clears the two
/// things it owns itself (the operator pools and the per-isolate context). The
/// split is the point: a helper in the adapter cannot reach `tx_lanes`'
/// thread-local from outside, and a helper here cannot reach the context's.
///
/// Everything cleared is PER-THREAD. There is no process-global state left for
/// it to wipe, and there must not be - a process-global wipe would empty a
/// concurrently running test's entries mid-assertion.
/// Test helper: install one collection's descriptor entry into this isolate's
/// store, so the CRUD passes and the unmask dispatcher's `lookup_mask_meta` /
/// `lookup_encryption_meta` resolve the column metadata. `schema` is the same
/// descriptor-shaped `{ <column>: FieldDef }` map native boot extracts from a
/// `RuntimeSchemaDescriptor` collection.
///
/// The entry lands under the COLD-START binding, which is what every test-side
/// binding is (`crud::write_pipeline`'s fixture, the `_for_tests` seams in
/// `crud`, `v8_classes::transaction`). A fixture that needs two deploys of one
/// app kept apart uses [`cache_schema_for_deploy_for_tests`] instead.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn cache_schema_for_tests(app_id: &str, collection: &str, schema: serde_json::Value) {
    cache_schema_for_deploy_for_tests(
        &zeroship_data_core::binding::DbBinding::cold_start(app_id),
        collection,
        schema,
    );
}

/// Test helper: [`cache_schema_for_tests`] for an explicit binding, so a
/// fixture can install two deploys of one app and assert they do not see each
/// other's descriptor entries.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn cache_schema_for_deploy_for_tests(
    binding: &zeroship_data_core::binding::DbBinding,
    collection: &str,
    schema: serde_json::Value,
) {
    zeroship_data_core::schema_cache::with_mut(|c| c.insert_one(binding, collection, schema));
}

// Test-only: `tracing-subscriber` capture layer for warn/error-shape contract
// tests. It moved here with the engine, and had to: every one of its six call
// sites is an engine file (`crud/{mask_drift,read_pipeline,unmask}.rs`), and
// after the cut `zeroship-plugin-db` had none left.
//
// Gate note: `cfg(test)` only (NOT `any(test, feature = "test-helpers")`)
// because `tracing-subscriber` is a `[dev-dependencies]` entry - it is
// unavailable when a downstream crate compiles this lib with
// `--features test-helpers`, which is non-test compilation from the integration
// target's perspective. Moving `tracing-subscriber` out of dev-deps would
// pollute the release dependency graph.
#[cfg(test)]
mod test_support;

#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn reset_engine_for_tests() {
    tx_lanes::reset_for_tests();
    crud::mask_policy::reset_for_tests();
    metrics::reset_for_tests();
    system_shape_charter::reset_for_tests();
    zeroship_data_core::schema_cache::reset_for_tests();
}
