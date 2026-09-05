//! Database plugin — backs `env.db` with a typed `#[v8_class]` surface.
//!
//! `env.db` is the `Db` v8_class instance (see [`v8_classes::db`]).
//! The creator-facing surface on that wrapper is:
//!
//! - `collection(name)` — mints a [`v8_classes::collection::Collection`]
//!   wrapper for CRUD, vector search, and `openSubscription()`.
//! - `transaction(callback, opts?)` — native transaction orchestrator
//!   that hands the callback a collections-only tx view (the old
//!   `beginTransaction` / `Transaction` wrapper surface was deleted).
//!
//! Platform-internal capabilities such as mask-policy installation and
//! replication do not sit on the public `Db` wrapper; they hang off the
//! private `__platform` capability handle instead.
//!
//! Each wrapper carries a `v8::Weak` guaranteed finalizer that
//! releases its backing resource on GC (broker handle, transaction
//! connection, migration advisory lock).
//!
//! Each app gets its own PostgreSQL schema (`"app_id".*`) for data
//! isolation. The pool is created lazily on first use (one per worker
//! thread).

// The `env.db` surface nests async blocks deeply - a v8_class method awaiting a
// pooled `compio-postgres` request awaiting the connection task's own
// request/response future - and rustc walks that whole chain when it computes a
// block's layout. Two driver changes each added per-request state (a request
// now carries both a disposition and a transaction effect), and together they
// pushed one block in `v8_classes::masked_value` past the default depth of 128.
//
// NEITHER driver change crosses it alone; only a build carrying both does. It
// is therefore invisible when each is checked on its own branch, which is how
// it reached a merge before anything noticed.
//
// `crates/zeroship-gateway/src/main.rs` carries this attribute for the same reason. It
// is a compiler resource limit, not a correctness guard - raising it costs
// compile time and nothing else.
#![recursion_limit = "256"]

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use compio_postgres::Pool;
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

use crate::context::{BackendInitState, with_mut as ctx_mut};
use zeroship_data_core::error::DbError;

// Module visibility note:
//
// Most modules are `pub(crate)` in normal builds. Several are also
// consumed by external test crates under `tests/`, which are compiled
// as separate crate targets. Those need `pub` visibility when the
// `test-helpers` Cargo feature is enabled (the `[[test]] integration`
// target lists `required-features = ["test-helpers"]`).
// The cfg-fork below keeps the release surface tight while exposing
// the modules for tests.
//
// The unconditionally-`pub` modules (`broker`, `error`, `query`,
// `v8_classes`) are reached even without the feature — see
// tests/subscription_finalizer.rs and tests/db_v8_class.rs.
//
// The `auth` module is always compiled; the two-arm ladder below only
// switches its visibility on `test-helpers` so the integration suite
// can probe the auth surface.

zeroship_core::declare_env_consumer!(
    /// The database plugin's own environment reads.
    ///
    /// A LIBRARY consumer, so `target` is the cargo package: this crate is
    /// linked into `zeroship-worker` AND into the CLI's `zeroship serve`
    /// vector, and naming one binary would be a claim the other falsifies.
    pub PluginDbConsumer,
    target = "zeroship-plugin-db",
    scope = "plugin_db");

// `broker` MOVED to `zeroship-data-core` on 2026-09-03, and it is the move the
// CDC extraction was waiting on. Two tiers publish into the broker - the ENGINE
// on local mutation (`exec::emit_local`) and CDC from the WAL
// (`wal_consumer`) - and the ADAPTER subscribes. While it lived here it was
// ENGINE-tier, so `cdc_lifecycle.rs` and `wal_consumer.rs` naming
// `crate::broker` were CDC-to-ENGINE up-edges; extracting data-engine WITH the
// broker inside it would have turned them into a CDC/data-engine cycle. A
// primitive two tiers both use belongs BELOW both.
//
// Re-exported rather than repointed, the same mechanism `budgets`, `encryption`
// and `lock_policy` used: every `crate::broker::*` call site resolves unchanged,
// including the ~30 in `wal_consumer.rs`.
pub use zeroship_data_core::broker;
// `binding` mirrors `backend` below: crate-private in release builds, `pub`
// under `test-helpers` so the integration targets can name the `DbBinding` that
// the descriptor store, the CRUD dispatchers and the search backends are keyed
// by. It carries no behaviour beyond two owned strings.
// `binding` and `error` moved to `zeroship-data-core`, the domain tier. Their
// visibility used to be a cfg-split pair here - `pub(crate)` in a release build,
// `pub` under `test-helpers` - which does not survive a crate boundary: across
// crates `cfg(test)` is the DEFINING crate's test build and never fires for a
// consumer. The domain tier gates the one test-only constructor on the feature
// alone; see `zeroship-data-core/Cargo.toml`.
//
// What stayed is the ADAPTER's half: `op_error` lowers `DbError` into the
// runtime's `OpError`, because `OpError` is a delivery mechanism and a domain
// type may not name one.
pub mod op_error;
// The advisory-lock RETRY POLICY, split out of `backend::LockManager` on
// 2026-09-02 for the same reason `op_error` stayed here: the trait is rank-0
// vocabulary bound for `zeroship-data-core`, and the policy names an executor.
// A default method body travels with its trait, so the split had to happen
// before the trait can move, not after.
// Moved to `zeroship-data-core` on 2026-09-02: both vendor crates call it, so it
// cannot live above them. Re-exported rather than repointed, so
// `crate::lock_policy::BoundedLockAcquire` and
// `zeroship_plugin_db::lock_policy::...` both still resolve.
pub use zeroship_data_core::lock_policy;
// The DDL builders + `QueryError` + `SqlDialect` +
// the system-field / validation helpers were extracted into the leaf crate
// `zeroship-schema`. plugin-db re-exports the module wholesale so every
// existing `crate::query::…` reference (and `use crate::query;` then
// `query::…`) resolves unchanged — behaviour identical, no call-site churn.
pub use zeroship_schema::query;
pub mod v8_classes;

// ---------------------------------------------------------------------------
// THE ENGINE TIER LEFT FOR `zeroship-data-engine` ON 2026-09-03.
// ---------------------------------------------------------------------------
//
// `crud`, `transaction`, `exec`, `backend`, `backend_handle`,
// `backend_selection`, `tx_route`, `tx_lanes`, `metrics`,
// `system_shape_charter`, `descriptor` and `auth` are all re-exported below
// rather than repointed, the mechanism `budgets`, `encryption`, `lock_policy`,
// `broker` and `read_set` used before them: every `crate::crud::…` /
// `crate::backend::…` call site in the modules that STAYED resolves unchanged,
// and so does every `zeroship_plugin_db::…` path in the 12 integration targets.
//
// The two-arm `#[cfg(not(feature = "test-helpers"))] pub(crate) use` /
// `#[cfg(feature = "test-helpers")] pub use` ladders describe THIS crate's
// release surface and are kept for the modules that had one. They no longer
// govern the ENGINE's own visibility - a re-export cannot widen what it
// re-exports, so the items behind them are `pub` in `zeroship-data-engine`,
// gated there on ITS `test-helpers` feature, which this crate's feature turns
// on transitively.
//
// `backend` is crate-private by default; under `test-helpers` it becomes `pub`
// so the integration-test targets (`tests/sqlite_integration.rs` in particular)
// can name `backend::SqliteBackend` + the `SqlExecutor` trait directly.
#[cfg(not(feature = "test-helpers"))]
pub(crate) use zeroship_data_engine::backend;
#[cfg(feature = "test-helpers")]
pub use zeroship_data_engine::backend;
// The engine owns concrete backend composition because it supplies the
// consumer-side ports implemented by the process broker. Integration targets
// reach the same production composition under `test-helpers`.
#[cfg(not(feature = "test-helpers"))]
pub(crate) use zeroship_data_engine::backend_selection;
#[cfg(feature = "test-helpers")]
pub use zeroship_data_engine::backend_selection;
// The per-isolate dispatch enum and the transaction lane owner. Crate-private
// here: nothing outside names either, and the tx-route TYPE has to be nameable
// wherever the `exec` entry points are, which is why only `tx_route` is `pub`.
pub(crate) use zeroship_data_engine::{backend_handle, tx_lanes};
pub use zeroship_data_engine::tx_route;
// The operator charter the worker parses once at construction. `pub(crate)`
// because nothing outside the crate has business reading the assignment
// authority - the descriptor mirror is what consumers verify against.
pub(crate) use zeroship_data_engine::system_shape_charter;
// THE schema authority for the data plane: the runtime descriptor this isolate
// was built from. One resolution function, no `Option`, no catalog read.
pub(crate) use zeroship_data_engine::descriptor;
// Raw usage metrics and the single emit point.
pub(crate) use zeroship_data_engine::metrics;
pub(crate) mod context;
/// This isolate's column-key source, for callers that construct a backend.
///
/// A backend constructor takes its key source as a PARAMETER - the per-isolate
/// context is ENGINE state, and once the backends are their own crates they
/// cannot name the crate that depends on them. This is the one function that
/// reads it, exported so a caller holding a pool can pass it in. The engine
/// composer cannot do the passing for the Postgres arm: taking the pool would
/// put `compio_postgres::Pool` in an engine signature, which
/// `tests/vendor_embedding_gate.sh` refuses.
#[cfg(any(test, feature = "test-helpers"))]
pub fn isolate_key_source() -> encryption::LocalKeySource {
    context::isolate_key_source()
}
/// This isolate's descriptor slice for one collection, for callers that drive a
/// search backend directly.
///
/// Exported for the same reason as [`isolate_key_source`] directly above: the
/// search traits take the schema as a PARAMETER now, and a caller outside this
/// crate has to be able to resolve the value the engine would have passed.
/// Passing `Value::Null` instead is NOT equivalent - the read pipeline refuses
/// it with `invalid_filter` / "read schema must be a field-map object", which
/// is how the integration suite caught the substitution.
#[cfg(any(test, feature = "test-helpers"))]
pub fn collection_schema(
    binding: &zeroship_data_core::binding::DbBinding,
    collection: &str,
) -> Result<std::sync::Arc<serde_json::Value>, DbError> {
    descriptor::collection_schema(binding, collection)
}
// `cross_app_fk` WAS DECLARED HERE and is deleted (2026-09-02), under the
// split's Phase 0.5 dead-code decision: "giving dead code a crate is how the
// existing clusters got there". 235 lines and eleven tests, exported `pub` and
// UNGATED - so it shipped in every binary - with zero production callers; its
// own rustdoc said "in a default build, nobody".
//
// It could not be wired where it lived. Decision 10 removed all DDL from this
// crate, so no path here sees a `refTarget` before DDL any more. What refuses
// `other_app.users` today is the charset rule in the engine's `validate_ident`
// (`zeroship-migrate-core/src/render/declarative.rs`), which `validate_desired`
// runs over every desired table name; a dot is neither alphanumeric nor `_`.
// Its two surviving tests moved there as `bare_identifier_tests`, because
// nothing had ever asserted that refusal.
//
// Do NOT read the engine's `validate_cross_app_fk_targets` as the replacement.
// That is a dangling-target check and explicitly PERMITS a target owned by
// another member app - the opposite policy, under a confusingly similar name.
// `crud` is crate-private in release builds; `pub`
// under `test-helpers` so `tests/sqlite_integration.rs` can reach
// `crud::encryption_pass::{encrypt_row_on_write, decrypt_row_on_read}`
// for the end-to-end encrypted-column CRUD round-trip test. Same shape
// as `encryption` below.
#[cfg(not(feature = "test-helpers"))]
pub(crate) use zeroship_data_engine::crud;
#[cfg(feature = "test-helpers")]
pub use zeroship_data_engine::crud;
// The diff classifier (`compute_diff`, `ChangeKind`, `ChangeClass`, `DiffOp`)
// and vendor-neutral schema metadata (`MaskMeta`, `EncryptionMeta`, `MaskKind`,
// `Classification`, `WrappedType`, `LiveSchema`, `ColumnInfo`) live in the leaf
// crate `zeroship-schema`. plugin-db re-exports that neutral module so every
// `crate::diff::…` reference resolves unchanged. PostgreSQL catalog reads live
// separately in `backend::pg_introspect`; no old schema-crate path is retained.
// The original `pub(crate)` vs `pub` (under `test-helpers`) visibility is
// preserved by the cfg gate; integration suites reach neutral `diff::{…}`
// values only under `test-helpers`.
#[cfg(not(feature = "test-helpers"))]
pub(crate) use zeroship_schema::diff;
#[cfg(feature = "test-helpers")]
pub use zeroship_schema::diff;
// `read_set` MOVED to `zeroship-data-core` on 2026-09-03, ahead of `broker`,
// which is the only in-crate item it had to shed before the broker could follow
// it. The read-set is a domain value - a normalised predicate over a row - and
// it had to go below the ENGINE because the BROKER evaluates it and the broker
// is named by two tiers at once.
//
// Re-exported rather than repointed, the same mechanism `budgets`, `encryption`
// and `lock_policy` used, so every `crate::read_set::*` call site resolves
// unchanged.
//
// The two-arm ladder is kept because it describes THIS crate's release surface,
// matching `diff` above: the mask-flip suite asserts that a predicate on a
// masked column is LOWERED rather than compared as written, and that assertion
// has to reach `normalise_filter`'s output. The production visibility is
// unchanged.
#[cfg(not(feature = "test-helpers"))]
pub(crate) use zeroship_data_core::read_set;
#[cfg(feature = "test-helpers")]
pub use zeroship_data_core::read_set;
pub(crate) mod v8_bridge;

// DB-1 execution budgets MOVED to `zeroship_data_core::budgets` on 2026-09-02.
// They are named by a vendor tier (`backend/pg_session_sql.rs`, which renders
// them into PostgreSQL GUCs) AND by the engine (`transaction/driver.rs`, whose
// cross-backend protocol deadline is derived from one of them). Two tiers
// naming one module is what puts it at rank 0, below both.
//
// Re-exported here rather than left as a path change for callers to chase: the
// live suites assert against these guards by name, and `zeroship-plugin-db`
// remains their public surface until the tiers themselves are crates.
pub use zeroship_data_core::budgets;
// Process-wide ownership of the `env.db` primitive: validated configuration,
// the plugin prototype, the stable thread-resource key, and the neutral
// operator-lifecycle handle.
pub mod service;

// Cross-backend column-encryption surface. Always
// compiled (not gated to `pg` / `sqlite`) because both backends
// consume it. The pure-Rust crypto module + trait surface underpin
// the backend impls and CRUD call sites for both PG and SQLite. See
// `docs/archive/p5-encryption-backup-implementation-plan.md` §9.
//
// Visibility: crate-private in release builds; `pub` under
// `test-helpers` so `tests/integration.rs` can reach
// `encryption::canonical_aad` etc. for the round-trip + row-swap
// fences.
// `encryption` MOVED to `zeroship-data-core` on 2026-09-02, and had to: both
// backends name `KeyStore` and `LocalKeySource`, so extracting either vendor
// while this module lived here would have made the vendor crate depend on the
// adapter that depends on it.
//
// Re-exported rather than repointed, so every `crate::encryption::…` call site
// resolves unchanged - the mechanism `budgets`, `capability` and `storage` used
// before it. The two-arm ladder is kept because it describes THIS crate's
// release surface: `pub(crate)` normally, `pub` under `test-helpers` so
// `tests/integration.rs` can reach `canonical_aad` for the round-trip fences.
#[cfg(not(feature = "test-helpers"))]
pub(crate) use zeroship_data_core::encryption;
#[cfg(feature = "test-helpers")]
pub use zeroship_data_core::encryption;

// `change_stream_pg` is the PG-arm adapter for the `ChangeStream`
// capability declared in `crate::backend::mod`. The adapter borrows
// `PostgresBackend` and the underlying replication helpers
// (`replication.rs` / `wal_consumer.rs`) are PG-only.
//
// **The consumer surface used to be `BackendHandle::as_change_stream_pg`, and
// that accessor is deleted.** It made the dispatch enum - a data-engine type -
// name this CDC module, so `backend_handle.rs` could not move to data-engine
// without dragging CDC along (#163). Callers now construct
// `PgChangeStream::new` directly from the `Rc<PostgresBackend>` they already
// hold, which is the direction that works: CDC composing over the vendor arm.
//
// Test visibility follows the `encryption` idiom directly above: crate-private
// in a shipped build, `pub` under `test-helpers` so `tests/integration.rs` can
// build a consumer without the enum growing an accessor for its benefit.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod change_stream_pg;
#[cfg(feature = "test-helpers")]
pub mod change_stream_pg;

// Process-wide owner for per-app CDC consumers. Native Subscription wrappers
// acquire leases here so all isolates in one worker share one logical slot.
mod cdc_lifecycle;

// `mod audit` is DELETED, name included. It owned the per-app
// `__zeroship_migrations` table, which despite the name was never a
// migration record: it was the provenance log for the DDL the data plane
// itself issued (`create_index_with_recovery_audited`, the two
// `ensure_*_index` hooks). With that DDL gone the log has nothing to
// record, and it went in the same change rather than before it -
// dropping the log first would have kept the writer while losing the
// provenance the log existed to give it.

// The `auth` module is always compiled: `auth::bootstrap` carries the
// per-app PG role machinery the data plane runs on every transaction.
// The `keys` / `session` submodules and the `__zeroship_admin` schema they
// spoke to are deleted -- see `auth/mod.rs` for why they are not coming back.
// `auth::util` was the shared-helper subtree the SQLite `SessionMinter` impl
// reused; that impl was deleted on 2026-09-02 and `util` followed it on
// 2026-09-04, having had no consumer in any crate or cfg in between.
#[cfg(not(feature = "test-helpers"))]
pub(crate) use zeroship_data_engine::auth;
#[cfg(feature = "test-helpers")]
pub use zeroship_data_engine::auth;

#[cfg(not(feature = "test-helpers"))]
pub(crate) use zeroship_data_engine::exec;
#[cfg(feature = "test-helpers")]
pub use zeroship_data_engine::exec;

// COMPILED ONLY INTO TEST BUILDS, and that is now structural rather than
// documented. `drop_namespace` has no production caller anywhere in the
// workspace - measured 2026-09-02 across every crate, excluding tests and its
// own file: zero call sites. `zeroship-control`'s
// `cron/workflow_engine.rs` says so independently ("plugin-db's currently
// unwired drop_namespace"). It is exercised only by the PG integration suite.
//
// The module carried `#![allow(dead_code)]` and a comment saying to remove the
// allow "when a database-keyed migrate-server coordinator owns the call". That
// comment was accurate and is the reason this is a GATE rather than a deletion:
// the code is wanted, its caller is not built yet. Gating rather than allowing
// makes the fact checkable - a production build no longer contains a
// `DROP SCHEMA` / `DROP ROLE` path at all - and takes the module's
// `compio_postgres` coupling out of the engine tier the split has to extract.
#[cfg(any(test, feature = "test-helpers"))]
pub mod drop_namespace;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod replication;
#[cfg(feature = "test-helpers")]
pub mod replication;

/// Operator-owned cleanup for abandoned worker replication slots.
pub mod slot_reaper;

#[cfg(not(feature = "test-helpers"))]
pub(crate) use zeroship_data_engine::transaction;
#[cfg(feature = "test-helpers")]
pub use zeroship_data_engine::transaction;

// Async-scoped transaction marker. Read by `transaction` to tell a
// genuinely NESTED `transaction()` call from one that merely overlaps
// another in time; see the module docs for the defect that distinction
// closes.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod tx_scope;
#[cfg(feature = "test-helpers")]
pub mod tx_scope;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod wal_consumer;
#[cfg(feature = "test-helpers")]
pub mod wal_consumer;

// `test_support` - the `tracing-subscriber` capture layer for warn/error-shape
// contract tests - LEFT for `zeroship-data-engine` on 2026-09-03, and it left
// because every one of its call sites did. Measured before the move: six, all
// in `crud/{mask_drift,read_pipeline,unmask}.rs`, and zero anywhere else in this
// crate. It is not re-exported: it is `cfg(test)` in the engine, so it is
// invisible here by construction, and a warn-shape assertion about an adapter
// event would need it rebuilt against `tracing_subscriber` as a dev-dependency
// of THIS crate.

// The synchronous `ensure_pool(scope)` helper that used to live here
// has been removed — every callback dispatches through
// `init_pool_async()` + `context::with_mut(...)` directly (or the
// `exec::ensure_pool` async helper that wraps the same).

// ---------------------------------------------------------------------------
// DbPlugin
// ---------------------------------------------------------------------------

/// The database plugin — registers `zeroship.db.*` methods.
///
/// **The prototype, not a per-runtime object.** One instance is minted by
/// [`service::DbService::new`] at composition and every runtime on every worker
/// thread clones the same `Arc`. There is deliberately no public constructor:
/// a `DbPlugin` is a view of validated service configuration, and minting one
/// beside the service would be a second, unvalidated configuration.
pub struct DbPlugin {
    url: String,
    /// Stable across every isolate in one worker process and distinct across
    /// worker containers. Used to derive the process's per-app CDC slot.
    worker_id: String,
    /// Process-wide usage meter (metering-as-infrastructure). Stamped into
    /// the per-isolate context on `register`; the exec boundary emits
    /// `db_reads` / `db_writes` / `db_rows_written` through it on success.
    /// `None` in meter-less test harnesses.
    meter: Option<std::sync::Arc<zeroship_metering::Meter>>,
    /// The service's stable thread-resource key. Stamped into the thread
    /// context on `register`, so every isolate this plugin serves — current and
    /// deploy-pinned alike — resolves resources under one identity.
    resource_key: service::DbResourceKey,
    /// The backend the service selected at composition. Carried so lazy pool
    /// init reads a decision rather than re-parsing the URL.
    backend: BackendUrl,
    /// The operator's assignment authority, parsed once at composition.
    ///
    /// Held here rather than re-parsed per request, and held on the PROTOTYPE
    /// rather than per-isolate, because it is identical for every app this
    /// process serves - it is compiled into the binary, not derived from any
    /// app's descriptor.
    ///
    /// Read by [`Self::register`], which projects it into the per-worker-thread
    /// context as a [`system_shape_charter::AssignmentPlan`]. The write pass
    /// iterates that projection and names no column of its own.
    system_shape_charter: zeroship_migrate_policy::RootCharter,
}

impl std::fmt::Debug for DbPlugin {
    /// No URL — it carries a password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbPlugin")
            .field("resource", &self.resource_key)
            .finish_non_exhaustive()
    }
}

impl DbPlugin {
    /// Mint the prototype. Crate-private: [`service::DbService::new`] is the
    /// only caller, and it has already validated the configuration.
    pub(crate) fn new(
        url: String,
        worker_id: String,
        meter: Option<std::sync::Arc<zeroship_metering::Meter>>,
        resource_key: service::DbResourceKey,
        backend: BackendUrl,
        system_shape_charter: zeroship_migrate_policy::RootCharter,
    ) -> Self {
        Self {
            url,
            worker_id,
            meter,
            resource_key,
            backend,
            system_shape_charter,
        }
    }

    /// The operator's assignment authority for this process.
    ///
    /// Every consumer that needs to know who assigns a column's value reads it
    /// from here. It deliberately has no setter and no descriptor-derived
    /// alternative: the descriptor is creator-authored, so it may mirror this
    /// but may never replace it.
    pub(crate) fn system_shape_charter(&self) -> &zeroship_migrate_policy::RootCharter {
        &self.system_shape_charter
    }
}

impl NativePlugin for DbPlugin {
    fn namespace(&self) -> &str {
        "db"
    }

    fn name(&self) -> &str {
        "database"
    }

    /// Mint a `Db` v8_class instance as the namespace value for
    /// `env.db`. The runtime then attaches the Db-scoped entry points
    /// registered via [`Self::register`] on top. The `.collection(name)`
    /// `#[v8_method]` on the instance returns a `Collection` v8_class
    /// wrapper whose CRUD methods call `crud::dispatch_*` directly.
    fn build_instance<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        v8_classes::db::mint_db(scope, app_id)
    }

    fn bind_runtime_descriptor(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        app_id: &str,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        // The runtime validates the complete descriptor before invoking this
        // hook. Collect every field map before mutating the shared thread
        // context anyway, so a future validator change cannot publish a
        // partial schema on error.
        let schemas = descriptor_schemas(descriptor)?;
        // Same refusal as `mint_db`: an app id that is not a legal schema name
        // has no binding to key the descriptor under, so publish nothing rather
        // than key it under a schema that cannot be addressed.
        let binding = v8_classes::db::binding_for_isolate(scope, app_id).ok_or_else(|| {
            format!("app id {app_id:?} is not a legal database schema name")
        })?;
        zeroship_data_core::schema_cache::with_mut(|c| c.replace_for_binding(&binding, schemas));
        Ok(())
    }

    fn register(&self, r: &mut NativeRegistrar) {
        // Poison the URL thread-local so `ensure_pool_initialized`
        // (invoked lazily on first callback) can find it. `register()`
        // may fire multiple times per thread in multi-tenant workers —
        // idempotent overwrite is intentional.
        //
        // Invariant: in today's production each worker thread hosts a single
        // DB URL, so the `different` branch is a no-op. It exists for the
        // multi-URL-per-thread case: when two DbPlugin instances with
        // distinct URLs register on the same thread, we must drop any
        // previously-created pool so `init_pool_async` / `ensure_pool`
        // build a fresh one for the new URL instead of silently aliasing
        // the first pool to the second URL.
        ctx_mut(|c| {
            if c.install_db_resources(&self.url, self.resource_key, self.backend.clone()) {
                c.clear_pool();
            }
            c.set_cdc_worker_id(&self.worker_id);
            // Stamp the process-wide meter so the exec boundary can emit a
            // per-app usage metric on each successful op.
        });
        // The meter is stamped on the metrics module, not parked in the context:
        // that module is its only reader. Outside the `ctx_mut` borrow above for
        // the same reason the charter stamp is.
        metrics::stamp(self.meter.clone());
        ctx_mut(|c| {
        });
        // Stamp the operator charter's assignment projection, so the write pass
        // reads the authority this process was composed with rather than
        // deriving one on its first write.
        //
        // OUTSIDE the `ctx_mut` closure, and on the charter's own thread-local:
        // the projection is an engine-owned value that the adapter merely
        // composes, so the adapter calls DOWN to store it rather than parking it
        // in its own struct for the engine to reach back up for.
        system_shape_charter::stamp(std::rc::Rc::new(
            system_shape_charter::AssignmentPlan::from_charter(self.system_shape_charter()),
        ));
        // Every JS-visible entry point lives on the Db v8_class wrapper
        // (see `v8_classes::db`).
        let _ = r;
    }
}

fn descriptor_schemas(
    descriptor: Option<&serde_json::Value>,
) -> Result<Vec<(String, serde_json::Value)>, String> {
    let Some(descriptor) = descriptor else {
        return Ok(Vec::new());
    };
    let collections = descriptor
        .get("collections")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "descriptor has no object `collections` field".to_string())?;

    collections
        .iter()
        .map(|(name, collection)| {
            let fields = collection
                .get("fields")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    format!("descriptor collection {name:?} has no object `fields` field")
                })?;
            Ok((name.clone(), serde_json::Value::Object(fields.clone())))
        })
        .collect()
}

#[cfg(test)]
mod runtime_descriptor_binding_tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use serde_json::json;
    use zeroship_runtime::{RuntimeState, SharedState, init_v8};

    use super::*;

    const APP: &str = "app_native_descriptor";
    const DEPLOY: &str = "deploy_native_descriptor";

    fn install_runtime_state(scope: &mut v8::PinScope<'_, '_>) {
        let mut env = HashMap::new();
        env.insert("APP_ID".to_string(), APP.to_string());
        env.insert("ZEROSHIP_DEPLOY_ID".to_string(), DEPLOY.to_string());
        let state: SharedState = Rc::new(RefCell::new(RuntimeState::new(env, None, None)));
        scope.set_slot(state);
    }

    fn plugin() -> std::sync::Arc<DbPlugin> {
        service::DbService::new(service::DbServiceConfig {
            url: "sqlite::memory:".to_string(),
            worker_id: "worker_descriptor_test".to_string(),
            meter: None,
        })
        .expect("db service")
        .plugin()
    }

    #[test]
    fn validated_runtime_descriptor_makes_declared_collection_serveable() {
        reset_context_for_tests();
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        install_runtime_state(scope);

        let runtime_descriptor = json!({
            "version": 2,
            "collections": {
                "users": {
                    "fields": {
                        "id": { "type": "id", "idPrefix": "usr" },
                        "email": { "type": "string", "required": true }
                    },
                    "options": {
                        "softDelete": false,
                        "versioning": true,
                        "strictness": "strict"
                    },
                    "indexes": []
                }
            }
        });
        plugin()
            .bind_runtime_descriptor(scope, APP, Some(&runtime_descriptor))
            .expect("bind descriptor");

        let binding = zeroship_data_core::binding::DbBinding::new(
            APP,
            DEPLOY,
            zeroship_schema::SchemaName::new(APP).unwrap(),
        );
        let schema = descriptor::collection_schema(&binding, "users")
            .expect("declared collection must resolve before any read");
        assert_eq!(
            schema.as_ref(),
            &runtime_descriptor["collections"]["users"]["fields"],
            "native boot must publish the descriptor's field map verbatim"
        );
    }

    #[test]
    fn schema_less_runtime_replaces_binding_with_an_empty_view() {
        reset_context_for_tests();
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        install_runtime_state(scope);

        let runtime_descriptor = json!({
            "version": 2,
            "collections": {
                "stale": {
                    "fields": { "id": { "type": "id" } },
                    "options": { "softDelete": false, "versioning": false },
                    "indexes": []
                }
            }
        });
        let plugin = plugin();
        plugin
            .bind_runtime_descriptor(scope, APP, Some(&runtime_descriptor))
            .expect("bind descriptor");
        plugin
            .bind_runtime_descriptor(scope, APP, None)
            .expect("bind schema-less runtime");

        let binding = zeroship_data_core::binding::DbBinding::new(
            APP,
            DEPLOY,
            zeroship_schema::SchemaName::new(APP).unwrap(),
        );
        let error = descriptor::collection_schema(&binding, "stale")
            .expect_err("schema-less binding must declare no collection");
        assert!(
            format!("{error:?}").contains("collection_not_declared"),
            "missing descriptor entry must remain a loud typed refusal: {error:?}"
        );
        assert!(
            descriptor::declared_collections(&binding).is_empty(),
            "schema-less transaction view source must be empty"
        );
    }
}

// The two bench entry points live in `backend::pg_row_json`, beside the decoders
// they measure, and are re-exported here so `zeroship_plugin_db::…_for_bench`
// keeps resolving for the Criterion targets and the integration test.
//
// They were DEFINED here until 2026-09-01, which put `&compio_postgres::Row`
// into two always-compiled `pub fn` signatures in the crate whose whole claim is
// that it is a thin Rust/V8 seam - a seam cannot link a database driver. A
// `pub use` names no type, so the re-export costs the adapter nothing. When
// `pg_row_json` becomes `data-postgres`, the benches move with it and this line
// is deleted rather than rewritten.
#[doc(hidden)]
pub use backend::pg_row_json::{first_row_or_null_for_bench, row_to_json_for_bench};

/// **Test-only**: install this thread's DB resources directly, bypassing the
/// usual `DbService` → `DbPlugin::register()` path. Used by integration tests
/// that drive DB ops directly without spinning up a full runtime.
///
/// It derives the same resource key `DbService` would from the same URL, so a
/// harness and a composed service address one identity - a harness that made up
/// its own key would silently get a private slice of the process-wide cache and
/// every cross-thread assertion would pass vacuously.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn set_db_url_for_tests(url: &str) {
    let key = service::DbResourceKey::for_url(url);
    let backend = service::select_backend(url).expect("test URL must be a supported backend");
    ctx_mut(|c| {
        c.install_db_resources(url, key, backend);
    });
}

/// **Test-only**: install a concrete Postgres pool/backend into the
/// per-thread context. Used by integration tests that need to force the
/// plugin onto a non-default login role without waiting for lazy init to
/// rebuild from a prior test's URL.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn set_postgres_pool_for_tests(pool: Rc<compio_postgres::Pool>, url: &str) {
    let key = service::DbResourceKey::for_url(url);
    let backend = service::select_backend(url).expect("test URL must be a supported backend");
    ctx_mut(|c| {
        c.install_db_resources(url, key, backend);
    });
    // The key source is read AFTER the borrow above is released, not inside it:
    // `isolate_key_source` takes a context borrow of its own, and nesting the
    // two panics. Production reaches the same shape through
    // `PostgresBackend::connect`, which is handed the source by its caller.
    let pg = crate::backend::PostgresBackend::new(pool, url.to_string(), context::isolate_key_source());
    ctx_mut(|c| c.set_postgres_backend(Rc::new(pg)));
}

/// **Test-only**: drop everything the per-thread context holds, including
/// the pool and any parked transaction client.
///
/// Those handles own live Postgres connections. A test that leaves them in
/// the context leaves the connections open, and because releasing a
/// connection is asynchronous they are then orphaned when the test's runtime
/// goes away. Clearing the context first lets the connections close while
/// there is still a runtime to close them.
///
/// Also drops this thread's operator pools, which own live connections for the
/// same reason.
///
/// Everything it clears is PER-THREAD: the descriptor store, the pool, the
/// parked transaction client, the mask-policy cache. There is no process-global
/// state left for it to wipe, and there must not be - `drain_pg()` is the
/// teardown of essentially every Postgres integration test, and a test binary
/// is multi-threaded unless the invocation says otherwise, so a process-global
/// wipe here would empty a concurrently running test's entries mid-assertion.
/// A fixture that needs its entries kept apart from another's takes its own
/// identity ([`binding::DbBinding`]'s deploy token, or a fixture-specific URL
/// and therefore [`service::DbResourceKey`]) rather than emptying a shared map.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn reset_context_for_tests() {
    service::close_operator_pools();
    ctx_mut(|c| *c = context::ThreadDbContext::new());
    // BOTH thread-locals, since the lanes became their own owner on 2026-09-02.
    //
    // **The reason is WITHIN one test, not between two.** This comment first
    // said resetting only the context would leak lanes "to the next test on the
    // same thread". That is false: libtest gives every `#[test]` its own OS
    // thread even under `--test-threads=1` (measured 2026-09-01), so a
    // `thread_local!` cannot leak across tests at all.
    //
    // What is real: this helper is called MID-TEST - by scenario setup that
    // resets between phases, and by the `ContextReset` drop guard in
    // `transaction/mod.rs`. A reset that cleared the context and left the lanes
    // would hand the next phase a stale transaction claim and a parked session,
    // inside one test.
    tx_lanes::reset_for_tests();
    crud::mask_policy::reset_for_tests();
    metrics::reset_for_tests();
    system_shape_charter::reset_for_tests();
    zeroship_data_core::schema_cache::reset_for_tests();
}

/// Test helper: hand this isolate the column root keys its backends
/// should resolve `t.encrypted(...)` columns from, instead of
/// `ZEROSHIP_COLUMN_KEY_<KEYID>`.
///
/// Each entry is `(key_id, root_hex)` where `root_hex` is 64 hex
/// characters. The keys land in the per-isolate context
/// (`ThreadDbContext::set_supplied_root_keys`), so every backend
/// constructed on this thread AFTERWARDS picks them up - including the
/// ones a test never sees, such as the `SqliteBackend` that
/// `init_pool_async` builds behind a V8 dispatch and the
/// `PostgresBackend` that `set_pool` builds. That is the whole point:
/// the process environment was the only channel that reached those, and
/// mutating it is process-global, racy, and `unsafe`. A thread-local is
/// none of the three, so two tests running in parallel on different
/// threads cannot see each other's roots.
///
/// An EMPTY slice installs a source that provably holds no key, which is
/// how a test asserts the `column_key_not_configured` error without
/// depending on what the ambient environment happens to contain.
///
/// The returned guard withdraws the whole source on drop, so a test's
/// roots do not leak into the next test that runs on the same thread.
/// Hold it for the body of the test (`let _keys = ...;`).
///
/// # Panics
/// If any `root_hex` is not 64 hex characters. A malformed fixture key
/// is a test bug, and failing here names the key id instead of
/// surfacing as a decrypt failure later.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
#[must_use]
pub fn supply_root_keys_for_tests(roots: &[(&str, &str)]) -> SuppliedRootKeysGuard {
    let keys = Rc::new(crate::encryption::SuppliedRootKeys::new());
    for (key_id, hex) in roots {
        keys.insert_hex(key_id, hex)
            .unwrap_or_else(|e| panic!("fixture root key '{key_id}' must parse: {e:?}"));
    }
    ctx_mut(|c| c.set_supplied_root_keys(Some(Rc::clone(&keys))));
    SuppliedRootKeysGuard { _keys: keys }
}

/// Guard returned by [`supply_root_keys_for_tests`]. Withdraws the
/// isolate's supplied root keys on drop.
///
/// Opaque on purpose: the source itself stays reachable only through the
/// context, so a test cannot hold a root alive past the guard.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
#[derive(Debug)]
pub struct SuppliedRootKeysGuard {
    _keys: Rc<crate::encryption::SuppliedRootKeys>,
}

#[cfg(any(test, feature = "test-helpers"))]
impl Drop for SuppliedRootKeysGuard {
    fn drop(&mut self) {
        ctx_mut(|c| c.set_supplied_root_keys(None));
    }
}

/// Test helper: install a `SqliteBackend` into the per-
/// isolate context so the unmask integration suite can drive
/// `crud::unmask::dispatch_unmask` against a freshly-constructed
/// backend without the full V8 runtime + plugin wiring.
///
/// Production code reaches the SQLite arm through the
/// `DbPlugin::build_instance` path; this helper short-circuits that
/// for SQLite-only integration tests in `tests/sqlite_integration.rs`.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn set_sqlite_backend_for_tests(backend: Rc<crate::backend::sqlite::SqliteBackend>) {
    ctx_mut(|c| c.set_sqlite_backend(backend));
}

// The two descriptor-store fixtures MOVED to `zeroship-data-engine` with the
// engine tier: they touch `zeroship_data_core::schema_cache` and nothing else,
// and the CRUD passes that call them are the engine's. Re-exported here so the
// integration targets and this crate's own tests keep the same path.
#[cfg(any(test, feature = "test-helpers"))]
pub use zeroship_data_engine::{cache_schema_for_deploy_for_tests, cache_schema_for_tests};

/// Test helper: clear the per-isolate mask-policy cache
/// entry for `app_id`. Used by `tests/sqlite_integration.rs` to
/// guarantee a clean slate between policy-driven unmask tests — the
/// per-isolate thread-local cache is process-wide and would otherwise
/// bleed state across test functions running on the same OS thread
/// (the `--test-threads=1` scenario, and also single-runtime tests).
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn clear_mask_policy_cache_for_tests(app_id: &str) {
    crud::mask_policy::cache_put(app_id, None);
}

/// **Test-only**: run the write-side prep an insert dispatch performs before the
/// engine sees the docs (encrypt + lower + system fields), without an isolate.
///
/// It lives HERE and not beside `crud::prepare_insert_many_docs_for_binding`,
/// which it calls, because resolving the key store and the dialect is the
/// DISPATCHER's frame: `tx_scope` is adapter state, so an engine module reading
/// it is an ENGINE-to-ADAPTER call. `test-helpers` is a normal cargo feature, so
/// such a call compiles into the library; the gate only hides it from
/// `tests/lib/tier_direction_census.sh`, which excises gated items. Cargo will
/// not be as forgiving once the engine is its own crate. Same argument, same
/// words, at `exec::ambient_route_for_tests`.
///
/// The binding is a COLD START, matching every other `_for_tests` seam here.
#[cfg(feature = "test-helpers")]
#[doc(hidden)]
pub async fn prepare_insert_many_docs_for_tests(
    docs: &mut serde_json::Value,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    let binding = zeroship_data_core::binding::DbBinding::cold_start(app_id);
    let backend = tx_scope::ensure_backend().await?;
    let dialect = tx_scope::configured_dialect();
    // The route the V8 dispatcher would have captured. The protection-floor
    // fence reads the live catalog, so the helper has to stand in for that half
    // of the dispatcher's frame too - a helper that skipped it would let a test
    // write through a fence production applies.
    let route = crate::exec::ambient_route_for_tests(app_id, backend.clone());
    crud::prepare_insert_many_docs_for_binding(
        backend.key_store(),
        dialect,
        &route,
        docs,
        &binding,
        collection,
        actor_id,
    )
    .await
}

/// **Test-only**: drive the REAL read pipeline (`crud::read_pipeline::apply`
/// with default options: decrypt + mask-wrap on) over a set of freshly-fetched
/// rows, so a round-trip test exercises the descriptor-sourced decrypt +
/// mask-wrap path end to end rather than an AEAD-unit shim. Returns the
/// finalized rows; `has_masked` is dropped (the caller asserts on contents).
///
/// Adapter-side for the reason spelled out on
/// [`prepare_insert_many_docs_for_tests`]: resolving the backend is the
/// dispatcher's job, and this helper is standing in for the dispatcher.
#[cfg(feature = "test-helpers")]
#[doc(hidden)]
pub async fn finalize_rows_on_read_for_tests(
    app_id: &str,
    collection: &str,
    rows: Vec<serde_json::Value>,
) -> Result<Vec<serde_json::Value>, DbError> {
    let binding = zeroship_data_core::binding::DbBinding::cold_start(app_id);
    let backend = tx_scope::ensure_backend().await?;
    // The route comes from the ambient parked-tx slot rather than from a V8
    // scope, because there is no isolate here - the same trade
    // `exec_mutation_with_emit_for_tests` below documents. `apply` needs a
    // route, not a handle: its unmask stage issues SELECTs of its own and they
    // must land on the lane the read that produced these rows ran on.
    let result = crud::read_pipeline::apply(
        &exec::ambient_route_for_tests(app_id, backend),
        &binding,
        collection,
        rows,
        crud::read_pipeline::ApplyOptions::default(),
    )
    .await?;
    Ok(result.rows)
}

/// **Test-only**: end-to-end wrapper around `exec::exec_mutation_with_emit` so
/// integration tests can drive the queue/drain machinery against a real
/// Postgres connection without spinning up a V8 isolate.
///
/// **This lived in `exec.rs` until the engine became its own crate**, and had
/// to move for the same reason as its two neighbours above: resolving the
/// backend is `tx_scope::ensure_backend`'s job, and that is adapter-side. The
/// engine keeps `exec::ambient_route_for_tests`, which now TAKES the handle.
///
/// The route is derived from the ambient parked-tx slot rather than from a V8
/// scope, because there is no isolate here. That is exactly the discriminator
/// production no longer uses; it is sound ONLY because a test drives one unit
/// of work at a time, and it is why this helper cannot stand in for the
/// `cxPlain` coverage in `tests/e2e_dev_vs_deployed_db.sh`.
///
/// # Errors
///
/// The statement's own error, stringified.
#[cfg(feature = "test-helpers")]
#[doc(hidden)]
pub async fn exec_mutation_with_emit_for_tests(
    bq: query::BuiltQuery,
    app_id: &str,
    collection: &str,
    op: zeroship_core::change_event::ChangeOp,
) -> Result<Vec<serde_json::Value>, String> {
    let backend = tx_scope::ensure_backend()
        .await
        .map_err(DbError::into_string)?;
    let route = exec::ambient_route_for_tests(app_id, backend);
    exec::exec_mutation_with_emit(bq, &route, collection, op)
        .await
        .map_err(DbError::into_string)
}

/// **Test-only**: exec a read query through the same shared pool-or-tx path
/// production CRUD uses, including the Postgres autocommit per-app role fence.
///
/// Adapter-side for the reason on [`exec_mutation_with_emit_for_tests`].
///
/// # Errors
///
/// The statement's own error, stringified.
#[cfg(feature = "test-helpers")]
#[doc(hidden)]
pub async fn exec_query_for_tests(
    app_id: &str,
    bq: query::BuiltQuery,
) -> Result<Vec<serde_json::Value>, String> {
    let backend = tx_scope::ensure_backend()
        .await
        .map_err(DbError::into_string)?;
    let route = exec::ambient_route_for_tests(app_id, backend);
    exec::exec_query(&route, bq)
        .await
        .map_err(DbError::into_string)
}

/// **Test-only**: `(idle, active, total)` for this isolate's data pool.
///
/// `transaction::probe::pool_counts` takes the handle now - the engine may not
/// read the adapter's context - so this is where the lookup lives.
#[cfg(feature = "test-helpers")]
#[doc(hidden)]
#[must_use]
pub fn pool_counts_for_tests() -> Option<(usize, usize, usize)> {
    transaction::probe::pool_counts(&context::with(|c| c.backend())?)
}

/// **Test-only**: install a real Postgres client into the active
/// isolate's `ThreadDbContext::tx_conn` slot (formerly the `TX_CONN`
/// thread-local, folded into `ThreadDbContext`) so the Gap B
/// integration tests can drive the deferred-broker-emit queue/drain
/// machinery without standing up a V8 isolate. Returns the
/// connection-task handle so the caller can detach it.
///
/// Asynchronous because it has to open a fresh Postgres connection
/// (the same shape the production `exec_begin` does). Pair with
/// [`uninstall_tx_marker_for_tests`] to release the slot.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub async fn install_tx_marker_for_tests(app_id: &str, url: &str) {
    // A pooled checkout, like the production path: `TxConnection::Postgres`
    // now carries an `OwnedPooledClient`, and a helper that opened a raw
    // connection would be testing a shape production no longer has.
    let pool = Rc::new(
        Pool::connect(url, 2)
            .await
            .expect("install_tx_marker_for_tests: pool connect failed"),
    );
    let client = pool
        .get_owned()
        .await
        .expect("install_tx_marker_for_tests: pooled checkout failed");
    // Issue a real BEGIN so the dummy connection behaves like a real
    // tx — not strictly required (the queueing path keys off
    // `ThreadDbContext::has_tx_for`), but matches the production state
    // machine more honestly.
    let _ = client.execute("BEGIN", &[]).await;
    crate::tx_lanes::with_mut(|l| {
        let _previous = l.install_tx_client(app_id, crate::tx_lanes::TxConnection::Postgres(client));
        debug_assert!(
            _previous.is_none(),
            "install_tx_marker_for_tests: slot already occupied"
        );
    });
}

/// **Test-only**: drop the transaction-connection slot, rolling back
/// the dummy tx server-side via an explicit `ROLLBACK` on the wire
/// (NOT just relying on connection close). Mirrors
/// [`install_tx_marker_for_tests`].
///
/// Async + sends `ROLLBACK` before dropping the Client because the
/// per-isolate `ThreadDbContext` is thread-local and the test's
/// compio runtime drops between tests. With `--test-threads=1` every
/// test shares one thread; if a prior test's Client is dropped without
/// explicit `ROLLBACK` the PG backend on the other end can linger as
/// `idle in transaction` for a window after Runtime::drop (the
/// connection task is dropped before its terminate-flush path runs,
/// and the server only observes EOF when the OS reaps the fd). A
/// later `pg_create_logical_replication_slot()` call (the p8a2
/// auto-spawn test) then blocks waiting for that ghost transaction —
/// the p8a2 ordering hang.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub async fn uninstall_tx_marker_for_tests(app_id: &str) {
    if let Some(client) = crate::tx_lanes::with_mut(|l| l.take_tx_client_for(app_id)) {
        match client {
            crate::tx_lanes::TxConnection::Postgres(client) => {
                // Best-effort: a connection already torn down (panic recovery)
                // is fine — drop closes the fd.
                let _ = client.batch_execute("ROLLBACK").await;
                drop(client);
            }
            crate::tx_lanes::TxConnection::Sqlite(client) => {
                let _ = client.exec("ROLLBACK", &[]).await;
                drop(client);
            }
        }
    }
}

/// **Test-only**: push a `ChangeEvent` onto the pending-emits queue
/// (the same path `exec_mutation_with_emit` takes when inside a tx).
/// Used by the Gap B test to assert the drain/clear behavior without
/// running real SQL.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn push_pending_emit_for_tests(ev: zeroship_core::change_event::ChangeEvent) {
    crate::tx_lanes::with_mut(|l| l.push_pending_emit(ev));
}

/// **Test-only**: drain the pending-emits queue (fire all events
/// through `emit_local`). Exposed so the Gap B tests can drive the
/// transaction settle path's commit branch without standing up V8.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn drain_pending_emits_for_tests(app_id: &str) {
    exec::drain_pending_emits_on_commit(app_id);
}

/// **Test-only**: clear the pending-emits queue without firing
/// (rollback branch).
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn clear_pending_emits_for_tests(app_id: &str) {
    exec::clear_pending_emits(app_id);
}

// `drop_pooled_lock_guard_without_release_for_tests` was deleted on
// 2026-09-04. Its doc claimed "used by the integration suite to verify the
// catastrophic Drop path"; no integration test ever called it, in this crate or
// any other. A `#[cfg(any(test, feature = "test-helpers"))] #[doc(hidden)] pub`
// item is invisible BOTH ways - rustc's dead_code lint cannot see a `pub` item,
// and a default-features build does not compile it at all - so nothing was ever
// going to report it. The Drop path it named is still guarded, by
// `drop_with_released_true_does_not_warn` in data-postgres's `lock_guard.rs`,
// against the flag rather than a live pool.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackendUrl {
    Postgres,
    Sqlite { path: PathBuf },
}

/// `true` iff `url` resolves to the SQLite (dev-tier) backend under the SAME
/// grammar [`backend_for_url`] uses (`sqlite:` / `sqlite://` / `file:` /
/// `:memory:` / a bare filesystem path). PG (`postgres://`/`postgresql://`)
/// and an empty / unknown-scheme URL are `false`.
///
/// Delegates to [`zeroship_core::db_url::is_sqlite_url`] so the grammar has ONE
/// source of truth shared with the runtime's single-isolate clamp and the
/// worker's hard-abort guard (neither can depend on plugin-db, which depends on
/// the runtime). A debug assertion keeps it in lock-step with `backend_for_url`
/// — the opener and the classifier can never silently diverge.
#[must_use]
pub fn is_sqlite_url(url: &str) -> bool {
    let v = zeroship_core::db_url::is_sqlite_url(url);
    debug_assert_eq!(
        v,
        matches!(backend_for_url(url), Ok(BackendUrl::Sqlite { .. })),
        "is_sqlite_url drifted from backend_for_url for {url:?}"
    );
    v
}

/// Classify a database URL.
///
/// **Call [`service::select_backend`] instead**, unless you are `is_sqlite_url`
/// below (a pure grammar check that opens nothing). Every real backend
/// selection goes through the service wrapper so the process's parse count is
/// a complete measurement rather than a sample of the sites that remembered.
pub(crate) fn backend_for_url(url: &str) -> Result<BackendUrl, DbError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(DbError::config_hinted(
            "invalid_database_url",
            "database URL is empty",
            "expected postgres://, postgresql://, sqlite:, file:, :memory:, or a filesystem path",
        ));
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower == ":memory:" {
        return Ok(BackendUrl::Sqlite {
            path: PathBuf::from(":memory:"),
        });
    }
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        return Ok(BackendUrl::Postgres);
    }
    if lower.starts_with("sqlite://") {
        // SQLite URLs are always local-file selectors here, so any URI
        // authority is folded into the filesystem path (`sqlite://host/db`
        // becomes `host/db`, not a remote host lookup).
        return Ok(BackendUrl::Sqlite {
            path: PathBuf::from(&trimmed["sqlite://".len()..]),
        });
    }
    if lower.starts_with("sqlite:") {
        return Ok(BackendUrl::Sqlite {
            path: PathBuf::from(&trimmed["sqlite:".len()..]),
        });
    }
    if lower.starts_with("file:") {
        return Ok(BackendUrl::Sqlite {
            path: PathBuf::from(&trimmed["file:".len()..]),
        });
    }

    let has_scheme = trimmed
        .split_once(':')
        .map(|(scheme, _)| {
            // Windows `C:\...` is rejected here as scheme `c`; that's
            // acceptable because zeroship only targets Linux workers.
            let mut chars = scheme.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        })
        .unwrap_or(false);
    if has_scheme {
        return Err(DbError::config_hinted(
            "unsupported_database_url_scheme",
            format!("unsupported database URL scheme in `{trimmed}`"),
            "expected postgres://, postgresql://, sqlite:, file:, :memory:, or a filesystem path",
        ));
    }

    Ok(BackendUrl::Sqlite {
        path: PathBuf::from(trimmed),
    })
}

/// Initialize the connection pool asynchronously.
///
/// Must be called on the compio runtime thread BEFORE any JS execution.
/// Typically called after the plugin has been registered on a Runtime but
/// before the isolate starts processing requests.
///
/// ```ignore
/// // Once, at composition, before any isolate exists:
/// let service = DbService::new(DbServiceConfig {
///     url,
///     worker_id: "worker-instance-id".into(),
///     meter: None,
/// })?;
/// // Inside a compio runtime, per isolate:
/// let runtime = Runtime::builder().plugin(service.plugin()).build();
/// zeroship_plugin_db::init_pool_async().await?;
/// // Now safe to run JS that calls zeroship.db.*
/// ```
struct BackendInitGuard;

impl Drop for BackendInitGuard {
    fn drop(&mut self) {
        ctx_mut(|c| c.finish_backend_init());
    }
}

/// Decide what this thread's installed DB resources mean for backend init.
///
/// `Ok(None)` - nothing is installed; the DB plugin is disabled on this thread
/// and `env.db` is legitimately absent.
/// `Ok(Some(..))` - a URL and the backend selection made for it at composition.
/// `Err(..)` - exactly one of the two is installed.
///
/// **The mixed state is an error, not a disabled plugin.** It is unreachable
/// today - `install_db_resources` writes both fields together and is the only
/// writer - but the two are separate `Option`s, so the arm has to say something,
/// and "either is missing means the DB plugin is disabled" says the wrong thing.
/// Under it, a thread that had a URL and somehow no selection would run every
/// `env.db` call as a silent no-op that reports success, which is
/// indistinguishable from an app that never configured a database. Naming the
/// state costs one arm and turns a whole class of silent misconfiguration into a
/// message with both halves in it.
fn backend_init_inputs(
    url: Option<String>,
    selection: Option<BackendUrl>,
) -> Result<Option<(String, BackendUrl)>, String> {
    match (url, selection) {
        (None, None) => Ok(None),
        (Some(url), Some(selection)) => Ok(Some((url, selection))),
        (Some(_), None) => Err(
            "db: a database URL is installed on this thread with no backend selection; \
             the two are installed together by DbPlugin::register, so this thread's \
             resources were installed by something else"
                .to_string(),
        ),
        (None, Some(selection)) => Err(format!(
            "db: a backend selection ({selection:?}) is installed on this thread with no \
             database URL; the two are installed together by DbPlugin::register, so this \
             thread's resources were installed by something else"
        )),
    }
}

pub async fn init_pool_async() -> Result<(), String> {
    let Some((url, selection)) =
        context::with(|c| backend_init_inputs(c.db_url(), c.backend_selection()))?
    else {
        return Ok(()); // Nothing configured — DB plugin is disabled
    };

    // SQLite installs only a backend handle (no pool), and lazy init
    // can be reached by multiple request futures before the first open
    // completes. Make cold init single-flight per isolate so concurrent
    // startup RPCs wait for the first backend instead of racing PRAGMA
    // bootstrap against the same dev database file.
    loop {
        match ctx_mut(|c| c.begin_backend_init()) {
            BackendInitState::Ready => return Ok(()),
            BackendInitState::Acquired => break,
            BackendInitState::InProgress => {
                compio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }

    let _init_guard = BackendInitGuard;

    async {
        // The backend selection the service made at composition, NOT a fresh
        // parse of the URL. `install_db_resources` stamped it onto this thread
        // when the plugin registered.
        match selection {
            BackendUrl::Postgres => {
                // Symmetric with the SQLite arm below: the backend is composed
                // whole by the tier that may name its driver, and this arm only
                // installs it. The connect, the pool size and the error-chain
                // walk all live in `PostgresBackend::connect`.
                let backend = crate::backend::PostgresBackend::connect(
                    &url,
                    8,
                    context::isolate_key_source(),
                )
                .await
                .map_err(DbError::into_string)?;
                service::note_backend_open();
                ctx_mut(|c| c.set_postgres_backend(Rc::new(backend)));
            }
            BackendUrl::Sqlite { path } => {
                let backend = crate::backend_selection::open_sqlite_backend(
                    &path,
                    context::isolate_key_source(),
                )
                    .await
                    .map_err(DbError::into_string)?;
                service::note_backend_open();
                ctx_mut(|c| c.set_sqlite_backend(Rc::new(backend)));
            }
        }
        Ok(())
    }
    .await
}

// App CDC deprovisioning moved onto the service's neutral operator-lifecycle
// handle: `DbService::lifecycle().deprovision_app(app_id)`. The free function
// that used to live here took a `&str` URL, re-ran `backend_for_url` on it and
// built a fresh two-connection `Pool` per deleted app.

#[cfg(test)]
mod backend_init_input_tests {
    use super::{BackendUrl, backend_init_inputs};

    /// Nothing installed is the DISABLED state and stays a success.
    ///
    /// The control for the two arms below. Without it, "every absent field is
    /// an error" would satisfy them and would break every meter-less,
    /// database-less harness in the tree.
    #[test]
    fn nothing_installed_is_a_disabled_plugin_not_an_error() {
        assert_eq!(backend_init_inputs(None, None), Ok(None));
    }

    #[test]
    fn both_installed_resolve_to_the_composed_selection() {
        assert_eq!(
            backend_init_inputs(
                Some("postgres://host/db".to_string()),
                Some(BackendUrl::Postgres)
            ),
            Ok(Some((
                "postgres://host/db".to_string(),
                BackendUrl::Postgres
            ))),
        );
    }

    /// A URL with no selection must be REPORTED, not silently disabled.
    ///
    /// The pre-fix guard was `Some((c.db_url()?, c.backend_selection()?))`, so
    /// either field being absent returned `Ok(())` under the comment "No URL
    /// configured - DB plugin is disabled". That is true of one of the three
    /// non-trivial states and wrong about the other two: a thread carrying a
    /// database and no selection ran every `env.db` op as a successful no-op.
    #[test]
    fn a_url_without_a_selection_is_a_reported_configuration_error() {
        let error = backend_init_inputs(Some("postgres://host/db".to_string()), None)
            .expect_err("a half-installed thread must not read as 'no database configured'");
        assert!(
            error.contains("no backend selection"),
            "the error must name which half is missing: {error}",
        );
    }

    #[test]
    fn a_selection_without_a_url_is_a_reported_configuration_error() {
        let error = backend_init_inputs(None, Some(BackendUrl::Postgres))
            .expect_err("a half-installed thread must not read as 'no database configured'");
        assert!(
            error.contains("no database URL"),
            "the error must name which half is missing: {error}",
        );
    }
}

#[cfg(test)]
mod backend_url_tests {
    use super::{BackendUrl, backend_for_url};
    use std::path::PathBuf;

    #[test]
    fn postgres_urls_dispatch_to_postgres() {
        assert!(matches!(
            backend_for_url("postgres://localhost/dev").unwrap(),
            BackendUrl::Postgres
        ));
        assert!(matches!(
            backend_for_url("postgresql://localhost/dev").unwrap(),
            BackendUrl::Postgres
        ));
    }

    #[test]
    fn sqlite_urls_dispatch_to_sqlite() {
        assert_eq!(
            backend_for_url("sqlite:/tmp/dev.sqlite").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from("/tmp/dev.sqlite"),
            }
        );
        assert_eq!(
            backend_for_url("sqlite:///tmp/dev.sqlite").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from("/tmp/dev.sqlite"),
            }
        );
        assert_eq!(
            backend_for_url("file:./dev.sqlite").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from("./dev.sqlite"),
            }
        );
        assert_eq!(
            backend_for_url(":memory:").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from(":memory:"),
            }
        );
        assert_eq!(
            backend_for_url("./dev.sqlite").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from("./dev.sqlite"),
            }
        );
        assert_eq!(
            backend_for_url("sqlite://host/db").unwrap(),
            BackendUrl::Sqlite {
                path: PathBuf::from("host/db"),
            }
        );
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        let err = backend_for_url("mysql://localhost/dev").unwrap_err();
        assert!(matches!(
            err,
            zeroship_data_core::error::DbError::Configuration {
                code: "unsupported_database_url_scheme",
                ..
            }
        ));
    }
}

#[cfg(test)]
mod backend_init_tests {
    use super::{context, ctx_mut, init_pool_async, set_db_url_for_tests};

    fn set_fresh_db_url(url: &str) {
        ctx_mut(|c| c.clear_pool());
        set_db_url_for_tests(url);
    }

    /// Eight concurrent cold inits open exactly ONE backend.
    ///
    /// The count is the assertion. "A usable backend is installed" - which is
    /// all this test asserted until the open counter existed - passes with no
    /// singleflight at all: eight sequential opens leave one installed too,
    /// because each overwrites the last. Remove `begin_backend_init` and this
    /// arm reports 8.
    ///
    /// It is also the liveness proof for [`crate::service::backend_open_count`]
    /// itself. The worker's "building the plugin set opens no pool" guard reads
    /// that counter and asserts it did NOT move; a counter wired to nothing
    /// satisfies that forever. This arm shows it moves when a backend really is
    /// opened.
    #[compio::test]
    async fn concurrent_sqlite_lazy_init_shares_one_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite:{}", dir.path().join("cold-init.sqlite").display());
        set_fresh_db_url(&url);
        let opens_before = crate::service::backend_open_count();

        const CONCURRENCY: usize = 8;
        let handles = (0..CONCURRENCY)
            .map(|_| compio::runtime::spawn(async { init_pool_async().await }))
            .collect::<Vec<_>>();

        for handle in handles {
            handle
                .await
                .expect("join concurrent init task")
                .expect("sqlite init should succeed");
        }

        assert!(
            context::with(|c| c.backend().is_some()),
            "concurrent init calls should leave a usable backend installed"
        );
        assert_eq!(
            crate::service::backend_open_count() - opens_before,
            1,
            "{CONCURRENCY} concurrent cold inits must open ONE backend, not one each",
        );
    }
}

/// The workflow journal schema name is derived TWICE, in two crates that
/// deliberately do not depend on each other, and until this module existed the
/// only thing holding them in agreement was a comment.
///
/// `zeroship-migrate-server` WRITES the schema
/// (`provisioning::workflow_journal_schema_name`, called at
/// `provisioning.rs:327` and `apply.rs:1526`); `zeroship-plugin-workflow` READS
/// it (`store::pg::app_schema_for`, called at `store/pg.rs:81`). The writer's
/// own doc says why they are separate: "this crate does not depend on that one,
/// so the derivation is duplicated rather than shared."
///
/// THIS LIVES IN plugin-db, WHICH IS NEITHER OF THEM, and that is not an
/// accident: plugin-db is the only crate that already dev-depends on both
/// (`Cargo.toml` :121 and :141, the latter noting "DEV-only ... so no cycle"),
/// so the check costs no new edge in the dependency graph.
///
/// WHY AN EQUALITY TEST AND NOT AN INTEGRATION TEST. The obvious alternative -
/// provision through the writer, then read through the reader - is what
/// `tests/integration.rs` looks like it does and does NOT: it computes the name
/// with the READER, then creates and drops that schema as its own fixture, so a
/// drift in the writer alone leaves it green. A test that builds its own
/// precondition cannot detect a disagreement between two producers.
#[cfg(test)]
mod journal_schema_derivations_agree {
    use uuid::Uuid;

    /// Both derivations must produce the same schema name for the same app.
    ///
    /// Asserted over several ids rather than one, because the shapes that could
    /// diverge are formatting choices - hyphenation, case, prefix - and a single
    /// fixed uuid can hide a difference that only some byte patterns expose.
    #[test]
    fn the_writer_and_the_reader_name_the_same_schema() {
        let ids = [
            Uuid::nil(),
            Uuid::max(),
            Uuid::parse_str("0198f0a1-0000-7000-8000-0123456789ab").expect("fixed uuid parses"),
            Uuid::new_v4(),
        ];
        for id in ids {
            let writer = zeroship_migrate_server::provisioning::workflow_journal_schema_name(&id);
            let reader = zeroship_plugin_workflow::store::pg::app_schema_for(&id);
            assert_eq!(
                writer, reader,
                "the migration service provisions the workflow journal schema as \
                 {writer} while the workflow plugin reads {reader}; a deploy would \
                 write its journal where nothing looks for it"
            );
        }
    }
}

/// `reset_context_for_tests` must clear ALL THREE thread-locals.
///
/// The context was one struct until 2026-09-02. It is now three owners with
/// three thread-locals - the adapter context, the engine.s lanes, and
/// data-core.s descriptor store - so "reset" became three calls, any one of
/// which could be dropped without an existing test noticing. One test per
/// store, because a single test asserting all three would pass while two of
/// them regressed.
///
/// **Both halves live in ONE test on purpose.** The obvious shape - claim in
/// test A, assert clean in test B - proves nothing here: libtest gives every
/// `#[test]` its own OS thread even under `--test-threads=1` (measured
/// 2026-09-01), so B would read a fresh thread-local and pass whatever the
/// helper does. Such a guard is green by construction. The real hazard is a
/// MID-TEST reset - scenario setup between phases, and the `ContextReset` drop
/// guard in `transaction/mod.rs` - and that is what this reproduces.
#[cfg(test)]
mod reset_clears_every_thread_local {
    /// Deleting `tx_lanes::reset_for_tests()` from `reset_context_for_tests`
    /// must fail this.
    ///
    /// `TxLanes` has TWO stores and both are asserted, because they have
    /// different lifetimes and a partial reset could plausibly clear one: the
    /// lane map is emptied by ordinary retirement, whereas the withdrawal
    /// tombstone is documented to outlive its lane and to be cleared only by
    /// the next `admit_transaction`. The tombstone is therefore the residue
    /// most likely to survive a reset that looks correct.
    #[test]
    fn a_mid_test_reset_drops_a_claim_and_a_withdrawal_tombstone() {
        let app = "app_reset_guard";

        assert!(
            crate::tx_lanes::with_mut(|l| l.try_claim_tx(app)),
            "an unclaimed app claims on a fresh thread"
        );
        // `tx_claimed_by`, not `has_tx_for`: claiming opens the lane, and
        // `has_tx_for` additionally requires the BEGIN to have landed a
        // session. The claim without a session is exactly the window this
        // helper has to clean up, so it is the one to assert on.
        assert!(crate::tx_lanes::with(|l| l.tx_claimed_by(app)));

        crate::tx_lanes::with_mut(|l| l.withdraw_tx_session(app));
        assert!(crate::tx_lanes::with(|l| l.tx_session_withdrawn(app)));

        crate::reset_context_for_tests();

        assert!(
            !crate::tx_lanes::with(|l| l.tx_claimed_by(app)),
            "reset_context_for_tests left a transaction claim behind: the lane \
             thread-local was not reset"
        );
        assert!(
            !crate::tx_lanes::with(|l| l.tx_session_withdrawn(app)),
            "reset_context_for_tests left a withdrawal tombstone behind: the \
             next phase's session would be destroyed on return instead of parked"
        );
        // Re-claiming is the stronger statement, and it is the one a later
        // phase of a multi-phase test actually makes: `tx_claimed_by` could
        // read false off a half-cleared lane that still refuses a new claim.
        assert!(
            crate::tx_lanes::with_mut(|l| l.try_claim_tx(app)),
            "the app is claimable again after a reset"
        );
    }

    /// The descriptor store is the THIRD thread-local the reset must clear, and
    /// the one furthest from the helper: it moved to `zeroship-data-core` on
    /// 2026-09-02, so a reset that forgot it would leave a stale schema visible
    /// to the next phase of a test - the exact L24 shape, where serving a read
    /// against the wrong descriptor is what drops the projection allowlist.
    #[test]
    fn a_mid_test_reset_drops_an_installed_descriptor() {
        let binding = zeroship_data_core::binding::DbBinding::cold_start("app_reset_schema");

        zeroship_data_core::schema_cache::with_mut(|c| {
            c.insert_one(
                &binding,
                "users",
                serde_json::json!({ "email": { "type": "string" } }),
            );
        });
        assert!(zeroship_data_core::schema_cache::with(|c| c.get(&binding, "users")).is_some());

        crate::reset_context_for_tests();

        assert!(
            zeroship_data_core::schema_cache::with(|c| c.get(&binding, "users")).is_none(),
            "reset_context_for_tests left a descriptor entry behind: the schema \
             thread-local in data-core was not reset"
        );
    }
}
