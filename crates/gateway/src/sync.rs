use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;
use uuid::Uuid;

use zeroship_core::types::{RouteEntry, RouteMap};

use crate::GateState;

impl std::fmt::Debug for RouteCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteCache").finish_non_exhaustive()
    }
}

pub struct RouteCache {
    routes: RwLock<RouteMap>,
    name_index: RwLock<HashMap<String, Uuid>>,
}

impl RouteCache {
    pub fn new() -> Self {
        Self {
            routes: RwLock::new(HashMap::new()),
            name_index: RwLock::new(HashMap::new()),
        }
    }

    pub fn update(&self, new_routes: RouteMap) {
        let mut name_idx = HashMap::new();
        for (id, entry) in &new_routes {
            name_idx.insert(entry.name.clone(), *id);
        }
        *self.routes.write().unwrap() = new_routes;
        *self.name_index.write().unwrap() = name_idx;
    }

    pub fn lookup_by_name(&self, name: &str) -> Option<(Uuid, RouteEntry)> {
        let name_idx = self.name_index.read().unwrap();
        let app_id = name_idx.get(name)?;
        let routes = self.routes.read().unwrap();
        routes.get(app_id).map(|e| (*app_id, e.clone()))
    }
}

pub fn start_sync(state: Arc<GateState>) {
    compio::runtime::spawn(async move {
        let interval = std::time::Duration::from_secs(state.config.poll_interval_secs);
        loop {
            compio::time::sleep(interval).await;
            if let Err(e) = sync_once(&state).await {
                eprintln!("[gate-sync] error: {e}");
            }
        }
    })
    .detach();
}

async fn sync_once(state: &GateState) -> Result<(), String> {
    let url = format!("{}/internal/routes", state.config.control_url);
    let response = http_get(&url, &state.config.control_key).await?;
    let routes: RouteMap =
        serde_json::from_str(&response).map_err(|e| format!("parse routes: {e}"))?;
    state.routes.update(routes);
    Ok(())
}

async fn http_get(url: &str, auth_key: &str) -> Result<String, String> {
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
        .ok_or("no header end")?;

    let header = std::str::from_utf8(&response[..header_end]).map_err(|e| e.to_string())?;
    if !header.starts_with("HTTP/1.1 200") && !header.starts_with("HTTP/1.0 200") {
        return Err(format!(
            "HTTP error: {}",
            header.lines().next().unwrap_or("?")
        ));
    }

    String::from_utf8(response[header_end + 4..].to_vec()).map_err(|e| e.to_string())
}
