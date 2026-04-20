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

/// Forward an HTTP request to a worker via the unified `/dispatch/{app_id}`
/// endpoint.
///
/// Wraps the original HTTP request (method, URL, headers, body) into the
/// JSON envelope the worker's kernel passes to `Runtime::call_fetch_handler`
/// (which in turn invokes the app's exported `default.fetch`). This is the
/// one path the kernel exposes — both `_rpc/*` URLs and normal HTTP requests
/// travel the same wire; any routing within the app happens in user-space JS
/// via the bootstrap router.
pub async fn forward_dispatch(
    ring: &HashRing,
    app_id: &Uuid,
    plan_id: &str,
    request_id: &Uuid,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &str,
    user_header: Option<&str>,
    worker_key: &str,
) -> Result<HttpResponse, String> {
    if ring.num_workers() == 0 {
        return Err("no workers configured".into());
    }
    let (idx, worker_url) = ring.select(app_id);
    ring.acquire(idx);

    // Build the HTTP envelope JSON.
    let envelope = serde_json::json!({
        "method": method,
        "url": url,
        "headers": headers,
        "body": body,
    });
    let envelope_bytes = serde_json::to_vec(&envelope).unwrap_or_default();

    let result = forward_to_worker_dispatch(
        worker_url, app_id, plan_id, request_id, &envelope_bytes, user_header, worker_key,
    ).await;
    ring.release(idx);
    result
}

/// Timeout for connecting and reading from workers.
const WORKER_TIMEOUT: Duration = Duration::from_secs(30);

/// Forward the HTTP envelope to a worker at `/dispatch/{app_id}`.
async fn forward_to_worker_dispatch(
    worker_url: &str,
    app_id: &Uuid,
    plan_id: &str,
    request_id: &Uuid,
    body: &[u8],
    user_header: Option<&str>,
    worker_key: &str,
) -> Result<HttpResponse, String> {
    let key = pool_key(worker_url);
    let path = format!("/dispatch/{app_id}");

    let host = extract_host(worker_url);

    // Attempt up to 2 times: once with a pooled connection (if any), once
    // with a fresh one. A pooled TCP connection can be half-open — the
    // peer closed it after a keep-alive timeout but we haven't noticed
    // yet. The write may succeed into the kernel buffer; the failure only
    // surfaces on read as "connection closed before headers complete".
    // Retrying on a fresh connection handles that race.
    let (mut stream, mut from_pool) = match CONN_POOL.with(|p| p.borrow_mut().take(&key)) {
        Some(s) => (s, true),
        None => {
            let (s, _, _) = compio::time::timeout(WORKER_TIMEOUT, connect(worker_url))
                .await
                .map_err(|_| "connect timeout".to_string())?
                .map_err(|e| format!("connect: {e}"))?;
            (s, false)
        }
    };

    let request = build_request(&path, &host, app_id, plan_id, request_id, body, user_header, worker_key);

    if stream.write_all(request).await.is_err() {
        let (new_stream, _, _) = compio::time::timeout(WORKER_TIMEOUT, connect(worker_url))
            .await
            .map_err(|_| "reconnect timeout".to_string())?
            .map_err(|e| format!("reconnect: {e}"))?;
        stream = new_stream;
        from_pool = false;
        let retry_request = build_request(&path, &host, app_id, plan_id, request_id, body, user_header, worker_key);
        stream.write_all(retry_request).await.map_err(|e| format!("write: {e}"))?;
    }

    let parsed = match compio::time::timeout(WORKER_TIMEOUT, read_http_headers(&mut stream)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) if from_pool => {
            let (new_stream, _, _) = compio::time::timeout(WORKER_TIMEOUT, connect(worker_url))
                .await
                .map_err(|_| format!("reconnect timeout (after: {e})"))?
                .map_err(|e| format!("reconnect: {e}"))?;
            stream = new_stream;
            from_pool = false;
            let retry_request = build_request(&path, &host, app_id, plan_id, request_id, body, user_header, worker_key);
            stream.write_all(retry_request).await.map_err(|e| format!("write: {e}"))?;
            compio::time::timeout(WORKER_TIMEOUT, read_http_headers(&mut stream))
                .await
                .map_err(|_| "read timeout".to_string())?
                .map_err(|e| format!("read: {e}"))?
        }
        Ok(Err(e)) => return Err(format!("read: {e}")),
        Err(_) => return Err("read timeout".to_string()),
    };
    let _ = from_pool;

    let hop_by_hop = ["connection", "keep-alive", "transfer-encoding",
                      "te", "trailer", "upgrade", "proxy-authorization", "proxy-authenticate"];

    let mut builder = HttpResponse::build(
        ntex::http::StatusCode::from_u16(parsed.status).unwrap_or(ntex::http::StatusCode::BAD_GATEWAY),
    );
    for (name, value) in &parsed.headers {
        let lname = name.to_ascii_lowercase();
        if !hop_by_hop.contains(&lname.as_str()) {
            builder.set_header(name.as_str(), value.as_str());
        }
    }

    if parsed.is_chunked {
        let (tx, rx) = ntex::channel::mpsc::channel();

        compio::runtime::spawn(async move {
            let mut leftover = parsed.trailing;

            loop {
                loop {
                    match decode_next_chunk(&leftover) {
                        ChunkDecode::Complete(data, consumed) => {
                            if data.is_empty() {
                                return;
                            }
                            let item: Result<ntex::util::Bytes, std::io::Error> =
                                Ok(ntex::util::Bytes::from(data));
                            if tx.send(item).is_err() {
                                return;
                            }
                            leftover = leftover[consumed..].to_vec();
                        }
                        ChunkDecode::Incomplete => break,
                    }
                }

                let read_buf = vec![0u8; 4096];
                let BufResult(r, returned) = stream.read(read_buf).await;
                match r {
                    Ok(0) => return,
                    Ok(n) => leftover.extend_from_slice(&returned[..n]),
                    Err(_) => return,
                }
            }
        }).detach();

        Ok(builder.streaming(rx))
    } else {
        let response_body = compio::time::timeout(
            WORKER_TIMEOUT,
            read_body_buffered(&mut stream, parsed.content_length, parsed.trailing),
        )
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| format!("read: {e}"))?;

        CONN_POOL.with(|p| p.borrow_mut().put(key, stream));
        Ok(builder.body(response_body))
    }
}

fn build_request(
    path: &str,
    host: &str,
    app_id: &Uuid,
    plan_id: &str,
    request_id: &Uuid,
    body: &[u8],
    user_header: Option<&str>,
    worker_key: &str,
) -> Vec<u8> {
    let user_line = match user_header {
        Some(val) => format!("ZeroShip-User: {val}\r\n"),
        None => String::new(),
    };
    let auth_line = if worker_key.is_empty() {
        String::new()
    } else {
        format!("Authorization: Bearer {worker_key}\r\n")
    };
    let header = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         X-App-Id: {app_id}\r\n\
         X-Plan-Id: {plan_id}\r\n\
         X-Request-Id: {request_id}\r\n\
         {auth_line}\
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

// ---------------------------------------------------------------------------
// HTTP response parsing — split into header + body phases for streaming support
// ---------------------------------------------------------------------------

/// Parsed HTTP response headers with metadata needed for body reading.
struct ParsedHeaders {
    status: u16,
    headers: Vec<(String, String)>,
    content_length: Option<usize>,
    is_chunked: bool,
    /// Bytes read past the header boundary (start of body data).
    trailing: Vec<u8>,
}

/// Read HTTP response headers from the stream. Returns parsed header info
/// and any trailing bytes that were read past the header boundary.
async fn read_http_headers(stream: &mut Stream) -> Result<ParsedHeaders, String> {
    let mut buf = Vec::with_capacity(4096);

    loop {
        let read_buf = vec![0u8; 4096];
        let BufResult(r, returned) = stream.read(read_buf).await;
        let n = r.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before headers complete".to_string());
        }
        buf.extend_from_slice(&returned[..n]);

        let mut parsed_headers = [httparse::EMPTY_HEADER; 32];
        let mut resp = httparse::Response::new(&mut parsed_headers);
        match resp.parse(&buf) {
            Ok(httparse::Status::Complete(header_len)) => {
                let status = resp.code.unwrap_or(502);
                let mut headers = Vec::new();
                let mut content_length = None;
                let mut is_chunked = false;

                for h in resp.headers.iter() {
                    let name = h.name.to_string();
                    let value = String::from_utf8_lossy(h.value).to_string();
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().ok();
                    }
                    if name.eq_ignore_ascii_case("transfer-encoding")
                        && value.to_ascii_lowercase().contains("chunked")
                    {
                        is_chunked = true;
                    }
                    headers.push((name, value));
                }

                let trailing = buf[header_len..].to_vec();

                return Ok(ParsedHeaders {
                    status,
                    headers,
                    content_length,
                    is_chunked,
                    trailing,
                });
            }
            Ok(httparse::Status::Partial) => continue,
            Err(e) => return Err(format!("parse: {e}")),
        }
    }
}

/// Read a complete body using Content-Length or close-delimited mode.
/// Used for non-chunked (buffered) responses.
async fn read_body_buffered(
    stream: &mut Stream,
    content_length: Option<usize>,
    trailing: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let mut body = trailing;

    if let Some(cl) = content_length {
        // Content-Length mode: read exactly `cl` bytes
        while body.len() < cl {
            let read_buf = vec![0u8; 4096];
            let BufResult(r, returned) = stream.read(read_buf).await;
            let n = r.map_err(|e| e.to_string())?;
            if n == 0 { break; }
            body.extend_from_slice(&returned[..n]);
        }
        body.truncate(cl);
    } else {
        // Close-delimited: read until connection close
        loop {
            let read_buf = vec![0u8; 4096];
            let BufResult(r, returned) = stream.read(read_buf).await;
            let n = r.map_err(|e| e.to_string())?;
            if n == 0 { break; }
            body.extend_from_slice(&returned[..n]);
        }
    }

    Ok(body)
}

// ---------------------------------------------------------------------------
// Chunked transfer-encoding decoder
// ---------------------------------------------------------------------------

/// Result of attempting to decode the next chunk from a buffer.
enum ChunkDecode {
    /// A complete chunk was decoded: (data, bytes_consumed_from_buffer).
    /// data is empty for the final zero-length terminator chunk.
    Complete(Vec<u8>, usize),
    /// Not enough data in the buffer to decode a complete chunk.
    Incomplete,
}

/// Attempt to decode the next HTTP chunked-encoding frame from `buf`.
///
/// Chunked format: `<hex-size>\r\n<data>\r\n`, terminated by `0\r\n\r\n`.
fn decode_next_chunk(buf: &[u8]) -> ChunkDecode {
    // Find the chunk size line ending (\r\n)
    let Some(crlf_pos) = find_crlf(buf) else {
        return ChunkDecode::Incomplete;
    };

    // Parse hex size
    let size_str = match std::str::from_utf8(&buf[..crlf_pos]) {
        Ok(s) => s.trim(),
        Err(_) => return ChunkDecode::Incomplete,
    };
    // Strip chunk extensions (anything after ';')
    let size_hex = size_str.split(';').next().unwrap_or("").trim();
    let chunk_size = match usize::from_str_radix(size_hex, 16) {
        Ok(s) => s,
        Err(_) => return ChunkDecode::Incomplete,
    };

    // Total bytes for this chunk: size_line + \r\n + data + \r\n
    let data_start = crlf_pos + 2; // past the first \r\n
    let chunk_end = data_start + chunk_size + 2; // data + trailing \r\n

    if buf.len() < chunk_end {
        return ChunkDecode::Incomplete;
    }

    if chunk_size == 0 {
        // Terminal chunk
        return ChunkDecode::Complete(Vec::new(), chunk_end);
    }

    let data = buf[data_start..data_start + chunk_size].to_vec();
    ChunkDecode::Complete(data, chunk_end)
}

/// Find the position of the first \r\n in `buf`.
fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}
