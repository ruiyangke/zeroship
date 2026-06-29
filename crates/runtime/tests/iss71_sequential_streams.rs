//! ISS-71b regression: sequential `stream()` RPCs on the SAME isolate.
//!
//! The browser E2E surfaced that the 2nd+ streamed response on an isolate (and
//! any stream issued after a prior one was aborted) stalls after its first
//! frame, while a single stream always streams fully. This drives the runtime
//! directly (no gateway/worker HTTP) to reproduce it deterministically: dispatch
//! the same async-generator stream procedure several times in a row on one
//! `Runtime` and assert every run yields all three frames + `d:{}`.

mod common;
use common::*;

use std::time::Duration;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, SettledFetch};

/// Dispatch `name` once and drain its SSE body. Each `wait_for_data` is bounded
/// so a STALLED stream returns the partial body (→ a clean assertion failure)
/// instead of hanging the test forever.
async fn dispatch_and_drain(runtime: &Runtime, name: &str) -> String {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url = format!("http://localhost/__zeroship/v1/{name}");
    let outcome = runtime.call_fetch_handler(
        "POST",
        &url,
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        ctx,
    );

    let reader = match outcome {
        FetchOutcome::Stream { body_reader, .. } => body_reader,
        FetchOutcome::Response { body, .. } => {
            return String::from_utf8_lossy(&body).into_owned();
        }
        FetchOutcome::Pending { rx, .. } => {
            match compio::time::timeout(Duration::from_secs(3), rx.recv()).await {
                Ok(Ok(SettledFetch::Stream { body_reader, .. })) => body_reader,
                Ok(Ok(SettledFetch::Response { body, .. })) => {
                    return String::from_utf8_lossy(&body).into_owned();
                }
                Ok(Ok(_)) => panic!("unexpected pending settle (not Stream/Response)"),
                Ok(Err(_)) => panic!("pending dispatch errored"),
                Err(_) => panic!("pending dispatch timed out"),
            }
        }
        _ => panic!("unexpected outcome (not Stream/Response/Pending)"),
    };

    let mut out = Vec::new();
    loop {
        while let Some(chunk) = reader.pop() {
            out.extend_from_slice(&chunk);
        }
        if reader.is_done() {
            break;
        }
        // Stall guard: a healthy 20ms-gap generator delivers the next frame
        // well within 2s. Timing out here is the bug.
        if compio::time::timeout(Duration::from_secs(2), reader.wait_for_data())
            .await
            .is_err()
        {
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn sequential_streams_on_one_isolate_each_yield_all_frames() {
    init_v8();
    // An async generator that yields three objects with a real timer gap
    // between them — exactly the shape of csr-todo's `searchTodos`.
    let modules = wrap_with_synthetic_entry(
        r#"
        async function* nums() {
            yield { n: 1 };
            await new Promise((r) => setTimeout(r, 20));
            yield { n: 2 };
            await new Promise((r) => setTimeout(r, 20));
            yield { n: 3 };
        }
        "#,
        "{ nums }",
    );
    let runtime = Runtime::builder().modules(modules).build();

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        for attempt in 1..=4 {
            let body = dispatch_and_drain(&runtime, "nums").await;
            assert!(
                body.contains("2:[{\"n\":1}]"),
                "attempt {attempt}: missing frame 1; body = {body:?}"
            );
            assert!(
                body.contains("2:[{\"n\":2}]"),
                "attempt {attempt}: missing frame 2 (STALL after first frame); body = {body:?}"
            );
            assert!(
                body.contains("2:[{\"n\":3}]"),
                "attempt {attempt}: missing frame 3; body = {body:?}"
            );
            assert!(
                body.contains("d:{}"),
                "attempt {attempt}: stream never terminated (no d:{{}}); body = {body:?}"
            );
        }
    });
}
