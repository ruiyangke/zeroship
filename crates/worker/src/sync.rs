use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use uuid::Uuid;
use zeroship_bundle::{BlobStore, Manifest};
use zeroship_core::types::{AppVersionInfo, VersionMap};
use zeroship_runtime::{EnvSnapshot, RuntimeLimits};

use crate::{cache, WorkerConfig};

const CONTROL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Resolve the worker-entry blob hash from a manifest. Returns `None` for
/// SSG-only deploys (worker missing) and logs+returns `None` if the
/// manifest is malformed (entry not in modules) so the worker stays
/// loud rather than silently running stale code.
pub(crate) fn worker_entry_hash(manifest: &Manifest, app_id: &Uuid) -> Option<String> {
    let worker = manifest.worker.as_ref()?;
    match worker.modules.get(&worker.entry) {
        Some(h) => Some(h.clone()),
        None => {
            tracing::warn!(
                app_id = %app_id,
                entry = ?worker.entry,
                "worker-sync: manifest.worker.entry missing from modules"
            );
            None
        }
    }
}

/// **Migration-first cutover (P4b)** — resolve the bundled
/// `RuntimeSchemaDescriptor` JSON (`schema.runtime.json`) for an app from its
/// `manifest.runtime_descriptor` slot. The descriptor is a separate
/// content-addressed blob; we read it via `BlobStore` so the runtime can
/// expose it as `globalThis.__zsRuntimeDescriptor`.
///
/// Returns `Ok(None)` only when the manifest has no migrations and no
/// descriptor (schema-less app). A manifest with migrations but no descriptor,
/// a missing descriptor blob, or non-UTF-8 descriptor bytes is a hard load
/// error; silently booting schema-less would hide a broken build/deploy.
pub(crate) async fn runtime_descriptor_json(
    manifest: &Manifest,
    blob_store: &Arc<dyn BlobStore>,
    app_id: &Uuid,
) -> Result<Option<String>, String> {
    let Some(desc) = manifest.runtime_descriptor.as_ref() else {
        if manifest.migrations.is_empty() {
            return Ok(None);
        }
        return Err(format!(
            "manifest has {} migration(s) but no runtime_descriptor",
            manifest.migrations.len()
        ));
    };
    let hash = desc.hash.clone();
    match blob_store.get_blob(&hash).await {
        Ok(bytes) => match String::from_utf8(bytes.to_vec()) {
            Ok(s) => Ok(Some(s)),
            Err(e) => {
                tracing::error!(
                    app_id = %app_id,
                    descriptor_hash = %hash,
                    error = %e,
                    "worker-sync: runtime_descriptor blob is not UTF-8; refusing to load app"
                );
                Err(format!("runtime_descriptor blob {hash} is not UTF-8: {e}"))
            }
        },
        Err(e) => {
            tracing::error!(
                app_id = %app_id,
                descriptor_hash = %hash,
                error = %e,
                "worker-sync: runtime_descriptor blob fetch failed; refusing to load app"
            );
            Err(format!("runtime_descriptor blob {hash} fetch failed: {e}"))
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
            Err(e) => tracing::error!(error = %e, "worker-sync: poll error"),
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
pub fn needs_reload(
    loaded: Option<&cache::LoadedMeta>,
    local_limits: Option<RuntimeLimits>,
    info: &AppVersionInfo,
) -> bool {
    let hash_changed = match loaded.and_then(|m| m.deploy_hash.as_deref()) {
        Some(lh) => info.deploy_hash.as_deref().is_some_and(|rh| lh != rh),
        None => info.deploy_hash.is_some(),
    };
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
        match fetch_app_env(&config.control_url, &config.control_key, app_id).await {
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
                            cache::remove_loaded_meta(local_id);
                            continue;
                        }
                    };
                    match config.blob_store.get_blob(&bundle_hash).await {
                        Ok(bytes) => {
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
                                match fetch_app_env(&config.control_url, &config.control_key, local_id).await {
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

                            // Resolve the bundled RuntimeSchemaDescriptor (if
                            // any) so the runtime sources the schema from the
                            // migration fold. Absent → schema-less app.
                            let descriptor_json = match info.manifest.as_ref() {
                                Some(m) => match runtime_descriptor_json(m, &config.blob_store, local_id).await {
                                    Ok(json) => json,
                                    Err(e) => {
                                        tracing::warn!(app_id = %local_id, error = %e, "worker-sync: descriptor load failed");
                                        continue;
                                    }
                                },
                                None => None,
                            };
                            let Some(env_entry) = get_env(envs, local_id) else {
                                tracing::warn!(app_id = %local_id, "worker-sync: env cache missing before app load");
                                continue;
                            };
                            match cache::load_app(
                                *local_id,
                                &bytes,
                                info.runtime.clone(),
                                info.net_policy.clone(),
                                info.deploy_hash.as_deref(),
                                descriptor_json.as_deref(),
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
                                        blob_prefix = &bundle_hash[..bundle_hash.len().min(8)],
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
pub async fn fetch_app_version(url_base: &str, auth_key: &str, app_id: &Uuid) -> Result<AppVersionInfo, String> {
    let url = format!("{url_base}/internal/apps/{app_id}");
    let body = http_get(&url, auth_key).await?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// Fetch the merged env for an app in the split `{vars, secrets, expose}`
/// wire shape (see `crates/runtime/src/fetch_outcome.rs::EnvSnapshot`).
/// The result is a JSON object string passed through verbatim to
/// `EnvSnapshot::from_validated_json` at cache-insert time so the hot
/// path skips the parse-then-reserialize round-trip.
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
    compio::time::timeout(CONTROL_REQUEST_TIMEOUT, http_get_bytes_inner(url, auth_key))
        .await
        .map_err(|_| control_timeout_error())?
}

fn control_timeout_error() -> String {
    format!(
        "control request timed out after {}s",
        CONTROL_REQUEST_TIMEOUT.as_secs()
    )
}

async fn http_get_bytes_inner(url: &str, auth_key: &str) -> Result<Vec<u8>, String> {
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use ntex::http::StatusCode;
    use ntex::web::{self, test};
    use sha2::{Digest, Sha256};
    use zeroship_bundle::{
        BlobStore, LocalDiskBlobStore, Manifest, MigrationFileEntry, RuntimeDescriptorEntry,
    };
    use zeroship_core::types::{AppNetPolicy, AppRuntimeLimits, NetAllowEntry};

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
                allow: vec![NetAllowEntry {
                    host: "db.example.com".to_string(),
                    port: 5432,
                }],
                max_sockets: 4,
                egress_ceiling_bytes: 1024 * 1024,
                ..AppNetPolicy::default()
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

    fn one_migration() -> MigrationFileEntry {
        MigrationFileEntry {
            name: "V0001__create_notes.sql".to_string(),
            hash: "b".repeat(64),
        }
    }

    #[test]
    fn runtime_descriptor_absent_without_migrations_is_schema_less() {
        let Ok(runtime) = compio::runtime::Runtime::new() else {
            eprintln!("skipping (cannot create compio runtime)");
            return;
        };
        runtime.block_on(async {
            let root = tmpdir("descriptor-schema-less");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(root.clone()).expect("blob store"));
            let app_id = Uuid::new_v4();
            let manifest = Manifest::default();

            let descriptor = runtime_descriptor_json(&manifest, &blob_store, &app_id)
                .await
                .expect("schema-less manifest must resolve cleanly");
            assert_eq!(descriptor, None);
            std::fs::remove_dir_all(root).ok();
        });
    }

    #[test]
    fn runtime_descriptor_missing_for_migrations_is_load_error() {
        let Ok(runtime) = compio::runtime::Runtime::new() else {
            eprintln!("skipping (cannot create compio runtime)");
            return;
        };
        runtime.block_on(async {
            let root = tmpdir("descriptor-missing-slot");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(root.clone()).expect("blob store"));
            let app_id = Uuid::new_v4();
            let mut manifest = Manifest::default();
            manifest.migrations = vec![one_migration()];

            let err = runtime_descriptor_json(&manifest, &blob_store, &app_id)
                .await
                .expect_err("migrations without descriptor must fail load");
            assert!(
                err.contains("migration") && err.contains("runtime_descriptor"),
                "error should name missing descriptor for migrations, got: {err}"
            );
            std::fs::remove_dir_all(root).ok();
        });
    }

    #[test]
    fn runtime_descriptor_blob_fetch_failure_is_load_error() {
        let Ok(runtime) = compio::runtime::Runtime::new() else {
            eprintln!("skipping (cannot create compio runtime)");
            return;
        };
        runtime.block_on(async {
            let root = tmpdir("descriptor-missing-blob");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(root.clone()).expect("blob store"));
            let app_id = Uuid::new_v4();
            let descriptor_hash = "c".repeat(64);
            let mut manifest = Manifest::default();
            manifest.migrations = vec![one_migration()];
            manifest.runtime_descriptor = Some(RuntimeDescriptorEntry {
                hash: descriptor_hash.clone(),
            });

            let err = runtime_descriptor_json(&manifest, &blob_store, &app_id)
                .await
                .expect_err("missing descriptor blob must fail load");
            assert!(
                err.contains(&descriptor_hash) && err.contains("fetch failed"),
                "error should name missing descriptor blob, got: {err}"
            );
            std::fs::remove_dir_all(root).ok();
        });
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
            .uri(&format!("/dispatch/{app_id}"))
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
        let Ok(runtime) = compio::runtime::Runtime::new() else {
            eprintln!("skipping (cannot create compio runtime)");
            return;
        };

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
                crate::cache::KernelConfig {
                    db_url: None,
                    kv_url: None,
                    storage_backend: None,
                    meter: std::sync::Arc::new(zeroship_metering::Meter::new()),
                },
            );

            // Seed the blob store with the (unchanged) worker bundle, keyed
            // by its real sha256 — `get_blob` verifies content hashes.
            let blob_root = tmpdir("blob");
            let blob_store: Arc<dyn BlobStore> =
                Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
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
                source,
                AppRuntimeLimits::default(),
                AppNetPolicy::default(),
                None,
                None,
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
                // Dead port: this scenario must not need the control plane
                // (SharedEnvs is already current when PHASE 2 swaps).
                control_url: "http://127.0.0.1:1".to_string(),
                control_key: String::new(),
                db_url: None,
                kv_url: None,
                storage_backend: None,
                max_isolates: 10,
                poll_interval_secs: 60,
                worker_key: String::new(),
                shutdown_timeout_secs: 0,
                blob_store,
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
