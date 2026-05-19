//! Database plugin — `zeroship.db.*` native primitives.
//!
//! Provides MongoDB-style CRUD operations backed by PostgreSQL:
//! - `zeroship.db.findOne(collection, filterJson)` → Promise
//! - `zeroship.db.find(collection, filterJson, optsJson)` → Promise
//! - `zeroship.db.insert(collection, docJson)` → Promise
//! - `zeroship.db.updateOne(collection, filterJson, updateJson)` → Promise
//! - `zeroship.db.deleteOne(collection, filterJson)` → Promise
//! - `zeroship.db.count(collection, filterJson)` → Promise
//!
//! Each app gets its own PostgreSQL schema (`"app_id".*`) for data isolation.
//! The pool is created lazily on first use (one per worker thread).

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
    /// to zero by any path that drains [`TX_CONN`] (the legacy
    /// `commitTransaction` / `rollbackTransaction` flat callbacks, the
    /// `Transaction` v8_class's `.commit()` / `.rollback()` methods, or
    /// its Weak-finalizer-driven `Drop`).
    ///
    /// Each `Transaction` wrapper carries the token it was minted with;
    /// commit / rollback / GC all compare against the live TX_TOKEN
    /// before acting, so the wrapper never re-rolls a transaction that
    /// the legacy SDK already committed via the flat callbacks.
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

/// Ensure the pool is initialized. If not, try to create it from `DB_URL`.
///
/// Returns `Some(())` on success, `None` if the pool could not be created
/// (throws a V8 exception in that case).
#[allow(dead_code)]
pub(crate) fn ensure_pool(scope: &mut v8::PinScope<'_, '_>) -> Option<()> {
    let has_pool = DB_POOL.with(|p| p.borrow().is_some());
    if has_pool {
        return Some(());
    }

    // No pool yet — we need the URL to create one.
    // But Pool::connect is async. We can't block here in a V8 callback.
    // The pool must be created before V8 callbacks run.
    //
    // If we reach here, the URL was not set at plugin register-time.
    let has_url = DB_URL.with(|u| u.borrow().is_some());
    if !has_url {
        let msg = v8::String::new(
            scope,
            "db: not configured — pass a URL to DbPlugin::new()",
        )
        .unwrap();
        let exc = v8::Exception::error(scope, msg);
        scope.throw_exception(exc);
        return None;
    }

    // We have a URL but no pool. The pool should have been created in init_pool_async().
    // If we're here, the async init hasn't completed yet — this shouldn't happen in practice
    // since init() is called before any JS execution.
    let msg = v8::String::new(
        scope,
        "db: pool not ready — init_pool_async() must complete before JS execution",
    )
    .unwrap();
    let exc = v8::Exception::error(scope, msg);
    scope.throw_exception(exc);
    None
}

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

    /// Stage 2 — mint a `Db` v8_class instance as the namespace value
    /// for `env.db`. The runtime then overlays the 27 flat callbacks
    /// (registered via [`Self::register`]) on top, so existing user
    /// code accessing `zeroship.db.find(...)` continues to work
    /// unchanged. The v8_class additionally exposes `.collection(name)`
    /// which returns a `Collection` v8_class wrapper whose CRUD
    /// methods forward back to the flat callbacks.
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
        r.add("findOne", callbacks::find_one);
        r.add("find", callbacks::find);
        r.add("insert", callbacks::insert);
        r.add("insertMany", callbacks::insert_many);
        r.add("updateOne", callbacks::update_one);
        r.add("updateMany", callbacks::update_many);
        r.add("deleteOne", callbacks::delete_one);
        r.add("deleteMany", callbacks::delete_many);
        r.add("upsert", callbacks::upsert);
        r.add("count", callbacks::count);
        r.add("distinct", callbacks::distinct);
        r.add("aggregate", callbacks::aggregate);
        r.add("registerModel", callbacks::register_model);
        r.add("beginTransaction", callbacks::begin_transaction);
        r.add("commitTransaction", callbacks::commit_transaction);
        r.add("rollbackTransaction", callbacks::rollback_transaction);
        // Tx wrapping deferral from T1 — install the auto-tx globals
        // (`__zsBeginAutoTx` / `__zsEndAutoTx`) used by the synthetic
        // SSR entry to wrap `query()` and `mutation()` handlers with a
        // READ ONLY / SERIALIZABLE tx envelope. The capability gate
        // (B3 runtime layer) is the primary enforcement; this is the
        // Postgres-level defense-in-depth around it.
        r.add_setup("install_auto_tx_globals", |scope, _ns_obj| {
            callbacks::install_auto_tx_globals(scope);
        });
        // B1 — @zeroship/migrations primitives
        r.add("migrationBegin", callbacks::migration_begin);
        r.add("migrationFetchBatch", callbacks::migration_fetch_batch);
        r.add("migrationCommitBatch", callbacks::migration_commit_batch);
        r.add("migrationStatus", callbacks::migration_status);
        r.add("migrationCancel", callbacks::migration_cancel);
        r.add("migrationReset", callbacks::migration_reset);
        // Stage 3 — v8_class-backed Migration wrapper. Mints a typed
        // runner instance whose `.status()` / `.cancel()` / `.reset()`
        // delegate to the same SQL the flat callbacks use, and whose
        // GC finalizer auto-cancels the run if the wrapper is dropped
        // without explicit teardown. The flat `migrationBegin`/`…`
        // callbacks above stay registered for back-compat with the
        // existing `@zeroship/migrations` SDK.
        r.add("migrationStart", callbacks::migration_start);
        // C1 / P8a — reactive queries (in-process broker;
        // streaming WAL consumer deferred to P8a.2)
        r.add("subscribe", callbacks::subscribe);
        r.add("subscribePoll", callbacks::subscribe_poll);
        r.add("subscribeClose", callbacks::subscribe_close);
        // Stage 3 — v8_class-backed Subscription wrapper whose Weak
        // finalizer closes the broker handle on GC. The handle-id
        // triple above stays for back-compat with the existing SDK
        // AsyncIterable shim.
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
