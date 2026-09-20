use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use zeroship_core::app_id::AppId;
use zeroship_core::types::{AppVersionInfo, VersionMap};
use zeroship_runtime::{EnvSnapshot, RuntimeLimits};

use crate::health::WorkerReadiness;
use crate::{cache, WorkerConfig};

const CONTROL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

use crate::executable::load_executable;

/// Process-wide snapshot of `/internal/versions`, refreshed by a single
/// background poller regardless of how many ntex worker threads are running.
pub type SharedVersions = Arc<RwLock<Option<VersionMap>>>;

/// One entry in `SharedEnvs`: the parsed env snapshot together with
/// the version it was hydrated against. Bundling the version here
/// lets reconcile dedup cross-thread — if thread A already stored
/// v=5, thread B comparing `cached.version vs info.env_version`
/// sees no change and skips its own fetch.
#[allow(missing_debug_implementations)]
pub struct CachedEnv {
    pub snapshot: EnvSnapshot,
    pub version: i64,
}

/// Process-wide env cache. Single source of truth — secret rotation
/// invalidates ALL threads at once via one `write()` lock.
///
/// `Arc<CachedEnv>` lets the handler clone an Arc (cheap) under the
/// read lock and use it after dropping the lock — so the hot path
/// never holds the lock across `await`.
pub type SharedEnvs = Arc<RwLock<HashMap<AppId, Arc<CachedEnv>>>>;

/// Start the single process-wide version-polling task. All ntex worker
/// threads observe its output through `shared`. Also takes a handle
/// to `SharedEnvs` so it can GC env entries for apps that the control
/// plane has deleted (see `version_poll_loop`).
pub fn start_version_poller(
    config: Arc<WorkerConfig>,
    shared: SharedVersions,
    envs: SharedEnvs,
    readiness: Arc<WorkerReadiness>,
    db_service: Option<Arc<zeroship_data_v8::service::DbService>>,
) {
    compio::runtime::spawn(async move {
        version_poll_loop(config, shared, envs, readiness, db_service).await;
    })
    .detach();
}

/// Start the per-thread reconcile loop. Reads the shared version map
/// populated by `start_version_poller` and updates this thread's local cache.
/// No HTTP traffic for version polling — the version map is already in
/// memory. Env fetches do hit the control plane (one HTTP per env
/// version bump) and write back into the process-wide `SharedEnvs`.
pub fn start_sync(config: Arc<WorkerConfig>, shared: SharedVersions, envs: SharedEnvs) {
    compio::runtime::spawn(async move {
        reconcile_loop(config, shared, envs).await;
    })
    .detach();
}

async fn version_poll_loop(
    config: Arc<WorkerConfig>,
    shared: SharedVersions,
    envs: SharedEnvs,
    readiness: Arc<WorkerReadiness>,
    db_service: Option<Arc<zeroship_data_v8::service::DbService>>,
) {
    let interval = std::time::Duration::from_secs(config.poll_interval_secs);
    let mut pending_cdc_deprovision = std::collections::HashSet::new();
    loop {
        match poll_versions(&config).await {
            Ok(versions) => {
                // A deleted app no longer has a control-plane row to drive a
                // central database cleanup. Detect the removal in this single
                // process-wide poller, retry failures on later polls, and let
                // every worker container run the idempotent cluster teardown.
                if db_service.is_some() {
                    if let Ok(guard) = shared.read() {
                        if let Some(previous) = guard.as_ref() {
                            pending_cdc_deprovision.extend(
                                previous
                                    .keys()
                                    .filter(|app_id| !versions.contains_key(app_id))
                                    .cloned(),
                            );
                        }
                    }
                }
                // Close local subscriptions after control removes an app.
                if let Some(service) = db_service.as_deref() {
                    let pending: Vec<AppId> = pending_cdc_deprovision.iter().cloned().collect();
                    for app_id in pending {
                        match service.lifecycle().deprovision_app(app_id.as_str()).await {
                            Ok(()) => {
                                pending_cdc_deprovision.remove(&app_id);
                                tracing::info!(
                                    app_id = app_id.as_str(),
                                    "worker-sync: deleted app CDC deprovisioned"
                                );
                            }
                            Err(error) => {
                                tracing::error!(
                                    app_id = app_id.as_str(),
                                    error = %error,
                                    "worker-sync: deleted app CDC deprovision failed; retrying"
                                );
                            }
                        }
                    }
                }
                // GC SharedEnvs against the latest known-app set BEFORE
                // swapping the new version map in. Apps deleted from
                // the control plane drop out of `versions`; their env
                // entries would otherwise leak forever (per-thread
                // reconcile only fires for locally-cached apps, so an
                // app that's been LRU-evicted from every thread gets
                // no `None`-branch cleanup).
                if let Ok(mut e) = envs.write() {
                    e.retain(|app_id, _| versions.contains_key(app_id));
                }
                if let Ok(mut guard) = shared.write() {
                    *guard = Some(versions);
                }
                // Stamped ONLY here: the control plane answered and its body
                // parsed. The `Err` arm below leaves the stamp alone, so a
                // control-plane outage ages this worker out of `/readyz`
                // within the staleness budget.
                readiness.control.mark_success();
            }
            Err(e) => tracing::error!(error = %e, "worker-sync: poll error"),
        }
        compio::time::sleep(interval).await;
    }
}

/// The credential for the version poll.
///
/// Still the shared control key, and deliberately so: `/internal/versions` is
/// on the POLLED tier, which the app-metadata distribution work replaces. It
/// is out of scope for the tiering rather than exempt from it - whatever
/// survives that work inherits the POLLED reasoning, where the rate is per
/// poll per process.
fn version_poll_authorization(config: &WorkerConfig) -> Option<String> {
    (!config.control_key.is_empty()).then(|| format!("Bearer {}", config.control_key))
}

async fn poll_versions(config: &WorkerConfig) -> Result<VersionMap, String> {
    let url = format!("{}/internal/versions", config.control_url);
    let response = http_get(&url, version_poll_authorization(config).as_deref())
        .await
        .map_err(|e| format!("fetch versions: {e}"))?;
    serde_json::from_str(&response).map_err(|e| format!("parse versions: {e}"))
}

async fn reconcile_loop(config: Arc<WorkerConfig>, shared: SharedVersions, envs: SharedEnvs) {
    let interval = std::time::Duration::from_secs(config.poll_interval_secs);
    // Per-thread startup jitter so the N ntex workers don't all wake at
    // the same instant and stampede the control plane every cycle.
    // Address-derived "thread id" is portable across compio runtimes.
    let jitter_seed = (&envs as *const _) as u64;
    let initial = std::time::Duration::from_millis(jitter_seed % interval.as_millis() as u64);
    compio::time::sleep(initial).await;
    loop {
        compio::time::sleep(interval).await;
        crate::metrics::inc(&crate::metrics::RECONCILE_ITERATIONS_TOTAL);
        // Snapshot the shared map under a brief read lock, then drop the lock
        // before doing any async work (no .await while holding a std RwLock).
        let snapshot: Option<VersionMap> = match shared.read() {
            Ok(g) => g.clone(),
            Err(_) => {
                crate::metrics::inc(&crate::metrics::LOCK_POISONED_TOTAL);
                tracing::error!("worker-sync: SharedVersions lock poisoned — restart recommended");
                continue;
            }
        };
        let Some(versions) = snapshot else { continue };
        if let Err(e) = reconcile_once(&config, &versions, &envs).await {
            tracing::error!(error = %e, "worker-sync: reconcile error");
        }
    }
}

/// PHASE 2 swap decision: does this thread's cached isolate need to be
/// torn down and reloaded to match the control plane's current app
/// state? True when any of:
///
/// - the deploy hash changed (new code),
/// - the runtime limits changed (CPU / wall / heap),
/// - the env version changed (var/secret rotation),
/// - the raw-TCP net policy changed (grant/revoke/cap edit).
///
/// The env arm is SEC-7: a pure env bump (dashboard secret rotation, no
/// redeploy) must reload the isolate. The runtime materializes the `env`
/// argument object and `process.env` once per isolate (first dispatch),
/// so refreshing `SharedEnvs` alone never reaches an already-running
/// isolate — a revoked credential would keep being served until LRU
/// eviction or the next code deploy. A full isolate swap also destroys
/// user-code module state that captured the old credential (e.g. a
/// module-level API client), which an in-place `env_obj` rebuild wouldn't.
///
/// `loaded` is the per-thread record of what the isolate was loaded
/// against (`cache::LoadedMeta`) — NOT the process-wide `SharedEnvs`
/// entry, which PHASE 1 refreshes independently of any isolate (by the
/// time PHASE 2 runs, `SharedEnvs` usually already matches
/// `info.env_version`, so comparing against it would mask the rotation).
/// `loaded == None` (isolate cached but nothing recorded) is treated as
/// "unknown state" → reload, never "assume current".
/// Reject a polling interval the reconcile loops cannot run on.
///
/// Zero is the case that matters. The per-thread jitter divides by the interval,
/// so a zero interval panics every reconcile task while the HTTP server keeps
/// serving: the process looks healthy and deploy, env and policy reconciliation
/// is simply dead. The version poller would also spin with no sleep between
/// control-plane calls.
pub fn validate_poll_interval_secs(secs: u64) -> Result<u64, String> {
    if secs == 0 {
        return Err(
            "poll interval must be at least 1 second: zero stops reconciliation \
             entirely and spins against the control plane"
                .to_string(),
        );
    }
    Ok(secs)
}

pub fn needs_reload(
    loaded: Option<&cache::LoadedMeta>,
    local_limits: Option<RuntimeLimits>,
    info: &AppVersionInfo,
) -> bool {
    // Compare the two hashes directly. Asking only whether a PRESENT remote hash
    // differs would treat a remote `None` as "unchanged", so an app that lost its
    // live deploy would keep serving the code it had cached.
    let hash_changed = loaded.and_then(|m| m.deploy_hash.as_deref()) != info.deploy_hash.as_deref();
    let limits_changed = local_limits != Some(cache::runtime_limits_from_app(&info.runtime));
    let env_changed = loaded.map(|m| m.env_version) != Some(info.env_version);
    let net_policy_changed = loaded.map(|m| &m.net_policy) != Some(&info.net_policy);
    hash_changed || limits_changed || env_changed || net_policy_changed
}

async fn reconcile_once(
    config: &WorkerConfig,
    versions: &VersionMap,
    envs: &SharedEnvs,
) -> Result<(), String> {
    // PHASE 1: env-only refresh for any app whose env is in SharedEnvs
    // (not just locally cached). Without this, an app loaded only on
    // thread A would have stale env until thread A's next reconcile
    // — staleness up to 2 × poll_interval. Iterating the union of
    // (envs.keys() ∩ versions.keys()) means thread B's reconcile
    // refreshes envs for apps thread A loaded too. The version dedup
    // (`cached_env_version == info.env_version`) skips work that's
    // already up-to-date, so this isn't N-thread amplification.
    let env_app_ids: Vec<AppId> = envs
        .read()
        .ok()
        .map(|e| e.keys().cloned().collect())
        .unwrap_or_default();
    for app_id in &env_app_ids {
        let Some(info) = versions.get(app_id) else {
            continue;
        };
        let cached_version = cached_env_version(envs, app_id);
        if cached_version == Some(info.env_version) {
            continue;
        }
        match fetch_app_env(&config.control_url, &config.service_auth, app_id).await {
            Ok(env_json) => {
                if let Err(e) = put_env_from_json(envs, app_id.clone(), &env_json, info.env_version)
                {
                    tracing::warn!(app_id = app_id.as_str(), error = %e, "worker-sync: env parse failed");
                }
            }
            Err(e) => {
                crate::metrics::inc(&crate::metrics::ENV_FETCH_FAILURES);
                tracing::warn!(app_id = app_id.as_str(), error = %e, "worker-sync: env-only refresh failed");
            }
        }
    }

    // PHASE 2: bundle / limits updates for locally-cached apps.
    // Bundle work is per-thread (each thread has its own V8 cache);
    // env work above is process-wide.
    let local_app_ids = cache::all_app_ids();
    for local_id in &local_app_ids {
        match versions.get(local_id) {
            // App still exists — check if deploy hash, limits, or env_version changed.
            Some(info) => {
                let loaded = cache::get_loaded_meta(local_id);
                let local_limits = cache::get_limits(local_id);
                if needs_reload(loaded.as_ref(), local_limits, info) {
                    let manifest = match info.manifest.as_ref() {
                        Some(manifest) if manifest.worker.is_some() => manifest,
                        _ => {
                            cache::evict_app(local_id);
                            cache::remove_loaded_meta(local_id);
                            continue;
                        }
                    };
                    match load_executable(manifest, &config.blob_store).await {
                        Ok(executable) => {
                            // Order: make sure SharedEnvs is current BEFORE
                            // the V8 swap. Otherwise concurrent dispatches
                            // on the same thread between cache::load_app and
                            // put_env_from_json see new code with stale env
                            // (or 503 with no env at all). PHASE 1 normally
                            // refreshed SharedEnvs already; re-fetch only
                            // when it is still stale (e.g. PHASE 1's fetch
                            // failed this cycle).
                            let shared_env_stale =
                                cached_env_version(envs, local_id) != Some(info.env_version);
                            let env_for_load: Option<String> = if shared_env_stale {
                                match fetch_app_env(
                                    &config.control_url,
                                    &config.service_auth,
                                    local_id,
                                )
                                .await
                                {
                                    Ok(json) => Some(json),
                                    Err(e) => {
                                        crate::metrics::inc(&crate::metrics::ENV_FETCH_FAILURES);
                                        tracing::warn!(app_id = local_id.as_str(), error = %e, "worker-sync: fetch env failed");
                                        continue; // don't swap V8 with no env
                                    }
                                }
                            } else {
                                None
                            };

                            if let Some(env_json) = env_for_load.as_deref() {
                                if let Err(e) = put_env_from_json(
                                    envs,
                                    local_id.clone(),
                                    env_json,
                                    info.env_version,
                                ) {
                                    tracing::warn!(app_id = local_id.as_str(), error = %e, "worker-sync: env parse failed");
                                    continue;
                                }
                            }

                            let Some(env_entry) = get_env(envs, local_id) else {
                                tracing::warn!(
                                    app_id = local_id.as_str(),
                                    "worker-sync: env cache missing before app load"
                                );
                                continue;
                            };
                            match cache::load_app(
                                local_id.clone(),
                                executable.modules,
                                info.runtime.clone(),
                                info.net_policy.clone(),
                                info.deploy_hash.as_deref(),
                                executable.descriptor.as_deref(),
                                manifest,
                                &env_entry.snapshot,
                            )
                            .await
                            {
                                Ok(()) => {
                                    // Record what the fresh isolate was loaded
                                    // against — the reload decision above keys
                                    // off this on the next cycle.
                                    cache::set_loaded_meta(
                                        local_id.clone(),
                                        cache::LoadedMeta {
                                            deploy_hash: info.deploy_hash.clone(),
                                            env_version: info.env_version,
                                            net_policy: info.net_policy.clone(),
                                        },
                                    );
                                    tracing::info!(
                                        app_id = local_id.as_str(),
                                        plan_id = %info.plan_id,
                                        deploy_hash = ?info.deploy_hash,
                                        "worker-sync: app updated"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(app_id = local_id.as_str(), error = %e, "worker-sync: app load failed");
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            crate::metrics::inc(&crate::metrics::BUNDLE_FETCH_FAILURES);
                            tracing::warn!(app_id = local_id.as_str(), error = %e, "worker-sync: fetch bundle failed");
                        }
                    }
                }
            }
            // App deleted from control plane — evict
            None => {
                tracing::info!(
                    app_id = local_id.as_str(),
                    "worker-sync: evicting deleted app"
                );
                cache::evict_app(local_id);
                cache::remove_loaded_meta(local_id);
                // Env entry GC'd centrally by version_poll_loop's
                // retain step; no per-thread removal needed.
                if let Ok(mut e) = envs.write() {
                    e.remove(local_id);
                }
            }
        }
    }

    Ok(())
}

/// Fetch just one app's version info. Used by `load_on_demand` in `handler.rs`
/// when a request arrives for an app that's not yet in the thread-local cache.
pub async fn fetch_app_version(
    url_base: &str,
    service_auth: &zeroship_core::service_peers::ServiceAuth,
    app_id: &AppId,
) -> Result<AppVersionInfo, String> {
    let url = format!("{url_base}/internal/apps/{}", app_id.as_str());
    let body = http_get(&url, control_authorization(service_auth)?.as_deref()).await?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// Fetch the merged env for an app in the split `{vars, secrets, expose}`
/// wire shape (see `crates/zeroship-runtime/src/fetch_outcome.rs::EnvSnapshot`).
/// The result is a JSON object string passed through verbatim to
/// `EnvSnapshot::from_validated_json` at cache-insert time so the hot
/// path skips the parse-then-reserialize round-trip.
///
/// Control returns 404 / 500 for non-existent apps or decrypt failures;
/// in both cases we propagate the error string so the caller can log it
/// and decide whether to evict the bundle or fail the request.
pub async fn fetch_app_env(
    url_base: &str,
    service_auth: &zeroship_core::service_peers::ServiceAuth,
    app_id: &AppId,
) -> Result<String, String> {
    fetch_app_env_supplying(
        url_base,
        service_auth,
        app_id,
        crate::cache::project_keys().as_deref(),
        crate::cache::app_bindings().as_deref(),
    )
    .await
}

/// [`fetch_app_env`] for a thread without the HTTP kernel, such as the
/// workflow host: `keys` is the database service's process-wide key source.
pub async fn fetch_app_env_supplying(
    url_base: &str,
    service_auth: &zeroship_core::service_peers::ServiceAuth,
    app_id: &AppId,
    keys: Option<&zeroship_data_orm::encryption::SuppliedProjectKeys>,
    bindings: Option<&zeroship_data_orm::resolved_bindings::SuppliedAppBindings>,
) -> Result<String, String> {
    // Resolve host material before publishing the environment or creating an
    // isolate. Every thread uses the database service's shared source.
    if let Some(keys) = keys {
        let app = app_id.as_str();
        if !keys.is_bound(app).map_err(|error| error.to_string())? {
            let url = format!("{url_base}/internal/apps/{}/data-key", app_id.as_str());
            let body = http_get(&url, control_authorization(service_auth)?.as_deref()).await?;
            let key = zeroship_core::project_data_key::ProjectDataKey::from_json(body)
                .map_err(|_| "invalid control project key response".to_string())?;
            keys.supply(app, key.project_id.as_str(), *key.key())
                .map_err(|error| error.to_string())?;
        }
    }
    // The app's database binding, resolved by Control and composed by nobody
    // else. An app Control serves no live binding for gets no `env.db`, which
    // is the fail-closed direction: a namespace whose every call would be
    // refused at session setup is worse than an absent one.
    if let Some(bindings) = bindings {
        let app = app_id.as_str();
        if !bindings.is_bound(app).map_err(|error| error.to_string())? {
            let url = format!("{url_base}/internal/apps/{app}/bindings");
            match http_get(&url, control_authorization(service_auth)?.as_deref()).await {
                Ok(body) => {
                    // EVERY live binding, because `env.databases` reaches every
                    // database this app binds. Supplying only the first would
                    // leave the rest unresolvable at isolate build.
                    for resolved in parse_resolved_bindings(&body)? {
                        bindings
                            .supply(app, resolved)
                            .map_err(|error| error.to_string())?;
                    }
                }
                // An app with no live binding is ordinary: not every app
                // declares a database. It is recorded and the environment is
                // published without one.
                Err(error) => tracing::debug!(
                    app_id = app,
                    %error,
                    "worker: control served no live database binding for this app"
                ),
            }
        }
    }
    let url = format!("{url_base}/internal/apps/{}/env", app_id.as_str());
    http_get(&url, control_authorization(service_auth)?.as_deref()).await
}

/// Decode Control's binding response: the SET of live bindings for one app.
///
/// Every field is parsed through its typed id, so a malformed response is a
/// refusal rather than a binding that composes a role name nothing created.
/// One malformed entry refuses the whole response: a partial set would leave
/// an isolate reaching for a handle the host never supplied.
fn parse_resolved_bindings(
    body: &str,
) -> Result<Vec<zeroship_data_orm::resolved_bindings::ResolvedBinding>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| "invalid control binding response".to_string())?;
    let entries = value
        .get("bindings")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "control binding response has no bindings".to_string())?;
    entries.iter().map(parse_resolved_binding).collect()
}

fn parse_resolved_binding(
    value: &serde_json::Value,
) -> Result<zeroship_data_orm::resolved_bindings::ResolvedBinding, String> {
    let field = |name: &str| -> Result<String, String> {
        value
            .get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("control binding response has no {name}"))
    };
    let database = zeroship_core::DatabaseId::parse(&field("database_id")?)
        .map_err(|error| error.to_string())?;
    let binding =
        zeroship_core::BindingId::parse(&field("binding_id")?).map_err(|error| error.to_string())?;
    let epoch = value
        .get("schema_epoch")
        .and_then(serde_json::Value::as_u64)
        .and_then(|epoch| u32::try_from(epoch).ok())
        .ok_or_else(|| "control binding response has no schema_epoch".to_string())?;
    Ok(zeroship_data_orm::resolved_bindings::ResolvedBinding {
        database,
        binding,
        epoch,
    })
}

/// Mint this worker's credential for one control-plane call.
///
/// The FULL assertion profile: these two reads fire once per app load and
/// again on a version change, so the single-use claim control writes is
/// proportional to app loads rather than to end-user traffic.
///
/// A fresh assertion per call, deliberately. Caching one would defeat the
/// single-use property it is minted to satisfy - the second presentation is
/// exactly what the callee's replay store refuses.
fn control_authorization(
    service_auth: &zeroship_core::service_peers::ServiceAuth,
) -> Result<Option<String>, String> {
    let control = zeroship_core::service_peers::service_issuer(
        zeroship_core::service_peers::CONTROL_SERVICE_NAME,
    )
    .map_err(|e| format!("control service issuer is malformed: {e}"))?;
    match service_auth.authorization_for(&control) {
        Some(header) => Ok(Some(header)),
        None => Err(
            "no service key material configured; cannot assert this worker's identity to control"
                .to_owned(),
        ),
    }
}

/// Parse + insert an env JSON into the shared cache, tagged with the
/// version it was hydrated against.
///
/// Validation is `IgnoredAny` — confirms the bytes are valid JSON
/// without materializing a `serde_json::Value` (and without the
/// follow-up `value.to_string()` re-serialize). The wire bytes
/// arrive as JSON and the JS side parses with `JSON.parse`, so
/// keeping them as bytes-in / bytes-out is the only sensible path.
pub fn put_env_from_json(
    envs: &SharedEnvs,
    app_id: AppId,
    env_json: &str,
    version: i64,
) -> Result<(), String> {
    serde_json::from_str::<serde::de::IgnoredAny>(env_json)
        .map_err(|e| format!("env json: {e}"))?;
    let entry = Arc::new(CachedEnv {
        snapshot: EnvSnapshot::from_validated_json(env_json.to_string()),
        version,
    });
    envs.write()
        .map_err(|e| format!("envs lock poisoned: {e}"))?
        .insert(app_id, entry);
    Ok(())
}

/// Read the cached env entry for an app. Cheap — clones an Arc under a
/// brief read lock; never holds the lock across `await`.
pub fn get_env(envs: &SharedEnvs, app_id: &AppId) -> Option<Arc<CachedEnv>> {
    envs.read().ok()?.get(app_id).cloned()
}

/// Read just the version of the cached env, if any. Used by reconcile
/// to dedup across threads — if SharedEnvs already has the latest
/// version, no fetch needed.
pub fn cached_env_version(envs: &SharedEnvs, app_id: &AppId) -> Option<i64> {
    envs.read().ok()?.get(app_id).map(|e| e.version)
}

/// Remove the cached env for an app. Used when the bundle load /
/// env-fetch fails so the next request retries from scratch.
#[allow(dead_code)]
pub fn remove_env(envs: &SharedEnvs, app_id: &AppId) {
    if let Ok(mut e) = envs.write() {
        e.remove(app_id);
    }
}

/// Simple HTTP GET returning response body as string.
async fn http_get(url: &str, authorization: Option<&str>) -> Result<String, String> {
    let bytes = http_get_bytes(url, authorization).await?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

thread_local! {
    /// Per-thread cyper client for control-plane calls.
    ///
    /// **Must be thread-local**, NOT a process-wide static. cyper's
    /// connector uses `SendWrapper` (panics if dereferenced from a
    /// thread other than the one that created it). With multiple ntex
    /// worker threads each running `reconcile_loop`, a process-wide
    /// `OnceLock<Client>` would cache pooled connections on whichever
    /// thread ran first, then panic the moment a different thread's
    /// reconcile fires. Per-thread costs a small handful of idle
    /// connections per host — cheap and correct.
    static CONTROL_CLIENT: cyper::Client = cyper::Client::new();
}

/// Clone this thread's cyper client. `cyper::Client` is cheaply clonable
/// (Arc internally); the clone shares the per-thread connection pool.
/// Compio futures stay on the thread that spawned them, so the cloned
/// client never touches the SendWrapper from another thread.
fn this_thread_control_client() -> cyper::Client {
    CONTROL_CLIENT.with(|c| c.clone())
}

/// HTTP GET returning the response body as bytes. Uses a proper HTTP client
/// (cyper) so the caller gets correct query-string handling, TLS support,
/// chunked/compressed decoding, keep-alive, and status-code parsing.
///
/// Used by the env / version polls (still HTTP). Worker-bundle bytes
/// come from `BlobStore` directly — see `reconcile_once` and
/// `handler::load_on_demand`.
async fn http_get_bytes(url: &str, authorization: Option<&str>) -> Result<Vec<u8>, String> {
    compio::time::timeout(
        CONTROL_REQUEST_TIMEOUT,
        http_get_bytes_inner(url, authorization),
    )
    .await
    .map_err(|_| control_timeout_error())?
}

fn control_timeout_error() -> String {
    format!(
        "control request timed out after {}s",
        CONTROL_REQUEST_TIMEOUT.as_secs()
    )
}

async fn http_get_bytes_inner(url: &str, authorization: Option<&str>) -> Result<Vec<u8>, String> {
    let client = this_thread_control_client();
    let mut builder = client
        .get(url)
        .map_err(|e| format!("invalid control URL: {e}"))?;
    // The COMPLETE header value, built by the caller. Two different credentials
    // ride this one function - a service assertion on the privileged per-app
    // reads, the shared control key on the version poll - so the scheme is the
    // caller's to choose and this function never invents one.
    if let Some(value) = authorization {
        builder = builder
            .header("authorization", value)
            .map_err(|e| format!("invalid auth header: {e}"))?;
    }

    let response = builder
        .send()
        .await
        .map_err(|e| format!("control request failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "HTTP error: {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        ));
    }

    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("read body: {e}"))
}

#[cfg(test)]
mod tests;
