//! Minimal HTTP server for raw V8 runtime -- benchmarkable with wrk/hey.
//!
//! Two modes:
//!   --mode=concurrent       (default) 1 Runtime worker, serial JS + concurrent I/O
//!   --mode=concurrent-pool  N Runtime workers, round-robin dispatch
//!
//! POST /rpc -> dispatch to V8 -> JSON-RPC response
//! GET /health -> {"status":"ok"}

use appbase_runtime::modules::ModuleEntry;
use appbase_runtime::runtime::Runtime;
use appbase_runtime::state::IncomingRequest;
use appbase_runtime::init_v8;
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
use tokio_util::sync::CancellationToken;

/// Default JS loaded when no --js flag is provided.
/// Loads shared scenarios from benches/scenarios.js at build time (ESM format).
const SERVER_JS: &str = include_str!("../benches/scenarios.js");

fn server_modules() -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: SERVER_JS.into(),
    }]
}

// ===========================================================================
// Spawn + warmup helper
// ===========================================================================

/// Spawn a Runtime on a dedicated thread and warmup with a ping.
/// Returns the request sender for dispatching requests.
fn spawn_and_warmup(name: &str) -> tokio::sync::mpsc::Sender<IncomingRequest> {
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<IncomingRequest>(1024);
    let modules = server_modules();
    let thread_name = name.to_string();
    let shutdown = CancellationToken::new();

    // Capture the server's multi-threaded tokio handle so fetch I/O
    // runs on the server's thread pool instead of the isolate's single-threaded runtime.
    let server_handle = tokio::runtime::Handle::current();

    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let mut runtime = Runtime::new(
                    modules,
                    req_rx,
                    shutdown,
                    None,
                    None,
                    std::collections::HashMap::new(),
                    Some(server_handle),
                );
                runtime.run().await;
            });
        })
        .unwrap();

    // Warmup: send a ping and wait for the reply
    let warmup_tx = req_tx.clone();
    std::thread::spawn(move || {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        warmup_tx
            .blocking_send(IncomingRequest {
                id: 0,
                body: r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":0}"#.to_string(),
                reply: reply_tx,
                cancel: CancellationToken::new(),
            })
            .unwrap();
        reply_rx.blocking_recv().unwrap().unwrap();
    })
    .join()
    .unwrap();

    req_tx
}

// ===========================================================================
// Dispatcher — 1 or N Runtime workers
// ===========================================================================

struct Dispatcher {
    senders: Vec<tokio::sync::mpsc::Sender<IncomingRequest>>,
    next: AtomicU64,
    next_id: AtomicU64,
}

impl Dispatcher {
    fn new(num_workers: usize) -> Self {
        let senders: Vec<_> = (0..num_workers)
            .map(|i| spawn_and_warmup(&format!("v8-worker-{i}")))
            .collect();

        Self {
            senders,
            next: AtomicU64::new(0),
            next_id: AtomicU64::new(1),
        }
    }

    async fn dispatch(&self, body: String) -> Result<appbase_runtime::init::RequestResult, String> {
        let idx = (self.next.fetch_add(1, Ordering::Relaxed) as usize) % self.senders.len();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        self.senders[idx]
            .send(IncomingRequest {
                id,
                body,
                reply: reply_tx,
                cancel: CancellationToken::new(),
            })
            .await
            .map_err(|_| "Request channel closed".to_string())?;

        match tokio::time::timeout(std::time::Duration::from_secs(30), reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("Reply channel closed".to_string()),
            Err(_) => Err("Request timed out (30s wall time)".to_string()),
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
