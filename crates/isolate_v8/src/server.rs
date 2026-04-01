//! Minimal HTTP server for raw V8 runtime -- benchmarkable with wrk/hey.
//!
//! Uses per-thread V8 isolates via dedicated worker threads.
//! POST /rpc -> dispatch to worker -> JSON-RPC response
//! GET /health -> {"status":"ok"}

use appbase_isolate_v8::{init_v8, Isolate};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

const SERVER_JS: &str = r#"
var __rpc = {
    ping: function() { return "pong"; },
    echo: function(msg) { return msg; },
    add: function(a, b) { return a + b; },
    fib: function(n) {
        function fib(n) { return n <= 1 ? n : fib(n-1) + fib(n-2); }
        return fib(n);
    },
    delayed: function() {
        return new Promise(function(resolve) {
            setTimeout(function() { resolve("done"); }, 1);
        });
    }
};
"#;

struct WorkRequest {
    body: String,
    reply: oneshot::Sender<Result<appbase_isolate_v8::RequestResult, String>>,
}

/// Spawn a V8 worker thread with its own isolate.
/// Returns a channel to send work to it.
fn spawn_worker(id: usize) -> mpsc::Sender<WorkRequest> {
    let (tx, mut rx) = mpsc::channel::<WorkRequest>(256);

    std::thread::Builder::new()
        .name(format!("v8-worker-{id}"))
        .spawn(move || {
            // V8 isolate created and used only on THIS thread
            let mut isolate = Isolate::new(SERVER_JS);
            // Warmup
            isolate
                .execute_request(r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":0}"#)
                .unwrap();

            // Process requests
            while let Some(req) = rx.blocking_recv() {
                let result = isolate.execute_request(&req.body);
                let _ = req.reply.send(result);
            }
        })
        .unwrap();

    tx
}

/// Round-robin dispatcher across worker threads.
struct Dispatcher {
    workers: Vec<mpsc::Sender<WorkRequest>>,
    next: std::sync::atomic::AtomicUsize,
}

impl Dispatcher {
    fn new(num_workers: usize) -> Self {
        let workers: Vec<_> = (0..num_workers).map(spawn_worker).collect();
        Self {
            workers,
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    async fn dispatch(&self, body: String) -> Result<appbase_isolate_v8::RequestResult, String> {
        let idx = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.workers.len();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.workers[idx]
            .send(WorkRequest {
                body,
                reply: reply_tx,
            })
            .await
            .map_err(|_| "Worker channel closed".to_string())?;
        reply_rx
            .await
            .map_err(|_| "Worker dropped reply".to_string())?
    }
}

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

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);

    let dispatcher = Arc::new(Dispatcher::new(num_workers));

    let addr = SocketAddr::from(([0, 0, 0, 0], 4000));
    let listener = TcpListener::bind(addr).await.unwrap();
    eprintln!("[v8-server] http://{addr}");
    eprintln!("[v8-server] {num_workers} V8 worker threads (1 isolate each)");

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
