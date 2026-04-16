use std::sync::Arc;

use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_core::types::{AppVersionInfo, VersionMap};
use zeroship_runtime::RuntimeLimits;

use crate::{cache, WorkerConfig};

/// Start the background sync loop on the current thread.
pub fn start_sync(config: Arc<WorkerConfig>) {
    compio::runtime::spawn(async move {
        sync_loop(config).await;
    })
    .detach();
}

async fn sync_loop(config: Arc<WorkerConfig>) {
    let interval = std::time::Duration::from_secs(config.poll_interval_secs);
    loop {
        compio::time::sleep(interval).await;
        if let Err(e) = sync_once(&config).await {
            eprintln!("[worker-sync] error: {e}");
        }
    }
}

async fn sync_once(config: &WorkerConfig) -> Result<(), String> {
    let url = format!("{}/internal/versions", config.control_url);

    let response =
        http_get(&url, &config.control_key).await.map_err(|e| format!("fetch versions: {e}"))?;

    let versions: VersionMap =
        serde_json::from_str(&response).map_err(|e| format!("parse versions: {e}"))?;

    // Only update apps that are ALREADY cached (not new ones — those load on-demand).
    let local_app_ids = cache::all_app_ids();
    for local_id in &local_app_ids {
        match versions.get(local_id) {
            // App still exists — check if deploy hash OR limits changed.
            Some(info) => {
                let remote_hash = &info.deploy_hash;
                let local_hash = cache::get_hash(local_id);
                let local_limits = cache::get_limits(local_id);
                let target_limits = RuntimeLimits {
                    cpu_limit: info.runtime.cpu_limit_ms.map(std::time::Duration::from_millis),
                    wall_timeout: info.runtime.wall_timeout_ms.map(std::time::Duration::from_millis),
                };
                let needs_update = match &local_hash {
                    Some(lh) => remote_hash.as_ref().is_some_and(|rh| lh != rh),
                    None => remote_hash.is_some(),
                } || local_limits != Some(target_limits);

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
            }
        }
    }

    Ok(())
}

pub async fn fetch_app_version(url_base: &str, auth_key: &str, app_id: &Uuid) -> Result<AppVersionInfo, String> {
    let url = format!("{url_base}/internal/apps/{app_id}");
    let body = http_get(&url, auth_key).await?;
    serde_json::from_str(&body).map_err(|e| e.to_string())
}

/// Simple HTTP GET returning response body as string.
async fn http_get(url: &str, auth_key: &str) -> Result<String, String> {
    let bytes = http_get_bytes(url, auth_key).await?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// Simple HTTP GET returning response body as bytes.
/// Public so handler.rs can use it for on-demand bundle loading.
pub async fn http_get_bytes(url: &str, auth_key: &str) -> Result<Vec<u8>, String> {
    use compio::buf::BufResult;
    use compio::io::{AsyncRead, AsyncWriteExt};
    use compio::net::TcpStream;

    let parsed = url::Url::parse(url).map_err(|e| e.to_string())?;
    let host = parsed.host_str().ok_or("no host")?;
    let port = parsed.port().unwrap_or(80);
    let path = parsed.path();

    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {auth_key}\r\nConnection: close\r\n\r\n"
    );

    let BufResult(r, _) = stream.write_all(request.into_bytes()).await;
    r.map_err(|e| e.to_string())?;

    let mut response = Vec::new();
    loop {
        let buf = vec![0u8; 8192];
        let BufResult(r, returned) = stream.read(buf).await;
        let n = r.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&returned[..n]);
    }

    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no HTTP header end")?;

    let header = std::str::from_utf8(&response[..header_end]).map_err(|e| e.to_string())?;
    if !header.starts_with("HTTP/1.1 200") && !header.starts_with("HTTP/1.0 200") {
        let status_line = header.lines().next().unwrap_or("unknown");
        return Err(format!("HTTP error: {status_line}"));
    }

    Ok(response[header_end + 4..].to_vec())
}
