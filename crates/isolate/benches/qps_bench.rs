//! QPS benchmark -- concurrent runtime with real fetch() and JS-side timing.

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

const PURE_JS: &str = r#"
globalThis.__rpc = {
    test() { return "ok"; }
};
"#;

/// fetch with ~100ms server delay + JS-side timing
const FETCH_100MS_JS: &str = r#"
globalThis.__rpc = {
    async test() {
        const start = Date.now();
        const resp = await fetch("https://httpbin.org/delay/0.1", {
            headers: { "accept": "application/json" }
        });
        await resp.text();
        return { elapsed_ms: Date.now() - start, status: resp.status };
    }
};
"#;

fn make_pool() -> Arc<IsolatePool> {
    let config = IsolateConfig::default();
    let data_dir = PathBuf::from("/tmp/appbase-bench-fetch");
    let _ = std::fs::create_dir_all(&data_dir);
    let pf: PluginFactory = Arc::new(|_| -> Vec<Box<dyn Plugin>> { vec![] });
    let mf: MeterFactory = Arc::new(|_| -> Arc<dyn PluginMeter> { Arc::new(NoopMeter) });
    let qf: QuotaFactory = Arc::new(|_| -> Arc<dyn PluginQuota> { Arc::new(NoopQuota) });
    IsolatePool::new(config, data_dir, pf, mf, qf)
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        let pool = make_pool();

        println!("=== QPS Benchmark: Concurrent Runtime ===\n");

        // Pure JS baseline
        {
            let n = 5_000u64;
            pool.dispatch("pure", PURE_JS, RPC_BODY.into()).await.unwrap();
            let start = Instant::now();
            for _ in 0..n {
                pool.dispatch("pure", PURE_JS, RPC_BODY.into()).await.unwrap();
            }
            let e = start.elapsed();
            println!("Pure JS (no I/O):            {:>5} reqs  {:.2}s  {:>8.0} req/s",
                n, e.as_secs_f64(), n as f64 / e.as_secs_f64());
        }

        // Verify fetch works
        println!();
        let r = pool.dispatch("w", FETCH_100MS_JS, RPC_BODY.into()).await;
        match &r {
            Ok(result) => println!("Warmup: {}", result.json),
            Err(e) => {
                println!("fetch failed: {e} (no internet?)");
                pool.shutdown_all();
                return;
            }
        }

        // Sequential: N requests one after another
        for n in [5, 10] {
            let start = Instant::now();
            for _ in 0..n {
                pool.dispatch("seq", FETCH_100MS_JS, RPC_BODY.into()).await.unwrap();
            }
            let e = start.elapsed();
            println!("fetch 100ms (seq, {}):     {:>5} reqs  {:.2}s  {:>8.1} req/s  {:>6.0}ms/req",
                n, n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_millis() as f64 / n as f64);
        }

        println!();

        // Concurrent: N requests all at once
        for n in [10u64, 50, 100, 200, 500] {
            let start = Instant::now();
            let mut handles = Vec::new();
            for _ in 0..n {
                let p = pool.clone();
                handles.push(tokio::spawn(async move {
                    p.dispatch("conc", FETCH_100MS_JS, RPC_BODY.into()).await
                }));
            }
            let mut ok = 0u64;
            let mut err = 0u64;
            for h in handles {
                match h.await.unwrap() {
                    Ok(_) => ok += 1,
                    Err(_) => err += 1,
                }
            }
            let e = start.elapsed();
            let status = if err > 0 { format!("  ({ok} ok, {err} err)") } else { String::new() };
            println!("fetch 100ms (conc, {:>3}):  {:>5} reqs  {:.2}s  {:>8.1} req/s  {:>6.0}ms/req{}",
                n, n, e.as_secs_f64(), ok as f64 / e.as_secs_f64(), e.as_millis() as f64 / n as f64, status);
        }

        println!("\n--- Analysis ---");
        println!("Sequential: each request waits for previous to finish.");
        println!("  N reqs * ~Xms = total time (linear)");
        println!("Concurrent: all requests overlap in the V8 event loop.");
        println!("  N reqs all at once, total time ~ 1 request time (constant)");

        pool.shutdown_all();
    });
}
