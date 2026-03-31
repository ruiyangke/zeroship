//! QPS benchmark — measures requests per second through the isolate pool.
//!
//! Tests sequential model with: pure JS, and DB plugin (real SQLite I/O).

use appbase_core::config::IsolateConfig;
use appbase_core::plugin::{
    MeterFactory, NoopMeter, NoopQuota, Plugin, PluginFactory, PluginMeter, PluginQuota,
    QuotaFactory,
};
use appbase_isolate::pool::IsolatePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const RPC_BODY: &str = r#"{"jsonrpc":"2.0","method":"test","params":{},"id":1}"#;

/// Pure JS — no I/O.
const PURE_JS: &str = r#"
globalThis.rpc = {
    dispatch(req) {
        const parsed = JSON.parse(req);
        return JSON.stringify({ result: "ok", method: parsed.method });
    }
};
"#;

/// JS that does a DB write + read (real SQLite I/O via plugin).
const DB_JS: &str = r#"
globalThis.rpc = {
    async dispatch(req) {
        await db.ensureTable("bench");
        await db.insert("bench", { value: Math.random() });
        const rows = await db.find("bench");
        return JSON.stringify({ count: rows.length });
    }
};
"#;

/// JS with heavy compute (~1ms).
const COMPUTE_JS: &str = r#"
globalThis.rpc = {
    dispatch(req) {
        let sum = 0;
        for (let i = 0; i < 100000; i++) sum += i;
        return JSON.stringify({ result: sum });
    }
};
"#;

fn make_pool_no_plugins() -> Arc<IsolatePool> {
    let config = IsolateConfig::default();
    let data_dir = PathBuf::from("/tmp/appbase-bench-noplugin");
    let _ = std::fs::create_dir_all(&data_dir);

    let plugin_factory: PluginFactory = Arc::new(|_| vec![]);
    let meter_factory: MeterFactory =
        Arc::new(|_| -> Arc<dyn PluginMeter> { Arc::new(NoopMeter) });
    let quota_factory: QuotaFactory =
        Arc::new(|_| -> Arc<dyn PluginQuota> { Arc::new(NoopQuota) });

    IsolatePool::new(config, data_dir, plugin_factory, meter_factory, quota_factory)
}

fn make_pool_with_db() -> Arc<IsolatePool> {
    let config = IsolateConfig::default();
    let data_dir = PathBuf::from("/tmp/appbase-bench-db");
    let _ = std::fs::create_dir_all(&data_dir);
    // Clean up old DB
    let _ = std::fs::remove_file("/tmp/appbase-bench-db/bench.db");

    let plugin_factory: PluginFactory = Arc::new(|_app_id| -> Vec<Box<dyn Plugin>> {
        vec![
            Box::new(appbase_plugins::db::DbPlugin::with_path("/tmp/appbase-bench-db/bench.db")),
        ]
    });
    let meter_factory: MeterFactory =
        Arc::new(|_| -> Arc<dyn PluginMeter> { Arc::new(NoopMeter) });
    let quota_factory: QuotaFactory =
        Arc::new(|_| -> Arc<dyn PluginQuota> { Arc::new(NoopQuota) });

    IsolatePool::new(config, data_dir, plugin_factory, meter_factory, quota_factory)
}

async fn bench_sequential(
    pool: &Arc<IsolatePool>,
    app_id: &str,
    server_js: &str,
    n: u64,
) -> (Duration, f64) {
    pool.dispatch(app_id, server_js, RPC_BODY.to_string())
        .await
        .unwrap();

    let start = Instant::now();
    for _ in 0..n {
        pool.dispatch(app_id, server_js, RPC_BODY.to_string())
            .await
            .unwrap();
    }
    let elapsed = start.elapsed();
    let qps = n as f64 / elapsed.as_secs_f64();
    (elapsed, qps)
}

async fn bench_concurrent(
    pool: &Arc<IsolatePool>,
    app_id: &str,
    server_js: &str,
    n: u64,
    concurrency: u64,
) -> (Duration, f64) {
    pool.dispatch(app_id, server_js, RPC_BODY.to_string())
        .await
        .unwrap();

    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..n {
        let pool = pool.clone();
        let app_id = app_id.to_string();
        let server_js = server_js.to_string();
        handles.push(tokio::spawn(async move {
            pool.dispatch(&app_id, &server_js, RPC_BODY.to_string())
                .await
                .unwrap();
        }));
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
        println!("=== QPS Benchmark: Sequential Actor Model ===\n");

        // Pure JS tests (no plugins)
        {
            let pool = make_pool_no_plugins();

            let n = 10_000;
            let (e, qps) = bench_sequential(&pool, "pure", PURE_JS, n).await;
            println!("Pure JS (sequential):     {n:>6} reqs in {:.2}s = {:>8.0} req/s  ({:.0}us/req)", e.as_secs_f64(), qps, e.as_micros() as f64 / n as f64);

            let n = 5_000;
            let (e, qps) = bench_sequential(&pool, "compute", COMPUTE_JS, n).await;
            println!("Compute 1ms (sequential): {n:>6} reqs in {:.2}s = {:>8.0} req/s  ({:.0}us/req)", e.as_secs_f64(), qps, e.as_micros() as f64 / n as f64);

            let n = 10_000;
            let (e, qps) = bench_concurrent(&pool, "pure_c", PURE_JS, n, 100).await;
            println!("Pure JS (100 concurrent): {n:>6} reqs in {:.2}s = {:>8.0} req/s  ({:.0}us/req)", e.as_secs_f64(), qps, e.as_micros() as f64 / n as f64);

            pool.shutdown_all();
        }

        println!();

        // DB tests (real SQLite I/O)
        {
            let pool = make_pool_with_db();

            let n = 500;
            let (e, qps) = bench_sequential(&pool, "db_seq", DB_JS, n).await;
            println!("DB write+read (sequential): {n:>4} reqs in {:.2}s = {:>8.0} req/s  ({:.0}us/req)", e.as_secs_f64(), qps, e.as_micros() as f64 / n as f64);

            let n = 500;
            let (e, qps) = bench_concurrent(&pool, "db_conc", DB_JS, n, 50).await;
            println!("DB write+read (50 conc):    {n:>4} reqs in {:.2}s = {:>8.0} req/s  ({:.0}us/req)", e.as_secs_f64(), qps, e.as_micros() as f64 / n as f64);

            pool.shutdown_all();
        }

        println!("\n--- The Problem ---");
        println!("With real I/O (DB), sequential model serializes ALL requests.");
        println!("50 concurrent callers doesn't help — they queue behind 1 V8 thread.");
        println!("Concurrent runtime would let I/O wait overlap between requests.");
    });
}
