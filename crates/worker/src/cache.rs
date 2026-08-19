use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use uuid::Uuid;

use zeroship_core::types::{AppNetPolicy, AppRuntimeLimits};
use zeroship_plugin_storage::StorageBackendConfig;
use zeroship_core::net_policy::{EgressRule, Verdict};
use zeroship_runtime::{EnvSnapshot, ModuleEntry, NetPolicy};
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::{Runtime, RuntimeLimits};

struct IsolateEntry {
    runtime: Runtime,
    last_used: std::time::Instant,
    app_id: String,
}

struct AppCache {
    isolates: HashMap<Uuid, IsolateEntry>,
    workflow_isolates: HashMap<PinnedWorkflowKey, IsolateEntry>,
    max_size: usize,
    max_pinned_isolates_per_app: usize,
}

#[derive(Clone, Debug, Eq)]
pub struct PinnedWorkflowKey {
    app_id: Uuid,
    deploy_hash: String,
}

impl PinnedWorkflowKey {
    fn new(app_id: Uuid, deploy_hash: impl Into<String>) -> Self {
        Self {
            app_id,
            deploy_hash: deploy_hash.into(),
        }
    }
}

impl PartialEq for PinnedWorkflowKey {
    fn eq(&self, other: &Self) -> bool {
        self.app_id == other.app_id && self.deploy_hash == other.deploy_hash
    }
}

impl Hash for PinnedWorkflowKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.app_id.hash(state);
        self.deploy_hash.hash(state);
    }
}

thread_local! {
    static CACHE: RefCell<Option<AppCache>> = const { RefCell::new(None) };
    static DB_URL: RefCell<Option<String>> = const { RefCell::new(None) };
    static CDC_WORKER_ID: RefCell<Option<String>> = const { RefCell::new(None) };
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
    /// Control-plane endpoint and raw control key used only to derive
    /// app-scoped workflow tokens in `WorkflowPlugin::build_instance`.
    /// The raw key stays in Rust process memory and is never exposed to V8.
    static CONTROL_URL: RefCell<Option<String>> = const { RefCell::new(None) };
    static CONTROL_KEY: RefCell<Option<String>> = const { RefCell::new(None) };
    /// The PROCESS-WIDE usage meter, cloned into every ntex worker thread's
    /// thread-local on `init_cache`. Metering is INFRASTRUCTURE: there is no
    /// creator-facing `env.meter` namespace. Instead `create_plugins` binds
    /// this ONE `Arc<Meter>` into the db/kv/storage plugin constructors, so
    /// each trusted primitive emits raw usage metrics (db_writes, kv_reads,
    /// storage_ops, …) at its op boundary — app code can neither forge nor
    /// suppress them. The worker itself feeds the five platform counters via
    /// `record_request`. All of it lands in this single place that the one
    /// per-process usage-event outbox drains. `None` until `init_cache` runs.
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
    pub control_url: String,
    pub control_key: String,
    pub db_url: Option<String>,
    pub cdc_worker_id: String,
    pub kv_url: Option<String>,
    pub storage_backend: Option<StorageBackendConfig>,
    /// The process-wide usage meter shared with the per-process outbox task
    /// (see `main`). Always set in the real worker; an `Arc<Meter>` is
    /// cheap so there is no "absent" tier — the namespace is registered
    /// unconditionally when present.
    pub meter: Arc<zeroship_metering::Meter>,
}

pub fn init_cache(max_size: usize, max_pinned_isolates_per_app: usize, kernel: KernelConfig) {
    CACHE.with(|c| {
        *c.borrow_mut() = Some(AppCache {
            isolates: HashMap::new(),
            max_size,
            workflow_isolates: HashMap::new(),
            max_pinned_isolates_per_app,
        });
    });
    if let Some(url) = kernel.db_url {
        DB_URL.with(|u| *u.borrow_mut() = Some(url));
    }
    CDC_WORKER_ID.with(|id| *id.borrow_mut() = Some(kernel.cdc_worker_id));
    if let Some(url) = kernel.kv_url {
        KV_URL.with(|u| *u.borrow_mut() = Some(url));
    }
    if let Some(backend) = kernel.storage_backend {
        STORAGE_BACKEND.with(|s| *s.borrow_mut() = Some(backend));
    }
    CONTROL_URL.with(|u| *u.borrow_mut() = Some(kernel.control_url));
    CONTROL_KEY.with(|k| *k.borrow_mut() = Some(kernel.control_key));
    METER.with(|m| *m.borrow_mut() = Some(kernel.meter));
}

pub fn db_url() -> Option<String> {
    DB_URL.with(|u| u.borrow().clone())
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
        let worker_id = CDC_WORKER_ID.with(|id| {
            id.borrow()
                .clone()
                .expect("CDC worker id must be installed with the kernel")
        });
        plugins.push(Arc::new(zeroship_plugin_db::DbPlugin::new(
            url,
            meter.clone(),
            worker_id,
        )));
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
    if let Some(control_url) = CONTROL_URL.with(|u| u.borrow().clone()) {
        let control_key = CONTROL_KEY.with(|k| k.borrow().clone()).unwrap_or_default();
        plugins.push(Arc::new(zeroship_plugin_workflow::WorkflowPlugin::new(
            control_url,
            control_key,
        )));
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
/// [`zeroship_metering::Meter::record_request`] — `requests` (always +1) plus the four the
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
            let recorded = CACHE.with(|c| {
                let cache = c.borrow();
                let Some(app_id) = cache
                    .as_ref()
                    .and_then(|cache| cache.isolates.get(app_id))
                    .map(|entry| entry.app_id.as_str())
                else {
                    return false;
                };
                meter.record_request(app_id, cpu_us, wall_us, egress_bytes, ingress_bytes);
                true
            });
            if !recorded {
                let id = app_id.to_string();
                meter.record_request(&id, cpu_us, wall_us, egress_bytes, ingress_bytes);
            }
        }
    });
}

/// Observability-only durable-workflow step volume. Billing parity rides the
/// fixed `requests` counter from `record_request`; this custom metric lets
/// operators inspect workflow replay volume without double-counting it.
pub fn record_workflow_step(app_id: &Uuid) {
    METER.with(|m| {
        if let Some(meter) = m.borrow().as_ref() {
            meter.increment(&app_id.to_string(), "workflow_steps", 1);
        }
    });
}

/// Record a successful workflow output blob write. The workflow blob store is
/// a trusted platform storage path, so it uses the same storage usage counters
/// as the native storage primitive.
pub fn record_workflow_blob_write(app_id: &Uuid, bytes: u64) {
    METER.with(|m| {
        if let Some(meter) = m.borrow().as_ref() {
            let id = app_id.to_string();
            meter.increment(&id, "storage_ops", 1);
            meter.increment(&id, "storage_bytes", bytes);
        }
    });
}

/// Record an INCREMENTAL streaming-usage delta (metering coverage #27, H1).
///
/// A long-lived SSE/streaming response accrues `egress_bytes` and
/// `stream_wall_us` continuously while it is open, so (a) usage bills
/// throughout a multi-hour stream instead of only at close, and (b) a worker
/// crash loses at most one recording interval's delta rather than the whole
/// stream. The streaming drain task (`handler::stream_response`, in the worker binary)
/// calls this every ~10s / ~1 MiB and once more at finalize with the
/// trailing delta.
///
/// `requests`/`cpu_us`/`ingress_bytes` are NOT touched here — they are the
/// unary parts of the request, recorded EXACTLY ONCE at stream start via
/// [`record_request`]. A stream is one request, so `requests` must never be
/// re-incremented per delta. `stream_wall_us` is a distinct metric from the
/// unary `wall_us` (the held-open duration is priced/observed on its own).
/// No-op when the meter is unset (degraded config) or both deltas are zero.
pub fn record_stream_delta(app_id: &Uuid, egress_bytes_delta: u64, stream_wall_us_delta: u64) {
    if egress_bytes_delta == 0 && stream_wall_us_delta == 0 {
        return;
    }
    METER.with(|m| {
        if let Some(meter) = m.borrow().as_ref() {
            let id = app_id.to_string();
            if egress_bytes_delta > 0 {
                meter.increment(&id, "egress_bytes", egress_bytes_delta);
            }
            if stream_wall_us_delta > 0 {
                meter.increment(&id, "stream_wall_us", stream_wall_us_delta);
            }
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

/// Read an app's runtime limits WITHOUT marking it recently used.
///
/// This is metadata, not a dispatch. Reconciliation calls it for every locally
/// cached app on every cycle, so routing it through `get_runtime` stamped
/// `last_used` on the whole cache each pass, in whatever order the map iterated.
/// Recency then reflected the last sweep rather than real traffic, and eviction
/// could take a hot app while keeping an idle one.
pub fn get_limits(app_id: &Uuid) -> Option<RuntimeLimits> {
    CACHE.with(|c| {
        let cache = c.borrow();
        limits_without_touching_recency(cache.as_ref()?, app_id)
    })
}

/// The read above, over a borrowed cache so the no-touch property is testable.
fn limits_without_touching_recency(cache: &AppCache, app_id: &Uuid) -> Option<RuntimeLimits> {
    cache.isolates.get(app_id).map(|entry| entry.runtime.limits())
}

pub fn get_workflow_runtime(app_id: &Uuid, deploy_hash: &str) -> Option<Runtime> {
    let key = PinnedWorkflowKey::new(*app_id, deploy_hash);
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let cache = cache.as_mut()?;
        if let Some(entry) = cache.workflow_isolates.get_mut(&key) {
            entry.last_used = std::time::Instant::now();
            Some(entry.runtime.clone())
        } else {
            None
        }
    })
}

fn build_runtime(
    app_id: Uuid,
    bundle_bytes: &[u8],
    app_limits: AppRuntimeLimits,
    app_net_policy: AppNetPolicy,
    deploy_hash: Option<&str>,
    runtime_descriptor: Option<&str>,
    env: &EnvSnapshot,
) -> Result<(Runtime, String), String> {
    let source = match std::str::from_utf8(bundle_bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(app_id = %app_id, error = %e, "worker: bundle is not UTF-8");
            return Err(format!("bundle is not UTF-8: {e}"));
        }
    };
    let modules: Vec<ModuleEntry> = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: source.to_string(),
    }];

    let plugins = create_plugins();
    let meter = METER.with(|m| m.borrow().clone());
    let app_id_string = app_id.to_string();
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id_string.clone());
    // **T6** — inject the per-app deploy/schema-version token so plugin-db's
    // deploy-keyed introspection cache keys off the real deploy hash. Workflow
    // replay also uses this slot, but with the run's pinned deploy hash.
    if let Some(dh) = deploy_hash {
        env_vars.insert("ZEROSHIP_DEPLOY_ID".to_string(), dh.to_string());
    }

    let limits = runtime_limits_from_app(&app_limits);
    let net_policy = net_policy_from_app(&app_id, &app_net_policy);
    let mut builder = Runtime::builder()
        .modules(modules)
        .env_vars(env_vars)
        .limits(limits)
        .plugins(plugins)
        .app_id(app_id)
        .net_policy(net_policy)
        .runtime_descriptor(runtime_descriptor.map(str::to_string));
    if let Some(meter) = meter {
        builder = builder.meter(meter);
    }
    let runtime = builder.build();
    runtime
        .initialize(env)
        .map_err(|e| format!("failed to initialize app runtime: {e}"))?;

    // Exit isolate so other isolates can be created/entered on this thread.
    runtime.exit_isolate();

    Ok((runtime, app_id_string))
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
pub fn load_app(
    app_id: Uuid,
    bundle_bytes: &[u8],
    app_limits: AppRuntimeLimits,
    app_net_policy: AppNetPolicy,
    deploy_hash: Option<&str>,
    runtime_descriptor: Option<&str>,
    env: &EnvSnapshot,
) -> Result<(), String> {
    let (runtime, app_id_string) = build_runtime(
        app_id,
        bundle_bytes,
        app_limits,
        app_net_policy,
        deploy_hash,
        runtime_descriptor,
        env,
    )?;

    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let cache = cache.as_mut().unwrap();

        // Only mutate the cache after the new runtime has initialized. A
        // corrupt descriptor during reload must not evict the last-good isolate.
        if cache.isolates.len() >= cache.max_size
            && !cache.isolates.contains_key(&app_id)
            && !evict_lru(cache)
        {
            tracing::warn!(
                app_id = %app_id,
                max_size = cache.max_size,
                "worker: isolate cache full and every isolate is leased; load deferred"
            );
            return Err("isolate cache full and every isolate is leased; load deferred".into());
        }

        // Start pump task for async V8 ops (timers, fetch, streams) only after
        // capacity is available and the runtime is about to become reachable.
        runtime.start_pump();
        cache.isolates.insert(
            app_id,
            IsolateEntry {
                runtime,
                last_used: std::time::Instant::now(),
                app_id: app_id_string,
            },
        );

        Ok(())
    })
}

pub fn load_pinned_workflow_app(
    app_id: Uuid,
    deploy_hash: &str,
    bundle_bytes: &[u8],
    app_limits: AppRuntimeLimits,
    app_net_policy: AppNetPolicy,
    runtime_descriptor: Option<&str>,
    env: &EnvSnapshot,
) -> Result<(), String> {
    let key = PinnedWorkflowKey::new(app_id, deploy_hash);
    let (runtime, app_id_string) = build_runtime(
        app_id,
        bundle_bytes,
        app_limits,
        app_net_policy,
        Some(deploy_hash),
        runtime_descriptor,
        env,
    )?;

    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        let cache = cache.as_mut().unwrap();
        if cache.max_pinned_isolates_per_app == 0 {
            return Err("pinned workflow isolate cache is disabled".to_string());
        }

        if !cache.workflow_isolates.contains_key(&key) {
            while pinned_count_for_app(cache, &app_id) >= cache.max_pinned_isolates_per_app {
                if !evict_pinned_lru_for_app(cache, &app_id) {
                    tracing::warn!(
                        app_id = %app_id,
                        deploy_hash = %deploy_hash,
                        max_pinned_isolates_per_app = cache.max_pinned_isolates_per_app,
                        "worker: pinned workflow isolate cache full and every isolate is leased"
                    );
                    return Err("pinned workflow isolate cache full and every isolate is leased".into());
                }
            }
        }

        runtime.start_pump();
        cache.workflow_isolates.insert(
            key,
            IsolateEntry {
                runtime,
                last_used: std::time::Instant::now(),
                app_id: app_id_string,
            },
        );

        Ok(())
    })
}

pub fn runtime_limits_from_app(limits: &AppRuntimeLimits) -> RuntimeLimits {
    RuntimeLimits {
        cpu_limit: limits.cpu_limit_ms.map(std::time::Duration::from_millis),
        wall_timeout: limits.wall_timeout_ms.map(std::time::Duration::from_millis),
        heap_limit_bytes: limits.heap_limit_mb.map(|mb| (mb as usize) * 1024 * 1024),
    }
}

/// Rebuild the runtime egress policy from the control-plane projection.
///
/// Every row is re-parsed through `EgressRule::parse`, the SAME authoring
/// boundary the creator-facing API applies, so a hand-edited database row cannot
/// inject a rule the API would have refused. An individually invalid row is
/// dropped rather than failing the whole app; a rule set that is empty after
/// that is `Denied`.
///
/// Dropping a bad row is safe in ONE direction only, and the direction matters:
/// a dropped ACCEPT narrows what the app can reach, so the failure mode is the
/// app's own traffic breaking. A dropped REJECT would WIDEN it, which is why an
/// unparseable REJECT denies the app outright instead.
pub fn net_policy_from_app(app_id: &Uuid, app_net: &AppNetPolicy) -> NetPolicy {
    if app_net.egress.is_empty() {
        return NetPolicy::Denied;
    }

    let mut rules = Vec::with_capacity(app_net.egress.len());
    for entry in &app_net.egress {
        match EgressRule::parse(entry.verdict, &entry.destination, entry.port) {
            Ok(rule) => rules.push(rule),
            Err(err) => {
                tracing::error!(
                    app_id = %app_id,
                    verdict = entry.verdict.as_str(),
                    destination = %entry.destination,
                    port = entry.port,
                    error = %err,
                    "worker: egress rule rejected at load"
                );
                if entry.verdict == Verdict::Reject {
                    tracing::error!(
                        app_id = %app_id,
                        "worker: an unparseable REJECT rule would widen reach if skipped; \
                         denying raw TCP"
                    );
                    return NetPolicy::Denied;
                }
            }
        }
    }

    if rules.is_empty() {
        return NetPolicy::Denied;
    }

    match NetPolicy::rules(rules, app_net.max_sockets, app_net.egress_ceiling_bytes) {
        Ok(policy) => policy,
        Err(err) => {
            tracing::error!(
                app_id = %app_id,
                error = %err,
                "worker: egress rule set rejected at load; denying raw TCP"
            );
            NetPolicy::Denied
        }
    }
}

/// Remove an app from the cache.
#[allow(dead_code)]
pub fn evict_app(app_id: &Uuid) {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if let Some(cache) = cache.as_mut() {
            cache.isolates.remove(app_id);
            cache.workflow_isolates.retain(|key, _| &key.app_id != app_id);
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

#[cfg(test)]
pub fn has_pinned_workflow_app(app_id: &Uuid, deploy_hash: &str) -> bool {
    let key = PinnedWorkflowKey::new(*app_id, deploy_hash);
    CACHE.with(|c| {
        let cache = c.borrow();
        cache
            .as_ref()
            .is_some_and(|c| c.workflow_isolates.contains_key(&key))
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
/// the env version (vars/secrets). `sync::needs_reload` compares these
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
    /// Worker-facing raw-TCP policy snapshot the isolate was built with.
    pub net_policy: AppNetPolicy,
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

fn evict_lru(cache: &mut AppCache) -> bool {
    refresh_socket_activity(cache);

    let Some(oldest_id) = cache
        .isolates
        .iter()
        .filter(|(_, entry)| !entry.runtime.is_isolate_leased())
        .min_by_key(|(_, entry)| {
            (
                entry.runtime.active_native_socket_count() > 0,
                entry.last_used,
            )
        })
        .map(|(id, _)| *id)
    else {
        return false;
    };

    {
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
            let active_sockets = entry.runtime.active_native_socket_count();
            if active_sockets > 0 {
                let closed = entry.runtime.close_native_sockets_for_eviction();
                tracing::info!(
                    app_id = %oldest_id,
                    active_sockets,
                    closed,
                    "worker: closing native sockets before isolate eviction"
                );
            }
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
    true
}

fn pinned_count_for_app(cache: &AppCache, app_id: &Uuid) -> usize {
    cache
        .workflow_isolates
        .keys()
        .filter(|key| &key.app_id == app_id)
        .count()
}

fn evict_pinned_lru_for_app(cache: &mut AppCache, app_id: &Uuid) -> bool {
    refresh_socket_activity(cache);

    let Some(oldest_key) = cache
        .workflow_isolates
        .iter()
        .filter(|(key, entry)| key.app_id == *app_id && !entry.runtime.is_isolate_leased())
        .min_by_key(|(_, entry)| {
            (
                entry.runtime.active_native_socket_count() > 0,
                entry.last_used,
            )
        })
        .map(|(key, _)| key.clone())
    else {
        return false;
    };

    tracing::info!(
        app_id = %oldest_key.app_id,
        deploy_hash = %oldest_key.deploy_hash,
        "worker: evicting pinned workflow LRU isolate"
    );
    crate::metrics::inc(&crate::metrics::LRU_EVICTIONS_TOTAL);

    if let Some(entry) = cache.workflow_isolates.get(&oldest_key) {
        let active_sockets = entry.runtime.active_native_socket_count();
        if active_sockets > 0 {
            let closed = entry.runtime.close_native_sockets_for_eviction();
            tracing::info!(
                app_id = %oldest_key.app_id,
                deploy_hash = %oldest_key.deploy_hash,
                active_sockets,
                closed,
                "worker: closing native sockets before pinned workflow isolate eviction"
            );
        }
        entry.runtime.with_scope(|scope| {
            zeroship_runtime::rpc::abort::entered_for_eviction(scope, oldest_key.app_id);
        });
    }

    cache.workflow_isolates.remove(&oldest_key);
    true
}

fn refresh_socket_activity(cache: &mut AppCache) {
    for entry in cache.isolates.values_mut() {
        if let Some(activity) = entry.runtime.last_native_socket_activity() {
            if activity > entry.last_used {
                entry.last_used = activity;
            }
        }
    }
    for entry in cache.workflow_isolates.values_mut() {
        if let Some(activity) = entry.runtime.last_native_socket_activity() {
            if activity > entry.last_used {
                entry.last_used = activity;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;

    fn test_runtime() -> Runtime {
        let runtime = Runtime::builder().build();
        runtime.exit_isolate();
        runtime
    }

    fn test_net_runtime() -> Runtime {
        let runtime = Runtime::builder()
            .net_policy(zeroship_runtime::NetPolicy::trusted(8, 1024 * 1024))
            .build();
        runtime.exit_isolate();
        runtime
    }

    fn entry(app_id: Uuid, runtime: Runtime, last_used: Instant) -> IsolateEntry {
        IsolateEntry {
            runtime,
            last_used,
            app_id: app_id.to_string(),
        }
    }

    async fn fetch_body(runtime: &Runtime) -> (u16, String) {
        let env = EnvSnapshot::empty();
        let ctx = zeroship_runtime::RequestCtx::new(zeroship_runtime::CancelFlag::new());
        runtime.enter_isolate();
        let outcome =
            runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
        runtime.exit_isolate();

        match outcome {
            zeroship_runtime::FetchOutcome::Response { status, body, .. } => {
                (status, String::from_utf8_lossy(&body).into_owned())
            }
            zeroship_runtime::FetchOutcome::Pending { rx, .. } => {
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("fetch response timed out")
                    .expect("pending fetch delivered DispatchError");
                match settled {
                    zeroship_runtime::SettledFetch::Response { status, body, .. } => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
                    _ => panic!("expected settled response"),
                }
            }
            _ => panic!("expected buffered fetch response"),
        }
    }

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
                4,
                KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: "test-control-key".to_string(),
                    db_url: Some("postgres://localhost/zs_unused".to_string()),
                    cdc_worker_id: "kernel-test-worker".to_string(),
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
            // creator surface is exactly these five namespaces.
            for expected in ["db", "kv", "storage", "workflows", "auth"] {
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
                4,
                KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    db_url: None,
                    cdc_worker_id: "kernel-test-worker".to_string(),
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
            // auth/workflows are unconditional; kv/storage/db must NOT appear;
            // and there is NO `meter` namespace (metering is infrastructure).
            assert!(has("auth"));
            assert!(has("workflows"));
            assert!(!has("meter"), "metering is infrastructure: no env.meter namespace");
            assert!(!has("kv"), "kv absent when unconfigured");
            assert!(!has("storage"), "storage absent when unconfigured");
            assert!(!has("db"), "db absent when unconfigured");
        })
        .join()
        .expect("degrade guard thread panicked");
    }

    #[test]
    fn net_policy_from_app_defaults_to_denied_when_no_grants() {
        let app_id = Uuid::new_v4();
        let policy = net_policy_from_app(&app_id, &AppNetPolicy::default());
        assert!(matches!(policy, NetPolicy::Denied));
    }

    #[test]
    fn net_policy_from_app_builds_a_rule_set_from_projection_rows() {
        let app_id = Uuid::new_v4();
        let policy = net_policy_from_app(
            &app_id,
            &AppNetPolicy {
                egress: vec![zeroship_core::types::NetEgressEntry {
                    verdict: Verdict::Accept,
                    destination: "DB.Example.COM.".to_string(),
                    port: 5432,
                }],
                max_sockets: 8,
                egress_ceiling_bytes: 2 * 1024 * 1024,
                ..AppNetPolicy::default()
            },
        );
        match &policy {
            NetPolicy::Rules {
                rules,
                max_sockets,
                egress_ceiling_bytes,
            } => {
                assert_eq!(*max_sockets, 8);
                assert_eq!(*egress_ceiling_bytes, 2 * 1024 * 1024);
                // The row is normalised on the way in, so the trailing dot and
                // the case are gone and one destination is one rule.
                assert_eq!(
                    rules.name_phase("db.example.com", 5432),
                    zeroship_core::net_policy::NamePhase::Resolve { name_accepted: true }
                );
                assert_eq!(
                    rules.name_phase("other.example.com", 5432),
                    zeroship_core::net_policy::NamePhase::NoRuleCouldAdmit
                );
            }
            other => panic!("expected Rules, got {other:?}"),
        }
    }

    #[test]
    fn net_policy_from_app_skips_bad_entries_and_keeps_good_ones() {
        let app_id = Uuid::new_v4();
        let policy = net_policy_from_app(
            &app_id,
            &AppNetPolicy {
                egress: vec![
                    zeroship_core::types::NetEgressEntry {
                        verdict: Verdict::Accept,
                        destination: "*".to_string(),
                        port: 443,
                    },
                    zeroship_core::types::NetEgressEntry {
                        verdict: Verdict::Accept,
                        destination: "smtp.example.com".to_string(),
                        port: 587,
                    },
                ],
                max_sockets: 4,
                egress_ceiling_bytes: 1024 * 1024,
                ..AppNetPolicy::default()
            },
        );
        let NetPolicy::Rules { rules, .. } = &policy else {
            panic!("expected Rules, got {policy:?}");
        };
        assert_eq!(
            rules.name_phase("smtp.example.com", 587),
            zeroship_core::net_policy::NamePhase::Resolve { name_accepted: true },
            "one malformed ACCEPT row must not brick the other valid rules"
        );
        assert_eq!(
            rules.name_phase("anything.example.com", 443),
            zeroship_core::net_policy::NamePhase::NoRuleCouldAdmit
        );
    }

    /// A hand-edited row carrying an unparseable REJECT must DENY, not be
    /// skipped: skipping an ACCEPT narrows the app's reach, skipping a REJECT
    /// widens it, and only one of those directions is safe to fail into.
    #[test]
    fn net_policy_from_app_denies_when_a_reject_row_is_unparseable() {
        let app_id = Uuid::new_v4();
        let policy = net_policy_from_app(
            &app_id,
            &AppNetPolicy {
                egress: vec![
                    zeroship_core::types::NetEgressEntry {
                        verdict: Verdict::Accept,
                        destination: "smtp.example.com".to_string(),
                        port: 587,
                    },
                    zeroship_core::types::NetEgressEntry {
                        verdict: Verdict::Reject,
                        destination: "not a destination".to_string(),
                        port: 587,
                    },
                ],
                max_sockets: 4,
                egress_ceiling_bytes: 1024 * 1024,
            },
        );
        assert!(
            matches!(policy, NetPolicy::Denied),
            "an unparseable REJECT must deny the app, never be skipped"
        );
    }

    /// Wildcards are no longer representable, so a row carrying one is dropped
    /// at load exactly as the API would have refused it at authoring time.
    #[test]
    fn net_policy_from_app_drops_wildcard_rows() {
        let app_id = Uuid::new_v4();
        let policy = net_policy_from_app(
            &app_id,
            &AppNetPolicy {
                egress: vec![zeroship_core::types::NetEgressEntry {
                    verdict: Verdict::Accept,
                    destination: "*.shared.example.test".to_string(),
                    port: 443,
                }],
                max_sockets: 4,
                egress_ceiling_bytes: 1024 * 1024,
            },
        );
        assert!(
            matches!(policy, NetPolicy::Denied),
            "a wildcard row must be skipped; with no rules left the app is denied"
        );
    }

    #[test]
    fn net_policy_from_app_cannot_produce_trusted() {
        let app_id = Uuid::new_v4();
        let policy = net_policy_from_app(
            &app_id,
            &AppNetPolicy {
                egress: vec![zeroship_core::types::NetEgressEntry {
                    verdict: Verdict::Accept,
                    destination: "db.example.com".to_string(),
                    port: 5432,
                }],
                max_sockets: u32::MAX,
                egress_ceiling_bytes: u64::MAX,
                ..AppNetPolicy::default()
            },
        );
        assert!(
            !matches!(policy, NetPolicy::Trusted { .. }),
            "creator AppNetPolicy has no representation for runtime Trusted"
        );
    }

    #[test]
    fn load_app_applies_net_policy_to_runtime_builder() {
        std::thread::spawn(|| {
            let Ok(runtime) = compio::runtime::Runtime::new() else {
                eprintln!("skipping (cannot create compio runtime)");
                return;
            };

            runtime.block_on(async {
                let app_id = Uuid::new_v4();
                init_cache(
                    4,
                    4,
                    KernelConfig {
                        control_url: "http://127.0.0.1:1".to_string(),
                        control_key: String::new(),
                        db_url: None,
                        cdc_worker_id: "kernel-test-worker".to_string(),
                        kv_url: None,
                        storage_backend: None,
                        meter: Arc::new(zeroship_metering::Meter::new()),
                    },
                );
                load_app(
                    app_id,
                    br#"export default { fetch() { return new Response("ok"); } }"#,
                    AppRuntimeLimits::default(),
                    AppNetPolicy {
                        egress: vec![zeroship_core::types::NetEgressEntry {
                            verdict: Verdict::Accept,
                            destination: "db.example.com".to_string(),
                            port: 5432,
                        }],
                        max_sockets: 6,
                        egress_ceiling_bytes: 1024 * 1024,
                        ..AppNetPolicy::default()
                    },
                    None,
                    None,
                    &EnvSnapshot::empty(),
                )
                .expect("app loads");
                let runtime = get_runtime(&app_id).expect("runtime loaded");
                let state = runtime.state();
                let state = state.borrow();
                let NetPolicy::Rules { rules, .. } = &state.net_policy else {
                    panic!("expected Rules, got {:?}", state.net_policy);
                };
                assert_eq!(
                    rules.name_phase("db.example.com", 5432),
                    zeroship_core::net_policy::NamePhase::Resolve { name_accepted: true }
                );
                // The port is part of the rule, so the same host on another
                // port is refused before DNS.
                assert_eq!(
                    rules.name_phase("db.example.com", 5433),
                    zeroship_core::net_policy::NamePhase::NoRuleCouldAdmit
                );
                assert_eq!(state.net_policy.max_sockets(), 6);
            });
        })
        .join()
        .expect("load_app net policy test thread panicked");
    }

    #[test]
    fn load_app_preserves_last_good_isolate_when_descriptor_validation_fails() {
        std::thread::spawn(|| {
            let Ok(runtime) = compio::runtime::Runtime::new() else {
                eprintln!("skipping (cannot create compio runtime)");
                return;
            };

            runtime.block_on(async {
                zeroship_runtime::init::init_v8();
                let app_id = Uuid::new_v4();
                init_cache(
                    4,
                    4,
                    KernelConfig {
                        control_url: "http://127.0.0.1:1".to_string(),
                        control_key: String::new(),
                        db_url: None,
                        cdc_worker_id: "kernel-test-worker".to_string(),
                        kv_url: None,
                        storage_backend: None,
                        meter: Arc::new(zeroship_metering::Meter::new()),
                    },
                );

                load_app(
                    app_id,
                    br#"export default { fetch() { return new Response("last-good"); } }"#,
                    AppRuntimeLimits::default(),
                    AppNetPolicy::default(),
                    Some("deploy-good"),
                    None,
                    &EnvSnapshot::empty(),
                )
                .expect("initial app loads");

                let before = get_runtime(&app_id).expect("initial runtime cached");
                assert_eq!(fetch_body(&before).await, (200, "last-good".to_string()));

                let err = load_app(
                    app_id,
                    br#"export default { fetch() { return new Response("bad-new"); } }"#,
                    AppRuntimeLimits::default(),
                    AppNetPolicy::default(),
                    Some("deploy-bad"),
                    Some(
                        r#"{"version":1,"collections":{"notes":{"fields":{"title":{"type":"string"}},"options":{"softDelete":false,"versioning":false},"indexes":[{"name":"bad","fields":[123]}]}}}"#,
                    ),
                    &EnvSnapshot::empty(),
                )
                .expect_err("corrupt descriptor must fail the reload");
                assert!(
                    err.contains("manifest.runtime_descriptor") && err.contains("indexes"),
                    "error should surface descriptor validation, got: {err}"
                );

                let after = get_runtime(&app_id).expect("last-good runtime must remain cached");
                assert_eq!(
                    fetch_body(&after).await,
                    (200, "last-good".to_string()),
                    "failed reload must keep serving the previous isolate"
                );
            });
        })
        .join()
        .expect("last-good preserve test thread panicked");
    }

    #[test]
    fn first_load_with_corrupt_descriptor_hard_errors_without_cached_isolate() {
        std::thread::spawn(|| {
            let Ok(runtime) = compio::runtime::Runtime::new() else {
                eprintln!("skipping (cannot create compio runtime)");
                return;
            };

            runtime.block_on(async {
                zeroship_runtime::init::init_v8();
                let app_id = Uuid::new_v4();
                init_cache(
                    4,
                    4,
                    KernelConfig {
                        control_url: "http://127.0.0.1:1".to_string(),
                        control_key: String::new(),
                        db_url: None,
                        cdc_worker_id: "kernel-test-worker".to_string(),
                        kv_url: None,
                        storage_backend: None,
                        meter: Arc::new(zeroship_metering::Meter::new()),
                    },
                );

                let err = load_app(
                    app_id,
                    br#"export default { fetch() { return new Response("bad-first"); } }"#,
                    AppRuntimeLimits::default(),
                    AppNetPolicy::default(),
                    Some("deploy-bad"),
                    Some(
                        r#"{"version":1,"collections":{"notes":{"fields":{"title":{"type":"string"}},"options":{"softDelete":false,"versioning":false},"indexes":[{"name":"bad","fields":[123]}]}}}"#,
                    ),
                    &EnvSnapshot::empty(),
                )
                .expect_err("first corrupt descriptor load must hard-error");
                assert!(
                    err.contains("manifest.runtime_descriptor") && err.contains("indexes"),
                    "error should surface descriptor validation, got: {err}"
                );
                assert!(
                    get_runtime(&app_id).is_none(),
                    "first failed load has no previous isolate to preserve"
                );
            });
        })
        .join()
        .expect("first-load corrupt descriptor test thread panicked");
    }

    #[test]
    fn reading_limits_does_not_refresh_recency() {
        std::thread::spawn(|| {
            let Ok(runtime) = compio::runtime::Runtime::new() else {
                eprintln!("skipping reading_limits_does_not_refresh_recency (no compio runtime)");
                return;
            };
            runtime.block_on(async {
                let app_id = Uuid::new_v4();
                init_cache(
                    4,
                    4,
                    KernelConfig {
                        control_url: "http://127.0.0.1:1".to_string(),
                        control_key: String::new(),
                        db_url: None,
                        cdc_worker_id: "kernel-test-worker".to_string(),
                        kv_url: None,
                        storage_backend: None,
                        meter: Arc::new(zeroship_metering::Meter::new()),
                    },
                );
                load_app(
                    app_id,
                    br#"export default { fetch() { return new Response("ok"); } }"#,
                    AppRuntimeLimits::default(),
                    AppNetPolicy::default(),
                    None,
                    None,
                    &EnvSnapshot::empty(),
                )
                .expect("app loads");

                // Backdate the entry so a stamp would be unmistakable.
                let stamped = Instant::now() - Duration::from_secs(600);
                CACHE.with(|c| {
                    let mut cache = c.borrow_mut();
                    cache.as_mut().unwrap().isolates.get_mut(&app_id).unwrap().last_used = stamped;
                });

                assert!(get_limits(&app_id).is_some(), "the limits read must find the app");

                let after = CACHE.with(|c| {
                    let cache = c.borrow();
                    cache.as_ref().unwrap().isolates.get(&app_id).unwrap().last_used
                });
                assert_eq!(
                    after, stamped,
                    "reading limits is metadata, not traffic. Reconciliation reads every \
                     cached app each cycle, so stamping recency here makes the whole cache \
                     look freshly used and eviction stops tracking real use"
                );
            });
        })
        .join()
        .unwrap();
    }

    #[test]
    fn evict_lru_prefers_idle_socketless_isolate_over_active_socketed_isolate() {
        std::thread::spawn(|| {
            let now = Instant::now();
            let socketed_id = Uuid::new_v4();
            let socketless_id = Uuid::new_v4();
            let socketed = test_runtime();
            {
                let state = socketed.state();
                let mut s = state.borrow_mut();
                s.active_native_sockets = 1;
                s.native_socket_last_activity = Some(now);
            }
            let socketless = test_runtime();
            let mut cache = AppCache {
                isolates: HashMap::new(),
                workflow_isolates: HashMap::new(),
                max_size: 2,
                max_pinned_isolates_per_app: 4,
            };
            cache.isolates.insert(
                socketed_id,
                entry(socketed_id, socketed, now - Duration::from_secs(600)),
            );
            cache.isolates.insert(
                socketless_id,
                entry(socketless_id, socketless, now - Duration::from_secs(1)),
            );

            assert!(evict_lru(&mut cache));
            assert!(
                cache.isolates.contains_key(&socketed_id),
                "socketed, recently-active isolate must remain cached"
            );
            assert!(
                !cache.isolates.contains_key(&socketless_id),
                "socketless isolate should be the preferred LRU victim"
            );
            assert_eq!(
                cache.isolates[&socketed_id].last_used, now,
                "socket activity should refresh the worker LRU timestamp"
            );
        })
        .join()
        .expect("socket-aware eviction test thread panicked");
    }

    #[test]
    fn evict_lru_never_evicts_leased_isolate() {
        std::thread::spawn(|| {
            let now = Instant::now();
            let leased_id = Uuid::new_v4();
            let victim_id = Uuid::new_v4();
            let leased = test_runtime();
            let lease = leased.lease_isolate();
            assert_eq!(leased.isolate_lease_count(), 1);
            let victim = test_runtime();
            let mut cache = AppCache {
                isolates: HashMap::new(),
                workflow_isolates: HashMap::new(),
                max_size: 2,
                max_pinned_isolates_per_app: 4,
            };
            cache.isolates.insert(
                leased_id,
                entry(leased_id, leased.clone(), now - Duration::from_secs(600)),
            );
            cache.isolates.insert(victim_id, entry(victim_id, victim, now));

            assert!(evict_lru(&mut cache));
            assert!(
                cache.isolates.contains_key(&leased_id),
                "leased isolate must be un-evictable while the lease is held"
            );
            assert!(
                !cache.isolates.contains_key(&victim_id),
                "non-leased isolate should be evicted instead"
            );
            drop(lease);
            assert_eq!(leased.isolate_lease_count(), 0);
        })
        .join()
        .expect("lease eviction test thread panicked");
    }

    #[test]
    fn evict_lru_closes_socketed_isolate_before_removal() {
        std::thread::spawn(|| {
            let now = Instant::now();
            let app_id = Uuid::new_v4();
            let runtime = test_net_runtime();
            let state = runtime.state();
            let socket_id = zeroship_runtime::node::net::state::alloc_native_socket_id(&state)
                .expect("alloc test socket id");
            zeroship_runtime::node::net::state::reserve_socket_slot(&state, socket_id)
                .expect("reserve test socket slot");
            assert_eq!(runtime.active_native_socket_count(), 1);

            let mut cache = AppCache {
                isolates: HashMap::new(),
                workflow_isolates: HashMap::new(),
                max_size: 1,
                max_pinned_isolates_per_app: 4,
            };
            cache
                .isolates
                .insert(app_id, entry(app_id, runtime.clone(), now - Duration::from_secs(60)));

            assert!(evict_lru(&mut cache));
            assert!(!cache.isolates.contains_key(&app_id));
            assert_eq!(
                runtime.active_native_socket_count(),
                0,
                "socket slot should be released before isolate removal"
            );
            let socket =
                zeroship_runtime::node::net::state::lookup_native_socket_state(&state, socket_id)
                    .expect("socket state remains inspectable through runtime clone");
            let socket = socket.borrow();
            assert!(socket.destroyed, "eviction must destroy open sockets");
            assert!(
                socket.close_emitted,
                "eviction should enqueue the close path before dropping the runtime"
            );
        })
        .join()
        .expect("socket close eviction test thread panicked");
    }
}
