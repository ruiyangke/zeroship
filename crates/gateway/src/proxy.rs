//! HTTP proxy with Consistent Hashing and Bounded Loads (CHWBL).
//!
//! - xxHash64 for fast, well-distributed hashing (30x faster than SHA-256)
//! - Connection pool per worker (keep-alive, reuse TCP connections)
//! - Bounded load: overflow spills to next worker on ring

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;
use ntex::web::HttpResponse;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// CHWBL Hash Ring (uses XXH3 — fastest hash with excellent distribution)
// ---------------------------------------------------------------------------

/// XXH3 hash — ~2ns per call, uses SIMD when available.
#[inline]
fn hash_bytes(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}



const VNODES_PER_WORKER: usize = 150;

/// Consistent hash ring with bounded loads.
pub struct HashRing {
    ring: BTreeMap<u64, usize>,
    workers: Vec<String>,
    active: Vec<AtomicU32>,
    max_per_worker: u32,
}

impl HashRing {
    pub fn new(worker_urls: Vec<String>, max_per_worker: u32) -> Self {
        let mut ring = BTreeMap::new();
        for (idx, url) in worker_urls.iter().enumerate() {
            for i in 0..VNODES_PER_WORKER {
                let key = format!("{url}-vnode-{i}");
                ring.insert(hash_bytes(key.as_bytes()), idx);
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

    pub fn select(&self, app_id: &Uuid) -> (usize, &str) {
        let hash = hash_bytes(app_id.as_bytes());
        let candidates = self.ring.range(hash..).chain(self.ring.iter());

        for (_, &worker_idx) in candidates {
            let current = self.active[worker_idx].load(Ordering::Relaxed);
            if current < self.max_per_worker {
                return (worker_idx, &self.workers[worker_idx]);
            }
        }

        let least = self
            .active
            .iter()
            .enumerate()
            .min_by_key(|(_, a)| a.load(Ordering::Relaxed))
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        (least, &self.workers[least])
    }

    pub fn acquire(&self, worker_idx: usize) {
        self.active[worker_idx].fetch_add(1, Ordering::Relaxed);
    }

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
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Connection Pool (per-worker, thread-local)
// ---------------------------------------------------------------------------

struct ConnPool {
    /// Idle connections per worker URL
    pools: HashMap<String, VecDeque<TcpStream>>,
    max_idle: usize,
}

impl ConnPool {
    fn new(max_idle: usize) -> Self {
        Self {
            pools: HashMap::new(),
            max_idle,
        }
    }

    fn take(&mut self, addr: &str) -> Option<TcpStream> {
        self.pools.get_mut(addr)?.pop_front()
    }

    fn put(&mut self, addr: String, stream: TcpStream) {
        let pool = self.pools.entry(addr).or_insert_with(VecDeque::new);
        if pool.len() < self.max_idle {
            pool.push_back(stream);
        }
        // else: drop the connection (pool full)
    }
}

thread_local! {
    static CONN_POOL: RefCell<ConnPool> = RefCell::new(ConnPool::new(8));
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
    let host = parsed.host_str().ok_or("no host")?.to_string();
    let port = parsed.port().unwrap_or(80);
    let path = parsed.path().to_string();
    let addr = format!("{host}:{port}");

    // Try to get a pooled connection, or create a new one
    let mut stream = CONN_POOL
        .with(|p| p.borrow_mut().take(&addr))
        .unwrap_or(TcpStream::connect(&addr).await.map_err(|e| e.to_string())?);

    // Use keep-alive (not Connection: close)
    let header = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         X-App-Id: {app_id}\r\n\
         X-Plan-Id: {plan_id}\r\n\
         X-Request-Id: {request_id}\r\n\
         Connection: keep-alive\r\n\
         \r\n",
        body.len()
    );

    let mut request_bytes = header.into_bytes();
    request_bytes.extend_from_slice(body);

    let BufResult(r, _) = stream.write_all(request_bytes).await;
    if r.is_err() {
        // Connection was stale — reconnect and retry
        stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;
        let header = format!(
            "POST {path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             X-App-Id: {app_id}\r\n\
             X-Plan-Id: {plan_id}\r\n\
             X-Request-Id: {request_id}\r\n\
             Connection: keep-alive\r\n\
             \r\n",
            body.len()
        );
        let mut retry_bytes = header.into_bytes();
        retry_bytes.extend_from_slice(body);
        let BufResult(r, _) = stream.write_all(retry_bytes).await;
        r.map_err(|e| e.to_string())?;
    }

    // Read response — with keep-alive we need Content-Length to know when body ends
    let mut response = Vec::with_capacity(4096);
    let mut header_end = None;
    let mut content_length: Option<usize> = None;

    loop {
        let buf = vec![0u8; 4096];
        let BufResult(r, returned) = stream.read(buf).await;
        let n = r.map_err(|e| e.to_string())?;
        if n == 0 {
            break; // connection closed
        }
        response.extend_from_slice(&returned[..n]);

        // Find header boundary if not found yet
        if header_end.is_none() {
            if let Some(pos) = response.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = Some(pos);
                // Parse Content-Length from headers
                let header_str = std::str::from_utf8(&response[..pos]).unwrap_or("");
                for line in header_str.lines() {
                    if let Some(val) = line.strip_prefix("Content-Length: ")
                        .or_else(|| line.strip_prefix("content-length: "))
                    {
                        content_length = val.trim().parse().ok();
                    }
                }
            }
        }

        // Check if we have the full response
        if let (Some(he), Some(cl)) = (header_end, content_length) {
            let body_start = he + 4;
            if response.len() >= body_start + cl {
                break; // full response received
            }
        }
    }

    let he = header_end.ok_or("no header end")?;
    let header_str = std::str::from_utf8(&response[..he]).map_err(|e| e.to_string())?;
    let body_start = he + 4;
    let body_end = content_length.map(|cl| body_start + cl).unwrap_or(response.len());
    let body_bytes = &response[body_start..body_end];

    // Return connection to pool (if keep-alive and healthy)
    if content_length.is_some() {
        CONN_POOL.with(|p| p.borrow_mut().put(addr, stream));
    }

    // Parse status
    let status = header_str
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(502);

    let mut builder = HttpResponse::build(
        ntex::http::StatusCode::from_u16(status)
            .unwrap_or(ntex::http::StatusCode::BAD_GATEWAY),
    );
    builder.content_type("application/json");

    for line in header_str.lines().skip(1) {
        if let Some((name, value)) = line.split_once(": ") {
            let lname = name.to_ascii_lowercase();
            if lname == "x-cpu-time-ms" || lname == "x-wall-time-ms" {
                builder.set_header(name, value.to_string());
            }
        }
    }

    Ok(builder.body(body_bytes.to_vec()))
}
