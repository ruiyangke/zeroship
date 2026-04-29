use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use uuid::Uuid;
use zeroship_core::types::{AppVersionInfo, Manifest, VersionMap};
use zeroship_runtime::{EnvSnapshot, RuntimeLimits};

use crate::{cache, WorkerConfig};

/// Resolve the worker-entry blob hash from a manifest. Returns `None` for
/// SSG-only deploys (worker missing) and logs+returns `None` if the
/// manifest is malformed (entry not in modules) so the worker stays
/// loud rather than silently running stale code.
pub(crate) fn worker_entry_hash(manifest: &Manifest, app_id: &Uuid) -> Option<String> {
    let worker = manifest.worker.as_ref()?;
    match worker.modules.get(&worker.entry) {
        Some(h) => Some(h.clone()),
        None => {
            eprintln!(
                "[worker-sync] manifest.worker.entry {:?} missing from modules for {app_id}",
                worker.entry
            );
            None
        }
    }
}

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
) {
    compio::runtime::spawn(async move {
        version_poll_loop(config, shared, envs).await;
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
) {
    let interval = std::time::Duration::from_secs(config.poll_interval_secs);
    loop {
        match poll_versions(&config).await {
            Ok(versions) => {
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
            }
            Err(e) => eprintln!("[worker-sync] poll error: {e}"),
        }
        compio::time::sleep(interval).await;
    }
}

async fn poll_versions(config: &WorkerConfig) -> Result<VersionMap, String> {
    let url = format!("{}/internal/versions", config.control_url);
    let response =
        http_get(&url, &config.control_key).await.map_err(|e| format!("fetch versions: {e}"))?;
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
                eprintln!("[worker-sync] SharedVersions lock poisoned — restart recommended");
                continue;
            }
        };
        let Some(versions) = snapshot else { continue };
        if let Err(e) = reconcile_once(&config, &versions, &envs).await {
            eprintln!("[worker-sync] reconcile error: {e}");
        }
    }
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
        match fetch_app_env(&config.control_url, &config.control_key, app_id).await {
            Ok(env_json) => {
                if let Err(e) = put_env_from_json(envs, *app_id, &env_json, info.env_version) {
                    eprintln!("[worker-sync] env parse {app_id}: {e}");
                }
            }
            Err(e) => {
                crate::metrics::inc(&crate::metrics::ENV_FETCH_FAILURES);
                eprintln!("[worker-sync] env-only refresh {app_id}: {e}");
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
                let remote_hash = &info.deploy_hash;
                let local_hash = cache::get_hash(local_id);
                let local_limits = cache::get_limits(local_id);
                let target_limits = RuntimeLimits {
                    cpu_limit: info.runtime.cpu_limit_ms.map(std::time::Duration::from_millis),
                    wall_timeout: info.runtime.wall_timeout_ms.map(std::time::Duration::from_millis),
                    heap_limit_bytes: info.runtime.heap_limit_mb.map(|mb| (mb as usize) * 1024 * 1024),
                };
                let needs_update = match &local_hash {
                    Some(lh) => remote_hash.as_ref().is_some_and(|rh| lh != rh),
                    None => remote_hash.is_some(),
                } || local_limits != Some(target_limits);

                // Env-only refresh handled in PHASE 1 above (covers
                // apps not on this thread too). Local computation here
                // just decides whether the bundle needs swap.
                let cached_version = cached_env_version(envs, local_id);
                let env_changed = cached_version != Some(info.env_version);

                if needs_update {
                    // Resolve the worker-bundle blob hash from the manifest
                    // shipped in `info`. The platform's invariant is that
                    // a deployed app has `manifest.worker.modules[entry]` —
                    // anything else is either an undeployed app (manifest
                    // is None) or an SSG-only deploy (worker is None);
                    // neither needs a V8 isolate.
                    let bundle_hash = match info
                        .manifest
                        .as_ref()
                        .and_then(|m| worker_entry_hash(m, local_id))
                    {
                        Some(h) => h,
                        None => {
                            // No worker code → drop any cached isolate so
                            // the LRU slot is freed and on-demand load
                            // doesn't fall back to a stale runtime.
                            cache::evict_app(local_id);
                            cache::remove_hash(local_id);
                            continue;
                        }
                    };
                    match config.blob_store.get_blob(&bundle_hash).await {
                        Ok(bytes) => {
                            // Order: fetch+parse env BEFORE the V8 swap.
                            // Otherwise concurrent dispatches on the same
                            // thread between cache::load_app and
                            // put_env_from_json see new code with stale env
                            // (or 503 with no env at all). Skip the env
                            // fetch when env_changed is false — the
                            // existing SharedEnvs entry is still valid.
                            let env_for_load: Option<String> = if env_changed {
                                match fetch_app_env(&config.control_url, &config.control_key, local_id).await {
                                    Ok(json) => Some(json),
                                    Err(e) => {
                                        crate::metrics::inc(&crate::metrics::ENV_FETCH_FAILURES);
                                eprintln!("[worker-sync] fetch env {local_id}: {e}");
                                        continue; // don't swap V8 with no env
                                    }
                                }
                            } else {
                                None
                            };

                            if let Some(env_json) = env_for_load.as_deref() {
                                if let Err(e) = put_env_from_json(envs, *local_id, env_json, info.env_version) {
                                    eprintln!("[worker-sync] env parse {local_id}: {e}");
                                    continue;
                                }
                            }

                            if cache::load_app(*local_id, &bytes, info.runtime.clone()) {
                                if let Some(remote_hash) = remote_hash {
                                    cache::set_hash(*local_id, remote_hash.clone());
                                }
                                eprintln!(
                                    "[worker-sync] updated {local_id} (plan: {}, blob: {}...)",
                                    info.plan_id,
                                    &bundle_hash[..bundle_hash.len().min(8)]
                                );
                            }
                        }
                        Err(e) => {
                            crate::metrics::inc(&crate::metrics::BUNDLE_FETCH_FAILURES);
                            eprintln!("[worker-sync] fetch bundle {local_id}: {e}");
                        }
                    }
                }
            }
            // App deleted from control plane — evict
            None => {
                eprintln!("[worker-sync] evicting deleted app {local_id}");
                cache::evict_app(local_id);
                cache::remove_hash(local_id);
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
pub async fn fetch_app_version(url_base: &str, auth_key: &str, app_id: &Uuid) -> Result<AppVersionInfo, String> {
    let url = format!("{url_base}/internal/apps/{app_id}");
    let body = http_get(&url, auth_key).await?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// Fetch the merged env (vars + decrypted secrets) for an app. The
/// result is a JSON object string — JSON.parsed once into an
/// `EnvSnapshot` at cache-insert time so the hot path skips parsing.
///
/// Control returns 404 / 500 for non-existent apps or decrypt failures;
/// in both cases we propagate the error string so the caller can log it
/// and decide whether to evict the bundle or fail the request.
pub async fn fetch_app_env(url_base: &str, auth_key: &str, app_id: &Uuid) -> Result<String, String> {
    let url = format!("{url_base}/internal/apps/{app_id}/env");
    http_get(&url, auth_key).await
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
async fn http_get(url: &str, auth_key: &str) -> Result<String, String> {
    let bytes = http_get_bytes(url, auth_key).await?;
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
async fn http_get_bytes(url: &str, auth_key: &str) -> Result<Vec<u8>, String> {
    let client = this_thread_control_client();
    let mut builder = client
        .get(url)
        .map_err(|e| format!("invalid control URL: {e}"))?;
    if !auth_key.is_empty() {
        builder = builder
            .header("authorization", &format!("Bearer {auth_key}"))
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
