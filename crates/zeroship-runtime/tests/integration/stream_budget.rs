//! The live-stream budget belongs to each runtime's isolate.
//!
//! A worker thread hosts many apps' isolates, so a budget shared by the thread
//! would let one app's live streams refuse its neighbour's. These tests run
//! two runtimes on one thread through the public dispatch surface and pin the
//! three properties the budget owes its tenants: a runtime at its cap leaves
//! every other runtime untouched; the runtime that reached it gets an ordinary
//! `RangeError` from every construction that would exceed it, never a panic;
//! and dropping a runtime releases every stream it held.

use crate::support::wrap_with_synthetic_entry;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::streams::budget::{StreamBudget, MAX_LIVE_STREAMS};
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx};

/// One tenant's entry. `held` keeps streams reachable across calls, the way
/// module state outlives a request on a cached isolate. `held[0]` is a byte
/// stream and `held[1]` a default one, so both tee paths have a source that
/// was created before the cap was reached; the rest are `WritableStream`s, the
/// construction that costs the least heap per stream.
const TENANT: &str = r#"
const held = [];

function next() {
    if (held.length === 0) return new ReadableStream({ type: "bytes" });
    if (held.length === 1) return new ReadableStream();
    return new WritableStream();
}

function source(index, init) {
    return held[index] ?? new ReadableStream(init);
}

function outcome(construct) {
    try {
        construct();
        return "ok";
    } catch (e) {
        return `${e.name}: ${e.message}`;
    }
}

export function hold(count) {
    while (held.length < count) held.push(next());
    return held.length;
}

export function fill() {
    for (;;) {
        try {
            held.push(next());
        } catch (e) {
            if (e instanceof RangeError) return held.length;
            throw e;
        }
    }
}

export function transformStream() {
    return outcome(() => new TransformStream());
}

export function multiStream() {
    return {
        "new TransformStream": outcome(() => new TransformStream()),
        "ReadableStream.prototype.tee": outcome(() => source(1).tee()),
        "tee source still unlocked": String(held[1].locked === false),
        "byte ReadableStream.prototype.tee": outcome(() => source(0, { type: "bytes" }).tee()),
        "byte tee source still unlocked": String(held[0].locked === false),
    };
}

export function constructions() {
    return {
        "new ReadableStream": outcome(() => new ReadableStream()),
        "new byte ReadableStream": outcome(() => new ReadableStream({ type: "bytes" })),
        "new WritableStream": outcome(() => new WritableStream()),
        "new TransformStream": outcome(() => new TransformStream()),
        "ReadableStream.prototype.tee": outcome(() => source(1).tee()),
        "byte ReadableStream.prototype.tee": outcome(() => source(0, { type: "bytes" }).tee()),
        "Response.prototype.body": outcome(() => new Response("body").body),
        "Response.prototype.clone": outcome(() => new Response("body").clone()),
        "Request.prototype.body": outcome(
            () => new Request("http://localhost/", { method: "POST", body: "body" }).body,
        ),
        "Blob.prototype.stream": outcome(() => new Blob(["body"]).stream()),
        "new CompressionStream": outcome(() => new CompressionStream("gzip")),
        "new TextEncoderStream": outcome(() => new TextEncoderStream()),
    };
}
"#;

const PROCEDURES: &str = "{ hold, fill, transformStream, multiStream, constructions }";

/// Build a tenant runtime and leave its isolate exited, as the worker's
/// per-thread cache keeps every isolate between dispatches. The heap is sized
/// so one isolate can hold the whole cap; under the default heap limit V8
/// terminates the isolate first.
fn tenant() -> Runtime {
    init_v8();
    let runtime = Runtime::builder()
        .modules(wrap_with_synthetic_entry(TENANT, PROCEDURES))
        .heap_limit_mb(512)
        .build();
    runtime.exit_isolate();
    runtime
}

/// Dispatch one synchronous procedure the way the worker does: enter the
/// tenant's isolate, call, exit. Returns the procedure's JSON result.
fn call(runtime: &Runtime, procedure: &str, input: &str) -> serde_json::Value {
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "POST",
        &format!("http://localhost/__zeroship/v1/{procedure}"),
        &[("content-type".into(), "application/json".into())],
        format!(r#"{{"json":{input}}}"#),
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("{procedure} did not settle synchronously");
    };
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(status, 200, "{procedure} failed: {body}");
    let envelope: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("{procedure}: {e}: {body}"));
    envelope["json"].clone()
}

/// The `(construction, outcome)` pairs a `constructions` or `multiStream`
/// call reported, checked to be nonempty so an empty report cannot pass.
fn outcomes(report: &serde_json::Value) -> Vec<(String, String)> {
    let pairs: Vec<(String, String)> = report
        .as_object()
        .unwrap_or_else(|| panic!("not an outcome map: {report}"))
        .iter()
        .map(|(name, outcome)| (name.clone(), outcome.as_str().unwrap_or_default().to_owned()))
        .collect();
    assert!(!pairs.is_empty(), "no constructions were attempted");
    pairs
}

fn assert_refused(report: &serde_json::Value) {
    for (construction, outcome) in outcomes(report) {
        assert!(
            outcome.starts_with("RangeError: ") && outcome.contains("too many concurrent streams"),
            "{construction} at the cap must throw the stream budget's RangeError, got {outcome:?}",
        );
    }
}

#[test]
fn a_runtime_at_its_stream_cap_leaves_its_neighbour_on_the_thread_unaffected() {
    let a = tenant();
    let b = tenant();

    let held = call(&a, "fill", "null");
    assert_eq!(
        held,
        serde_json::json!(MAX_LIVE_STREAMS),
        "a fresh runtime holds exactly the cap before its budget refuses a stream",
    );
    for (construction, outcome) in outcomes(&call(&b, "constructions", "null")) {
        assert_eq!(
            outcome, "ok",
            "{construction} in B must not be charged for the streams A holds",
        );
    }

    // The control: A really is exhausted, so B's results above were measured
    // against a neighbour holding the whole cap.
    assert_refused(&call(&a, "constructions", "null"));
}

#[test]
fn exceeding_the_stream_cap_throws_range_error_in_the_runtime_that_exceeded_it() {
    let a = tenant();

    // Two free: a TransformStream needs three (itself and both halves).
    assert_eq!(call(&a, "hold", &(MAX_LIVE_STREAMS - 2).to_string()), MAX_LIVE_STREAMS - 2);
    let outcome = call(&a, "transformStream", "null");
    assert!(
        outcome.as_str().unwrap_or_default().starts_with("RangeError: "),
        "a TransformStream with two streams free must throw RangeError, got {outcome}",
    );

    // The refusal allocated nothing. One free: a TransformStream is refused
    // again, and a tee needs two, so each tee is refused before it locks its
    // source.
    assert_eq!(call(&a, "hold", &(MAX_LIVE_STREAMS - 1).to_string()), MAX_LIVE_STREAMS - 1);
    let report = call(&a, "multiStream", "null");
    for (construction, outcome) in outcomes(&report) {
        if construction.ends_with("still unlocked") {
            assert_eq!(outcome, "true", "{construction}: {report}");
        } else {
            assert!(
                outcome.starts_with("RangeError: "),
                "{construction} with one stream free must throw RangeError, got {outcome:?}",
            );
        }
    }

    // The refusals above allocated nothing: exactly one stream is still free.
    assert_eq!(call(&a, "hold", &MAX_LIVE_STREAMS.to_string()), MAX_LIVE_STREAMS);
    assert_refused(&call(&a, "constructions", "null"));

    // The runtime that exceeded its budget keeps serving.
    assert_eq!(call(&a, "hold", &MAX_LIVE_STREAMS.to_string()), MAX_LIVE_STREAMS);
}

#[test]
fn dropping_a_runtime_releases_every_stream_it_held() {
    let a = tenant();
    let budget = a.with_scope(|scope| StreamBudget::of(scope));

    assert_eq!(call(&a, "fill", "null"), MAX_LIVE_STREAMS);
    assert_eq!(budget.live(), MAX_LIVE_STREAMS, "the probe reads A's own budget");

    // Evict A with every stream still reachable from its module state.
    let isolate = a.into_inner_probe_for_test();
    assert_eq!(isolate.strong_count(), 0, "A's isolate outlived its last handle");
    assert_eq!(budget.live(), 0, "streams A held stayed charged after its isolate was disposed");
}
