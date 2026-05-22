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
//!   namespace (`.setup`, `.watchdog`, `.dropAbandoned`), always scoped
//!   to the calling app — no JS-supplied app-id override.
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

// Module visibility note:
//
// Most modules are `pub(crate)` in normal builds. Several are also
// consumed by external test crates under `tests/`, which are compiled
// as separate crate targets. Those need `pub` visibility when the
// `test-helpers` Cargo feature is enabled (the `[[test]] integration`
// target lists `required-features = ["test-helpers"]`). The cfg-fork
// below keeps the release surface tight while exposing the modules
// for tests.
//
// The unconditionally-`pub` modules (`broker`, `error`, `query`,
// `v8_classes`) are reached even without the feature — see
// tests/subscription_finalizer.rs and tests/db_v8_class.rs.

// Always pub:
pub mod broker;
pub mod error;
pub mod query;
pub mod v8_classes;

// Always crate-private:
pub(crate) mod backend;
pub(crate) mod context;
pub(crate) mod crud;
pub(crate) mod diff;
pub(crate) mod read_set;
pub(crate) mod v8_bridge;

// Crate-private in release, pub under `test-helpers` (for tests/integration.rs):
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod audit;
#[cfg(feature = "test-helpers")]
pub mod audit;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod auth;
#[cfg(feature = "test-helpers")]
pub mod auth;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod exec;
#[cfg(feature = "test-helpers")]
pub mod exec;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod migrations;
#[cfg(feature = "test-helpers")]
pub mod migrations;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod orchestrator;
#[cfg(feature = "test-helpers")]
pub mod orchestrator;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod replication;
#[cfg(feature = "test-helpers")]
pub mod replication;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod replication_ops;
#[cfg(feature = "test-helpers")]
pub mod replication_ops;

#[cfg(not(feature = "test-helpers"))]
pub(crate) mod wal_consumer;
#[cfg(feature = "test-helpers")]
pub mod wal_consumer;

// ---------------------------------------------------------------------------
// Per-isolate state
// ---------------------------------------------------------------------------
//
// All per-isolate slots live on [`context::IsolateDbContext`]; this
// module just re-exports the helpers the rest of the crate calls.

/// Allocate a fresh non-zero [`crate::context::IsolateDbContext::tx_token`]
/// value. Called by
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
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub fn set_db_url_for_tests(url: &str) {
    ctx_mut(|c| {
        c.set_db_url(url);
    });
}

/// **Test-only**: clear [`crate::context::IsolateDbContext::mig_lock`]
/// for the current thread. Safe across test boundaries when an earlier
/// test left the lock held.
///
/// Async + best-effort `ROLLBACK; SELECT pg_advisory_unlock_all();` on
/// the lock client BEFORE dropping it. We can't rely on Client::drop
/// alone to clean up the server-side state — dropping the Client just
/// closes the channel to the per-connection task. That task is on the
/// same compio runtime as the test; when the test fn returns, the
/// runtime drops, and the task is dropped *before* it gets to send
/// Terminate / shutdown the socket. With io_uring the fd is still
/// closed (so the server eventually notices EOF), but in the meantime
/// the backend is "idle in transaction" / holding session-scoped
/// advisory locks, which blocks `pg_create_logical_replication_slot()`
/// in a later p8a2 test (logical-slot creation must drain the proc
/// array of in-flight xacts before it can take its snapshot).
///
/// Sending an explicit ROLLBACK + advisory_unlock_all over the wire
/// from within the test makes the server-side cleanup synchronous from
/// PG's perspective — the next test's slot creation never observes
/// our backend as in-transaction.
#[cfg(any(test, feature = "test-helpers"))]
#[doc(hidden)]
pub async fn clear_migration_lock_for_tests() {
    // Take the client out (if any) so we can send cleanup SQL on it
    // before drop. Best-effort: a connection that's already dead is
    // expected during the panic-recovery path and not worth treating
    // as an error.
    if let Some(client) = ctx_mut(|c| c.take_mig_client()) {
        let _ = client.batch_execute("ROLLBACK; SELECT pg_advisory_unlock_all();").await;
        drop(client);
    }
    migrations::release_active_lock();
}

/// **Test-only**: install a real Postgres client into the active
/// isolate's `IsolateDbContext::tx_conn` slot (formerly the `TX_CONN`
/// thread-local, folded into `IsolateDbContext` in Stage 8d-R4) so the
/// Gap B integration tests can drive the deferred-broker-emit
/// queue/drain machinery without standing up a V8 isolate. Returns
/// the connection-task handle so the caller can detach it.
///
/// Asynchronous because it has to open a fresh Postgres connection
/// (the same shape the production `exec_begin` does). Pair with
/// [`uninstall_tx_marker_for_tests`] to release the slot.
#[cfg(any(test, feature = "test-helpers"))]
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
pub async fn uninstall_tx_marker_for_tests() {
    if let Some(client) = ctx_mut(|c| c.take_tx_client()) {
        // Best-effort: a connection already torn down (panic recovery)
        // is fine — drop closes the fd.
        let _ = client.batch_execute("ROLLBACK").await;
        drop(client);
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
pub fn drain_pending_emits_for_tests() {
    exec::drain_pending_emits_on_commit();
}

/// **Test-only**: clear the pending-emits queue without firing
/// (rollback branch).
#[cfg(any(test, feature = "test-helpers"))]
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
