//! QPS benchmark -- raw V8 per-request model.

use appbase_isolate_v8::{execute_request, init_v8, IsolatePool};
use std::sync::Arc;
use std::time::Instant;

const RPC_BODY: &str = r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#;
const FIB_30: &str = r#"{"jsonrpc":"2.0","method":"fib","params":[30],"id":1}"#;
const FIB_35: &str = r#"{"jsonrpc":"2.0","method":"fib","params":[35],"id":1}"#;

const SERVER_JS: &str = r#"
var __rpc = {
    test: function() { return "ok"; },
    fib: function(n) {
        function fib(n) { return n <= 1 ? n : fib(n-1) + fib(n-2); }
        return fib(n);
    }
};
"#;

fn main() {
    init_v8();

    println!("=== Raw V8 QPS Benchmark (Per-Request Model) ===\n");

    // Test 1: Single isolate, sequential
    {
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        // warmup
        execute_request(&mut isolate, SERVER_JS, RPC_BODY).unwrap();

        let n = 50_000u64;
        let start = Instant::now();
        for _ in 0..n {
            execute_request(&mut isolate, SERVER_JS, RPC_BODY).unwrap();
        }
        let e = start.elapsed();
        println!(
            "Single isolate (seq):      {:>7} reqs  {:.2}s  {:>9.0} req/s  {:.1}us/req",
            n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Test 2: Pool, sequential
    {
        let pool = IsolatePool::new(SERVER_JS, 8);
        pool.execute(RPC_BODY).unwrap();

        let n = 50_000u64;
        let start = Instant::now();
        for _ in 0..n {
            pool.execute(RPC_BODY).unwrap();
        }
        let e = start.elapsed();
        println!(
            "Pool (seq):                {:>7} reqs  {:.2}s  {:>9.0} req/s  {:.1}us/req",
            n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Test 3: Per-thread isolates, multi-threaded concurrent
    // Each thread creates its own isolate (V8 isolates are !Send)
    {
        let server_js = SERVER_JS.to_string();
        for threads in [2u64, 4, 8] {
            let n = 50_000u64;
            let per_thread = n / threads;
            let js = server_js.clone();

            let start = Instant::now();
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let js = js.clone();
                    std::thread::spawn(move || {
                        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
                        for _ in 0..per_thread {
                            execute_request(&mut isolate, &js, RPC_BODY).unwrap();
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            let e = start.elapsed();
            println!(
                "{threads} threads ({threads} isolates):      {:>7} reqs  {:.2}s  {:>9.0} req/s  {:.1}us/req",
                n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
            );
        }
    }

    // Test 4: fib(30) throughput
    {
        let pool = IsolatePool::new(SERVER_JS, 8);
        pool.execute(FIB_30).unwrap();

        let n = 1_000u64;
        let start = Instant::now();
        for _ in 0..n {
            pool.execute(FIB_30).unwrap();
        }
        let e = start.elapsed();
        println!(
            "\nfib(30) throughput:         {:>7} reqs  {:.2}s  {:>9.0} req/s  {:.1}us/req",
            n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Test 5: Per-request CPU measurement
    {
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        let r = execute_request(&mut isolate, SERVER_JS, FIB_35).unwrap();
        println!(
            "\nPer-request CPU:\n  fib(35): cpu={:.2}ms wall={:.2}ms json={}",
            r.cpu_time.as_secs_f64() * 1000.0,
            r.wall_time.as_secs_f64() * 1000.0,
            r.json
        );
    }

    println!("\n--- Summary ---");
    println!("Raw V8, no deno_core, no event loop, no channel overhead.");
    println!("Fresh context per request = exact per-request CPU attribution.");
}
