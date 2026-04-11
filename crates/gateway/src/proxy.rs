//! HTTP proxy with CHWBL routing + connection pool + Unix domain socket support.
//!
//! - XXH3 for fast, well-distributed hashing
//! - Connection pool per worker (keep-alive, TCP or UDS)
//! - Bounded load: overflow spills to next worker on ring
//! - unix:///path/to/socket URLs for same-machine workers (bypasses TCP/IP stack)

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpStream, UnixStream};
use ntex::web::HttpResponse;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Stream abstraction (TCP or Unix)
// ---------------------------------------------------------------------------

enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Stream {
    async fn write_all(&mut self, data: Vec<u8>) -> Result<(), std::io::Error> {
        match self {
            Self::Tcp(s) => {
                let BufResult(r, _) = compio::io::AsyncWriteExt::write_all(s, data).await;
                r
            }
            Self::Unix(s) => {
                let BufResult(r, _) = compio::io::AsyncWriteExt::write_all(s, data).await;
                r
            }
        }
    }

    async fn read(&mut self, buf: Vec<u8>) -> BufResult<usize, Vec<u8>> {
        match self {
            Self::Tcp(s) => compio::io::AsyncRead::read(s, buf).await,
            Self::Unix(s) => compio::io::AsyncRead::read(s, buf).await,
        }
    }
}

// ---------------------------------------------------------------------------
// CHWBL Hash Ring (XXH3)
// ---------------------------------------------------------------------------

const VNODES_PER_WORKER: usize = 150;

#[inline]
fn hash_bytes(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}

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
                ring.insert(hash_bytes(format!("{url}-vnode-{i}").as_bytes()), idx);
            }
        }
        let active = (0..worker_urls.len()).map(|_| AtomicU32::new(0)).collect();
        Self { ring, workers: worker_urls, active, max_per_worker }
    }

    pub fn select(&self, app_id: &Uuid) -> (usize, &str) {
        let hash = hash_bytes(app_id.as_bytes());
        for (_, &idx) in self.ring.range(hash..).chain(self.ring.iter()) {
            if self.active[idx].load(Ordering::Relaxed) < self.max_per_worker {
                return (idx, &self.workers[idx]);
            }
        }
        let least = self.active.iter().enumerate()
            .min_by_key(|(_, a)| a.load(Ordering::Relaxed))
            .map(|(i, _)| i).unwrap_or(0);
        (least, &self.workers[least])
    }

    pub fn acquire(&self, idx: usize) { self.active[idx].fetch_add(1, Ordering::Relaxed); }
    pub fn release(&self, idx: usize) { self.active[idx].fetch_sub(1, Ordering::Release); }
    pub fn num_workers(&self) -> usize { self.workers.len() }
}

impl std::fmt::Debug for HashRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HashRing").field("workers", &self.workers.len()).finish()
    }
}

// ---------------------------------------------------------------------------
// Connection Pool (TCP + Unix, thread-local)
// ---------------------------------------------------------------------------

struct ConnPool {
    pools: HashMap<String, VecDeque<Stream>>,
    max_idle: usize,
}

impl ConnPool {
    fn new(max_idle: usize) -> Self {
        Self { pools: HashMap::new(), max_idle }
    }
    fn take(&mut self, key: &str) -> Option<Stream> {
        self.pools.get_mut(key)?.pop_front()
    }
    fn put(&mut self, key: String, stream: Stream) {
        let pool = self.pools.entry(key).or_default();
        if pool.len() < self.max_idle { pool.push_back(stream); }
    }
}

thread_local! {
    static CONN_POOL: RefCell<ConnPool> = RefCell::new(ConnPool::new(8));
}

// ---------------------------------------------------------------------------
// Connect helper — TCP or Unix based on URL scheme
// ---------------------------------------------------------------------------

/// Parse worker URL and connect.
/// - `http://host:port` → TCP
/// - `unix:///path/to/socket` → Unix domain socket
async fn connect(worker_url: &str) -> Result<(Stream, String, String), String> {
    if let Some(path) = worker_url.strip_prefix("unix://") {
        let stream = UnixStream::connect(path).await.map_err(|e| e.to_string())?;
        Ok((Stream::Unix(stream), path.to_string(), String::new()))
    } else {
        let parsed = url::Url::parse(worker_url).map_err(|e| e.to_string())?;
        let host = parsed.host_str().ok_or("no host")?.to_string();
        let port = parsed.port().unwrap_or(80);
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;
        Ok((Stream::Tcp(stream), addr, host))
    }
}

/// Pool key for a worker URL.
fn pool_key(worker_url: &str) -> String {
    if let Some(path) = worker_url.strip_prefix("unix://") {
        format!("unix:{path}")
    } else if let Ok(parsed) = url::Url::parse(worker_url) {
        let host = parsed.host_str().unwrap_or("localhost");
        let port = parsed.port().unwrap_or(80);
        format!("tcp:{host}:{port}")
    } else {
        worker_url.to_string()
    }
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
    let (idx, worker_url) = ring.select(app_id);
    ring.acquire(idx);
    let result = forward_to_worker(worker_url, app_id, plan_id, request_id, body).await;
    ring.release(idx);
    result
}

async fn forward_to_worker(
    worker_url: &str,
    app_id: &Uuid,
    plan_id: &str,
    request_id: &Uuid,
    body: &[u8],
) -> Result<HttpResponse, String> {
    let key = pool_key(worker_url);
    let path = format!("/dispatch/{app_id}");
    let host = if worker_url.starts_with("unix://") { "localhost" } else {
        // Extract host from URL
        url::Url::parse(worker_url).ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "localhost".to_string())
            .leak() // safe: worker URLs are static for the process lifetime
    };

    // Get pooled connection or create new
    let mut stream = match CONN_POOL.with(|p| p.borrow_mut().take(&key)) {
        Some(s) => s,
        None => {
            let (s, _, _) = connect(worker_url).await?;
            s
        }
    };

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

    if stream.write_all(request_bytes.clone()).await.is_err() {
        // Stale connection — reconnect and retry
        let (new_stream, _, _) = connect(worker_url).await?;
        stream = new_stream;
        stream.write_all(request_bytes).await.map_err(|e| e.to_string())?;
    }

    // Read response with Content-Length framing
    let mut response = Vec::with_capacity(4096);
    let mut header_end = None;
    let mut content_length: Option<usize> = None;

    loop {
        let buf = vec![0u8; 4096];
        let BufResult(r, returned) = stream.read(buf).await;
        let n = r.map_err(|e| e.to_string())?;
        if n == 0 { break; }
        response.extend_from_slice(&returned[..n]);

        if header_end.is_none() {
            if let Some(pos) = response.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = Some(pos);
                let hdr = std::str::from_utf8(&response[..pos]).unwrap_or("");
                for line in hdr.lines() {
                    if let Some(val) = line.strip_prefix("Content-Length: ")
                        .or_else(|| line.strip_prefix("content-length: "))
                    {
                        content_length = val.trim().parse().ok();
                    }
                }
            }
        }

        if let (Some(he), Some(cl)) = (header_end, content_length) {
            if response.len() >= he + 4 + cl { break; }
        }
    }

    let he = header_end.ok_or("no header end")?;
    let header_str = std::str::from_utf8(&response[..he]).map_err(|e| e.to_string())?;
    let body_start = he + 4;
    let body_end = content_length.map(|cl| body_start + cl).unwrap_or(response.len());
    let body_bytes = &response[body_start..body_end];

    // Return to pool if healthy
    if content_length.is_some() {
        CONN_POOL.with(|p| p.borrow_mut().put(key, stream));
    }

    let status = header_str.split_whitespace().nth(1)
        .and_then(|s| s.parse::<u16>().ok()).unwrap_or(502);

    let mut builder = HttpResponse::build(
        ntex::http::StatusCode::from_u16(status).unwrap_or(ntex::http::StatusCode::BAD_GATEWAY),
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
