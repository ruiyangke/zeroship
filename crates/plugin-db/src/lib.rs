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
//! Platform-internal capabilities such as model registration,
//! migrations, and replication no longer sit on the public `Db`
//! wrapper; they hang off the private `__platform` capability handle
//! instead.
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
// `crates/gateway/src/main.rs` carries this attribute for the same reason. It
// is a compiler resource limit, not a correctness guard - raising it costs
// compile time and nothing else.
#![recursion_limit = "256"]

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use compio_postgres::Pool;
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

use crate::context::{BackendInitState, with_mut as ctx_mut};
use crate::error::DbError;

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

// Always pub:
pub mod broker;
pub mod error;
// The DDL builders + `QueryError` + `SqlDialect` +
// the system-field / validation helpers were extracted into the leaf crate
// `zeroship-schema`. plugin-db re-exports the module wholesale so every
// existing `crate::query::…` reference (and `use crate::query;` then
// `query::…`) resolves unchanged — behaviour identical, no call-site churn.
pub use zeroship_schema::query;
pub mod v8_classes;

// `backend` is crate-private by default; under `test-helpers` it
// becomes `pub` so the integration-test targets
// (`tests/sqlite_integration.rs` in particular) can name
// `backend::SqliteBackend` + the `SqlExecutor` trait directly. The PG
// `tests/integration.rs` target reaches PG-specific behaviour through
// the lifted-to-pub helpers in `exec` / `migrations` /
// `register_model` / `drop_namespace` — those continue to gate on
// `test-helpers`. The backend traits themselves carry no
// production-only behaviour (their bodies are SQL + RPC plumbing), so
// exposing them under the same gate is safe.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod backend;
#[cfg(feature = "test-helpers")]
pub mod backend;
pub(crate) mod context;
// `cross_app_fk` is `pub` (not `pub(crate)`) because integration tests
// in both `tests/integration.rs` (PG arm) and
// `tests/sqlite_integration.rs` (SQLite arm) call the validator
// directly to pin the rejection contract. The function is a pure JSON
// walk — no DB round-trip — so exposing it has zero runtime impact;
// those integration tests are, as of 2026-08-20, its ONLY reachable
// callers. This comment claimed a "production call site is one line in
// `register_model/bootstrap.rs::bootstrap`", and that line is real but
// sits in a `#[cfg(any(test, feature = "test-helpers"))]` module. See
// the enumeration in `cross_app_fk`'s own module header, and read the
// FK-stays-in-app property off `zeroship_schema::query` instead.
pub mod cross_app_fk;
// `crud` is crate-private in release builds; `pub`
// under `test-helpers` so `tests/sqlite_integration.rs` can reach
// `crud::encryption_pass::{encrypt_row_on_write, decrypt_row_on_read}`
// for the end-to-end encrypted-column CRUD round-trip test. Same shape
// as `encryption` below.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod crud;
#[cfg(feature = "test-helpers")]
pub mod crud;
// The diff classifier (`compute_diff`,
// `ChangeKind`, `ChangeClass`, `DiffOp`), the live introspection
// (`read_live_schema`, `estimate_row_count`), and the schema metadata
// types (`MaskMeta`, `EncryptionMeta`, `MaskKind`, `Classification`,
// `WrappedType`, `LiveSchema`, `ColumnInfo`) were extracted into the leaf
// crate `zeroship-schema`. plugin-db re-exports the module so every
// `crate::diff::…` reference resolves unchanged. The original `pub(crate)`
// vs `pub` (under `test-helpers`) visibility is preserved by the cfg gate;
// the integration suites reach `diff::{…}` only under `test-helpers`.
#[cfg(not(feature = "test-helpers"))]
pub(crate) use zeroship_schema::diff;
#[cfg(feature = "test-helpers")]
pub use zeroship_schema::diff;
pub(crate) mod read_set;
pub(crate) mod v8_bridge;

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
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod encryption;
#[cfg(feature = "test-helpers")]
pub mod encryption;

// `change_stream_pg` is the PG-arm adapter for the `ChangeStream`
// capability declared in `crate::backend::mod`. Crate-private — the
// stable consumer surface is the `BackendHandle::as_change_stream_pg`
// accessor (mirroring the `as_postgres` / `as_sqlite` shape). The
// adapter borrows `PostgresBackend` and the underlying replication
// helpers (`replication.rs` / `wal_consumer.rs`) are PG-only.
pub(crate) mod change_stream_pg;

// Process-wide owner for per-app CDC consumers. Native Subscription wrappers
// acquire leases here so all isolates in one worker share one logical slot.
mod cdc_lifecycle;

// Crate-private in release, pub under `test-helpers` (for tests/integration.rs):
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod audit;
#[cfg(feature = "test-helpers")]
pub mod audit;

// The `auth` module is always compiled: the PG-side submodules
// (`bootstrap`, `keys`, `session`) carry the per-app role + session
// machinery, and `auth::util` is the shared-helper subtree the SQLite
// `SessionMinter` impl reuses.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod auth;
#[cfg(feature = "test-helpers")]
pub mod auth;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod exec;
#[cfg(feature = "test-helpers")]
pub mod exec;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod drop_namespace;
#[cfg(feature = "test-helpers")]
pub mod drop_namespace;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod register_model;
#[cfg(feature = "test-helpers")]
pub mod register_model;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod replication;
#[cfg(feature = "test-helpers")]
pub mod replication;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod replication_ops;
#[cfg(feature = "test-helpers")]
pub mod replication_ops;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod transaction;
#[cfg(feature = "test-helpers")]
pub mod transaction;

// Async-scoped transaction marker. Read by `transaction` to tell a
// genuinely NESTED `transaction()` call from one that merely overlaps
// another in time; see the module docs for the defect that distinction
// closes.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod tx_scope;
#[cfg(feature = "test-helpers")]
pub mod tx_scope;

// The tx-vs-pool routing decision, frozen at the V8 dispatch frame and
// carried into the spawned future. Always `pub` (not feature-gated like
// its neighbours): the `TxRoute` TYPE has to be nameable wherever the
// `exec` entry points are, and its production constructor needs a
// `&mut v8::PinScope`, so exposing the type opens nothing. See the
// module docs for why a missed dispatch site cannot compile.
pub mod tx_route;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod wal_consumer;
#[cfg(feature = "test-helpers")]
pub mod wal_consumer;

// Test-only: `tracing-subscriber` capture layer for warn/error-shape
// contract tests. See `test_support/mod.rs` for the module preamble.
//
// Gate note: `cfg(test)` only (NOT `any(test, feature = "test-helpers")`)
// because `tracing-subscriber` is a `[dev-dependencies]` entry — it
// is unavailable when downstream crates compile the lib with
// `--features test-helpers` (which is non-test compilation from the
// integration target's perspective). Moving `tracing-subscriber` out
// of dev-deps would pollute the release dependency graph. The
// `test_support` surface is therefore reachable from in-crate unit
// tests (`#[cfg(test)] mod tests { use crate::test_support; }`) but
// not from `crates/plugin-db/tests/integration.rs`. End-to-end
// warn-shape coverage from integration tests would need a separate
// helper rebuilt off `tracing_subscriber` re-exposed elsewhere.
#[cfg(test)]
pub(crate) mod test_support;

// ---------------------------------------------------------------------------
// Per-isolate state
// ---------------------------------------------------------------------------
//
// All per-isolate slots live on [`context::IsolateDbContext`]; this
// module just re-exports the helpers the rest of the crate calls.

/// Check if a model is already registered for this app on this thread.
pub(crate) fn is_model_registered(app_id: &str, collection: &str) -> bool {
    context::with(|c| c.is_model_registered(app_id, collection))
}

/// Mark a model as registered.
pub(crate) fn mark_model_registered(app_id: &str, collection: &str) {
    ctx_mut(|c| c.mark_model_registered(app_id, collection));
}

/// Test helper — mark a model registered on the current isolate,
/// mirroring what `register_model` does at the SDK boundary. The runtime schema
/// resolver gates on `is_model_registered` (the cold-schema contract), so a
/// faithful e2e that drives the CRUD pipelines directly must mark the model.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn mark_model_registered_for_tests(app_id: &str, collection: &str) {
    mark_model_registered(app_id, collection);
}

/// Test helper — clear the registered mark so a re-register of the same
/// `(app, collection)` with a CHANGED schema re-runs the cold path (a real dev
/// re-deploy mints a fresh isolate; tests reuse one). Used by the destructive-
/// apply test to drive a v1→v2 schema change through the engine.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn clear_model_registered_for_tests(app_id: &str, collection: &str) {
    ctx_mut(|c| c.clear_model_registered(app_id, collection));
}

/// Test helper — stamp the per-`app_id` deploy/schema-version token into the
/// per-isolate context, the way `mint_db` does from the worker-injected
/// `ZEROSHIP_DEPLOY_ID`. A faithful e2e that drives the CRUD pipelines directly
/// uses this to simulate a redeploy (bump the token) and prove the deploy-keyed
/// introspection cache re-introspects the new schema's metadata.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn set_deploy_token_for_tests(app_id: &str, token: &str) {
    context::with_mut(|c| c.set_deploy_token(app_id, token));
}

// The synchronous `ensure_pool(scope)` helper that used to live here
// has been removed — every callback dispatches through
// `init_pool_async()` + `context::with_mut(...)` directly (or the
// `exec::ensure_pool` async helper that wraps the same).

// ---------------------------------------------------------------------------
// DbPlugin
// ---------------------------------------------------------------------------

/// The database plugin — registers `zeroship.db.*` methods.
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
}

impl std::fmt::Debug for DbPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbPlugin").finish()
    }
}

impl DbPlugin {
    /// Create a new `DbPlugin` instance over a DB URL and (optionally) the
    /// process-wide usage meter. Pass `None` for the meter in test harnesses
    /// where metering is not under test; the worker / dev-serve vectors pass
    /// `Some(meter)` so each db op emits a per-app usage metric.
    #[must_use]
    pub fn new(
        url: impl Into<String>,
        meter: Option<std::sync::Arc<zeroship_metering::Meter>>,
        worker_id: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            worker_id: worker_id.into(),
            meter,
        }
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
            if c.set_db_url(&self.url) {
                c.clear_pool();
            }
            c.set_cdc_worker_id(&self.worker_id);
            // Stamp the process-wide meter so the exec boundary can emit a
            // per-app usage metric on each successful op.
            c.set_meter(self.meter.clone());
        });
        // Every JS-visible entry point lives on the Db v8_class wrapper
        // (see `v8_classes::db`).
        let _ = r;
    }
}

/// **Bench-only**: thin wrapper around `v8_bridge::row_to_json` so the
/// `bench_row_to_json` Criterion harness in `benches/` can measure the
/// row-to-json index-lookup path without the bench having to live
/// inside `v8_bridge` itself.
///
/// `#[doc(hidden)]` keeps this off the public docs surface; the function
/// is still `pub` because Criterion benches link against the crate as an
/// external dependency and cannot reach `pub(crate)` items.
/// `compio_postgres::test_utils::row_for_test` (doc-hidden there, and always
/// compiled) is the matching `Row` synthesiser — see
/// `crates/plugin-db/benches/bench_row_to_json.rs` for the wiring.
#[doc(hidden)]
#[must_use]
pub fn row_to_json_for_bench(row: &compio_postgres::Row) -> serde_json::Value {
    v8_bridge::row_to_json(row)
}

/// **Bench-only**: the full `&[Row] → JSON-string` path the SDK sees on
/// a `find().first()` (or any other `first_row_or_null`-resolving) call. Runs
/// both halves the dispatcher executes between Postgres and V8:
///
/// 1. `v8_bridge::rows_to_json_value` — decode every `Row` into a
///    `serde_json::Value` (the same work `bench_row_to_json` covers
///    for a single row).
/// 2. `crud::first_row_or_null` — take the first element, fall back
///    to `Value::Null`, and serialise once for `ResolveValue::Json`.
///
/// This composed path is a bottleneck: at wide rows (50 columns) the
/// JSON string + V8 `JSON.parse` tail dominates the read budget. The
/// matching Criterion harness is
/// `crates/plugin-db/benches/bench_first_row_or_null.rs`.
///
/// Returns the raw JSON string (without going through `ResolveValue`) so
/// the bench measures the Rust-side cost in isolation. The remaining V8
/// `JSON.parse` cost is structural and lives in `zeroship-runtime`; it
/// is not part of this microbench.
///
/// Same visibility rationale as [`row_to_json_for_bench`]: `pub` so the
/// external bench target can link against it, `#[doc(hidden)]` so it
/// does not leak into the public surface.
#[doc(hidden)]
#[must_use]
pub fn first_row_or_null_for_bench(rows: &[compio_postgres::Row]) -> String {
    let values = v8_bridge::rows_to_json_value(rows);
    values
        .into_iter()
        .next()
        .unwrap_or(serde_json::Value::Null)
        .to_string()
}

/// **Test-only**: set the per-thread `DB_URL` directly, bypassing the
/// usual `DbPlugin::register()` path. Used by integration tests that
/// drive DB ops directly without spinning up a full runtime.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn set_db_url_for_tests(url: &str) {
    ctx_mut(|c| {
        c.set_db_url(url);
    });
}

/// **Test-only**: install a concrete Postgres pool/backend into the
/// per-thread context. Used by integration tests that need to force the
/// plugin onto a non-default login role without waiting for lazy init to
/// rebuild from a prior test's URL.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn set_postgres_pool_for_tests(pool: Rc<compio_postgres::Pool>, url: &str) {
    ctx_mut(|c| {
        c.set_db_url(url);
        c.set_pool(pool);
    });
}

/// **Test-only**: drop everything the per-thread context holds, including
/// the pool and any parked transaction client.
///
/// Those handles own live Postgres connections. A test that leaves them in
/// the context leaves the connections open, and because releasing a
/// connection is asynchronous they are then orphaned when the test's runtime
/// goes away. Clearing the context first lets the connections close while
/// there is still a runtime to close them.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn reset_context_for_tests() {
    ctx_mut(|c| *c = context::IsolateDbContext::new());
}

/// Test helper: hand this isolate the column root keys its backends
/// should resolve `t.encrypted(...)` columns from, instead of
/// `ZEROSHIP_COLUMN_KEY_<KEYID>`.
///
/// Each entry is `(key_id, root_hex)` where `root_hex` is 64 hex
/// characters. The keys land in the per-isolate context
/// (`IsolateDbContext::set_supplied_root_keys`), so every backend
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

/// Test helper: cache a schema in the per-isolate
/// context so the unmask dispatcher's `lookup_mask_meta` /
/// `lookup_encryption_meta` calls find the column metadata. Mirrors
/// the cache install the production `register_model` orchestrator
/// performs on the SDK boundary.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn cache_schema_for_tests(app_id: &str, collection: &str, schema: serde_json::Value) {
    ctx_mut(|c| {
        c.cache_schema(app_id, collection, schema);
        // Production `register_model` BOTH caches the declared
        // schema AND marks the model registered; the runtime schema resolver
        // (`crud::introspect_schema::runtime_schema_for`) gates on
        // `is_model_registered` to preserve the cold-schema contract. Mark it
        // here too so this helper stays a faithful mirror of registration (a
        // schema cached but not marked registered would never be consulted, an
        // unfaithful half-state).
        c.mark_model_registered(app_id, collection);
    });
}

/// Test helper: drop every cached declared schema for `app_id` (and clear
/// the model-registered marks for the given collections). Simulates a FRESH
/// isolate booting against a WARM app file — the per-isolate sibling cache starts
/// empty even though the file already holds the tables. The warm-multi-
/// collection drop-suppression path must survive exactly this state.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn simulate_fresh_isolate_for_tests(app_id: &str, collections: &[&str]) {
    ctx_mut(|c| {
        c.clear_schemas_for_app(app_id);
        for coll in collections {
            c.clear_model_registered(app_id, coll);
        }
    });
}

/// Test helper: clear the per-isolate mask-policy cache
/// entry for `app_id`. Used by `tests/sqlite_integration.rs` to
/// guarantee a clean slate between policy-driven unmask tests — the
/// per-isolate thread-local cache is process-wide and would otherwise
/// bleed state across test functions running on the same OS thread
/// (the `--test-threads=1` scenario, and also single-runtime tests).
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn clear_mask_policy_cache_for_tests(app_id: &str) {
    ctx_mut(|c| c.set_mask_policy_for_app(app_id, None));
}

/// **Test-only**: install a real Postgres client into the active
/// isolate's `IsolateDbContext::tx_conn` slot (formerly the `TX_CONN`
/// thread-local, folded into `IsolateDbContext`) so the Gap B
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
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .expect("install_tx_marker_for_tests: connect failed");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    // Issue a real BEGIN so the dummy connection behaves like a real
    // tx — not strictly required (the queueing path keys off
    // `IsolateDbContext::has_tx_for`), but matches the production state
    // machine more honestly.
    let _ = client.execute("BEGIN", &[]).await;
    ctx_mut(|c| {
        let _previous =
            c.install_tx_client(app_id, crate::context::TxConnection::Postgres(client));
        debug_assert!(_previous.is_none(), "install_tx_marker_for_tests: slot already occupied");
    });
}

/// **Test-only**: drop the transaction-connection slot, rolling back
/// the dummy tx server-side via an explicit `ROLLBACK` on the wire
/// (NOT just relying on connection close). Mirrors
/// [`install_tx_marker_for_tests`].
///
/// Async + sends `ROLLBACK` before dropping the Client because the
/// per-isolate `IsolateDbContext` is thread-local and the test's
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
    if let Some(client) = ctx_mut(|c| c.take_tx_client_for(app_id)) {
        match client {
            crate::context::TxConnection::Postgres(client) => {
                // Best-effort: a connection already torn down (panic recovery)
                // is fine — drop closes the fd.
                let _ = client.batch_execute("ROLLBACK").await;
                drop(client);
            }
            crate::context::TxConnection::Sqlite(client) => {
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
pub fn push_pending_emit_for_tests(ev: broker::ChangeEvent) {
    ctx_mut(|c| c.push_pending_emit(ev));
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

/// **Test-only**: acquire a real pooled Postgres `LockGuard` against a
/// caller-owned pool and drop it without `release()`/`into_held()`.
/// Used by the integration suite to verify the catastrophic Drop path
/// closes the pooled client instead of leaking the advisory lock onto
/// a reusable backend session.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub async fn drop_pooled_lock_guard_without_release_for_tests(
    pool: Rc<compio_postgres::Pool>,
    url: &str,
    app_id: &str,
    name: &str,
) -> Result<(), String> {
    use crate::backend::{LockScope, PostgresBackend};

    let backend = PostgresBackend::new(Rc::clone(&pool), url.to_string());
    let client = pool
        .get()
        .await
        .map_err(|e| error::DbError::from_pg(&e).into_string())?;
    let scope = LockScope::GlobalApp {
        app_id: app_id.to_string(),
        name: name.to_string(),
    };
    let guard = backend::lock_guard::LockGuard::acquire(&backend, client, &scope)
        .await
        .map_err(error::DbError::into_string)?;
    drop(guard);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BackendUrl {
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

fn backend_for_url(url: &str) -> Result<BackendUrl, DbError> {
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
/// // Inside a compio runtime:
/// let runtime = Runtime::builder()
///     .plugin(DbPlugin::new(url, None, "worker-instance-id"))
///     .build();
/// zeroship_plugin_db::init_pool_async().await?;
/// // Now safe to run JS that calls zeroship.db.*
/// ```
struct BackendInitGuard;

impl Drop for BackendInitGuard {
    fn drop(&mut self) {
        ctx_mut(|c| c.finish_backend_init());
    }
}

pub async fn init_pool_async() -> Result<(), String> {
    let url = context::with(|c| c.db_url());
    let Some(url) = url else {
        return Ok(()); // No URL configured — DB plugin is disabled
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
        let backend = backend_for_url(&url).map_err(DbError::into_string)?;
        match backend {
            BackendUrl::Postgres => {
                let pool = Pool::connect(&url, 8).await.map_err(|e| {
                    // Walk the error source chain so the root cause (e.g. ECONNREFUSED,
                    // TLS handshake failure) reaches the JS console instead of the
                    // generic "error connecting to server" wrapper.
                    let mut msg = format!("db: failed to connect: {e}");
                    let mut cur: &dyn std::error::Error = &e;
                    while let Some(src) = std::error::Error::source(cur) {
                        msg.push_str(&format!(" — caused by: {src}"));
                        cur = src;
                    }
                    msg
                })?;

                ctx_mut(|c| c.set_pool(Rc::new(pool)));
            }
            BackendUrl::Sqlite { path } => {
                let backend = crate::backend::sqlite::SqliteBackend::open(&path)
                    .await
                    .map_err(DbError::into_string)?;
                ctx_mut(|c| c.set_sqlite_backend(Rc::new(backend)));
            }
        }
        Ok(())
    }
    .await
}

/// Tear down all CDC state for a deleted app without requiring a live V8
/// isolate.
///
/// The worker's process-wide version poller calls this after an app disappears
/// from the control-plane registry. It closes local subscriptions, stops this
/// process's consumer, then drops every worker slot. Publication membership
/// remains owned by the migration service. The Postgres teardown is idempotent
/// so every worker container may observe the same deletion safely.
pub async fn deprovision_app_cdc(db_url: &str, app_id: &str) -> Result<(), DbError> {
    cdc_lifecycle::shutdown_app(app_id).await;
    broker::drop_app(Some(app_id));

    match backend_for_url(db_url)? {
        BackendUrl::Sqlite { .. } => Ok(()),
        BackendUrl::Postgres => {
            let pool = Pool::connect(db_url, 2).await.map_err(|error| DbError::Transient {
                message: format!("db CDC app-delete connection failed: {error}"),
            })?;
            replication::drop_worker_slots(&pool, app_id).await
        }
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
            crate::error::DbError::Configuration {
                code: "unsupported_database_url_scheme",
                ..
            }
        ));
    }
}

#[cfg(test)]
mod backend_init_tests {
    use super::{context, ctx_mut, init_pool_async};

    fn set_fresh_db_url(url: &str) {
        ctx_mut(|c| {
            c.clear_pool();
            c.set_db_url(url);
        });
    }

    #[compio::test]
    async fn concurrent_sqlite_lazy_init_shares_one_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = format!("sqlite:{}", dir.path().join("cold-init.sqlite").display());
        set_fresh_db_url(&url);

        let handles = (0..8)
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
    }
}
