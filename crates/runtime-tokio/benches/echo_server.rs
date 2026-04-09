//! Async HTTP echo server for fetch benchmarks.
//! Returns `{"ok":true}` for any request. Handles 100K+ concurrent connections.

use bytes::Bytes;
use http_body_util::Full;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::TcpListener;

const BODY: &str = r#"{"ok":true}"#;

async fn handle(_req: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    Ok(Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(BODY)))
        .unwrap())
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(8888);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await.unwrap();
    eprintln!("[echo-rs] http://{addr} (async, hyper)");

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        tokio::task::spawn(async move {
            let _ = http1::Builder::new()
                .serve_connection(io, service_fn(handle))
                .await;
        });
    }
}
