//! Database plugin — backs `env.db` with a typed `#[v8_class]` surface
//! plus a few Db-scoped entry points.
//!
//! The `env.db` namespace is the `Db` v8_class instance (see
//! [`v8_classes::db`]); per-collection CRUD lives on the `Collection`
//! wrapper minted by `env.db.collection(name)`. Transactions,
//! migration runs, and reactive subscriptions are separate wrappers
//! (`Transaction` / `Migration` / `Subscription`), each with a `Drop`
//! finalizer that releases its backing resource on GC.
//!
//! The flat callbacks registered on `env.db` are intentionally
//! short — entry points that mint wrappers, plus DB-scoped ops:
//!
//! - `registerModel(collection, schema)` — DDL orchestrator (A2/A3)
//! - `collection(name)` — returns a `Collection` v8_class instance
//! - `beginTransaction(level?)` — mints a `Transaction` wrapper
//! - `migrations` (v8_getter) — returns the `Migrations` v8_class
//!   namespace exposing `.start(spec)` (mints a `Migration` wrapper) and
//!   `.status / .cancel / .reset({name, collection})` to observe an
//!   existing migration by coordinates without taking the advisory lock
//! - `openSubscription(collection)` — returns a `Subscription` wrapper
//! - `replicationSetup / Watchdog / DropAbandoned` — operator surface
//! - `startReplicationConsumer` — auto-spawn the supervised WAL consumer
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
pub mod diff;
pub mod migrations;
pub mod query;
pub mod read_set;
pub mod replication;
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
        // Per-collection CRUD lives on the Collection v8_class instance
        // returned by `env.db.collection(name)` — see `v8_classes::collection`.
        // The Db-level entry points below mint those wrappers (and the
        // Transaction / Migration / Subscription wrappers) plus the few
        // genuinely Db-scoped ops (schema registration, replication).
        r.add("registerModel", callbacks::register_model);
        // Transaction entry point. Returns a Transaction v8_class
        // instance whose `.commit()` / `.rollback()` / `.collection(n)`
        // are explicit methods; Drop auto-rollbacks via connection
        // close.
        r.add("beginTransaction", callbacks::begin_transaction);
        // Tx wrapping deferral from T1 — install the auto-tx globals
        // (`__zsBeginAutoTx` / `__zsEndAutoTx`) used by the synthetic
        // SSR entry to wrap `query()` and `mutation()` handlers with a
        // READ ONLY / READ COMMITTED tx envelope. The capability gate
        // (B3 runtime layer) is the primary enforcement; this is the
        // Postgres-level defense-in-depth around it.
        r.add_setup("install_auto_tx_globals", |scope, _ns_obj| {
            callbacks::install_auto_tx_globals(scope);
        });
        // B1 — `@zeroship/migrations`. The `Migrations` namespace
        // (`env.db.migrations`) exposes `.start(spec)` /
        // `.status(spec)` / `.cancel(spec)` / `.reset(spec)` —
        // see `v8_classes::migrations`. `.start` mints a `Migration`
        // wrapper whose `.fetchBatch()` / `.commitBatch()` drive
        // the run. Status / cancel / reset on the namespace operate
        // by `(name, collection)` coordinates without touching the
        // advisory lock — safe to call from any worker.
        // C1 / P8a — reactive queries. `openSubscription` mints a
        // Subscription v8_class wrapper whose `.pollJson()` / `.close()`
        // are methods; Weak finalizer closes the broker handle on GC.
        r.add("openSubscription", callbacks::open_subscription);
        r.add("replicationSetup", callbacks::replication_setup);
        r.add("replicationWatchdog", callbacks::replication_watchdog);
        r.add("replicationDropAbandoned", callbacks::replication_drop_abandoned);
        // P8a.2 finish-up — auto-spawn the supervised consumer. Apps
        // opt in once at module init: `await env.db.startReplicationConsumer()`.
        // Idempotent — second call short-circuits.
        r.add("startReplicationConsumer", callbacks::start_replication_consumer);
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
