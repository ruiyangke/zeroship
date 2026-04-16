use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use uuid::Uuid;

use zeroship_core::types::AppRuntimeLimits;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::{Runtime, RuntimeHandle, RuntimeLimits};

struct IsolateEntry {
    handle: RuntimeHandle,
    last_used: std::time::Instant,
}

struct AppCache {
    isolates: HashMap<Uuid, IsolateEntry>,
    max_size: usize,
}

thread_local! {
    static CACHE: RefCell<Option<AppCache>> = const { RefCell::new(None) };
    static DB_URL: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn init_cache(max_size: usize, db_url: Option<String>) {
    CACHE.with(|c| {
        *c.borrow_mut() = Some(AppCache {
            isolates: HashMap::new(),
            max_size,
        });
    });
    if let Some(url) = db_url {
        DB_URL.with(|u| *u.borrow_mut() = Some(url));
    }
}

/// Create plugins for a new Runtime.
fn create_plugins() -> Vec<Box<dyn NativePlugin>> {
    let mut plugins: Vec<Box<dyn NativePlugin>> = Vec::new();
    if DB_URL.with(|u| u.borrow().is_some()) {
        let plugin = zeroship_plugin_db::DbPlugin::new();
        let config = std::sync::Arc::new(zeroship_runtime::plugin::PluginConfig {
            db_url: DB_URL.with(|u| u.borrow().clone()),
            ..Default::default()
        });
        plugin.init(&config);
        plugins.push(Box::new(plugin));
    }
    plugins
}

/// Get or create a V8 runtime for an app. Returns None if the app isn't loaded.
pub fn get_runtime(app_id: &Uuid) -> Option<RuntimeHandle> {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let cache = cache.as_mut()?;
        if let Some(entry) = cache.isolates.get_mut(app_id) {
            entry.last_used = std::time::Instant::now();
            Some(entry.handle.clone())
        } else {
            None
        }
    })
}

pub fn get_limits(app_id: &Uuid) -> Option<RuntimeLimits> {
    get_runtime(app_id).map(|handle| handle.limits())
}

/// Load an app from bundle bytes. Creates V8 runtime + starts pump task.
pub fn load_app(app_id: Uuid, bundle_bytes: &[u8], app_limits: AppRuntimeLimits) -> bool {
    let mut bundle = match zeroship_bundle::AppBundle::from_bytes(bundle_bytes) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[worker] failed to parse bundle for {app_id}: {e}");
            return false;
        }
    };
    let modules = bundle.to_module_entries();

    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let cache = cache.as_mut().unwrap();

        // Evict LRU if at capacity
        if cache.isolates.len() >= cache.max_size && !cache.isolates.contains_key(&app_id) {
            evict_lru(cache);
        }

        // Remove old runtime if exists
        cache.isolates.remove(&app_id);

        let plugins = create_plugins();
        let mut env_vars = HashMap::new();
        env_vars.insert("APP_ID".to_string(), app_id.to_string());

        let limits = runtime_limits_from_app(&app_limits);
        let rt = Rc::new(RefCell::new(Runtime::new_with_plugins(
            modules.clone(),
            env_vars,
            limits.cpu_limit,
            limits.wall_timeout,
            plugins,
        )));
        let handle = RuntimeHandle::new(rt.clone(), limits, modules);

        // Warmup (isolate is entered after new_direct)
        {
            let result = rt
                .borrow_mut()
                .dispatch_rpc(handle.modules(), r#"{"jsonrpc":"2.0","method":"__ping","params":[],"id":0}"#);
            if let Err(e) = &result {
                eprintln!("[worker] warmup warning for {app_id}: {e}");
            }
        }

        // Exit isolate so other isolates can be created/entered on this thread.
        // The handler will enter/exit around each dispatch_rpc call.
        rt.borrow_mut().exit_isolate();

        // Start pump task for async V8 ops (timers, fetch, streams).
        Runtime::start_pump(rt.clone());
        cache.isolates.insert(
            app_id,
            IsolateEntry {
                handle,
                last_used: std::time::Instant::now(),
            },
        );

        true
    })
}

fn runtime_limits_from_app(limits: &AppRuntimeLimits) -> RuntimeLimits {
    RuntimeLimits {
        cpu_limit: limits.cpu_limit_ms.map(std::time::Duration::from_millis),
        wall_timeout: limits.wall_timeout_ms.map(std::time::Duration::from_millis),
    }
}

/// Remove an app from the cache.
#[allow(dead_code)]
pub fn evict_app(app_id: &Uuid) {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if let Some(cache) = cache.as_mut() {
            cache.isolates.remove(app_id);
        }
    });
}

/// Check if an app is loaded.
#[allow(dead_code)]
pub fn has_app(app_id: &Uuid) -> bool {
    CACHE.with(|c| {
        let cache = c.borrow();
        cache
            .as_ref()
            .is_some_and(|c| c.isolates.contains_key(app_id))
    })
}

/// Get all app IDs currently loaded in the cache.
pub fn all_app_ids() -> Vec<Uuid> {
    CACHE.with(|c| {
        let cache = c.borrow();
        cache
            .as_ref()
            .map(|c| c.isolates.keys().copied().collect())
            .unwrap_or_default()
    })
}

// Deploy hash tracking — separate thread-local map.
thread_local! {
    static HASHES: RefCell<HashMap<Uuid, String>> = RefCell::new(HashMap::new());
}

pub fn get_hash(app_id: &Uuid) -> Option<String> {
    HASHES.with(|h| h.borrow().get(app_id).cloned())
}

pub fn set_hash(app_id: Uuid, hash: String) {
    HASHES.with(|h| {
        h.borrow_mut().insert(app_id, hash);
    });
}

#[allow(dead_code)]
pub fn remove_hash(app_id: &Uuid) {
    HASHES.with(|h| {
        h.borrow_mut().remove(app_id);
    });
}

fn evict_lru(cache: &mut AppCache) {
    if let Some((&oldest_id, _)) = cache.isolates.iter().min_by_key(|(_, e)| e.last_used) {
        eprintln!("[worker] evicting LRU isolate {oldest_id}");
        cache.isolates.remove(&oldest_id);
        HASHES.with(|h| {
            h.borrow_mut().remove(&oldest_id);
        });
    }
}
