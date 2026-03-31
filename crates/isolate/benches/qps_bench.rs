//! QPS benchmark — sequential actor model with real network I/O via fetch().
//!
//! Now that deno_fetch is wired, we can test with real HTTP calls
//! to demonstrate the sequential bottleneck.

use appbase_core::config::IsolateConfig;
use appbase_core::plugin::{
    MeterFactory, NoopMeter, NoopQuota, Plugin, PluginFactory, PluginMeter, PluginQuota,
    QuotaFactory,
};
use appbase_isolate::pool::IsolatePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const RPC_BODY: &str = r#"{"jsonrpc":"2.0","method":"test","params":{},"id":1}"#;

/// Pure JS — no I/O.
const PURE_JS: &str = r#"
globalThis.rpc = {
    dispatch(req) {
        return JSON.stringify({ result: "ok" });
    }
};
"#;

/// Real HTTP fetch — httpbin delays response by N seconds.
/// This is REAL network I/O that blocks the actor thread.
const FETCH_JS: &str = r#"
globalThis.rpc = {
    async dispatch(req) {
        const parsed = JSON.parse(req);
        const delay = parsed.params?.delay || 0;
        const resp = await fetch("https://httpbin.org/delay/" + delay);
        return JSON.stringify({ status: resp.status });
    }
};
"#;

/// Fetch a fast endpoint (no artificial delay).
const FETCH_FAST_JS: &str = r#"
globalThis.rpc = {
    async dispatch(req) {
        const resp = await fetch("https://httpbin.org/get");
        return JSON.stringify({ status: resp.status });
    }
};
"#;

fn make_pool() -> Arc<IsolatePool> {
    let config = IsolateConfig::default();
    let data_dir = PathBuf::from("/tmp/appbase-bench-fetch");
    let _ = std::fs::create_dir_all(&data_dir);

    let plugin_factory: PluginFactory = Arc::new(|_| -> Vec<Box<dyn Plugin>> { vec![] });
    let meter_factory: MeterFactory =
        Arc::new(|_| -> Arc<dyn PluginMeter> { Arc::new(NoopMeter) });
    let quota_factory: QuotaFactory =
        Arc::new(|_| -> Arc<dyn PluginQuota> { Arc::new(NoopQuota) });

    IsolatePool::new(config, data_dir, plugin_factory, meter_factory, quota_factory)
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let pool = make_pool();

        println!("=== QPS Benchmark: Sequential Actor with fetch() ===\n");

        // Test 1: Pure JS baseline (no I/O)
        {
            let n = 5_000u64;
            // warmup
            pool.dispatch("pure", PURE_JS, RPC_BODY.into()).await.unwrap();
            let start = Instant::now();
            for _ in 0..n {
                pool.dispatch("pure", PURE_JS, RPC_BODY.into()).await.unwrap();
            }
            let e = start.elapsed();
            let qps = n as f64 / e.as_secs_f64();
            println!("Pure JS (no I/O):      {n} reqs in {:.2}s = {qps:.0} req/s", e.as_secs_f64());
        }

        // Test 2: Real fetch — fast endpoint (network RTT only, ~100-300ms)
        {
            let n = 5u64;
            println!("\nFetching https://httpbin.org/get (real network I/O)...");
            // warmup + verify it works
            let r = pool.dispatch("fetch_fast", FETCH_FAST_JS, RPC_BODY.into()).await;
            match &r {
                Ok(result) => println!("  warmup response: {}", result.json),
                Err(e) => {
                    println!("  fetch failed: {e}");
                    println!("  (skipping network tests — no internet?)");
                    pool.shutdown_all();
                    return;
                }
            }

            let start = Instant::now();
            for _ in 0..n {
                pool.dispatch("fetch_fast", FETCH_FAST_JS, RPC_BODY.into()).await.unwrap();
            }
            let e = start.elapsed();
            let qps = n as f64 / e.as_secs_f64();
            let avg_ms = e.as_millis() as f64 / n as f64;
            println!("fetch (fast, seq):     {n} reqs in {:.2}s = {qps:.1} req/s  ({avg_ms:.0}ms/req)", e.as_secs_f64());
        }

        // Test 3: Concurrent fetch — same endpoint, 5 concurrent callers
        {
            let n = 10u64;
            let pool_c = pool.clone();
            let start = Instant::now();
            let mut handles = Vec::new();
            for _ in 0..n {
                let p = pool_c.clone();
                handles.push(tokio::spawn(async move {
                    p.dispatch("fetch_conc", FETCH_FAST_JS, RPC_BODY.into()).await.unwrap();
                }));
            }
            for h in handles {
                h.await.unwrap();
            }
            let e = start.elapsed();
            let qps = n as f64 / e.as_secs_f64();
            let avg_ms = e.as_millis() as f64 / n as f64;
            println!("fetch (fast, 10 conc): {n} reqs in {:.2}s = {qps:.1} req/s  ({avg_ms:.0}ms/req)", e.as_secs_f64());
        }

        println!("\n--- Analysis ---");
        println!("Sequential: each fetch blocks the V8 thread for ~200ms (network RTT).");
        println!("Concurrent callers queue behind the blocked thread — no parallelism.");
        println!("With concurrent runtime: 10 fetches would overlap, completing in ~200ms total.");

        pool.shutdown_all();
    });
}
