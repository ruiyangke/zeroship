//! The pump CPU share budget, policed on every plan.
//!
//! The per-request CPU timer only sees JavaScript a dispatch runs. Timer
//! callbacks and promise continuations run later, inside the isolate's event
//! pump, and `RuntimeInner::record_pump_cpu` polices them instead: pump-side
//! JavaScript may hold only a share of wall time over a window. These cases run
//! on the unlimited shape - no CPU limit and no wall timeout - because that is
//! the plan where nothing else would ever answer a request the stop strands.
//!
//! The window and share are replaced through `RuntimeBuilder::pump_cpu_budget`
//! so a case reaches the stop without holding a core for the production window.

use std::time::Duration;

use zeroship_runtime::channel::{CancelFlag, StreamReader};
use zeroship_runtime::runtime::DispatchError;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime, SettledFetch};

use crate::support::m;

/// Short enough that a case reaches the stop promptly, long enough that the
/// light route below spans several windows.
const WINDOW: Duration = Duration::from_millis(200);
const MAX_FRACTION: f64 = 0.5;

/// How long a case waits for an answer the stop owes it. Far beyond the window,
/// so expiring it means the request was stranded, not that the stop was slow.
const SETTLE_WITHIN: Duration = Duration::from_secs(10);

/// Routes:
/// - `/spin` holds the pump: every timer callback busy-waits for most of the
///   time it occupies and re-arms itself, and the request never settles on its
///   own.
/// - `/park` waits on a promise nothing resolves.
/// - `/stream` answers at once with a body whose every chunk is produced by a
///   continuation that holds the pump the way `/spin` does.
/// - `/drip` answers at once with a body that streams a light chunk every few
///   milliseconds and never ends.
/// - `/light` does the same kind of work at a small share of wall time and
///   answers once it has run across several windows.
/// - `/oom` allocates past the heap cap, for the case that needs a recovered
///   heap kill on the isolate first.
/// - anything else answers synchronously.
///
/// The `ticks` procedure streams through native RPC: every item is produced by
/// a continuation that holds the pump the way `/spin` does.
const SOURCE: &str = r#"
    const burn = (ms) => { const end = Date.now() + ms; while (Date.now() < end) {} };
    export default {
        fetch(request) {
            const path = new URL(request.url).pathname;
            if (path === "/spin") {
                return new Promise(() => {
                    const spin = () => { burn(20); setTimeout(spin, 1); };
                    setTimeout(spin, 1);
                });
            }
            if (path === "/park") {
                return new Promise(() => {});
            }
            if (path === "/stream") {
                const encoder = new TextEncoder();
                let chunk = 0;
                return new Response(new ReadableStream({
                    async pull(controller) {
                        await new Promise((resolve) => setTimeout(resolve, 1));
                        burn(20);
                        controller.enqueue(encoder.encode(`chunk ${chunk++}\n`));
                    },
                }));
            }
            if (path === "/drip") {
                const encoder = new TextEncoder();
                let chunk = 0;
                return new Response(new ReadableStream({
                    async pull(controller) {
                        await new Promise((resolve) => setTimeout(resolve, 5));
                        controller.enqueue(encoder.encode(`drip ${chunk++}\n`));
                    },
                }));
            }
            if (path === "/light") {
                return new Promise((resolve) => {
                    let ticks = 0;
                    const step = () => {
                        burn(2);
                        ticks += 1;
                        if (ticks === 20) {
                            resolve(new Response(`light ${ticks}`));
                        } else {
                            setTimeout(step, 20);
                        }
                    };
                    setTimeout(step, 20);
                });
            }
            if (path === "/oom") {
                const live = [];
                let sink = 0;
                for (let i = 0; i < 400; i++) {
                    const s = "x".repeat(1024 * 1024) + i;
                    sink += s.charCodeAt(s.length - 2);
                    live.push(s);
                }
                return new Response(`allocated ${live.length} ${sink}`);
            }
            return new Response("sync");
        },
        rpc: {
            ticks: () => ({
                [Symbol.asyncIterator]() { return this; },
                async next() {
                    await new Promise((resolve) => setTimeout(resolve, 1));
                    burn(20);
                    return { value: "tick", done: false };
                },
                return() { return { done: true }; },
            }),
        },
    };
"#;

fn budgeted() -> zeroship_runtime::RuntimeBuilder {
    init_v8();
    Runtime::builder()
        .modules(m(SOURCE))
        .idle_gc_after_ms(0)
        .pump_cpu_budget(WINDOW, MAX_FRACTION)
}

fn dispatch(runtime: &Runtime, path: &str) -> FetchOutcome {
    runtime.call_fetch_handler(
        "GET",
        &format!("http://localhost{path}"),
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    )
}

/// A route that must still be waiting on the pump when the dispatch returns.
fn pending(runtime: &Runtime, path: &str) -> zeroship_runtime::ResultReceiver<Result<SettledFetch, DispatchError>> {
    match dispatch(runtime, path) {
        FetchOutcome::Pending { rx, .. } => rx,
        _ => panic!("{path} must be pending on the pump when its dispatch returns"),
    }
}

/// The answer a pending request receives, or a panic naming the request that
/// was stranded.
async fn settled(
    path: &str,
    rx: &zeroship_runtime::ResultReceiver<Result<SettledFetch, DispatchError>>,
) -> Result<(u16, String), DispatchError> {
    let settled = compio::time::timeout(SETTLE_WITHIN, rx.recv())
        .await
        .unwrap_or_else(|_| panic!("{path} was never answered: the stop stranded it"));
    settled.map(|settled| match settled {
        SettledFetch::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        _ => panic!("{path} settled to something other than a buffered response"),
    })
}

/// A synchronous answer.
fn answered(runtime: &Runtime, path: &str) -> (u16, String) {
    match dispatch(runtime, path) {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        _ => panic!("{path} must answer synchronously"),
    }
}

fn assert_cpu_stop(path: &str, outcome: Result<(u16, String), DispatchError>) {
    match outcome {
        Err(error) => {
            assert_eq!(
                (error.message.as_str(), error.status),
                ("CPU time limit exceeded", 500),
                "{path} must receive the CPU termination error"
            );
        }
        Ok((status, body)) => {
            panic!("{path} must be failed by the stop, but answered {status}: {body}")
        }
    }
}

/// Exceeding the share stops the isolate the way a CPU overrun stops a request:
/// the request whose continuations held the pump AND a bystander parked on the
/// same isolate are answered with the CPU termination error, and the isolate
/// runs no handler afterwards.
#[test]
fn exceeding_the_pump_share_fails_every_pending_request_and_stops_the_isolate() {
    let runtime = budgeted().build();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        assert_eq!(answered(&runtime, "/"), (200, "sync".into()), "the isolate serves before the stop");
        let parked = pending(&runtime, "/park");
        let spinning = pending(&runtime, "/spin");

        assert_cpu_stop("/spin", settled("/spin", &spinning).await);
        assert_cpu_stop("/park", settled("/park", &parked).await);
        assert!(
            !runtime.has_pending_requests_for_test(),
            "the stop leaves no request waiting on the isolate"
        );

        assert_eq!(
            answered(&runtime, "/"),
            (
                500,
                r#"{"message":"module init failed: CPU time limit exceeded","name":"Error"}"#.into()
            ),
            "a stopped isolate refuses dispatch with its cause, and runs no handler"
        );
    });
}

/// A streamed body read to its end: the bytes, and the producer's failure if
/// the body ended in one.
async fn read_to_end(reader: &StreamReader) -> (String, Option<String>) {
    let mut received = Vec::new();
    compio::time::timeout(SETTLE_WITHIN, async {
        while !reader.is_done() {
            received.extend(reader.drain().concat());
            reader.wait_for_data().await;
        }
    })
    .await
    .expect("the streamed body was never ended: the stop stranded it");
    received.extend(reader.drain().concat());
    (String::from_utf8_lossy(&received).into_owned(), reader.error())
}

/// A response whose head is already out but whose body the pump is still
/// producing is not left open by the stop: its body ends as a producer failure
/// naming the cause, so the consumer stops waiting instead of hanging on a
/// stream nothing will ever write to again.
#[test]
fn exceeding_the_pump_share_fails_a_body_still_streaming() {
    let runtime = budgeted().build();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        let FetchOutcome::Stream { status, body_reader, .. } = dispatch(&runtime, "/stream") else {
            panic!("/stream answers its head at once and streams its body");
        };
        assert_eq!(status, 200);
        let (received, error) = read_to_end(&body_reader).await;
        assert!(
            received.starts_with("chunk 0\n"),
            "the body was streaming before the stop: {received:?}"
        );
        assert_eq!(
            error.as_deref(),
            Some("CPU time limit exceeded"),
            "the body ends as a producer failure naming the cause, not as a clean end"
        );
    });
}

/// A streaming RPC procedure has its own end-of-stream protocol, and a client
/// reads a stream that stops without its terminal frame as one that finished.
/// So the stop ends it with the terminal error frame carrying the CPU error,
/// followed by the done frame - what the client turns into a thrown error
/// rather than a silent prefix of the data.
#[test]
fn exceeding_the_pump_share_ends_an_rpc_stream_with_its_error_frame() {
    let runtime = budgeted().build();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        let outcome = runtime.call_fetch_handler(
            "POST",
            "http://localhost/__zeroship/v1/ticks",
            &[("content-type".into(), "application/json".into())],
            "{}",
            &EnvSnapshot::empty(),
            RequestCtx::new(CancelFlag::new()),
        );
        let FetchOutcome::Stream { status, body_reader, .. } = outcome else {
            panic!("the ticks procedure answers its head at once and streams its items");
        };
        assert_eq!(status, 200);
        let (received, error) = read_to_end(&body_reader).await;
        let frames: Vec<&str> = received.lines().collect();
        let [items @ .., failure, done] = frames.as_slice() else {
            panic!("the stream ends in two terminal frames: {received:?}");
        };
        assert!(
            !items.is_empty() && items.iter().all(|frame| *frame == r#"0:"tick""#),
            "items streamed before the stop: {received:?}"
        );
        assert_eq!(
            *failure,
            r#"e:{"message":"CPU time limit exceeded","name":"Error","code":"INTERNAL","retryable":false}"#
        );
        assert_eq!(*done, "d:{}");
        assert_eq!(error, None, "the terminal frames are the stream's own end");
    });
}

/// Host quarantine ends a body it cuts off with its own reason, not a pump
/// share cause: the forwarded-body failure belongs to quarantine, whichever
/// path invoked it.
#[test]
fn host_quarantine_fails_a_body_still_streaming_with_its_own_reason() {
    let runtime = budgeted().build();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        let FetchOutcome::Stream { body_reader, .. } = dispatch(&runtime, "/drip") else {
            panic!("/drip answers its head at once and streams its body");
        };
        compio::time::timeout(SETTLE_WITHIN, async {
            while !body_reader.has_data() {
                body_reader.wait_for_data().await;
            }
        })
        .await
        .expect("the body streams before the quarantine");
        runtime.quarantine();
        let (received, error) = read_to_end(&body_reader).await;
        assert!(received.starts_with("drip 0\n"), "the body was streaming: {received:?}");
        assert_eq!(error.as_deref(), Some("runtime has been quarantined"));
    });
}

/// The cause is the CPU budget even on an isolate whose last detected
/// termination was a heap kill it recovered from. The stop names its own
/// cause rather than reporting whichever limit fired last.
#[test]
fn a_recovered_heap_kill_does_not_rename_a_pump_share_stop() {
    let runtime = budgeted().heap_limit_mb(32).build();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        let heap_before = zeroship_runtime::heap_limit_callback_hits();
        let killed = match dispatch(&runtime, "/oom") {
            FetchOutcome::Response { status, body, .. } => {
                Ok((status, String::from_utf8_lossy(&body).into_owned()))
            }
            FetchOutcome::Pending { rx, .. } => settled("/oom", &rx).await,
            _ => panic!("/oom answers with a buffered response or an error"),
        };
        assert!(
            zeroship_runtime::heap_limit_callback_hits() > heap_before,
            "the premise: the heap cap fired on this isolate"
        );
        match killed {
            Ok((status, body)) => assert!(
                !(200..300).contains(&status),
                "the premise: /oom is refused by the heap cap, got {status}: {body}"
            ),
            Err(error) => assert_eq!(error.message, "memory limit exceeded", "the premise"),
        }
        assert_eq!(answered(&runtime, "/"), (200, "sync".into()), "a heap kill is recovered");

        let spinning = pending(&runtime, "/spin");
        assert_cpu_stop("/spin", settled("/spin", &spinning).await);
    });
}

/// The rejection control: the same work at a small share of wall time, run
/// across several windows, is never stopped. Without it the cases above would
/// pass over a guard that stopped any isolate the pump ran JavaScript for.
#[test]
fn pump_work_under_its_share_is_not_stopped() {
    let runtime = budgeted().build();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        let started = std::time::Instant::now();
        let light = pending(&runtime, "/light");
        assert_eq!(
            settled("/light", &light).await.expect("light pump work is not stopped"),
            (200, "light 20".into())
        );
        assert!(
            started.elapsed() > WINDOW * 2,
            "the light route ran across several budget windows, so the guard judged it"
        );
        assert_eq!(answered(&runtime, "/"), (200, "sync".into()), "the isolate still serves");
    });
}

/// A module whose top-level await holds the pump the way `/spin` does, so the
/// share is exceeded before startup finishes.
const STARTUP_SOURCE: &str = r#"
    const burn = (ms) => { const end = Date.now() + ms; while (Date.now() < end) {} };
    await new Promise((resolve) => {
        let steps = 0;
        const step = () => {
            burn(20);
            steps += 1;
            if (steps < 100000) { setTimeout(step, 1); } else { resolve(); }
        };
        setTimeout(step, 1);
    });
    export default { fetch() { return new Response("served"); } };
"#;

/// Exceeding the share while the module is still starting fails the load
/// itself, and a request already waiting on startup is answered with the
/// same cause. A host that awaits startup before serving sees the load fail;
/// one that queues requests on it answers them with the refusal.
#[test]
fn exceeding_the_pump_share_during_startup_fails_the_load_and_its_waiting_requests() {
    init_v8();
    let runtime = Runtime::builder()
        .modules(m(STARTUP_SOURCE))
        .idle_gc_after_ms(0)
        .pump_cpu_budget(WINDOW, MAX_FRACTION)
        .build();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let waiting = pending(&runtime, "/");
        let error = compio::time::timeout(SETTLE_WITHIN, runtime.initialize(&EnvSnapshot::empty()))
            .await
            .expect("startup was never ended: the stop stranded it")
            .expect_err("startup cannot succeed once the isolate is stopped");
        assert_eq!(error, "module init failed: CPU time limit exceeded");
        assert_eq!(
            settled("/", &waiting).await.expect("the waiting request is answered, not failed"),
            (
                500,
                r#"{"message":"module init failed: CPU time limit exceeded","name":"Error"}"#.into()
            )
        );
        assert!(runtime.is_quarantined(), "the stopped isolate is quarantined");
    });
}
