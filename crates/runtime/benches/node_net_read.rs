//! node:net inbound read throughput bench.
//!
//! Drives the production runtime's `node:net` client path against a local TCP
//! source server. The measured section reads a fixed 16 MiB payload into JS so
//! the native read pump, `SocketEvent::Data`, and the unavoidable V8 Buffer
//! boundary copy are all exercised.

#![allow(unsafe_code)]

use std::io::Write;
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, HostPort, ModuleEntry, NetPolicy, RequestCtx, Runtime,
    SettledFetch,
};

const PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
const MAX_WAIT: Duration = Duration::from_secs(15);

struct EnvGuard {
    prev_dev: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set_dev() -> Self {
        let prev_dev = std::env::var_os("ZEROSHIP_DEV");
        unsafe {
            std::env::set_var("ZEROSHIP_DEV", "1");
        }
        Self { prev_dev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_dev {
                Some(value) => std::env::set_var("ZEROSHIP_DEV", value),
                None => std::env::remove_var("ZEROSHIP_DEV"),
            }
        }
    }
}

fn spawn_tcp_source_server(payload: Arc<Vec<u8>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind node:net bench source");
    let addr = listener
        .local_addr()
        .expect("node:net bench source local_addr");

    thread::spawn(move || {
        for accepted in listener.incoming() {
            let Ok(mut stream) = accepted else {
                break;
            };
            let _ = stream.set_nodelay(true);
            if stream.write_all(payload.as_slice()).is_ok() {
                let _ = stream.shutdown(Shutdown::Write);
            }
        }
    });

    addr
}

fn build_runtime(addr: SocketAddr) -> Runtime {
    init_v8();
    let module = format!(
        r#"
import net from "node:net";

export default {{
    async fetch() {{
        const total = await new Promise((resolve) => {{
            const socket = net.createConnection({{
                host: "127.0.0.1",
                port: {},
            }});
            let bytes = 0;
            socket.on("data", (chunk) => {{
                bytes += chunk.length;
            }});
            socket.on("end", () => resolve(bytes));
            socket.on("error", (err) => resolve(`error:${{err.code || ""}}:${{err.message}}:${{bytes}}`));
            setTimeout(() => resolve(`timeout:${{bytes}}`), {});
        }});
        return new Response(String(total), {{
            headers: {{ "content-type": "text/plain" }},
        }});
    }},
}};
"#,
        addr.port(),
        MAX_WAIT.as_millis() - 1000
    );
    let modules = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: module,
    }];
    Runtime::builder()
        .modules(modules)
        .net_policy(
            NetPolicy::allowlist(
                vec![HostPort::new("127.0.0.1", addr.port())],
                4,
                PAYLOAD_BYTES as u64,
            )
            .expect("valid node:net bench net policy"),
        )
        .idle_gc_after_ms(0)
        .build()
}

async fn read_payload_once(runtime: &Runtime) -> usize {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    let body = response_body(outcome).await;
    let text = String::from_utf8(body).expect("node:net bench response body is utf-8");
    text.parse::<usize>()
        .unwrap_or_else(|err| panic!("node:net bench expected byte count, got {text:?}: {err}"))
}

async fn response_body(outcome: FetchOutcome) -> Vec<u8> {
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "node:net bench response status");
            body
        }
        FetchOutcome::Pending { rx, .. } => {
            match compio::time::timeout(MAX_WAIT, rx.recv()).await {
                Ok(Ok(SettledFetch::Response { status, body, .. })) => {
                    assert_eq!(status, 200, "node:net bench pending response status");
                    body
                }
                Ok(Ok(SettledFetch::Stream { status, body_reader, .. })) => {
                    assert_eq!(status, 200, "node:net bench pending stream status");
                    let mut body = Vec::new();
                    for chunk in body_reader.drain() {
                        body.extend_from_slice(&chunk);
                    }
                    body
                }
                Ok(Ok(SettledFetch::WebSocketUpgrade { .. })) => {
                    panic!("node:net bench got unexpected websocket upgrade")
                }
                Ok(Err(err)) => panic!("node:net bench dispatch error: {}", err.message),
                Err(_) => panic!("node:net bench timed out waiting for fetch response"),
            }
        }
        FetchOutcome::Stream {
            status,
            body_reader,
            ..
        } => {
            assert_eq!(status, 200, "node:net bench stream response status");
            let mut body = Vec::new();
            for chunk in body_reader.drain() {
                body.extend_from_slice(&chunk);
            }
            body
        }
        FetchOutcome::WebSocketUpgrade { .. } => {
            panic!("node:net bench got unexpected websocket upgrade")
        }
    }
}

fn bench_node_net_read(c: &mut Criterion) {
    let _env = EnvGuard::set_dev();
    let payload = Arc::new(vec![b'x'; PAYLOAD_BYTES]);
    let addr = spawn_tcp_source_server(payload);
    let compio_rt = compio::runtime::Runtime::new()
        .expect("failed to create compio runtime for node:net read bench");
    let runtime = build_runtime(addr);

    compio_rt.block_on(async {
        runtime.start_pump();
    });

    let warm = compio_rt.block_on(read_payload_once(&runtime));
    assert_eq!(warm, PAYLOAD_BYTES, "node:net read bench warmup bytes");

    let mut group = c.benchmark_group("node_net_read/plain_tcp_source");
    group.throughput(Throughput::Bytes(PAYLOAD_BYTES as u64));
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));
    group.bench_function("16MiB", |b| {
        b.iter(|| {
            let bytes = compio_rt.block_on(read_payload_once(&runtime));
            assert_eq!(bytes, PAYLOAD_BYTES, "node:net read bench bytes");
            black_box(bytes);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_node_net_read);
criterion_main!(benches);
