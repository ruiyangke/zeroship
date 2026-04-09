mod common;
use common::*;

use appbase_runtime::{init_v8, ModuleEntry};
use appbase_runtime::runtime::{Runtime, DispatchOutcome};

#[test]
fn on_request_basic() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export function onRequest(request) {
                return new Response("Hello from " + request.method + " " + request.url, {
                    status: 200,
                    headers: { "X-Custom": "test" },
                });
            }
        "#.into(),
    }];
    let mut runtime = Runtime::new_direct(modules, no_env(), None, None);

    let (status, _headers, body) = match runtime.dispatch_http("GET", "http://localhost/hello", "[]", "") {
        DispatchOutcome::HttpComplete { status, headers, body, .. } => (status, headers, body),
        _ => panic!("expected HttpComplete"),
    };
    assert_eq!(status, 200);
    assert!(body.contains("Hello from GET"), "got: {}", body);
}

#[test]
fn on_request_with_rpc() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export function add(a, b) { return a + b; }
            export function onRequest(request) {
                return new Response("HTTP handler", { status: 200 });
            }
        "#.into(),
    }];
    let mut runtime = Runtime::new_direct(modules, no_env(), None, None);

    // RPC still works
    let rpc = runtime.dispatch_rpc(r#"{"jsonrpc":"2.0","method":"add","params":[3,4],"id":1}"#).unwrap();
    assert!(rpc.json.contains("7"));

    // HTTP also works
    match runtime.dispatch_http("GET", "http://localhost/", "[]", "") {
        DispatchOutcome::HttpComplete { status, body, .. } => {
            assert_eq!(status, 200);
            assert!(body.contains("HTTP handler"));
        }
        _ => panic!("expected HttpComplete"),
    }
}

#[test]
fn no_on_request_returns_error() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: "export function ping() { return 'pong'; }".into(),
    }];
    let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
    match runtime.dispatch_http("GET", "http://localhost/", "[]", "") {
        DispatchOutcome::Complete(Err(e)) => assert!(e.contains("No onRequest")),
        _ => panic!("expected error"),
    }
}

#[test]
fn on_request_async() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export async function onRequest(request) {
                return new Response("async response", { status: 201 });
            }
        "#.into(),
    }];
    let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
    match runtime.dispatch_http("GET", "http://localhost/", "[]", "") {
        DispatchOutcome::HttpComplete { status, body, .. } => {
            assert_eq!(status, 201);
            assert!(body.contains("async response"));
        }
        _ => panic!("expected HttpComplete for trivially-async handler"),
    }
}

#[test]
fn url_in_http_handler() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export function onRequest(request) {
                const url = new URL(request.url);
                return new Response(JSON.stringify({
                    path: url.pathname,
                    query: url.searchParams.get("name"),
                }), { headers: { "Content-Type": "application/json" } });
            }
        "#.into(),
    }];
    let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
    match runtime.dispatch_http("GET", "http://localhost/hello?name=world", "[]", "") {
        DispatchOutcome::HttpComplete { status, body, .. } => {
            assert_eq!(status, 200);
            assert!(body.contains("\"path\":\"/hello\""), "got: {}", body);
            assert!(body.contains("\"query\":\"world\""), "got: {}", body);
        }
        _ => panic!("expected HttpComplete"),
    }
}

#[test]
fn streaming_http_response_sync() {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export function onRequest(request) {
                var stream = new ReadableStream({
                    start(controller) {
                        controller.enqueue("data: event 0\n\n");
                        controller.enqueue("data: event 1\n\n");
                        controller.enqueue("data: event 2\n\n");
                        controller.close();
                    }
                });
                return new Response(stream, {
                    headers: { "Content-Type": "text/event-stream" }
                });
            }
        "#.into(),
    }];
    let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
    match runtime.dispatch_http("GET", "http://localhost/events", "[]", "") {
        DispatchOutcome::HttpComplete { status, body, .. } => {
            assert_eq!(status, 200);
            assert!(body.contains("data: event 0"), "got: {}", body);
            assert!(body.contains("data: event 2"), "got: {}", body);
        }
        DispatchOutcome::HttpStream { status, headers, body: body_reader, .. } => {
            assert_eq!(status, 200);
            let ct = headers.iter().find(|(k, _)| k == "content-type");
            assert!(ct.is_some());
            let mut body = String::new();
            for chunk in body_reader.drain() {
                body.push_str(&String::from_utf8_lossy(&chunk));
            }
            assert!(body.contains("data: event 0"), "got: {}", body);
        }
        _ => panic!("expected HttpComplete or HttpStream"),
    }
}
