//! Integration test: verify that the POSIX CPU timer kills runaway V8 isolates.
//!
//! Each test reports wall time for the operation.
//! CPU budget is 50ms (from IsolateConfig default).

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
const FIB_30_BODY: &str = r#"{"jsonrpc":"2.0","method":"fib","params":[30],"id":1}"#;
const FIB_40_BODY: &str = r#"{"jsonrpc":"2.0","method":"fib","params":[40],"id":1}"#;
const FIB_45_BODY: &str = r#"{"jsonrpc":"2.0","method":"fib","params":[45],"id":1}"#;

/// Recursive fibonacci with JS-side timing.
const FIBONACCI_JS: &str = r#"
globalThis.__rpc = {
    fib(n) {
        function fib(n) {
            if (n <= 1) return n;
            return fib(n - 1) + fib(n - 2);
        }
        const start = Date.now();
        const result = fib(n);
        const elapsed = Date.now() - start;
        return { n, result, js_cpu_ms: elapsed };
    }
};
"#;

const INFINITE_LOOP_JS: &str = r#"
globalThis.__rpc = {
    test() {
        while (true) {}
        return "unreachable";
    }
};
"#;

const HEAVY_COMPUTE_JS: &str = r#"
globalThis.__rpc = {
    test() {
        let sum = 0;
        for (let i = 0; i < 1000000; i++) sum += i;
        return sum;
    }
};
"#;

const FETCH_JS: &str = r#"
globalThis.__rpc = {
    async test() {
        const start = Date.now();
        const resp = await fetch("https://httpbin.org/get");
        const wall_ms = Date.now() - start;
        return { status: resp.status, wall_ms };
    }
};
"#;

fn make_pool() -> Arc<IsolatePool> {
    let config = IsolateConfig::default(); // 50ms CPU limit
    let data_dir = PathBuf::from("/tmp/appbase-cpu-test");
    let _ = std::fs::create_dir_all(&data_dir);

    let pf: PluginFactory = Arc::new(|_| -> Vec<Box<dyn Plugin>> { vec![] });
    let mf: MeterFactory = Arc::new(|_| -> Arc<dyn PluginMeter> { Arc::new(NoopMeter) });
    let qf: QuotaFactory = Arc::new(|_| -> Arc<dyn PluginQuota> { Arc::new(NoopQuota) });

    IsolatePool::new(config, data_dir, pf, mf, qf)
}

fn report(test: &str, wall: std::time::Duration, outcome: &str) {
    println!("[{test:>30}]  wall={:>7.1}ms  {outcome}", wall.as_secs_f64() * 1000.0);
}

#[tokio::test]
async fn infinite_loop_is_killed() {
    let pool = make_pool();
    let start = Instant::now();
    let result = pool.dispatch("loop_app", INFINITE_LOOP_JS, RPC_BODY.into()).await;
    let wall = start.elapsed();

    assert!(result.is_err());
    report("while(true){}", wall, &format!("KILLED: {}", result.unwrap_err()));
    assert!(wall.as_secs() < 35);
    pool.shutdown_all();
}

#[tokio::test]
async fn heavy_compute_completes() {
    let pool = make_pool();
    let start = Instant::now();
    let result = pool.dispatch("compute_app", HEAVY_COMPUTE_JS, RPC_BODY.into()).await;
    let wall = start.elapsed();

    assert!(result.is_ok(), "Should succeed: {:?}", result);
    report("1M iterations", wall, &format!("OK: {}", result.unwrap().json));
    pool.shutdown_all();
}

#[tokio::test]
async fn isolate_recovers_after_timeout() {
    let pool = make_pool();

    let start1 = Instant::now();
    let r1 = pool.dispatch("recover_app", INFINITE_LOOP_JS, RPC_BODY.into()).await;
    let wall1 = start1.elapsed();
    assert!(r1.is_err());
    report("recover: while(true)", wall1, &format!("KILLED: {}", r1.unwrap_err()));

    let start2 = Instant::now();
    let r2 = pool.dispatch("recover_app2", HEAVY_COMPUTE_JS, RPC_BODY.into()).await;
    let wall2 = start2.elapsed();
    assert!(r2.is_ok(), "Recovery should succeed: {:?}", r2);
    report("recover: compute", wall2, &format!("OK: {}", r2.unwrap().json));

    pool.shutdown_all();
}

#[tokio::test]
async fn fetch_not_killed_by_cpu_timer() {
    let pool = make_pool();
    let start = Instant::now();
    let result = pool.dispatch("fetch_app", FETCH_JS, RPC_BODY.into()).await;
    let wall = start.elapsed();

    match &result {
        Ok(r) => report("fetch(httpbin)", wall, &format!("OK: {}", r.json)),
        Err(e) => {
            report("fetch(httpbin)", wall, &format!("ERR: {e}"));
            if !e.contains("CPU") && !e.contains("time limit") {
                return; // network error is OK
            }
            panic!("fetch should not be killed by CPU timer: {e}");
        }
    }
    pool.shutdown_all();
}

#[tokio::test]
async fn fibonacci_30_within_budget() {
    let pool = make_pool();
    let start = Instant::now();
    let result = pool.dispatch("fib30", FIBONACCI_JS, FIB_30_BODY.into()).await;
    let wall = start.elapsed();

    assert!(result.is_ok(), "fib(30) should complete: {:?}", result);
    let json = result.unwrap().json;
    report("fib(30) ~5ms CPU", wall, &format!("OK: {json}"));
    assert!(json.contains("832040"));
    pool.shutdown_all();
}

#[tokio::test]
async fn fibonacci_40_exceeds_budget() {
    let pool = make_pool();
    let start = Instant::now();
    let result = pool.dispatch("fib40", FIBONACCI_JS, FIB_40_BODY.into()).await;
    let wall = start.elapsed();

    assert!(result.is_err(), "fib(40) should be killed: {:?}", result);
    report("fib(40) ~500ms CPU", wall, &format!("KILLED: {}", result.unwrap_err()));
    assert!(wall.as_millis() < 500);
    pool.shutdown_all();
}

#[tokio::test]
async fn fibonacci_45_killed_precisely() {
    let pool = make_pool();
    let start = Instant::now();
    let result = pool.dispatch("fib45", FIBONACCI_JS, FIB_45_BODY.into()).await;
    let wall = start.elapsed();

    assert!(result.is_err(), "fib(45) should be killed: {:?}", result);
    report("fib(45) ~5000ms CPU", wall, &format!("KILLED: {}", result.unwrap_err()));
    assert!(wall.as_millis() < 1000, "Should be killed well before 5s: {:.0}ms", wall.as_millis());
    pool.shutdown_all();
}
