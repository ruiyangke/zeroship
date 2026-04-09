//! Minimal HTTP server for V8 runtime using compio (io_uring).
//!
//! POST /rpc -> dispatch to V8 -> JSON-RPC response
//! GET /health -> {"status":"ok"}
//!
//! Uses httparse for zero-copy HTTP parsing and compio for io_uring I/O.
//! Connection handlers call V8 directly via Rc<RefCell<Runtime>> — no channel.
//!
//! ## Async dispatch architecture
//!
//! Sync handlers (ping, fib): `dispatch_start` → Complete → immediate response.
//! Async handlers (setTimeout, fetch): `dispatch_start` → Pending → await oneshot.
//! A pump task owns `AsyncWork`, polls pending ops/timers, enters V8 briefly
//! to resolve promises, and sends results via the oneshot channels.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use appbase_runtime_compio::modules::ModuleEntry;
use appbase_runtime_compio::runtime::Runtime;
use appbase_runtime_compio::{AsyncWork, AsyncEvent, DispatchOutcome};
use appbase_runtime_compio::init_v8;
use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use futures::StreamExt;

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
// HTTP connection handler (compio I/O, direct V8 dispatch)
// ===========================================================================

/// Static HTTP response parts.
const HEALTH_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}";
const NOT_FOUND_RESPONSE: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found";
const SERVICE_UNAVAILABLE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";

async fn handle_connection(
    mut stream: TcpStream,
    runtime: Rc<RefCell<Runtime>>,
) {
    // Accumulation buffer for incoming data
    let mut data = Vec::with_capacity(8192);
    // Reusable read buffer — compio takes ownership then returns it
    let mut read_buf = Vec::with_capacity(4096);

    loop {
        // Reuse the read buffer across reads (avoid allocation per read)
        read_buf.clear();
        let BufResult(result, returned_buf) = stream.read(read_buf).await;
        read_buf = returned_buf;

        let n = match result {
            Ok(0) => return,   // connection closed
            Ok(n) => n,
            Err(_) => return,  // read error
        };

        // Append the read data to our accumulation buffer
        data.extend_from_slice(&read_buf[..n]);

        // Cursor: track how far we've consumed instead of drain() per request
        let mut consumed = 0;

        // Try to parse one or more HTTP requests from the accumulated data
        loop {
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut req = httparse::Request::new(&mut headers);

            let header_len = match req.parse(&data[consumed..]) {
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
            if data.len() - consumed < total_len {
                break; // need more body data
            }

            let body_bytes = &data[consumed + header_len..consumed + total_len];

            // Route
            let response_bytes: Vec<u8> = match (method, path) {
                ("GET", "/health") => HEALTH_RESPONSE.to_vec(),
                ("POST", "/rpc") => {
                    dispatch_rpc(body_bytes, &runtime).await
                }
                _ => NOT_FOUND_RESPONSE.to_vec(),
            };

            // Write response (compio takes ownership of the Vec)
            let BufResult(write_result, _) = stream.write_all(response_bytes).await;
            if write_result.is_err() {
                return;
            }

            // Advance cursor past this request
            consumed += total_len;

            if consumed >= data.len() {
                break; // no more data, go back to reading
            }
            // Otherwise loop to parse next pipelined request
        }

        // Drain consumed bytes once (not per-request)
        if consumed >= data.len() {
            data.clear();
        } else if consumed > 0 {
            data.drain(..consumed);
        }
    }
}

/// Build an HTTP 200 JSON response without format!() overhead.
/// Uses itoa for Content-Length and direct byte assembly.
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

/// Dispatch a JSON-RPC request into V8 with async support.
///
/// - Sync handlers: borrow Runtime briefly, return immediately.
/// - Async handlers: borrow Runtime briefly for dispatch_start, then release
///   the borrow and await the oneshot (pump task drives the promise to settlement).
///
/// The RefCell borrow is NEVER held across any .await point.
async fn dispatch_rpc(
    body_bytes: &[u8],
    runtime: &Rc<RefCell<Runtime>>,
) -> Vec<u8> {
    let body_str = match std::str::from_utf8(body_bytes) {
        Ok(s) => s,
        Err(_) => return SERVICE_UNAVAILABLE.to_vec(),
    };

    // Phase 1: dispatch into V8 (scoped borrow)
    let outcome = runtime.borrow_mut().dispatch_start(body_str);

    // Phase 2: handle outcome
    match outcome {
        DispatchOutcome::Complete(Ok(result)) => {
            build_json_response(&result.json)
        }
        DispatchOutcome::Complete(Err(e)) => {
            build_json_response(&format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\"")))
        }
        DispatchOutcome::Pending(rx) => {
            // Await the pump task settling this promise.
            // The RefCell borrow is NOT held here — other tasks can run.
            match rx.await {
                Ok(Ok(result)) => build_json_response(&result.json),
                Ok(Err(e)) => {
                    build_json_response(&format!(r#"{{"error":"{}"}}"#, e.replace('"', "\\\"")))
                }
                Err(_) => {
                    // Oneshot dropped — pump shut down
                    SERVICE_UNAVAILABLE.to_vec()
                }
            }
        }
    }
}

// ===========================================================================
// Pump task — drives async V8 work (ops, timers) to completion
// ===========================================================================

/// Background task that owns `AsyncWork` and drives pending ops/timers.
///
/// On each iteration:
/// 1. Drain newly spawned tasks from Runtime into AsyncWork
/// 2. Wait for the next op or timer to complete (truly async — compio wakes us)
/// 3. Briefly borrow Runtime to enter V8 and handle the result
/// 4. Check if any promises settled, send results via oneshot channels
///
/// The RefCell borrow on Runtime is scoped and NEVER held across .await.
async fn pump_task(runtime: Rc<RefCell<Runtime>>, mut work: AsyncWork) {
    loop {
        // Drain any newly spawned tasks (from dispatch_start calls)
        {
            let mut rt = runtime.borrow_mut();
            rt.drain_new_tasks_into(&mut work);
        }

        // If no pending work and no pending requests, just yield and check again
        let has_pending = {
            let rt = runtime.borrow();
            rt.has_pending_requests()
        };
        if work.pending_ops.is_empty() && work.pending_timers.is_empty() && !has_pending {
            // Nothing to do — sleep briefly to avoid busy-spin, then check again
            compio::time::sleep(std::time::Duration::from_millis(1)).await;
            continue;
        }

        // Wait for the next event from pending ops or timers.
        // This is a true async wait — compio's reactor wakes us when I/O
        // completes or a timer fires. No busy-spinning.
        let event = {
            // Use futures::select! to wait for whichever completes first.
            // If one collection is empty, select_next_some would never resolve,
            // so we guard with is_empty checks.
            let has_ops = !work.pending_ops.is_empty();
            let has_timers = !work.pending_timers.is_empty();

            match (has_ops, has_timers) {
                (true, true) => {
                    futures::select! {
                        r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                        r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                    }
                }
                (true, false) => {
                    let r = work.pending_ops.select_next_some().await;
                    Some(AsyncEvent::Op(r))
                }
                (false, true) => {
                    let r = work.pending_timers.select_next_some().await;
                    Some(AsyncEvent::Timer(r))
                }
                (false, false) => {
                    // No futures to poll — yield so connection handlers can dispatch
                    compio::time::sleep(std::time::Duration::from_millis(1)).await;
                    None
                }
            }
        };

        if let Some(event) = event {
            // Briefly borrow Runtime to enter V8 and handle the event
            let mut rt = runtime.borrow_mut();
            rt.handle_async_event(event, &mut work);
            // drain_new_tasks_into is called inside handle_async_event
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
// Single-worker entry point (one compio runtime + one V8 isolate)
// ===========================================================================

fn run_single_worker(port: u16, use_reuseport: bool, worker_id: Option<usize>) {
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
                eprintln!("[v8-server-compio] worker {id} ready on port {port}");
            } else {
                eprintln!("[v8-server-compio] http://0.0.0.0:{port}");
            }

            // Create Runtime directly — no channel, no V8 loop task
            let runtime = Rc::new(RefCell::new(
                Runtime::new_direct(server_modules(), HashMap::new()),
            ));

            // Warmup: dispatch a ping directly (uses dispatch_rpc for sync)
            {
                let result = runtime.borrow_mut().dispatch_rpc(
                    r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":0}"#,
                );
                if let Err(e) = result {
                    eprintln!("[v8-server-compio] warmup failed: {e}");
                }
            }

            // Spawn the pump task — owns AsyncWork, polls pending ops/timers,
            // briefly borrows Runtime to enter V8 and resolve promises.
            let mut async_work = AsyncWork::new();

            // Initial drain: pick up any tasks from warmup
            runtime.borrow_mut().drain_new_tasks_into(&mut async_work);

            let rt_pump = runtime.clone();
            compio::runtime::spawn(async move {
                pump_task(rt_pump, async_work).await;
            }).detach();

            // Accept loop
            loop {
                let (stream, _addr) = listener.accept().await.unwrap();
                let rt = runtime.clone();
                compio::runtime::spawn(handle_connection(stream, rt)).detach();
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
        run_single_worker(port, false, None);
    } else {
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
