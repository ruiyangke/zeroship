mod common;
use common::*;

use appbase_runtime::{init_v8, ModuleEntry};
use appbase_runtime::io::runtime::Runtime;

#[test]
fn basic_rpc() {
    let r = dispatch(m(r#"export function ping() { return "pong"; }"#),
        r#"{"jsonrpc":"2.0","method":"ping","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("pong"));
}

#[test]
fn persistent_context() {
    let results = dispatch_multi(
        m(r#"
            let n = 0;
            export function count() { return ++n; }
        "#),
        &[
            r#"{"jsonrpc":"2.0","method":"count","params":[],"id":1}"#,
            r#"{"jsonrpc":"2.0","method":"count","params":[],"id":2}"#,
            r#"{"jsonrpc":"2.0","method":"count","params":[],"id":3}"#,
        ],
    );
    assert!(results[0].as_ref().unwrap().json.contains("\"result\":1"));
    assert!(results[1].as_ref().unwrap().json.contains("\"result\":2"));
    assert!(results[2].as_ref().unwrap().json.contains("\"result\":3"));
}

#[test]
fn per_request_cpu() {
    init_v8();
    let mut runtime = Runtime::new_direct(m(r#"
        export function fib(n) {
            function f(n) { return n <= 1 ? n : f(n-1) + f(n-2); }
            return f(n);
        }
    "#), no_env(), None, None);
    let r1 = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"fib","params":[20],"id":1}"#).unwrap();
    let r2 = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"fib","params":[35],"id":2}"#).unwrap();
    assert!(r2.cpu_time > r1.cpu_time * 5);
}

#[test]
fn sync_still_works_with_event_loop() {
    let r = dispatch(m(r#"export function add(a, b) { return a + b; }"#),
        r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":1}"#).unwrap();
    assert!(r.json.contains("\"result\":7"));
}

#[test]
fn set_timeout_zero_delay() {
    let r = dispatch(m(r#"
        export function immediate() {
            return new Promise(function(resolve) {
                setTimeout(function() { resolve("immediate"); }, 0);
            });
        }
    "#), r#"{"jsonrpc":"2.0","method":"immediate","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("immediate"));
}

#[test]
fn promise_resolve_sync() {
    let r = dispatch(m(r#"
        export async function test() {
            return "sync-async";
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("sync-async"));
}

#[test]
fn promise_then_chain_sync() {
    let r = dispatch(m(r#"
        export function test() {
            return Promise.resolve(1).then(v => v + 10).then(v => v * 2);
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("\"result\":22"));
}
