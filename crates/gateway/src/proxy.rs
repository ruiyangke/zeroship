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

    /// CHWBL routing with an extra affinity key, used for subscription
    /// traffic. The hash is `app_id || affinity` so reconnects from the
    /// same `(app, principal)` pick the same worker, while different
    /// principals on the same app still spread naturally across the
    /// fleet. The overload guard (`max_per_worker`) is honored: when the
    /// affinity-preferred worker is saturated we walk the ring forward
    /// to the next viable slot. This is a "sticky bit" in CHWBL
    /// terminology — affinity steers the choice but does not override
    /// capacity.
    pub fn select_with_affinity(&self, app_id: &Uuid, affinity: &str) -> (usize, &str) {
        let mut combined = Vec::with_capacity(16 + affinity.len() + 1);
        combined.extend_from_slice(app_id.as_bytes());
        combined.push(b':');
        combined.extend_from_slice(affinity.as_bytes());
        let hash = hash_bytes(&combined);
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

    let mut builder = HttpResponse::build(
        ntex::http::StatusCode::from_u16(parsed.status).unwrap_or(ntex::http::StatusCode::BAD_GATEWAY),
    );
    // SEC-9: the worker response is creator-controlled (untrusted). Drop
    // hop-by-hop headers, force every app `Set-Cookie` host-only (strip any
    // `Domain` so it can't be scoped to a sibling `*.zeroship.ai` app or the
    // platform), and cap cookie count + total size to block a cookie-bomb DoS.
    for (name, value) in sanitize_app_response_headers(&parsed.headers) {
        builder.set_header(name.as_str(), value.as_str());
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

// ---------------------------------------------------------------------------
// App response header sanitation (SEC-9)
// ---------------------------------------------------------------------------

/// Hop-by-hop headers stripped from the worker (app) response before it is
/// forwarded to the browser. RFC 7230 §6.1.
const APP_RESPONSE_HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
];

/// Max number of `Set-Cookie` headers a single app response may emit. Beyond
/// this the extras are dropped — a creator app cannot flood the browser (and
/// every subsequent request's `Cookie` header) with unbounded cookies.
const MAX_APP_SET_COOKIES: usize = 16;

/// Max total bytes (summed `Set-Cookie` header VALUES) a single app response
/// may emit. Once the running total would exceed this, further `Set-Cookie`
/// headers are dropped. Bounds the cookie-bomb / request-header-bloat DoS.
const MAX_APP_SET_COOKIE_TOTAL_BYTES: usize = 8 * 1024;

/// Sanitize a creator-app (worker) HTTP response's headers before they reach
/// the browser (SEC-9).
///
/// * drops hop-by-hop headers ([`APP_RESPONSE_HOP_BY_HOP`]),
/// * forces every `Set-Cookie` host-only by stripping its `Domain` attribute
///   ([`strip_cookie_domain`]) — so a creator app cannot scope a cookie to the
///   parent `zeroship.ai` or a sibling `*.zeroship.ai` app (cross-tenant
///   injection / fixation),
/// * caps the number ([`MAX_APP_SET_COOKIES`]) and total value size
///   ([`MAX_APP_SET_COOKIE_TOTAL_BYTES`]) of `Set-Cookie` headers, dropping the
///   overflow to block a cookie-bomb / request-header-bloat DoS.
///
/// Non-cookie headers pass through unchanged (apart from the hop-by-hop drop).
fn sanitize_app_response_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cookie_count: usize = 0;
    let mut cookie_bytes: usize = 0;
    for (name, value) in headers {
        let lname = name.to_ascii_lowercase();
        if APP_RESPONSE_HOP_BY_HOP.contains(&lname.as_str()) {
            continue;
        }
        if lname == "set-cookie" {
            let scrubbed = strip_cookie_domain(value);
            // Enforce both caps; drop the overflow rather than truncating a
            // cookie mid-value (a partial Set-Cookie is worse than none).
            if cookie_count >= MAX_APP_SET_COOKIES
                || cookie_bytes.saturating_add(scrubbed.len()) > MAX_APP_SET_COOKIE_TOTAL_BYTES
            {
                continue;
            }
            cookie_count += 1;
            cookie_bytes += scrubbed.len();
            out.push((name.clone(), scrubbed));
            continue;
        }
        out.push((name.clone(), value.clone()));
    }
    out
}

/// Remove the `Domain` attribute from a single `Set-Cookie` header value,
/// forcing the cookie host-only (SEC-9). All other attributes (and the
/// `name=value` pair) are preserved in order. Cookie attributes are
/// `;`-delimited and the attribute name is case-insensitive.
fn strip_cookie_domain(set_cookie: &str) -> String {
    let mut parts = set_cookie.split(';');
    // The first `;`-segment is the cookie's `name=value` pair — ALWAYS
    // preserved, even if the cookie is literally named `domain`. Only the
    // trailing ATTRIBUTE segments are subject to the Domain strip.
    let Some(name_value) = parts.next() else {
        return String::new();
    };
    let mut kept: Vec<&str> = vec![name_value.trim()];
    for part in parts {
        let trimmed = part.trim();
        // `Domain` is an `=`-valued attribute (`Domain=example.com`); match the
        // attribute name case-insensitively up to the `=`.
        let attr = trimmed.split('=').next().unwrap_or(trimmed).trim();
        if attr.eq_ignore_ascii_case("domain") {
            continue;
        }
        kept.push(trimmed);
    }
    // Rejoin with the canonical `; ` separator so the output is stable
    // regardless of the app's original spacing.
    kept.join("; ")
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

// ---------------------------------------------------------------------------
// Generic HTTP proxy — used for `auth.zeroship.ai` → hydra / crates/auth
// ---------------------------------------------------------------------------
//
// Unlike `forward_to_worker_dispatch`, which packages requests into a JSON
// envelope for the worker kernel, this path is a transparent HTTP/1.1
// reverse proxy: it preserves the request method, path+query, headers,
// and body verbatim, then streams the response back. Set-Cookie and
// Location headers MUST pass through untouched — Hydra's session cookie
// (scoped to `auth.zeroship.ai`) and its OAuth2 302 redirects depend on
// the client seeing them as if they came directly from hydra.

/// Hop-by-hop headers from RFC 7230 §6.1. These are NOT forwarded in
/// either direction (request to upstream, or response back to client).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
    // Strip `host` from the inbound headers — we set our own.
    "host",
    // `content-length` is determined by the body we forward, not by the
    // inbound header (which may disagree if upstream filters tamper).
    "content-length",
];

/// Forward an HTTP/1.1 request to `upstream_base` and return the response
/// verbatim. `upstream_base` is the full base URL of the upstream
/// (scheme/host/optional-port, e.g., `http://hydra:4444`); the inbound
/// request's path+query and method/headers/body are used as-is.
pub async fn forward_http(
    upstream_base: &str,
    method: &str,
    path_and_query: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<HttpResponse, String> {
    let host_hdr = extract_host_with_port(upstream_base);

    let (mut stream, _, _) = compio::time::timeout(WORKER_TIMEOUT, connect(upstream_base))
        .await
        .map_err(|_| "connect timeout".to_string())?
        .map_err(|e| format!("connect: {e}"))?;

    let request = build_forward_request(method, path_and_query, &host_hdr, headers, body);
    stream
        .write_all(request)
        .await
        .map_err(|e| format!("write: {e}"))?;

    let parsed = compio::time::timeout(WORKER_TIMEOUT, read_http_headers(&mut stream))
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| format!("read: {e}"))?;

    let mut builder = HttpResponse::build(
        ntex::http::StatusCode::from_u16(parsed.status)
            .unwrap_or(ntex::http::StatusCode::BAD_GATEWAY),
    );
    for (name, value) in &parsed.headers {
        let lname = name.to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lname.as_str()) {
            continue;
        }
        // Preserve Set-Cookie and Location verbatim; ntex's set_header
        // appends rather than replaces for multi-valued headers like
        // Set-Cookie when used via .header() (HttpResponseBuilder).
        builder.header(name.as_str(), value.as_str());
    }

    if parsed.is_chunked {
        let (tx, rx) = ntex::channel::mpsc::channel();

        compio::runtime::spawn(async move {
            let mut leftover = parsed.trailing;

            loop {
                while let ChunkDecode::Complete(data, consumed) = decode_next_chunk(&leftover) {
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

                let read_buf = vec![0u8; 4096];
                let BufResult(r, returned) = stream.read(read_buf).await;
                match r {
                    Ok(0) => return,
                    Ok(n) => leftover.extend_from_slice(&returned[..n]),
                    Err(_) => return,
                }
            }
        })
        .detach();

        Ok(builder.streaming(rx))
    } else {
        let response_body = compio::time::timeout(
            WORKER_TIMEOUT,
            read_body_buffered(&mut stream, parsed.content_length, parsed.trailing),
        )
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| format!("read: {e}"))?;

        Ok(builder.body(response_body))
    }
}

/// Build a raw HTTP/1.1 request to forward.
fn build_forward_request(
    method: &str,
    path_and_query: &str,
    host: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Vec<u8> {
    let mut req = String::with_capacity(256 + headers.len() * 32 + body.len());
    req.push_str(method);
    req.push(' ');
    req.push_str(path_and_query);
    req.push_str(" HTTP/1.1\r\n");
    req.push_str("Host: ");
    req.push_str(host);
    req.push_str("\r\n");

    for (name, value) in headers {
        let lname = name.to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lname.as_str()) {
            continue;
        }
        req.push_str(name);
        req.push_str(": ");
        req.push_str(value);
        req.push_str("\r\n");
    }

    req.push_str("Content-Length: ");
    req.push_str(&body.len().to_string());
    req.push_str("\r\n");
    // Close after one round-trip — keeps the proxy path simple, no
    // pool, no half-open retries. The auth host's request rate is low
    // (OIDC discovery / token exchange / userinfo) so pooling isn't
    // worth the complexity here.
    req.push_str("Connection: close\r\n\r\n");

    let mut bytes = req.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// Extract `host:port` from an upstream URL for the `Host:` header.
/// Falls back to `localhost` on parse failure (mirrors `extract_host`'s
/// posture for the worker dispatch path).
fn extract_host_with_port(upstream_url: &str) -> String {
    url::Url::parse(upstream_url)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_string();
            Some(match u.port() {
                Some(p) => format!("{host}:{p}"),
                None => host,
            })
        })
        .unwrap_or_else(|| "localhost".to_string())
}

#[cfg(test)]
mod forward_http_tests {
    use super::*;

    #[test]
    fn extract_host_with_port_includes_port() {
        assert_eq!(
            extract_host_with_port("http://hydra:4444"),
            "hydra:4444"
        );
    }

    #[test]
    fn extract_host_with_port_omits_default_port() {
        // The url crate normalizes :80 / :443 away when they're the
        // default for the scheme — that's fine for forwarding because
        // upstream will still resolve the host.
        assert_eq!(extract_host_with_port("http://hydra"), "hydra");
    }

    #[test]
    fn extract_host_with_port_falls_back_on_garbage() {
        assert_eq!(extract_host_with_port("not a url"), "localhost");
    }

    #[test]
    fn build_forward_request_omits_hop_by_hop_and_host() {
        let headers = vec![
            ("Cookie".to_string(), "abc=1".to_string()),
            ("Host".to_string(), "client-supplied:9999".to_string()),
            ("Connection".to_string(), "keep-alive".to_string()),
            ("Content-Length".to_string(), "999".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        let body = b"x=1";
        let req = build_forward_request("POST", "/oauth2/token", "hydra:4444", &headers, body);
        let s = String::from_utf8(req).unwrap();

        assert!(s.starts_with("POST /oauth2/token HTTP/1.1\r\n"));
        assert!(s.contains("Host: hydra:4444\r\n"));
        assert!(s.contains("Cookie: abc=1\r\n"));
        assert!(s.contains("Accept: application/json\r\n"));
        // Hop-by-hop and host headers from the inbound side must be dropped.
        assert!(!s.contains("client-supplied"));
        assert!(!s.contains("keep-alive"));
        // Content-Length recomputed from the actual body length.
        assert!(s.contains("Content-Length: 3\r\n"));
        // Always close — single-shot proxy.
        assert!(s.contains("Connection: close\r\n"));
        // Body follows the blank line.
        assert!(s.ends_with("\r\n\r\nx=1"));
    }
}

#[cfg(test)]
mod app_response_cookie_tests {
    use super::*;

    fn set_cookies(out: &[(String, String)]) -> Vec<String> {
        out.iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, v)| v.clone())
            .collect()
    }

    #[test]
    fn app_set_cookie_domain_is_stripped_to_host_only() {
        // SEC-9: a creator app at evil.zeroship.ai must not be able to scope a
        // cookie to the parent domain (or any sibling). The forwarded
        // Set-Cookie is forced host-only — its `Domain` attribute is removed —
        // while the rest of the cookie (name=value, Path, Secure, HttpOnly,
        // SameSite) is preserved.
        //
        // Pre-fix the helper forwards Set-Cookie verbatim → the `Domain`
        // survives → RED.
        let headers = vec![
            (
                "Set-Cookie".to_string(),
                "sid=abc; Domain=zeroship.ai; Path=/; Secure; HttpOnly; SameSite=Lax".to_string(),
            ),
            ("Content-Type".to_string(), "text/html".to_string()),
        ];
        let out = sanitize_app_response_headers(&headers);
        let cookies = set_cookies(&out);
        assert_eq!(cookies.len(), 1, "the one app cookie is forwarded");
        let c = &cookies[0];
        assert!(
            !c.to_ascii_lowercase().contains("domain="),
            "Domain must be stripped (host-only cookie); got {c:?}"
        );
        // The non-Domain attributes survive so the app's cookie still works.
        assert!(c.contains("sid=abc"), "cookie name=value preserved: {c:?}");
        assert!(c.contains("Path=/"), "Path preserved: {c:?}");
        assert!(c.contains("Secure"), "Secure preserved: {c:?}");
        assert!(c.contains("HttpOnly"), "HttpOnly preserved: {c:?}");
        assert!(c.contains("SameSite=Lax"), "SameSite preserved: {c:?}");
        // A non-cookie header is untouched.
        assert!(
            out.iter()
                .any(|(k, v)| k == "Content-Type" && v == "text/html"),
            "non-cookie headers pass through: {out:?}"
        );
    }

    #[test]
    fn app_set_cookie_named_domain_is_preserved() {
        // SEC-9 regression: the Domain STRIP must apply only to the `Domain`
        // ATTRIBUTE, never to a cookie literally NAMED `domain`. The first
        // `;`-segment is the cookie's name=value pair; dropping it (as a naive
        // attribute filter does) silently breaks the app's cookie.
        let out = strip_cookie_domain("domain=abc123; Path=/; Domain=evil.zeroship.ai");
        // The name=value pair (cookie named `domain`) survives...
        assert!(
            out.starts_with("domain=abc123"),
            "cookie named `domain` lost its name=value pair: {out:?}"
        );
        // ...the Domain ATTRIBUTE is stripped...
        assert!(
            !out.to_ascii_lowercase().contains("domain=evil"),
            "Domain attribute must still be stripped: {out:?}"
        );
        // ...and other attributes are kept.
        assert!(out.contains("Path=/"), "Path preserved: {out:?}");
    }

    #[test]
    fn app_set_cookie_count_is_capped() {
        // SEC-9: an app cannot flood the browser with unbounded cookies.
        // Pre-fix all 40 are forwarded → RED.
        let mut headers = Vec::new();
        for i in 0..40 {
            headers.push(("Set-Cookie".to_string(), format!("c{i}=v{i}; Path=/")));
        }
        let out = sanitize_app_response_headers(&headers);
        assert!(
            set_cookies(&out).len() <= MAX_APP_SET_COOKIES,
            "Set-Cookie count must be capped at {MAX_APP_SET_COOKIES}; got {}",
            set_cookies(&out).len()
        );
    }

    #[test]
    fn app_set_cookie_total_size_is_capped() {
        // SEC-9: a few enormous cookies are a cookie-bomb DoS — cap the total
        // forwarded Set-Cookie bytes. Pre-fix every megabyte cookie is
        // forwarded → RED.
        let big = "x".repeat(4096);
        let mut headers = Vec::new();
        for i in 0..8 {
            headers.push(("Set-Cookie".to_string(), format!("big{i}={big}")));
        }
        let out = sanitize_app_response_headers(&headers);
        let total: usize = set_cookies(&out).iter().map(String::len).sum();
        assert!(
            total <= MAX_APP_SET_COOKIE_TOTAL_BYTES,
            "total Set-Cookie bytes must be capped at {MAX_APP_SET_COOKIE_TOTAL_BYTES}; got {total}"
        );
    }

    #[test]
    fn app_set_cookie_without_domain_is_unchanged() {
        // The common host-only cookie is forwarded byte-for-byte (no Domain to
        // strip) — sanitation must not corrupt a well-behaved app cookie.
        let headers = vec![(
            "Set-Cookie".to_string(),
            "theme=dark; Path=/; Secure; SameSite=Strict".to_string(),
        )];
        let out = sanitize_app_response_headers(&headers);
        assert_eq!(
            set_cookies(&out),
            vec!["theme=dark; Path=/; Secure; SameSite=Strict".to_string()]
        );
    }
}
