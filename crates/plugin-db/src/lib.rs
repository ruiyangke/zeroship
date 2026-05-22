//! Database plugin — backs `env.db` with a typed `#[v8_class]` surface.
//!
//! `env.db` is the `Db` v8_class instance (see [`v8_classes::db`]).
//! Every operation lives on the wrapper: `registerModel`,
//! `collection(name)` (mints a [`v8_classes::collection::Collection`]),
//! `beginTransaction(opts?)` (mints a [`v8_classes::transaction::Transaction`]),
//! `openSubscription(name)` (mints a [`v8_classes::subscription::Subscription`]),
//! `startReplicationConsumer(opts?)`. Two nested namespaces hang off
//! the Db wrapper as cached `#[v8_getter]`s:
//!
//! - `db.migrations` — the [`v8_classes::migrations::Migrations`]
//!   namespace (`.start(spec)` mints a `Migration` wrapper;
//!   `.status / .cancel / .reset({name, collection})` operate on the
//!   audit row by coordinates).
//! - `db.replication` — the [`v8_classes::replication::Replication`]
//!   operator namespace (`.setup`, `.watchdog`, `.dropAbandoned`).
//!
//! Each wrapper carries a `v8::Weak` guaranteed finalizer that
//! releases its backing resource on GC (broker handle, transaction
//! connection, migration advisory lock).
//!
//! Each app gets its own PostgreSQL schema (`"app_id".*`) for data
//! isolation. The pool is created lazily on first use (one per worker
//! thread).

use std::rc::Rc;

use compio_postgres::Pool;
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

use crate::context::with_mut as ctx_mut;

pub mod audit;
pub mod auth;
pub mod broker;
pub(crate) mod context;
pub mod crud;
pub mod diff;
pub mod error;
pub mod exec;
pub mod migrations;
pub mod orchestrator;
pub mod query;
pub mod read_set;
pub mod replication;
pub mod replication_ops;
pub mod v8_bridge;
pub mod v8_classes;
pub mod wal_consumer;

// ---------------------------------------------------------------------------
// Per-isolate state
// ---------------------------------------------------------------------------
//
// All per-isolate slots live on [`context::IsolateDbContext`]; this
// module just re-exports the helpers the rest of the crate calls.

/// Allocate a fresh non-zero TX_TOKEN value. Called by
/// `orchestrator::transaction::begin_transaction_dispatch` right before
/// stamping the token onto the freshly-minted `Transaction` wrapper.
pub(crate) fn next_tx_token() -> u64 {
    ctx_mut(|c| c.next_tx_token())
}

/// Check if a model is already registered for this app on this thread.
pub(crate) fn is_model_registered(app_id: &str, collection: &str) -> bool {
    context::with(|c| c.is_model_registered(app_id, collection))
}

/// Mark a model as registered.
pub(crate) fn mark_model_registered(app_id: &str, collection: &str) {
    ctx_mut(|c| c.mark_model_registered(app_id, collection));
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
}

impl std::fmt::Debug for DbPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbPlugin").finish()
    }
}

impl DbPlugin {
    /// Create a new `DbPlugin` instance.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
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
        });
        // Every JS-visible entry point lives on the Db v8_class
        // wrapper (see `v8_classes::db`); the registrar only installs
        // the auto-tx globals — `query()` / `mutation()` defense in
        // depth at the Postgres level around the B3 capability gate.
        r.add_setup("install_auto_tx_globals", |scope, _ns_obj| {
            orchestrator::auto_tx::install_auto_tx_globals(scope);
        });
    }
}

/// **Test-only**: set the per-thread `DB_URL` directly, bypassing the
/// usual `DbPlugin::register()` path. Used by integration tests that
/// drive `migrations::exec_*` without spinning up a full runtime.
#[doc(hidden)]
pub fn set_db_url_for_tests(url: &str) {
    ctx_mut(|c| {
        c.set_db_url(url);
    });
}

/// **Test-only**: clear `MIG_LOCK` for the current thread. Safe across
/// test boundaries when an earlier test left the lock held.
#[doc(hidden)]
pub fn clear_migration_lock_for_tests() {
    migrations::release_active_lock();
}

/// **Test-only**: install a real Postgres client into `TX_CONN` so
/// the Gap B integration tests can drive the deferred-broker-emit
/// queue/drain machinery without standing up a V8 isolate. Returns
/// the connection-task handle so the caller can detach it.
///
/// Asynchronous because it has to open a fresh Postgres connection
/// (the same shape the production `exec_begin` does). Pair with
/// [`uninstall_tx_marker_for_tests`] to release the slot.
#[doc(hidden)]
pub async fn install_tx_marker_for_tests(url: &str) {
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .expect("install_tx_marker_for_tests: connect failed");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    // Issue a real BEGIN so the dummy connection behaves like a real
    // tx — not strictly required (the queueing path keys off
    // `IsolateDbContext::has_tx`), but matches the production state
    // machine more honestly.
    let _ = client.execute("BEGIN", &[]).await;
    ctx_mut(|c| {
        let _previous = c.install_tx_client(client);
        debug_assert!(_previous.is_none(), "install_tx_marker_for_tests: slot already occupied");
    });
}

/// **Test-only**: drop the transaction-connection slot (rolls back the
/// dummy tx server-side via connection close). Mirrors
/// [`install_tx_marker_for_tests`].
#[doc(hidden)]
pub fn uninstall_tx_marker_for_tests() {
    let client = ctx_mut(|c| c.take_tx_client());
    drop(client);
}

/// **Test-only**: push a `ChangeEvent` onto the pending-emits queue
/// (the same path `exec_mutation_with_emit` takes when inside a tx).
/// Used by the Gap B test to assert the drain/clear behavior without
/// running real SQL.
#[doc(hidden)]
pub fn push_pending_emit_for_tests(ev: broker::ChangeEvent) {
    ctx_mut(|c| c.push_pending_emit(ev));
}

/// **Test-only**: drain the pending-emits queue (fire all events
/// through `emit_local`). Exposed so the Gap B tests can drive the
/// transaction settle path's commit branch without standing up V8.
#[doc(hidden)]
pub fn drain_pending_emits_for_tests() {
    exec::drain_pending_emits_on_commit();
}

/// **Test-only**: clear the pending-emits queue without firing
/// (rollback branch).
#[doc(hidden)]
pub fn clear_pending_emits_for_tests() {
    exec::clear_pending_emits();
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
///     .plugin(DbPlugin::new(url))
///     .build();
/// zeroship_plugin_db::init_pool_async().await?;
/// // Now safe to run JS that calls zeroship.db.*
/// ```
pub async fn init_pool_async() -> Result<(), String> {
    let url = context::with(|c| c.db_url());
    let Some(url) = url else {
        return Ok(()); // No URL configured — DB plugin is disabled
    };

    let pool = Pool::connect(&url, 8)
        .await
        .map_err(|e| {
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
    Ok(())
}
