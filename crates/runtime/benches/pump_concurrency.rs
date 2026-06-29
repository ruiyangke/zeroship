//! Pump concurrency scaling bench.
//!
//! Measures the wall time for one isolate to drain N already-pending
//! `default.fetch` promises whose continuations are released by zero-delay
//! timers. The setup intentionally creates all N `FetchOutcome::Pending`
//! receivers before starting the pump, so the measured section exercises the
//! pump's per-settlement scans over a maximally full `pending_requests` map.

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use zeroship_runtime::channel::{CancelFlag, ResultReceiver};
use zeroship_runtime::runtime::{DispatchError, Runtime};
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch,
};

const CONCURRENCY_LEVELS: [usize; 4] = [16, 64, 256, 1024];

const DEFER_FETCH_MODULE: &str = r#"
export default {
    async fetch() {
        await new Promise(resolve => setTimeout(resolve, 0));
        return new Response("ok");
    },
};
"#;

type PendingRx = ResultReceiver<Result<SettledFetch, DispatchError>>;

fn build_runtime() -> Runtime {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: DEFER_FETCH_MODULE.into(),
    }];
    Runtime::builder()
        .modules(modules)
        // The benchmark creates short-lived runtimes. Disable idle-GC ticker
        // noise so the only pump task is the one settling request promises.
        .idle_gc_after_ms(0)
        .build()
}

fn prepare_pending_requests(
    runtime: &Runtime,
    env: &EnvSnapshot,
    request_count: usize,
) -> Vec<PendingRx> {
    let mut receivers = Vec::with_capacity(request_count);

    for i in 0..request_count {
        let ctx = RequestCtx::new(CancelFlag::new());
        let url = format!("http://localhost/pump-concurrency/{i}");
        let outcome = runtime.call_fetch_handler("GET", &url, &[], "", &env, ctx);

        match outcome {
            FetchOutcome::Pending { rx, .. } => receivers.push(rx),
            FetchOutcome::Response { status, body, .. } => {
                let body = String::from_utf8_lossy(&body);
                panic!(
                    "expected Pending before pump start, got Response status={status} body={body}"
                );
            }
            FetchOutcome::Stream { .. } => {
                panic!("expected Pending before pump start, got Stream")
            }
            FetchOutcome::WebSocketUpgrade { .. } => {
                panic!("expected Pending before pump start, got WebSocketUpgrade")
            }
        }
    }

    assert_eq!(receivers.len(), request_count);
    receivers
}

async fn drain_pending_requests(receivers: Vec<PendingRx>) {
    for rx in receivers {
        let settled = rx.recv().await.expect("pending fetch delivered DispatchError");
        match settled {
            SettledFetch::Response { status, body, .. } => {
                assert_eq!(status, 200);
                assert_eq!(&body, b"ok");
                black_box(body);
            }
            SettledFetch::Stream { .. } => {
                panic!("expected settled Response, got Stream")
            }
            SettledFetch::WebSocketUpgrade { .. } => {
                panic!("expected settled Response, got WebSocketUpgrade")
            }
        }
    }
}

fn bench_pump_concurrency(c: &mut Criterion) {
    let mut group = c.benchmark_group("pump_concurrency/fetch_pending_setTimeout0");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    let compio_rt = compio::runtime::Runtime::new()
        .expect("failed to create compio runtime for pump bench");
    let runtime = build_runtime();
    let env = EnvSnapshot::empty();
    runtime
        .initialize(&env)
        .expect("failed to initialize pump bench module");
    compio_rt.block_on(async {
        runtime.start_pump();
    });

    for request_count in CONCURRENCY_LEVELS {
        group.throughput(Throughput::Elements(request_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(request_count),
            &request_count,
            |b, &request_count| {
                b.iter(|| {
                    let receivers = prepare_pending_requests(&runtime, &env, request_count);
                    compio_rt.block_on(drain_pending_requests(receivers));
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_pump_concurrency);
criterion_main!(benches);
