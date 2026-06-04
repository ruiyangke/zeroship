//! Reusable compio HTTP server for zeroship.
//!
//! Extracted from `server.rs` (the zeroship-bench-server binary) so that both the
//! benchmark binary and the CLI (`zeroship serve`) can share the same server
//! logic.
//!
//! ## Usage
//!
//! ```ignore
//! use zeroship_runtime::serve::{start_server, ServerOptions};
//! use zeroship_runtime::ModuleEntry;
//!
//! let modules = vec![ModuleEntry { specifier: "index.js".into(), source: "...".into() }];
//! start_server(modules, ServerOptions { port: 3000, ..Default::default() });
//! ```

#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

use crate::channel::CancelFlag;
use crate::init::init_v8;
use crate::modules::ModuleEntry;
use crate::plugin::NativePlugin;
use crate::runtime::{Runtime, RuntimeLimits};
use crate::{EnvSnapshot, FetchOutcome, RequestCtx, SettledFetch};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use futures::{pin_mut, FutureExt};


// ===========================================================================
// Public API
// ===========================================================================

/// Configuration for the compio HTTP server.
#[derive(Clone)]
pub struct ServerOptions {
    pub port: u16,
    /// Number of worker threads. 0 = auto-detect from available parallelism.
    pub workers: usize,
    /// Per-request CPU time limit (enforced by V8 interrupt).
    pub cpu_limit: Option<Duration>,
    /// Per-request wall-clock timeout.
    pub wall_timeout: Option<Duration>,
    /// V8 heap limit in bytes (per worker). `None` → runtime default (128 MB).
    /// Dev callers typically raise this to 256-512 MB for app bundles that
    /// pull in heavy dependencies (LangChain, SDKs, etc).
    pub heap_limit_bytes: Option<usize>,
    /// Env vars exposed to JS as `process.env.*`. Cloned into each worker.
    pub env_vars: HashMap<String, String>,
    /// Native plugins to register on each worker's `zeroship.*` namespace.
    /// Each worker thread gets its own `Runtime`, so each plugin instance
    /// is cloned (via `Arc`) into every worker.
    pub plugins: Vec<Arc<dyn NativePlugin>>,
}

impl std::fmt::Debug for ServerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerOptions")
            .field("port", &self.port)
            .field("workers", &self.workers)
            .field("cpu_limit", &self.cpu_limit)
            .field("wall_timeout", &self.wall_timeout)
            .field("plugins", &self.plugins.len())
            .finish()
    }
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            port: 3000,
            workers: 0,
            cpu_limit: None,
            wall_timeout: None,
            heap_limit_bytes: None,
            env_vars: HashMap::new(),
            plugins: Vec::new(),
        }
    }
}

/// Start the compio HTTP server. This function blocks forever.
///
/// - Single worker: runs on the calling thread.
/// - Multi-worker: spawns N threads with SO_REUSEPORT, then joins them all.
pub fn start_server(modules: Vec<ModuleEntry>, options: ServerOptions) -> ! {
    init_v8();

    let num_workers = if options.workers == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    } else {
        options.workers
    };

    if options.cpu_limit.is_some() || options.wall_timeout.is_some() {
        tracing::info!(
            cpu_limit = ?options.cpu_limit,
            wall_timeout = ?options.wall_timeout,
            "runtime limits configured"
        );
    }

    if num_workers <= 1 {
        run_single_worker(
            options.port,
            false,
            None,
            options.cpu_limit,
            options.wall_timeout,
            options.heap_limit_bytes,
            modules,
            options.env_vars,
            options.plugins,
        );
    } else {
        tracing::info!(workers = num_workers, port = options.port, "runtime spawning workers");
        let mut handles = Vec::new();
        for i in 0..num_workers {
            let worker_modules = modules.clone();
            let cpu_limit = options.cpu_limit;
            let wall_timeout = options.wall_timeout;
            let heap_limit_bytes = options.heap_limit_bytes;
            let port = options.port;
            let worker_env = options.env_vars.clone();
            let worker_plugins = options.plugins.clone();
            let handle = std::thread::Builder::new()
                .name(format!("worker-{i}"))
                .spawn(move || {
                    run_single_worker(
                        port,
                        true,
                        Some(i),
                        cpu_limit,
                        wall_timeout,
                        heap_limit_bytes,
                        worker_modules,
                        worker_env,
                        worker_plugins,
                    );
                })
                .unwrap();
            handles.push(handle);
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    // The server loop never returns, but if it somehow does (all workers crashed):
    std::process::exit(1);
}

async fn recv_with_timeout<T>(
    rx: &crate::channel::ResultReceiver<T>,
    timeout: Option<Duration>,
    cancel: &crate::channel::CancelFlag,
    handle: &Runtime,
) -> Option<T> {
    if let Some(limit) = timeout {
        let recv = rx.recv().fuse();
        let sleep = compio::time::sleep(limit).fuse();
        pin_mut!(recv, sleep);
        futures::select! {
            result = recv => Some(result),
            _ = sleep => {
                cancel.cancel();
                handle.notify_pump();
                None
            }
        }
    } else {
        Some(rx.recv().await)
    }
}

// ===========================================================================
// Static responses
// ===========================================================================

const HEALTH_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}";
const HEADERS_TOO_LARGE_RESPONSE: &[u8] =
    b"HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const PAYLOAD_TOO_LARGE_RESPONSE: &[u8] =
    b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const BAD_REQUEST_RESPONSE: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

// ---------------------------------------------------------------------------
// Per-connection limits
//
// The standalone server accepts arbitrary HTTP traffic, so every input path
// needs an explicit cap. Without these, a slow/malicious client can pin
// memory and a compio task indefinitely:
// - no header cap → unlimited buffer growth before httparse decides it's
//   "complete" or gives up
// - no body cap → a `Content-Length: 999999999999` lie forces us to wait
//   for bytes that will never arrive, holding the scratch buffer open
// - no connection cap → slowloris-style drip attacks keep `data` growing
//   even across partial reads
//
// Limits are deliberately static (not configurable) — the standalone server
// is a dev/benchmark entrypoint; operators who need different numbers deploy
// the worker+gateway where the gateway enforces its own rules.
// ---------------------------------------------------------------------------

/// Max bytes in the HTTP request line + headers. Matches nginx's default
/// `large_client_header_buffers`. Triggers 431 Request Header Fields Too Large.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Max bytes in a single request body. Covers JSON-RPC dispatch and the
/// HTTP envelope from the gateway. Triggers 413 Content Too Large.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Max bytes buffered in `data` before we give up waiting for the request
/// to complete. Mostly defense against slowloris: a client that writes one
/// byte per second but never finishes the headers would otherwise force us
/// to keep the connection open forever.
const MAX_CONNECTION_BUFFER_BYTES: usize = MAX_HEADER_BYTES + MAX_BODY_BYTES + 4096;

/// Idle ceiling for a streaming (chunked / SSE) response body. If the
/// handler produces no chunk and does not finish within this window, the
/// pump closes the connection and releases the worker task.
///
/// This is *idle*, not wall: every emitted chunk resets the clock, so a
/// legitimate long-lived SSE/LLM stream that keeps producing tokens runs
/// indefinitely. The cap only fires on a stream that stalls — e.g. a
/// handler stuck in an infinite `await` that never enqueues or closes —
/// which otherwise pins the TCP connection and its compio task forever
/// (resource-exhaustion DoS). 5 minutes is generous: it comfortably
/// exceeds keepalive/heartbeat intervals of every SSE client we target
/// while still bounding a wedged stream.
///
/// Static, like the other per-connection limits above: the standalone
/// server is a dev/bench entrypoint; the worker+gateway deployment
/// enforces its own stream bounds.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

// ===========================================================================
// HTTP connection handler
// ===========================================================================

async fn handle_connection(
    mut stream: TcpStream,
    runtime: Runtime,
) {
    let mut data = Vec::with_capacity(8192);
    let mut read_buf = Vec::with_capacity(4096);

    loop {
        read_buf.clear();
        let BufResult(result, returned_buf) = stream.read(read_buf).await;
        read_buf = returned_buf;

        let n = match result {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };

        // Enforce the per-connection buffer cap before appending. A client
        // that keeps sending bytes without completing a request would
        // otherwise grow `data` without bound.
        if data.len().saturating_add(n) > MAX_CONNECTION_BUFFER_BYTES {
            let _ = stream.write_all(PAYLOAD_TOO_LARGE_RESPONSE.to_vec()).await;
            return;
        }

        data.extend_from_slice(&read_buf[..n]);

        let mut consumed = 0;

        loop {
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut req = httparse::Request::new(&mut headers);

            let header_len = match req.parse(&data[consumed..]) {
                Ok(httparse::Status::Complete(len)) => len,
                Ok(httparse::Status::Partial) => {
                    // Reject before we commit more memory — if the unparsed
                    // slice is already over the header cap we're never going
                    // to accept this request.
                    if data.len() - consumed > MAX_HEADER_BYTES {
                        let _ = stream.write_all(HEADERS_TOO_LARGE_RESPONSE.to_vec()).await;
                        return;
                    }
                    break;
                }
                Err(_) => {
                    let _ = stream.write_all(BAD_REQUEST_RESPONSE.to_vec()).await;
                    return;
                }
            };

            if header_len > MAX_HEADER_BYTES {
                let _ = stream.write_all(HEADERS_TOO_LARGE_RESPONSE.to_vec()).await;
                return;
            }

            let method = req.method.unwrap_or("GET");
            let path = req.path.unwrap_or("/");

            // Determine body framing: Content-Length, Transfer-Encoding:
            // chunked, or no body. An earlier revision only supported
            // Content-Length and silently mis-parsed chunked requests
            // (treating Content-Length=0 as "body-less" and then parsing
            // the body bytes as the next pipelined request).
            let is_chunked = headers.iter().any(|h| {
                h.name.eq_ignore_ascii_case("transfer-encoding")
                    && std::str::from_utf8(h.value)
                        .map(|v| v.to_ascii_lowercase().contains("chunked"))
                        .unwrap_or(false)
            });
            let content_length: usize = headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                .and_then(|h| std::str::from_utf8(h.value).ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);

            if content_length > MAX_BODY_BYTES {
                // Trust the declared length enough to reject early —
                // waiting for the full body to arrive just so we can reject
                // it afterwards would defeat the purpose of the cap.
                let _ = stream.write_all(PAYLOAD_TOO_LARGE_RESPONSE.to_vec()).await;
                return;
            }

            // For chunked requests, decode chunks out of `data` starting at
            // `consumed + header_len` into an owned `Vec<u8>`. For
            // Content-Length requests, we borrow from `data` directly.
            //
            // We pre-allocate an owned buffer only on the chunked path so
            // the hot Content-Length path — which handles essentially all
            // traffic — stays alloc-free. Both paths end up with a byte
            // slice the dispatch code can read; the owned Vec is kept in
            // scope below to back the slice.
            let chunked_body_buf: Vec<u8>;
            let (body_bytes, total_input_consumed): (&[u8], usize) = if is_chunked {
                match decode_chunked_body(&data[consumed + header_len..], MAX_BODY_BYTES) {
                    ChunkedDecode::Complete { body_bytes, consumed_input } => {
                        chunked_body_buf = body_bytes;
                        (chunked_body_buf.as_slice(), header_len + consumed_input)
                    }
                    ChunkedDecode::Incomplete => break,
                    ChunkedDecode::TooLarge => {
                        let _ = stream.write_all(PAYLOAD_TOO_LARGE_RESPONSE.to_vec()).await;
                        return;
                    }
                    ChunkedDecode::Invalid => {
                        let _ = stream.write_all(BAD_REQUEST_RESPONSE.to_vec()).await;
                        return;
                    }
                }
            } else {
                let total_len = header_len + content_length;
                if data.len() - consumed < total_len {
                    break;
                }
                (&data[consumed + header_len..consumed + total_len], total_len)
            };

            let total_len = total_input_consumed;

            // `/health` is the only kernel-level route — a liveness
            // probe for process managers, served without touching V8.
            // Every other request flows through `handle_request` →
            // `call_fetch_handler`, which dispatches to default.rpc /
            // default.fetchFast / default.fetch in that order. URL
            // routing within those tiers is user-space.
            if method == "GET" && path == "/health" {
                let BufResult(write_result, _) = stream.write_all(HEALTH_RESPONSE.to_vec()).await;
                if write_result.is_err() { return; }
            } else {
                let body_str = std::str::from_utf8(body_bytes).unwrap_or("");
                let host = headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("host"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .unwrap_or("localhost");
                let full_url = format!("http://{}{}", host, path);

                // Materialize request headers as `(name, value)` pairs once —
                // reused both for the fetch handler (as its request headers)
                // and for the WebSocket handshake if this turns into an upgrade.
                let request_headers: Vec<(String, String)> = headers.iter()
                    .filter(|h| !h.name.is_empty())
                    .map(|h| (
                        h.name.to_string(),
                        std::str::from_utf8(h.value).unwrap_or("").to_string(),
                    ))
                    .collect();

                // Check for WebSocket upgrade BEFORE dispatch — after a
                // successful upgrade the stream is no longer HTTP, so we
                // must not loop back to parse another request out of it.
                let is_upgrade = request_headers.iter().any(|(name, value)|
                    name.eq_ignore_ascii_case("upgrade") &&
                    value.eq_ignore_ascii_case("websocket")
                );

                let wrote_ok = handle_request(
                    &mut stream, method, &full_url, &request_headers, body_str, &runtime,
                ).await;
                if !wrote_ok { return; }
                if is_upgrade { return; }
            }

            consumed += total_len;

            if consumed >= data.len() {
                break;
            }
        }

        if consumed >= data.len() {
            data.clear();
        } else if consumed > 0 {
            data.drain(..consumed);
        }
    }
}

// ===========================================================================
// Response builders
// ===========================================================================

fn build_http_response(status: u16, headers: &[(String, String)], body: &str) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "OK",
    };

    let mut buf = Vec::with_capacity(256 + body.len());
    buf.extend_from_slice(b"HTTP/1.1 ");
    let mut status_buf = itoa::Buffer::new();
    buf.extend_from_slice(status_buf.format(status).as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(status_text.as_bytes());
    buf.extend_from_slice(b"\r\n");

    let mut has_content_length = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            has_content_length = true;
        }
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    if !has_content_length {
        buf.extend_from_slice(b"Content-Length: ");
        let mut len_buf = itoa::Buffer::new();
        buf.extend_from_slice(len_buf.format(body.len()).as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf.extend_from_slice(b"\r\n");
    buf.extend_from_slice(body.as_bytes());
    buf
}

/// Stream an HTTP/1.1 chunked response body from `reader` to `stream`.
///
/// Called after the response headers (including `Transfer-Encoding: chunked`)
/// have already been written. Loops until the reader reports `is_done`, then
/// writes the `0\r\n\r\n` terminator.
///
/// Allocation: one `Vec<u8>` for the whole response. Previously every chunk
/// allocated `String` (from `format!`) + `Vec` (for framing) — one alloc pair
/// per token on SSE/LLM-streaming endpoints. The scratch buffer is returned
/// by `write_all` via compio's ownership-transfer model, cleared (keeping
/// capacity) and reused for the next chunk, so after the first chunk
/// steady-state allocation is zero.
/// Outcome of attempting to decode a chunked request body in place.
enum ChunkedDecode {
    /// Body decoded successfully. `body_bytes` holds the concatenated
    /// chunk payloads, `consumed_input` tells the caller how many input
    /// bytes were consumed (including framing, so it can slice past them).
    Complete { body_bytes: Vec<u8>, consumed_input: usize },
    /// Not enough input to finish a chunk or the terminator. Caller should
    /// read more bytes and retry — no state mutated.
    Incomplete,
    /// Decoded body would exceed `max_bytes`. Send 413 and close.
    TooLarge,
    /// Framing malformed (bad chunk size line, non-hex digits, etc).
    /// Send 400 and close.
    Invalid,
}

/// Decode an HTTP/1.1 chunked-transfer body into a plain byte sequence.
///
/// Grammar (RFC 9112 §7.1):
///   chunked-body = *chunk last-chunk trailer-section CRLF
///   chunk        = chunk-size [ chunk-ext ] CRLF chunk-data CRLF
///   chunk-size   = 1*HEXDIG
///   last-chunk   = 1*("0") [ chunk-ext ] CRLF
///
/// We ignore chunk extensions (everything on the size line after `;`) and
/// trailers (we accept an empty trailer section only). That matches what
/// every real client sends — no browser or SDK uses chunk extensions.
fn decode_chunked_body(input: &[u8], max_bytes: usize) -> ChunkedDecode {
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0;

    loop {
        // Find end of chunk-size line (\r\n).
        let Some(line_end) = find_crlf(&input[pos..]) else {
            return ChunkedDecode::Incomplete;
        };
        let size_line = &input[pos..pos + line_end];
        // Strip chunk extensions if present.
        let size_field = match size_line.iter().position(|&b| b == b';') {
            Some(p) => &size_line[..p],
            None => size_line,
        };
        let size_str = match std::str::from_utf8(size_field) {
            Ok(s) => s.trim(),
            Err(_) => return ChunkedDecode::Invalid,
        };
        let chunk_size = match usize::from_str_radix(size_str, 16) {
            Ok(n) => n,
            Err(_) => return ChunkedDecode::Invalid,
        };
        pos += line_end + 2; // past the CRLF

        if chunk_size == 0 {
            // last-chunk. Accept one more CRLF to close the trailer section.
            if input.len() < pos + 2 {
                return ChunkedDecode::Incomplete;
            }
            if &input[pos..pos + 2] != b"\r\n" {
                // Non-empty trailers aren't supported; walk the trailer
                // lines until we hit the empty one. Every real trailer
                // ends with CRLFCRLF.
                while pos < input.len() {
                    let Some(tlen) = find_crlf(&input[pos..]) else {
                        return ChunkedDecode::Incomplete;
                    };
                    pos += tlen + 2;
                    if tlen == 0 {
                        break;
                    }
                }
            } else {
                pos += 2;
            }
            return ChunkedDecode::Complete {
                body_bytes: out,
                consumed_input: pos,
            };
        }

        if out.len().saturating_add(chunk_size) > max_bytes {
            return ChunkedDecode::TooLarge;
        }
        // Need chunk data + trailing CRLF fully in buffer.
        if input.len() < pos + chunk_size + 2 {
            return ChunkedDecode::Incomplete;
        }
        out.extend_from_slice(&input[pos..pos + chunk_size]);
        pos += chunk_size;
        if &input[pos..pos + 2] != b"\r\n" {
            return ChunkedDecode::Invalid;
        }
        pos += 2;
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Outcome of waiting for the next streaming-body event.
#[derive(Debug, PartialEq, Eq)]
enum StreamWait {
    /// Data is available or the stream finished — drain and continue.
    Ready,
    /// The idle ceiling elapsed with no chunk and no completion. The pump
    /// must stop and release the connection.
    IdleTimeout,
}

/// Await the next chunk (or stream completion), bounded by `idle_timeout`.
///
/// Races `reader.wait_for_data()` against a `compio::time::sleep`. Returns
/// `Ready` the instant data arrives or the writer signals done/overflow;
/// returns `IdleTimeout` if neither happens before the deadline. Extracted
/// from the pump loop so the idle bound is unit-testable without a socket.
async fn wait_for_data_or_idle(
    reader: &crate::channel::StreamReader,
    idle_timeout: Duration,
) -> StreamWait {
    let data = reader.wait_for_data().fuse();
    let sleep = compio::time::sleep(idle_timeout).fuse();
    pin_mut!(data, sleep);
    futures::select! {
        _ = data => StreamWait::Ready,
        _ = sleep => StreamWait::IdleTimeout,
    }
}

async fn stream_chunked_body(
    stream: &mut TcpStream,
    reader: crate::channel::StreamReader,
) -> bool {
    stream_chunked_body_with_idle(stream, reader, STREAM_IDLE_TIMEOUT).await
}

async fn stream_chunked_body_with_idle(
    stream: &mut TcpStream,
    reader: crate::channel::StreamReader,
    idle_timeout: Duration,
) -> bool {
    use std::io::Write as _;

    let mut scratch: Vec<u8> = Vec::with_capacity(4096);
    loop {
        while let Some(chunk) = reader.pop() {
            scratch.clear();
            // Writing into a `Vec<u8>` via `std::io::Write` doesn't heap-
            // allocate — the formatter goes straight through `extend_from_slice`
            // on the existing capacity.
            let _ = write!(scratch, "{:x}\r\n", chunk.len());
            scratch.extend_from_slice(&chunk);
            scratch.extend_from_slice(b"\r\n");
            let BufResult(r, returned) = stream.write_all(scratch).await;
            scratch = returned;
            if r.is_err() {
                return false;
            }
        }
        if reader.is_done() {
            break;
        }
        // Bound the idle wait: a handler that produces nothing and never
        // closes would otherwise pin this connection + compio task forever.
        // Every emitted chunk resets the clock (we loop back and re-arm the
        // sleep), so a steadily-producing stream is never cut off.
        if wait_for_data_or_idle(&reader, idle_timeout).await == StreamWait::IdleTimeout {
            // Stream wedged. Send the terminator so a well-behaved client
            // sees a clean (if truncated) end, then drop the connection.
            let _ = stream.write_all(b"0\r\n\r\n" as &'static [u8]).await;
            return false;
        }
    }
    // `&'static [u8]` implements `IoBuf`, so the trailer ships without a
    // `.to_vec()` allocation.
    let BufResult(r, _) = stream.write_all(b"0\r\n\r\n" as &'static [u8]).await;
    r.is_ok()
}

fn build_stream_response_headers(status: u16, headers: &[(String, String)]) -> Vec<u8> {
    let status_text = match status {
        200 => "OK", 404 => "Not Found", 500 => "Internal Server Error", _ => "OK",
    };
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"HTTP/1.1 ");
    let mut status_buf = itoa::Buffer::new();
    buf.extend_from_slice(status_buf.format(status).as_bytes());
    buf.push(b' ');
    buf.extend_from_slice(status_text.as_bytes());
    buf.extend_from_slice(b"\r\n");

    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") { continue; }
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    buf.extend_from_slice(b"\r\n");
    buf
}

// ===========================================================================
// Unified request dispatch
// ===========================================================================

/// Single entry point from the TCP parser into the runtime. Builds the
/// fetch-handler envelope (env + per-request ctx), invokes
/// `Runtime::call_fetch_handler`, and writes the `FetchOutcome` back to
/// the stream.
///
/// Mirrors `worker::handler::dispatch` — the worker receives the envelope
/// over HTTP from the gateway, serve.rs constructs it from the raw TCP
/// request — but past that point the shape is identical, so both paths
/// share the same outcome vocabulary.
///
/// Returns `true` if the write to `stream` succeeded (connection can be
/// reused for HTTP keep-alive). After a successful WebSocket upgrade the
/// caller exits the connection handler since the stream is no longer HTTP.
async fn handle_request(
    stream: &mut TcpStream,
    method: &str,
    url: &str,
    request_headers: &[(String, String)],
    body: &str,
    runtime: &Runtime,
) -> bool {
    // Env is empty in the standalone server — there's no control plane in
    // front of it providing per-app secrets / vars. `call_fetch_handler`
    // still reads it (as `fetch(req, env, ctx)`'s second arg), just as
    // `{}`.
    let env = EnvSnapshot::empty();
    let cancel = CancelFlag::new();
    let ctx = RequestCtx::new(cancel.clone());

    // Dev-tier auth: in self-contained dev (`ZEROSHIP_DEV=1`) there is no
    // gateway to HMAC-sign a `ZeroShip-User` header, so the JS dev-auth
    // provider (`@zeroship/bootstrap/dev`) mints a local `__zeroship_dev_session`
    // cookie instead. Resolve the dev identity from that cookie and thread it
    // through the SAME `call_fetch_handler_with_user` path the worker uses for
    // the gateway header — identical `user_json` shape, identical native
    // plumbing (`env.auth.getUser()` + `currentUser()`). Returns `None` (and
    // dispatches anonymously) outside dev or when no valid cookie is present.
    let user_json = crate::dev_auth::resolve_dev_user_json(request_headers);

    let outcome =
        runtime.call_fetch_handler_with_user(method, url, request_headers, body, &env, ctx, user_json);

    match outcome {
        FetchOutcome::Response { status, headers, body, logs: _ } => {
            let resp = build_http_response(status, &headers, &body);
            let BufResult(r, _) = stream.write_all(resp).await;
            r.is_ok()
        }
        FetchOutcome::Stream { status, headers, body_reader, logs: _ } => {
            // Notify the pump — a streaming handler may have queued timers
            // or fetches in its `start()` callback that won't run until
            // the pump loop picks them up.
            runtime.notify_pump();
            let header_bytes = build_stream_response_headers(status, &headers);
            let BufResult(r, _) = stream.write_all(header_bytes).await;
            if r.is_err() { return false; }
            stream_chunked_body(stream, body_reader).await
        }
        FetchOutcome::WebSocketUpgrade { ws_id, headers } => {
            handle_websocket_upgrade(stream, ws_id, &headers, request_headers, runtime).await
        }
        FetchOutcome::Pending { rx, cancel: cf } => {
            match recv_with_timeout(&rx, runtime.wall_timeout(), &cf, runtime).await {
                Some(Ok(SettledFetch::Response { status, headers, body, .. })) => {
                    let resp = build_http_response(status, &headers, &body);
                    let BufResult(r, _) = stream.write_all(resp).await;
                    r.is_ok()
                }
                Some(Ok(SettledFetch::Stream { status, headers, body_reader, .. })) => {
                    let header_bytes = build_stream_response_headers(status, &headers);
                    let BufResult(r, _) = stream.write_all(header_bytes).await;
                    if r.is_err() { return false; }
                    stream_chunked_body(stream, body_reader).await
                }
                Some(Ok(SettledFetch::WebSocketUpgrade { ws_id, headers, .. })) => {
                    handle_websocket_upgrade(stream, ws_id, &headers, request_headers, runtime).await
                }
                Some(Err(e)) => {
                    let body = format!(
                        r#"{{"message":"{}","name":"Error"}}"#,
                        e.message.replace('"', "\\\"")
                    );
                    let resp = build_http_response(e.status, &[], &body);
                    let BufResult(r, _) = stream.write_all(resp).await;
                    r.is_ok()
                }
                None => {
                    let resp = build_http_response(
                        504, &[],
                        r#"{"message":"request timed out","name":"Error"}"#,
                    );
                    let BufResult(r, _) = stream.write_all(resp).await;
                    r.is_ok()
                }
            }
        }
    }
}

// ===========================================================================
// WebSocket handshake + frame I/O
// ===========================================================================

/// Compute the `Sec-WebSocket-Accept` value per RFC 6455 section 4.2.2.
fn compute_ws_accept_key(key: &str) -> String {
    use sha1::Digest;
    let mut hasher = sha1::Sha1::new();
    hasher.update(key.trim().as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, hasher.finalize())
}

/// Write the HTTP 101 Switching Protocols response to complete the WebSocket handshake.
async fn write_ws_handshake(
    stream: &mut TcpStream,
    accept_key: &str,
    response_headers: &[(String, String)],
) -> bool {
    let mut buf = Vec::with_capacity(256);
    buf.extend_from_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
    buf.extend_from_slice(b"Upgrade: websocket\r\n");
    buf.extend_from_slice(b"Connection: Upgrade\r\n");
    buf.extend_from_slice(b"Sec-WebSocket-Accept: ");
    buf.extend_from_slice(accept_key.as_bytes());
    buf.extend_from_slice(b"\r\n");

    // Write any additional headers from the JS Response (e.g. Sec-WebSocket-Protocol)
    for (name, value) in response_headers {
        let lname = name.to_lowercase();
        if lname == "upgrade" || lname == "connection" || lname == "sec-websocket-accept" {
            continue; // already written
        }
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(b": ");
        buf.extend_from_slice(value.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }
    buf.extend_from_slice(b"\r\n");

    let BufResult(r, _) = stream.write_all(buf).await;
    r.is_ok()
}

/// Read a single WebSocket frame from the stream.
/// Returns (opcode, payload) or None on error/EOF.
///
/// Client-to-server frames are always masked (RFC 6455 section 5.1).
async fn read_ws_frame<R: AsyncRead + Unpin>(stream: &mut R) -> Option<(u8, Vec<u8>)> {
    // Read header (2 bytes), payload-len extension, mask, and payload
    // bytes via `read_exact` so partial reads don't corrupt the
    // framing. Compio returns fewer bytes than requested when:
    //   - the kernel TCP buffer holds less than the request size, OR
    //   - the io_uring SQE was completed with a short result.
    // Either way we have to keep reading until we've consumed the
    // expected number of bytes. The previous design treated every
    // `n < expected` short read as EOF, which silently discarded the
    // rest of the frame *and* every frame pipelined behind it.
    let header = read_exact(stream, 2).await?;
    let _fin = (header[0] & 0x80) != 0;
    let opcode = header[0] & 0x0F;
    let masked = (header[1] & 0x80) != 0;
    let mut payload_len = (header[1] & 0x7F) as u64;

    // Extended payload length
    if payload_len == 126 {
        let ext = read_exact(stream, 2).await?;
        payload_len = u16::from_be_bytes([ext[0], ext[1]]) as u64;
    } else if payload_len == 127 {
        let ext = read_exact(stream, 8).await?;
        payload_len = u64::from_be_bytes([
            ext[0], ext[1], ext[2], ext[3], ext[4], ext[5], ext[6], ext[7],
        ]);
    }

    // Reject oversized frames BEFORE allocating the payload buffer.
    //
    // A 16-byte 64-bit-length header can declare a payload of up to
    // ~16 EiB. Without this guard the `vec![0u8; len]` below would
    // honour that request and abort the process (OOM) — a trivial DoS
    // for any client that can open a WebSocket on the native `zeroship
    // serve` path. Cap at the same `DEFAULT_MAX_FRAME_SIZE` the JS-side
    // `FrameReader` enforces so the two WS paths agree.
    //
    // We synthesise an RFC 6455 §7.4.1 status 1009 ("Message Too Big")
    // Close frame (opcode 0x8, payload = 1009 big-endian) and return it
    // instead of the real frame. Both `read_ws_frame` call sites already
    // handle a 0x8 frame by echoing a Close with the carried code and
    // tearing the connection down — so the peer sees a clean 1009 close
    // and we never touch the oversized length again.
    const MAX_WS_FRAME_PAYLOAD: u64 = crate::web::websocket::constants::DEFAULT_MAX_FRAME_SIZE as u64;
    if payload_len > MAX_WS_FRAME_PAYLOAD {
        return Some((0x8, 1009u16.to_be_bytes().to_vec()));
    }

    // Masking key (4 bytes if masked)
    let mask_key = if masked {
        let mk = read_exact(stream, 4).await?;
        Some([mk[0], mk[1], mk[2], mk[3]])
    } else {
        None
    };

    // Read payload
    let len = payload_len as usize;
    let mut payload = if len > 0 {
        read_exact(stream, len).await?
    } else {
        Vec::new()
    };

    // Unmask
    if let Some(mk) = mask_key {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mk[i % 4];
        }
    }

    Some((opcode, payload))
}

/// Read exactly `n` bytes from `stream`, looping over partial reads.
/// Returns None on EOF or error before `n` bytes have been read.
async fn read_exact<R: AsyncRead + Unpin>(stream: &mut R, n: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; n];
    let mut offset = 0;
    while offset < n {
        let chunk = vec![0u8; n - offset];
        let BufResult(r, returned) = stream.read(chunk).await;
        let read_n = match r {
            Ok(0) => return None, // EOF
            Ok(k) => k,
            Err(_) => return None,
        };
        buf[offset..offset + read_n].copy_from_slice(&returned[..read_n]);
        offset += read_n;
    }
    Some(buf)
}

/// Write a WebSocket frame to the stream (server-to-client: unmasked).
async fn write_ws_frame<W: compio::io::AsyncWrite + Unpin>(stream: &mut W, opcode: u8, payload: &[u8]) -> bool {
    let len = payload.len();
    let mut frame = Vec::with_capacity(10 + len);

    // FIN + opcode
    frame.push(0x80 | opcode);

    // Payload length (unmasked)
    if len < 126 {
        frame.push(len as u8);
    } else if len <= 65535 {
        frame.push(126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }

    frame.extend_from_slice(payload);

    let BufResult(r, _) = stream.write_all(frame).await;
    r.is_ok()
}

/// Handle a WebSocket upgrade: perform handshake, then run the bidirectional pump.
///
/// Native path (`runtime_native_websocket` feature): owns the `TcpStream`
/// (passed by value because the inner pump splits read/write into two
/// independent compio tasks sharing an `Rc<TcpStream>` — a single
/// `&mut TcpStream` borrow can't be split). io_uring multiplexes
/// concurrent submissions on the same fd, so reads and writes proceed
/// without blocking each other.
///
/// Polyfill path: keeps the prior single-task design. The polyfill's
/// outbound queue lives on the per-WS state, drained via the
/// `outgoing_ready` flag — no channel cancellation hazard.
async fn handle_websocket_upgrade(
    stream: &mut TcpStream,
    ws_id: u32,
    response_headers: &[(String, String)],
    request_headers: &[(String, String)],
    runtime: &Runtime,
) -> bool {
    // Find the Sec-WebSocket-Key from request headers
    let ws_key = request_headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
        .map(|(_, v)| v.as_str())
        .unwrap_or("");


    if ws_key.is_empty() {
        let response = build_http_response(400, &[], "Missing Sec-WebSocket-Key");
        let BufResult(r, _) = stream.write_all(response).await;
        return r.is_ok();
    }

    // Compute accept key and send 101 response
    let accept_key = compute_ws_accept_key(ws_key);
    if !write_ws_handshake(stream, &accept_key, response_headers).await {
        return false;
    }

    // The server-side WS is the peer of the client side. The native
    // WebSocketPair allocates two consecutive ids (client = N,
    // server = N+1).
    #[cfg(feature = "runtime_native_websocket")]
    let server_ws_id: u32 = ws_id + 1;
    #[cfg(not(feature = "runtime_native_websocket"))]
    let server_ws_id = {
        let state = runtime.state();
        let s = state.borrow();
        s.websockets.get(&ws_id).and_then(|ws| ws.peer_id).unwrap_or(0)
    };

    #[cfg(not(feature = "runtime_native_websocket"))]
    if server_ws_id == 0 {
        return false;
    }

    // Open the kernel-outbound channel on the CLIENT-side native WS:
    // anything the JS server-side `socket.send()`s ends up here as
    // a `WsEvent::MessageText` / `WsEvent::MessageBinary` / Close.
    #[cfg(feature = "runtime_native_websocket")]
    let kernel_rx = {
        use futures::channel::mpsc;
        use crate::websocket_native::network as nw;
        let (tx, rx) = mpsc::unbounded::<nw::WsEvent>();
        let state = runtime.state();
        if let Some(ws) = nw::lookup_native_ws_state(&state, ws_id) {
            ws.borrow_mut().kernel_outbound = Some(tx);
        } else {
            return false;
        }
        rx
    };

    // Polyfill-only: notification handles for this WebSocket's outgoing queue.
    #[cfg(not(feature = "runtime_native_websocket"))]
    let (outgoing_ready, pump_waker) = {
        let state = runtime.state();
        let s = state.borrow();
        if let Some(ws) = s.websockets.get(&server_ws_id) {
            (ws.outgoing_ready.clone(), ws.pump_waker.clone())
        } else {
            return false;
        }
    };

    // Native path: bidirectional pump implemented as two compio tasks
    // sharing the TCP fd via `Rc<TcpStream>`. Reader runs `read_ws_frame`
    // in a steady loop and dispatches frames to V8; writer drains
    // `kernel_rx` and emits frames to TCP. Each task awaits only its
    // own io_uring submissions — never select-cancel a partially-
    // completed read, which on io_uring drops the buffer with bytes
    // already in it (and shifts every subsequent frame's framing).
    #[cfg(feature = "runtime_native_websocket")]
    {
        return native_ws_pump(stream, server_ws_id, kernel_rx, runtime).await;
    }

    #[cfg(not(feature = "runtime_native_websocket"))]
    {
        loop {
            outgoing_ready.set(false);
            let mut got_close = false;
            loop {
                let msg = {
                    let state = runtime.state();
                    let mut s = state.borrow_mut();
                    match s.websockets.get_mut(&server_ws_id) {
                        Some(ws) => ws.outgoing.pop_front(),
                        None => return true,
                    }
                };
                let Some(msg) = msg else { break };
                match msg {
                    crate::state::WsMessage::Text(text) => {
                        if !write_ws_frame(stream, 0x1, text.as_bytes()).await {
                            return false;
                        }
                    }
                    crate::state::WsMessage::Binary(data) => {
                        if !write_ws_frame(stream, 0x2, &data).await {
                            return false;
                        }
                    }
                    crate::state::WsMessage::Close(code, reason) => {
                        let mut close_payload = Vec::with_capacity(2 + reason.len());
                        close_payload.extend_from_slice(&code.to_be_bytes());
                        close_payload.extend_from_slice(reason.as_bytes());
                        let _ = write_ws_frame(stream, 0x8, &close_payload).await;
                        got_close = true;
                    }
                }
            }

            if got_close {
                return true;
            }

            let event = WsPollBoth::new(
                read_ws_frame(stream),
                outgoing_ready.clone(),
                pump_waker.clone(),
            )
            .await;

            match event {
                WsEvent::Outgoing => {
                    continue;
                }
                WsEvent::Frame(None) => {
                    return true;
                }
                WsEvent::Frame(Some((0x1, payload))) | WsEvent::Frame(Some((0x2, payload))) => {
                    let text = String::from_utf8(payload).unwrap_or_default();
                    deliver_ws_message(runtime, server_ws_id, &text);
                }
                WsEvent::Frame(Some((0x8, payload))) => {
                    let (code, reason) = if payload.len() >= 2 {
                        let code = u16::from_be_bytes([payload[0], payload[1]]);
                        let reason = String::from_utf8(payload[2..].to_vec()).unwrap_or_default();
                        (code, reason)
                    } else {
                        (1000, String::new())
                    };
                    deliver_ws_close(runtime, server_ws_id, code, &reason);
                    let mut close_payload = Vec::with_capacity(2 + reason.len());
                    close_payload.extend_from_slice(&code.to_be_bytes());
                    close_payload.extend_from_slice(reason.as_bytes());
                    let _ = write_ws_frame(stream, 0x8, &close_payload).await;
                    return true;
                }
                WsEvent::Frame(Some((0x9, payload))) => {
                    let _ = write_ws_frame(stream, 0xA, &payload).await;
                }
                WsEvent::Frame(Some((0xA, _))) => {}
                WsEvent::Frame(Some(_)) => {}
            }
        }
    }
}

/// Write one kernel-side WebSocket event (the JS server's `socket.send()`
/// or `socket.close()` output) to the wire. Returns false on TCP write
/// failure; sets `got_close` to true when the event was a Close (the
/// caller should return true after a clean close handshake).
#[cfg(feature = "runtime_native_websocket")]
async fn write_kernel_event<W: compio::io::AsyncWrite + Unpin>(
    stream: &mut W,
    ev: crate::websocket_native::network::WsEvent,
    got_close: &mut bool,
) -> bool {
    use crate::websocket_native::network as nw;
    match ev {
        nw::WsEvent::MessageText(text) => {
            write_ws_frame(stream, 0x1, text.as_bytes()).await
        }
        nw::WsEvent::MessageBinary(data) => {
            write_ws_frame(stream, 0x2, &data).await
        }
        nw::WsEvent::Close { code, reason, .. } => {
            let mut close_payload = Vec::with_capacity(2 + reason.len());
            close_payload.extend_from_slice(&code.to_be_bytes());
            close_payload.extend_from_slice(reason.as_bytes());
            let ok = write_ws_frame(stream, 0x8, &close_payload).await;
            *got_close = true;
            ok
        }
        nw::WsEvent::Open { .. } | nw::WsEvent::Error { .. } => {
            // Open: pair sockets fire this at accept(); we don't
            // propagate to TCP. Error: same.
            true
        }
    }
}

/// Native-WebSocket bidirectional pump.
///
/// Architecture: a single task runs the read loop; the writer is a
/// concurrent future driven on the same task via `join`. Both halves
/// borrow the TCP stream as `&TcpStream` (immutable) — compio's
/// `impl AsyncRead/AsyncWrite for &TcpStream` issues independent
/// io_uring submissions, so reads and writes proceed in parallel.
///
/// Why this design instead of the previous `select(read, recv)`:
/// io_uring read submissions that have already completed (kernel
/// filled the user buffer) but haven't been polled yet still LOSE
/// their bytes when the future is dropped. A `select` arm that
/// returns on `recv` resolution drops the surviving `read` future,
/// which in turn drops the buffer with already-received bytes. The
/// next `read` then returns from the middle of the previous frame
/// (mask byte or payload), corrupting every subsequent frame's
/// framing.
///
/// `join` polls both futures cooperatively until both complete; no
/// future is cancelled mid-completion. The reader runs `read_ws_frame`
/// in a steady loop and exits on EOF or peer Close. The writer drains
/// `kernel_rx` (frames the JS server enqueued via `socket.send()`),
/// plus a small close-echo channel the reader uses to forward peer
/// Closes for an RFC 6455-clean handshake.
#[cfg(feature = "runtime_native_websocket")]
async fn native_ws_pump(
    stream: &mut TcpStream,
    server_ws_id: u32,
    mut kernel_rx: futures::channel::mpsc::UnboundedReceiver<crate::websocket_native::network::WsEvent>,
    runtime: &Runtime,
) -> bool {
    use futures::channel::mpsc;
    use futures::stream::StreamExt;
    use futures::FutureExt;

    // Reader → writer channel for echoing the peer's Close.
    let (close_tx, mut close_rx) = mpsc::unbounded::<(u16, String)>();

    // Both halves use `&TcpStream` for AsyncRead/AsyncWrite — compio
    // multiplexes simultaneous SQEs on the same fd.
    let stream_shared: &TcpStream = stream;

    // Reader: read TCP frames, dispatch to V8. Owns the close-echo
    // sender; dropping it on return tells the writer there are no
    // more close-echoes to wait for.
    let reader = async move {
        let mut s = stream_shared; // `&TcpStream`, takes `&mut &TcpStream` for reads
        loop {
            let frame = read_ws_frame(&mut s).await;
            match frame {
                None => return true, // EOF
                Some((0x1, payload)) | Some((0x2, payload)) => {
                    let text = String::from_utf8(payload).unwrap_or_default();
                    deliver_ws_message(runtime, server_ws_id, &text);
                }
                Some((0x8, payload)) => {
                    let (code, reason) = if payload.len() >= 2 {
                        let code = u16::from_be_bytes([payload[0], payload[1]]);
                        let reason = String::from_utf8(payload[2..].to_vec()).unwrap_or_default();
                        (code, reason)
                    } else {
                        (1000, String::new())
                    };
                    deliver_ws_close(runtime, server_ws_id, code, &reason);
                    let _ = close_tx.unbounded_send((code, reason));
                    return true;
                }
                Some((0x9, _payload)) => {
                    // Ping handling on this dev/bench path is a
                    // no-op — there are no client-driven keepalive
                    // pings on the bench scenarios. The
                    // `network::run_plain_driver` (used by JS-side
                    // `new WebSocket(url)`) handles RFC 6455 Pings
                    // natively. Production traffic flows through
                    // gateway → worker which uses that path.
                }
                Some((0xA, _)) => {}
                Some(_) => {}
            }
        }
    };

    // Writer: drain kernel_rx + close_rx; write frames to TCP. Exits
    // when both feeds close (reader dropped close_tx + JS dropped
    // kernel_outbound), or after writing a Close.
    let writer = async move {
        let mut s = stream_shared;
        loop {
            futures::select! {
                ev = kernel_rx.next().fuse() => {
                    let Some(ev) = ev else {
                        // kernel_outbound dropped — the WS state
                        // was destroyed. Wait only on close_rx
                        // from now on.
                        if let Some((code, reason)) = close_rx.next().await {
                            let mut close_payload = Vec::with_capacity(2 + reason.len());
                            close_payload.extend_from_slice(&code.to_be_bytes());
                            close_payload.extend_from_slice(reason.as_bytes());
                            let _ = write_ws_frame(&mut s, 0x8, &close_payload).await;
                        }
                        return;
                    };
                    let mut got_close = false;
                    if !write_kernel_event(&mut s, ev, &mut got_close).await {
                        return;
                    }
                    if got_close {
                        return;
                    }
                }
                cz = close_rx.next().fuse() => {
                    let Some((code, reason)) = cz else {
                        // Reader exited without sending close —
                        // remaining drain is from kernel_rx; loop.
                        continue;
                    };
                    let mut close_payload = Vec::with_capacity(2 + reason.len());
                    close_payload.extend_from_slice(&code.to_be_bytes());
                    close_payload.extend_from_slice(reason.as_bytes());
                    let _ = write_ws_frame(&mut s, 0x8, &close_payload).await;
                    return;
                }
            }
        }
    };

    // Run both halves cooperatively on this task. `join` polls both
    // until both return — no cancellation, no dropped io_uring
    // submissions. `select_biased`-style preference doesn't matter
    // here: each future only awaits its own ops.
    let (reader_result, _) = futures::future::join(reader, writer).await;
    reader_result
}

enum WsEvent {
    Frame(Option<(u8, Vec<u8>)>),
    Outgoing,
}

/// Combined future: waits for either a TCP frame OR an outgoing notification.
/// Avoids the overhead of `Fuse` wrappers + `futures::select!` (saves ~9% CPU).
///
/// SAFETY: `read_fut` is structurally pinned.
struct WsPollBoth<F> {
    read_fut: F,
    outgoing_ready: Rc<Cell<bool>>,
    pump_waker: Rc<RefCell<Option<Waker>>>,
}

impl<F> WsPollBoth<F> {
    fn new(read_fut: F, outgoing_ready: Rc<Cell<bool>>, pump_waker: Rc<RefCell<Option<Waker>>>) -> Self {
        Self { read_fut, outgoing_ready, pump_waker }
    }
}

impl<F: std::future::Future<Output = Option<(u8, Vec<u8>)>>> std::future::Future for WsPollBoth<F> {
    type Output = WsEvent;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<WsEvent> {
        // SAFETY: read_fut is structurally pinned — we never move self after pinning.
        let this = unsafe { self.get_unchecked_mut() };

        // Check outgoing notification first (cheapest — just a Cell read).
        if this.outgoing_ready.get() {
            return std::task::Poll::Ready(WsEvent::Outgoing);
        }

        // Poll the TCP read.
        let read_pin = unsafe { std::pin::Pin::new_unchecked(&mut this.read_fut) };
        if let std::task::Poll::Ready(frame) = read_pin.poll(cx) {
            return std::task::Poll::Ready(WsEvent::Frame(frame));
        }

        // Neither ready — store our waker for the outgoing notification.
        *this.pump_waker.borrow_mut() = Some(cx.waker().clone());
        // Double-check after storing waker.
        if this.outgoing_ready.get() {
            return std::task::Poll::Ready(WsEvent::Outgoing);
        }

        std::task::Poll::Pending
    }
}

/// Enter V8 to call `ws._onMessage(data)` on the server WebSocket.
fn deliver_ws_message(runtime: &Runtime, ws_id: u32, data: &str) {
    runtime.enter_v8_for_ws_message(ws_id, data);
}

/// Enter V8 to call `ws._onClose(code, reason)` on the server WebSocket.
fn deliver_ws_close(runtime: &Runtime, ws_id: u32, code: u16, reason: &str) {
    runtime.enter_v8_for_ws_close(ws_id, code, reason);
}

// ===========================================================================
// SO_REUSEPORT listener
// ===========================================================================

fn create_reuseport_listener(port: u16) -> std::net::TcpListener {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    socket.set_reuse_port(true).unwrap();
    socket.set_reuse_address(true).unwrap();
    socket
        .bind(
            &format!("0.0.0.0:{port}")
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        )
        .unwrap();
    socket.listen(1024).unwrap();
    socket.set_nonblocking(true).unwrap();
    socket.into()
}

const ACCEPT_ERROR_BASE_BACKOFF: Duration = Duration::from_millis(10);
const ACCEPT_ERROR_MAX_BACKOFF: Duration = Duration::from_millis(250);

fn accept_error_backoff(consecutive_errors: u32) -> Duration {
    let shift = consecutive_errors.saturating_sub(1).min(5);
    let multiplier = 1_u32 << shift;
    ACCEPT_ERROR_BASE_BACKOFF
        .saturating_mul(multiplier)
        .min(ACCEPT_ERROR_MAX_BACKOFF)
}

async fn accept_loop(listener: TcpListener, runtime: Runtime) {
    let mut consecutive_errors = 0_u32;
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                consecutive_errors = 0;
                let rt = runtime.clone();
                compio::runtime::spawn(async move {
                    crate::panic_util::guard("handle_connection", handle_connection(stream, rt))
                        .await;
                })
                .detach();
            }
            Err(err) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                let backoff = accept_error_backoff(consecutive_errors);
                tracing::warn!(
                    error = %err,
                    consecutive_errors,
                    backoff_ms = backoff.as_millis(),
                    "runtime accept error"
                );
                compio::time::sleep(backoff).await;
            }
        }
    }
}

// ===========================================================================
// Single-worker entry point
// ===========================================================================

fn run_single_worker(
    port: u16,
    use_reuseport: bool,
    worker_id: Option<usize>,
    cpu_limit: Option<Duration>,
    wall_timeout: Option<Duration>,
    heap_limit_bytes: Option<usize>,
    modules: Vec<ModuleEntry>,
    env_vars: HashMap<String, String>,
    plugins: Vec<Arc<dyn NativePlugin>>,
) {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            let listener = if use_reuseport {
                let std_listener = create_reuseport_listener(port);
                unsafe {
                    use std::os::fd::{FromRawFd, IntoRawFd};
                    TcpListener::from_raw_fd(std_listener.into_raw_fd())
                }
            } else {
                TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap()
            };

            if let Some(id) = worker_id {
                tracing::info!(worker_id = id, port, "runtime worker ready");
            } else {
                tracing::info!(port, addr = %format!("http://0.0.0.0:{port}"), "runtime listening");
            }

            let runtime = Runtime::builder()
                .modules(modules)
                .env_vars(env_vars)
                .limits(RuntimeLimits {
                    cpu_limit,
                    wall_timeout,
                    heap_limit_bytes,
                })
                .plugins(plugins)
                .build();

            // Start the async event loop pump (timers, fetch, streams).
            // (Warmup removed — `call_fetch_handler` initializes lazily via
            // `ensure_initialized` on the first request; the kernel no
            // longer exposes a bare-function dispatch primitive.)
            runtime.start_pump();

            // Accept loop
            crate::panic_util::guard("runtime_accept_loop", accept_loop(listener, runtime)).await;
        });
}

#[cfg(test)]
mod chunked_decode_tests {
    use super::*;

    fn assert_complete(result: ChunkedDecode, expected_body: &[u8]) {
        match result {
            ChunkedDecode::Complete { body_bytes, consumed_input: _ } => {
                assert_eq!(&body_bytes[..], expected_body);
            }
            other => panic!("expected Complete, got {:?}", discriminant(other)),
        }
    }

    fn discriminant(r: ChunkedDecode) -> &'static str {
        match r {
            ChunkedDecode::Complete { .. } => "Complete",
            ChunkedDecode::Incomplete => "Incomplete",
            ChunkedDecode::TooLarge => "TooLarge",
            ChunkedDecode::Invalid => "Invalid",
        }
    }

    #[test]
    fn decodes_minimal_chunked_body() {
        // 5\r\nhello\r\n0\r\n\r\n  →  "hello"
        let input = b"5\r\nhello\r\n0\r\n\r\n";
        assert_complete(decode_chunked_body(input, 1024), b"hello");
    }

    #[test]
    fn decodes_multi_chunk_body() {
        let input = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        assert_complete(decode_chunked_body(input, 1024), b"hello world");
    }

    #[test]
    fn accept_error_backoff_is_small_and_bounded() {
        assert_eq!(accept_error_backoff(1), Duration::from_millis(10));
        assert_eq!(accept_error_backoff(2), Duration::from_millis(20));
        assert_eq!(accept_error_backoff(6), Duration::from_millis(250));
        assert_eq!(accept_error_backoff(u32::MAX), Duration::from_millis(250));
    }

    #[test]
    fn ignores_chunk_extensions() {
        // Extensions after ';' on the size line must be ignored.
        let input = b"5;meta=x\r\nhello\r\n0\r\n\r\n";
        assert_complete(decode_chunked_body(input, 1024), b"hello");
    }

    #[test]
    fn incomplete_on_partial_chunk() {
        // Size line is there but the data isn't fully buffered yet.
        let input = b"5\r\nhel";
        matches!(decode_chunked_body(input, 1024), ChunkedDecode::Incomplete);
    }

    #[test]
    fn too_large_rejects() {
        // Claimed chunk size blows past the cap before we even try to read
        // the data bytes. Catch early to avoid letting a hostile client
        // force us to allocate a huge buffer.
        let input = b"1000000\r\n";
        matches!(decode_chunked_body(input, 1024), ChunkedDecode::TooLarge);
    }

    #[test]
    fn invalid_hex_rejects() {
        let input = b"zzz\r\n";
        matches!(decode_chunked_body(input, 1024), ChunkedDecode::Invalid);
    }

    #[test]
    fn empty_body_accepted() {
        let input = b"0\r\n\r\n";
        assert_complete(decode_chunked_body(input, 1024), b"");
    }
}

#[cfg(test)]
mod ws_frame_tests {
    use super::*;

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(fut)
    }

    /// Build a masked client->server frame header that *declares* a
    /// 64-bit payload length of `declared_len` (using the 127 extended
    /// form). Only the header bytes are emitted — for the oversized case
    /// the reader must reject before it ever tries to read the payload,
    /// so trailing payload bytes are deliberately absent.
    fn masked_header(opcode: u8, declared_len: u64) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(0x80 | (opcode & 0x0F)); // FIN + opcode
        out.push(0x80 | 127); // MASK bit + 127 (8-byte extended length)
        out.extend_from_slice(&declared_len.to_be_bytes());
        out
    }

    /// A full, valid, masked client frame (header + mask key + masked
    /// payload) carrying `payload` under `opcode`.
    fn masked_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(0x80 | (opcode & 0x0F));
        let mask = [0xA1u8, 0xB2, 0xC3, 0xD4];
        let len = payload.len();
        if len <= 125 {
            out.push(0x80 | len as u8);
        } else if len <= u16::MAX as usize {
            out.push(0x80 | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.push(0x80 | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
        out.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            out.push(b ^ mask[i % 4]);
        }
        out
    }

    /// Regression for P4-B-1: a frame declaring a payload larger than the
    /// cap must be rejected WITHOUT allocating the declared buffer, and
    /// must surface as a 1009 ("Message Too Big") Close frame. Pre-fix the
    /// reader did `vec![0u8; len]` with no bound, so this header would have
    /// triggered a multi-terabyte allocation / OOM abort instead.
    #[test]
    fn oversized_frame_rejected_with_1009_no_alloc() {
        // Declare ~280 TB. Only the 10-byte header is provided; if the
        // reader tried to honour the length it would allocate (and abort)
        // long before noticing the payload bytes are missing.
        let declared: u64 = 280_000_000_000_000;
        let header = masked_header(0x2, declared);
        let mut r: &[u8] = &header;

        let frame = block_on(read_ws_frame(&mut r));
        let (opcode, payload) = frame.expect("oversized frame should yield a Close, not EOF");
        assert_eq!(opcode, 0x8, "oversized frame must surface as a Close frame");
        assert_eq!(payload.len(), 2, "Close payload must carry just the 2-byte status code");
        let code = u16::from_be_bytes([payload[0], payload[1]]);
        assert_eq!(code, 1009, "must close with status 1009 (Message Too Big)");
    }

    /// A frame exactly at the cap (DEFAULT_MAX_FRAME_SIZE) still parses
    /// normally — the guard rejects strictly-greater-than, not equal.
    #[test]
    fn frame_at_cap_still_parses() {
        let cap = crate::web::websocket::constants::DEFAULT_MAX_FRAME_SIZE as usize;
        let payload = vec![0x5Au8; cap];
        let bytes = masked_frame(0x2, &payload);
        let mut r: &[u8] = &bytes;

        let frame = block_on(read_ws_frame(&mut r));
        let (opcode, got) = frame.expect("at-cap frame should parse");
        assert_eq!(opcode, 0x2);
        assert_eq!(got, payload, "payload round-trips through unmasking");
    }

    /// A normal small frame still parses and unmasks correctly after the
    /// guard is in place.
    #[test]
    fn normal_frame_still_parses() {
        let bytes = masked_frame(0x1, b"hello");
        let mut r: &[u8] = &bytes;

        let frame = block_on(read_ws_frame(&mut r));
        let (opcode, payload) = frame.expect("normal frame should parse");
        assert_eq!(opcode, 0x1);
        assert_eq!(payload, b"hello");
    }
}

#[cfg(test)]
mod stream_idle_tests {
    //! Regression for P4-B-3 / RT-3: the SSE/chunked streaming pump must
    //! release a stalled stream instead of pinning the connection + worker
    //! task forever. We exercise the real idle-wait helper the pump calls on
    //! every loop iteration (`wait_for_data_or_idle`) against a live
    //! `StreamReader`/`StreamWriter` pair — no shim, no fake.
    use super::*;
    use crate::channel::stream_buffer;
    use std::time::Instant;

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(fut)
    }

    /// A stream that produces nothing and never closes must resolve to
    /// `IdleTimeout` once the deadline elapses — not hang. Pre-fix the pump
    /// awaited `reader.wait_for_data()` with no bound, so the equivalent wait
    /// would block forever and this test would never return.
    #[test]
    fn idle_stream_times_out() {
        let (_writer, reader) = stream_buffer();
        // Keep `_writer` alive but silent: not closed, no chunks pushed —
        // exactly the wedged-handler shape.
        let idle = Duration::from_millis(50);
        let start = Instant::now();
        let outcome = block_on(wait_for_data_or_idle(&reader, idle));
        assert_eq!(
            outcome,
            StreamWait::IdleTimeout,
            "a silent, never-closed stream must hit the idle ceiling"
        );
        assert!(
            start.elapsed() >= idle,
            "must wait the full idle window before giving up"
        );
    }

    /// A stream with a chunk already queued must resolve `Ready` immediately
    /// — the idle bound must never cut off a producing stream.
    #[test]
    fn active_stream_not_prematurely_closed() {
        let (writer, reader) = stream_buffer();
        // Data is available before we even start waiting.
        assert_eq!(
            writer.push(b"token".to_vec()),
            crate::channel::StreamPushResult::Ok
        );
        // Generous idle window: if the bound fired here it would be a bug,
        // but the test would still finish fast because `Ready` short-circuits.
        let idle = Duration::from_secs(30);
        let start = Instant::now();
        let outcome = block_on(wait_for_data_or_idle(&reader, idle));
        assert_eq!(
            outcome,
            StreamWait::Ready,
            "available data must resolve Ready, not time out"
        );
        assert!(
            start.elapsed() < idle,
            "Ready must short-circuit well before the idle deadline"
        );
    }

    /// A closed (completed) stream resolves `Ready` so the pump can write its
    /// terminator and finish cleanly — the idle bound must not swallow a
    /// legitimate end-of-stream.
    #[test]
    fn closed_stream_resolves_ready() {
        let (writer, reader) = stream_buffer();
        writer.close();
        let outcome = block_on(wait_for_data_or_idle(&reader, Duration::from_secs(30)));
        assert_eq!(outcome, StreamWait::Ready);
    }
}
