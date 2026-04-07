//! QPS benchmark -- raw V8 per-request model with persistent context + event loop.
//! Compares sync, async, CPU-heavy, and multi-threaded scenarios.

use appbase_runtime::{init_v8, Isolate, IsolatePool, ModuleEntry};
use std::time::Instant;

const RPC_BODY: &str = r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#;
const FIB_30: &str = r#"{"jsonrpc":"2.0","method":"fib","params":[30],"id":1}"#;
const FIB_35: &str = r#"{"jsonrpc":"2.0","method":"fib","params":[35],"id":1}"#;
const ASYNC_BODY: &str = r#"{"jsonrpc":"2.0","method":"delayed","params":[],"id":1}"#;
const CHAIN_BODY: &str = r#"{"jsonrpc":"2.0","method":"chain","params":[],"id":1}"#;

const SERVER_JS: &str = r#"
export function test() { return "ok"; }
export function fib(n) {
    function fib(n) { return n <= 1 ? n : fib(n-1) + fib(n-2); }
    return fib(n);
}
export function delayed() {
    return new Promise(function(resolve) {
        setTimeout(function() { resolve("done"); }, 1);
    });
}
export function chain() {
    return new Promise(function(resolve) {
        setTimeout(function() { resolve(1); }, 0);
    }).then(function(v) { return v + 10; }).then(function(v) { return v * 2; });
}
"#;

fn server_modules() -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: SERVER_JS.into(),
    }]
}

fn bench(name: &str, n: u64, f: impl Fn()) {
    let start = Instant::now();
    for _ in 0..n {
        f();
    }
    let e = start.elapsed();
    println!(
        "{name:<35} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}us/req",
        n,
        e.as_secs_f64(),
        n as f64 / e.as_secs_f64(),
        e.as_micros() as f64 / n as f64
    );
}

fn main() {
    init_v8();

    println!("=== Raw V8 Benchmark (Persistent Context + Event Loop) ===\n");
    println!("--- Sync RPC (no event loop overhead) ---\n");

    // Sync: single isolate
    {
        let mut isolate = Isolate::new(server_modules(), std::collections::HashMap::new());
        isolate.execute_request(RPC_BODY).unwrap();
        let n = 100_000u64;
        let start = Instant::now();
        for _ in 0..n {
            isolate.execute_request(RPC_BODY).unwrap();
        }
        let e = start.elapsed();
        println!(
            "{:<35} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}us/req",
            "sync RPC (single isolate)", n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Sync: pool
    {
        let pool = IsolatePool::new(server_modules(), std::collections::HashMap::new(), 8);
        pool.execute(RPC_BODY).unwrap();
        let n = 100_000u64;
        let start = Instant::now();
        for _ in 0..n {
            pool.execute(RPC_BODY).unwrap();
        }
        let e = start.elapsed();
        println!(
            "{:<35} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}us/req",
            "sync RPC (pool)", n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Sync: multi-threaded
    for threads in [2u64, 4, 8] {
        let n = 100_000u64;
        let per_thread = n / threads;
        let start = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                std::thread::spawn(move || {
                    let mut isolate = Isolate::new(server_modules(), std::collections::HashMap::new());
                    for _ in 0..per_thread {
                        isolate.execute_request(RPC_BODY).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let e = start.elapsed();
        println!(
            "{:<35} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}us/req",
            format!("sync RPC ({threads} threads)"), n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    println!("\n--- Async RPC (event loop active) ---\n");

    // Async: setTimeout 1ms
    {
        let mut isolate = Isolate::new(server_modules(), std::collections::HashMap::new());
        isolate.execute_request(ASYNC_BODY).unwrap();
        let n = 1_000u64;
        let start = Instant::now();
        for _ in 0..n {
            isolate.execute_request(ASYNC_BODY).unwrap();
        }
        let e = start.elapsed();
        println!(
            "{:<35} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}us/req",
            "async setTimeout(1ms)", n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Async: promise chain (setTimeout 0 + .then.then)
    {
        let mut isolate = Isolate::new(server_modules(), std::collections::HashMap::new());
        isolate.execute_request(CHAIN_BODY).unwrap();
        let n = 10_000u64;
        let start = Instant::now();
        for _ in 0..n {
            isolate.execute_request(CHAIN_BODY).unwrap();
        }
        let e = start.elapsed();
        println!(
            "{:<35} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}us/req",
            "async Promise chain (0ms+then+then)", n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Async: multi-threaded setTimeout 1ms
    for threads in [2u64, 4, 8] {
        let n = 1_000u64;
        let per_thread = n / threads;
        let start = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                std::thread::spawn(move || {
                    let mut isolate = Isolate::new(server_modules(), std::collections::HashMap::new());
                    for _ in 0..per_thread {
                        isolate.execute_request(ASYNC_BODY).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let e = start.elapsed();
        println!(
            "{:<35} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}us/req",
            format!("async setTimeout ({threads} threads)"), n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    println!("\n--- CPU-Heavy (fibonacci) ---\n");

    // fib(30): single + multi
    {
        let mut isolate = Isolate::new(server_modules(), std::collections::HashMap::new());
        isolate.execute_request(FIB_30).unwrap();
        let n = 100u64;
        let start = Instant::now();
        for _ in 0..n {
            isolate.execute_request(FIB_30).unwrap();
        }
        let e = start.elapsed();
        println!(
            "{:<35} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}ms/req",
            "fib(30) single thread", n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_millis() as f64 / n as f64
        );
    }

    for threads in [2u64, 4, 8] {
        let n = 100u64;
        let per_thread = n / threads;
        let start = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                std::thread::spawn(move || {
                    let mut isolate = Isolate::new(server_modules(), std::collections::HashMap::new());
                    for _ in 0..per_thread {
                        isolate.execute_request(FIB_30).unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let e = start.elapsed();
        println!(
            "{:<35} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}ms/req",
            format!("fib(30) {threads} threads"), n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Per-request CPU
    {
        let mut isolate = Isolate::new(server_modules(), std::collections::HashMap::new());
        isolate.execute_request(FIB_35).unwrap();
        let r = isolate.execute_request(FIB_35).unwrap();
        println!(
            "\nfib(35) per-request: cpu={:.2}ms wall={:.2}ms",
            r.cpu_time.as_secs_f64() * 1000.0,
            r.wall_time.as_secs_f64() * 1000.0,
        );
    }

    // =========================================================================
    println!("\n=== Runtime Model (serial JS, concurrent I/O) ===\n");

    use appbase_runtime::runtime::Runtime;
    use appbase_runtime::state::IncomingRequest;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio_util::sync::CancellationToken;

    let next_id = AtomicU64::new(1);

    // Helper: send N requests to a Runtime worker and collect results
    fn bench_runtime(
        req_tx: &tokio::sync::mpsc::Sender<IncomingRequest>,
        next_id: &AtomicU64,
        body: &str,
        n: u64,
    ) -> Vec<Result<appbase_runtime::RequestResult, String>> {
        let mut reply_rxs = Vec::new();
        // Send all requests at once
        for _ in 0..n {
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            req_tx.blocking_send(IncomingRequest {
                id,
                body: body.to_string(),
                reply: reply_tx,
                cancel: CancellationToken::new(),
            }).unwrap();
            reply_rxs.push(reply_rx);
        }
        // Collect results (blocking)
        reply_rxs.into_iter().map(|rx| rx.blocking_recv().unwrap()).collect()
    }

    // Start a Runtime worker on a dedicated thread
    let (req_tx, req_rx) = tokio::sync::mpsc::channel::<IncomingRequest>(1024);
    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut runtime = Runtime::new(
                server_modules(),
                req_rx,
                shutdown_clone,
                None,
                std::collections::HashMap::new(),
            );
            runtime.run().await;
        });
    });

    // Warmup
    {
        let id = next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        req_tx.blocking_send(IncomingRequest {
            id,
            body: RPC_BODY.to_string(),
            reply: tx,
            cancel: CancellationToken::new(),
        }).unwrap();
        rx.blocking_recv().unwrap().unwrap();
    }

    // Runtime: sync RPC
    {
        let n = 10_000u64;
        let start = Instant::now();
        let results = bench_runtime(&req_tx, &next_id, RPC_BODY, n);
        let e = start.elapsed();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        println!(
            "{:<40} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}us/req",
            "runtime: sync RPC (ping)", n, e.as_secs_f64(), ok as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Runtime: async setTimeout(0) (Promise chain)
    {
        let n = 10_000u64;
        let start = Instant::now();
        let results = bench_runtime(&req_tx, &next_id, CHAIN_BODY, n);
        let e = start.elapsed();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        println!(
            "{:<40} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}us/req",
            "runtime: Promise chain (0ms)", n, e.as_secs_f64(), ok as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Runtime: 100 requests with 1ms timer (overlap test)
    {
        let timer_body = r#"{"jsonrpc":"2.0","method":"delayed","params":[],"id":1}"#;
        let n = 100u64;
        let start = Instant::now();
        let results = bench_runtime(&req_tx, &next_id, timer_body, n);
        let e = start.elapsed();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        println!(
            "{:<40} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}ms/req  (should be ~1ms not 100ms)",
            "runtime: 100x setTimeout(1ms)", n, e.as_secs_f64(), ok as f64 / e.as_secs_f64(), e.as_millis() as f64 / n as f64
        );
    }

    // Runtime: fib(30) (CPU-heavy, serial JS — no speedup expected)
    {
        let n = 10u64;
        let start = Instant::now();
        let results = bench_runtime(&req_tx, &next_id, FIB_30, n);
        let e = start.elapsed();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        println!(
            "{:<40} {:>7} reqs  {:.2}s  {:>9.0} req/s  {:>8.1}ms/req  (CPU-bound, no overlap)",
            "runtime: fib(30)", n, e.as_secs_f64(), ok as f64 / e.as_secs_f64(), e.as_millis() as f64 / n as f64
        );
    }

    // Shutdown Runtime worker
    shutdown.cancel();

    println!("\n=== Summary ===");
    println!("Per-request model: 1 request at a time, blocking. Best for CPU-heavy multi-thread.");
    println!("Runtime model:     N requests overlapping I/O, serial JS. Best for I/O-heavy apps.");
    println!("Both models: per-request CPU tracking, clean kill, zero collateral.");
}
