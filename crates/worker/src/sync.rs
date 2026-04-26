use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_core::types::{AppVersionInfo, VersionMap};
use zeroship_runtime::{EnvSnapshot, RuntimeLimits};

use crate::{cache, WorkerConfig};

/// Process-wide snapshot of `/internal/versions`, refreshed by a single
/// background poller regardless of how many ntex worker threads are running.
///
/// This replaces the previous "one HTTP poll per thread" behaviour that
/// multiplied control-plane traffic by `workers_count`.
pub type SharedVersions = Arc<RwLock<Option<VersionMap>>>;

/// Process-wide env cache. Replaces the per-thread `ENVS` thread_local
/// (~16x memory overhead at default worker count). Single source of
/// truth — secret rotation invalidates ALL threads at once via one
/// `write()` lock.
///
/// `Arc<EnvSnapshot>` lets the handler clone an Arc (cheap) under the
/// read lock and use it after dropping the lock — so the hot path
/// never holds the lock across `await`.
pub type SharedEnvs = Arc<RwLock<HashMap<Uuid, Arc<EnvSnapshot>>>>;

/// Start the single process-wide version-polling task. All ntex worker
/// threads observe its output through `shared`.
pub fn start_version_poller(config: Arc<WorkerConfig>, shared: SharedVersions) {
    compio::runtime::spawn(async move {
        version_poll_loop(config, shared).await;
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

async fn version_poll_loop(config: Arc<WorkerConfig>, shared: SharedVersions) {
    let interval = std::time::Duration::from_secs(config.poll_interval_secs);
    loop {
        match poll_versions(&config).await {
            Ok(versions) => {
                // RwLock write is brief — just swap the map in.
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
    loop {
        compio::time::sleep(interval).await;
        // Snapshot the shared map under a brief read lock, then drop the lock
        // before doing any async work (no .await while holding a std RwLock).
        let snapshot: Option<VersionMap> = shared.read().ok().and_then(|g| g.clone());
        let Some(versions) = snapshot else { continue };
        if let Err(e) = reconcile_once(&config, &versions, &envs).await {
            eprintln!("[worker-sync] reconcile error: {e}");
        }
    }
}

async fn reconcile_once(config: &WorkerConfig, versions: &VersionMap, envs: &SharedEnvs) -> Result<(), String> {
    // Only update apps that are ALREADY cached (not new ones — those load on-demand).
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

                // Env-only refresh: bundle didn't change but env_version did
                // (creator rotated a secret). Skip the heavy bundle refetch
                // — just re-pull /internal/apps/:id/env.
                let local_env_version = cache::get_env_version(local_id);
                let env_changed = local_env_version != Some(info.env_version);
                if !needs_update && env_changed {
                    match fetch_app_env(&config.control_url, &config.control_key, local_id).await {
                        Ok(env_json) => {
                            if let Err(e) = put_env_from_json(envs, *local_id, &env_json) {
                                eprintln!("[worker-sync] env parse {local_id}: {e}");
                            } else {
                                cache::set_env_version(*local_id, info.env_version);
                                eprintln!(
                                    "[worker-sync] env refreshed for {local_id} (v{} → v{})",
                                    local_env_version.unwrap_or(0), info.env_version,
                                );
                            }
                        }
                        Err(e) => eprintln!("[worker-sync] env-only refresh {local_id}: {e}"),
                    }
                }

                if needs_update {
                    let bundle_url =
                        format!("{}/internal/bundles/{}", config.control_url, local_id);
                    match http_get_bytes(&bundle_url, &config.control_key).await {
                        Ok(bytes) => {
                            let computed = hex::encode(Sha256::digest(&bytes));
                            if remote_hash.as_ref().is_some_and(|remote_hash| computed != *remote_hash) {
                                eprintln!(
                                    "[worker-sync] hash mismatch for {local_id}: expected {}, got {computed}",
                                    remote_hash.as_deref().unwrap_or("")
                                );
                                continue;
                            }
                            if cache::load_app(*local_id, &bytes, info.runtime.clone()) {
                                if let Some(remote_hash) = remote_hash {
                                    cache::set_hash(*local_id, remote_hash.clone());
                                }
                                // Fetch the app's env snapshot (vars + decrypted
                                // secrets) alongside the bundle — keeps the
                                // shared env fresh on every version bump.
                                match fetch_app_env(&config.control_url, &config.control_key, local_id).await {
                                    Ok(env_json) => {
                                        if let Err(e) = put_env_from_json(envs, *local_id, &env_json) {
                                            eprintln!("[worker-sync] env parse {local_id}: {e}");
                                        } else {
                                            cache::set_env_version(*local_id, info.env_version);
                                        }
                                    }
                                    Err(e) => eprintln!("[worker-sync] fetch env {local_id}: {e}"),
                                }
                                eprintln!(
                                    "[worker-sync] updated {local_id} (plan: {}, hash: {}...)",
                                    info.plan_id,
                                    &computed[..computed.len().min(8)]
                                );
                            }
                        }
                        Err(e) => eprintln!("[worker-sync] fetch bundle {local_id}: {e}"),
                    }
                }
            }
            // App deleted from control plane — evict
            None => {
                eprintln!("[worker-sync] evicting deleted app {local_id}");
                cache::evict_app(local_id);
                cache::remove_hash(local_id);
                cache::remove_env_version(local_id);
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

/// Parse + insert an env JSON into the shared cache. Failure modes are
/// surfaced — caller decides whether to fail-loud or fall through.
pub fn put_env_from_json(envs: &SharedEnvs, app_id: Uuid, env_json: &str) -> Result<(), String> {
    let parsed: serde_json::Value = serde_json::from_str(env_json)
        .map_err(|e| format!("env json: {e}"))?;
    let snapshot = Arc::new(EnvSnapshot::new(parsed));
    envs.write()
        .map_err(|e| format!("envs lock poisoned: {e}"))?
        .insert(app_id, snapshot);
    Ok(())
}

/// Read the cached env for an app. Cheap — clones an Arc under a brief
/// read lock; never holds the lock across `await`.
pub fn get_env(envs: &SharedEnvs, app_id: &Uuid) -> Option<Arc<EnvSnapshot>> {
    envs.read().ok()?.get(app_id).cloned()
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

/// Shared cyper client for control-plane calls. Unlike the fetch client, this
/// one talks only to the operator-configured `CONTROL_URL`, so we use cyper's
/// default resolver (no SSRF guard — the operator trusts this endpoint by
/// construction).
fn control_client() -> &'static cyper::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<cyper::Client> = OnceLock::new();
    CLIENT.get_or_init(cyper::Client::new)
}

/// HTTP GET returning the response body as bytes. Uses a proper HTTP client
/// (cyper) so the caller gets correct query-string handling, TLS support,
/// chunked/compressed decoding, keep-alive, and status-code parsing —
/// instead of a hand-rolled TCP GET that assumed port 80, dropped the query,
/// and split the body on `\r\n\r\n`.
///
/// Public so handler.rs can use it for on-demand bundle loading.
pub async fn http_get_bytes(url: &str, auth_key: &str) -> Result<Vec<u8>, String> {
    let client = control_client();
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
