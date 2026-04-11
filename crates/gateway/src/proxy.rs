//! HTTP proxy with Consistent Hashing and Bounded Loads (CHWBL).
//!
//! Requests for the same app always go to the same "home" worker (cache locality).
//! When a worker exceeds 125% of average load, overflow spills to the next
//! worker on the hash ring (hotspot protection).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;
use ntex::web::HttpResponse;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// CHWBL Hash Ring
// ---------------------------------------------------------------------------

const VNODES_PER_WORKER: usize = 150;

/// Consistent hash ring with bounded loads.
pub struct HashRing {
    /// Sorted ring: hash position → worker index
    ring: BTreeMap<u64, usize>,
    /// Worker URLs
    workers: Vec<String>,
    /// Active request count per worker
    active: Vec<AtomicU32>,
    /// Max requests per worker: ceil(total_active / num_workers * (1 + epsilon))
    /// We use a simpler static bound for v1.
    max_per_worker: u32,
}

impl HashRing {
    pub fn new(worker_urls: Vec<String>, max_per_worker: u32) -> Self {
        let mut ring = BTreeMap::new();
        for (idx, url) in worker_urls.iter().enumerate() {
            for i in 0..VNODES_PER_WORKER {
                let key = format!("{url}-vnode-{i}");
                let hash = hash_bytes(key.as_bytes());
                ring.insert(hash, idx);
            }
        }

        let active = (0..worker_urls.len())
            .map(|_| AtomicU32::new(0))
            .collect();

        Self {
            ring,
            workers: worker_urls,
            active,
            max_per_worker,
        }
    }

    /// Select a worker for the given app_id using CHWBL.
    /// Returns (worker_index, worker_url).
    pub fn select(&self, app_id: &Uuid) -> (usize, &str) {
        let hash = hash_bytes(app_id.as_bytes());

        // Walk ring clockwise from the hash position
        let candidates = self
            .ring
            .range(hash..)
            .chain(self.ring.iter()); // wrap around

        for (_, &worker_idx) in candidates {
            let current = self.active[worker_idx].load(Ordering::Relaxed);
            if current < self.max_per_worker {
                return (worker_idx, &self.workers[worker_idx]);
            }
        }

        // All workers at capacity — fallback to least loaded
        let least = self
            .active
            .iter()
            .enumerate()
            .min_by_key(|(_, a)| a.load(Ordering::Relaxed))
            .map(|(idx, _)| idx)
            .unwrap_or(0);

        (least, &self.workers[least])
    }

    /// Increment active count for a worker. Call before forwarding.
    pub fn acquire(&self, worker_idx: usize) {
        self.active[worker_idx].fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement active count for a worker. Call after response received.
    pub fn release(&self, worker_idx: usize) {
        self.active[worker_idx].fetch_sub(1, Ordering::Release);
    }

    pub fn num_workers(&self) -> usize {
        self.workers.len()
    }
}

impl std::fmt::Debug for HashRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashRing")
            .field("workers", &self.workers.len())
            .field("vnodes", &self.ring.len())
            .field("max_per_worker", &self.max_per_worker)
            .finish()
    }
}

/// SHA-256 truncated to u64 — excellent distribution for consistent hashing.
/// FNV-1a clusters badly with sequential inputs (worker IPs, app UUIDs).
fn hash_bytes(data: &[u8]) -> u64 {
    use sha2::{Sha256, Digest};
    let hash = Sha256::digest(data);
    u64::from_le_bytes(hash[..8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// HTTP proxy
// ---------------------------------------------------------------------------

pub async fn forward(
    ring: &HashRing,
    app_id: &Uuid,
    plan_id: &str,
    request_id: &Uuid,
    body: &[u8],
) -> Result<HttpResponse, String> {
    if ring.num_workers() == 0 {
        return Err("no workers configured".into());
    }

    // CHWBL: select worker with bounded load
    let (worker_idx, worker_url) = ring.select(app_id);
    ring.acquire(worker_idx);

    let result = forward_to_worker(worker_url, app_id, plan_id, request_id, body).await;

    ring.release(worker_idx);
    result
}

async fn forward_to_worker(
    worker_url: &str,
    app_id: &Uuid,
    plan_id: &str,
    request_id: &Uuid,
    body: &[u8],
) -> Result<HttpResponse, String> {
    let parsed = url::Url::parse(&format!("{worker_url}/dispatch/{app_id}"))
        .map_err(|e| e.to_string())?;
    let host = parsed.host_str().ok_or("no host")?;
    let port = parsed.port().unwrap_or(80);
    let path = parsed.path();

    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;

    let header = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         X-App-Id: {app_id}\r\n\
         X-Plan-Id: {plan_id}\r\n\
         X-Request-Id: {request_id}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );

    let mut request_bytes = header.into_bytes();
    request_bytes.extend_from_slice(body);

    let BufResult(r, _) = stream.write_all(request_bytes).await;
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
    let body_bytes = &response[header_end + 4..];

    let status = header
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(502);

    let mut builder = HttpResponse::build(
        ntex::http::StatusCode::from_u16(status)
            .unwrap_or(ntex::http::StatusCode::BAD_GATEWAY),
    );
    builder.content_type("application/json");

    for line in header.lines().skip(1) {
        if let Some((name, value)) = line.split_once(": ") {
            let lname = name.to_ascii_lowercase();
            if lname == "x-cpu-time-ms" || lname == "x-wall-time-ms" {
                builder.set_header(name, value.to_string());
            }
        }
    }

    Ok(builder.body(body_bytes.to_vec()))
}
