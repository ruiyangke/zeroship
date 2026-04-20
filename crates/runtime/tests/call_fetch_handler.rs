mod common;
use common::*;

use std::time::Duration;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime, SettledFetch};

#[test]
fn simple_response() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                return new Response("hello", { status: 200 });
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200);
            assert_eq!(body, "hello");
        }
        _ => panic!("expected Response outcome"),
    }
}

#[test]
fn async_response() {
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                await new Promise(r => setTimeout(r, 0));
                return new Response("later", { status: 202 });
            }
        };
    "#);

    // call_fetch_handler must return Pending for async handlers. The pump
    // then drives the promise to completion and delivers the final
    // SettledFetch via the receiver. This mirrors the idiom used in
    // crates/runtime/tests/http.rs ~line 198 for dispatch_http.
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let runtime = Runtime::builder().modules(modules).build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler(
            "GET",
            "http://localhost/",
            &[],
            "",
            &env,
            ctx,
        );

        let FetchOutcome::Pending { rx, cancel: _ } = outcome else {
            panic!("expected Pending outcome, got different variant");
        };

        let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("receiver wait timed out")
            .expect("pending delivered DispatchError");

        match settled {
            SettledFetch::Response { status, body, .. } => {
                assert_eq!(status, 202);
                assert_eq!(body, "later");
            }
            _ => panic!("expected SettledFetch::Response"),
        }
    });
}

#[test]
fn handler_throwing_http_error_preserves_status() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const err = new Error("not found");
                err.status = 404;
                throw err;
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 404, "expected 404 from thrown err.status, got status={} body={}", status, body);
            assert!(body.contains("not found"), "body: {}", body);
        }
        _ => panic!("expected Response outcome"),
    }
}

#[test]
fn streaming_response() {
    // A synchronously-closed ReadableStream is collapsed by `inspect_response`
    // to `ResponseInfo::Complete` (matching dispatch_http's behavior). To
    // actually exercise the `Stream` arm, the handler must be async so
    // inspect_response runs while the stream is still open (pre-start). We
    // then schedule the close via a setTimeout(..., 0) so the pump fires it
    // only after inspect_response has snapshotted the stream.
    //
    // Pattern mirrors `streaming_http_response_async_closes_cleanly` in
    // tests/http.rs.
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                // Force the handler to be async so inspect_response runs
                // while the stream is still open (pre-start).
                await new Promise(r => setTimeout(r, 0));
                const enc = new TextEncoder();
                const stream = new ReadableStream({
                    start(controller) {
                        controller.enqueue(enc.encode("chunk1"));
                        controller.enqueue(enc.encode("chunk2"));
                        // Keep the stream open until after the Response is
                        // returned; close via a timer task so inspect_response
                        // sees it as still-open (Stream arm), then it drains
                        // cleanly once the pump fires the timer.
                        //
                        // queueMicrotask is too early: microtasks drain
                        // before the handler's outer Promise resolves, so
                        // inspect_response would see a closed stream and
                        // collapse it to Complete. A setTimeout(..., 0)
                        // yields to the compio event loop and fires only
                        // after inspect_response has snapshotted the stream.
                        setTimeout(() => controller.close(), 0);
                    }
                });
                return new Response(stream, {
                    status: 200,
                    headers: { "content-type": "text/plain" }
                });
            }
        };
    "#);

    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let runtime = Runtime::builder().modules(modules).build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler(
            "GET",
            "http://localhost/",
            &[],
            "",
            &env,
            ctx,
        );

        let FetchOutcome::Pending { rx, cancel: _ } = outcome else {
            panic!("expected Pending outcome for async handler");
        };

        let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("receiver wait timed out")
            .expect("pending delivered DispatchError");

        match settled {
            SettledFetch::Stream { status, headers: _, body_reader: _, logs: _ } => {
                assert_eq!(status, 200);
                // Body drain verified by the sibling http.rs tests that
                // exercise the same StreamReader machinery — here we
                // assert only that the Stream variant was produced.
            }
            SettledFetch::Response { status, body, .. } => {
                panic!(
                    "expected Stream variant but got Response — inspect_response may \
                     have collapsed the stream because it closed too early \
                     (status={}, body={})",
                    status, body
                );
            }
            SettledFetch::WebSocketUpgrade { .. } => {
                panic!("expected Stream variant, got WebSocketUpgrade");
            }
        }
    });
}
