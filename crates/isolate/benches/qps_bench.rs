//! QPS benchmark — concurrent actor model with real network I/O via fetch().

use appbase_core::config::IsolateConfig;
use appbase_core::plugin::{
    MeterFactory, NoopMeter, NoopQuota, Plugin, PluginFactory, PluginMeter, PluginQuota,
    QuotaFactory,
};
use appbase_isolate::pool::IsolatePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

const RPC_BODY: &str = r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#;

/// Pure JS — no I/O.
const PURE_JS: &str = r#"
globalThis.__rpc = {
    test() { return "ok"; }
};
"#;

/// Fetch httpbin /get — fast, no server delay.
const FETCH_FAST_JS: &str = r#"
globalThis.__rpc = {
    async test() {
        const resp = await fetch("https://httpbin.org/get", {
            headers: { "accept": "application/json" }
        });
        const data = await resp.json();
        return { status: resp.status, origin: data.origin };
    }
};
"#;

/// Fetch httpbin /delay/3 — 3 second server-side delay, with JS-side timing.
const FETCH_3S_JS: &str = r#"
globalThis.__rpc = {
    async test() {
        const start = Date.now();
        const resp = await fetch("https://httpbin.org/delay/3", {
            headers: { "accept": "application/json" }
        });
        const body = await resp.text();
        const elapsed = Date.now() - start;
        return { status: resp.status, elapsed_ms: elapsed, body_len: body.length };
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

        println!("=== QPS Benchmark: Concurrent Actor with real fetch() ===\n");

        // Test 1: Pure JS baseline
        {
            let n = 5_000u64;
            pool.dispatch("pure", PURE_JS, RPC_BODY.into()).await.unwrap();
            let start = Instant::now();
            for _ in 0..n {
                pool.dispatch("pure", PURE_JS, RPC_BODY.into()).await.unwrap();
            }
            let e = start.elapsed();
            println!("Pure JS (no I/O):         {:>5} reqs  {:.2}s  {:>8.0} req/s  {:>6.0}ms/req",
                n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_millis() as f64 / n as f64);
        }

        // Test 2: fetch /get — verify fetch works + measure fast endpoint
        {
            println!("\nTesting fetch() with httpbin.org...");
            let r = pool.dispatch("fast", FETCH_FAST_JS, RPC_BODY.into()).await;
            match &r {
                Ok(result) => println!("  Warmup OK: {}", result.json),
                Err(e) => {
                    println!("  fetch failed: {e}");
                    println!("  (no internet? skipping network tests)");
                    pool.shutdown_all();
                    return;
                }
            }

            let n = 3u64;
            let start = Instant::now();
            for _ in 0..n {
                pool.dispatch("fast", FETCH_FAST_JS, RPC_BODY.into()).await.unwrap();
            }
            let e = start.elapsed();
            println!("fetch /get (seq):         {:>5} reqs  {:.2}s  {:>8.1} req/s  {:>6.0}ms/req",
                n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_millis() as f64 / n as f64);
        }

        // Test 3: fetch /delay/3 — 3 second server delay, SEQUENTIAL
        {
            println!("\nfetch /delay/3 — each request takes ~3 seconds (server delay)");

            // First call — print full response to verify
            let r = pool.dispatch("slow_seq", FETCH_3S_JS, RPC_BODY.into()).await.unwrap();
            println!("  Response: {}", r.json);

            let n = 3u64;
            let start = Instant::now();
            for _ in 0..n {
                let r = pool.dispatch("slow_seq", FETCH_3S_JS, RPC_BODY.into()).await.unwrap();
                println!("  elapsed in JS: {} | Rust wall: {:.1}s", r.json, start.elapsed().as_secs_f64());
            }
            let e = start.elapsed();
            println!("fetch /delay/3 (seq):     {:>5} reqs  {:.1}s  {:>8.2} req/s  {:>6.0}ms/req",
                n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_millis() as f64 / n as f64);
            println!("  ^ Expected: ~{:.0}s total (3s × {} = {}s, sequential)", e.as_secs_f64(), n, 3 * n);
        }

        // Test 4: fetch /delay/3 — 3 second delay, 3 CONCURRENT callers
        {
            println!("\nfetch /delay/3 — 3 concurrent callers (proves the bottleneck)");

            let n = 3u64;
            let start = Instant::now();
            let mut handles = Vec::new();
            for _ in 0..n {
                let p = pool.clone();
                handles.push(tokio::spawn(async move {
                    p.dispatch("slow_conc", FETCH_3S_JS, RPC_BODY.into()).await.unwrap();
                }));
            }
            for h in handles {
                h.await.unwrap();
            }
            let e = start.elapsed();
            println!("fetch /delay/3 (3 conc):  {:>5} reqs  {:.1}s  {:>8.2} req/s  {:>6.0}ms/req",
                n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_millis() as f64 / n as f64);

            println!("\n--- Result ---");
            println!("Sequential:  3 × 3s = 9s total (requests serialized)");
            println!("Concurrent:  should be ~3s (all 3 fetches overlap in event loop)");
        }

        pool.shutdown_all();
    });
}
