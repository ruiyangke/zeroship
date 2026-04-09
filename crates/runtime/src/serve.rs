//! Reusable compio HTTP server for appbase.
//!
//! Extracted from `server.rs` (the v8-server-compio binary) so that both the
//! benchmark binary and the CLI (`appbase serve`) can share the same server
//! logic.
//!
//! ## Usage
//!
//! ```ignore
//! use appbase_runtime::serve::{start_server, ServerOptions};
//! use appbase_runtime::ModuleEntry;
//!
//! let modules = vec![ModuleEntry { specifier: "index.js".into(), source: "...".into() }];
//! start_server(modules, ServerOptions { port: 3000, ..Default::default() });
//! ```

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use std::collections::HashMap;

use crate::init::init_v8;
use crate::modules::ModuleEntry;
use crate::runtime::{AsyncEvent, AsyncWork, DispatchOutcome, HttpDispatchResult, Runtime};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use futures::StreamExt;

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
            "[appbase] cpu_limit={:?} wall_timeout={:?}",
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
        eprintln!("[appbase] {num_workers} workers on port {}", options.port);
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

                    let wrote_ok = dispatch_http(
                        &mut stream, method, &full_url, &headers_json, body_str, &runtime,
                    ).await;
                    if !wrote_ok { return; }
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
        | DispatchOutcome::HttpPending(_) => {
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
) -> bool {
    let outcome = runtime.borrow_mut().dispatch_http(method, url, headers_json, body);

    match outcome {
        DispatchOutcome::HttpComplete { status, headers, body, logs: _ } => {
            let response = build_http_response(status, &headers, &body);
            let BufResult(r, _) = stream.write_all(response).await;
            r.is_ok()
        }
        DispatchOutcome::HttpStream { status, headers, body, logs: _ } => {
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
                yield_now().await;
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
                yield_now().await;
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
                        yield_now().await;
                    }
                    let BufResult(r, _) = stream.write_all(b"0\r\n\r\n".to_vec()).await;
                    r.is_ok()
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
        DispatchOutcome::Pending(_) => {
            let response = build_http_response(500, &[], r#"{"error":"Unexpected pending state"}"#);
            let BufResult(r, _) = stream.write_all(response).await;
            r.is_ok()
        }
    }
}

// ===========================================================================
// Pump task
// ===========================================================================

async fn pump_task(
    runtime: Rc<RefCell<Runtime>>,
    mut work: AsyncWork,
    mut notify_rx: futures::channel::mpsc::Receiver<()>,
) {
    loop {
        {
            let mut rt = runtime.borrow_mut();
            rt.drain_new_tasks_into(&mut work);
        }

        let event = {
            let has_ops = !work.pending_ops.is_empty();
            let has_timers = !work.pending_timers.is_empty();

            match (has_ops, has_timers) {
                (true, true) => {
                    futures::select! {
                        r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                        r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (true, false) => {
                    futures::select! {
                        r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (false, true) => {
                    futures::select! {
                        r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                        _ = notify_rx.next() => None,
                    }
                }
                (false, false) => {
                    let _ = notify_rx.next().await;
                    None
                }
            }
        };

        if let Some(event) = event {
            let mut rt = runtime.borrow_mut();
            rt.handle_async_event(event, &mut work);
        }
    }
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
                eprintln!("[appbase] worker {id} ready on port {port}");
            } else {
                eprintln!("[appbase] http://0.0.0.0:{port}");
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
                    eprintln!("[appbase] warmup failed: {e}");
                }
            }

            // Pump task setup
            let mut async_work = AsyncWork::new();
            let (notify_tx, notify_rx) = futures::channel::mpsc::channel::<()>(1);
            runtime.borrow_mut().set_pump_notify(notify_tx);
            runtime.borrow_mut().drain_new_tasks_into(&mut async_work);

            let rt_pump = runtime.clone();
            compio::runtime::spawn(async move {
                pump_task(rt_pump, async_work, notify_rx).await;
            }).detach();

            // Accept loop
            loop {
                let (stream, _addr) = listener.accept().await.unwrap();
                let rt = runtime.clone();
                compio::runtime::spawn(handle_connection(stream, rt)).detach();
            }
        });
}
