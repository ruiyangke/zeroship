//! Heap-cap tests for `RuntimeBuilder::heap_limit_mb`.
//!
//! One process hosts many tenants' isolates, so an isolate whose heap reaches
//! its cap has to stop alone. The cap's near-heap-limit callback requests
//! termination and grants bounded headroom for the allocation in flight to
//! finish (`heap_cap` in the runtime). The runtime then runs no more JavaScript
//! in that isolate: the request that reached the cap and every request still
//! pending on the isolate fail with `memory limit exceeded`, and the isolate is
//! quarantined for its host to replace. The app sees a non-2xx, never a
//! success and never a hang, and no other isolate notices.
//!
//! A case whose failure would abort the process runs through
//! `in_own_process!`, so the abort fails that case with its own output.
//!
//! ## An allocation that is never read is never allocated
//!
//! Building `"x".repeat(1024 * 1024) + i` does not consume heap. V8 leaves the
//! value unmaterialised until something reads it, so a loop retaining such
//! strings holds far more nominal bytes than its cap while `used_heap_size`
//! barely moves and the near-heap-limit callback has nothing to fire about.
//! Reading one character of each forces the allocation. So a test which
//! allocates must prove it allocated: the callback count is the witness that
//! the cap, and not some other failure, refused the request.

use crate::support;
use support::*;
use crate::in_own_process;

use std::time::Duration;

use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime, SettledFetch};
use zeroship_runtime::channel::CancelFlag;

/// Run a single fetch handler against a freshly-built runtime and return the
/// (status, body) pair.
///
/// This drives `Pending` to settlement instead of rejecting it. A handler that
/// exhausts the heap does NOT come back synchronously: V8 terminates the
/// isolate mid-allocation, the dispatch cannot produce a Response on the spot,
/// and the outcome arrives as `Pending` carrying a `DispatchError`. A sync-only
/// helper reports that as a harness panic, which reads as "the cap is broken"
/// when the cap is in fact the thing that fired.
///
/// A `DispatchError` is mapped to status 500 rather than panicking: for these
/// tests it IS the result under test, not a harness failure.
fn dispatch_against(runtime: &Runtime) -> (u16, String) {
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
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        // Name the VARIANT. `type_name_of_val` on the binding prints the enum's
        // own path for every arm, so a mismatch here reports only
        // "got FetchOutcome" - true of all four and a description of none.
        FetchOutcome::Stream { status, .. } => {
            panic!("expected sync Response, got Stream (status {status})")
        }
        FetchOutcome::Pending { rx, cancel: _ } => compio::runtime::Runtime::new()
            .unwrap()
            .block_on(async {
                runtime.start_pump();
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("pending heap-limit dispatch never settled");
                match settled {
                    Ok(SettledFetch::Response { status, body, .. }) => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
                    Ok(SettledFetch::Stream { status, .. }) => {
                        (status, "<stream>".to_string())
                    }
                    Ok(SettledFetch::WebSocketUpgrade { .. }) => {
                        panic!("unexpected WebSocketUpgrade from a heap-limit dispatch")
                    }
                    // The OOM surface. Not a harness failure - the contract
                    // under test is that the app does not get a 2xx.
                    Err(e) => (500, format!("DispatchError: {e:?}")),
                }
            }),
        FetchOutcome::WebSocketUpgrade { ws_id, .. } => {
            panic!("expected sync Response, got WebSocketUpgrade (ws_id {ws_id})")
        }
    }
}

// ---------------------------------------------------------------------------
// 1 — default builder: no per-app cap, just the runtime's 128 MB default.
//     A modest in-heap allocation completes normally.
// ---------------------------------------------------------------------------

#[test]
fn runtime_default_no_heap_cap() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                // ~10 MB of in-heap objects — comfortably under 128 MB default.
                // (Plain JS objects live in the V8 heap, unlike ArrayBuffer
                // backing stores which V8 routes through the array-buffer
                // allocator and excludes from the GC budget.)
                const objs = [];
                for (let i = 0; i < 100_000; i++) {
                    objs.push({ id: i, name: "item-" + i, payload: "x".repeat(64) });
                }
                return Response.json({ count: objs.length });
            }
        };
    "#);
    let rt = Runtime::builder().modules(modules).build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""count":100000"#), "body: {body}");
}

// ---------------------------------------------------------------------------
// 2 — heap_limit_mb(32): allocate well past the cap, expect a non-2xx.
// ---------------------------------------------------------------------------
//
// The cap bounds this allocation once the allocation is real: see the module
// header.
//
// V8 routes ArrayBuffer backing stores through its array-buffer allocator,
// which is NOT counted against the heap limit configured by
// `CreateParams::heap_limits`, so a test that allocates buffers would pass
// against a cap that never fires. Strings live in the GC heap and do count,
// which is why the allocation below is strings - provided each one is read,
// per the module header.
//
// The allocation is deliberately a few hundred large strings rather than the
// 100M small property stores this test used before. That version could not
// finish inside any reasonable deadline, so the test reported a timeout whose
// message named neither the cap nor the allocation - it looked like a hang in
// the harness rather than a result about the runtime.

#[test]
fn runtime_heap_cap_enforces_oom() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const live = [];
                try {
                    // Large strings, retained. Building them is NOT enough:
                    // until something reads one, V8 leaves the value
                    // unmaterialised and the heap is never consumed, so the
                    // near-heap-limit callback never fires. Touching a byte
                    // forces materialisation.
                    let sink = 0;
                    for (let i = 0; i < 400; i++) {
                        const s = "x".repeat(1024 * 1024) + i;
                        sink += s.charCodeAt(s.length - 2);
                        live.push(s);
                    }
                    if (sink < 0) { throw new Error("unreachable"); }
                } catch (e) {
                    return new Response(JSON.stringify({
                        oom: true,
                        name: e?.name ?? "unknown",
                        message: String(e?.message ?? e),
                        iterations: live.length,
                    }), { status: 500, headers: { "content-type": "application/json" } });
                }
                return Response.json({ oom: false, iterations: live.length });
            }
        };
    "#);
    let before = zeroship_runtime::heap_limit_callback_hits();
    let rt = Runtime::builder().modules(modules).heap_limit_mb(32).build();
    let (status, body) = dispatch_against(&rt);
    let fired = zeroship_runtime::heap_limit_callback_hits() - before;

    // Reported unconditionally, including on the passing path: "the cap held"
    // and "V8 never consulted the cap" produce the same green here, and only
    // this number separates them.
    println!("near-heap-limit callback fired {fired} time(s) during this dispatch");

    // WITNESS, asserted before the outcome: the heap cap is what refused this
    // request. A non-2xx alone is satisfied by any failure - a CPU kill, a
    // module error, a panic mapped to 500 - none of which exercise the cap.
    assert!(
        fired > 0,
        "the near-heap-limit callback never fired, so whatever produced status \
         {status} was not the heap cap and this test did not exercise it; \
         body: {body}",
    );

    // Termination cannot be caught, so the handler's own `oom:true` arm
    // never answers: the runtime refuses the request. The contract is
    // "non-2xx".
    assert!(
        !(200..300).contains(&status),
        "expected non-2xx with heap_limit_mb(32), got {status}; body: {body}",
    );
}

// ---------------------------------------------------------------------------
// 3 — heap_limit_mb(64): typical handler completes normally.
// ---------------------------------------------------------------------------

#[test]
fn runtime_heap_cap_normal_load_passes() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                // Small JSON in / JSON out — well under any reasonable cap.
                const items = [];
                for (let i = 0; i < 100; i++) {
                    items.push({ id: i, name: "item-" + i });
                }
                return Response.json({ count: items.length, first: items[0] });
            }
        };
    "#);
    let rt = Runtime::builder().modules(modules).heap_limit_mb(64).build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""count":100"#), "body: {body}");
}

// Positive control for the counter used above. A zero from a freshly-written
// counter is indistinguishable from a counter that can never increment, so
// this asserts the instrument can move at all before the zero above is read as
// a result about V8.
#[test]
fn near_heap_limit_callback_counter_can_fire() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const live = [];
                try {
                    // Regular old-space objects, sized to cross a 32 MiB cap
                    // without running away: small unique-keyed property bags.
                    for (let i = 0; i < 3000; i++) {
                        const o = {};
                        for (let k = 0; k < 100; k++) {
                            o["k_" + i + "_" + k] = "v_" + i + "_" + k;
                        }
                        live.push(o);
                    }
                } catch (e) {
                    return Response.json({ oom: true, iterations: live.length });
                }
                return Response.json({ oom: false, iterations: live.length });
            }
        };
    "#);
    let before = zeroship_runtime::heap_limit_callback_hits();
    let rt = Runtime::builder().modules(modules).heap_limit_mb(32).build();
    let (status, body) = dispatch_against(&rt);
    let fired = zeroship_runtime::heap_limit_callback_hits() - before;
    println!("control: status {status}, fired {fired}, body {body}");
    assert!(
        fired > 0,
        "the near-heap-limit counter never incremented even for a regular \
         old-space allocation past the cap, so a zero elsewhere says nothing \
         about V8 - it may just mean this instrument is dead"
    );
}

/// A heap-limit termination settles the request it ended, instead of leaving
/// the caller waiting.
///
/// This is a REGULAR old-space allocation, so the near-heap-limit callback
/// runs and calls `terminate_execution`. The isolate really is terminated, and
/// something still has to convert that into a result.
///
/// The assertion is only "settles, non-2xx". Which error surfaces is not
/// pinned - a terminated isolate can be reported as a DispatchError or as a
/// 500, and both satisfy the contract that an app never hangs.
#[test]
fn heap_termination_settles_the_request_instead_of_hanging() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const live = [];
                for (let i = 0; i < 20000; i++) {
                    const o = {};
                    for (let k = 0; k < 100; k++) { o["k_" + i + "_" + k] = "v_" + i + "_" + k; }
                    live.push(o);
                }
                return Response.json({ oom: false, iterations: live.length });
            }
        };
    "#);
    let before = zeroship_runtime::heap_limit_callback_hits();
    let rt = Runtime::builder().modules(modules).heap_limit_mb(32).build();
    let (status, body) = dispatch_against(&rt);
    let fired = zeroship_runtime::heap_limit_callback_hits() - before;

    // Reported so a future failure can distinguish "termination never fired"
    // from "it fired and the result still did not arrive".
    println!("callback fired {fired} time(s); status {status}");
    assert!(
        fired > 0,
        "this test is meant to exercise the path where termination DOES fire; \
         it fired 0 times, so the allocation no longer trips the cap and the \
         test is measuring something else"
    );
    assert!(
        !(200..300).contains(&status),
        "expected non-2xx after heap termination, got {status}; body: {body}",
    );
}

// ---------------------------------------------------------------------------
// Reaching the cap stops the isolate, never the process
// ---------------------------------------------------------------------------

/// The cap every stop case runs under.
const STOP_CAP_MB: u32 = 64;

/// How long a case waits for an answer the stop owes it. Expiring it means
/// the request was stranded, not that the stop was slow.
const SETTLE_WITHIN: Duration = Duration::from_secs(30);

/// Routes for the stop cases. Each allocating route allocates in a timer
/// callback its request is waiting on, so the allocation runs on the event
/// pump with the request pending: the shape in which a request receives the
/// cap's error as its answer.
///
/// - `/huge` makes one allocation several times the cap: `fill` gives the
///   array its whole double backing store at once.
/// - `/grow` grows the heap past the cap in small retained steps.
/// - `/queued` queues a microtask that reports through `env.probe.ran()`,
///   makes the `/huge` allocation, and then loops, so the termination the
///   cap requested is taken at the loop's interrupt check and unwinds to the
///   runtime with the microtask still queued.
/// - `/scheduled` does the same with a zero-delay timer in place of the
///   microtask.
/// - `/report` queues the same microtask and answers: the control showing a
///   queued microtask does report.
/// - `/report-later` answers from a zero-delay timer that reports: the control
///   showing a queued timer does report.
/// - `/under` holds a sizeable share of the cap and answers.
/// - `/park` waits on a promise nothing settles.
/// - anything else answers at once.
const STOP_SOURCE: &str = r#"
    const later = (work) => new Promise(() => setTimeout(work, 0));
    export default {
        fetch(request, env) {
            const path = new URL(request.url).pathname;
            if (path === "/huge") {
                return later(() => new Array(20_000_000).fill(1.5));
            }
            if (path === "/grow") {
                return later(() => {
                    const held = [];
                    for (;;) held.push({ index: held.length, half: held.length + 0.5 });
                });
            }
            if (path === "/queued") {
                return later(() => {
                    queueMicrotask(() => env.probe.ran());
                    new Array(20_000_000).fill(1.5);
                    let spins = 0;
                    for (;;) spins++;
                });
            }
            if (path === "/scheduled") {
                return later(() => {
                    setTimeout(() => env.probe.ran(), 0);
                    new Array(20_000_000).fill(1.5);
                    let spins = 0;
                    for (;;) spins++;
                });
            }
            if (path === "/report-later") {
                return new Promise((resolve) => setTimeout(() => {
                    env.probe.ran();
                    resolve(new Response("reported"));
                }, 0));
            }
            if (path === "/report") {
                queueMicrotask(() => env.probe.ran());
                return new Response("reported");
            }
            if (path === "/under") {
                const held = new Array(2_000_000).fill(2.5);
                return new Response(String(held.length));
            }
            if (path === "/park") {
                return new Promise(() => {});
            }
            return new Response("served");
        }
    };
"#;

thread_local! {
    /// Queued work that reported through `env.probe.ran()` on this thread.
    static QUEUED_WORK_RAN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn ran_callback(_scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue) {
    QUEUED_WORK_RAN.with(|ran| ran.set(ran.get() + 1));
}

/// `env.probe.ran()`, through which queued work reports that it ran.
struct Probe;

impl zeroship_runtime::NativePlugin for Probe {
    fn namespace(&self) -> &'static str {
        "probe"
    }

    fn register(&self, r: &mut zeroship_runtime::NativeRegistrar) {
        r.add("ran", ran_callback);
    }
}

/// A tenant isolate built from [`STOP_SOURCE`] and left exited, as the
/// worker's per-thread cache keeps every isolate between dispatches.
fn tenant(heap_limit_mb: Option<u32>) -> Runtime {
    init_v8();
    let mut builder = Runtime::builder().modules(m(STOP_SOURCE)).plugin(Probe).idle_gc_after_ms(0);
    if let Some(mb) = heap_limit_mb {
        builder = builder.heap_limit_mb(mb);
    }
    let runtime = builder.build();
    runtime.exit_isolate();
    runtime
}

/// Dispatch one request the way the worker does: enter the tenant's isolate,
/// call, exit.
fn dispatch_entered(runtime: &Runtime, path: &str) -> FetchOutcome {
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "GET",
        &format!("http://localhost{path}"),
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    outcome
}

/// A synchronous answer.
fn answered(runtime: &Runtime, path: &str) -> (u16, String) {
    match dispatch_entered(runtime, path) {
        FetchOutcome::Response { status, body, .. } => (status, String::from_utf8_lossy(&body).into_owned()),
        _ => panic!("{path} must answer synchronously"),
    }
}

type Reply = zeroship_runtime::ResultReceiver<Result<SettledFetch, zeroship_runtime::runtime::DispatchError>>;

/// A request that must still be waiting on the pump when its dispatch returns.
fn pending(runtime: &Runtime, path: &str) -> Reply {
    match dispatch_entered(runtime, path) {
        FetchOutcome::Pending { rx, .. } => rx,
        _ => panic!("{path} must be pending on the pump when its dispatch returns"),
    }
}

/// Require the answer `path` receives to be the heap cap's error.
async fn assert_cap_error(path: &str, reply: &Reply) {
    let settled = compio::time::timeout(SETTLE_WITHIN, reply.recv())
        .await
        .unwrap_or_else(|_| panic!("{path} was never answered: the stop stranded it"));
    match settled {
        Err(error) => assert_eq!(
            (error.message.as_str(), error.status),
            ("memory limit exceeded", 500),
            "{path} must receive the heap cap's error"
        ),
        Ok(_) => panic!("{path} must be failed by the heap cap, but it answered"),
    }
}

/// Run `route` on a capped tenant with a request parked beside it, and a
/// bystander tenant on the same thread, and require the cap to stop the
/// capped isolate alone: both of its requests receive the cap's error, it is
/// quarantined and refuses later dispatch with the cause, and the bystander
/// serves before and after.
fn assert_route_stops_only_its_isolate(route: &str) {
    let bystander = tenant(None);
    let capped = tenant(Some(STOP_CAP_MB));
    compio::runtime::Runtime::new().expect("compio runtime").block_on(async {
        bystander.start_pump();
        capped.start_pump();
        assert_eq!(answered(&bystander, "/"), (200, "served".to_owned()), "the bystander serves before");
        assert_eq!(answered(&capped, "/"), (200, "served".to_owned()), "the capped isolate serves before");

        let hits_before = zeroship_runtime::heap_limit_callback_hits();
        let parked = pending(&capped, "/park");
        let reaching = pending(&capped, route);
        assert_cap_error(route, &reaching).await;
        assert_cap_error("/park", &parked).await;
        assert!(
            zeroship_runtime::heap_limit_callback_hits() > hits_before,
            "the near-heap-limit callback never fired, so the cap did not refuse {route}",
        );

        assert!(capped.is_quarantined(), "the isolate that reached its cap is quarantined");
        assert_eq!(
            answered(&capped, "/"),
            (500, r#"{"message":"module init failed: memory limit exceeded","name":"Error"}"#.to_owned()),
            "a stopped isolate refuses dispatch with its cause, and runs no handler",
        );
        assert!(!bystander.is_quarantined(), "the bystander is not stopped");
        assert_eq!(
            answered(&bystander, "/"),
            (200, "served".to_owned()),
            "another tenant's isolate on the same thread keeps serving",
        );
    });
}

/// One allocation several times the cap, larger than any headroom the cap
/// had left, stops that isolate and leaves the process and the thread's other
/// isolate serving.
#[test]
fn one_allocation_past_the_cap_stops_its_isolate_and_not_the_process() {
    in_own_process!(one_allocation_past_the_cap_stops_its_isolate_and_not_the_process, {
        assert_route_stops_only_its_isolate("/huge");
    });
}

/// Growing past the cap in small steps stops the isolate the same way, rather
/// than leaving it serving once the termination has unwound.
#[test]
fn gradual_growth_past_the_cap_stops_its_isolate_and_not_the_process() {
    in_own_process!(gradual_growth_past_the_cap_stops_its_isolate_and_not_the_process, {
        assert_route_stops_only_its_isolate("/grow");
    });
}

/// Run `route` on a fresh capped tenant whose queued work reports through
/// `env.probe.ran()`, after `control` shows such work does report, and require
/// the work `route` queued before reaching the cap never to run.
fn assert_queued_work_never_runs(control: &str, route: &str) {
    let capped = tenant(Some(STOP_CAP_MB));
    compio::runtime::Runtime::new().expect("compio runtime").block_on(async {
        capped.start_pump();
        let before = QUEUED_WORK_RAN.with(std::cell::Cell::get);
        match dispatch_entered(&capped, control) {
            FetchOutcome::Response { status, body, .. } => {
                assert_eq!((status, String::from_utf8_lossy(&body).into_owned()), (200, "reported".to_owned()));
            }
            FetchOutcome::Pending { rx, .. } => {
                let settled = compio::time::timeout(SETTLE_WITHIN, rx.recv())
                    .await
                    .unwrap_or_else(|_| panic!("{control} was never answered"));
                assert!(settled.is_ok(), "{control} must answer");
            }
            _ => panic!("{control} must answer with a buffered response"),
        }
        assert_eq!(
            QUEUED_WORK_RAN.with(std::cell::Cell::get),
            before + 1,
            "the control: work {control} queued reports",
        );

        let reaching = pending(&capped, route);
        assert_cap_error(route, &reaching).await;
        assert!(capped.is_quarantined(), "the isolate that reached its cap is quarantined");
        assert_eq!(
            QUEUED_WORK_RAN.with(std::cell::Cell::get),
            before + 1,
            "work {route} queued before the cap was reached ran after it",
        );
    });
}

/// A microtask queued before the allocation that reached the cap never runs:
/// the termination is spent once it unwinds to the runtime, and the queued
/// work would otherwise run on headroom granted only for unwinding.
#[test]
fn a_microtask_queued_before_the_cap_is_reached_never_runs() {
    in_own_process!(a_microtask_queued_before_the_cap_is_reached_never_runs, {
        assert_queued_work_never_runs("/report", "/queued");
    });
}

/// A zero-delay timer queued before the allocation that reached the cap never
/// fires, for the same reason: the pump drains new timers right after the
/// termination unwinds, and must not run them on an isolate past its cap.
#[test]
fn a_timer_queued_before_the_cap_is_reached_never_fires() {
    in_own_process!(a_timer_queued_before_the_cap_is_reached_never_fires, {
        assert_queued_work_never_runs("/report-later", "/scheduled");
    });
}

/// The rejection control: an app holding a sizeable share of its cap, but
/// under it, is untouched. It answers, the callback never fires, and the
/// isolate keeps serving.
#[test]
fn an_app_under_its_cap_is_untouched() {
    in_own_process!(an_app_under_its_cap_is_untouched, {
        let capped = tenant(Some(STOP_CAP_MB));
        compio::runtime::Runtime::new().expect("compio runtime").block_on(async {
            capped.start_pump();
            let hits_before = zeroship_runtime::heap_limit_callback_hits();
            assert_eq!(answered(&capped, "/under"), (200, "2000000".to_owned()));
            assert_eq!(answered(&capped, "/under"), (200, "2000000".to_owned()));
            assert_eq!(
                zeroship_runtime::heap_limit_callback_hits(),
                hits_before,
                "the near-heap-limit callback fired for an app under its cap",
            );
            assert!(!capped.is_quarantined(), "an app under its cap is not stopped");
            assert_eq!(answered(&capped, "/"), (200, "served".to_owned()));
        });
    });
}

/// Evidence for `UNWIND_GRANTS`. The largest object V8 builds in a single step
/// between interrupt checks is a `FixedDoubleArray` at its maximum capacity, a
/// 1 GiB allocation. Under a small cap this one allocation consults the
/// near-heap-limit callback exactly twice before it completes and the isolate
/// terminates: once to cross the cap, once when V8 reconsults after a GC at the
/// raised limit frees nothing. The process survives and the isolate is
/// quarantined. `UNWIND_GRANTS` must be at least that count; mutating it to 1
/// makes this one allocation abort the process (SIGTRAP), which is the
/// before-the-fix failure this case pins.
#[test]
fn a_single_maximal_allocation_is_caught_within_the_grant_ceiling() {
    in_own_process!(a_single_maximal_allocation_is_caught_within_the_grant_ceiling, {
        init_v8();
        // 128 Mi doubles = 1 GiB: `FixedDoubleArray::kMaxLength`, the largest
        // single heap object, allocated in one builtin call.
        let runtime = Runtime::builder()
            .modules(m(r#"
                export default {
                    fetch() {
                        const a = new Array(128 * 1024 * 1024).fill(0.5);
                        return new Response(String(a.length));
                    }
                };
            "#))
            .heap_limit_mb(STOP_CAP_MB)
            .idle_gc_after_ms(0)
            .build();
        let before = zeroship_runtime::heap_limit_callback_hits();
        let (status, body) = compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(async {
                runtime.start_pump();
                answer(&runtime)
            });
        let fired = zeroship_runtime::heap_limit_callback_hits() - before;
        println!("one maximal allocation consulted the callback {fired} time(s); status {status}");
        assert_eq!(
            fired, 2,
            "the largest single allocation consults the callback twice; \
             UNWIND_GRANTS must cover that. status {status}, body {body}",
        );
        assert!(
            !(200..300).contains(&status),
            "the maximal allocation must be refused, got {status}: {body}",
        );
        assert!(runtime.is_quarantined(), "the isolate is quarantined after the cap");
    });
}

/// Drive one dispatch to a buffered answer, starting the pump only now so a
/// `Pending` the allocation left settles.
fn answer(runtime: &Runtime) -> (u16, String) {
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "GET", "http://localhost/", &[], "", &EnvSnapshot::empty(), RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    match outcome {
        FetchOutcome::Response { status, body, .. } => (status, String::from_utf8_lossy(&body).into_owned()),
        other => panic!("expected a buffered Response, got {}", std::any::type_name_of_val(&other)),
    }
}

/// The stop releases the heap the cap's grants raised, rather than leaving the
/// isolate holding it until it is dropped. The allocation that reached the cap
/// is not retained, so the full GC the stop runs collects it: the isolate's
/// used heap afterwards is a small fraction of what it allocated. Without that
/// GC the used heap stays at the allocation's size until disposal.
#[test]
fn the_stop_releases_the_raised_heap() {
    in_own_process!(the_stop_releases_the_raised_heap, {
        init_v8();
        // ~160 MiB of doubles, not retained, allocated from a timer so the
        // request is pending on the pump when the cap is reached.
        let runtime = Runtime::builder()
            .modules(m(r#"
                export default {
                    fetch() {
                        return new Promise(() => setTimeout(() => { new Array(20_000_000).fill(1.5); }, 0));
                    }
                };
            "#))
            .heap_limit_mb(STOP_CAP_MB)
            .idle_gc_after_ms(0)
            .build();
        compio::runtime::Runtime::new().expect("compio runtime").block_on(async {
            runtime.start_pump();
            let env = EnvSnapshot::empty();
            let ctx = RequestCtx::new(CancelFlag::new());
            let FetchOutcome::Pending { rx, .. } =
                runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx)
            else {
                panic!("the allocation runs on a timer, so the dispatch is pending");
            };
            let settled = compio::time::timeout(SETTLE_WITHIN, rx.recv())
                .await
                .expect("the dispatch is answered, not stranded");
            match settled {
                Err(e) => assert_eq!(e.message, "memory limit exceeded", "the premise: the heap cap stops it"),
                Ok(_) => panic!("the premise: the heap cap stops it, but it answered"),
            }
            assert!(runtime.is_quarantined(), "the premise: the isolate is quarantined");

            let (used, _limit) = zeroship_runtime::heap_used_and_limit(&runtime);
            println!("used heap after the stop: {} MiB", used >> 20);
            assert!(
                used < (48 << 20),
                "the stop did not release the raised heap: {} MiB still used after a ~160 MiB allocation",
                used >> 20,
            );
        });
    });
}
