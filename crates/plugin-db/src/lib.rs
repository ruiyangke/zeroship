//! Database plugin — `appbase.db.*` native primitives.
//!
//! Provides MongoDB-style CRUD operations backed by PostgreSQL:
//! - `appbase.db.findOne(collection, filterJson)` → Promise
//! - `appbase.db.find(collection, filterJson, optsJson)` → Promise
//! - `appbase.db.insert(collection, docJson)` → Promise
//! - `appbase.db.updateOne(collection, filterJson, updateJson)` → Promise
//! - `appbase.db.deleteOne(collection, filterJson)` → Promise
//! - `appbase.db.count(collection, filterJson)` → Promise
//!
//! Each app gets its own PostgreSQL schema (`"app_id".*`) for data isolation.
//! The pool is created lazily on first use (one per worker thread).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use appbase_pg::Pool;
use appbase_runtime::plugin::{NativePlugin, NativeRegistrar, PluginConfig};

pub mod callbacks;
pub mod query;

// ---------------------------------------------------------------------------
// Thread-local state
// ---------------------------------------------------------------------------

thread_local! {
    /// Connection pool — created lazily on first DB operation.
    pub(crate) static DB_POOL: RefCell<Option<Rc<Pool>>> = const { RefCell::new(None) };

    /// Database URL — set during `init()`, consumed on first pool creation.
    pub(crate) static DB_URL: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Ensure the pool is initialized. If not, try to create it from `DB_URL`.
///
/// Returns `Some(())` on success, `None` if the pool could not be created
/// (throws a V8 exception in that case).
pub(crate) fn ensure_pool(scope: &mut v8::PinScope<'_, '_>) -> Option<()> {
    let has_pool = DB_POOL.with(|p| p.borrow().is_some());
    if has_pool {
        return Some(());
    }

    // No pool yet — we need the URL to create one.
    // But Pool::connect is async. We can't block here in a V8 callback.
    // The pool must be created before V8 callbacks run.
    //
    // If we reach here, init() was not called or the URL was not set.
    let has_url = DB_URL.with(|u| u.borrow().is_some());
    if !has_url {
        let msg = v8::String::new(
            scope,
            "db: not configured — set db_url in PluginConfig",
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

/// The database plugin — registers `appbase.db.*` methods.
pub struct DbPlugin;

impl std::fmt::Debug for DbPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbPlugin").finish()
    }
}

impl DbPlugin {
    /// Create a new `DbPlugin` instance.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for DbPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl NativePlugin for DbPlugin {
    fn namespace(&self) -> &str {
        "db"
    }

    fn name(&self) -> &str {
        "database"
    }

    fn init(&self, config: &Arc<PluginConfig>) {
        if let Some(url) = &config.db_url {
            DB_URL.with(|u| *u.borrow_mut() = Some(url.clone()));
        }
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("findOne", callbacks::find_one);
        r.add("find", callbacks::find);
        r.add("insert", callbacks::insert);
        r.add("updateOne", callbacks::update_one);
        r.add("deleteOne", callbacks::delete_one);
        r.add("count", callbacks::count);
    }

    fn shutdown(&self) {
        DB_POOL.with(|p| p.borrow_mut().take());
        DB_URL.with(|u| u.borrow_mut().take());
    }
}

/// Initialize the connection pool asynchronously.
///
/// Must be called on the compio runtime thread BEFORE any JS execution.
/// Typically called after `plugin.init(config)` but before the isolate
/// starts processing requests.
///
/// ```ignore
/// let plugin = DbPlugin::new();
/// plugin.init(&config);
/// appbase_plugin_db::init_pool_async().await;
/// // Now safe to run JS that calls appbase.db.*
/// ```
pub async fn init_pool_async() -> Result<(), String> {
    let url = DB_URL.with(|u| u.borrow().clone());
    let Some(url) = url else {
        return Ok(()); // No URL configured — DB plugin is disabled
    };

    let pool = Pool::connect(&url, 8)
        .await
        .map_err(|e| format!("db: failed to connect: {e}"))?;

    DB_POOL.with(|p| *p.borrow_mut() = Some(Rc::new(pool)));
    Ok(())
}
