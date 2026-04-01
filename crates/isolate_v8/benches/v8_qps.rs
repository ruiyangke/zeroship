//! QPS benchmark -- raw V8 per-request model with persistent context.

use appbase_isolate_v8::{init_v8, Isolate, IsolatePool};
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

    println!("=== Raw V8 QPS Benchmark (Persistent Context) ===\n");

    // Test 1: Single isolate, sequential -- persistent context
    {
        let mut isolate = Isolate::new(SERVER_JS);
        isolate.execute_request(RPC_BODY).unwrap(); // warmup + init

        let n = 100_000u64;
        let start = Instant::now();
        for _ in 0..n {
            isolate.execute_request(RPC_BODY).unwrap();
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

        let n = 100_000u64;
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

    // Test 3: Per-thread isolates, multi-threaded
    {
        let server_js = SERVER_JS.to_string();
        for threads in [2u64, 4, 8] {
            let n = 100_000u64;
            let per_thread = n / threads;
            let js = server_js.clone();

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
                "{threads} threads ({threads} isolates):      {:>7} reqs  {:.2}s  {:>9.0} req/s  {:.1}us/req",
                n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
            );
        }
    }

    // Test 4: fib(30) throughput
    {
        let mut isolate = Isolate::new(SERVER_JS);
        isolate.execute_request(FIB_30).unwrap();

        let n = 1_000u64;
        let start = Instant::now();
        for _ in 0..n {
            isolate.execute_request(FIB_30).unwrap();
        }
        let e = start.elapsed();
        println!(
            "\nfib(30) throughput:         {:>7} reqs  {:.2}s  {:>9.0} req/s  {:.1}us/req",
            n, e.as_secs_f64(), n as f64 / e.as_secs_f64(), e.as_micros() as f64 / n as f64
        );
    }

    // Test 5: Per-request CPU measurement
    {
        let mut isolate = Isolate::new(SERVER_JS);
        isolate.execute_request(FIB_35).unwrap(); // warmup

        let r = isolate.execute_request(FIB_35).unwrap();
        println!(
            "\nPer-request CPU:\n  fib(35): cpu={:.2}ms wall={:.2}ms json={}",
            r.cpu_time.as_secs_f64() * 1000.0,
            r.wall_time.as_secs_f64() * 1000.0,
            r.json
        );
    }

    println!("\n--- Comparison ---");
    println!("deno_core (single isolate, wrk):  121,000 req/s  129us/req");
    println!("Raw V8 (single isolate, this):     see above");
    println!("Raw V8 (8 threads, this):          see above");
}
