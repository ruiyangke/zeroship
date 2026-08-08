//! Diagnosis for the hypothesised pull/cancel double-borrow race in
//! `crates/runtime/src/web/streams/readable_default_controller.rs`'s
//! `set_up_readable_stream_default_controller_native`.
//!
//! The hypothesis: both the `pull` and `cancel` `AlgorithmFn` closures
//! built there close over the *same* `Rc<RefCell<S>>` and hold a
//! `borrow_mut()` across an `.await`. If `cancel_steps` ever fires while
//! a `pull` future is suspended mid-poll, the cancel closure's
//! `source_rc.borrow_mut()` should hit an already-mutably-borrowed
//! `RefCell` and panic with `BorrowMutError`.
//!
//! This file contains two tests that together settle the question:
//!
//! 1. [`js_driven_read_then_cancel_never_touches_native_source`] — the
//!    literal ask: a real `Runtime`, JS calls `.read()` then
//!    `.cancel()` on a `NativeSource`-backed stream. **This does NOT
//!    reproduce the panic**, and the test proves *why*: `pull()` and
//!    `cancel()` are never invoked at all (counters stay at 0,
//!    verified by execution, not just by reading the source). The
//!    mechanism is `algorithm_snapshot()` in
//!    `readable_default_controller.rs` (used by both
//!    `invoke_pull_algorithm` and `cancel_steps`), which converts
//!    `AlgorithmFn::Native` / `AlgorithmFn::NativeReason` to
//!    `AlgorithmSnapshot::Noop` *before* either the pull or cancel
//!    driving path ever calls the boxed closure — see
//!    `readable_default_controller.rs:705-714` — and
//!    `AlgorithmFn::invoke_with_controller` /
//!    `invoke_with_reason` short-circuit the same way independently
//!    at lines 264-266 / 313-315. So today, on this exact JS surface,
//!    the boxed closures containing `source_rc.borrow_mut()` are
//!    genuinely dead code: constructed and stored, never called.
//!
//! 2. [`direct_closure_pull_then_cancel_panics_with_borrow_mut_error`]
//!    — bypasses the (currently absent) driving mechanism and invokes
//!    the *actual* boxed closures built by
//!    `set_up_readable_stream_default_controller_native` directly, the
//!    way a future runtime-loop driver would. **This DOES reproduce**
//!    a `BorrowMutError` panic, confirming the hazard described in the
//!    dispatch brief is real in the closures as written — it just has
//!    no live caller yet.
//!
//! Established by reading (not merely guessed): the exact non-driving
//! mechanism in (1), cited above with line numbers. Established by
//! running: both the "never invoked" claim in (1) and the panic in (2).

#![allow(unsafe_code)]

use std::cell::Cell;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_runtime::streams::readable_default_controller::{AlgorithmFn, DefaultControllerState};
use zeroship_runtime::streams::slots::{read_slot, CONTROLLER};
use zeroship_runtime::streams::{from_native_source, NativeReadableController, NativeSource};
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

// ---------------------------------------------------------------------------
// Test 1 — the literal ask: drive via JS on a real Runtime.
// ---------------------------------------------------------------------------

thread_local! {
    static PULL_CALLS: Cell<u32> = const { Cell::new(0) };
    static CANCEL_CALLS: Cell<u32> = const { Cell::new(0) };
}

/// A `NativeSource` whose `pull()` genuinely suspends forever
/// (`std::future::pending()` — no timer, no reactor dependency, never
/// resolves on its own) so that IF it were ever driven, it would still
/// be suspended when a subsequent `cancel()` lands. Records every call
/// into thread-local counters so the JS-driven test can observe
/// (from Rust) whether the runtime ever actually invoked these methods.
struct SuspendingSource;

impl NativeSource for SuspendingSource {
    fn pull(
        &mut self,
        _controller: &mut NativeReadableController,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>> {
        Box::pin(async move {
            PULL_CALLS.with(|c| c.set(c.get() + 1));
            std::future::pending::<()>().await;
            Ok(())
        })
    }

    fn cancel(
        &mut self,
        _reason: Option<v8::Global<v8::Value>>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>> {
        Box::pin(async move {
            CANCEL_CALLS.with(|c| c.set(c.get() + 1));
            Ok(())
        })
    }
}

fn make_stream_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let stream = from_native_source(scope, SuspendingSource, 1.0);
    rv.set(stream.into());
}

fn counts_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let pull = PULL_CALLS.with(|c| c.get());
    let cancel = CANCEL_CALLS.with(|c| c.get());
    let obj = v8::Object::new(scope);
    let pull_key = v8::String::new(scope, "pull").unwrap();
    obj.set(scope, pull_key.into(), v8::Number::new(scope, pull as f64).into());
    let cancel_key = v8::String::new(scope, "cancel").unwrap();
    obj.set(scope, cancel_key.into(), v8::Number::new(scope, cancel as f64).into());
    rv.set(obj.into());
}

struct RaceTestPlugin;

impl NativePlugin for RaceTestPlugin {
    fn namespace(&self) -> &str {
        "racetest"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("makeStream", make_stream_callback);
        r.add("counts", counts_callback);
    }
}

async fn run_js(module_src: &str) -> String {
    init_v8();
    // Fresh counters per invocation — tests in this file don't share a
    // thread, but be explicit rather than relying on that.
    PULL_CALLS.with(|c| c.set(0));
    CANCEL_CALLS.with(|c| c.set(0));

    let modules = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: module_src.to_string(),
    }];
    let runtime = Runtime::builder()
        .modules(modules)
        .plugins(vec![Arc::new(RaceTestPlugin) as Arc<dyn NativePlugin>])
        .build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    match outcome {
        FetchOutcome::Response { body, .. } => String::from_utf8_lossy(&body).into_owned(),
        FetchOutcome::Pending { rx, .. } => {
            match compio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Ok(SettledFetch::Response { body, .. })) => {
                    String::from_utf8_lossy(&body).into_owned()
                }
                Ok(Ok(_)) => panic!("handler settled as a stream or WS upgrade, expected a Response"),
                Ok(Err(_)) => panic!("settled-fetch channel closed before a Response arrived"),
                Err(_) => panic!("handler did not settle within 10s"),
            }
        }
        _ => panic!("handler neither returned nor pended a Response"),
    }
}

/// The literal ask from the dispatch brief: build a real `Runtime`,
/// have JS start reading a `NativeSource`-backed stream (parking the
/// read), then cancel it while "parked", and assert no panic + cancel
/// resolves.
///
/// **Result: GREEN, and not for a trivial reason.** The test proves via
/// execution (thread-local counters visible to both the V8 callback and
/// the Rust assertions) that `reader.read()` and `reader.cancel()` on a
/// `NativeSource`-backed stream never call into `SuspendingSource::pull`
/// or `::cancel` AT ALL on this build — `pull` and `cancel` counts stay
/// 0 throughout, and the `read()` promise is still unsettled after a
/// timer well past when a real pull would have delivered a chunk. The
/// double-borrow hazard cannot fire here because neither closure runs.
#[compio::test]
async fn js_driven_read_then_cancel_never_touches_native_source() {
    let body = run_js(
        r#"
export default {
    async fetch(_request, env) {
        const s = env.racetest.makeStream();
        const reader = s.getReader();
        const readPromise = reader.read();

        // Flush whatever microtask reactions `call_pull_if_needed`'s
        // `upon_promise` scheduled.
        await Promise.resolve();
        await Promise.resolve();
        await Promise.resolve();
        const afterRead = env.racetest.counts();

        // Race the still-outstanding read() against a timer: if pull()
        // had ever been driven and enqueued a chunk, read() would have
        // resolved well within 50ms.
        const raceResult = await Promise.race([
            readPromise.then(() => "resolved"),
            new Promise((r) => setTimeout(() => r("timeout"), 50)),
        ]);

        let cancelSettled = false;
        let cancelThrew = null;
        try {
            await reader.cancel("race-test-reason");
            cancelSettled = true;
        } catch (e) {
            cancelThrew = { name: e?.name ?? null, message: e?.message ?? String(e) };
        }
        const afterCancel = env.racetest.counts();

        return new Response(JSON.stringify({
            afterRead, raceResult, cancelSettled, cancelThrew, afterCancel,
        }), { headers: { "content-type": "application/json" } });
    }
};
"#,
    )
    .await;

    let v: serde_json::Value = serde_json::from_str(&body).expect("handler response must be JSON");
    assert_eq!(
        v["afterRead"],
        serde_json::json!({"pull": 0, "cancel": 0}),
        "pull()/cancel() must not have run yet after read() + microtask flush: {body}"
    );
    assert_eq!(
        v["raceResult"],
        serde_json::json!("timeout"),
        "read() must still be unsettled 50ms later (nothing ever pushes a chunk): {body}"
    );
    assert_eq!(
        v["cancelSettled"],
        serde_json::json!(true),
        "reader.cancel() must resolve without throwing: {body}"
    );
    assert_eq!(
        v["afterCancel"],
        serde_json::json!({"pull": 0, "cancel": 0}),
        "cancel() must not have invoked NativeSource::cancel either: {body}"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — bypass the (currently absent) driver and call the actual
// boxed closures directly, the way a future runtime-loop driver would.
// ---------------------------------------------------------------------------

/// Same suspend-forever shape as `SuspendingSource`, but independent
/// (no thread-local bookkeeping needed — this test never runs a JS
/// driving path at all, it drives the closures by hand).
struct RaceSource;

impl NativeSource for RaceSource {
    fn pull(
        &mut self,
        _controller: &mut NativeReadableController,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>> {
        // Genuinely suspends: never resolves on its own, no reactor
        // required to reach the Pending state (unlike a timer, this
        // can't flake on a slow CI box).
        Box::pin(async move {
            std::future::pending::<()>().await;
            Ok(())
        })
    }

    fn cancel(
        &mut self,
        _reason: Option<v8::Global<v8::Value>>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), v8::Global<v8::Value>>> + 'static>> {
        Box::pin(async move { Ok(()) })
    }
}

fn run_in_v8<F, R>(f: F) -> R
where
    F: FnOnce(&mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    f(scope)
}

/// Drives the exact boxed `pull`/`cancel` closures that
/// `set_up_readable_stream_default_controller_native` builds — reached
/// via the same internal-field raw-pointer technique the production
/// code itself uses in `readable_stream_default_controller_clear_algorithms`
/// (`readable_default_controller.rs:860-870`) — bypassing
/// `algorithm_snapshot`'s Noop short-circuit (see test 1's doc comment)
/// entirely. This is what a runtime-loop driver wired up to actually
/// call `AlgorithmFn::Native`/`NativeReason` would do.
///
/// **Result: RED, as predicted.** `pull_fut.poll()` suspends
/// (confirmed `Poll::Pending`) holding `source_rc`'s `RefMut` across the
/// await, exactly as the source comments describe. The subsequent
/// `cancel_fut.poll()` call panics — `RefCell::borrow_mut` on an
/// already-mutably-borrowed cell — with the standard library's
/// `BorrowMutError` panic message. This confirms the hazard described
/// in the dispatch brief is real *in the closures as written*; it is
/// simply unreachable today because nothing calls them (test 1).
///
/// Left RED deliberately per the dispatch brief ("DO NOT FIX
/// ANYTHING" — diagnosis only). Do not add `#[should_panic]` here
/// without checking with whoever owns the fix; a red test is the
/// intended deliverable of this dispatch.
#[test]
fn direct_closure_pull_then_cancel_panics_with_borrow_mut_error() {
    run_in_v8(|scope| {
        let stream = from_native_source(scope, RaceSource, 1.0);

        let controller_v = read_slot(scope, stream, CONTROLLER);
        let controller = v8::Local::<v8::Object>::try_from(controller_v)
            .expect("from_native_source must wire a controller onto the stream's [[controller]] slot");

        let raw = controller
            .get_internal_field(scope, 0)
            .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
            .expect("controller must have internal field 0 set")
            .value() as *mut DefaultControllerState;
        assert!(!raw.is_null(), "controller's internal field must not be null");
        // SAFETY: mirrors `readable_stream_default_controller_clear_algorithms`'s
        // own raw-pointer access to the controller wrapper's internal
        // field 0 (readable_default_controller.rs:860-870) — the External
        // was set by `set_up_readable_stream_default_controller_native`
        // to a live `Box<DefaultControllerState>`, dropped only by the
        // isolate's weak finalizer, which hasn't run (the isolate is
        // still alive and we hold `scope`).
        let state: &mut DefaultControllerState = unsafe { &mut *raw };

        let pull_alg = std::mem::replace(&mut state.pull_algorithm, AlgorithmFn::Noop);
        let cancel_alg = std::mem::replace(&mut state.cancel_algorithm, AlgorithmFn::Noop);

        let AlgorithmFn::Native(mut pull_fn) = pull_alg else {
            panic!(
                "set_up_readable_stream_default_controller_native must install \
                 AlgorithmFn::Native as the pull algorithm"
            );
        };
        let AlgorithmFn::NativeReason(mut cancel_fn) = cancel_alg else {
            panic!(
                "set_up_readable_stream_default_controller_native must install \
                 AlgorithmFn::NativeReason as the cancel algorithm"
            );
        };

        let controller_obj_global = v8::Global::new(scope, controller);
        let mut pull_fut = pull_fn(controller_obj_global);

        let waker: &'static Waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        // Step 1: poll the pull future once. It must suspend (Pending)
        // — that's the "pull is parked" precondition the hypothesis
        // needs. This is where `source_rc.borrow_mut()` is acquired and
        // held live across the `std::future::pending().await` inside it.
        let poll1 = pull_fut.as_mut().poll(&mut cx);
        assert!(
            matches!(poll1, Poll::Pending),
            "pull() must genuinely suspend for this test to exercise the race; \
             it resolved instead, which would prove nothing"
        );

        // Step 2: cancel "lands" while the pull future above is still
        // alive (not dropped) and still holding its RefMut. Poll the
        // cancel future once — this is where the brief predicts a
        // `BorrowMutError` panic.
        let reason_v: v8::Local<v8::Value> = v8::undefined(scope).into();
        let reason_g = v8::Global::new(scope, reason_v);
        let mut cancel_fut = cancel_fn(Some(reason_g));

        let _ = cancel_fut.as_mut().poll(&mut cx); // <-- expected panic here

        // Keep `pull_fut` alive up to (and, if we somehow got here,
        // past) the cancel poll — dropping it earlier would release the
        // RefMut and defeat the whole point of the test.
        drop(pull_fut);
    });
}
