use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use uuid::Uuid;
use zeroship_metering::Meter;

const CPU_US: u64 = 500;
const WALL_US: u64 = 1_200;
const EGRESS_BYTES: u64 = 2_048;
const INGRESS_BYTES: u64 = 256;
const THREADS: usize = 8;

fn seed_meter(apps: &[String]) -> Arc<Meter> {
    let meter = Arc::new(Meter::new());
    for app in apps {
        meter.increment(app, "requests", 0);
    }
    meter
}

fn record_one(meter: &Meter, app: &str) {
    meter.record_request(
        black_box(app),
        black_box(CPU_US),
        black_box(WALL_US),
        black_box(EGRESS_BYTES),
        black_box(INGRESS_BYTES),
    );
}

fn bench_single_thread(c: &mut Criterion) {
    let app = Uuid::new_v4().to_string();
    let meter = seed_meter(std::slice::from_ref(&app));

    c.bench_function("record_request/single_thread", |b| {
        b.iter(|| record_one(&meter, &app));
    });
}

fn bench_threads_same_app(c: &mut Criterion) {
    let app = Uuid::new_v4().to_string();
    let meter = seed_meter(std::slice::from_ref(&app));
    let mut group = c.benchmark_group("record_request/threads_same_app");
    group.throughput(Throughput::Elements(1));
    group.bench_with_input(BenchmarkId::from_parameter(THREADS), &THREADS, |b, &threads| {
        b.iter_custom(|iters| parallel_elapsed(iters, threads, &meter, |thread_id| {
            black_box(thread_id);
            &app
        }));
    });
    group.finish();
}

fn bench_threads_distinct_apps(c: &mut Criterion) {
    let apps: Vec<String> = (0..THREADS).map(|_| Uuid::new_v4().to_string()).collect();
    let meter = seed_meter(&apps);
    let mut group = c.benchmark_group("record_request/threads_distinct_apps");
    group.throughput(Throughput::Elements(1));
    group.bench_with_input(BenchmarkId::from_parameter(THREADS), &THREADS, |b, &threads| {
        b.iter_custom(|iters| parallel_elapsed(iters, threads, &meter, |thread_id| {
            apps[thread_id].as_str()
        }));
    });
    group.finish();
}

fn parallel_elapsed<'a>(
    iters: u64,
    threads: usize,
    meter: &'a Arc<Meter>,
    app_for_thread: impl Fn(usize) -> &'a str + Copy + Send + Sync,
) -> Duration {
    let base = iters / threads as u64;
    let remainder = iters % threads as u64;
    let start = Instant::now();
    std::thread::scope(|scope| {
        for thread_id in 0..threads {
            let calls = base + u64::from(thread_id < remainder as usize);
            scope.spawn(move || {
                let app = app_for_thread(thread_id);
                for _ in 0..calls {
                    record_one(meter, app);
                }
            });
        }
    });
    start.elapsed()
}

criterion_group!(
    benches,
    bench_single_thread,
    bench_threads_same_app,
    bench_threads_distinct_apps
);
criterion_main!(benches);
