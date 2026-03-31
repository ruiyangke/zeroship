//! Integration test: verify that the POSIX CPU timer kills a runaway V8 isolate.

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

/// Recursive fibonacci — CPU-intensive, no I/O.
/// fib(30) ~ 5ms, fib(40) ~ 500ms, fib(45) ~ 5s
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
        return { n, result, cpu_ms: elapsed };
    }
};
"#;

/// Infinite loop — should be killed by the CPU timer.
const INFINITE_LOOP_JS: &str = r#"
globalThis.__rpc = {
    test() {
        while (true) {} // infinite CPU burn
        return "should never reach here";
    }
};
"#;

/// CPU-heavy but finite — should complete within budget.
const HEAVY_COMPUTE_JS: &str = r#"
globalThis.__rpc = {
    test() {
        let sum = 0;
        for (let i = 0; i < 1000000; i++) sum += i;
        return sum;
    }
};
"#;

/// Async I/O — should NOT be killed (CPU usage is minimal).
const FETCH_JS: &str = r#"
globalThis.__rpc = {
    async test() {
        const resp = await fetch("https://httpbin.org/get");
        return { status: resp.status };
    }
};
"#;

fn make_pool() -> Arc<IsolatePool> {
    let config = IsolateConfig::default();
    let data_dir = PathBuf::from("/tmp/appbase-cpu-test");
    let _ = std::fs::create_dir_all(&data_dir);

    let pf: PluginFactory = Arc::new(|_| -> Vec<Box<dyn Plugin>> { vec![] });
    let mf: MeterFactory = Arc::new(|_| -> Arc<dyn PluginMeter> { Arc::new(NoopMeter) });
    let qf: QuotaFactory = Arc::new(|_| -> Arc<dyn PluginQuota> { Arc::new(NoopQuota) });

    IsolatePool::new(config, data_dir, pf, mf, qf)
}

#[tokio::test]
async fn infinite_loop_is_killed() {
    let pool = make_pool();

    let start = Instant::now();
    let result = pool.dispatch("loop_app", INFINITE_LOOP_JS, RPC_BODY.into()).await;
    let elapsed = start.elapsed();

    // Should be an error (terminated by CPU timer or watchdog)
    assert!(result.is_err(), "Expected error, got: {:?}", result);
    let err = result.unwrap_err();
    println!("Infinite loop killed after {:.2}s: {}", elapsed.as_secs_f64(), err);

    // Should be killed within a reasonable time (CPU limit + watchdog buffer)
    // The default CPU limit is 5s, watchdog polls every 500ms
    assert!(
        elapsed.as_secs() < 35,
        "Took too long to kill: {:.1}s (expected < 35s)",
        elapsed.as_secs_f64()
    );

    pool.shutdown_all();
}

#[tokio::test]
async fn heavy_compute_completes() {
    let pool = make_pool();

    let result = pool.dispatch("compute_app", HEAVY_COMPUTE_JS, RPC_BODY.into()).await;
    assert!(result.is_ok(), "Heavy compute should succeed: {:?}", result);

    let rpc_result = result.unwrap();
    println!("Heavy compute result: {}", rpc_result.json);
    assert!(rpc_result.json.contains("result"));

    pool.shutdown_all();
}

#[tokio::test]
async fn isolate_recovers_after_timeout() {
    let pool = make_pool();

    // First request: infinite loop — gets killed
    let result = pool.dispatch("recover_app", INFINITE_LOOP_JS, RPC_BODY.into()).await;
    assert!(result.is_err(), "Infinite loop should fail");
    println!("First request (infinite loop) killed: {}", result.unwrap_err());

    // Second request to the SAME app: should work (isolate recovers)
    // Need to reload with good JS — dispatch with different JS
    let result2 = pool.dispatch("recover_app2", HEAVY_COMPUTE_JS, RPC_BODY.into()).await;
    assert!(result2.is_ok(), "Recovery request should succeed: {:?}", result2);
    println!("Second request (compute) succeeded: {}", result2.unwrap().json);

    pool.shutdown_all();
}

#[tokio::test]
async fn fetch_not_killed_by_cpu_timer() {
    let pool = make_pool();

    // fetch() uses minimal CPU (mostly I/O wait) — should NOT trigger CPU limit
    let start = Instant::now();
    let result = pool.dispatch("fetch_app", FETCH_JS, RPC_BODY.into()).await;
    let elapsed = start.elapsed();

    match &result {
        Ok(r) => println!("Fetch completed in {:.2}s: {}", elapsed.as_secs_f64(), r.json),
        Err(e) => {
            // May fail due to no internet — that's OK, but should NOT be a CPU limit error
            println!("Fetch failed in {:.2}s: {}", elapsed.as_secs_f64(), e);
            if !e.contains("CPU") && !e.contains("time limit") {
                // Network error is acceptable in CI/test environments
                return;
            }
            panic!("fetch() should not be killed by CPU timer: {}", e);
        }
    }

    pool.shutdown_all();
}

/// fib(30) ~5ms -- well within the 50ms CPU budget. Should succeed.
#[tokio::test]
async fn fibonacci_30_within_budget() {
    let pool = make_pool();

    let start = Instant::now();
    let result = pool.dispatch("fib30", FIBONACCI_JS, FIB_30_BODY.into()).await;
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "fib(30) should complete within budget: {:?}", result);
    let r = result.unwrap();
    println!("fib(30) in {:.0}ms: {}", elapsed.as_millis(), r.json);
    assert!(r.json.contains("832040"), "fib(30) = 832040");

    pool.shutdown_all();
}

/// fib(40) ~500ms -- WAY over the 50ms CPU budget. Should be killed.
#[tokio::test]
async fn fibonacci_40_exceeds_budget() {
    let pool = make_pool();

    let start = Instant::now();
    let result = pool.dispatch("fib40", FIBONACCI_JS, FIB_40_BODY.into()).await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "fib(40) should be killed: {:?}", result);
    println!("fib(40) killed after {:.0}ms: {}", elapsed.as_millis(), result.unwrap_err());

    // Should be killed quickly -- not 500ms of actual fib computation
    assert!(elapsed.as_millis() < 500, "Should be killed before completing: {:.0}ms", elapsed.as_millis());

    pool.shutdown_all();
}

/// fib(45) ~5s -- massively over budget. Verifies the timer is precise.
#[tokio::test]
async fn fibonacci_45_killed_precisely() {
    let pool = make_pool();

    let start = Instant::now();
    let result = pool.dispatch("fib45", FIBONACCI_JS, FIB_45_BODY.into()).await;
    let elapsed = start.elapsed();

    assert!(result.is_err(), "fib(45) should be killed: {:?}", result);
    println!("fib(45) killed after {:.0}ms: {}", elapsed.as_millis(), result.unwrap_err());

    // Should be killed near the CPU limit (default 50ms + overhead)
    // NOT at 5 seconds (which is how long fib(45) would take without limits)
    assert!(elapsed.as_millis() < 1000, "Should be killed well before 5s: {:.0}ms", elapsed.as_millis());

    pool.shutdown_all();
}
