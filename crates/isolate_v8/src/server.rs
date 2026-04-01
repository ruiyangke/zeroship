//! Minimal HTTP server for raw V8 runtime -- benchmarkable with wrk/hey.
//!
//! Two modes:
//!   --mode=concurrent       (default) 1 ConcurrentIsolate, serial JS + concurrent I/O
//!   --mode=concurrent-pool  N ConcurrentIsolates, round-robin dispatch
//!
//! POST /rpc -> dispatch to V8 -> JSON-RPC response
//! GET /health -> {"status":"ok"}

use appbase_isolate_v8::concurrent::{ConcurrentIsolate, Event};
use appbase_isolate_v8::init_v8;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;

/// Default JS loaded when no --js flag is provided.
/// Loads shared scenarios from benches/scenarios.js at build time.
const SERVER_JS: &str = include_str!("../benches/scenarios.js");

// ===========================================================================
// Spawn + warmup helper
// ===========================================================================

/// Spawn a ConcurrentIsolate on a dedicated thread and warmup with a ping.
/// Returns the event sender for dispatching requests.
///
/// `cpu_limit` — if `Some`, arms a POSIX CPU timer per request batch (Linux only).
fn spawn_and_warmup(
    name: &str,
    cpu_limit: Option<std::time::Duration>,
) -> std::sync::mpsc::Sender<Event> {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let event_tx_clone = event_tx.clone();
    let js = SERVER_JS.to_string();
    let thread_name = name.to_string();
    let tokio_handle = tokio::runtime::Handle::current();

    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let mut isolate = ConcurrentIsolate::new(
                &js, event_rx, event_tx_clone, Some(tokio_handle), cpu_limit,
            );
            isolate.run_event_loop();
        })
        .unwrap();

    // Warmup (on a separate thread to avoid blocking tokio runtime)
    let warmup_tx = event_tx.clone();
    std::thread::spawn(move || {
        let (tx, rx) = tokio::sync::oneshot::channel();
        warmup_tx
            .send(Event::NewRequest {
                id: 0,
                body: r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":0}"#.to_string(),
                reply: tx,
            })
            .unwrap();
        rx.blocking_recv().unwrap().unwrap();
    })
    .join()
    .unwrap();

    event_tx
}

// ===========================================================================
// Dispatcher — 1 or N concurrent isolates
// ===========================================================================

struct Dispatcher {
    senders: Vec<std::sync::mpsc::Sender<Event>>,
    next: AtomicU64,
    next_id: AtomicU64,
}

impl Dispatcher {
    fn new(num_workers: usize) -> Self {
        let cpu_limit = Some(std::time::Duration::from_secs(5));
        let senders: Vec<_> = (0..num_workers)
            .map(|i| spawn_and_warmup(&format!("v8-worker-{i}"), cpu_limit))
            .collect();

        Self {
            senders,
            next: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
        }
    }

    async fn dispatch(&self, body: String) -> Result<appbase_isolate_v8::RequestResult, String> {
        let idx = (self.next.fetch_add(1, Ordering::Relaxed) as usize) % self.senders.len();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.senders[idx]
            .send(Event::NewRequest {
                id,
                body,
                reply: reply_tx,
            })
            .map_err(|_| "Event channel closed".to_string())?;

        match tokio::time::timeout(std::time::Duration::from_secs(30), reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Reply channel closed".to_string()),
            Err(_) => Err("Request timed out (30s wall time)".to_string()),
        }
    }
}

impl Drop for Dispatcher {
    fn drop(&mut self) {
        for tx in &self.senders {
            let _ = tx.send(Event::Shutdown);
        }
    }
}

// ===========================================================================
// HTTP handler
// ===========================================================================

async fn handle_request(
    req: Request<Incoming>,
    dispatcher: Arc<Dispatcher>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    match (req.method().clone(), req.uri().path()) {
        (hyper::Method::GET, "/health") => Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(r#"{"status":"ok"}"#)))
            .unwrap()),
        (hyper::Method::POST, "/rpc") => {
            let body = http_body_util::BodyExt::collect(req.into_body())
                .await
                .unwrap()
                .to_bytes();
            let body_str = String::from_utf8_lossy(&body).to_string();

            match dispatcher.dispatch(body_str).await {
                Ok(result) => Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "application/json")
                    .header(
                        "x-cpu-time-ms",
                        format!("{:.3}", result.cpu_time.as_secs_f64() * 1000.0),
                    )
                    .header(
                        "x-wall-time-ms",
                        format!("{:.3}", result.wall_time.as_secs_f64() * 1000.0),
                    )
                    .body(Full::new(Bytes::from(result.json)))
                    .unwrap()),
                Err(e) => Ok(Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(format!(
                        r#"{{"error":"{}"}}"#,
                        e.replace('"', "\\\"")
                    ))))
                    .unwrap()),
            }
        }
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from("Not Found")))
            .unwrap()),
    }
}

#[tokio::main]
async fn main() {
    init_v8();

    let mode = std::env::args()
        .find(|a| a.starts_with("--mode="))
        .map(|a| a.strip_prefix("--mode=").unwrap().to_string())
        .unwrap_or_else(|| "concurrent".to_string());

    let port: u16 = std::env::args()
        .find(|a| a.starts_with("--port="))
        .and_then(|a| a.strip_prefix("--port=").unwrap().parse().ok())
        .unwrap_or(4000);

    let num_workers = match mode.as_str() {
        "concurrent-pool" => {
            let n = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8);
            eprintln!("[v8-server] mode=concurrent-pool ({n} V8 threads)");
            n
        }
        _ => {
            eprintln!("[v8-server] mode=concurrent (1 V8 thread)");
            1
        }
    };

    let dispatcher = Arc::new(Dispatcher::new(num_workers));

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await.unwrap();
    eprintln!("[v8-server] http://{addr}");

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let dispatcher = dispatcher.clone();

        tokio::task::spawn(async move {
            let service = service_fn(move |req| {
                let d = dispatcher.clone();
                handle_request(req, d)
            });
            if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                if !e.is_incomplete_message() {
                    eprintln!("[v8-server] Error: {e}");
                }
            }
        });
    }
}
