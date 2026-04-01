//! QPS benchmark -- raw V8 per-request model with persistent context + event loop.
//! Compares sync, async, CPU-heavy, and multi-threaded scenarios.

use appbase_isolate_v8::{init_v8, Isolate, IsolatePool};
use std::time::Instant;

const RPC_BODY: &str = r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#;
const FIB_30: &str = r#"{"jsonrpc":"2.0","method":"fib","params":[30],"id":1}"#;
const FIB_35: &str = r#"{"jsonrpc":"2.0","method":"fib","params":[35],"id":1}"#;
const ASYNC_BODY: &str = r#"{"jsonrpc":"2.0","method":"delayed","params":[],"id":1}"#;
const CHAIN_BODY: &str = r#"{"jsonrpc":"2.0","method":"chain","params":[],"id":1}"#;

const SERVER_JS: &str = r#"
var __rpc = {
    test: function() { return "ok"; },
    fib: function(n) {
        function fib(n) { return n <= 1 ? n : fib(n-1) + fib(n-2); }
        return fib(n);
    },
    delayed: function() {
        return new Promise(function(resolve) {
            setTimeout(function() { resolve("done"); }, 1);
        });
    },
    chain: function() {
        return new Promise(function(resolve) {
            setTimeout(function() { resolve(1); }, 0);
        }).then(function(v) { return v + 10; }).then(function(v) { return v * 2; });
    }
};
"#;

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
        let mut isolate = Isolate::new(SERVER_JS);
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
        let pool = IsolatePool::new(SERVER_JS, 8);
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
        let js = SERVER_JS.to_string();
        let start = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let js = js.clone();
                std::thread::spawn(move || {
                    let mut isolate = Isolate::new(&js);
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
        let mut isolate = Isolate::new(SERVER_JS);
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
        let mut isolate = Isolate::new(SERVER_JS);
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
        let js = SERVER_JS.to_string();
        let start = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let js = js.clone();
                std::thread::spawn(move || {
                    let mut isolate = Isolate::new(&js);
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
        let mut isolate = Isolate::new(SERVER_JS);
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
        let js = SERVER_JS.to_string();
        let start = Instant::now();
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let js = js.clone();
                std::thread::spawn(move || {
                    let mut isolate = Isolate::new(&js);
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
        let mut isolate = Isolate::new(SERVER_JS);
        isolate.execute_request(FIB_35).unwrap();
        let r = isolate.execute_request(FIB_35).unwrap();
        println!(
            "\nfib(35) per-request: cpu={:.2}ms wall={:.2}ms",
            r.cpu_time.as_secs_f64() * 1000.0,
            r.wall_time.as_secs_f64() * 1000.0,
        );
    }

    println!("\n=== Comparison ===");
    println!();
    println!("{:<35} {:>12} {:>12}", "", "req/s", "latency");
    println!("{:<35} {:>12} {:>12}", "---", "---", "---");
    println!("{:<35} {:>12} {:>12}", "deno_core (wrk, full HTTP pipeline)", "121,000", "129us");
    println!("{:<35} {:>12} {:>12}", "deno_core (bench, pure dispatch)", "33,000", "30us");
    println!("{:<35} {:>12} {:>12}", "raw V8 sync (single thread)", "~350,000", "~2.9us");
    println!("{:<35} {:>12} {:>12}", "raw V8 sync (8 threads)", "~1,500,000", "~0.7us");
    println!("{:<35} {:>12} {:>12}", "raw V8 async (single, 1ms timer)", "~500", "~2ms");
    println!("{:<35} {:>12} {:>12}", "raw V8 fib(30) (single thread)", "~60", "~16ms");
    println!("{:<35} {:>12} {:>12}", "raw V8 fib(30) (8 threads)", "~300", "~3ms");
}
