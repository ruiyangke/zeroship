//! A panic in the isolate's event pump stops the isolate.
//!
//! `panic_util::guard` catches and logs a panic in the pump task, but the pump
//! is the only thing that settles the isolate's pending requests and ends its
//! native work. Left alone, the isolate is a zombie a caching host keeps
//! dispatching to: its requests never settle, its transactions and downloads
//! never end, and its end actions never run. These cases make the pump loop
//! itself panic - through the runtime's test-only hook, so the panic happens
//! outside the native-callback containment - and require the stop every other
//! isolate stop takes: a pending request is answered with the pump panic's
//! error, an isolate end action runs, a commit in flight reports indeterminate,
//! and the runtime reports quarantined.

use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::DispatchError;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, ResultReceiver, Runtime,
    SettledFetch,
};

use crate::support::{commits, ends};

/// How long a case waits for the answer the stop owes it. Far beyond any
/// scheduling delay, so expiring it means the request was stranded, not that
/// the stop was slow.
const SETTLE_WITHIN: Duration = Duration::from_secs(5);

/// What the pump panic's stop answers a pending request with. The pump is
/// platform code, so the answer names no creator code.
const PUMP_PANIC_CAUSE: &str = "runtime failed";

/// Routes: `/hold-end` registers cleanup on its isolate's stop (`ends`) and
/// answers; `/commit` holds a commit whose outcome it never learns (`commits`)
/// and waits; anything else waits on a promise nothing resolves.
const SOURCE: &str = r#"
    export default {
        fetch(request, env) {
            const path = new URL(request.url).pathname;
            if (path === "/hold-end") {
                env.ends.hold();
                return new Response("held");
            }
            if (path === "/commit") {
                env.commits.hold();
                return new Promise(() => {});
            }
            return new Promise(() => {});
        },
    };
"#;

/// A runtime whose event pump panics at the top of its first loop iteration.
///
/// The isolate is exited after build, as the worker keeps it between turns
/// (`zeroship-worker::cache`), so dispatch re-enters it for each call.
fn panicking_pump() -> Runtime {
    init_v8();
    let runtime = Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: SOURCE.into(),
        }])
        .plugin(ends::Probe)
        .plugin(commits::Probe)
        .idle_gc_after_ms(0)
        .panic_pump_for_test()
        .build();
    runtime.exit_isolate();
    runtime
}

/// Dispatch `path` with the isolate entered for the call and exited after it,
/// the way the pump tests drive dispatch.
fn dispatch(runtime: &Runtime, path: &str) -> FetchOutcome {
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

/// Leave the isolate entered, modelling a panic between the pump's own
/// `enter_isolate` and `exit_isolate`. The stop must not enter or exit V8: it
/// answers the requests and runs the Rust end actions, and the isolate is
/// disposed later by the depth its drop already records.
fn leave_isolate_entered(runtime: &Runtime) {
    runtime.enter_isolate();
}

/// A request pending on the isolate is answered with the pump panic's error,
/// the isolate end actions run, and the runtime reports quarantined.
#[test]
fn a_pump_panic_answers_a_pending_request_and_runs_the_isolate_end() {
    let runtime = panicking_pump();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let end = ends::watch();
        let FetchOutcome::Response { status, .. } = dispatch(&runtime, "/hold-end") else {
            panic!("/hold-end answers at once");
        };
        assert_eq!(status, 200);
        assert!(end.held(), "the answered request registered its isolate end");

        let parked: ResultReceiver<Result<SettledFetch, DispatchError>> =
            match dispatch(&runtime, "/") {
                FetchOutcome::Pending { rx, .. } => rx,
                _ => panic!("the parked request is pending on the pump"),
            };
        leave_isolate_entered(&runtime);
        runtime.start_pump();

        let answer = compio::time::timeout(SETTLE_WITHIN, parked.recv())
            .await
            .expect("the pump panic stranded the parked request");
        let Err(error) = answer else {
            panic!("the parked request must be answered with the pump panic's error");
        };
        assert_eq!(error.message, PUMP_PANIC_CAUSE);
        assert_eq!(error.code, None, "the parked request has no commit in flight");
        assert!(
            runtime.is_quarantined(),
            "the pump panic quarantines the isolate"
        );
        assert_eq!(
            end.runs(),
            1,
            "the stop runs the isolate end the answered request left"
        );
        assert!(
            !runtime.has_pending_requests_for_test(),
            "the stop leaves no request waiting on the isolate"
        );

        compio::time::timeout(SETTLE_WITHIN, runtime.shutdown())
            .await
            .expect("the stopped isolate's native work must join");
    });
}

/// A request with a commit in flight when the pump panics is answered that
/// whether the commit applied is unknown, not with the plain pump panic error.
#[test]
fn a_commit_in_flight_when_the_pump_panics_is_answered_as_indeterminate() {
    let runtime = panicking_pump();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let commit = commits::watch();
        let committing: ResultReceiver<Result<SettledFetch, DispatchError>> =
            match dispatch(&runtime, "/commit") {
                FetchOutcome::Pending { rx, .. } => rx,
                _ => panic!("the committing request is pending on the pump"),
            };
        assert!(commit.held(), "the committing request holds its commit");
        leave_isolate_entered(&runtime);
        runtime.start_pump();

        let answer = compio::time::timeout(SETTLE_WITHIN, committing.recv())
            .await
            .expect("the pump panic stranded the committing request");
        let Err(error) = answer else {
            panic!("the committing request must be ended by the pump panic's stop");
        };
        commit.assert_ended_as_indeterminate(&error, PUMP_PANIC_CAUSE);
        assert!(
            runtime.is_quarantined(),
            "the pump panic quarantines the isolate"
        );
        assert!(
            !runtime.has_pending_requests_for_test(),
            "the stop leaves no request waiting on the isolate"
        );

        compio::time::timeout(SETTLE_WITHIN, runtime.shutdown())
            .await
            .expect("the stopped isolate's native work must join");
    });
}
