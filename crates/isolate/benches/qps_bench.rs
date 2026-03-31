//! QPS benchmark — measures requests per second through the isolate pool.
//!
//! Tests two scenarios:
//! 1. Pure JS computation (no I/O) — measures V8 + actor overhead
//! 2. Simulated async I/O (setTimeout) — measures sequential vs concurrent impact

use appbase_core::config::IsolateConfig;
use appbase_core::plugin::{MeterFactory, NoopMeter, NoopQuota, Plugin, PluginFactory, QuotaFactory};
use appbase_isolate::pool::IsolatePool;
use std::sync::Arc;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Simple JS that does pure computation (no I/O, no await).
const PURE_JS: &str = r#"
globalThis.rpc = {
    dispatch(req) {
        const parsed = JSON.parse(req);
        return JSON.stringify({ result: "ok", method: parsed.method });
    }
};
"#;

/// JS that does moderate computation (~0.1ms of CPU work).
const COMPUTE_LIGHT_JS: &str = r#"
globalThis.rpc = {
    dispatch(req) {
        let sum = 0;
        for (let i = 0; i < 1000; i++) sum += i;
        return JSON.stringify({ result: sum });
    }
};
"#;

/// JS that does heavy computation (~1ms of CPU work).
const COMPUTE_HEAVY_JS: &str = r#"
globalThis.rpc = {
    dispatch(req) {
        let sum = 0;
        for (let i = 0; i < 100000; i++) sum += i;
        return JSON.stringify({ result: sum });
    }
};
"#;

const RPC_BODY: &str = r#"{"jsonrpc":"2.0","method":"test","params":{},"id":1}"#;

fn make_pool() -> Arc<IsolatePool> {
    let config = IsolateConfig::default();
    let data_dir = PathBuf::from("/tmp/appbase-bench");
    let _ = std::fs::create_dir_all(&data_dir);

    let plugin_factory: PluginFactory = Arc::new(|_app_id| -> Vec<Box<dyn Plugin>> { vec![] });
    let meter_factory: MeterFactory = Arc::new(|_app_id| -> Arc<dyn appbase_core::plugin::PluginMeter> {
        Arc::new(NoopMeter)
    });
    let quota_factory: QuotaFactory = Arc::new(|_app_id| -> Arc<dyn appbase_core::plugin::PluginQuota> {
        Arc::new(NoopQuota)
    });

    IsolatePool::new(config, data_dir, plugin_factory, meter_factory, quota_factory)
}

async fn bench_sequential(pool: &Arc<IsolatePool>, app_id: &str, server_js: &str, n: u64) -> (Duration, f64) {
    // Warm up — first request creates the isolate
    pool.dispatch(app_id, server_js, RPC_BODY.to_string()).await.unwrap();

    let start = Instant::now();
    for _ in 0..n {
        pool.dispatch(app_id, server_js, RPC_BODY.to_string()).await.unwrap();
    }
    let elapsed = start.elapsed();
    let qps = n as f64 / elapsed.as_secs_f64();
    (elapsed, qps)
}

async fn bench_concurrent(pool: &Arc<IsolatePool>, app_id: &str, server_js: &str, n: u64, concurrency: u64) -> (Duration, f64) {
    // Warm up
    pool.dispatch(app_id, server_js, RPC_BODY.to_string()).await.unwrap();

    let pool = pool.clone();
    let start = Instant::now();

    // Note: with sequential actor model, these will queue up
    // With concurrent model, they would interleave at await points
    let mut handles = Vec::new();
    for _ in 0..n {
        let pool = pool.clone();
        let app_id = app_id.to_string();
        let server_js = server_js.to_string();
        handles.push(tokio::spawn(async move {
            pool.dispatch(&app_id, &server_js, RPC_BODY.to_string()).await.unwrap();
        }));
        // Limit in-flight concurrency
        if handles.len() >= concurrency as usize {
            handles.remove(0).await.unwrap();
        }
    }
    for h in handles {
        h.await.unwrap();
    }

    let elapsed = start.elapsed();
    let qps = n as f64 / elapsed.as_secs_f64();
    (elapsed, qps)
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let pool = make_pool();

        println!("=== Isolate QPS Benchmark (Sequential Actor Model) ===\n");

        // Test 1: Minimal RPC (JSON parse + serialize)
        let n = 10_000;
        let (elapsed, qps) = bench_sequential(&pool, "minimal", PURE_JS, n).await;
        println!("Minimal RPC:        {n} reqs in {:.2}s = {qps:.0} req/s  ({:.1}us/req)",
            elapsed.as_secs_f64(), elapsed.as_micros() as f64 / n as f64);

        // Test 2: Light compute (~0.1ms JS work)
        let n = 10_000;
        let (elapsed, qps) = bench_sequential(&pool, "light", COMPUTE_LIGHT_JS, n).await;
        println!("Light compute:      {n} reqs in {:.2}s = {qps:.0} req/s  ({:.1}us/req)",
            elapsed.as_secs_f64(), elapsed.as_micros() as f64 / n as f64);

        // Test 3: Heavy compute (~1ms JS work)
        let n = 5_000;
        let (elapsed, qps) = bench_sequential(&pool, "heavy", COMPUTE_HEAVY_JS, n).await;
        println!("Heavy compute:      {n} reqs in {:.2}s = {qps:.0} req/s  ({:.1}us/req)",
            elapsed.as_secs_f64(), elapsed.as_micros() as f64 / n as f64);

        // Test 4: Concurrent callers on minimal RPC (shows actor queue overhead)
        let n = 10_000;
        let (elapsed, qps) = bench_concurrent(&pool, "conc_min", PURE_JS, n, 100).await;
        println!("Minimal (100 conc): {n} reqs in {:.2}s = {qps:.0} req/s  ({:.1}us/req)",
            elapsed.as_secs_f64(), elapsed.as_micros() as f64 / n as f64);

        // Test 5: Concurrent callers on heavy compute
        let n = 5_000;
        let (elapsed, qps) = bench_concurrent(&pool, "conc_heavy", COMPUTE_HEAVY_JS, n, 100).await;
        println!("Heavy (100 conc):   {n} reqs in {:.2}s = {qps:.0} req/s  ({:.1}us/req)",
            elapsed.as_secs_f64(), elapsed.as_micros() as f64 / n as f64);

        // Test 6: Multiple apps (tests pool overhead)
        let n_per_app = 1000;
        let n_apps = 10;
        let start = Instant::now();
        let mut handles = Vec::new();
        for i in 0..n_apps {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..n_per_app {
                    pool.dispatch(&format!("app{i}"), PURE_JS, RPC_BODY.to_string()).await.unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let elapsed = start.elapsed();
        let total = n_per_app * n_apps;
        let qps = total as f64 / elapsed.as_secs_f64();
        println!("Multi-app ({n_apps}×{n_per_app}): {total} reqs in {:.2}s = {qps:.0} req/s",
            elapsed.as_secs_f64());

        println!("\n--- Summary ---");
        println!("Sequential model: all requests to one app serialize through 1 V8 thread");
        println!("Multi-app: each app gets its own thread, so N apps = N× throughput");

        pool.shutdown_all();
    });
}
