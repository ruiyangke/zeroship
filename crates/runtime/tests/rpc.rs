mod common;
use common::*;

use zeroship_runtime::init_v8;
use zeroship_runtime::runtime::Runtime;

#[test]
fn basic_rpc() {
    let r = dispatch(
        m(r#"export function ping() { return "pong"; }"#),
        "ping",
        "[]",
    ).unwrap();
    // New wire: body is the raw JSON value, no envelope.
    assert_eq!(r.json, "\"pong\"");
}

#[test]
fn persistent_context() {
    let results = dispatch_multi(
        m(r#"
            let n = 0;
            export function count() { return ++n; }
        "#),
        &[("count", "[]"), ("count", "[]"), ("count", "[]")],
    );
    assert_eq!(results[0].as_ref().unwrap().json, "1");
    assert_eq!(results[1].as_ref().unwrap().json, "2");
    assert_eq!(results[2].as_ref().unwrap().json, "3");
}

#[test]
fn per_request_cpu() {
    init_v8();
    let modules = m(r#"
        export function fib(n) {
            function f(n) { return n <= 1 ? n : f(n-1) + f(n-2); }
            return f(n);
        }
    "#);
    let runtime = Runtime::builder().modules(modules).build();
    let r1 = runtime.dispatch_rpc("fib", "[20]").unwrap();
    let r2 = runtime.dispatch_rpc("fib", "[35]").unwrap();
    assert!(r2.cpu_time > r1.cpu_time * 5);
}

#[test]
fn sync_still_works_with_event_loop() {
    let r = dispatch(
        m(r#"export function add(a, b) { return a + b; }"#),
        "add",
        "[3,4]",
    ).unwrap();
    assert_eq!(r.json, "7");
}

#[test]
fn set_timeout_zero_delay() {
    let r = dispatch(m(r#"
        export function immediate() {
            return new Promise(function(resolve) {
                setTimeout(function() { resolve("immediate"); }, 0);
            });
        }
    "#), "immediate", "[]").unwrap();
    assert_eq!(r.json, "\"immediate\"");
}

#[test]
fn promise_resolve_sync() {
    let r = dispatch(m(r#"
        export async function test() {
            return "sync-async";
        }
    "#), "test", "[]").unwrap();
    assert_eq!(r.json, "\"sync-async\"");
}

#[test]
fn promise_then_chain_sync() {
    let r = dispatch(m(r#"
        export function test() {
            return Promise.resolve(1).then(v => v + 10).then(v => v * 2);
        }
    "#), "test", "[]").unwrap();
    assert_eq!(r.json, "22");
}

#[test]
fn async_generator_streams_sse() {
    // An async generator should be auto-wrapped in a Response(text/event-stream).
    // The dispatch_rpc path collapses Complete responses to their body, so
    // we see the full SSE frame sequence as a single string.
    let r = dispatch(m(r#"
        export async function* chat() {
            yield { token: "Hi" };
            yield { token: "!" };
        }
    "#), "chat", "[]").unwrap();
    assert!(r.json.contains("event: yield"), "got: {}", r.json);
    assert!(r.json.contains(r#"{"token":"Hi"}"#), "got: {}", r.json);
    assert!(r.json.contains(r#"{"token":"!"}"#), "got: {}", r.json);
    assert!(r.json.contains("event: return"), "got: {}", r.json);
}

#[test]
fn method_not_found_errors() {
    // Method lookup fails before touching user code. The error bubbles
    // out of dispatch_rpc as an Err("Method not found: ..." ).
    let err = dispatch(
        m(r#"export function ping() { return "pong"; }"#),
        "nope",
        "[]",
    ).unwrap_err();
    assert!(err.contains("Method not found"), "got: {}", err);
}
