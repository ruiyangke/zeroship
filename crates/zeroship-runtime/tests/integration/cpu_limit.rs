//! CPU-limit termination, which had no test at all until now.
//!
//! `cargo test -p zeroship-runtime cpu` matched zero tests while
//! `check_v8_terminated` - the function BOTH the CPU timer and the heap-limit
//! callback depend on - was being rewritten. The heap route gained coverage in
//! `heap_limits.rs`; this file covers the older route, so a change made for one
//! cannot silently break the other.
//!
//! Linux-only: the CPU timer is a POSIX timer and `RuntimeInner` only carries
//! one under `#[cfg(target_os = "linux")]`. On other targets there is nothing
//! to test, so the test compiles out rather than passing vacuously.

#![cfg(target_os = "linux")]

use crate::support;
use support::*;

use std::sync::mpsc;
use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::{JavaScriptModule, NativePlugin, NativeRegistrar};
use zeroship_runtime::state::{OpResult, ResolveValue, SharedState};
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime, SettledFetch, WorkflowOutcome,
    WORKFLOW_DISPATCH_MODULE,
};

use crate::in_own_process;

/// Drive a dispatch to settlement, mapping a DispatchError to 500. Mirrors the
/// helper in `heap_limits.rs`: a terminated isolate may answer synchronously or
/// through the pending channel, and both are valid.
fn dispatch_against(runtime: &Runtime) -> (u16, String) {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    match runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx) {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        FetchOutcome::Stream { status, .. } => (status, "<stream>".to_string()),
        FetchOutcome::WebSocketUpgrade { .. } => panic!("unexpected WebSocketUpgrade"),
        FetchOutcome::Pending { rx, cancel: _ } => compio::runtime::Runtime::new()
            .unwrap()
            .block_on(async {
                runtime.start_pump();
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("cpu-limited dispatch never settled");
                match settled {
                    Ok(SettledFetch::Response { status, body, .. }) => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
                    Ok(SettledFetch::Stream { status, .. }) => (status, "<stream>".to_string()),
                    Ok(SettledFetch::WebSocketUpgrade { .. }) => {
                        panic!("unexpected WebSocketUpgrade")
                    }
                    Err(e) => (500, format!("DispatchError: {e:?}")),
                }
            }),
    }
}

/// Drive a workflow replay dispatch to settlement. Symmetric to
/// `dispatch_against`, but for `call_workflow_dispatch`'s own outcome type and
/// its JSON-only (no status code) settlement.
fn dispatch_workflow_against(runtime: &Runtime, envelope_json: &str) -> Result<String, String> {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    match runtime.call_workflow_dispatch(envelope_json, &env, ctx) {
        WorkflowOutcome::Response { json, .. } => Ok(json),
        WorkflowOutcome::Pending { rx, .. } => {
            compio::runtime::Runtime::new().unwrap().block_on(async {
                runtime.start_pump();
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("workflow dispatch never settled");
                match settled {
                    Ok(settled) => Ok(settled.json),
                    Err(error) => Err(error.message),
                }
            })
        }
    }
}

/// A handler that burns CPU past its limit must be terminated and the request
/// settled with a non-2xx, rather than running to completion or hanging.
///
/// The loop is BOUNDED on purpose, and the bound is the point: it completes on
/// its own in about a second (see the control below), so if the timer never
/// fired this test would see a 200 and fail. An unbounded loop cannot make that
/// distinction - it would hang either way, reporting "cap broken" and "cap
/// works but nothing settles" as the same timeout.
#[test]
fn cpu_limit_terminates_a_runaway_handler() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                // Pure CPU, no allocation: this must trip the CPU timer and not
                // the heap cap, so the two termination routes stay separable.
                let x = 0;
                for (let i = 0; i < 300000000; i++) { x = (x + 1) % 2147483647; }
                return Response.json({ completed: true, x });
            }
        };
    "#);
    let heap_before = zeroship_runtime::heap_limit_callback_hits();
    let rt = Runtime::builder()
        .modules(modules)
        .cpu_limit(Duration::from_millis(200))
        .build();

    let (status, body) = dispatch_against(&rt);
    // WITNESS that the CPU limit is what refused this, not the heap cap. Both
    // routes now end in the same non-2xx through the same function, so status
    // alone cannot tell them apart, and a heap kill here would mean the test
    // is measuring the wrong limit. The loop allocates nothing, so any
    // heap-callback activity at all is a signal that it does.
    //
    // Exact zero relies on more than "this file allocates nothing": the
    // counter is process-wide, and this file shares its test binary with
    // `heap_limits.rs`, whose tests DO trip it on purpose. What actually
    // makes the zero safe is that nextest runs every test in its own process
    // (`.config/nextest.toml`), so no other test's hits ever land in this
    // one's before/after window. Under a bare `cargo test`, where tests share
    // one process, this subtraction is racy against whichever heap test ran
    // concurrently; move to a per-runtime counter before relying on it there.
    assert_eq!(
        zeroship_runtime::heap_limit_callback_hits() - heap_before,
        0,
        "the heap callback fired during a pure-CPU handler, so status {status} \
         may be a memory kill rather than the CPU limit this test names",
    );
    assert!(
        !(200..300).contains(&status),
        "a runaway handler under cpu_limit(200ms) returned {status}; body: {body}",
    );
}

// Control for the test above: the SAME bounded loop with no cpu_limit. If this
// returns 200 promptly, the loop completes on its own, so a Pending-that-never-
// settles under cpu_limit is caused by the termination and not by slow JS.
#[test]
fn control_same_loop_without_a_cpu_limit_completes() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                let x = 0;
                for (let i = 0; i < 300000000; i++) { x = (x + 1) % 2147483647; }
                return Response.json({ completed: true, x });
            }
        };
    "#);
    let rt = Runtime::builder().modules(modules).build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""completed":true"#), "body: {body}");
}

// ===========================================================================
// Transparent huge pages
// ===========================================================================

/// Read the kernel's own report of this process's transparent-huge-page flag
/// from `/proc/self/status`'s `THP_enabled` line (1 = enabled, 0 = disabled;
/// see proc(5) - the opposite polarity from `prctl(PR_GET_THP_DISABLE)`).
/// Plain file I/O - no `unsafe` needed to observe what
/// `disable_transparent_huge_pages` (which uses a `prctl` syscall under
/// `rustix::thread`) did.
fn thp_enabled_flag() -> i32 {
    let status = std::fs::read_to_string("/proc/self/status")
        .expect("read this process's /proc/self/status");
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("THP_enabled:") {
            return value
                .trim()
                .parse()
                .unwrap_or_else(|e| panic!("THP_enabled is not an integer: {value:?} ({e})"));
        }
    }
    panic!("/proc/self/status carries no THP_enabled line on this kernel");
}

/// The CPU limit runs on the thread CPU clock, which also counts the kernel's
/// work on the thread's behalf. A page fault into memory advised for
/// transparent huge pages can compact memory synchronously, so a process that
/// hosts isolates turns transparent huge pages off; otherwise that work is
/// charged to whichever request's window takes the fault.
///
/// Runs in its own process (`in_own_process!`): the flag is inherited across
/// fork and exec, so under a bare `cargo test`, where many tests share one
/// process and most of them call `init_v8()` too, the child this macro
/// re-execs can start with huge pages already off. The test turns them back
/// on itself before the "before" check, rather than trusting the child's
/// inherited state, so the check that follows is never vacuous about
/// whether `init_v8` is what turned them off again.
#[test]
fn hosting_isolates_turns_off_transparent_huge_pages() {
    in_own_process!(hosting_isolates_turns_off_transparent_huge_pages, {
        rustix::thread::disable_transparent_huge_pages(false)
            .expect("turn transparent huge pages back on for this process");

        let before = thp_enabled_flag();
        assert_eq!(
            before, 1,
            "rejection control: THP_enabled must read 1 (on) before init_v8 runs, \
             or the assertion below would pass vacuously",
        );

        init_v8();

        let after = thp_enabled_flag();
        assert_eq!(
            after, 0,
            "init_v8 must turn transparent huge pages off for this process",
        );
    });
}

// ===========================================================================
// Off-CPU time is not charged to the CPU budget
// ===========================================================================
//
// `RuntimeInner::check_cpu_limit` compares `PendingRequest::cpu_accumulated`
// (summed from the thread CPU clock) against `cpu_limit`. Each of the seven
// sites that feed `cpu_accumulated` reads that clock around its own V8
// window: the initial fetch dispatch, the workflow dispatch, the three
// `handle_op_result_pump` arms (`Completed`, `Failed`, `JsValue`),
// `handle_timer_pump` (a delayed timer) and `fire_ready_timers_pump` (a
// zero-delay timer). A test below binds each one by making its window block
// off-CPU for longer than the budget while spending almost no CPU, then
// asserting the request is not refused for CPU.
//
// `Atomics.wait` cannot be the off-CPU tool here: the committed isolate-
// runaway proposal disables it for creator code, so a test built on it would
// start failing once that change lands for a reason unrelated to this one.
// `ParkPlugin` below blocks a thread off-CPU by receiving on a channel
// nobody sends on, with a timeout - ordinary blocking I/O, not CPU.
//
// Every test here follows up the parked window with two zero-delay turns
// before returning. Settling in the SAME window that just parked would
// remove the request from `pending_requests` before `check_cpu_limit` (or
// even the `cpu_accumulated` update feeding it) runs against it - a real gap
// in `check_cpu_limit`'s own enforcement, not something these tests should
// paper over. The extra turns keep the request pending long enough for the
// accumulated total, parked window included, to actually reach the check.

/// `env.park.sync(ms)` blocks the calling thread for `ms` milliseconds
/// without doing CPU work. `env.park.completed()` / `.failed()` /
/// `.jsValue()` each return a promise that settles later, through the
/// matching `handle_op_result_pump` arm (`OpResult::Completed`, `Failed` and
/// `JsValue` respectively) - the same raw op-result shapes
/// `tests/integration/auth_plugin.rs`'s `TurnPlugin` uses for `Completed` and
/// `Failed`.
struct ParkPlugin;

/// A minimal `zeroship:workflows/dispatch` host module (see
/// `WORKFLOW_DISPATCH_MODULE`) so the one workflow-dispatch test can drive
/// `call_workflow_dispatch` without depending on `zeroship-workflow-v8`. A
/// plugin module's specifier must live under its own namespace's
/// `zeroship:<namespace>/` prefix, so this is a separate plugin from
/// `ParkPlugin` even though the two are always registered together. The
/// bridge ignores the replay envelope and just awaits whatever async
/// function the test's creator module assigned to
/// `globalThis.__zsWorkflowBody`.
struct WorkflowDispatchBridgePlugin;

impl NativePlugin for WorkflowDispatchBridgePlugin {
    fn namespace(&self) -> &str {
        "workflows"
    }

    fn register(&self, _r: &mut NativeRegistrar) {}

    fn host_javascript_modules(&self) -> &'static [JavaScriptModule] {
        &[JavaScriptModule {
            specifier: WORKFLOW_DISPATCH_MODULE,
            source: "export async function dispatch(_creator, _envelope, _ctx) { \
                      return await globalThis.__zsWorkflowBody(); }",
        }]
    }
}

fn park_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let ms = args.get(0).integer_value(scope).unwrap_or(0).max(0) as u64;
    let (_tx, rx) = mpsc::channel::<()>();
    let _ = rx.recv_timeout(Duration::from_millis(ms));
    rv.set(v8::undefined(scope).into());
}

fn park_completed_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let resolver = v8::PromiseResolver::new(scope).expect("promise resolver");
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);

    let mut s = state.borrow_mut();
    let op_id = s.next_op_id;
    s.next_op_id += 1;
    s.pending_resolvers.insert(op_id, resolver);
    let request_id = s.executing_request_id;
    s.spawned_ops.push(Box::pin(async move {
        compio::time::sleep(Duration::from_millis(1)).await;
        OpResult::Completed { op_id, value: "park-completed".to_string(), request_id }
    }));
    drop(s);

    rv.set(promise.into());
}

fn park_failed_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let resolver = v8::PromiseResolver::new(scope).expect("promise resolver");
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);

    let mut s = state.borrow_mut();
    let op_id = s.next_op_id;
    s.next_op_id += 1;
    s.pending_resolvers.insert(op_id, resolver);
    let request_id = s.executing_request_id;
    s.spawned_ops.push(Box::pin(async move {
        compio::time::sleep(Duration::from_millis(1)).await;
        OpResult::Failed { op_id, error: "park-failed".to_string(), request_id }
    }));
    drop(s);

    rv.set(promise.into());
}

fn park_js_value_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let resolver = v8::PromiseResolver::new(scope).expect("promise resolver");
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        compio::time::sleep(Duration::from_millis(1)).await;
        OpResult::JsValue { resolver, value: ResolveValue::Undefined, request_id }
    }));

    rv.set(promise.into());
}

impl NativePlugin for ParkPlugin {
    fn namespace(&self) -> &str {
        "park"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("sync", park_sync_callback);
        r.add("completed", park_completed_callback);
        r.add("failed", park_failed_callback);
        r.add("jsValue", park_js_value_callback);
    }
}

/// Window: `fire_ready_timers_pump` (a zero-delay `setTimeout`), bound
/// through the park plugin rather than `Atomics.wait` for the reason
/// explained above.
#[test]
fn time_a_window_spends_off_cpu_is_not_charged_to_the_cpu_budget() {
    init_v8();
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                for (let turn = 0; turn < 4; turn++) {
                    await new Promise((resolve) => setTimeout(resolve, 0));
                    env.park.sync(150);
                }
                return Response.json({ completed: true });
            }
        };
    "#);
    let rt = Runtime::builder()
        .modules(modules)
        .plugin(ParkPlugin)
        .cpu_limit(Duration::from_millis(50))
        .build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
}

/// Window: the initial fetch dispatch (`call_fetch_handler_started`, before
/// the handler's first `await`).
#[test]
fn initial_fetch_dispatch_off_cpu_time_is_not_charged_to_the_cpu_budget() {
    init_v8();
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                env.park.sync(150);
                await new Promise((resolve) => setTimeout(resolve, 0));
                await new Promise((resolve) => setTimeout(resolve, 0));
                return Response.json({ completed: true });
            }
        };
    "#);
    let rt = Runtime::builder()
        .modules(modules)
        .plugin(ParkPlugin)
        .cpu_limit(Duration::from_millis(50))
        .build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
}

/// Window: the workflow dispatch (`call_workflow_dispatch`), reached through
/// a different kernel entry point than any fetch/RPC path.
#[test]
fn workflow_dispatch_off_cpu_time_is_not_charged_to_the_cpu_budget() {
    init_v8();
    let modules = m(r#"
        import { env } from "zeroship";
        globalThis.__zsWorkflowBody = async function () {
            env.park.sync(150);
            await new Promise((resolve) => setTimeout(resolve, 0));
            await new Promise((resolve) => setTimeout(resolve, 0));
            return { completed: true };
        };
        export default {};
    "#);
    let rt = Runtime::builder()
        .modules(modules)
        .plugin(ParkPlugin)
        .plugin(WorkflowDispatchBridgePlugin)
        .cpu_limit(Duration::from_millis(50))
        .build();
    let result = dispatch_workflow_against(&rt, "{}");
    let json = result.unwrap_or_else(|message| panic!("workflow dispatch was refused: {message}"));
    assert!(json.contains(r#""completed":true"#), "json: {json}");
}

/// Window: `handle_timer_pump`, reached through a delayed (non-zero)
/// `setTimeout`, as opposed to `fire_ready_timers_pump`'s zero-delay turns.
#[test]
fn delayed_timer_off_cpu_time_is_not_charged_to_the_cpu_budget() {
    init_v8();
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                await new Promise((resolve) => setTimeout(() => {
                    env.park.sync(150);
                    resolve();
                }, 5));
                await new Promise((resolve) => setTimeout(resolve, 0));
                await new Promise((resolve) => setTimeout(resolve, 0));
                return Response.json({ completed: true });
            }
        };
    "#);
    let rt = Runtime::builder()
        .modules(modules)
        .plugin(ParkPlugin)
        .cpu_limit(Duration::from_millis(50))
        .build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
}

/// Window: `handle_op_result_pump`'s `Completed` arm. The code right after
/// `await env.park.completed()` is this arm's own continuation, run inside
/// its `perform_microtask_checkpoint` - the same window the arm measures.
#[test]
fn op_completed_off_cpu_time_is_not_charged_to_the_cpu_budget() {
    init_v8();
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                await env.park.completed();
                env.park.sync(150);
                await new Promise((resolve) => setTimeout(resolve, 0));
                await new Promise((resolve) => setTimeout(resolve, 0));
                return Response.json({ completed: true });
            }
        };
    "#);
    let rt = Runtime::builder()
        .modules(modules)
        .plugin(ParkPlugin)
        .cpu_limit(Duration::from_millis(50))
        .build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
}

/// Window: `handle_op_result_pump`'s `Failed` arm, reached the same way as
/// `Completed` above but through a rejected op.
#[test]
fn op_failed_off_cpu_time_is_not_charged_to_the_cpu_budget() {
    init_v8();
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                try { await env.park.failed(); } catch (e) {}
                env.park.sync(150);
                await new Promise((resolve) => setTimeout(resolve, 0));
                await new Promise((resolve) => setTimeout(resolve, 0));
                return Response.json({ completed: true });
            }
        };
    "#);
    let rt = Runtime::builder()
        .modules(modules)
        .plugin(ParkPlugin)
        .cpu_limit(Duration::from_millis(50))
        .build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
}

/// Window: `handle_op_result_pump`'s `JsValue` arm - the shape
/// `#[v8_async_method]` and other resolver-carrying ops use.
#[test]
fn op_js_value_off_cpu_time_is_not_charged_to_the_cpu_budget() {
    init_v8();
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                await env.park.jsValue();
                env.park.sync(150);
                await new Promise((resolve) => setTimeout(resolve, 0));
                await new Promise((resolve) => setTimeout(resolve, 0));
                return Response.json({ completed: true });
            }
        };
    "#);
    let rt = Runtime::builder()
        .modules(modules)
        .plugin(ParkPlugin)
        .cpu_limit(Duration::from_millis(50))
        .build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
}

/// Rejection control for the off-CPU tests above: a request whose turns each
/// stay far below the budget, but which spend longer than the budget in CPU
/// between them, is still refused. The loop is bounded so that a request the
/// budget does not stop completes and fails this test.
#[test]
fn cpu_spent_across_turns_is_charged_to_the_cpu_budget() {
    init_v8();
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                let x = 0;
                for (let turn = 0; turn < 2000; turn++) {
                    await new Promise((resolve) => setTimeout(resolve, 0));
                    for (let i = 0; i < 200000; i++) { x = (x + 1) % 2147483647; }
                }
                return Response.json({ completed: true, x });
            }
        };
    "#);
    let rt = Runtime::builder()
        .modules(modules)
        .cpu_limit(Duration::from_millis(50))
        .build();
    let (status, body) = dispatch_against(&rt);
    assert!(
        !(200..300).contains(&status),
        "a request that spent its budget across turns returned {status}; body: {body}",
    );
    assert!(body.contains("CPU time limit exceeded"), "status {status}; body: {body}");
}
