use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use zeroship_bundle::compiled::CompiledManifest;
use zeroship_bundle::Manifest;
use zeroship_core::app_id::AppId;
use zeroship_core::net_policy::{EgressRule, Verdict};
use zeroship_core::types::{AppNetPolicy, AppRuntimeLimits};
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::{Runtime, RuntimeLimits};
use zeroship_runtime::{EnvSnapshot, ModuleEntry, NetPolicy};
use zeroship_storage::StorageBackendConfig;
use zeroship_workflow::service::runner::ready::ReadyApps;

#[cfg(test)]
pub(crate) mod fixture;

struct IsolateEntry {
    runtime: Runtime,
    last_used: std::time::Instant,
    app_id: AppId,
    /// The deploy's DECLARED route policy, compiled once at load.
    ///
    /// It lives on the isolate entry rather than in a map of its own so its
    /// lifetime is the isolate's by construction: an eviction or a reload
    /// cannot leave a stale policy behind for the next deploy to be judged
    /// against, because there is no separate map to forget to clean.
    ///
    /// `Rc` because `handler::dispatch` reads it out of the thread-local
    /// borrow and then holds it across the isolate entry; cloning the compiled
    /// tables per request would put the whole resource map on the hot path.
    policy: Rc<CompiledManifest>,
}

struct AppCache {
    isolates: HashMap<AppId, IsolateEntry>,
    max_size: usize,
}

thread_local! {
    static CACHE: RefCell<Option<AppCache>> = const { RefCell::new(None) };
    /// This thread's handle to the PROCESS-WIDE `env.db` service.
    ///
    /// Not a URL and not a plugin: the service is constructed ONCE in `main`,
    /// before ntex spawns any worker thread and therefore before any isolate
    /// exists, and each thread is handed the same `Arc` through
    /// [`KernelConfig`]. `create_plugins` clones the service's plugin prototype
    /// rather than minting one, so every runtime in the process shares one
    /// plugin object, one validated configuration and one live-metadata cache.
    static DB_SERVICE: RefCell<Option<Arc<zeroship_data_v8::service::DbService>>> =
        const { RefCell::new(None) };
    /// The process-owned store is configured before worker threads start.
    /// Isolates receive scoped handles from it through the V8 binding.
    static KV_STORE: RefCell<Option<zeroship_kv::KvStore>> = const { RefCell::new(None) };
    /// Parsed object-storage backend config for the `env.storage` namespace.
    /// Held per-thread like `DB_URL`. When `Some`, `create_plugins` mints a
    /// `StorageBinding` over the selected backend: `LocalFs` (a SHARED volume
    /// across nodes — the same multi-node pattern the deploy blob store uses)
    /// or `S3` (S3/R2/MinIO — inherently shared). When `None`, the `storage`
    /// namespace is absent.
    static STORAGE_BACKEND: RefCell<Option<StorageBackendConfig>> = const { RefCell::new(None) };
    /// The app backends the process's workflow host has made ready. Every
    /// isolate's `env.workflows` resolves its own app here on each call, so an
    /// unknown or unready app is refused and nothing reaches Control.
    static WORKFLOWS: RefCell<Option<ReadyApps>> = const { RefCell::new(None) };
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
/// An absent `db_service`, `kv_store`, or `storage_backend` means that
/// the corresponding `env.*` namespace is not
/// registered (matching the long-standing DB behaviour), never a panic.
/// The real multi-node stack SHOULD set all of them so deployed apps get
/// the complete `env.{db,kv,storage,auth}` kernel.
pub struct KernelConfig {
    /// Ready app backends published by the process's workflow host. With no
    /// host the registry stays empty and every `env.workflows` call is refused.
    pub workflows: ReadyApps,
    /// The ONE process-wide `env.db` service, built in `main` before any
    /// worker thread exists. `None` when no database is configured, in which
    /// case the `db` namespace is simply absent.
    pub db_service: Option<Arc<zeroship_data_v8::service::DbService>>,
    pub kv_store: Option<zeroship_kv::KvStore>,
    pub storage_backend: Option<StorageBackendConfig>,
    /// The process-wide usage meter shared with the per-process outbox task
    /// (see `main`). Always set in the real worker; an `Arc<Meter>` is
    /// cheap so there is no "absent" tier — the namespace is registered
    /// unconditionally when present.
    pub meter: Arc<zeroship_metering::Meter>,
}

impl std::fmt::Debug for KernelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelConfig")
            .field("db_configured", &self.db_service.is_some())
            .field("kv_configured", &self.kv_store.is_some())
            .field("storage_configured", &self.storage_backend.is_some())
            .finish_non_exhaustive()
    }
}

/// Install this thread's isolate cache and runtime kernel.
///
/// Replacing the kernel also replaces its DB and KV stores. Passing no KV
/// store removes the previous binding and invalidates cached plugin prototypes.
/// Storage retains its existing installation behavior.
pub fn init_cache(max_size: usize, kernel: KernelConfig) {
    CACHE.with(|c| {
        *c.borrow_mut() = Some(AppCache {
            isolates: HashMap::new(),
            max_size,
        });
    });
    DB_SERVICE.with(|s| *s.borrow_mut() = kernel.db_service);
    KV_STORE.with(|store| *store.borrow_mut() = kernel.kv_store);
    if let Some(backend) = kernel.storage_backend {
        STORAGE_BACKEND.with(|s| *s.borrow_mut() = Some(backend));
    }
    WORKFLOWS.with(|w| *w.borrow_mut() = Some(kernel.workflows));
    METER.with(|m| *m.borrow_mut() = Some(kernel.meter));
    // Every input `plugin_set` builds from was just replaced, so any cached
    // prototype set is now stale. Clearing here rather than trusting
    // "init_cache runs once" keeps the cache correct under repeated
    // installation instead of correct-by-convention.
    PLUGIN_SET.with(|p| *p.borrow_mut() = None);
}

/// Project material held by the installed database service for this host.
pub fn project_keys() -> Option<Arc<zeroship_data_orm::encryption::SuppliedProjectKeys>> {
    DB_SERVICE.with(|service| {
        service
            .borrow()
            .as_ref()
            .map(|service| service.project_keys().clone())
    })
}

/// The app-to-database bindings this host has resolved.
///
/// A process-wide store, not per-thread state: every worker thread that builds
/// an isolate for one app must narrow to the same role, and a per-thread copy
/// would let two threads disagree about which epoch is live.
pub fn app_bindings() -> Option<Arc<zeroship_data_orm::resolved_bindings::SuppliedAppBindings>> {
    DB_SERVICE.with(|service| {
        service
            .borrow()
            .as_ref()
            .map(|service| service.app_bindings().clone())
    })
}

thread_local! {
    /// The thread's plugin prototype set, minted once and cloned thereafter.
    ///
    /// SC-5 of the runtime-db-binding design requires `build_runtime` to
    /// perform no backend selection and clone an `Arc` rather than minting a
    /// plugin set, so that the isolates on one OS thread share one backend and
    /// one cache instead of each resolving their own. This is the first,
    /// behaviour-neutral half of that: the plugins are the same values,
    /// constructed once.
    ///
    /// Cleared by [`init_cache`], which is the single writer of every
    /// thread-local this set is built from. Without that invalidation a second
    /// `init_cache` on the same thread - which tests do - would keep serving
    /// plugins built from the previous configuration, and the staleness would
    /// be invisible because the plugins would still work, just against the
    /// wrong backend.
    static PLUGIN_SET: RefCell<Option<Vec<Arc<dyn NativePlugin>>>> =
        const { RefCell::new(None) };
}

/// The thread's plugin set, minting it on first use.
///
/// Returns clones of the same `Arc`s on every call, so two runtimes built on
/// one thread share plugin instances rather than each holding their own.
fn plugin_set() -> Vec<Arc<dyn NativePlugin>> {
    PLUGIN_SET.with(|p| {
        let mut slot = p.borrow_mut();
        if slot.is_none() {
            *slot = Some(create_plugins());
        }
        slot.as_ref().expect("just populated").clone()
    })
}

/// Create plugins for a new Runtime — the kernel every deployed app boots
/// against. This is the SINGLE source of truth for the multi-node `env.*`
/// surface; the CLI `zeroship serve` vector (`crates/zeroship-cli/src/main.rs`)
/// mirrors it for the single-tenant dev path.
///
/// Namespaces and their multi-node backend choices:
/// - `auth` — pushed unconditionally. Stateless (callbacks read the
///   per-request user from `RuntimeState`), so every app gets a working
///   `env.auth.getUser()` / `env.auth.requireUser()`.
/// - `db` — pushed when a DB URL is configured. Postgres is inherently
///   shared across nodes.
/// - `kv` — pushed when startup supplied a configured `KvStore`. The
///   distributed worker selects Redis so nodes share storage; its URL selects
///   standalone or cluster mode. Isolate construction clones the configured
///   store and performs no backend selection.
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
    // CLONE the prototype the process-wide service owns. No backend selection,
    // no URL parse, no `DbPlugin` construction: all three happened once, in
    // `main`, before this thread existed. Two worker threads therefore push the
    // SAME plugin object rather than two equal ones.
    if let Some(service) = DB_SERVICE.with(|s| s.borrow().clone()) {
        plugins.push(service.plugin());
    }
    if let Some(store) = KV_STORE.with(|s| s.borrow().clone()) {
        plugins.push(Arc::new(zeroship_kv_v8::KvBinding::new(
            store,
            meter.clone(),
        )));
    }
    if let Some(cfg) = STORAGE_BACKEND.with(|s| s.borrow().clone()) {
        match zeroship_storage::StorageStore::open(&cfg) {
            Ok(backend) => plugins.push(Arc::new(zeroship_storage_v8::StorageBinding::new(
                backend,
                meter.clone(),
            ))),
            Err(e) => {
                // Credentials were validated at boot (see worker main); a
                // failure here means the env changed under us. Degrade the
                // namespace rather than crash the isolate.
                tracing::error!(error = %e, "env.storage backend init failed; namespace absent");
            }
        }
    }
    if let Some(apps) = WORKFLOWS.with(|w| w.borrow().clone()) {
        plugins.push(Arc::new(zeroship_workflow_v8::WorkflowBinding::ready(apps)));
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
    app_id: &AppId,
    cpu_us: u64,
    wall_us: u64,
    egress_bytes: u64,
    ingress_bytes: u64,
) {
    METER.with(|m| {
        if let Some(meter) = m.borrow().as_ref() {
            let recorded = CACHE.with(|c| {
                let cache = c.borrow();
                let Some(entry_app_id) = cache
                    .as_ref()
                    .and_then(|cache| cache.isolates.get(app_id))
                    .map(|entry| &entry.app_id)
                else {
                    return false;
                };
                meter.record_request(entry_app_id, cpu_us, wall_us, egress_bytes, ingress_bytes);
                true
            });
            if !recorded {
                meter.record_request(app_id, cpu_us, wall_us, egress_bytes, ingress_bytes);
            }
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
pub fn record_stream_delta(app_id: &AppId, egress_bytes_delta: u64, stream_wall_us_delta: u64) {
    if egress_bytes_delta == 0 && stream_wall_us_delta == 0 {
        return;
    }
    METER.with(|m| {
        if let Some(meter) = m.borrow().as_ref() {
            if egress_bytes_delta > 0 {
                meter.increment(app_id, "egress_bytes", egress_bytes_delta);
            }
            if stream_wall_us_delta > 0 {
                meter.increment(app_id, "stream_wall_us", stream_wall_us_delta);
            }
        }
    });
}

/// Get or create a V8 runtime for an app. Returns None if the app isn't loaded.
pub fn get_runtime(app_id: &AppId) -> Option<Runtime> {
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
pub fn get_limits(app_id: &AppId) -> Option<RuntimeLimits> {
    CACHE.with(|c| {
        let cache = c.borrow();
        limits_without_touching_recency(cache.as_ref()?, app_id)
    })
}

/// The read above, over a borrowed cache so the no-touch property is testable.
fn limits_without_touching_recency(cache: &AppCache, app_id: &AppId) -> Option<RuntimeLimits> {
    cache
        .isolates
        .get(app_id)
        .map(|entry| entry.runtime.limits())
}

/// The worker-internal environment handed to an isolate.
///
/// EVERY ENTRY OF THIS MAP IS READABLE BY APP JS. It is not a private channel:
/// `zeroship_runtime::core::init` copies the whole map into `process.env` as
/// the last-resort layer, so anything placed here is reachable through
/// `process.env.NAME` or any npm package that walks `Object.keys(process.env)`.
/// The `env.*` primitive surface deliberately excludes these vars;
/// `process.env` does not, and that asymmetry is the trap.
///
/// The rule, therefore: THIS MAP MAY CARRY ONLY IDENTIFIERS THE APP ALREADY
/// POSSESSES. An app knows its own id and its own deploy, so disclosing them
/// costs nothing. An identifier naming a resource SHARED WITH ANOTHER TENANT -
/// a datastore key, or a database id once databases are shared - must never
/// enter it, because two apps under one actor that read equal values have
/// confirmed co-residency. Both of those ids stay internal.
///
/// Being unforgeable is NOT sufficient to qualify. Creator `vars` cannot shadow
/// a worker-internal entry, which is why metering is trustworthy, but that is a
/// forgery property; the concern here is disclosure, and the map is readable
/// either way.
///
/// `worker_env_is_exactly_the_app_owned_ids` binds the key set. Widening it is
/// a security decision, so it must be an edit to that test and not a silent
/// insert here.
fn app_visible_env_vars(app_id: &str, deploy_hash: Option<&str>) -> HashMap<String, String> {
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id.to_string());
    // Bind descriptors to the actual deployment, including pinned workflow
    // isolates, so different deployments cannot share schema entries.
    if let Some(dh) = deploy_hash {
        env_vars.insert("ZEROSHIP_DEPLOY_ID".to_string(), dh.to_string());
    }
    env_vars
}

#[allow(clippy::too_many_arguments)]
async fn build_runtime(
    app_id: &AppId,
    modules: Vec<ModuleEntry>,
    app_limits: AppRuntimeLimits,
    app_net_policy: AppNetPolicy,
    deploy_hash: Option<&str>,
    runtime_descriptor: Option<&str>,
    env: &EnvSnapshot,
) -> Result<Runtime, String> {
    let plugins = plugin_set();
    let meter = METER.with(|m| m.borrow().clone());
    let env_vars = app_visible_env_vars(app_id.as_str(), deploy_hash);

    let limits = runtime_limits_from_app(&app_limits);
    let net_policy = net_policy_from_app(app_id, &app_net_policy);
    let mut builder = Runtime::builder()
        .modules(modules)
        .env_vars(env_vars)
        .limits(limits)
        .plugins(plugins)
        .net_policy(net_policy)
        .runtime_descriptor(runtime_descriptor.map(str::to_string))
        // The builder is what binds BOTH app-scoped behaviours, and skipping it
        // silently loses each in a different way. It stamps `state.meter` -
        // read by `RuntimeInner::bill_pump_cpu` for async/pump CPU and by
        // `node:net` for socket egress/ingress - keyed by `app_id.as_str()`,
        // the same rendering `env.{db,kv,storage}` read off `APP_ID` above. It
        // also sets `RuntimeInner::app_id`, which gates the eviction-time
        // `AbortController` fan-out: without it an evicted isolate's in-flight
        // controllers never fire, and nothing errors.
        .app_id(app_id.clone());
    if let Some(meter) = meter {
        builder = builder.meter(meter);
    }
    let runtime = builder.build();
    // Startup can await host operations; keep the isolate exited between turns.
    runtime.exit_isolate();

    runtime
        .initialize(env)
        .await
        .map_err(|e| format!("failed to initialize app runtime: {e}"))?;

    Ok(runtime)
}

/// Load an app from its verified module graph, with the entry module first.
/// Creates the V8 runtime and starts its pump after initialization succeeds.
///
/// `manifest` is the deploy's own manifest, and it is what makes the worker a
/// real enforcer of the declared route policy rather than a tier that trusts
/// the gateway to have gated already: it is compiled here, once per load, and
/// consulted per dispatch by `crate::policy::enforce`. Callers that hold no
/// manifest pass `&Manifest::default()`, whose empty resource tree declares
/// nothing and therefore refuses nothing.
#[allow(clippy::too_many_arguments)]
pub async fn load_app(
    app_id: AppId,
    modules: Vec<ModuleEntry>,
    app_limits: AppRuntimeLimits,
    app_net_policy: AppNetPolicy,
    deploy_hash: Option<&str>,
    runtime_descriptor: Option<&str>,
    manifest: &Manifest,
    env: &EnvSnapshot,
) -> Result<(), String> {
    let runtime = build_runtime(
        &app_id,
        modules,
        app_limits,
        app_net_policy,
        deploy_hash,
        runtime_descriptor,
        env,
    ).await?;
    let policy = Rc::new(CompiledManifest::compile(manifest));

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
                app_id = app_id.as_str(),
                max_size = cache.max_size,
                "worker: isolate cache full and every isolate is leased; load deferred"
            );
            return Err("isolate cache full and every isolate is leased; load deferred".into());
        }

        // Start pump task for async V8 ops (timers, fetch, streams) only after
        // capacity is available and the runtime is about to become reachable.
        runtime.start_pump();
        cache.isolates.insert(
            app_id.clone(),
            IsolateEntry {
                runtime,
                last_used: std::time::Instant::now(),
                app_id,
                policy,
            },
        );

        Ok(())
    })
}

/// The declared route policy of the deploy this thread has resident, WITHOUT
/// marking the app recently used.
///
/// Not a dispatch in itself - the caller may refuse the request on what this
/// returns, and a refused request must not count as traffic for eviction
/// purposes any more than the reconcile loop's metadata reads do
/// (`get_limits` above carries the same property, for the same reason).
pub fn get_declared_policy(app_id: &AppId) -> Option<Rc<CompiledManifest>> {
    CACHE.with(|c| {
        let cache = c.borrow();
        Some(cache.as_ref()?.isolates.get(app_id)?.policy.clone())
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
pub fn net_policy_from_app(app_id: &AppId, app_net: &AppNetPolicy) -> NetPolicy {
    if app_net.egress.is_empty() {
        return NetPolicy::Denied;
    }

    let mut rules = Vec::with_capacity(app_net.egress.len());
    for entry in &app_net.egress {
        match EgressRule::parse(entry.verdict, &entry.destination, entry.port) {
            Ok(rule) => rules.push(rule),
            Err(err) => {
                tracing::error!(
                    app_id = app_id.as_str(),
                    verdict = entry.verdict.as_str(),
                    destination = %entry.destination,
                    port = entry.port,
                    error = %err,
                    "worker: egress rule rejected at load"
                );
                if entry.verdict == Verdict::Reject {
                    tracing::error!(
                        app_id = app_id.as_str(),
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
                app_id = app_id.as_str(),
                error = %err,
                "worker: egress rule set rejected at load; denying raw TCP"
            );
            NetPolicy::Denied
        }
    }
}

/// Remove an app from the cache.
#[allow(dead_code)]
pub fn evict_app(app_id: &AppId) {
    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if let Some(cache) = cache.as_mut() {
            cache.isolates.remove(app_id);
        }
    });
}

/// Check if an app is loaded.
#[allow(dead_code)]
pub fn has_app(app_id: &AppId) -> bool {
    CACHE.with(|c| {
        let cache = c.borrow();
        cache
            .as_ref()
            .is_some_and(|c| c.isolates.contains_key(app_id))
    })
}

/// Get all app IDs currently loaded in the cache.
pub fn all_app_ids() -> Vec<AppId> {
    CACHE.with(|c| {
        let cache = c.borrow();
        cache
            .as_ref()
            .map(|c| c.isolates.keys().cloned().collect())
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
    static LOADED_META: RefCell<HashMap<AppId, LoadedMeta>> = RefCell::new(HashMap::new());
}

pub fn get_loaded_meta(app_id: &AppId) -> Option<LoadedMeta> {
    LOADED_META.with(|m| m.borrow().get(app_id).cloned())
}

pub fn set_loaded_meta(app_id: AppId, meta: LoadedMeta) {
    LOADED_META.with(|m| {
        m.borrow_mut().insert(app_id, meta);
    });
}

pub fn remove_loaded_meta(app_id: &AppId) {
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
        .map(|(id, _)| id.clone())
    else {
        return false;
    };

    {
        tracing::info!(app_id = oldest_id.as_str(), "worker: evicting LRU isolate");
        crate::metrics::inc(&crate::metrics::LRU_EVICTIONS_TOTAL);

        // Fire every in-flight `AbortController` before removing the isolate.
        if let Some(entry) = cache.isolates.get(&oldest_id) {
            let active_sockets = entry.runtime.active_native_socket_count();
            if active_sockets > 0 {
                let closed = entry.runtime.close_native_sockets_for_eviction();
                tracing::info!(
                    app_id = oldest_id.as_str(),
                    active_sockets,
                    closed,
                    "worker: closing native sockets before isolate eviction"
                );
            }
            entry.runtime.with_scope(|scope| {
                zeroship_runtime::rpc::entered_for_eviction(scope, &oldest_id);
            });
        }

        cache.isolates.remove(&oldest_id);
        LOADED_META.with(|m| {
            m.borrow_mut().remove(&oldest_id);
        });
        // Env in `SharedEnvs` is process-wide and may still be needed
        // by other threads — DON'T evict it here. The version_poll_loop
        // GCs SharedEnvs against the known-app set every cycle, so an
        // app deleted from control plane gets cleaned up there.
    }
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
}

#[cfg(test)]
pub(crate) fn test_modules(source: &[u8]) -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: std::str::from_utf8(source).unwrap().into(),
    }]
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;

    /// Every entry of the worker-internal env map lands in `process.env` and is
    /// therefore readable by app JS, so the key set is a security surface
    /// rather than an implementation detail. Both ids below are the app's own.
    ///
    /// This asserts the EXACT set, not a subset, because the failure it guards
    /// is an ADDITION: a datastore key or a shared database id inserted beside
    /// `APP_ID` would be published to every tenant that holds it, turning the
    /// map into a co-tenancy oracle. A `contains_key` assertion would pass
    /// straight through that. If you are here because this test failed, the
    /// question to answer is not "how do I make it pass" but "does the app
    /// already possess the value I just added"; if it does not, it belongs in
    /// `DbServiceConfig` or another channel that terminates before V8.
    #[test]
    fn worker_env_is_exactly_the_app_owned_ids() {
        let with_deploy = app_visible_env_vars("app_abc", Some("deploy_xyz"));
        let mut keys: Vec<&str> = with_deploy.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["APP_ID", "ZEROSHIP_DEPLOY_ID"],
            "the worker env map is copied into process.env; it may carry only \
             ids the app already possesses"
        );
        assert_eq!(
            with_deploy.get("APP_ID").map(String::as_str),
            Some("app_abc")
        );
        assert_eq!(
            with_deploy.get("ZEROSHIP_DEPLOY_ID").map(String::as_str),
            Some("deploy_xyz")
        );

        // A raw-JS deploy or local dev injects no deploy hash. That arm must
        // narrow the set, never widen it.
        let without_deploy = app_visible_env_vars("app_abc", None);
        let keys: Vec<&str> = without_deploy.keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            ["APP_ID"],
            "with no deploy hash the map carries the app id alone"
        );
    }

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

    fn entry(app_id: AppId, runtime: Runtime, last_used: Instant) -> IsolateEntry {
        IsolateEntry {
            runtime,
            last_used,
            app_id,
            // These fixtures exercise eviction and recency, never policy. An
            // empty resource tree declares nothing, so it cannot make an
            // eviction arm pass or fail for an auth reason.
            policy: Rc::new(CompiledManifest::compile(&Manifest::default())),
        }
    }

    async fn fetch_body(runtime: &Runtime) -> (u16, String) {
        let env = EnvSnapshot::empty();
        let ctx = zeroship_runtime::RequestCtx::new(zeroship_runtime::CancelFlag::new());
        runtime.enter_isolate();
        let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
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
    /// edit that drops `AuthPlugin` from `create_plugins` fails loudly,
    /// not just under `zeroship serve` (the CLI vector). `AuthPlugin` is
    /// stateless, so it is pushed even when no DB URL is configured.
    #[test]
    fn create_plugins_registers_auth_namespace() {
        let plugins = create_plugins();
        assert!(
            plugins.iter().any(|p| p.namespace() == "auth"),
            "worker create_plugins must include the auth namespace; got: {:?}",
            plugins.iter().map(|p| p.namespace()).collect::<Vec<_>>()
        );
    }

    /// Runtime construction must not open a database or select a backend.
    #[test]
    fn building_the_plugin_set_selects_no_backend_and_opens_no_pool() {
        use zeroship_data_orm::connection::{
            backend_open_count, configuration_parse_count as url_parse_count,
        };

        std::thread::spawn(|| {
            // Composition happens first and is allowed exactly one parse; the
            // arm measures everything AFTER it.
            let service = fixture::database_service("postgres://localhost/zs_unused_build");
            let parses = url_parse_count();
            let opens = backend_open_count();

            init_cache(
                4,
                KernelConfig {
                    workflows: ReadyApps::default(),
                    db_service: Some(service),
                    kv_store: None,
                    storage_backend: None,
                    meter: Arc::new(zeroship_metering::Meter::new()),
                },
            );
            let plugins = plugin_set();
            assert!(
                plugins.iter().any(|p| p.namespace() == "db"),
                "the arm must rule on a plugin set that actually contains db; got {:?}",
                plugins.iter().map(|p| p.namespace()).collect::<Vec<_>>(),
            );

            assert_eq!(
                url_parse_count(),
                parses,
                "building the plugin set must select no backend",
            );
            assert_eq!(
                backend_open_count(),
                opens,
                "building the plugin set must open no pool",
            );

            // Now the call the contract actually names, which runs
            // `DbPlugin::register` on this thread through `Runtime::initialize`.
            compio::runtime::Runtime::new()
                .expect("compio runtime")
                .block_on(async {
                    let runtime = build_runtime(
                        &AppId::mint(),
                        crate::cache::test_modules(
                            br#"export default { fetch() { return new Response("ok"); } }"#,
                        ),
                        AppRuntimeLimits::default(),
                        AppNetPolicy::default(),
                        Some("deploy_build_runtime_guard"),
                        None,
                        &EnvSnapshot::empty(),
                    ).await
                    .expect("the guard needs a runtime that actually built");
                    // The DSN is unreachable, so a build that DID connect would
                    // have failed above - but that is an argument about the
                    // fixture. The counters are the measurement.
                    assert_eq!(
                        url_parse_count(),
                        parses,
                        "build_runtime must select no backend",
                    );
                    assert_eq!(
                        backend_open_count(),
                        opens,
                        "build_runtime must open no pool",
                    );
                    drop(runtime);
                });
        })
        .join()
        .expect("plugin-set build guard thread panicked");
    }

    /// SC-5's discriminating arm: the db plugin prototype is PROCESS-wide.
    ///
    /// Two OS worker threads, each installing the kernel the way `main` does,
    /// must end up holding the SAME `DbPlugin` object - not two structurally
    /// identical ones. Pointer identity is the only instrument that can tell
    /// those apart.
    ///
    /// **Two threads, not two isolates on one thread, and that is the point.**
    /// The per-thread memo this replaces already made two runtimes on ONE
    /// thread share a plugin (the arm below asserts that and passed before this
    /// work began). It could not make two threads share one, because the memo
    /// slot was a `thread_local!`. Run this arm against a `create_plugins` that
    /// mints from the service instead of cloning its prototype and it fails
    /// with two different addresses.
    ///
    /// ONE service is built here and handed to both threads, which is what
    /// `main` does. Building a service per thread would test nothing: two
    /// services own two prototypes by construction, so the arm would fail on a
    /// correct implementation.
    /// **The arm returns the `Arc`s, not their addresses, and that is not a
    /// stylistic choice.** Written the obvious way - each thread reports
    /// `Arc::as_ptr(..) as usize` and the parent compares the two numbers - it
    /// reports EQUAL under the mutation it is supposed to catch. A thread's
    /// plugin set is a `thread_local!`, so it is dropped when the thread exits;
    /// the allocator then hands thread two the address thread one just freed,
    /// and two freshly minted prototypes compare equal. This was not
    /// hypothetical: running the mutation is how it was found, with the arm's
    /// cross-thread assertion green and only its second assertion red. Holding
    /// both `Arc`s alive in the parent makes address reuse impossible, which is
    /// what `Arc::ptr_eq` needs to mean what it says.
    #[test]
    fn db_plugin_prototype_is_one_object_across_worker_threads() {
        fn db_plugin() -> Arc<dyn NativePlugin> {
            let set = plugin_set();
            assert!(!set.is_empty(), "the kernel must register some namespaces");
            set.iter()
                .find(|p| p.namespace() == "db")
                .expect("the db namespace must be registered when a service is installed")
                .clone()
        }
        let service = fixture::database_service("postgres://localhost/zs_unused_shared");
        let kernel = |service: Arc<zeroship_data_v8::service::DbService>| KernelConfig {
            workflows: ReadyApps::default(),
            db_service: Some(service),
            kv_store: None,
            storage_backend: None,
            meter: Arc::new(zeroship_metering::Meter::new()),
        };

        let one = Arc::clone(&service);
        let first = std::thread::spawn(move || {
            init_cache(4, kernel(one));
            db_plugin()
        })
        .join()
        .expect("thread one");

        let two = Arc::clone(&service);
        let second = std::thread::spawn(move || {
            init_cache(4, kernel(two));
            db_plugin()
        })
        .join()
        .expect("thread two");

        assert!(
            Arc::ptr_eq(&first, &second),
            "two OS worker threads must clone ONE db plugin prototype, not mint one each"
        );
        let prototype: Arc<dyn NativePlugin> = service.plugin();
        assert!(
            Arc::ptr_eq(&first, &prototype),
            "the object both threads hold must be the SERVICE's prototype",
        );
    }

    /// SC-5: two runtimes built on one OS thread must SHARE plugin
    /// instances, not each mint their own.
    ///
    /// `build_runtime` used to call `create_plugins` on every build, so a
    /// current isolate and a deploy-pinned isolate on the same thread each
    /// constructed their own `DbPlugin` - and would each resolve their own
    /// backend and their own caches once the service lands. The design's arm
    /// is that `build_runtime` performs no backend selection and clones an
    /// `Arc` instead; this asserts the sharing half of it by pointer identity,
    /// which is the only thing that distinguishes one instance from two
    /// structurally identical ones.
    ///
    /// The second half - that `init_cache` INVALIDATES the set - is the part a
    /// naive "cache forever" implementation fails. Every input the prototype
    /// is built from is written by `init_cache`, so a set retained across a
    /// re-install would keep serving plugins bound to the previous
    /// configuration. That failure is invisible: the plugins still work, just
    /// against the wrong backend.
    #[test]
    fn plugin_set_is_shared_per_thread_and_invalidated_by_init_cache() {
        std::thread::spawn(|| {
            let kernel = || KernelConfig {
                workflows: ReadyApps::default(),
                db_service: Some(fixture::database_service("postgres://localhost/zs_unused")),
                kv_store: None,
                storage_backend: None,
                meter: Arc::new(zeroship_metering::Meter::new()),
            };

            init_cache(4, kernel());
            let a = plugin_set();
            let b = plugin_set();
            assert_eq!(a.len(), b.len(), "the set must be stable across calls");
            assert!(!a.is_empty(), "the kernel must register some namespaces");
            for (x, y) in a.iter().zip(b.iter()) {
                assert!(
                    Arc::ptr_eq(x, y),
                    "two calls on one thread must return the SAME plugin instances, \
                     not equal ones: '{}' differs",
                    x.namespace()
                );
            }

            // Re-installing the kernel must drop the cached prototypes.
            init_cache(4, kernel());
            let c = plugin_set();
            assert_eq!(c.len(), a.len());
            assert!(
                a.iter().zip(c.iter()).all(|(x, y)| !Arc::ptr_eq(x, y)),
                "init_cache must invalidate the cached plugin set; a set retained \
                 across re-install is bound to the previous configuration"
            );
        })
        .join()
        .expect("plugin-sharing guard thread panicked");
    }

    /// Re-installing a kernel with no database must turn the `db` namespace
    /// OFF, not keep the previous one.
    ///
    /// This is the one behaviour the service step changed rather than preserved,
    /// and it went in undisclosed. The slot it replaced was written only inside
    /// `if let Some(url) = kernel.db_url`, so `db_url: None` was indistinguishable
    /// from "leave it alone": a worker re-installed without a database kept
    /// serving `env.db` against the previous DSN. `DB_SERVICE` is assigned on
    /// both arms, so the absence is now expressible.
    ///
    /// The first half is the control. Without it a `create_plugins` that never
    /// registers `db` at all would satisfy the second half exactly as well.
    #[test]
    fn re_installing_the_kernel_without_a_database_clears_the_db_namespace() {
        std::thread::spawn(|| {
            let with_db = || KernelConfig {
                workflows: ReadyApps::default(),
                db_service: Some(fixture::database_service("postgres://localhost/zs_unused_sticky")),
                kv_store: None,
                storage_backend: None,
                meter: Arc::new(zeroship_metering::Meter::new()),
            };

            init_cache(4, with_db());
            assert!(
                plugin_set().iter().any(|p| p.namespace() == "db"),
                "the fixture must start from a kernel that HAS the db namespace",
            );
            assert!(
                DB_SERVICE.with(|service| service.borrow().is_some()),
                "the fixture must start from a bound database service",
            );

            init_cache(
                4,
                KernelConfig {
                    db_service: None,
                    ..with_db()
                },
            );
            assert!(
                !plugin_set().iter().any(|p| p.namespace() == "db"),
                "a kernel installed with no database must not keep serving env.db \
                 against the previous one; got {:?}",
                plugin_set()
                    .iter()
                    .map(|p| p.namespace())
                    .collect::<Vec<_>>(),
            );
            assert!(
                DB_SERVICE.with(|service| service.borrow().is_none()),
                "the previous database service must not survive a kernel that carries none",
            );
        })
        .join()
        .expect("db-service stickiness guard thread panicked");
    }

    /// Structural guard (no external services): when the kernel config carries
    /// a DB URL, a KV URL, and a storage root, the SAME `create_plugins` a
    /// deployed app boots against installs all four
    /// `env.{db,kv,storage,auth}` namespaces. This is the always-runnable
    /// complement to the redis-gated faithful dispatch test in
    /// `handler.rs` — it asserts the plugin VECTOR, the latter asserts the
    /// JS namespaces resolve + round-trip end-to-end.
    ///
    /// Runs on a fresh thread so the kernel thread-locals don't leak into other
    /// tests sharing this thread.
    #[test]
    fn create_plugins_registers_full_kernel_when_configured() {
        std::thread::spawn(|| {
            init_cache(
                4,
                KernelConfig {
                    workflows: ReadyApps::default(),
                    db_service: Some(fixture::database_service("postgres://localhost/zs_unused")),
                    kv_store: Some(
                        zeroship_kv::KvStore::open(&zeroship_kv::KvConfig::Redis {
                            redis: zeroship_kv::RedisConfig::new(zeroship_kv::Topology::Standalone { endpoint: "127.0.0.1:6379".into() }),
                        })
                        .unwrap(),
                    ),
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
                    "create_plugins must register the '{expected}' namespace when configured; got: {namespaces:?}"
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

    #[test]
    fn replacing_kernel_without_kv_removes_its_cached_binding() {
        std::thread::spawn(|| {
            let kernel = |kv_store| KernelConfig {
                workflows: ReadyApps::default(),
                db_service: None,
                kv_store,
                storage_backend: None,
                meter: Arc::new(zeroship_metering::Meter::new()),
            };
            let store = zeroship_kv::KvStore::open(&zeroship_kv::KvConfig::Redis {
                redis: zeroship_kv::RedisConfig::new(zeroship_kv::Topology::Standalone {
                    endpoint: "127.0.0.1:6379".into(),
                }),
            })
            .unwrap();
            init_cache(4, kernel(Some(store)));
            assert!(plugin_set().iter().any(|plugin| plugin.namespace() == "kv"));
            init_cache(4, kernel(None));
            assert!(!plugin_set().iter().any(|plugin| plugin.namespace() == "kv"));
        })
        .join()
        .unwrap();
    }

    /// Degrade-don't-panic: with no kv/storage configured (only db), the
    /// kv + storage namespaces are simply absent — `create_plugins`
    /// never panics. Mirrors the long-standing DB behaviour. Fresh thread
    /// keeps the empty kernel thread-locals isolated.
    #[test]
    fn create_plugins_omits_kv_storage_when_unconfigured() {
        std::thread::spawn(|| {
            init_cache(
                4,
                KernelConfig {
                    workflows: ReadyApps::default(),
                    db_service: None,
                    kv_store: None,
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
            assert!(
                !has("meter"),
                "metering is infrastructure: no env.meter namespace"
            );
            assert!(!has("kv"), "kv absent when unconfigured");
            assert!(!has("storage"), "storage absent when unconfigured");
            assert!(!has("db"), "db absent when unconfigured");
        })
        .join()
        .expect("degrade guard thread panicked");
    }

    #[test]
    fn net_policy_from_app_defaults_to_denied_when_no_grants() {
        let app_id = AppId::mint();
        let policy = net_policy_from_app(&app_id, &AppNetPolicy::default());
        assert!(matches!(policy, NetPolicy::Denied));
    }

    #[test]
    fn net_policy_from_app_builds_a_rule_set_from_projection_rows() {
        let app_id = AppId::mint();
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
                    zeroship_core::net_policy::NamePhase::Resolve {
                        name_accepted: true
                    }
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
        let app_id = AppId::mint();
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
            },
        );
        let NetPolicy::Rules { rules, .. } = &policy else {
            panic!("expected Rules, got {policy:?}");
        };
        assert_eq!(
            rules.name_phase("smtp.example.com", 587),
            zeroship_core::net_policy::NamePhase::Resolve {
                name_accepted: true
            },
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
        let app_id = AppId::mint();
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

    /// The control for the row above, and the one that makes the projection's
    /// only unsafe direction visible at all: a VALID reject row must survive
    /// into the rule set.
    ///
    /// The unparseable-reject row cannot see this. Dropping every reject on the
    /// floor leaves it green - the app is denied either way - so without this
    /// pair `net_policy_from_app`'s own warning, that "a dropped REJECT would
    /// WIDEN it", is a comment with nothing behind it.
    #[test]
    fn net_policy_from_app_keeps_a_valid_reject_row() {
        let app_id = AppId::mint();
        let policy = net_policy_from_app(
            &app_id,
            &AppNetPolicy {
                egress: vec![
                    zeroship_core::types::NetEgressEntry {
                        verdict: Verdict::Accept,
                        destination: "93.184.216.0/24".to_string(),
                        port: 443,
                    },
                    zeroship_core::types::NetEgressEntry {
                        verdict: Verdict::Reject,
                        destination: "93.184.216.7/32".to_string(),
                        port: 443,
                    },
                ],
                max_sockets: 4,
                egress_ceiling_bytes: 1024 * 1024,
            },
        );
        let NetPolicy::Rules { rules, .. } = &policy else {
            panic!("expected Rules, got {policy:?}");
        };
        assert_eq!(
            rules.address_phase("93.184.216.7".parse().unwrap(), 443, false),
            zeroship_core::net_policy::AddressPhase::RangeRejected,
            "the carved-out address must still be refused: a reject the \
             projection drops WIDENS what the app reaches"
        );
        // The control, differing in ONE thing - the address. Everything else in
        // the accept range is still admitted, so the row above is about the
        // reject surviving and not about the whole rule set being dropped.
        assert_eq!(
            rules.address_phase("93.184.216.34".parse().unwrap(), 443, false),
            zeroship_core::net_policy::AddressPhase::Admitted
        );
    }

    /// Wildcards are no longer representable, so a row carrying one is dropped
    /// at load exactly as the API would have refused it at authoring time.
    #[test]
    fn net_policy_from_app_drops_wildcard_rows() {
        let app_id = AppId::mint();
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

        // The control, differing in ONE character: the same row without the
        // `*.`. Without it this row is green against a projection that denies
        // every app there is.
        let exact = net_policy_from_app(
            &app_id,
            &AppNetPolicy {
                egress: vec![zeroship_core::types::NetEgressEntry {
                    verdict: Verdict::Accept,
                    destination: "shared.example.test".to_string(),
                    port: 443,
                }],
                max_sockets: 4,
                egress_ceiling_bytes: 1024 * 1024,
            },
        );
        assert!(matches!(exact, NetPolicy::Rules { .. }));
    }

    #[test]
    fn net_policy_from_app_cannot_produce_trusted() {
        let app_id = AppId::mint();
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
            let runtime = compio::runtime::Runtime::new().expect("compio runtime");

            runtime.block_on(async {
                let app_id = AppId::mint();
                init_cache(
                    4,
                    KernelConfig {
                        workflows: ReadyApps::default(),
                        db_service: None,
                        kv_store: None,
                        storage_backend: None,
                        meter: Arc::new(zeroship_metering::Meter::new()),
                    },
                );
                load_app(
                    app_id.clone(),
                    crate::cache::test_modules(
                        br#"export default { fetch() { return new Response("ok"); } }"#,
                    ),
                    AppRuntimeLimits::default(),
                    AppNetPolicy {
                        egress: vec![zeroship_core::types::NetEgressEntry {
                            verdict: Verdict::Accept,
                            destination: "db.example.com".to_string(),
                            port: 5432,
                        }],
                        max_sockets: 6,
                        egress_ceiling_bytes: 1024 * 1024,
                    },
                    None,
                    None,
                    &zeroship_bundle::Manifest::default(),
                    &EnvSnapshot::empty(),
                ).await
                .expect("app loads");
                let runtime = get_runtime(&app_id).expect("runtime loaded");
                let state = runtime.state();
                let state = state.borrow();
                let NetPolicy::Rules { rules, .. } = &state.net_policy else {
                    panic!("expected Rules, got {:?}", state.net_policy);
                };
                assert_eq!(
                    rules.name_phase("db.example.com", 5432),
                    zeroship_core::net_policy::NamePhase::Resolve {
                        name_accepted: true
                    }
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
    fn an_active_isolate_loads_its_complete_module_graph() {
        std::thread::spawn(|| {
            let runtime = compio::runtime::Runtime::new().unwrap();
            runtime.block_on(async {
                use zeroship_bundle::{BlobStore, LocalDiskBlobStore, RuntimeDescriptorEntry, WorkerCode};

                zeroship_runtime::init::init_v8();
                let app_id = AppId::mint();
                init_cache(4, KernelConfig {
                    workflows: ReadyApps::default(),
                    db_service: None,
                    kv_store: None,
                    storage_backend: None,
                    meter: Arc::new(zeroship_metering::Meter::new()),
                });
                let directory = tempfile::tempdir().unwrap();
                let blobs: Arc<dyn BlobStore> = Arc::new(LocalDiskBlobStore::new(directory.path().into()).unwrap());
                let descriptor = r#"{"version":2,"collections":{}}"#;
                let descriptor_hash = zeroship_bundle::sha256_hex(descriptor.as_bytes());
                blobs.put_blob(&descriptor_hash, descriptor.as_bytes()).await.unwrap();
                for deployment in ["original", "replacement"] {
                    let mut modules = HashMap::new();
                    for (name, source) in [
                        ("app/z-entry.js", "import value from './a-part.js'; export default { async fetch() { const { tail } = await import('./lazy.js'); return new Response(value + tail); } };".to_owned()),
                        ("app/a-part.js", format!("export default '{deployment}';")),
                        ("app/lazy.js", "export const tail = ':dynamic';".to_owned()),
                    ] {
                        let hash = zeroship_bundle::sha256_hex(source.as_bytes());
                        blobs.put_blob(&hash, source.as_bytes()).await.unwrap();
                        modules.insert(name.into(), hash);
                    }
                    let manifest = Manifest {
                        worker: Some(WorkerCode { entry: "app/z-entry.js".into(), modules }),
                        runtime_descriptor: vec![RuntimeDescriptorEntry {
                            label: "main".into(),
                            database_id: zeroship_core::DatabaseId::mint(),
                            primary: true,
                            hash: descriptor_hash.clone(),
                        }],
                        ..Manifest::default()
                    };
                    let executable = crate::executable::load_executable(&manifest, &blobs).await.unwrap();
                    assert_eq!(executable.modules[0].specifier, "app/z-entry.js");
                    assert_eq!(serde_json::from_str::<serde_json::Value>(executable.descriptor.as_deref().unwrap()).unwrap(), serde_json::from_str::<serde_json::Value>(descriptor).unwrap());
                    load_app(app_id.clone(), executable.modules, AppRuntimeLimits::default(),
                        AppNetPolicy::default(), Some(deployment), executable.descriptor.as_deref(),
                        &manifest, &EnvSnapshot::empty()).await.unwrap();
                }
                let active = get_runtime(&app_id).unwrap();
                assert_eq!(fetch_body(&active).await, (200, "replacement:dynamic".into()));
            });
        }).join().unwrap();
    }

    #[test]
    fn load_app_preserves_last_good_isolate_when_descriptor_validation_fails() {
        std::thread::spawn(|| {
            let runtime = compio::runtime::Runtime::new().expect("compio runtime");

            runtime.block_on(async {
                zeroship_runtime::init::init_v8();
                let app_id = AppId::mint();
                init_cache(
                    4,
                    KernelConfig {
                        workflows: ReadyApps::default(),
                        db_service: None,
                        kv_store: None,
                        storage_backend: None,
                        meter: Arc::new(zeroship_metering::Meter::new()),
                    },
                );

                load_app(
                    app_id.clone(),
                    crate::cache::test_modules(br#"export default { fetch() { return new Response("last-good"); } }"#),
                    AppRuntimeLimits::default(),
                    AppNetPolicy::default(),
                    Some("deploy-good"),
                    None,
                    &zeroship_bundle::Manifest::default(),
                    &EnvSnapshot::empty(),
                ).await
                .expect("initial app loads");

                let before = get_runtime(&app_id).expect("initial runtime cached");
                assert_eq!(fetch_body(&before).await, (200, "last-good".to_string()));

                let err = load_app(
                    app_id.clone(),
                    crate::cache::test_modules(br#"export default { fetch() { return new Response("bad-new"); } }"#),
                    AppRuntimeLimits::default(),
                    AppNetPolicy::default(),
                    Some("deploy-bad"),
                    Some(
                        // `version` MUST be 2, and the corruption MUST be the
                        // `indexes` entry below. Both arms assert the error
                        // names `indexes`, so a v1 fixture would fail the
                        // version check FIRST and the arm would pass on the
                        // wrong error - which is exactly what happened when the
                        // descriptor went to v2 and these fixtures did not.
                        r#"{"version":2,"collections":{"notes":{"fields":{"title":{"type":"string"}},"options":{"softDelete":false,"versioning":false},"indexes":[{"name":"bad","fields":[123]}]}}}"#,
                    ),
                    &zeroship_bundle::Manifest::default(),
                    &EnvSnapshot::empty(),
                ).await
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
    fn async_startup_publishes_only_ready_isolates_and_preserves_a_failed_reload() {
        std::thread::spawn(|| {
            use futures::FutureExt;
            compio::runtime::Runtime::new().unwrap().block_on(async {
                init_cache(4, KernelConfig {
                    workflows: ReadyApps::default(),
                    db_service: None,
                    kv_store: None,
                    storage_backend: None,
                    meter: Arc::new(zeroship_metering::Meter::new()),
                });
                let app_id = AppId::mint();
                let manifest = Manifest::default();
                let env = EnvSnapshot::empty();
                let mut loading = Box::pin(load_app(
                    app_id.clone(),
                    test_modules(br#"await new Promise(resolve => setTimeout(resolve, 10));
                        export default { fetch() { return new Response("ready"); } }"#),
                    AppRuntimeLimits::default(), AppNetPolicy::default(),
                    Some("ready-deploy"), None, &manifest, &env,
                ));
                assert!(loading.as_mut().now_or_never().is_none(), "fixture must await startup");
                assert!(get_runtime(&app_id).is_none(), "pending startup must not enter the cache");
                loading.await.expect("ready app loads");
                assert_eq!(fetch_body(&get_runtime(&app_id).unwrap()).await, (200, "ready".into()));

                let mut reloading = Box::pin(load_app(
                    app_id.clone(),
                    test_modules(br#"await new Promise(resolve => setTimeout(resolve, 10));
                        throw new Error("candidate startup rejected");"#),
                    AppRuntimeLimits::default(), AppNetPolicy::default(),
                    Some("failed-deploy"), None, &manifest, &env,
                ));
                assert!(reloading.as_mut().now_or_never().is_none(), "fixture must await reload");
                assert_eq!(fetch_body(&get_runtime(&app_id).unwrap()).await, (200, "ready".into()));
                let error = reloading.await.expect_err("failed startup must reject the load");
                assert!(error.contains("candidate startup rejected"), "{error}");
                assert_eq!(fetch_body(&get_runtime(&app_id).unwrap()).await, (200, "ready".into()));
            });
        }).join().expect("async startup cache test thread");
    }

    #[test]
    fn first_load_with_corrupt_descriptor_hard_errors_without_cached_isolate() {
        std::thread::spawn(|| {
            let runtime = compio::runtime::Runtime::new().expect("compio runtime");

            runtime.block_on(async {
                zeroship_runtime::init::init_v8();
                let app_id = AppId::mint();
                init_cache(
                    4,
                    KernelConfig {
                        workflows: ReadyApps::default(),
                        db_service: None,
                        kv_store: None,
                        storage_backend: None,
                        meter: Arc::new(zeroship_metering::Meter::new()),
                    },
                );

                let err = load_app(
                    app_id.clone(),
                    crate::cache::test_modules(br#"export default { fetch() { return new Response("bad-first"); } }"#),
                    AppRuntimeLimits::default(),
                    AppNetPolicy::default(),
                    Some("deploy-bad"),
                    Some(
                        // `version` MUST be 2, and the corruption MUST be the
                        // `indexes` entry below. Both arms assert the error
                        // names `indexes`, so a v1 fixture would fail the
                        // version check FIRST and the arm would pass on the
                        // wrong error - which is exactly what happened when the
                        // descriptor went to v2 and these fixtures did not.
                        r#"{"version":2,"collections":{"notes":{"fields":{"title":{"type":"string"}},"options":{"softDelete":false,"versioning":false},"indexes":[{"name":"bad","fields":[123]}]}}}"#,
                    ),
                    &zeroship_bundle::Manifest::default(),
                    &EnvSnapshot::empty(),
                ).await
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
            let runtime = compio::runtime::Runtime::new().expect("compio runtime");
            runtime.block_on(async {
                let app_id = AppId::mint();
                init_cache(
                    4,
                    KernelConfig {
                        workflows: ReadyApps::default(),
                        db_service: None,
                        kv_store: None,
                        storage_backend: None,
                        meter: Arc::new(zeroship_metering::Meter::new()),
                    },
                );
                load_app(
                    app_id.clone(),
                    crate::cache::test_modules(
                        br#"export default { fetch() { return new Response("ok"); } }"#,
                    ),
                    AppRuntimeLimits::default(),
                    AppNetPolicy::default(),
                    None,
                    None,
                    &zeroship_bundle::Manifest::default(),
                    &EnvSnapshot::empty(),
                ).await
                .expect("app loads");

                // Backdate the entry so a stamp would be unmistakable.
                let stamped = Instant::now() - Duration::from_secs(600);
                CACHE.with(|c| {
                    let mut cache = c.borrow_mut();
                    cache
                        .as_mut()
                        .unwrap()
                        .isolates
                        .get_mut(&app_id)
                        .unwrap()
                        .last_used = stamped;
                });

                assert!(
                    get_limits(&app_id).is_some(),
                    "the limits read must find the app"
                );

                let after = CACHE.with(|c| {
                    let cache = c.borrow();
                    cache
                        .as_ref()
                        .unwrap()
                        .isolates
                        .get(&app_id)
                        .unwrap()
                        .last_used
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
            let socketed_id = AppId::mint();
            let socketless_id = AppId::mint();
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
                max_size: 2,
            };
            cache.isolates.insert(
                socketed_id.clone(),
                entry(
                    socketed_id.clone(),
                    socketed,
                    now - Duration::from_secs(600),
                ),
            );
            cache.isolates.insert(
                socketless_id.clone(),
                entry(
                    socketless_id.clone(),
                    socketless,
                    now - Duration::from_secs(1),
                ),
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
            let leased_id = AppId::mint();
            let victim_id = AppId::mint();
            let leased = test_runtime();
            let lease = leased.lease_isolate();
            assert_eq!(leased.isolate_lease_count(), 1);
            let victim = test_runtime();
            let mut cache = AppCache {
                isolates: HashMap::new(),
                max_size: 2,
            };
            cache.isolates.insert(
                leased_id.clone(),
                entry(
                    leased_id.clone(),
                    leased.clone(),
                    now - Duration::from_secs(600),
                ),
            );
            cache
                .isolates
                .insert(victim_id.clone(), entry(victim_id.clone(), victim, now));

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
            let app_id = AppId::mint();
            let runtime = test_net_runtime();
            let state = runtime.state();
            let socket_id = zeroship_runtime::node::net::state::alloc_native_socket_id(&state)
                .expect("alloc test socket id");
            zeroship_runtime::node::net::state::reserve_socket_slot(&state, socket_id)
                .expect("reserve test socket slot");
            assert_eq!(runtime.active_native_socket_count(), 1);

            let mut cache = AppCache {
                isolates: HashMap::new(),
                max_size: 1,
            };
            cache.isolates.insert(
                app_id.clone(),
                entry(
                    app_id.clone(),
                    runtime.clone(),
                    now - Duration::from_secs(60),
                ),
            );

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
