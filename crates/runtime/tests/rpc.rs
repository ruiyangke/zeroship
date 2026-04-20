mod common;
use common::*;

// These tests exercise the pre-kernel-cut named-export + JSON-args contract,
// riding on top of `call_fetch_handler` via the `DISPATCH_BOOTSTRAP_JS` helper
// in `common/mod.rs`. They prove that a simple user module can still expose
// individual functions as RPC methods — the exact pattern the PR 2 bootstrap
// will re-implement in user-space.

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
    // An async generator should be auto-wrapped in a Response(text/event-stream)
    // by the DISPATCH_BOOTSTRAP_JS helper. dispatch() buffers the full stream
    // body into a single string for assertion purposes.
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
    // The bootstrap throws `Error('Method not found: <name>')` with
    // err.status = 404 when the user module has no matching named export.
    let err = dispatch(
        m(r#"export function ping() { return "pong"; }"#),
        "nope",
        "[]",
    ).unwrap_err();
    assert!(err.contains("Method not found"), "got: {}", err);
}

#[test]
fn plain_object_with_status_is_not_response() {
    // Regression guard for the `__zsResponse` prototype tag: a handler
    // return shaped like `{ status, url }` must be JSON-encoded verbatim,
    // not fed into the Response inspection path.
    let r = dispatch(m(r#"
        export async function fetchLike() {
            return { status: 200, url: "http://example.com" };
        }
    "#), "fetchLike", "[]").unwrap();
    assert_eq!(r.json, r#"{"status":200,"url":"http://example.com"}"#);
}

#[test]
fn user_returned_response_passes_through() {
    // A user-constructed `new Response(...)` goes through the HTTP
    // inspection path (the tag is on `Response.prototype`). The bootstrap
    // passes the Response through unchanged; dispatch() collapses the
    // buffered body into the json field for assertion.
    let r = dispatch(m(r#"
        export function respond() {
            return new Response("hello", { status: 200 });
        }
    "#), "respond", "[]").unwrap();
    assert_eq!(r.json, "hello");
}
