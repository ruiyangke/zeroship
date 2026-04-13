use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use uuid::Uuid;

use appbase_runtime::plugin::NativePlugin;
use appbase_runtime::runtime::{AsyncEvent, AsyncWork, Runtime};

struct IsolateEntry {
    runtime: Rc<RefCell<Runtime>>,
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

/// Initialize async resources (DB pool). Must be called on compio runtime.
pub async fn init_async() {
    if DB_URL.with(|u| u.borrow().is_some()) {
        if let Err(e) = appbase_plugin_db::init_pool_async().await {
            eprintln!("[worker] db pool init failed: {e}");
        }
    }
}

/// Create plugins for a new Runtime.
fn create_plugins() -> Vec<Box<dyn NativePlugin>> {
    let mut plugins: Vec<Box<dyn NativePlugin>> = Vec::new();
    if DB_URL.with(|u| u.borrow().is_some()) {
        let plugin = appbase_plugin_db::DbPlugin::new();
        let config = std::sync::Arc::new(appbase_runtime::plugin::PluginConfig {
            db_url: DB_URL.with(|u| u.borrow().clone()),
            ..Default::default()
        });
        plugin.init(&config);
        plugins.push(Box::new(plugin));
    }
    plugins
}

/// Get or create a V8 runtime for an app. Returns None if the app isn't loaded.
pub fn get_runtime(app_id: &Uuid) -> Option<Rc<RefCell<Runtime>>> {
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

/// Load an app from bundle bytes. Creates V8 runtime + starts pump task.
pub fn load_app(app_id: Uuid, bundle_bytes: &[u8]) -> bool {
    let mut bundle = match appbase_bundle::AppBundle::from_bytes(bundle_bytes) {
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

        let rt = Rc::new(RefCell::new(Runtime::new_with_plugins(
            modules,
            env_vars,
            None,
            None,
            plugins,
        )));

        // Warmup (isolate is entered after new_direct)
        {
            let result = rt
                .borrow_mut()
                .dispatch_rpc(r#"{"jsonrpc":"2.0","method":"__ping","params":[],"id":0}"#);
            if let Err(e) = &result {
                eprintln!("[worker] warmup warning for {app_id}: {e}");
            }
        }

        // Exit isolate so other isolates can be created/entered on this thread.
        // The handler will enter/exit around each dispatch_rpc call.
        rt.borrow_mut().exit_isolate();

        // Start pump task for async V8 ops
        let mut async_work = AsyncWork::new();
        let (notify_tx, notify_rx) = futures::channel::mpsc::channel::<()>(1);
        rt.borrow_mut().set_pump_notify(notify_tx);
        rt.borrow_mut().drain_new_tasks_into(&mut async_work);

        let rt_pump = rt.clone();
        compio::runtime::spawn(async move {
            pump_task(rt_pump, async_work, notify_rx).await;
        })
        .detach();

        cache.isolates.insert(
            app_id,
            IsolateEntry {
                runtime: rt,
                last_used: std::time::Instant::now(),
            },
        );

        true
    })
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

/// Pump task — drives async V8 operations (timers, fetch).
/// Copied from `appbase_runtime::serve` — must stay in sync.
async fn pump_task(
    runtime: Rc<RefCell<Runtime>>,
    mut work: AsyncWork,
    mut notify_rx: futures::channel::mpsc::Receiver<()>,
) {
    use futures::StreamExt;
    loop {
        {
            let mut rt = runtime.borrow_mut();
            rt.enter_isolate();
            rt.drain_new_tasks_into(&mut work);
            rt.exit_isolate();
        }

        let event = {
            let has_ops = !work.pending_ops.is_empty();
            let has_timers = !work.pending_timers.is_empty();

            match (has_ops, has_timers) {
                (true, true) => {
                    futures::select! {
                        r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                        r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (true, false) => {
                    futures::select! {
                        r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (false, true) => {
                    futures::select! {
                        r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (false, false) => {
                    let _ = notify_rx.next().await;
                    None
                }
            }
        };

        if let Some(event) = event {
            let mut rt = runtime.borrow_mut();
            rt.enter_isolate();
            rt.handle_async_event(event, &mut work);
            rt.exit_isolate();
        }
    }
}
