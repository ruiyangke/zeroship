use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use uuid::Uuid;
use zeroship_core::types::{AppVersionInfo, VersionMap};
use zeroship_runtime::{EnvSnapshot, RuntimeLimits};

use crate::health::WorkerReadiness;
use crate::{cache, WorkerConfig};

const CONTROL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

use crate::executable::load_executable;

/// Process-wide snapshot of `/internal/versions`, refreshed by a single
/// background poller regardless of how many ntex worker threads are running.
///
/// This replaces the previous "one HTTP poll per thread" behaviour that
/// multiplied control-plane traffic by `workers_count`.
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
pub type SharedEnvs = Arc<RwLock<HashMap<Uuid, Arc<CachedEnv>>>>;

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
                                    .copied(),
                            );
                        }
                    }
                }
                // Close local subscriptions after control removes an app.
                if let Some(service) = db_service.as_deref() {
                    let pending: Vec<Uuid> = pending_cdc_deprovision.iter().copied().collect();
                    for app_id in pending {
                        match service.lifecycle().deprovision_app(&app_id.to_string()).await {
                            Ok(()) => {
                                pending_cdc_deprovision.remove(&app_id);
                                tracing::info!(
                                    app_id = %app_id,
                                    "worker-sync: deleted app CDC deprovisioned"
                                );
                            }
                            Err(error) => {
                                tracing::error!(
                                    app_id = %app_id,
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
    let response =
        http_get(&url, version_poll_authorization(config).as_deref())
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

async fn reconcile_once(config: &WorkerConfig, versions: &VersionMap, envs: &SharedEnvs) -> Result<(), String> {
    // PHASE 1: env-only refresh for any app whose env is in SharedEnvs
    // (not just locally cached). Without this, an app loaded only on
    // thread A would have stale env until thread A's next reconcile
    // — staleness up to 2 × poll_interval. Iterating the union of
    // (envs.keys() ∩ versions.keys()) means thread B's reconcile
    // refreshes envs for apps thread A loaded too. The version dedup
    // (`cached_env_version == info.env_version`) skips work that's
    // already up-to-date, so this isn't N-thread amplification.
    let env_app_ids: Vec<Uuid> = envs
        .read()
        .ok()
        .map(|e| e.keys().copied().collect())
        .unwrap_or_default();
    for app_id in &env_app_ids {
        let Some(info) = versions.get(app_id) else { continue };
        let cached_version = cached_env_version(envs, app_id);
        if cached_version == Some(info.env_version) { continue; }
        match fetch_app_env(&config.control_url, &config.service_auth, app_id).await {
            Ok(env_json) => {
                if let Err(e) = put_env_from_json(envs, *app_id, &env_json, info.env_version) {
                    tracing::warn!(app_id = %app_id, error = %e, "worker-sync: env parse failed");
                }
            }
            Err(e) => {
                crate::metrics::inc(&crate::metrics::ENV_FETCH_FAILURES);
                tracing::warn!(app_id = %app_id, error = %e, "worker-sync: env-only refresh failed");
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
                                match fetch_app_env(&config.control_url, &config.service_auth, local_id).await {
                                    Ok(json) => Some(json),
                                    Err(e) => {
                                        crate::metrics::inc(&crate::metrics::ENV_FETCH_FAILURES);
                                        tracing::warn!(app_id = %local_id, error = %e, "worker-sync: fetch env failed");
                                        continue; // don't swap V8 with no env
                                    }
                                }
                            } else {
                                None
                            };

                            if let Some(env_json) = env_for_load.as_deref() {
                                if let Err(e) = put_env_from_json(envs, *local_id, env_json, info.env_version) {
                                    tracing::warn!(app_id = %local_id, error = %e, "worker-sync: env parse failed");
                                    continue;
                                }
                            }

                            let Some(env_entry) = get_env(envs, local_id) else {
                                tracing::warn!(app_id = %local_id, "worker-sync: env cache missing before app load");
                                continue;
                            };
                            match cache::load_app(
                                *local_id,
                                executable.modules,
                                info.runtime.clone(),
                                info.net_policy.clone(),
                                info.deploy_hash.as_deref(),
                                executable.descriptor.as_deref(),
                                manifest,
                                &env_entry.snapshot,
                            ) {
                                Ok(()) => {
                                    // Record what the fresh isolate was loaded
                                    // against — the reload decision above keys
                                    // off this on the next cycle.
                                    cache::set_loaded_meta(*local_id, cache::LoadedMeta {
                                        deploy_hash: info.deploy_hash.clone(),
                                        env_version: info.env_version,
                                        net_policy: info.net_policy.clone(),
                                    });
                                    tracing::info!(
                                        app_id = %local_id,
                                        plan_id = %info.plan_id,
                                        deploy_hash = ?info.deploy_hash,
                                        "worker-sync: app updated"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(app_id = %local_id, error = %e, "worker-sync: app load failed");
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            crate::metrics::inc(&crate::metrics::BUNDLE_FETCH_FAILURES);
                            tracing::warn!(app_id = %local_id, error = %e, "worker-sync: fetch bundle failed");
                        }
                    }
                }
            }
            // App deleted from control plane — evict
            None => {
                tracing::info!(app_id = %local_id, "worker-sync: evicting deleted app");
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
    app_id: &Uuid,
) -> Result<AppVersionInfo, String> {
    let url = format!("{url_base}/internal/apps/{app_id}");
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
    app_id: &Uuid,
) -> Result<String, String> {
    // Resolve host material before publishing the environment or creating an
    // isolate. Every thread uses the database service's shared source.
    if let Some(keys) = crate::cache::project_keys() {
        let app = app_id.to_string();
        if !keys.is_bound(&app).map_err(|error| error.to_string())? {
            let url = format!("{url_base}/internal/apps/{app_id}/data-key");
            let body = http_get(&url, control_authorization(service_auth)?.as_deref()).await?;
            let key = zeroship_core::project_data_key::ProjectDataKey::from_json(body)
                .map_err(|_| "invalid control project key response".to_string())?;
            keys.supply(&app, key.project_id.as_str(), *key.key())
                .map_err(|error| error.to_string())?;
        }
    }
    let url = format!("{url_base}/internal/apps/{app_id}/env");
    http_get(&url, control_authorization(service_auth)?.as_deref()).await
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
/// follow-up `value.to_string()` re-serialize the previous code did).
/// For a 64 KiB env the saved work is meaningful; the wire bytes
/// arrive as JSON and the JS side parses with `JSON.parse`, so
/// keeping them as bytes-in / bytes-out is the only sensible path.
pub fn put_env_from_json(
    envs: &SharedEnvs,
    app_id: Uuid,
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
pub fn get_env(envs: &SharedEnvs, app_id: &Uuid) -> Option<Arc<CachedEnv>> {
    envs.read().ok()?.get(app_id).cloned()
}

/// Read just the version of the cached env, if any. Used by reconcile
/// to dedup across threads — if SharedEnvs already has the latest
/// version, no fetch needed.
pub fn cached_env_version(envs: &SharedEnvs, app_id: &Uuid) -> Option<i64> {
    envs.read().ok()?.get(app_id).map(|e| e.version)
}

/// Remove the cached env for an app. Used when the bundle load /
/// env-fetch fails so the next request retries from scratch.
#[allow(dead_code)]
pub fn remove_env(envs: &SharedEnvs, app_id: &Uuid) {
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
/// chunked/compressed decoding, keep-alive, and status-code parsing —
/// instead of a hand-rolled TCP GET that assumed port 80, dropped the query,
/// and split the body on `\r\n\r\n`.
///
/// Used by the env / version polls (still HTTP). Worker-bundle bytes
/// come from `BlobStore` directly — see `reconcile_once` and
/// `handler::load_on_demand`.
async fn http_get_bytes(url: &str, authorization: Option<&str>) -> Result<Vec<u8>, String> {
    compio::time::timeout(CONTROL_REQUEST_TIMEOUT, http_get_bytes_inner(url, authorization))
        .await
        .map_err(|_| control_timeout_error())?
}

fn control_timeout_error() -> String {
    format!(
        "control request timed out after {}s",
        CONTROL_REQUEST_TIMEOUT.as_secs()
    )
}

async fn http_get_bytes_inner(
    url: &str,
    authorization: Option<&str>,
) -> Result<Vec<u8>, String> {
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
mod tests {
    use std::sync::{Arc, RwLock};

    use ntex::http::StatusCode;
    use ntex::web::{self, test};
    use sha2::{Digest, Sha256};
    use zeroship_bundle::{BlobStore, LocalDiskBlobStore, Manifest, RuntimeDescriptorEntry};
    use zeroship_core::net_policy::Verdict;
    use zeroship_core::types::{AppNetPolicy, AppRuntimeLimits, NetEgressEntry};

    use super::*;

    #[test]
    fn control_timeout_defaults_to_five_seconds() {
        assert_eq!(CONTROL_REQUEST_TIMEOUT, std::time::Duration::from_secs(5));
        assert_eq!(control_timeout_error(), "control request timed out after 5s");
    }

    // -----------------------------------------------------------------------
    // needs_reload — the PHASE 2 isolate-swap decision
    // -----------------------------------------------------------------------

    fn version_info(deploy_hash: Option<&str>, env_version: i64, runtime: AppRuntimeLimits) -> AppVersionInfo {
        AppVersionInfo {
            deploy_hash: deploy_hash.map(str::to_string),
            plan_id: "starter".to_string(),
            runtime,
            env_version,
            manifest: None,
            net_policy: AppNetPolicy::default(),
        }
    }

    fn loaded_meta(deploy_hash: Option<&str>, env_version: i64) -> cache::LoadedMeta {
        cache::LoadedMeta {
            deploy_hash: deploy_hash.map(str::to_string),
            env_version,
            net_policy: AppNetPolicy::default(),
        }
    }

    fn matching_limits(runtime: &AppRuntimeLimits) -> RuntimeLimits {
        cache::runtime_limits_from_app(runtime)
    }

    #[test]
    fn needs_reload_false_when_state_matches() {
        let info = version_info(Some("h1"), 7, AppRuntimeLimits::default());
        let loaded = loaded_meta(Some("h1"), 7);
        assert!(
            !needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
            "no hash / limits / env change must NOT reload (reload churn would \
             drop module state + in-flight work for nothing)"
        );
    }

    /// SEC-7 regression: a pure env-version bump — deploy hash unchanged,
    /// limits unchanged — MUST trigger an isolate reload. The V8 side
    /// materializes the `env` argument object and `process.env` once per
    /// isolate, so without a reload a rotated/removed credential keeps
    /// being served until LRU eviction or the next code deploy, defeating
    /// revocation. Pre-fix this fails: the decision only consulted the
    /// deploy hash and the limits.
    #[test]
    fn needs_reload_true_when_only_env_version_bumps() {
        let info = version_info(Some("h1"), 2, AppRuntimeLimits::default());
        let loaded = loaded_meta(Some("h1"), 1);
        assert!(
            needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
            "SEC-7: env-only version bump (hash + limits unchanged) must \
             reload the isolate so secret rotation actually applies"
        );
    }

    #[test]
    fn poll_interval_of_zero_is_rejected() {
        assert!(
            super::validate_poll_interval_secs(0).is_err(),
            "zero divides the reconcile jitter and would panic every reconcile \
             task while the server kept serving"
        );
        assert_eq!(super::validate_poll_interval_secs(1), Ok(1));
        assert_eq!(super::validate_poll_interval_secs(60), Ok(60));
    }

    /// A cached app whose deploy GOES AWAY must reload, which is how the stale
    /// code stops being served. The comparison only asked whether a remote hash
    /// differed from the loaded one, and `None` differs from nothing, so losing
    /// the deploy read as "unchanged". With limits, env version and net policy
    /// all steady, the isolate then kept serving an app that no longer has a
    /// live deploy, until LRU eviction happened to reclaim it.
    #[test]
    fn needs_reload_true_when_the_deploy_goes_away() {
        let info = version_info(None, 7, AppRuntimeLimits::default());
        let loaded = loaded_meta(Some("h1"), 7);
        assert!(
            needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
            "an app that lost its live deploy must reload rather than keep \
             serving the code it had cached"
        );
    }

    #[test]
    fn needs_reload_true_when_deploy_hash_changes() {
        let info = version_info(Some("h2"), 7, AppRuntimeLimits::default());
        let loaded = loaded_meta(Some("h1"), 7);
        assert!(needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info));
    }

    #[test]
    fn needs_reload_true_when_limits_change() {
        let info = version_info(
            Some("h1"),
            7,
            AppRuntimeLimits {
                cpu_limit_ms: Some(123),
                ..AppRuntimeLimits::default()
            },
        );
        let loaded = loaded_meta(Some("h1"), 7);
        assert!(needs_reload(
            Some(&loaded),
            Some(matching_limits(&AppRuntimeLimits::default())),
            &info
        ));
    }

    #[test]
    fn needs_reload_true_when_net_policy_changes() {
        let info = version_info(Some("h1"), 7, AppRuntimeLimits::default());
        let loaded = cache::LoadedMeta {
            deploy_hash: Some("h1".to_string()),
            env_version: 7,
            net_policy: AppNetPolicy {
                egress: vec![NetEgressEntry {
                    verdict: Verdict::Accept,
                    destination: "db.example.com".to_string(),
                    port: 5432,
                }],
                max_sockets: 4,
                egress_ceiling_bytes: 1024 * 1024,
            },
        };
        assert!(
            needs_reload(Some(&loaded), Some(matching_limits(&info.runtime)), &info),
            "revoking the last net grant changes AppVersionInfo.net_policy to \
             default-deny and must rebuild the isolate on the next reconcile tick"
        );
    }

    /// Isolate cached but no per-thread record of what it was loaded
    /// against (both load paths record one, so this is defensive):
    /// unknown state → reload, never "assume current".
    #[test]
    fn needs_reload_true_when_isolate_meta_missing() {
        let info = version_info(Some("h1"), 0, AppRuntimeLimits::default());
        assert!(needs_reload(None, Some(matching_limits(&info.runtime)), &info));
    }

    // -----------------------------------------------------------------------
    // SEC-7 faithful end-to-end: env-only rotation reaches a live isolate
    // -----------------------------------------------------------------------

    fn tmpdir(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "zs-worker-sync-{label}-{}",
            Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).expect("mkdir tmp");
        path
    }

    #[test]
    fn runtime_descriptor_absent_is_schema_less() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let root = tmpdir("descriptor-schema-less");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(root.clone()).expect("blob store"));
            let manifest = executable_manifest(&blob_store).await;
            let executable = load_executable(&manifest, &blob_store)
                .await
                .expect("schema-less manifest must resolve cleanly");
            assert_eq!(executable.descriptor, None);
            std::fs::remove_dir_all(root).ok();
        });
    }

    #[test]
    fn runtime_descriptor_blob_fetch_failure_is_load_error() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        runtime.block_on(async {
            let root = tmpdir("descriptor-missing-blob");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(root.clone()).expect("blob store"));
            let descriptor_hash = "c".repeat(64);
            let manifest = Manifest {
                runtime_descriptor: Some(RuntimeDescriptorEntry {
                    hash: descriptor_hash.clone(),
                }),
                ..executable_manifest(&blob_store).await
            };

            let Err(err) = load_executable(&manifest, &blob_store).await else {
                panic!("missing descriptor blob must fail load");
            };
            assert!(
                err.contains("app executable storage"),
                "error should report the missing descriptor, got: {err}"
            );
            std::fs::remove_dir_all(root).ok();
        });
    }

    async fn executable_manifest(store: &Arc<dyn BlobStore>) -> Manifest {
        let source = b"export default { fetch() { return new Response('ok'); } };";
        let hash = zeroship_bundle::sha256_hex(source);
        store.put_blob(&hash, source).await.unwrap();
        Manifest {
            worker: Some(zeroship_bundle::WorkerCode {
                entry: "index.js".into(),
                modules: [("index.js".into(), hash)].into(),
            }),
            ..Manifest::default()
        }
    }

    fn dispatch_req(app_id: &Uuid) -> ntex::http::Request {
        let frame = zeroship_core::dispatch_frame::encode_dispatch_frame(
            "GET",
            "http://example.test/env-probe",
            &[],
            b"",
        )
        .expect("dispatch frame");
        test::TestRequest::post()
            // The route takes the app id in its printed, typed form; this test
            // holds the uuid the version feed serves, so it renders it the way
            // the gateway does rather than spelling the uuid into the URL.
            .uri(&format!(
                "/dispatch/{}",
                zeroship_core::app_id::canonical_app_id_for(app_id).as_str()
            ))
            .header(
                "authorization",
                crate::handler::tests::gateway_authorization(),
            )
            .set_payload(frame)
            .to_request()
    }

    /// SEC-7 regression, full pipeline: an env-var/secret rotation that
    /// bumps ONLY the env version (no code redeploy, no limits change)
    /// must swap the running isolate so the rotated values actually reach
    /// JS. Pre-fix, `reconcile_once` refreshed `SharedEnvs` (PHASE 1) but
    /// never reloaded the isolate (the PHASE 2 swap decision ignored the
    /// env version), so the already-materialized `env` argument object and
    /// `process.env` kept serving the revoked credential indefinitely.
    ///
    /// Drives the REAL path: `cache::load_app` → ntex `/dispatch/{app_id}`
    /// → V8 env materialization → `reconcile_once` (the production
    /// reconcile decision, real `LocalDiskBlobStore`) → `/dispatch` again.
    /// The only seeded step is the `SharedEnvs` refresh itself
    /// (`put_env_from_json` at the bumped version) — byte-for-byte what
    /// PHASE 1 does after its control-plane fetch, minus the HTTP round
    /// trip (the control URL points at a dead port, so any unexpected
    /// HTTP dependence fails loudly instead of silently passing).
    #[test]
    fn reconcile_swaps_isolate_on_env_only_rotation() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");

        runtime.block_on(async {
            // Internally idempotent (guarded by a Once in the runtime crate),
            // so safe alongside handler.rs tests in the same process.
            zeroship_runtime::init::init_v8();

            let app_id = Uuid::new_v4();
            // Reads one var and one secret off the materialized `env`
            // argument, plus the var again via `process.env` — the two
            // V8 surfaces SEC-7 is about.
            let source: &[u8] = br#"
                export default {
                  fetch(req, env) {
                    return new Response(
                      [env.API_TOKEN, env.SIGNING_SECRET, process.env.API_TOKEN].join("|")
                    );
                  }
                }
            "#;

            crate::cache::init_cache(
                10,
                4,
                crate::cache::KernelConfig {
                    control_url: "http://127.0.0.1:1".to_string(),
                    control_key: String::new(),
                    db_service: None,
                    kv_store: None,
                    storage_backend: None,
                    meter: std::sync::Arc::new(zeroship_metering::Meter::new()),
                },
            );

            // Seed the blob store with the (unchanged) worker bundle, keyed
            // by its real sha256 — `get_blob` verifies content hashes.
            let blob_root = tmpdir("blob");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
            let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
                zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
                    .expect("workflow blob store"),
            );
            let blob_hash = hex::encode(Sha256::digest(source));
            blob_store.put_blob(&blob_hash, source).await.expect("seed blob");

            let envs: SharedEnvs = Arc::new(RwLock::new(HashMap::new()));
            put_env_from_json(
                &envs,
                app_id,
                r#"{"vars":{"API_TOKEN":"var-old"},"secrets":{"SIGNING_SECRET":"sec-old"},"expose":[]}"#,
                1,
            )
            .expect("insert env v1");
            let env_v1 = get_env(&envs, &app_id).expect("env v1 cached");

            // Load the app the way the worker does, recording that the
            // isolate was hydrated against env version 1.
            crate::cache::load_app(
                app_id,
                crate::cache::test_modules(source),
                AppRuntimeLimits::default(),
                AppNetPolicy::default(),
                None,
                None,
                &zeroship_bundle::Manifest::default(),
                &env_v1.snapshot,
            )
            .expect("app loads");
            crate::cache::set_loaded_meta(
                app_id,
                crate::cache::LoadedMeta {
                    deploy_hash: Some("deploy-h1".to_string()),
                    env_version: 1,
                    net_policy: AppNetPolicy::default(),
                },
            );

            let logs = crate::logs::new_store();
            let config = Arc::new(crate::WorkerConfig {
                service_auth: crate::handler::tests::test_service_auth(),
                // Dead port: this scenario must not need the control plane
                // (SharedEnvs is already current when PHASE 2 swaps).
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_url: None,
                kv_store: None,
                storage_backend: None,
                max_isolates: 10,
                max_pinned_isolates_per_app: 4,
                poll_interval_secs: 60,
                shutdown_timeout_secs: 0,
                blob_store,
                workflow_blob_store,
                max_step_blob_bytes: 64 * 1024 * 1024,
                workflow_advance_unsigned: false,
            });

            let app = test::init_service(
                web::App::new()
                    .state(config.clone())
                    .state(envs.clone())
                    .state(logs)
                    .service(
                        web::resource("/dispatch/{app_id}")
                            .route(web::post().to(crate::handler::dispatch)),
                    ),
            )
            .await;

            // 1. First dispatch materializes env v1 inside the isolate.
            let resp = test::call_service(&app, dispatch_req(&app_id)).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_eq!(&body[..], b"var-old|sec-old|var-old", "isolate boots on env v1");

            // 2. Rotation: control bumps env_version to 2; some thread's
            //    PHASE 1 refreshes the process-wide SharedEnvs. This is
            //    exactly `put_env_from_json` with the new payload+version.
            put_env_from_json(
                &envs,
                app_id,
                r#"{"vars":{"API_TOKEN":"var-new"},"secrets":{"SIGNING_SECRET":"sec-new"},"expose":[]}"#,
                2,
            )
            .expect("insert env v2");

            // 3. The live isolate does NOT see the refresh — `env` and
            //    `process.env` were materialized once. This pins the
            //    mechanism that makes the reload necessary.
            let resp = test::call_service(&app, dispatch_req(&app_id)).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_eq!(
                &body[..],
                b"var-old|sec-old|var-old",
                "pre-reconcile, an already-materialized isolate still serves the old env \
                 (this is why the reconcile swap must fire)"
            );

            // 4. Reconcile against a version map where ONLY env_version
            //    changed: same deploy hash, same limits.
            let manifest: Manifest = serde_json::from_value(serde_json::json!({
                "version": 1,
                "worker": { "entry": "index.js", "modules": { "index.js": blob_hash } },
            }))
            .expect("manifest");
            let mut versions: VersionMap = HashMap::new();
            versions.insert(
                app_id,
                AppVersionInfo {
                    deploy_hash: Some("deploy-h1".to_string()),
                    plan_id: "starter".to_string(),
                    runtime: AppRuntimeLimits::default(),
                    env_version: 2,
                    manifest: Some(manifest),
                    net_policy: AppNetPolicy::default(),
                },
            );
            reconcile_once(&config, &versions, &envs).await.expect("reconcile");

            // 5. The rotated credentials must now reach JS. Pre-fix this
            //    fails with the old values: reconcile never swapped the
            //    isolate on an env-only bump.
            let resp = test::call_service(&app, dispatch_req(&app_id)).await;
            assert_eq!(resp.status(), StatusCode::OK);
            let body = test::read_body(resp).await;
            assert_eq!(
                &body[..],
                b"var-new|sec-new|var-new",
                "SEC-7: env-only rotation must reach the running isolate after reconcile"
            );
            assert_eq!(
                crate::cache::get_loaded_meta(&app_id).map(|m| m.env_version),
                Some(2),
                "reload must record the env version the new isolate was hydrated against"
            );

            let _ = std::fs::remove_dir_all(blob_root);
        });
    }
}
