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
use zeroship_core::app_derivation;
use zeroship_core::app_id::AppId;

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

    /// Place an app on a worker.
    ///
    /// The ring position comes from
    /// [`zeroship_core::app_derivation::ring_key`], which returns the app id's
    /// EMBEDDED bits and nothing else. This read `app_id.as_bytes()` until the
    /// derivation seam landed, and that spelling was a trap rather than a
    /// shorthand: `Uuid::as_bytes` and `str::as_bytes` both coerce to `&[u8]`,
    /// so the day the id becomes a typed string this line would have kept
    /// compiling and quietly hashed the printed form instead - rehashing the
    /// whole ring and evicting every warm isolate in the fleet at once. Going
    /// through a function that returns `[u8; 16]` makes that substitution
    /// impossible to make by accident.
    pub fn select(&self, app_id: &AppId) -> (usize, &str) {
        let hash = hash_bytes(app_derivation::ring_key(app_id));
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
    pub fn select_with_affinity(&self, app_id: &AppId, affinity: &str) -> (usize, &str) {
        let mut combined = Vec::with_capacity(16 + affinity.len() + 1);
        // The app half is the EMBEDDED bits, for the reason spelled out on
        // [`Self::select`].
        combined.extend_from_slice(app_derivation::ring_key(app_id));
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
/// Wraps the original HTTP request metadata into a small length-prefixed JSON
/// prefix, then appends the original request body as raw bytes. The worker's
/// kernel passes the decoded shape to `Runtime::call_fetch_handler` (which in
/// turn invokes the app's exported `default.fetch`). This is the one path the
/// kernel exposes — both `_rpc/*` URLs and normal HTTP requests travel the
/// same wire; any routing within the app happens in user-space JS via the
/// bootstrap router.
// `forward_dispatch` -> `forward_to_worker_path` -> `build_request` thread
// the same per-request identifiers (app/plan/request id, worker key, user
// header) down the dispatch hot path. Bundling them into a params struct is
// a real refactor across all three functions, not a mechanical lint fix -
// not doing that as part of a lint sweep.
#[allow(clippy::too_many_arguments)]
pub async fn forward_dispatch(
    ring: &HashRing,
    app_id: &AppId,
    plan_id: &str,
    request_id: &Uuid,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    user_header: Option<&str>,
    authorization: Option<&str>,
) -> Result<HttpResponse, String> {
    if ring.num_workers() == 0 {
        return Err("no workers configured".into());
    }

    let envelope_bytes =
        zeroship_core::dispatch_frame::encode_dispatch_frame(method, url, headers, body)
            .map_err(|e| format!("invalid dispatch envelope: {e}"))?;

    let (idx, worker_url) = ring.select(app_id);
    ring.acquire(idx);

    let result = forward_to_worker_dispatch(
        worker_url, app_id, plan_id, request_id, &envelope_bytes, user_header, authorization,
    ).await;
    ring.release(idx);
    result
}

/// Timeout for connecting and reading from workers.
const WORKER_TIMEOUT: Duration = Duration::from_secs(30);

/// Forward the HTTP envelope to a worker at `/dispatch/{app_id}`.
async fn forward_to_worker_dispatch(
    worker_url: &str,
    app_id: &AppId,
    plan_id: &str,
    request_id: &Uuid,
    body: &[u8],
    user_header: Option<&str>,
    authorization: Option<&str>,
) -> Result<HttpResponse, String> {
    let path = format!("/dispatch/{}", app_id.as_str());

    forward_to_worker_path(
        worker_url,
        &path,
        app_id,
        plan_id,
        request_id,
        body,
        user_header,
        authorization,
    )
    .await
}

// See the allow on `forward_dispatch` above - same call-chain rationale.
#[allow(clippy::too_many_arguments)]
async fn forward_to_worker_path(
    worker_url: &str,
    path: &str,
    app_id: &AppId,
    plan_id: &str,
    request_id: &Uuid,
    body: &[u8],
    user_header: Option<&str>,
    authorization: Option<&str>,
) -> Result<HttpResponse, String> {
    let key = pool_key(worker_url);

    let host = extract_host(worker_url);

    // Attempt up to 2 times: once with a pooled connection (if any), once
    // with a fresh one. A pooled TCP connection can be half-open — the
    // peer closed it after a keep-alive timeout but we haven't noticed
    // yet. The write may succeed into the kernel buffer; the failure only
    // surfaces on read as "connection closed before headers complete".
    // Retrying on a fresh connection handles that race.
    //
    // The retry is gated on ZERO RESPONSE BYTES HAVING BEEN READ
    // ([`ReadFailure::consumed`]). Once the worker has sent us so much as a
    // partial status line it has parsed the request and run the handler —
    // any writes that handler made are committed. Re-sending the request to
    // a fresh worker would execute a non-idempotent mutation twice, and the
    // gateway's idempotency layer cannot catch it: that layer sits above
    // this function with its in-flight lock already held.
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

    let request = build_request(path, &host, app_id, plan_id, request_id, body, user_header, authorization);

    if stream.write_all(request).await.is_err() {
        let (new_stream, _, _) = compio::time::timeout(WORKER_TIMEOUT, connect(worker_url))
            .await
            .map_err(|_| "reconnect timeout".to_string())?
            .map_err(|e| format!("reconnect: {e}"))?;
        stream = new_stream;
        from_pool = false;
        let retry_request = build_request(path, &host, app_id, plan_id, request_id, body, user_header, authorization);
        stream.write_all(retry_request).await.map_err(|e| format!("write: {e}"))?;
    }

    let parsed = match compio::time::timeout(WORKER_TIMEOUT, read_http_headers(&mut stream)).await {
        Ok(Ok(p)) => p,
        // Retry ONLY when nothing came back on the wire. A failure that
        // already consumed response bytes proves the worker ran the handler;
        // replaying it would double-execute the mutation, so it propagates.
        Ok(Err(e)) if from_pool && !e.consumed => {
            let (new_stream, _, _) = compio::time::timeout(WORKER_TIMEOUT, connect(worker_url))
                .await
                .map_err(|_| format!("reconnect timeout (after: {e})"))?
                .map_err(|e| format!("reconnect: {e}"))?;
            stream = new_stream;
            from_pool = false;
            let retry_request = build_request(path, &host, app_id, plan_id, request_id, body, user_header, authorization);
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
        compio::runtime::spawn(relay_chunked(stream, parsed.trailing, tx)).detach();
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

/// Relay a chunked upstream body into `tx`, starting from the bytes already
/// read past its headers, until its terminating zero-length chunk.
///
/// An upstream that closes or fails before that chunk sent an incomplete
/// body: a worker ends a response that way when the code producing it failed
/// part-way. The relay ends the downstream body with an error in that case,
/// never with a clean end, which would tell the client a cut-off body was
/// whole.
async fn relay_chunked(
    mut stream: Stream,
    mut leftover: Vec<u8>,
    tx: ntex::channel::mpsc::Sender<Result<ntex::util::Bytes, std::io::Error>>,
) {
    loop {
        loop {
            match decode_next_chunk(&leftover) {
                ChunkDecode::Complete(data, consumed) => {
                    if data.is_empty() {
                        return;
                    }
                    if tx.send(Ok(ntex::util::Bytes::from(data))).is_err() {
                        return;
                    }
                    leftover = leftover[consumed..].to_vec();
                }
                ChunkDecode::Incomplete => break,
                ChunkDecode::Malformed => {
                    // The framing cannot be repaired by more bytes, so waiting
                    // for them would hold the downstream body open until the
                    // peer closes. End it now, the same way a cut-by-EOF body
                    // ends below.
                    let error = std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "upstream sent malformed chunked framing",
                    );
                    let _ = tx.send(Err(error));
                    return;
                }
            }
        }

        let read_buf = vec![0u8; 4096];
        let BufResult(r, returned) = stream.read(read_buf).await;
        let cut = match r {
            Ok(0) => std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "upstream response ended before its final chunk",
            ),
            Ok(n) => {
                leftover.extend_from_slice(&returned[..n]);
                continue;
            }
            Err(error) => error,
        };
        // Both ways the upstream can stop short end the body the same way.
        let _ = tx.send(Err(cut));
        return;
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

// See the allow on `forward_dispatch` above - same call-chain rationale.
#[allow(clippy::too_many_arguments)]
fn build_request(
    path: &str,
    host: &str,
    app_id: &AppId,
    plan_id: &str,
    request_id: &Uuid,
    body: &[u8],
    user_header: Option<&str>,
    authorization: Option<&str>,
) -> Vec<u8> {
    let user_line = match user_header {
        Some(val) => format!("ZeroShip-User: {val}\r\n"),
        None => String::new(),
    };
    // The peer credential, minted per call by the caller and passed through
    // verbatim. `None` means this process holds no service key, in which case
    // the worker refuses the call - which is the intended outcome, not a
    // degraded one.
    let auth_line = match authorization {
        Some(value) => format!("Authorization: {value}\r\n"),
        None => String::new(),
    };
    let header = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         X-App-Id: {}\r\n\
         X-Plan-Id: {plan_id}\r\n\
         X-Request-Id: {request_id}\r\n\
         {auth_line}\
         {user_line}\
         Connection: keep-alive\r\n\
         \r\n",
        body.len(),
        app_id.as_str()
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

/// A failure reading a worker response's headers, carrying the one fact a
/// caller needs to decide whether the request may be re-sent.
///
/// `consumed` is true once ANY response byte has been read off the socket.
/// The header read is a loop — it accumulates 4 KiB chunks until `httparse`
/// reports `Complete` — so a failure can arrive on the second or tenth
/// iteration, long after the worker proved it had received and processed the
/// request. Collapsing that into a bare error string is what let a partial
/// response be mistaken for a never-delivered one.
struct ReadFailure {
    message: String,
    /// True once at least one byte of the response has been read. A
    /// `consumed` failure is NOT safe to retry: the request reached a
    /// handler, so re-sending it re-runs the handler's side effects.
    consumed: bool,
}

impl std::fmt::Display for ReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Read HTTP response headers from the stream. Returns parsed header info
/// and any trailing bytes that were read past the header boundary.
async fn read_http_headers(stream: &mut Stream) -> Result<ParsedHeaders, ReadFailure> {
    let mut buf = Vec::with_capacity(4096);
    // Flipped the instant the first read extends `buf`, and never cleared.
    let mut consumed = false;

    loop {
        let read_buf = vec![0u8; 4096];
        let BufResult(r, returned) = stream.read(read_buf).await;
        let n = match r {
            Ok(n) => n,
            Err(e) => {
                return Err(ReadFailure { message: e.to_string(), consumed });
            }
        };
        if n == 0 {
            return Err(ReadFailure {
                message: "connection closed before headers complete".to_string(),
                consumed,
            });
        }
        consumed = true;
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
            Err(e) => {
                return Err(ReadFailure { message: format!("parse: {e}"), consumed });
            }
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
    /// A complete chunk was decoded: (data, bytes consumed from the buffer).
    /// data is empty for the final zero-length terminator chunk.
    Complete(Vec<u8>, usize),
    /// Not enough data in the buffer to decode a complete chunk.
    Incomplete,
    /// The framing itself is invalid: no further bytes can make it decodable.
    /// The relay must end the downstream body with an error rather than wait
    /// for a byte that would never arrive.
    Malformed,
}

/// Maximum bytes allowed in a chunk-size line - the hex size plus any chunk
/// extensions - before the framing is [`ChunkDecode::Malformed`].
///
/// RFC 9112 defines `chunk-size = 1*HEXDIG` and leaves chunk extensions
/// unbounded, so a decoder that accumulates until it sees CRLF lets a peer
/// that never sends one grow the relay's buffer without limit while the
/// downstream response stays open. A `usize` is at most 16 hex digits on a
/// 64-bit target, so the size field is tiny; the rest of the line is
/// creator-controlled metadata, the same kind this module already bounds at
/// 8 KiB for `Set-Cookie` ([`MAX_APP_SET_COOKIE_TOTAL_BYTES`]). Applying that
/// bound to a chunk-size line leaves a legitimate extension ample room while
/// classifying a line that runs past it as malformed.
const MAX_CHUNK_SIZE_LINE_BYTES: usize = 8 * 1024;

/// Attempt to decode the next HTTP chunked-encoding frame from `buf`.
///
/// Chunked format: `<hex-size>\r\n<data>\r\n`, terminated by `0\r\n\r\n`.
/// `chunk-size` may carry `;`-delimited chunk extensions, which are ignored.
///
/// Returns [`ChunkDecode::Malformed`] for framing no later bytes can repair: a
/// size line that is not UTF-8, is not hexadecimal, runs past
/// [`MAX_CHUNK_SIZE_LINE_BYTES`] without a CRLF, or names a size whose
/// `chunk_end` would overflow `usize`.
fn decode_next_chunk(buf: &[u8]) -> ChunkDecode {
    // Bound the size line before parsing it. When no CRLF has arrived yet the
    // whole buffer is a size line still in progress; once it exceeds the
    // bound no later CRLF can make it valid.
    let Some(line_len) = find_crlf(buf) else {
        return if buf.len() > MAX_CHUNK_SIZE_LINE_BYTES {
            ChunkDecode::Malformed
        } else {
            ChunkDecode::Incomplete
        };
    };
    if line_len > MAX_CHUNK_SIZE_LINE_BYTES {
        return ChunkDecode::Malformed;
    }

    // Parse hex size
    let Ok(size_str) = std::str::from_utf8(&buf[..line_len]) else {
        return ChunkDecode::Malformed;
    };
    let size_str = size_str.trim();
    // Strip chunk extensions (anything after ';')
    let size_hex = size_str.split(';').next().unwrap_or("").trim();
    let Ok(chunk_size) = usize::from_str_radix(size_hex, 16) else {
        return ChunkDecode::Malformed;
    };

    // Total bytes for this chunk: size_line + \r\n + data + \r\n. A huge
    // declared size overflows `usize`; classify that as malformed instead of
    // wrapping (a wrong parse) or panicking in a debug build.
    let data_start = line_len + 2; // past the first \r\n
    let Some(chunk_end) = data_start.checked_add(chunk_size).and_then(|end| end.checked_add(2))
    else {
        return ChunkDecode::Malformed;
    };

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

#[cfg(test)]
mod chunk_decode_tests {
    //! Framing classification for the chunked decoder. The malformed cases
    //! here must not be mistaken for "need more bytes": that classification
    //! holds the downstream body open until the peer closes.

    use super::*;

    #[test]
    fn a_non_hex_size_is_malformed() {
        assert!(matches!(decode_next_chunk(b"zz\r\n"), ChunkDecode::Malformed));
    }

    #[test]
    fn a_non_utf8_size_line_is_malformed() {
        assert!(matches!(
            decode_next_chunk(&[0xff, 0xfe, b'\r', b'\n']),
            ChunkDecode::Malformed
        ));
    }

    #[test]
    fn an_overflowing_size_is_malformed() {
        // `usize::MAX` parses as hex but leaves no room for the two CRLF bytes
        // after the data, so `chunk_end` overflows. An unchecked sum would
        // panic in a debug build and wrap in release; the checked add must
        // reject it.
        let line = format!("{:x}\r\n", usize::MAX);
        assert!(matches!(
            decode_next_chunk(line.as_bytes()),
            ChunkDecode::Malformed
        ));
    }

    #[test]
    fn a_size_line_past_the_bound_is_malformed() {
        // No CRLF, so the whole buffer is a size line still in progress; past
        // the bound no later CRLF can make it valid.
        let buf = vec![b'a'; MAX_CHUNK_SIZE_LINE_BYTES + 1];
        assert!(matches!(decode_next_chunk(&buf), ChunkDecode::Malformed));
    }

    #[test]
    fn a_size_line_at_the_bound_without_crlf_is_incomplete() {
        // The rejection control: the bound is inclusive. A line that has
        // reached exactly the bound may still be completed by the CRLF that
        // follows, so it is not yet malformed.
        let buf = vec![b'a'; MAX_CHUNK_SIZE_LINE_BYTES];
        assert!(matches!(decode_next_chunk(&buf), ChunkDecode::Incomplete));
    }

    #[test]
    fn a_size_line_with_chunk_extensions_parses() {
        // Control: a well-formed extension line still yields its chunk.
        match decode_next_chunk(b"5;ext=1\r\nhello\r\n") {
            ChunkDecode::Complete(data, consumed) => {
                assert_eq!(data, b"hello");
                assert_eq!(consumed, 16);
            }
            ChunkDecode::Incomplete => panic!("a complete chunk was reported incomplete"),
            ChunkDecode::Malformed => panic!("a chunk-extension line was reported malformed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Generic HTTP proxy — used for `auth.zeroship.ai` → op / crates/auth
// ---------------------------------------------------------------------------
//
// Unlike `forward_to_worker_dispatch`, which packages requests into a JSON
// envelope for the worker kernel, this path is a transparent HTTP/1.1
// reverse proxy: it preserves the request method, path+query, headers,
// and body verbatim, then streams the response back. Set-Cookie and
// Location headers MUST pass through untouched — OP's session cookie
// (scoped to `auth.zeroship.ai`) and its OAuth2 302 redirects depend on
// the client seeing them as if they came directly from op.

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
/// (scheme/host/optional-port, e.g., `http://op:4444`); the inbound
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
        compio::runtime::spawn(relay_chunked(stream, parsed.trailing, tx)).detach();
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
            extract_host_with_port("http://op:4444"),
            "op:4444"
        );
    }

    #[test]
    fn extract_host_with_port_omits_default_port() {
        // The url crate normalizes :80 / :443 away when they're the
        // default for the scheme — that's fine for forwarding because
        // upstream will still resolve the host.
        assert_eq!(extract_host_with_port("http://op"), "op");
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
        let req = build_forward_request("POST", "/oauth2/token", "op:4444", &headers, body);
        let s = String::from_utf8(req).unwrap();

        assert!(s.starts_with("POST /oauth2/token HTTP/1.1\r\n"));
        assert!(s.contains("Host: op:4444\r\n"));
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

#[cfg(test)]
mod pooled_retry_tests {
    //! Retry-on-pooled-connection-failure safety.
    //!
    //! `forward_to_worker_path` retries a request when the read of the
    //! response headers fails on a connection that came out of the keep-alive
    //! pool. That retry is correct for the half-open race (the peer had
    //! already closed; the request never reached a handler) and WRONG once
    //! the worker has sent us any response bytes — those bytes prove the
    //! worker parsed the request and ran the handler, so re-sending a
    //! non-idempotent POST executes the mutation a second time.
    //!
    //! These tests drive the real socket path: a live `compio` TCP listener
    //! plays the worker, and the assertion is on HOW MANY TIMES THE WORKER
    //! RECEIVED THE REQUEST — not on any internal flag of the read helper.

    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    /// What the mock worker does with a request once it has read one.
    #[derive(Clone, Copy)]
    enum MockBehavior {
        /// Send a status line + one header and then close, WITHOUT the
        /// terminating blank line. `httparse` therefore reports
        /// `Status::Partial`, the loop iterates, and the next read returns 0.
        /// Bytes have been consumed by the time the error is produced.
        PartialThenClose,
        /// Read the request off the wire and close WITHOUT writing a single
        /// response byte and WITHOUT counting it as received — the half-open
        /// keep-alive race, where the pooled socket's peer was already gone
        /// and the bytes we wrote were discarded, never reaching a handler.
        ///
        /// Draining the request rather than closing on accept is a
        /// determinism device: closing first would race the client's write
        /// (an RST could fail `write_all` and send the code down the
        /// pre-read reconnect at the top of `forward_to_worker_path`
        /// instead of the read-failure arm this test is about). Draining
        /// guarantees the client's write succeeds and the failure surfaces
        /// on the read, with zero bytes consumed.
        DrainThenClose,
        /// A complete, well-formed response.
        FullResponse,
    }

    /// True once `buf` holds a complete HTTP request (headers + the body
    /// length its `Content-Length` declares).
    fn request_complete(buf: &[u8]) -> bool {
        let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let head = String::from_utf8_lossy(&buf[..pos]);
        let content_length = head
            .lines()
            .find_map(|line| {
                let (k, v) = line.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);
        buf.len() >= pos + 4 + content_length
    }

    async fn serve_one(
        mut stream: compio::net::TcpStream,
        behavior: MockBehavior,
        counter: Arc<AtomicUsize>,
    ) {
        let mut acc: Vec<u8> = Vec::new();
        while !request_complete(&acc) {
            let buf = vec![0u8; 4096];
            let BufResult(r, buf) = compio::io::AsyncRead::read(&mut stream, buf).await;
            match r {
                Ok(0) | Err(_) => return,
                Ok(n) => acc.extend_from_slice(&buf[..n]),
            }
        }

        if matches!(behavior, MockBehavior::DrainThenClose) {
            // Bytes discarded, no handler ran, nothing written back.
            return;
        }

        // Count only AFTER a full request has arrived: this is the moment a
        // real worker would have parsed it and run the handler (committing
        // whatever writes it makes).
        counter.fetch_add(1, Ordering::SeqCst);

        let response: Vec<u8> = match behavior {
            MockBehavior::PartialThenClose => {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n".to_vec()
            }
            MockBehavior::FullResponse => {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec()
            }
            MockBehavior::DrainThenClose => unreachable!(),
        };
        let _ = compio::io::AsyncWriteExt::write_all(&mut stream, response).await;
        // Drop -> FIN. The client's follow-up read returns 0.
    }

    /// Start a mock worker. `behaviors[i]` governs the i-th accepted
    /// connection; connections past the end of the slice reuse the last
    /// entry. Returns (worker_url, request_counter).
    async fn start_mock_worker(behaviors: Vec<MockBehavior>) -> (String, Arc<AtomicUsize>) {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock worker");
        let addr = listener.local_addr().expect("mock worker local addr");
        let worker_url = format!("http://{addr}");
        let counter = Arc::new(AtomicUsize::new(0));
        let accept_counter = Arc::clone(&counter);

        compio::runtime::spawn(async move {
            let mut conn_idx = 0usize;
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    break;
                };
                let behavior = behaviors[conn_idx.min(behaviors.len() - 1)];
                conn_idx += 1;
                let c = Arc::clone(&accept_counter);
                compio::runtime::spawn(serve_one(stream, behavior, c)).detach();
            }
        })
        .detach();

        (worker_url, counter)
    }

    /// Put a live connection to `worker_url` into the thread-local keep-alive
    /// pool, exactly as a completed prior request would have. The next
    /// `forward_to_worker_path` for that URL takes it, so `from_pool` is true.
    async fn seed_pool(worker_url: &str) {
        let (stream, _, _) = connect(worker_url).await.expect("seed pool connect");
        CONN_POOL.with(|p| p.borrow_mut().put(pool_key(worker_url), stream));
    }

    async fn dispatch(worker_url: &str) -> Result<HttpResponse, String> {
        let app_id = AppId::mint();
        let request_id = Uuid::new_v4();
        forward_to_worker_path(
            worker_url,
            "/dispatch/test",
            &app_id,
            "plan_test",
            &request_id,
            br#"{"op":"charge"}"#,
            None,
            None,
        )
        .await
    }

    #[compio::test]
    async fn partial_response_on_pooled_connection_is_not_replayed() {
        // The worker received the POST, ran the handler (any DB writes are
        // already committed), emitted part of the status line, then died.
        // The gateway must NOT re-send that POST to a fresh worker: the
        // mutation would execute twice.
        //
        // Connection 0 is the seeded pooled one and answers partially; every
        // later connection would be a REPLAY, so it answers fully to make the
        // replay visibly "succeed" rather than error out for its own reasons.
        let (worker_url, counter) = start_mock_worker(vec![
            MockBehavior::PartialThenClose,
            MockBehavior::FullResponse,
        ])
        .await;
        seed_pool(&worker_url).await;

        let _ = dispatch(&worker_url).await;

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "the worker must receive the non-idempotent request EXACTLY ONCE; \
             it received it {} times, so the mutation ran twice",
            counter.load(Ordering::SeqCst)
        );
    }

    #[compio::test]
    async fn partial_response_on_pooled_connection_surfaces_an_error() {
        // Companion to the count assertion: with the replay suppressed the
        // call must fail rather than silently return a bogus response.
        let (worker_url, _counter) = start_mock_worker(vec![
            MockBehavior::PartialThenClose,
            MockBehavior::FullResponse,
        ])
        .await;
        seed_pool(&worker_url).await;

        let result = dispatch(&worker_url).await;

        assert!(
            result.is_err(),
            "a truncated worker response must surface as an error, not a response"
        );
    }

    #[compio::test]
    async fn half_open_pooled_connection_is_still_retried() {
        // Guard on the OTHER side of the predicate: when zero response bytes
        // were read, the request never reached a handler and the retry is
        // exactly right. That behaviour must survive the fix — a fix that
        // simply deleted the retry would pass the test above and fail here.
        //
        // Connection 0 (the pooled one) discards the request and closes;
        // connection 1 is the retry and answers fully.
        let (worker_url, counter) = start_mock_worker(vec![
            MockBehavior::DrainThenClose,
            MockBehavior::FullResponse,
        ])
        .await;
        seed_pool(&worker_url).await;

        let result = dispatch(&worker_url).await;

        assert!(
            result.is_ok(),
            "a half-open pooled connection must be retried transparently: {result:?}",
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "exactly one worker should have actually received the request"
        );
    }
}

#[cfg(test)]
mod chunked_relay_tests {
    //! What the gateway forwards when a worker's chunked body is cut short.
    //!
    //! A worker whose response producer fails part-way drops the connection
    //! without the terminating zero-length chunk. A live `compio` listener
    //! plays that worker here, and the assertion is on the body the gateway
    //! hands its own client: the chunk that arrived, then an error - never a
    //! clean end, which would present the prefix as the whole body.

    use super::*;
    use ntex::http::body::{Body, MessageBody, ResponseBody};

    const HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

    /// A worker that reads one whole request, answers with `body` after a
    /// chunked head, and closes.
    async fn worker_answering(body: &'static [u8]) -> String {
        worker_answering_inner(body.to_vec(), false).await
    }

    /// A worker that answers with `body` after a chunked head and then holds
    /// the connection open, reading until the gateway closes it. This is the
    /// peer the malformed-framing tests need: it never sends EOF on its own,
    /// so the relay must classify the framing itself instead of waiting for a
    /// close that never comes.
    async fn worker_answering_holding_open(body: Vec<u8>) -> String {
        worker_answering_inner(body, true).await
    }

    async fn worker_answering_inner(body: Vec<u8>, hold_open: bool) -> String {
        let listener = compio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the worker");
        let url = format!("http://{}", listener.local_addr().expect("its address"));
        compio::runtime::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            // Read the whole request first, so closing afterwards is a FIN
            // and never a reset that could discard the answer.
            let mut request = Vec::new();
            loop {
                let BufResult(read, buf) =
                    compio::io::AsyncRead::read(&mut stream, vec![0u8; 4096]).await;
                match read {
                    Ok(0) | Err(_) => return,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
                let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break;
                }
            }
            let mut answer = HEAD.to_vec();
            answer.extend_from_slice(&body);
            let _ = compio::io::AsyncWriteExt::write_all(&mut stream, answer).await;
            if hold_open {
                // No FIN: keep the connection open until the gateway closes
                // it. The relay's classification of the framing is what ends
                // the exchange, never a peer close.
                loop {
                    let BufResult(read, _) =
                        compio::io::AsyncRead::read(&mut stream, vec![0u8; 4096]).await;
                    if matches!(read, Ok(0) | Err(_)) {
                        return;
                    }
                }
            }
        })
        .detach();
        url
    }

    /// Every item of the body the gateway serves, until its end.
    async fn relayed(worker_url: &str) -> Vec<Result<Vec<u8>, String>> {
        let mut response = forward_to_worker_path(
            worker_url,
            "/dispatch/test",
            &AppId::mint(),
            "plan_test",
            &Uuid::new_v4(),
            b"{}",
            None,
            None,
        )
        .await
        .expect("the worker answered its head");
        let mut body: ResponseBody<Body> = response.take_body();
        compio::time::timeout(Duration::from_secs(10), async {
            let mut items = Vec::new();
            while let Some(item) = std::future::poll_fn(|cx| body.poll_next_chunk(cx)).await {
                items.push(item.map(|bytes| bytes.to_vec()).map_err(|error| error.to_string()));
            }
            items
        })
        .await
        .expect("the relayed body ends")
    }

    /// Two cuts: the connection closing straight after a chunk, and what the
    /// worker's HTTP server actually sends when a body fails - its own error
    /// response head where the next chunk would go, then the close.
    #[compio::test]
    async fn a_chunked_body_cut_short_reaches_the_client_as_an_error() {
        for cut in [
            &b"5\r\nhello\r\n"[..],
            b"5\r\nhello\r\nHTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        ] {
            let items = relayed(&worker_answering(cut).await).await;
            assert_eq!(items.len(), 2, "the chunk, then the end of the body: {items:?}");
            assert_eq!(items[0], Ok(b"hello".to_vec()));
            assert!(
                items[1].is_err(),
                "a body that lost its final chunk must end in an error, not cleanly: {items:?}"
            );
        }
    }

    /// The rejection control: the same body WITH its final chunk ends
    /// cleanly, so the case above is about the missing chunk alone.
    #[compio::test]
    async fn a_complete_chunked_body_ends_cleanly() {
        let items = relayed(&worker_answering(b"5\r\nhello\r\n0\r\n\r\n").await).await;
        assert_eq!(items, vec![Ok(b"hello".to_vec())]);
    }

    /// A peer that writes a size line which is not hexadecimal and keeps the
    /// connection open must make the relay end its body in error promptly -
    /// not wait for the close that never comes. The bounded wait is `relayed`'s
    /// own timeout around the poll loop.
    #[compio::test]
    async fn a_non_hex_size_line_held_open_ends_the_relay_with_an_error() {
        let items = relayed(&worker_answering_holding_open(b"zz\r\n".to_vec()).await).await;
        assert_eq!(items.len(), 1, "only the framing error, no chunk: {items:?}");
        assert!(
            items[0].is_err(),
            "a non-hex size line must end the body in error, not hold it open: {items:?}"
        );
    }

    /// Same, for a size line that is not UTF-8.
    #[compio::test]
    async fn a_non_utf8_size_line_held_open_ends_the_relay_with_an_error() {
        let items =
            relayed(&worker_answering_holding_open(vec![0xff, 0xfe, b'\r', b'\n']).await).await;
        assert_eq!(items.len(), 1, "only the framing error, no chunk: {items:?}");
        assert!(
            items[0].is_err(),
            "a non-UTF-8 size line must end the body in error, not hold it open: {items:?}"
        );
    }

    /// A size line that never sends its CRLF and runs past the bound must end
    /// the body in error rather than grow the relay's buffer without limit.
    #[compio::test]
    async fn a_size_line_past_the_bound_held_open_ends_the_relay_with_an_error() {
        let items = relayed(
            &worker_answering_holding_open(vec![b'a'; MAX_CHUNK_SIZE_LINE_BYTES + 1]).await,
        )
        .await;
        assert_eq!(items.len(), 1, "only the framing error, no chunk: {items:?}");
        assert!(
            items[0].is_err(),
            "a size line past the bound must end the body in error, not hold it open: {items:?}"
        );
    }
}
