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

use std::cell::RefCell;
use std::rc::Rc;

use compio_postgres::{Client, Pool};
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

pub mod audit;
pub mod auth;
pub mod broker;
pub mod callbacks;
pub mod crud;
pub mod diff;
pub mod exec;
pub mod migrations;
pub mod orchestrator;
pub mod query;
pub mod read_set;
pub mod replication;
pub mod v8_bridge;
pub mod v8_classes;
pub mod wal_consumer;

// ---------------------------------------------------------------------------
// Thread-local state
// ---------------------------------------------------------------------------

thread_local! {
    /// Connection pool — created lazily on first DB operation.
    pub(crate) static DB_POOL: RefCell<Option<Rc<Pool>>> = const { RefCell::new(None) };

    /// Database URL — poisoned during `register()`, consumed on first pool creation.
    pub(crate) static DB_URL: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Registered models — keyed by "app_id:collection". Prevents redundant DDL
    /// on subsequent cold starts within the same deploy.
    static REGISTERED_MODELS: RefCell<std::collections::HashSet<String>> =
        RefCell::new(std::collections::HashSet::new());

    /// Active transaction connection. Only one transaction at a time per isolate
    /// (V8 is single-threaded). If Some, all CRUD ops use this connection.
    ///
    /// We store a raw [`Client`] (not a [`compio_postgres::Transaction<'a>`])
    /// because the lifetime of `Transaction<'a>` is tied to its parent
    /// [`Client`] — which cannot live inside a thread-local. Instead we issue
    /// `BEGIN`/`COMMIT`/`ROLLBACK` via `client.execute(...)` directly.
    pub(crate) static TX_CONN: RefCell<Option<Client>> = const { RefCell::new(None) };

    /// True when the active [`TX_CONN`] was opened by the auto-tx wrapper
    /// (`__zsBeginAutoTx`) — defense-in-depth read-only/serializable
    /// envelope around `query()`/`mutation()` handlers.
    ///
    /// User-driven `db.transaction(async tx => {...})` calls leave this
    /// `false`, so the auto-tx end callback never touches a user-owned tx.
    /// Conversely, if the auto-tx began the transaction, user-level
    /// `commitTransaction`/`rollbackTransaction` are NOT expected to fire
    /// — the auto-tx is opaque to user code; user-driven tx ops short
    /// out at the "nested transactions not supported" check anyway.
    pub(crate) static AUTO_TX_OWNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// Ownership token for the active transaction connection.
    ///
    /// Stamped non-zero by `begin_transaction` on success and cleared
    /// to zero by any path that drains [`TX_CONN`] (the `Transaction`
    /// v8_class's `.commit()` / `.rollback()` methods, or its
    /// Weak-finalizer-driven `Drop`).
    ///
    /// Each `Transaction` wrapper carries the token it was minted with;
    /// commit / rollback / GC all compare against the live TX_TOKEN
    /// before acting, so the wrapper never double-acts on a transaction
    /// another path already settled (e.g. an explicit `.commit()`
    /// followed by the finalizer running on GC).
    pub(crate) static TX_TOKEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// Monotonic counter feeding [`TX_TOKEN`]. Incremented inside
    /// [`next_tx_token`]; never reset (a u64 at 1 GHz tx/s would take
    /// ~584 years to wrap, so non-uniqueness within a worker lifetime
    /// is a non-issue).
    static TX_TOKEN_COUNTER: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// Broker events queued during an active transaction.
    ///
    /// While [`TX_CONN`] is `Some`, every successful CRUD mutation
    /// pushes its `ChangeEvent` here instead of calling
    /// [`crate::wal_consumer::emit_local`] directly. The transaction
    /// settle path (`Transaction::end` for user-driven tx;
    /// `exec_auto_end` for the auto-tx wrapper) drains the queue and
    /// either fires every event through `emit_local` on COMMIT or
    /// clears it on ROLLBACK. This closes the "emit-before-commit"
    /// dual-write window where a subscriber could `find()` rows that
    /// don't yet exist on disk (or that a ROLLBACK is about to undo).
    ///
    /// `None` outside a transaction; non-empty `Some(Vec<_>)` only
    /// while a tx is active. Drained atomically by `Vec::take`.
    pub(crate) static PENDING_EMITS: RefCell<Option<Vec<crate::broker::ChangeEvent>>> =
        const { RefCell::new(None) };
}

/// Allocate a fresh non-zero TX_TOKEN value. Called by
/// `callbacks::begin_transaction` right before stamping the token onto
/// the freshly-minted `Transaction` wrapper.
pub(crate) fn next_tx_token() -> u64 {
    TX_TOKEN_COUNTER.with(|c| {
        let n = c.get().wrapping_add(1);
        c.set(n);
        n
    })
}

/// Check if a model is already registered for this app on this thread.
pub(crate) fn is_model_registered(app_id: &str, collection: &str) -> bool {
    let key = format!("{app_id}:{collection}");
    REGISTERED_MODELS.with(|r| r.borrow().contains(&key))
}

/// Mark a model as registered.
pub(crate) fn mark_model_registered(app_id: &str, collection: &str) {
    let key = format!("{app_id}:{collection}");
    REGISTERED_MODELS.with(|r| { r.borrow_mut().insert(key); });
}

// The synchronous `ensure_pool(scope)` helper that used to live here
// has been removed — every callback dispatches through
// `init_pool_async()` + `DB_POOL.with(...)` directly (or the
// `callbacks::ensure_pool` async helper that wraps the same).

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
    /// wrapper whose CRUD methods call `callbacks::dispatch_*` directly.
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
        DB_URL.with(|u| {
            let mut cell = u.borrow_mut();
            let different = cell.as_deref() != Some(self.url.as_str());
            if different {
                *cell = Some(self.url.clone());
                DB_POOL.with(|p| *p.borrow_mut() = None);
            }
        });
        // Every JS-visible entry point lives on the Db v8_class
        // wrapper (see `v8_classes::db`); the registrar only installs
        // the auto-tx globals — `query()` / `mutation()` defense in
        // depth at the Postgres level around the B3 capability gate.
        r.add_setup("install_auto_tx_globals", |scope, _ns_obj| {
            callbacks::install_auto_tx_globals(scope);
        });
    }
}

/// **Test-only**: set the per-thread `DB_URL` directly, bypassing the
/// usual `DbPlugin::register()` path. Used by integration tests that
/// drive `migrations::exec_*` without spinning up a full runtime.
#[doc(hidden)]
pub fn set_db_url_for_tests(url: &str) {
    DB_URL.with(|u| *u.borrow_mut() = Some(url.to_string()));
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
    // TX_CONN.is_some()), but matches the production state machine
    // more honestly.
    let _ = client.execute("BEGIN", &[]).await;
    TX_CONN.with(|tx| *tx.borrow_mut() = Some(client));
}

/// **Test-only**: drop the `TX_CONN` slot (rolls back the dummy tx
/// server-side via connection close). Mirrors
/// [`install_tx_marker_for_tests`].
#[doc(hidden)]
pub fn uninstall_tx_marker_for_tests() {
    let client = TX_CONN.with(|tx| tx.borrow_mut().take());
    drop(client);
}

/// **Test-only**: push a `ChangeEvent` onto the pending-emits queue
/// (the same path `exec_mutation_with_emit` takes when inside a tx).
/// Used by the Gap B test to assert the drain/clear behavior without
/// running real SQL.
#[doc(hidden)]
pub fn push_pending_emit_for_tests(ev: broker::ChangeEvent) {
    PENDING_EMITS.with(|p| {
        let mut slot = p.borrow_mut();
        slot.get_or_insert_with(Vec::new).push(ev);
    });
}

/// **Test-only**: drain the pending-emits queue (fire all events
/// through `emit_local`). Exposed so the Gap B tests can drive the
/// transaction settle path's commit branch without standing up V8.
#[doc(hidden)]
pub fn drain_pending_emits_for_tests() {
    callbacks::drain_pending_emits_on_commit();
}

/// **Test-only**: clear the pending-emits queue without firing
/// (rollback branch).
#[doc(hidden)]
pub fn clear_pending_emits_for_tests() {
    callbacks::clear_pending_emits();
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
    let url = DB_URL.with(|u| u.borrow().clone());
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

    DB_POOL.with(|p| *p.borrow_mut() = Some(Rc::new(pool)));
    Ok(())
}
