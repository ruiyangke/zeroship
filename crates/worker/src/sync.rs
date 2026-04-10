use std::collections::HashMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use uuid::Uuid;

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
    // Fetch version map from control plane
    let url = format!("{}/internal/versions", config.control_url);

    let response =
        http_get(&url, &config.control_key).await.map_err(|e| format!("fetch versions: {e}"))?;

    let versions: HashMap<Uuid, Option<String>> =
        serde_json::from_str(&response).map_err(|e| format!("parse versions: {e}"))?;

    // Compare with local state
    for (app_id, remote_hash) in &versions {
        let local_hash = cache::get_hash(app_id);
        let needs_update = match (&local_hash, remote_hash) {
            (None, Some(_)) => true,              // New app
            (Some(l), Some(r)) if l != r => true, // Updated
            _ => false,
        };

        if needs_update {
            if let Some(hash) = remote_hash {
                let bundle_url = format!("{}/internal/bundles/{}", config.control_url, app_id);
                match http_get_bytes(&bundle_url, &config.control_key).await {
                    Ok(bytes) => {
                        // Verify hash
                        let computed = hex::encode(Sha256::digest(&bytes));
                        if computed != *hash {
                            eprintln!(
                                "[worker-sync] hash mismatch for {app_id}: expected {hash}, got {computed}"
                            );
                            continue;
                        }
                        if cache::load_app(*app_id, &bytes) {
                            cache::set_hash(*app_id, hash.clone());
                            eprintln!(
                                "[worker-sync] loaded {app_id} (hash: {}...)",
                                &hash[..hash.len().min(8)]
                            );
                        }
                    }
                    Err(e) => eprintln!("[worker-sync] fetch bundle {app_id}: {e}"),
                }
            }
        }
    }

    // Evict apps that are no longer in the version map
    let local_app_ids = cache::all_app_ids();
    for local_id in &local_app_ids {
        if !versions.contains_key(local_id) {
            eprintln!("[worker-sync] evicting deleted app {local_id}");
            cache::evict_app(local_id);
            cache::remove_hash(local_id);
        }
    }

    Ok(())
}

/// Simple HTTP GET returning response body as string.
async fn http_get(url: &str, auth_key: &str) -> Result<String, String> {
    let bytes = http_get_bytes(url, auth_key).await?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// Simple HTTP GET returning response body as bytes.
/// Uses a raw TCP connection for minimal deps.
async fn http_get_bytes(url: &str, auth_key: &str) -> Result<Vec<u8>, String> {
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

    // Read all response bytes
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

    // Find body after \r\n\r\n
    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no HTTP header end")?;

    // Check status
    let header = std::str::from_utf8(&response[..header_end]).map_err(|e| e.to_string())?;
    if !header.starts_with("HTTP/1.1 200") && !header.starts_with("HTTP/1.0 200") {
        let status_line = header.lines().next().unwrap_or("unknown");
        return Err(format!("HTTP error: {status_line}"));
    }

    Ok(response[header_end + 4..].to_vec())
}
