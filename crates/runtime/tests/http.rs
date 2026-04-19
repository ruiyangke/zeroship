use zeroship_runtime::{init_v8, ModuleEntry};
use zeroship_runtime::runtime::{Runtime, DispatchOutcome};

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
    let runtime = Runtime::builder().modules(modules.clone()).build();

    let (status, _headers, body) = match runtime.dispatch_http("GET", "http://localhost/hello", "[]", "", None) {
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
    let runtime = Runtime::builder().modules(modules.clone()).build();

    // RPC still works
    let rpc = runtime.dispatch_rpc("add", "[3,4]").unwrap();
    assert_eq!(rpc.json, "7");

    // HTTP also works
    match runtime.dispatch_http("GET", "http://localhost/", "[]", "", None) {
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
    let runtime = Runtime::builder().modules(modules.clone()).build();
    match runtime.dispatch_http("GET", "http://localhost/", "[]", "", None) {
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
    let runtime = Runtime::builder().modules(modules.clone()).build();
    match runtime.dispatch_http("GET", "http://localhost/", "[]", "", None) {
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
    let runtime = Runtime::builder().modules(modules.clone()).build();
    match runtime.dispatch_http("GET", "http://localhost/hello?name=world", "[]", "", None) {
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
    let runtime = Runtime::builder().modules(modules.clone()).build();
    match runtime.dispatch_http("GET", "http://localhost/events", "[]", "", None) {
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

// Regression: when an async ReadableStream.start() enqueued chunks across
// timer-driven await points and then called controller.close(), the earlier
// close implementation removed the stream from `outbound_streams` before the
// pump's final flush ran. That flush keys off outbound_streams membership,
// so (1) the last chunks before close stayed buffered forever, and (2) the
// forwarder was never closed — HTTP clients saw the response hang waiting
// for the chunked-encoding terminator that never arrived. The streaming
// `[DONE]` marker in SSE was the canonical symptom.
#[test]
fn streaming_http_response_async_closes_cleanly() {
    init_v8();

    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export async function onRequest(request) {
                // Force the handler promise to be pending past the initial
                // microtask drain so the runtime takes the async dispatch
                // path — that's where the bug lived (StreamForwarder +
                // outbound_streams membership vs direct_writer).
                await new Promise((r) => setTimeout(r, 0));
                const encoder = new TextEncoder();
                const body = new ReadableStream({
                    async start(controller) {
                        controller.enqueue(encoder.encode("data: tick 0\n\n"));
                        await new Promise((r) => setTimeout(r, 0));
                        controller.enqueue(encoder.encode("data: tick 1\n\n"));
                        await new Promise((r) => setTimeout(r, 0));
                        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
                        controller.close();
                    },
                });
                return new Response(body, {
                    headers: { "Content-Type": "text/event-stream" },
                });
            }
        "#.into(),
    }];

    compio::runtime::Runtime::new().unwrap().block_on(async move {
        let runtime = Runtime::builder().modules(modules).build();
        runtime.start_pump();

        // onRequest resolves to the Response synchronously (start() is async
        // but the enclosing function returns without awaiting it), so we go
        // straight to HttpStream with the pump driving the timer chain.
        let reader = match runtime.dispatch_http("GET", "http://localhost/events", "[]", "", None) {
            DispatchOutcome::HttpStream { status, body, .. } => {
                assert_eq!(status, 200);
                body
            }
            DispatchOutcome::HttpPending { rx, .. } => {
                let result = compio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                    .await
                    .expect("dispatch_http timed out")
                    .expect("dispatch_http returned error");
                match result {
                    zeroship_runtime::runtime::HttpDispatchResult::Stream { body, status, .. } => {
                        assert_eq!(status, 200);
                        body
                    }
                    other => panic!("expected Stream, got {:?}", std::mem::discriminant(&other)),
                }
            }
            other => panic!(
                "expected HttpStream or HttpPending, got {:?}",
                std::mem::discriminant(&other)
            ),
        };

        // Drain the body until is_done. The critical assertion is that the
        // stream *does* close — a buggy close path leaves the reader waiting
        // forever for a chunk that never arrives.
        let collected = compio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut out = String::new();
            loop {
                while let Some(chunk) = reader.pop() {
                    out.push_str(&String::from_utf8_lossy(&chunk));
                }
                if reader.is_done() {
                    break;
                }
                reader.wait_for_data().await;
            }
            out
        })
        .await
        .expect("reader never saw close after controller.close()");

        assert!(collected.contains("data: tick 0"), "got: {collected}");
        assert!(collected.contains("data: tick 1"), "got: {collected}");
        assert!(collected.contains("data: [DONE]"), "final marker missing; got: {collected}");
    });
}
