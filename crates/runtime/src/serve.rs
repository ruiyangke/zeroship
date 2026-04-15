//! Reusable compio HTTP server for zeroship.
//!
//! Extracted from `server.rs` (the v8-server-compio binary) so that both the
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
use std::rc::Rc;
use std::task::Waker;
use std::time::Duration;
use std::collections::HashMap;

use crate::init::init_v8;
use crate::modules::ModuleEntry;
use crate::runtime::{DispatchOutcome, HttpDispatchResult, Runtime};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};


// ===========================================================================
// Public API
// ===========================================================================

/// Configuration for the compio HTTP server.
#[derive(Clone, Debug)]
pub struct ServerOptions {
    pub port: u16,
    /// Number of worker threads. 0 = auto-detect from available parallelism.
    pub workers: usize,
    /// Per-request CPU time limit (enforced by V8 interrupt).
    pub cpu_limit: Option<Duration>,
    /// Per-request wall-clock timeout.
    pub wall_timeout: Option<Duration>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            port: 3000,
            workers: 0,
            cpu_limit: None,
            wall_timeout: None,
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
        eprintln!(
            "[zeroship] cpu_limit={:?} wall_timeout={:?}",
            options.cpu_limit, options.wall_timeout
        );
    }

    if num_workers <= 1 {
        run_single_worker(
            options.port,
            false,
            None,
            options.cpu_limit,
            options.wall_timeout,
            modules,
        );
    } else {
        eprintln!("[zeroship] {num_workers} workers on port {}", options.port);
        let mut handles = Vec::new();
        for i in 0..num_workers {
            let worker_modules = modules.clone();
            let cpu_limit = options.cpu_limit;
            let wall_timeout = options.wall_timeout;
            let port = options.port;
            let handle = std::thread::Builder::new()
                .name(format!("worker-{i}"))
                .spawn(move || {
                    run_single_worker(port, true, Some(i), cpu_limit, wall_timeout, worker_modules);
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

// ===========================================================================
// Yield helper
// ===========================================================================

/// Yield control back to the compio event loop so other tasks can run.
fn yield_now() -> impl std::future::Future<Output = ()> {
    let mut yielded = false;
    std::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
}

// ===========================================================================
// Static responses
// ===========================================================================

const HEALTH_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}";
const NOT_FOUND_RESPONSE: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
const SERVICE_UNAVAILABLE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";

// ===========================================================================
// HTTP connection handler
// ===========================================================================

async fn handle_connection(
    mut stream: TcpStream,
    runtime: Rc<RefCell<Runtime>>,
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

        data.extend_from_slice(&read_buf[..n]);

        let mut consumed = 0;

        loop {
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut req = httparse::Request::new(&mut headers);

            let header_len = match req.parse(&data[consumed..]) {
                Ok(httparse::Status::Complete(len)) => len,
                Ok(httparse::Status::Partial) => break,
                Err(_) => return,
            };

            let method = req.method.unwrap_or("GET");
            let path = req.path.unwrap_or("/");

            let content_length: usize = headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                .and_then(|h| std::str::from_utf8(h.value).ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);

            let total_len = header_len + content_length;
            if data.len() - consumed < total_len {
                break;
            }

            let body_bytes = &data[consumed + header_len..consumed + total_len];

            let has_http = runtime.borrow().has_http_handler();
            match (method, path) {
                ("GET", "/health") => {
                    let BufResult(write_result, _) = stream.write_all(HEALTH_RESPONSE.to_vec()).await;
                    if write_result.is_err() { return; }
                }
                ("POST", "/rpc") => {
                    let response_bytes = dispatch_rpc(body_bytes, &runtime).await;
                    let BufResult(write_result, _) = stream.write_all(response_bytes).await;
                    if write_result.is_err() { return; }
                }
                _ if has_http => {
                    let headers_json = collect_headers_json(&headers);
                    let body_str = std::str::from_utf8(body_bytes).unwrap_or("");
                    let host = headers.iter()
                        .find(|h| h.name.eq_ignore_ascii_case("host"))
                        .and_then(|h| std::str::from_utf8(h.value).ok())
                        .unwrap_or("localhost");
                    let full_url = format!("http://{}{}", host, path);

                    // Collect raw request headers for WebSocket upgrade detection
                    let raw_headers: Vec<(String, String)> = headers.iter()
                        .filter(|h| !h.name.is_empty())
                        .map(|h| (
                            h.name.to_string(),
                            std::str::from_utf8(h.value).unwrap_or("").to_string(),
                        ))
                        .collect();

                    // Check if this is a WebSocket upgrade BEFORE dispatch
                    let is_upgrade = headers.iter().any(|h|
                        h.name.eq_ignore_ascii_case("upgrade") &&
                        std::str::from_utf8(h.value).unwrap_or("").eq_ignore_ascii_case("websocket")
                    );

                    let wrote_ok = dispatch_http(
                        &mut stream, method, &full_url, &headers_json, body_str, &runtime,
                        &raw_headers,
                    ).await;
                    if !wrote_ok { return; }

                    // After WebSocket upgrade, the stream is no longer HTTP.
                    // The dispatch_http call handled the full WebSocket lifecycle.
                    // Exit the connection handler — don't try to parse more HTTP.
                    if is_upgrade { return; }
                }
                _ => {
                    let BufResult(write_result, _) = stream.write_all(NOT_FOUND_RESPONSE.to_vec()).await;
                    if write_result.is_err() { return; }
                }
            };

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

fn build_json_response(body: &str) -> Vec<u8> {
    const PREFIX: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ";
    const SEPARATOR: &[u8] = b"\r\n\r\n";

    let mut len_buf = itoa::Buffer::new();
    let len_str = len_buf.format(body.len());
    let total = PREFIX.len() + len_str.len() + SEPARATOR.len() + body.len();

    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(PREFIX);
    buf.extend_from_slice(len_str.as_bytes());
    buf.extend_from_slice(SEPARATOR);
    buf.extend_from_slice(body.as_bytes());
    buf
}

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
// Header collection
// ===========================================================================

fn collect_headers_json(headers: &[httparse::Header<'_>]) -> String {
    let mut buf = String::from("[");
    let mut first = true;
    for h in headers {
        if h.name.is_empty() { continue; }
        if !first { buf.push(','); }
        first = false;
        let val = std::str::from_utf8(h.value).unwrap_or("");
        buf.push('[');
        buf.push('"');
        buf.push_str(h.name);
        buf.push('"');
        buf.push(',');
        buf.push('"');
        for ch in val.chars() {
            match ch {
                '"' => buf.push_str("\\\""),
                '\\' => buf.push_str("\\\\"),
                '\n' => buf.push_str("\\n"),
                '\r' => buf.push_str("\\r"),
                _ => buf.push(ch),
            }
        }
        buf.push('"');
        buf.push(']');
    }
    buf.push(']');
    buf
}

// ===========================================================================
// RPC dispatch
// ===========================================================================

async fn dispatch_rpc(
    body_bytes: &[u8],
    runtime: &Rc<RefCell<Runtime>>,
) -> Vec<u8> {
    let body_str = match std::str::from_utf8(body_bytes) {
        Ok(s) => s,
        Err(_) => return SERVICE_UNAVAILABLE.to_vec(),
    };

    let outcome = runtime.borrow_mut().dispatch_start(body_str);

    match outcome {
        DispatchOutcome::Complete(Ok(result)) => {
            build_json_response(&result.json)
        }
        DispatchOutcome::Complete(Err(e)) => {
            build_json_response(&format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\"")))
        }
        DispatchOutcome::Pending(rx) => {
            let wall_limit = runtime.borrow().wall_timeout();
            let deadline = wall_limit.map(|d| std::time::Instant::now() + d);
            loop {
                if let Some(result) = rx.try_recv() {
                    break match result {
                        Ok(r) => build_json_response(&r.json),
                        Err(e) => {
                            let escaped = e.replace('"', "\\\"");
                            build_json_response(&format!(r#"{{"error":"{escaped}"}}"#))
                        }
                    };
                }
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    break build_json_response(r#"{"error":"Request timed out"}"#);
                }
                yield_now().await;
            }
        }
        DispatchOutcome::HttpComplete { .. }
        | DispatchOutcome::HttpStream { .. }
        | DispatchOutcome::HttpPending(_)
        | DispatchOutcome::WebSocketUpgrade { .. } => {
            SERVICE_UNAVAILABLE.to_vec()
        }
    }
}

// ===========================================================================
// HTTP dispatch
// ===========================================================================

async fn dispatch_http(
    stream: &mut TcpStream,
    method: &str,
    url: &str,
    headers_json: &str,
    body: &str,
    runtime: &Rc<RefCell<Runtime>>,
    request_headers: &[(String, String)],
) -> bool {
    let outcome = runtime.borrow_mut().dispatch_http(method, url, headers_json, body);

    match outcome {
        DispatchOutcome::HttpComplete { status, headers, body, logs: _ } => {
            let response = build_http_response(status, &headers, &body);
            let BufResult(r, _) = stream.write_all(response).await;
            r.is_ok()
        }
        DispatchOutcome::HttpStream { status, headers, body, logs: _ } => {
            // Notify the pump that there may be new async tasks (e.g. timers
            // from an async ReadableStream start() callback).
            runtime.borrow_mut().notify_pump();

            let header_bytes = build_stream_response_headers(status, &headers);
            let BufResult(r, _) = stream.write_all(header_bytes).await;
            if r.is_err() { return false; }

            loop {
                for chunk in body.drain() {
                    let size_hex = format!("{:x}\r\n", chunk.len());
                    let mut chunk_data = Vec::with_capacity(size_hex.len() + chunk.len() + 2);
                    chunk_data.extend_from_slice(size_hex.as_bytes());
                    chunk_data.extend_from_slice(&chunk);
                    chunk_data.extend_from_slice(b"\r\n");
                    let BufResult(r, _) = stream.write_all(chunk_data).await;
                    if r.is_err() { return false; }
                }
                if body.is_done() { break; }
                // Sleep briefly to allow the compio event loop to fire timers
                // and drive async I/O (e.g. fetch responses, setTimeout).
                // yield_now() only yields to ready tasks — doesn't poll I/O.
                compio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            let BufResult(r, _) = stream.write_all(b"0\r\n\r\n".to_vec()).await;
            r.is_ok()
        }
        DispatchOutcome::HttpPending(rx) => {
            let wall_limit = runtime.borrow().wall_timeout();
            let deadline = wall_limit.map(|d| std::time::Instant::now() + d);
            let result = loop {
                if let Some(result) = rx.try_recv() {
                    break Some(result);
                }
                if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                    break None;
                }
                // Sleep briefly to let compio poll I/O events that the pump needs.
                compio::time::sleep(std::time::Duration::from_millis(1)).await;
            };

            match result {
                Some(Ok(HttpDispatchResult::Complete { status, headers, body, logs: _ })) => {
                    let response = build_http_response(status, &headers, &body);
                    let BufResult(r, _) = stream.write_all(response).await;
                    r.is_ok()
                }
                Some(Ok(HttpDispatchResult::Stream { status, headers, body, logs: _ })) => {
                    let header_bytes = build_stream_response_headers(status, &headers);
                    let BufResult(r, _) = stream.write_all(header_bytes).await;
                    if r.is_err() { return false; }

                    loop {
                        for chunk in body.drain() {
                            let size_hex = format!("{:x}\r\n", chunk.len());
                            let mut chunk_data = Vec::with_capacity(size_hex.len() + chunk.len() + 2);
                            chunk_data.extend_from_slice(size_hex.as_bytes());
                            chunk_data.extend_from_slice(&chunk);
                            chunk_data.extend_from_slice(b"\r\n");
                            let BufResult(r, _) = stream.write_all(chunk_data).await;
                            if r.is_err() { return false; }
                        }
                        if body.is_done() { break; }
                        compio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    let BufResult(r, _) = stream.write_all(b"0\r\n\r\n".to_vec()).await;
                    r.is_ok()
                }
                Some(Ok(HttpDispatchResult::WebSocket { ws_id, headers, logs: _ })) => {
                    handle_websocket_upgrade(stream, ws_id, &headers, request_headers, runtime).await
                }
                Some(Err(e)) => {
                    let body = format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\""));
                    let response = build_http_response(500, &[], &body);
                    let BufResult(r, _) = stream.write_all(response).await;
                    r.is_ok()
                }
                None => {
                    let response = build_http_response(504, &[], r#"{"error":"Request timed out"}"#);
                    let BufResult(r, _) = stream.write_all(response).await;
                    r.is_ok()
                }
            }
        }
        DispatchOutcome::Complete(Ok(result)) => {
            let response = build_http_response(200, &[("content-type".into(), "application/json".into())], &result.json);
            let BufResult(r, _) = stream.write_all(response).await;
            r.is_ok()
        }
        DispatchOutcome::Complete(Err(e)) => {
            let body = format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\""));
            let response = build_http_response(500, &[], &body);
            let BufResult(r, _) = stream.write_all(response).await;
            r.is_ok()
        }
        DispatchOutcome::WebSocketUpgrade { ws_id, headers } => {

            handle_websocket_upgrade(stream, ws_id, &headers, request_headers, runtime).await
        }
        DispatchOutcome::Pending(_) => {
            let response = build_http_response(500, &[], r#"{"error":"Unexpected pending state"}"#);
            let BufResult(r, _) = stream.write_all(response).await;
            r.is_ok()
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
async fn read_ws_frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    // Read the first 2 bytes: FIN/opcode + MASK/payload-len
    let mut header = vec![0u8; 2];
    let BufResult(r, returned) = stream.read(header).await;
    header = returned;
    if r.is_err() || r.as_ref().is_ok_and(|&n| n < 2) {
        return None;
    }

    let _fin = (header[0] & 0x80) != 0;
    let opcode = header[0] & 0x0F;
    let masked = (header[1] & 0x80) != 0;
    let mut payload_len = (header[1] & 0x7F) as u64;

    // Extended payload length
    if payload_len == 126 {
        let mut ext = vec![0u8; 2];
        let BufResult(r, returned) = stream.read(ext).await;
        ext = returned;
        if r.is_err() || r.as_ref().is_ok_and(|&n| n < 2) {
            return None;
        }
        payload_len = u16::from_be_bytes([ext[0], ext[1]]) as u64;
    } else if payload_len == 127 {
        let mut ext = vec![0u8; 8];
        let BufResult(r, returned) = stream.read(ext).await;
        ext = returned;
        if r.is_err() || r.as_ref().is_ok_and(|&n| n < 8) {
            return None;
        }
        payload_len = u64::from_be_bytes([ext[0], ext[1], ext[2], ext[3], ext[4], ext[5], ext[6], ext[7]]);
    }

    // Masking key (4 bytes if masked)
    let mask_key = if masked {
        let mut mk = vec![0u8; 4];
        let BufResult(r, returned) = stream.read(mk).await;
        mk = returned;
        if r.is_err() || r.as_ref().is_ok_and(|&n| n < 4) {
            return None;
        }
        Some([mk[0], mk[1], mk[2], mk[3]])
    } else {
        None
    };

    // Read payload
    let len = payload_len as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        let BufResult(r, returned) = stream.read(payload).await;
        payload = returned;
        if r.is_err() {
            return None;
        }
        // May need to read more if partial
        let read_n = r.unwrap_or(0);
        if read_n < len {
            // compio may return partial reads — keep reading
            let mut offset = read_n;
            while offset < len {
                let remaining = vec![0u8; len - offset];
                let BufResult(r2, returned2) = stream.read(remaining).await;
                let n2 = r2.unwrap_or(0);
                if n2 == 0 { return None; }
                payload[offset..offset + n2].copy_from_slice(&returned2[..n2]);
                offset += n2;
            }
        }
    }

    // Unmask
    if let Some(mk) = mask_key {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mk[i % 4];
        }
    }

    Some((opcode, payload))
}

/// Write a WebSocket frame to the stream (server-to-client: unmasked).
async fn write_ws_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> bool {
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
async fn handle_websocket_upgrade(
    stream: &mut TcpStream,
    ws_id: u32,
    response_headers: &[(String, String)],
    request_headers: &[(String, String)],
    runtime: &Rc<RefCell<Runtime>>,
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

    // Find the server-side WebSocket ID (the peer of ws_id, which is the client side)
    let server_ws_id = {
        let state = runtime.borrow().state().clone();
        let s = state.borrow();
        s.websockets.get(&ws_id).and_then(|ws| ws.peer_id).unwrap_or(0)
    };

    if server_ws_id == 0 {
        return false;
    }

    // Grab the notification handles for this WebSocket's outgoing queue.
    let (outgoing_ready, pump_waker) = {
        let state = runtime.borrow().state().clone();
        let s = state.borrow();
        if let Some(ws) = s.websockets.get(&server_ws_id) {
            (ws.outgoing_ready.clone(), ws.pump_waker.clone())
        } else {
            return false;
        }
    };

    // Bidirectional pump: TCP <-> JS
    // Event-driven: select between TCP read and outgoing notification.

    loop {
        // Drain any pending outgoing messages first (non-blocking).
        let mut got_close = false;
        {
            let outgoing: Vec<crate::state::WsMessage> = {
                outgoing_ready.set(false);
                let state = runtime.borrow().state().clone();
                let mut s = state.borrow_mut();
                if let Some(ws) = s.websockets.get_mut(&server_ws_id) {
                    ws.outgoing.drain(..).collect()
                } else {
                    return true;
                }
            };

            for msg in outgoing {
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
        }

        if got_close {
            return true;
        }

        // Wait for either: a TCP frame arrives, or JS queues an outgoing message.
        // Hand-rolled poll avoids Fuse wrapper + waker clone/drop overhead (~9% CPU).
        let event = WsPollBoth::new(
            read_ws_frame(stream),
            outgoing_ready.clone(),
            pump_waker.clone(),
        ).await;

        match event {
            WsEvent::Outgoing => {
                continue;
            }
            WsEvent::Frame(None) => {
                return true;
            }
            WsEvent::Frame(Some((0x1, payload))) | WsEvent::Frame(Some((0x2, payload))) => {
                // RFC 6455: text frames must be valid UTF-8. Use from_utf8 (no lossy scan).
                // Safety: if the client sends invalid UTF-8, we substitute rather than crash.
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
fn deliver_ws_message(
    runtime: &Rc<RefCell<Runtime>>,
    ws_id: u32,
    data: &str,
) {
    let mut rt = runtime.borrow_mut();
    rt.enter_v8_for_ws_message(ws_id, data);
}

/// Enter V8 to call `ws._onClose(code, reason)` on the server WebSocket.
fn deliver_ws_close(
    runtime: &Rc<RefCell<Runtime>>,
    ws_id: u32,
    code: u16,
    reason: &str,
) {
    let mut rt = runtime.borrow_mut();
    rt.enter_v8_for_ws_close(ws_id, code, reason);
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

// ===========================================================================
// Single-worker entry point
// ===========================================================================

fn run_single_worker(
    port: u16,
    use_reuseport: bool,
    worker_id: Option<usize>,
    cpu_limit: Option<Duration>,
    wall_timeout: Option<Duration>,
    modules: Vec<ModuleEntry>,
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
                eprintln!("[zeroship] worker {id} ready on port {port}");
            } else {
                eprintln!("[zeroship] http://0.0.0.0:{port}");
            }

            let runtime = Rc::new(RefCell::new(
                Runtime::new_direct(modules, HashMap::new(), cpu_limit, wall_timeout),
            ));

            // Warmup
            {
                let result = runtime.borrow_mut().dispatch_rpc(
                    r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":0}"#,
                );
                if let Err(e) = result {
                    eprintln!("[zeroship] warmup failed: {e}");
                }
            }

            // Start the async event loop pump (timers, fetch, streams).
            Runtime::start_pump(runtime.clone());

            // Accept loop
            loop {
                let (stream, _addr) = listener.accept().await.unwrap();
                let rt = runtime.clone();
                compio::runtime::spawn(handle_connection(stream, rt)).detach();
            }
        });
}
