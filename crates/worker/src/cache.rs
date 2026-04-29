use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

use zeroship_bundle::ModuleEntry;
use zeroship_core::types::AppRuntimeLimits;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::{Runtime, RuntimeLimits};

struct IsolateEntry {
    runtime: Runtime,
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
fn create_plugins() -> Vec<Arc<dyn NativePlugin>> {
    if let Some(url) = DB_URL.with(|u| u.borrow().clone()) {
        vec![Arc::new(zeroship_plugin_db::DbPlugin::new(url))]
    } else {
        vec![]
    }
}

/// Get or create a V8 runtime for an app. Returns None if the app isn't loaded.
pub fn get_runtime(app_id: &Uuid) -> Option<Runtime> {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let cache = cache.as_mut()?;
        if let Some(entry) = cache.isolates.get_mut(app_id) {
            entry.last_used = std::time::Instant::now();
            Some(entry.runtime.clone())
        } else {
            None
        }
    })
}

pub fn get_limits(app_id: &Uuid) -> Option<RuntimeLimits> {
    get_runtime(app_id).map(|runtime| runtime.limits())
}

/// Load an app from bundle bytes. Creates V8 runtime + starts pump task.
pub fn load_app(app_id: Uuid, bundle_bytes: &[u8], app_limits: AppRuntimeLimits) -> bool {
    // Two deploy shapes hit this entry point:
    //  - .appbundle (APPB magic) — produced by the CLI / vite plugin
    //  - raw ES module bytes — `POST /api/apps/:id/deploy` with
    //    `application/javascript`, used by the chat agent's `deploy_app`
    //    tool. Single-module case; we synthesize a one-entry module list.
    let modules: Vec<ModuleEntry> = match zeroship_bundle::AppBundle::from_bytes(bundle_bytes) {
        Ok(mut bundle) => bundle.to_module_entries(),
        Err(zeroship_bundle::BundleError::BadMagic) => {
            match std::str::from_utf8(bundle_bytes) {
                Ok(src) => vec![ModuleEntry {
                    specifier: "index.js".to_string(),
                    source: src.to_string(),
                }],
                Err(e) => {
                    eprintln!("[worker] raw module bundle is not UTF-8 for {app_id}: {e}");
                    return false;
                }
            }
        }
        Err(e) => {
            eprintln!("[worker] failed to parse bundle for {app_id}: {e}");
            return false;
        }
    };

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
        let runtime = Runtime::builder()
            .modules(modules)
            .env_vars(env_vars)
            .limits(limits)
            .plugins(plugins)
            .build();

        // Exit isolate so other isolates can be created/entered on this thread.
        // The handler will enter/exit around each call_fetch_handler call.
        // (Warmup removed — `call_fetch_handler` does lazy init via
        // `ensure_initialized` on the first request.)
        runtime.exit_isolate();

        // Start pump task for async V8 ops (timers, fetch, streams).
        runtime.start_pump();
        cache.isolates.insert(
            app_id,
            IsolateEntry {
                runtime,
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
        heap_limit_bytes: limits.heap_limit_mb.map(|mb| (mb as usize) * 1024 * 1024),
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

// Deploy hash tracking — kept thread_local because it pairs 1:1 with
// `CACHE` (which holds the `!Send` V8 Runtime). Env data + version
// both live in the process-wide `SharedEnvs` so cross-thread reconcile
// sees a single source of truth — see `sync::CachedEnv`.
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

// Env get/put + version moved to crate::sync (SharedEnvs). See
// put_env_from_json, get_env, cached_env_version there. Version no
// longer needs a per-thread tracker because it's bundled with the env
// data in CachedEnv — cross-thread reconciles dedupe correctly.

fn evict_lru(cache: &mut AppCache) {
    if let Some((&oldest_id, _)) = cache.isolates.iter().min_by_key(|(_, e)| e.last_used) {
        eprintln!("[worker] evicting LRU isolate {oldest_id}");
        crate::metrics::inc(&crate::metrics::LRU_EVICTIONS_TOTAL);
        cache.isolates.remove(&oldest_id);
        HASHES.with(|h| { h.borrow_mut().remove(&oldest_id); });
        // Env in `SharedEnvs` is process-wide and may still be needed
        // by other threads — DON'T evict it here. The version_poll_loop
        // GCs SharedEnvs against the known-app set every cycle, so an
        // app deleted from control plane gets cleaned up there.
    }
}
