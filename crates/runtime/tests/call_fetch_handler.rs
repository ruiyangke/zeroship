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
