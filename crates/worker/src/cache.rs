use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use uuid::Uuid;

use zeroship_core::types::AppRuntimeLimits;
use zeroship_plugin_storage::StorageBackendConfig;
use zeroship_runtime::ModuleEntry;
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
    /// Redis connection URL for the multi-node KV backend. Held per-thread
    /// like `DB_URL`. When `Some`, `create_plugins` mints a `KvPlugin`
    /// backed by `Redis` (shared across every worker node — see the
    /// backend-choice rationale on `init_cache`). When `None`, the `kv`
    /// namespace is simply absent (degrade, don't panic).
    static KV_URL: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Parsed object-storage backend config for the `env.storage` namespace.
    /// Held per-thread like `DB_URL`. When `Some`, `create_plugins` mints a
    /// `StoragePlugin` over the selected backend: `LocalFs` (a SHARED volume
    /// across nodes — the same multi-node pattern the deploy blob store uses)
    /// or `S3` (S3/R2/MinIO — inherently shared). When `None`, the `storage`
    /// namespace is absent.
    static STORAGE_BACKEND: RefCell<Option<StorageBackendConfig>> = const { RefCell::new(None) };
    /// The PROCESS-WIDE usage meter, cloned into every ntex worker thread's
    /// thread-local on `init_cache`. Metering is INFRASTRUCTURE: there is no
    /// creator-facing `env.meter` namespace. Instead `create_plugins` binds
    /// this ONE `Arc<Meter>` into the db/kv/storage plugin constructors, so
    /// each trusted primitive emits raw usage metrics (db_writes, kv_reads,
    /// storage_ops, …) at its op boundary — app code can neither forge nor
    /// suppress them. The worker itself feeds the five platform counters via
    /// `record_request`. All of it lands in this single place that the one
    /// per-process flush task drains. `None` until `init_cache` runs.
    static METER: RefCell<Option<Arc<zeroship_metering::Meter>>> = const { RefCell::new(None) };
}

/// Per-thread runtime-kernel config the worker threads each install.
///
/// All four backend handles degrade independently: an absent `db_url` /
/// `kv_url` / `storage_backend` means that `env.*` namespace is simply not
/// registered (matching the long-standing DB behaviour), never a panic.
/// The real multi-node stack SHOULD set all of them so deployed apps get
/// the complete `env.{db,kv,storage,auth}` kernel.
pub struct KernelConfig {
    pub db_url: Option<String>,
    pub kv_url: Option<String>,
    pub storage_backend: Option<StorageBackendConfig>,
    /// The process-wide usage meter shared with the per-process flush task
    /// (see `main`). Always set in the real worker; an `Arc<Meter>` is
    /// cheap so there is no "absent" tier — the namespace is registered
    /// unconditionally when present.
    pub meter: Arc<zeroship_metering::Meter>,
}

pub fn init_cache(max_size: usize, kernel: KernelConfig) {
    CACHE.with(|c| {
        *c.borrow_mut() = Some(AppCache {
            isolates: HashMap::new(),
            max_size,
        });
    });
    if let Some(url) = kernel.db_url {
        DB_URL.with(|u| *u.borrow_mut() = Some(url));
    }
    if let Some(url) = kernel.kv_url {
        KV_URL.with(|u| *u.borrow_mut() = Some(url));
    }
    if let Some(backend) = kernel.storage_backend {
        STORAGE_BACKEND.with(|s| *s.borrow_mut() = Some(backend));
    }
    METER.with(|m| *m.borrow_mut() = Some(kernel.meter));
}

/// Create plugins for a new Runtime — the kernel every deployed app boots
/// against. This is the SINGLE source of truth for the multi-node `env.*`
/// surface; the CLI `zeroship serve` vector (`crates/cli/src/main.rs`)
/// mirrors it for the single-tenant dev path.
///
/// Namespaces and their multi-node backend choices:
/// - `auth` — pushed unconditionally. Stateless (callbacks read the
///   per-request user from `RuntimeState`), so every app gets a working
///   `env.auth.getUser()` / `env.auth.requireUser()`.
/// - `db` — pushed when a DB URL is configured. Postgres is inherently
///   shared across nodes.
/// - `kv` — pushed when a KV (Redis) URL is configured, backed by the
///   `Redis` backend. **Redis is chosen, NOT the single-tenant embedded
///   `redb` backend**: redb is a per-process file lock, so on an N-node
///   worker pool each node would hold its own divergent KV (a `set` on
///   node A invisible on node B). Redis is a shared network store — one
///   logical keyspace across every node — so KV stays consistent. The
///   `Redis` backend's URL transparently selects single-node
///   (`redis://host`) or cluster (`?cluster=true`) mode.
/// - `storage` — pushed when a storage backend is configured (`--storage-url`).
///   `LocalFs` gets multi-node consistency from rooting its path on a SHARED
///   volume — the exact pattern the deploy blob store already uses
///   (control/gateway/worker all mount the same `bundles` volume). `S3`
///   (S3/R2/MinIO) is the prod backend behind the same `Backend` trait and is
///   inherently shared across nodes. An object written on node A is readable
///   on node B in both cases.
fn create_plugins() -> Vec<Arc<dyn NativePlugin>> {
    let mut plugins: Vec<Arc<dyn NativePlugin>> = Vec::new();
    // The process-wide meter, if configured. Metering is infrastructure:
    // rather than a creator-facing `env.meter` namespace, the meter is
    // bound into the db/kv/storage producers so each trusted primitive
    // emits a raw usage metric at its op boundary (per-app scoped at mint
    // time via the runtime's server-injected APP_ID). The five platform
    // counters keep flowing through `record_request` (below, unchanged).
    let meter = METER.with(|m| m.borrow().clone());
    if let Some(url) = DB_URL.with(|u| u.borrow().clone()) {
        plugins.push(Arc::new(zeroship_plugin_db::DbPlugin::new(url, meter.clone())));
    }
    if let Some(url) = KV_URL.with(|u| u.borrow().clone()) {
        plugins.push(Arc::new(zeroship_plugin_kv::KvPlugin::with_backend_and_meter(
            Arc::new(zeroship_plugin_kv::Redis::new(url)),
            meter.clone(),
        )));
    }
    if let Some(cfg) = STORAGE_BACKEND.with(|s| s.borrow().clone()) {
        match zeroship_plugin_storage::build_backend(&cfg) {
            Ok(backend) => plugins.push(Arc::new(
                zeroship_plugin_storage::StoragePlugin::with_backend_and_meter(
                    backend,
                    meter.clone(),
                ),
            )),
            Err(e) => {
                // Credentials were validated at boot (see worker main); a
                // failure here means the env changed under us. Degrade the
                // namespace rather than crash the isolate.
                tracing::error!(error = %e, "env.storage backend init failed; namespace absent");
            }
        }
    }
    plugins.push(Arc::new(zeroship_runtime::auth::AuthPlugin));
    plugins
}

/// Auto-counter hook: record one completed dispatched request's platform
/// counters against the process-wide meter for `app_id`. Called by the
/// dispatch handler once a request has been served. No-op when the meter is
/// unset (degraded config).
///
/// Feeds all five platform auto-counters in one shot via
/// [`Meter::record_request`] — `requests` (always +1) plus the four the
/// dispatch handler measures per request:
/// - `cpu_us` — V8 thread CPU microseconds (CLOCK_THREAD_CPUTIME_ID delta
///   around the synchronous isolate entry; the same clock the CPU limiter
///   uses),
/// - `wall_us` — wall-clock microseconds spanning the dispatch,
/// - `egress_bytes` — response body bytes the worker produced,
/// - `ingress_bytes` — request body bytes the worker received.
pub fn record_request(
    app_id: &Uuid,
    cpu_us: u64,
    wall_us: u64,
    egress_bytes: u64,
    ingress_bytes: u64,
) {
    METER.with(|m| {
        if let Some(meter) = m.borrow().as_ref() {
            meter.record_request(
                &app_id.to_string(),
                cpu_us,
                wall_us,
                egress_bytes,
                ingress_bytes,
            );
        }
    });
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
///
/// Bundle bytes are the raw ES module source (UTF-8). The caller
/// (`sync::reconcile_once` or `handler::load_on_demand`) resolves the
/// worker-entry blob hash from the manifest and reads it via
/// `BlobStore::get_blob` before calling here. Single-module bundles
/// (today's only shape) become a one-element `modules` vector tagged
/// `index.js`; multi-module deploys will pass a richer slice once the
/// V8 module-resolve callback lands.
pub fn load_app(app_id: Uuid, bundle_bytes: &[u8], app_limits: AppRuntimeLimits) -> bool {
    let source = match std::str::from_utf8(bundle_bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(app_id = %app_id, error = %e, "worker: bundle is not UTF-8");
            return false;
        }
    };
    let modules: Vec<ModuleEntry> = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: source.to_string(),
    }];

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
        // Pass `app_id` so the runtime's RPC fast path can register
        // every in-flight `AbortController` with `crate::rpc::abort`,
        // keyed by `(app_id, request_id)`. `evict_lru` walks that
        // registry on eviction.
        let runtime = Runtime::builder()
            .modules(modules)
            .env_vars(env_vars)
            .limits(limits)
            .plugins(plugins)
            .app_id(app_id)
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

pub fn runtime_limits_from_app(limits: &AppRuntimeLimits) -> RuntimeLimits {
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

// Loaded-state tracking — kept thread_local because it pairs 1:1 with
// `CACHE` (which holds the `!Send` V8 Runtime). The env DATA + its
// current version live in the process-wide `SharedEnvs` (see
// `sync::CachedEnv`); `LoadedMeta.env_version` is deliberately separate:
// it records which env version THIS thread's isolate was hydrated
// against, so reconcile can tell "SharedEnvs is fresh" apart from "the
// running isolate has actually materialized it" (SEC-7).

/// What a cached isolate was loaded against: the deploy hash (code) and
/// the env version (vars/secrets). `sync::needs_reload` compares both
/// with the control plane's current `AppVersionInfo` to decide whether
/// the isolate must be swapped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedMeta {
    /// Canonical manifest hash of the deploy the isolate runs. `None`
    /// when the control plane reported no deploy hash at load time.
    pub deploy_hash: Option<String>,
    /// Control-plane monotonic env counter the isolate's env was
    /// hydrated against.
    pub env_version: i64,
}

thread_local! {
    static LOADED_META: RefCell<HashMap<Uuid, LoadedMeta>> = RefCell::new(HashMap::new());
}

pub fn get_loaded_meta(app_id: &Uuid) -> Option<LoadedMeta> {
    LOADED_META.with(|m| m.borrow().get(app_id).cloned())
}

pub fn set_loaded_meta(app_id: Uuid, meta: LoadedMeta) {
    LOADED_META.with(|m| {
        m.borrow_mut().insert(app_id, meta);
    });
}

pub fn remove_loaded_meta(app_id: &Uuid) {
    LOADED_META.with(|m| {
        m.borrow_mut().remove(app_id);
    });
}

fn evict_lru(cache: &mut AppCache) {
    if let Some((&oldest_id, _)) = cache.isolates.iter().min_by_key(|(_, e)| e.last_used) {
        tracing::info!(app_id = %oldest_id, "worker: evicting LRU isolate");
        crate::metrics::inc(&crate::metrics::LRU_EVICTIONS_TOTAL);

        // Fire every in-flight `AbortController` for this app BEFORE
        // removing the isolate. User code awaiting a fetch /
        // `setTimeout` / `addEventListener("abort")` gets one V8 turn
        // to observe the cancellation; the synchronous abort dispatch
        // runs inside `with_scope`. The current implementation stops at
        // abort fan-out; a drain timer and explicit disposed state can
        // be added later if eviction needs to become more graceful.
        if let Some(entry) = cache.isolates.get(&oldest_id) {
            entry.runtime.with_scope(|scope| {
                zeroship_runtime::rpc::abort::entered_for_eviction(scope, oldest_id);
            });
        }

        cache.isolates.remove(&oldest_id);
        LOADED_META.with(|m| { m.borrow_mut().remove(&oldest_id); });
        // Env in `SharedEnvs` is process-wide and may still be needed
        // by other threads — DON'T evict it here. The version_poll_loop
        // GCs SharedEnvs against the known-app set every cycle, so an
        // app deleted from control plane gets cleaned up there.
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Slice 1a regression guard: every Runtime the worker builds must
    /// carry the `auth` namespace so `env.auth.getUser()` resolves for
    /// production end-user apps. The faithful e2e drives `env.auth`
    /// through this very vector — assert it's present here so a future
    /// edit that drops `AuthPlugin` from `create_plugins()` fails loudly,
    /// not just under `zeroship serve` (the CLI vector). `AuthPlugin` is
    /// stateless, so it is pushed even when no DB URL is configured.
    #[test]
    fn create_plugins_registers_auth_namespace() {
        let plugins = create_plugins();
        assert!(
            plugins.iter().any(|p| p.namespace() == "auth"),
            "worker create_plugins() must include the auth namespace; got: {:?}",
            plugins.iter().map(|p| p.namespace()).collect::<Vec<_>>()
        );
    }

    /// Phase-2 structural guard (no external services): when the kernel
    /// config carries a DB URL, a KV URL, and a storage root, the SAME
    /// `create_plugins()` a deployed app boots against installs all four
    /// `env.{db,kv,storage,auth}` namespaces. This is the always-runnable
    /// complement to the redis-gated faithful dispatch test in
    /// `handler.rs` — it asserts the plugin VECTOR, the latter asserts the
    /// JS namespaces resolve + round-trip end-to-end.
    ///
    /// Pre-Phase-2 this FAILS: `create_plugins()` ignored kv/storage
    /// entirely, so `kv` and `storage` were never in the vector. Runs on a
    /// fresh thread so the kernel thread-locals don't leak into other
    /// tests sharing this thread.
    #[test]
    fn create_plugins_registers_full_kernel_when_configured() {
        std::thread::spawn(|| {
            init_cache(
                4,
                KernelConfig {
                    db_url: Some("postgres://localhost/zs_unused".to_string()),
                    kv_url: Some("redis://127.0.0.1:6379".to_string()),
                    storage_backend: Some(StorageBackendConfig::Local(PathBuf::from(
                        "/tmp/zs-cache-test-storage",
                    ))),
                    meter: Arc::new(zeroship_metering::Meter::new()),
                },
            );
            let plugins = create_plugins();
            let namespaces: Vec<String> =
                plugins.iter().map(|p| p.namespace().to_string()).collect();
            // Metering is infrastructure now: there is NO `meter` namespace.
            // The meter is bound INTO the db/kv/storage producers, so the
            // creator surface is exactly these four namespaces.
            for expected in ["db", "kv", "storage", "auth"] {
                assert!(
                    namespaces.iter().any(|n| n == expected),
                    "create_plugins() must register the '{expected}' namespace when configured; got: {namespaces:?}"
                );
            }
            assert!(
                !namespaces.iter().any(|n| n == "meter"),
                "metering is infrastructure: there must be NO env.meter namespace; got: {namespaces:?}"
            );
        })
        .join()
        .expect("kernel-config plugin guard thread panicked");
    }

    /// Degrade-don't-panic: with no kv/storage configured (only db), the
    /// kv + storage namespaces are simply absent — `create_plugins()`
    /// never panics. Mirrors the long-standing DB behaviour. Fresh thread
    /// keeps the empty kernel thread-locals isolated.
    #[test]
    fn create_plugins_omits_kv_storage_when_unconfigured() {
        std::thread::spawn(|| {
            init_cache(
                4,
                KernelConfig {
                    db_url: None,
                    kv_url: None,
                    storage_backend: None,
                    // The meter is always provided (an `Arc<Meter>` is cheap;
                    // there is no degraded "no meter" tier). It is bound into
                    // the producers rather than exposed as a namespace, so it
                    // adds NO entry to the plugin vector.
                    meter: Arc::new(zeroship_metering::Meter::new()),
                },
            );
            let plugins = create_plugins();
            let namespaces: Vec<String> =
                plugins.iter().map(|p| p.namespace().to_string()).collect();
            let has = |n: &str| namespaces.iter().any(|x| x == n);
            // auth is unconditional; kv/storage/db must NOT appear; and there
            // is NO `meter` namespace (metering is infrastructure).
            assert!(has("auth"));
            assert!(!has("meter"), "metering is infrastructure: no env.meter namespace");
            assert!(!has("kv"), "kv absent when unconfigured");
            assert!(!has("storage"), "storage absent when unconfigured");
            assert!(!has("db"), "db absent when unconfigured");
        })
        .join()
        .expect("degrade guard thread panicked");
    }
}
