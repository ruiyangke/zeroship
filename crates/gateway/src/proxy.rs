//! HTTP proxy with CHWBL routing + connection pool + Unix domain socket support.
//!
//! - XXH3 for fast, well-distributed hashing
//! - Connection pool per worker (keep-alive, TCP or UDS)
//! - Bounded load: overflow spills to next worker on ring
//! - unix:///path/to/socket URLs for same-machine workers (bypasses TCP/IP stack)

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use compio::buf::BufResult;
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
    user_header: Option<&str>,
) -> Result<HttpResponse, String> {
    if ring.num_workers() == 0 {
        return Err("no workers configured".into());
    }
    let (idx, worker_url) = ring.select(app_id);
    ring.acquire(idx);
    let result = forward_to_worker(worker_url, app_id, plan_id, request_id, body, user_header).await;
    ring.release(idx);
    result
}

/// Timeout for connecting and reading from workers.
const WORKER_TIMEOUT: Duration = Duration::from_secs(30);

async fn forward_to_worker(
    worker_url: &str,
    app_id: &Uuid,
    plan_id: &str,
    request_id: &Uuid,
    body: &[u8],
    user_header: Option<&str>,
) -> Result<HttpResponse, String> {
    let key = pool_key(worker_url);
    let path = format!("/dispatch/{app_id}");

    // Cache host extraction (no .leak())
    let host = extract_host(worker_url);

    // Get pooled connection or create new (with timeout)
    let mut stream = match CONN_POOL.with(|p| p.borrow_mut().take(&key)) {
        Some(s) => s,
        None => {
            let (s, _, _) = compio::time::timeout(WORKER_TIMEOUT, connect(worker_url))
                .await
                .map_err(|_| "connect timeout".to_string())?
                .map_err(|e| format!("connect: {e}"))?;
            s
        }
    };

    // Build request (no clone — rebuild on retry if needed)
    let request = build_request(&path, &host, app_id, plan_id, request_id, body, user_header);

    if stream.write_all(request).await.is_err() {
        // Stale connection — reconnect with timeout
        let (new_stream, _, _) = compio::time::timeout(WORKER_TIMEOUT, connect(worker_url))
            .await
            .map_err(|_| "reconnect timeout".to_string())?
            .map_err(|e| format!("reconnect: {e}"))?;
        stream = new_stream;
        let retry_request = build_request(&path, &host, app_id, plan_id, request_id, body, user_header);
        stream.write_all(retry_request).await.map_err(|e| format!("write: {e}"))?;
    }

    // Read response with timeout and httparse
    let (status, headers, response_body) = compio::time::timeout(
        WORKER_TIMEOUT,
        read_http_response(&mut stream),
    )
    .await
    .map_err(|_| "read timeout".to_string())?
    .map_err(|e| format!("read: {e}"))?;

    // Return connection to pool if healthy
    CONN_POOL.with(|p| p.borrow_mut().put(key, stream));

    // Build gateway response — forward all non-hop-by-hop headers
    let mut builder = HttpResponse::build(
        ntex::http::StatusCode::from_u16(status).unwrap_or(ntex::http::StatusCode::BAD_GATEWAY),
    );

    let hop_by_hop = ["connection", "keep-alive", "transfer-encoding",
                      "te", "trailer", "upgrade", "proxy-authorization", "proxy-authenticate"];
    for (name, value) in &headers {
        let lname = name.to_ascii_lowercase();
        if !hop_by_hop.contains(&lname.as_str()) {
            builder.set_header(name.as_str(), value.as_str());
        }
    }

    Ok(builder.body(response_body))
}

fn build_request(path: &str, host: &str, app_id: &Uuid, plan_id: &str, request_id: &Uuid, body: &[u8], user_header: Option<&str>) -> Vec<u8> {
    let user_line = match user_header {
        Some(val) => format!("X-ZS-User: {val}\r\n"),
        None => String::new(),
    };
    let header = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         X-App-Id: {app_id}\r\n\
         X-Plan-Id: {plan_id}\r\n\
         X-Request-Id: {request_id}\r\n\
         {user_line}\
         Connection: keep-alive\r\n\
         \r\n",
        body.len()
    );
    let mut bytes = header.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn extract_host(worker_url: &str) -> String {
    if worker_url.starts_with("unix://") {
        "localhost".to_string()
    } else {
        url::Url::parse(worker_url).ok()
            .and_then(|u| u.host_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "localhost".to_string())
    }
}

/// Read a full HTTP response using httparse, supporting both Content-Length and close-delimited.
async fn read_http_response(stream: &mut Stream) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
    let mut buf = Vec::with_capacity(4096);
    let mut header_len = 0;
    let mut status = 200u16;
    let mut content_length: Option<usize> = None;
    let mut headers_parsed = Vec::new();
    let mut headers_done = false;

    loop {
        let read_buf = vec![0u8; 4096];
        let BufResult(r, returned) = stream.read(read_buf).await;
        let n = r.map_err(|e| e.to_string())?;
        if n == 0 { break; }
        buf.extend_from_slice(&returned[..n]);

        if !headers_done {
            let mut parsed_headers = [httparse::EMPTY_HEADER; 32];
            let mut resp = httparse::Response::new(&mut parsed_headers);
            match resp.parse(&buf) {
                Ok(httparse::Status::Complete(len)) => {
                    header_len = len;
                    status = resp.code.unwrap_or(502);
                    headers_done = true;

                    for h in resp.headers.iter() {
                        let name = h.name.to_string();
                        let value = String::from_utf8_lossy(h.value).to_string();
                        if name.eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().ok();
                        }
                        headers_parsed.push((name, value));
                    }
                }
                Ok(httparse::Status::Partial) => continue,
                Err(e) => return Err(format!("parse: {e}")),
            }
        }

        // Check if we have the full body
        if headers_done {
            if let Some(cl) = content_length {
                if buf.len() >= header_len + cl { break; }
            }
            // No Content-Length: read until connection close (handled by n == 0 above)
        }
    }

    if !headers_done {
        return Err("incomplete response".to_string());
    }

    let body_end = content_length.map(|cl| header_len + cl).unwrap_or(buf.len());
    let body = buf[header_len..body_end].to_vec();

    Ok((status, headers_parsed, body))
}
