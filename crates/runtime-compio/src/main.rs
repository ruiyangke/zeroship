//! Minimal HTTP server for V8 runtime using compio (io_uring).
//!
//! POST /rpc -> dispatch to V8 -> JSON-RPC response
//! GET /health -> {"status":"ok"}
//!
//! Uses httparse for zero-copy HTTP parsing and compio for io_uring I/O.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use appbase_runtime_compio::modules::ModuleEntry;
use appbase_runtime_compio::runtime::Runtime;
use appbase_runtime_compio::state::{IncomingRequest, RequestKind, RequestReply};
use appbase_runtime_compio::init_v8;
use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Default JS loaded when no --js flag is provided.
const SERVER_JS: &str = include_str!("../benches/scenarios.js");

fn server_modules() -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: SERVER_JS.into(),
    }]
}

// ===========================================================================
// HTTP connection handler (compio I/O)
// ===========================================================================

/// Monotonic request ID counter (shared across connection handlers).
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Shared no-op cancellation token. Cloning is an Arc clone (cheap) vs.
/// `CancellationToken::new()` which allocates a new tree node per call.
/// This token is never cancelled and serves requests that have no timeout.
static SHARED_CANCEL: OnceLock<CancellationToken> = OnceLock::new();

fn shared_cancel() -> CancellationToken {
    SHARED_CANCEL.get_or_init(CancellationToken::new).clone()
}

/// Static HTTP response parts.
const HEALTH_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}";
const NOT_FOUND_RESPONSE: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";

async fn handle_connection(
    mut stream: TcpStream,
    req_tx: tokio::sync::mpsc::Sender<IncomingRequest>,
) {
    // Accumulation buffer for incoming data
    let mut data = Vec::with_capacity(8192);

    loop {
        // Read into a fresh buffer (compio takes ownership)
        let read_buf = Vec::with_capacity(4096);
        let BufResult(result, read_buf) = stream.read(read_buf).await;

        let n = match result {
            Ok(0) => return,   // connection closed
            Ok(n) => n,
            Err(_) => return,  // read error
        };

        // Append the read data to our accumulation buffer
        data.extend_from_slice(&read_buf[..n]);

        // Try to parse one or more HTTP requests from the accumulated data
        loop {
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut req = httparse::Request::new(&mut headers);

            let header_len = match req.parse(&data) {
                Ok(httparse::Status::Complete(len)) => len,
                Ok(httparse::Status::Partial) => break, // need more data
                Err(_) => return,                       // parse error
            };

            let method = req.method.unwrap_or("GET");
            let path = req.path.unwrap_or("/");

            // Extract Content-Length
            let content_length: usize = headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                .and_then(|h| std::str::from_utf8(h.value).ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);

            let total_len = header_len + content_length;
            if data.len() < total_len {
                break; // need more body data
            }

            let body_bytes = &data[header_len..total_len];

            // Route
            let response_bytes: Vec<u8> = match (method, path) {
                ("GET", "/health") => HEALTH_RESPONSE.to_vec(),
                ("POST", "/rpc") => {
                    dispatch_rpc(body_bytes, &req_tx).await
                }
                _ => NOT_FOUND_RESPONSE.to_vec(),
            };

            // Write response (compio takes ownership of the Vec)
            let BufResult(write_result, _) = stream.write_all(response_bytes).await;
            if write_result.is_err() {
                return;
            }

            // Consume the processed request from the buffer
            data.drain(..total_len);

            if data.is_empty() {
                break; // no more data, go back to reading
            }
            // Otherwise loop to parse next pipelined request
        }
    }
}

async fn dispatch_rpc(
    body_bytes: &[u8],
    req_tx: &tokio::sync::mpsc::Sender<IncomingRequest>,
) -> Vec<u8> {
    let body_str = String::from_utf8_lossy(body_bytes).into_owned();

    let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let cancel = shared_cancel();

    if req_tx
        .send(IncomingRequest {
            id,
            kind: RequestKind::Rpc(body_str),
            reply: reply_tx,
            cancel,
        })
        .await
        .is_err()
    {
        return b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".to_vec();
    }

    let reply = match reply_rx.await {
        Ok(reply) => reply,
        Err(_) => {
            return b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".to_vec();
        }
    };

    let response_body = match reply {
        Ok(RequestReply::Complete(result)) => result.json,
        Ok(RequestReply::Stream(_)) => r#"{"error":"streaming not supported"}"#.to_string(),
        Err(e) => format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\"")),
    };

    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        response_body.len(),
        response_body
    )
    .into_bytes()
}

// ===========================================================================
// V8 event loop
// ===========================================================================

async fn v8_loop(
    req_rx: tokio::sync::mpsc::Receiver<IncomingRequest>,
    modules: Vec<ModuleEntry>,
) {
    let shutdown = CancellationToken::new();
    let mut runtime = Runtime::new(modules, req_rx, shutdown, HashMap::new());
    runtime.run().await;
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
// Single-worker entry point (one compio runtime + one V8 isolate)
// ===========================================================================

fn run_single_worker(port: u16, use_reuseport: bool, worker_id: Option<usize>) {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            let listener = if use_reuseport {
                // Convert std TcpListener -> compio TcpListener via RawFd
                let std_listener = create_reuseport_listener(port);
                unsafe {
                    use std::os::fd::{FromRawFd, IntoRawFd};
                    TcpListener::from_raw_fd(std_listener.into_raw_fd())
                }
            } else {
                TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap()
            };

            if let Some(id) = worker_id {
                eprintln!("[v8-server-compio] worker {id} ready on port {port}");
            } else {
                eprintln!("[v8-server-compio] http://0.0.0.0:{port}");
            }

            // Channel from HTTP handlers to V8 event loop
            let (req_tx, req_rx) = tokio::sync::mpsc::channel::<IncomingRequest>(1024);

            // Spawn V8 event loop
            let modules = server_modules();
            compio::runtime::spawn(v8_loop(req_rx, modules)).detach();

            // Warmup: send a ping and wait
            {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                req_tx
                    .send(IncomingRequest {
                        id: 0,
                        kind: RequestKind::Rpc(
                            r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":0}"#.to_string(),
                        ),
                        reply: reply_tx,
                        cancel: shared_cancel(),
                    })
                    .await
                    .unwrap();
                let _ = reply_rx.await;
            }

            // Accept loop
            loop {
                let (stream, _addr) = listener.accept().await.unwrap();
                let tx = req_tx.clone();
                compio::runtime::spawn(handle_connection(stream, tx)).detach();
            }
        });
}

// ===========================================================================
// main
// ===========================================================================

fn main() {
    init_v8();

    let port: u16 = std::env::args()
        .find(|a| a.starts_with("--port="))
        .and_then(|a| a.strip_prefix("--port=").unwrap().parse().ok())
        .unwrap_or(5000);

    let num_workers: usize = std::env::args()
        .find(|a| a.starts_with("--workers="))
        .and_then(|a| a.strip_prefix("--workers=").unwrap().parse().ok())
        .unwrap_or(1);

    if num_workers <= 1 {
        // Single-worker: no SO_REUSEPORT needed
        run_single_worker(port, false, None);
    } else {
        // Multi-worker: each thread gets its own compio runtime + V8 isolate
        eprintln!("[v8-server-compio] {num_workers} workers on port {port}");
        let mut handles = Vec::new();
        for i in 0..num_workers {
            let handle = std::thread::Builder::new()
                .name(format!("compio-worker-{i}"))
                .spawn(move || {
                    run_single_worker(port, true, Some(i));
                })
                .unwrap();
            handles.push(handle);
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}
