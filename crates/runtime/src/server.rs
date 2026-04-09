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
use appbase_runtime::state::{IncomingRequest, RequestKind, RequestReply};
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
                kind: RequestKind::Rpc(r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":0}"#.to_string()),
                reply: reply_tx,
                cancel: CancellationToken::new(),
            })
            .unwrap();
        match reply_rx.blocking_recv().unwrap().unwrap() {
            RequestReply::Complete(_) => {}
            RequestReply::Stream(_) => panic!("unexpected stream reply on warmup"),
        }
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
    shared_cancel: CancellationToken,
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
            shared_cancel: CancellationToken::new(),
        }
    }

    async fn dispatch(&self, body: String) -> Result<appbase_runtime::init::RequestResult, String> {
        let idx = (self.next.fetch_add(1, Ordering::Relaxed) as usize) % self.senders.len();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        self.senders[idx]
            .send(IncomingRequest {
                id,
                kind: RequestKind::Rpc(body),
                reply: reply_tx,
                cancel: self.shared_cancel.clone(),
            })
            .await
            .map_err(|_| "Request channel closed".to_string())?;

        match reply_rx.await {
            Ok(Ok(RequestReply::Complete(result))) => Ok(result),
            Ok(Ok(RequestReply::Stream(_))) => Err("Streaming responses not supported in benchmark server".to_string()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("Reply channel closed".to_string()),
        }
    }
}

// ===========================================================================
// HTTP handler
// ===========================================================================

/// Static header value for "application/json" — avoids per-request allocation.
static JSON_CT: hyper::header::HeaderValue = hyper::header::HeaderValue::from_static("application/json");

async fn handle_request(
    req: Request<Incoming>,
    dispatcher: Arc<Dispatcher>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    match (req.method().clone(), req.uri().path()) {
        (hyper::Method::GET, "/health") => {
            let mut resp = Response::new(Full::new(Bytes::from(r#"{"status":"ok"}"#)));
            resp.headers_mut().insert(hyper::header::CONTENT_TYPE, JSON_CT.clone());
            Ok(resp)
        }
        (hyper::Method::POST, "/rpc") => {
            let body_bytes = http_body_util::BodyExt::collect(req.into_body())
                .await
                .unwrap()
                .to_bytes();
            let body_str = String::from_utf8(body_bytes.into()).unwrap_or_default();

            match dispatcher.dispatch(body_str).await {
                Ok(result) => {
                    let mut resp = Response::new(Full::new(Bytes::from(result.json)));
                    resp.headers_mut().insert(hyper::header::CONTENT_TYPE, JSON_CT.clone());
                    Ok(resp)
                }
                Err(e) => {
                    let mut resp = Response::new(Full::new(Bytes::from(format!(
                        r#"{{"error":"{}"}}"#,
                        e.replace('"', "\\\"")
                    ))));
                    *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                    resp.headers_mut().insert(hyper::header::CONTENT_TYPE, JSON_CT.clone());
                    Ok(resp)
                }
            }
        }
        _ => {
            let mut resp = Response::new(Full::new(Bytes::from("Not Found")));
            *resp.status_mut() = StatusCode::NOT_FOUND;
            Ok(resp)
        }
    }
}

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
