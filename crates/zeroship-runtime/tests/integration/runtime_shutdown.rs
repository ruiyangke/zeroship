use crate::support::{commits, ends};
use futures::FutureExt;
use std::{cell::{Cell, RefCell}, rc::Rc, time::Duration};
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_runtime::state::{RequestLink, SharedState, admit_op};
use zeroship_runtime::{
    CancelFlag, EnvSnapshot, FetchOutcome, ModuleEntry, OpResult, RequestCtx, ResultReceiver,
    Runtime, SettledFetch,
    runtime::{DispatchError, InnerProbe},
};

/// What a request still pending after its dispatch is answered through.
type Reply = ResultReceiver<Result<SettledFetch, DispatchError>>;

struct NativeOperation {
    dropped: Rc<Cell<bool>>,
    started: Rc<Cell<bool>>,
    isolate: Rc<InnerProbe>,
}
impl Drop for NativeOperation {
    fn drop(&mut self) {
        assert!(
            self.isolate.strong_count() > 0,
            "native resources must be destroyed before V8"
        );
        self.dropped.set(true);
    }
}

thread_local! {
    /// The operation the next `env.probe.hang()` queues.
    static NEXT_OPERATION: RefCell<Option<NativeOperation>> = const { RefCell::new(None) };
}

/// `env.probe.hang()` queues the thread's next [`NativeOperation`] as a host
/// op that never settles, the way any binding queues one, and
/// `env.probe.panicOnEnd()` registers an end action that panics.
struct ProbePlugin;

impl NativePlugin for ProbePlugin {
    fn namespace(&self) -> &'static str {
        "probe"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("hang", hang_callback);
        r.add("panicOnEnd", panic_on_end_callback);
        r.add("keepLink", keep_link_callback);
    }
}

thread_local! {
    /// The link the latest `env.probe.keepLink()` kept.
    static KEPT_LINK: RefCell<Option<RequestLink>> = const { RefCell::new(None) };
}

/// `env.probe.keepLink()` keeps the link to its request, as a binding keeps
/// one for work that goes on after the callback, so the case can register
/// cleanup on it whenever that work would.
fn keep_link_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let owner = admit_op(scope).expect("the request is live");
    let link = owner.retain();
    KEPT_LINK.with_borrow_mut(|kept| *kept = Some(link));
}

/// The link the latest `env.probe.keepLink()` kept.
fn kept_link() -> RequestLink {
    KEPT_LINK
        .with_borrow_mut(Option::take)
        .expect("the request kept its link")
}

/// `env.probe.panicOnEnd()` registers an action on its request's early end
/// that panics, standing in for native cleanup with a defect, and queues a
/// host op that never settles, which holds the registration.
fn panic_on_end_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let owner = admit_op(scope).expect("the request is live");
    let ended = owner
        .retain()
        .on_end_or_isolate_stop(|| panic!("an action waiting on a request's end panicked"));
    state.borrow_mut().spawn_op(owner, async move {
        let _registered = ended;
        std::future::pending::<OpResult>().await
    });
}

fn hang_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let owner = admit_op(scope).expect("the hanging request is live");
    let operation = NEXT_OPERATION
        .with_borrow_mut(Option::take)
        .expect("the test supplied the operation to queue");
    state.borrow_mut().spawn_op(owner, async move {
        operation.started.set(true);
        let _operation = operation;
        std::future::pending::<OpResult>().await
    });
}

fn runtime() -> Runtime {
    zeroship_runtime::init_v8();
    Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: "export default { async fetch(request, env) { \
                     const path = new URL(request.url).pathname; \
                     if (path === '/hang') env.probe.hang(); \
                     if (path === '/commit') env.commits.hold(); \
                     if (path === '/panic-on-end') env.probe.panicOnEnd(); \
                     if (path === '/hold-end') { env.ends.hold(); return new Response('held'); } \
                     if (path === '/hold-end-pending') env.ends.hold(); \
                     if (path === '/keep-link') { env.probe.keepLink(); return new Response('kept'); } \
                     if (path === '/keep-link-pending') env.probe.keepLink(); \
                     return await new Promise(() => {}); } };"
                .into(),
        }])
        .plugin(ProbePlugin)
        .plugin(commits::Probe)
        .plugin(ends::Probe)
        .build()
}

#[compio::test]
async fn host_interrupt_is_permanent_and_safe_after_isolate_disposal() {
    let runtime = runtime();
    runtime.initialize(&EnvSnapshot::empty()).await.unwrap();
    let interrupt = runtime.interrupt_handle();
    let remote = interrupt.clone();
    std::thread::spawn(move || remote.cancel()).join().unwrap();
    assert!(
        runtime
            .initialize(&EnvSnapshot::empty())
            .await
            .unwrap_err()
            .contains("interrupted")
    );
    for _ in 0..2 {
        let FetchOutcome::Response { status, body, .. } = runtime.call_fetch_handler(
            "GET",
            "http://local/",
            &[],
            [],
            &EnvSnapshot::empty(),
            RequestCtx::new(CancelFlag::new()),
        ) else {
            panic!("interrupted runtime must reject dispatch synchronously");
        };
        assert_eq!(status, 500);
        assert!(String::from_utf8(body).unwrap().contains("interrupted"));
    }
    runtime.exit_isolate();
    runtime.shutdown().await;
    drop(runtime);
    interrupt.cancel();
}

/// A host interrupt raised on another thread while the pump waits on a host op
/// that never settles answers the requests still pending with the
/// interruption's error and stops the isolate. Nothing else could end that
/// wait: no JavaScript is running for the interrupt to terminate, and no
/// operation will settle to wake the pump.
#[compio::test]
async fn a_host_interrupt_from_another_thread_answers_requests_pending_on_an_idle_pump() {
    let runtime = runtime();
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://local/",
        &[],
        [],
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("request must be pending");
    };
    runtime.exit_isolate();
    runtime.start_pump();
    let (dropped, _, hanging) = native_operation(&runtime).await;
    let interrupt = runtime.interrupt_handle();
    std::thread::spawn(move || interrupt.cancel()).join().unwrap();
    let answers = compio::time::timeout(Duration::from_secs(5), async {
        (rx.recv().await, hanging.recv().await)
    })
    .await;
    let stopped = runtime.is_quarantined();
    // Joined before any assertion, so a failing case does not drop the isolate
    // under the host op it still holds.
    compio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .expect("an interrupted isolate's native work must join");
    let (waiting, holding) = answers.expect("the interrupt must answer both pending requests");
    for (request, answer) in [("awaiting a promise", waiting), ("holding the host op", holding)] {
        let Err(error) = answer else {
            panic!("the request {request} must not settle as a response");
        };
        assert_eq!(error.message, "runtime execution interrupted", "{request}");
        assert_eq!(error.code, None, "the request {request} has no commit in flight");
    }
    assert!(stopped, "the interrupt stops the isolate before its host does");
    assert!(dropped.get(), "the host op the isolate waited on is destroyed");
}

/// An action waiting on a request's early end that panics does not keep the
/// stop that ends the request from answering every pending request and
/// quarantining the isolate. The control is
/// `a_host_interrupt_from_another_thread_answers_requests_pending_on_an_idle_pump`,
/// whose requests register no such action.
#[compio::test]
async fn a_panicking_end_action_does_not_keep_a_stop_from_answering_and_quarantining() {
    let runtime = runtime();
    let dispatch = |path: &str| {
        let outcome = runtime.call_fetch_handler(
            "GET",
            &format!("http://local{path}"),
            &[],
            [],
            &EnvSnapshot::empty(),
            RequestCtx::new(CancelFlag::new()),
        );
        let FetchOutcome::Pending { rx, .. } = outcome else {
            panic!("{path} must be pending");
        };
        rx
    };
    let waiting = dispatch("/");
    let panicking = dispatch("/panic-on-end");
    runtime.exit_isolate();
    runtime.start_pump();
    let interrupt = runtime.interrupt_handle();
    std::thread::spawn(move || interrupt.cancel()).join().unwrap();
    let answers = compio::time::timeout(Duration::from_secs(5), async {
        (waiting.recv().await, panicking.recv().await)
    })
    .await;
    let stopped = runtime.is_quarantined();
    compio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .expect("an interrupted isolate's native work must join");
    let (waiting, panicking) =
        answers.expect("the stop must answer both pending requests despite the panicking action");
    for (request, answer) in [("awaiting a promise", waiting), ("with the action", panicking)] {
        let Err(error) = answer else {
            panic!("the request {request} must not settle as a response");
        };
        assert_eq!(error.message, "runtime execution interrupted", "{request}");
    }
    assert!(stopped, "the stop quarantines the isolate despite the panicking action");
}

/// A host interrupt raised on another thread, while the pump waits with a
/// request's commit in flight, answers that request that whether the commit
/// applied is unknown, and ends the request, so what waits on its end runs.
/// The control is the case above, whose requests had no commit in flight and
/// are answered with the plain interruption.
#[compio::test]
async fn a_host_interrupt_answers_a_request_with_a_commit_in_flight_as_indeterminate() {
    let runtime = runtime();
    runtime.exit_isolate();
    runtime.start_pump();
    let commit = commits::watch();
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://local/commit",
        &[],
        [],
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the committing request must be pending");
    };
    runtime.notify_pump();
    // Once the pump has polled the commit, it has nothing left to do and
    // waits, so only the interrupt's own wake can reach it.
    compio::time::timeout(Duration::from_secs(5), async {
        while !commit.polled() {
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the commit must enter the real pump");
    let interrupt = runtime.interrupt_handle();
    std::thread::spawn(move || interrupt.cancel()).join().unwrap();
    let answer = compio::time::timeout(Duration::from_secs(5), rx.recv()).await;
    compio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .expect("an interrupted isolate's native work must join");
    let Err(error) = answer.expect("the interrupt must answer the committing request") else {
        panic!("an interrupted request must not settle as a response");
    };
    commit.assert_ended_as_indeterminate(&error, "runtime execution interrupted");
}

/// A host interrupt raised on another thread while startup waits on a
/// top-level await that never settles releases the initialization waiting on
/// it with the interruption's error.
#[compio::test]
async fn a_host_interrupt_from_another_thread_releases_a_waiting_initialization() {
    zeroship_runtime::init_v8();
    let runtime = Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: "await new Promise(() => {}); export default { fetch() { throw new Error('interrupted handler ran'); } };".into(),
        }])
        .build();
    runtime.exit_isolate();
    let initializing_runtime = runtime.clone();
    let (started, started_rx) = futures::channel::oneshot::channel();
    let initialization = compio::runtime::spawn(async move {
        let env = EnvSnapshot::empty();
        let initialize = initializing_runtime.initialize(&env);
        futures::pin_mut!(initialize);
        assert!(futures::poll!(&mut initialize).is_pending());
        started.send(()).unwrap();
        initialize.await
    });
    compio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .expect("initialization must register its readiness waiter")
        .unwrap();
    let interrupt = runtime.interrupt_handle();
    std::thread::spawn(move || interrupt.cancel()).join().unwrap();
    let error = compio::time::timeout(Duration::from_secs(5), initialization)
        .await
        .expect("the interrupt must release the waiting initialization")
        .unwrap()
        .unwrap_err();
    assert!(error.contains("runtime execution interrupted"), "{error}");
    compio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .expect("an interrupted isolate's native work must join");
}

#[compio::test]
async fn quarantine_releases_pending_initialization_and_startup_requests() {
    zeroship_runtime::init_v8();
    let runtime = Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: "await new Promise(() => {}); export default { fetch() { throw new Error('quarantined handler ran'); } };".into(),
        }])
        .build();
    let FetchOutcome::Pending { rx, .. } = runtime.call_fetch_handler(
        "GET",
        "http://local/",
        &[],
        [],
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    ) else {
        panic!("unresolved startup must queue the request");
    };
    runtime.exit_isolate();
    let initializing_runtime = runtime.clone();
    let (started, started_rx) = futures::channel::oneshot::channel();
    let initialization = compio::runtime::spawn(async move {
        let env = EnvSnapshot::empty();
        let initialize = initializing_runtime.initialize(&env);
        futures::pin_mut!(initialize);
        assert!(futures::poll!(&mut initialize).is_pending());
        started.send(()).unwrap();
        initialize.await
    });
    compio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .expect("initialization must register its readiness waiter")
        .unwrap();
    runtime.quarantine();
    compio::time::timeout(Duration::from_secs(5), async {
        runtime.shutdown().await;
        let error = initialization.await.unwrap().unwrap_err();
        assert!(error.contains("quarantined"), "{error}");
        let SettledFetch::Response { status, body, .. } = rx.recv().await.unwrap() else {
            panic!("quarantine must release the queued startup request");
        };
        assert_eq!(status, 500);
        assert!(String::from_utf8(body).unwrap().contains("quarantined"));
    })
    .await
    .expect("quarantine must wake readiness waiters and finish native teardown");
}

/// Queue a host op that never settles, through a request whose handler calls
/// `env.probe.hang()`, and wait for the pump to start it. Answers whether the
/// op is destroyed, the isolate's strong-count probe, and the request's reply.
async fn native_operation(runtime: &Runtime) -> (Rc<Cell<bool>>, Rc<InnerProbe>, Reply) {
    let started = Rc::new(Cell::new(false));
    let dropped = Rc::new(Cell::new(false));
    let isolate = Rc::new(runtime.clone().into_inner_probe_for_test());
    NEXT_OPERATION.with_borrow_mut(|next| {
        *next = Some(NativeOperation {
            dropped: dropped.clone(),
            started: started.clone(),
            isolate: isolate.clone(),
        });
    });
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://local/hang",
        &[],
        [],
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the hanging request must be pending");
    };
    assert!(
        NEXT_OPERATION.with_borrow(Option::is_none),
        "the handler must have queued the operation"
    );
    runtime.notify_pump();
    compio::time::timeout(Duration::from_secs(5), async {
        while !started.get() {
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("native operation must enter the real pump");
    (dropped, isolate, rx)
}

#[compio::test]
async fn shutdown_joins_native_operations_and_prevents_further_dispatch() {
    let runtime = runtime();
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://local/",
        &[],
        [],
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("request must be pending");
    };
    runtime.exit_isolate();
    runtime.start_pump();
    let (dropped, _, _) = native_operation(&runtime).await;
    // Abandon the first wait, then resume it: cancellation must not lose the
    // native teardown barrier or reopen the isolate.
    let _ = runtime.shutdown().now_or_never();
    compio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .unwrap();
    assert!(dropped.get());
    assert!(rx.recv().await.is_err());
    runtime.enter_isolate();
    let blocked = runtime.call_fetch_handler(
        "GET",
        "http://local/",
        &[],
        [],
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    let FetchOutcome::Response { status, body, .. } = blocked else {
        panic!("quarantined dispatch must fail synchronously");
    };
    assert_eq!(status, 500);
    assert!(String::from_utf8(body).unwrap().contains("quarantined"));
}

#[compio::test]
async fn quarantine_retains_v8_until_native_destructors_have_finished() {
    let runtime = runtime();
    runtime.exit_isolate();
    runtime.start_pump();
    let (dropped, isolate, _) = native_operation(&runtime).await;
    runtime.quarantine();
    drop(runtime);
    compio::time::timeout(Duration::from_secs(5), async {
        while isolate.strong_count() != 0 {
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("quarantined isolate must be disposed after native teardown");
    assert!(dropped.get());
}

/// Dispatch `path` on `runtime`, entered, and hand back what it answered.
fn dispatched(runtime: &Runtime, path: &str) -> FetchOutcome {
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "GET",
        &format!("http://local{path}"),
        &[],
        [],
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    outcome
}

/// Register an isolate end through a request that is answered at once, park
/// another request, interrupt the isolate from another thread, and answer how
/// many times the end ran once the stop has answered the parked request.
/// `withdraw` withdraws the end before the interrupt, as work that finishes
/// on its own does.
async fn isolate_end_runs_at_an_interrupt(withdraw: bool) -> u32 {
    let runtime = runtime();
    runtime.exit_isolate();
    runtime.start_pump();
    let end = ends::watch();
    let FetchOutcome::Response { status, .. } = dispatched(&runtime, "/hold-end") else {
        panic!("the request holding the end is answered at once");
    };
    assert_eq!(status, 200);
    assert!(end.held(), "the answered request registered its end");
    if withdraw {
        end.withdraw();
    }
    let FetchOutcome::Pending { rx: parked, .. } = dispatched(&runtime, "/") else {
        panic!("the parked request is pending");
    };
    let interrupt = runtime.interrupt_handle();
    std::thread::spawn(move || interrupt.cancel()).join().unwrap();
    let answer = compio::time::timeout(Duration::from_secs(5), parked.recv()).await;
    compio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .expect("an interrupted isolate's native work must join");
    assert!(
        answer.expect("the stop answers the parked request").is_err(),
        "the parked request is ended by the stop"
    );
    end.runs()
}

/// Work a request left running past its answer is ended when its isolate
/// stops: its end action runs once at the stop, though the request that
/// registered it already has its answer.
#[compio::test]
async fn an_isolate_end_registered_by_an_answered_request_runs_at_the_stop() {
    assert_eq!(isolate_end_runs_at_an_interrupt(false).await, 1);
}

/// The control beside it: work that finished on its own withdrew its end, so
/// the stop does not run it.
#[compio::test]
async fn a_withdrawn_isolate_end_does_not_run_at_the_stop() {
    assert_eq!(isolate_end_runs_at_an_interrupt(true).await, 0);
}

/// An end registered by a request still pending runs once when the isolate
/// stops: the stop ends the request, which runs it, and the isolate's own end
/// does not run it again.
#[compio::test]
async fn an_isolate_end_of_a_pending_request_runs_once_at_the_stop() {
    let runtime = runtime();
    runtime.exit_isolate();
    runtime.start_pump();
    let end = ends::watch();
    let FetchOutcome::Pending { rx: holding, .. } = dispatched(&runtime, "/hold-end-pending") else {
        panic!("the request holding the end is pending");
    };
    assert!(end.held(), "the pending request registered its end");
    let interrupt = runtime.interrupt_handle();
    std::thread::spawn(move || interrupt.cancel()).join().unwrap();
    let answer = compio::time::timeout(Duration::from_secs(5), holding.recv()).await;
    compio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .expect("an interrupted isolate's native work must join");
    assert!(answer.expect("the stop answers the request").is_err());
    assert_eq!(end.runs(), 1, "the end runs once, not once per end that reaches it");
}

/// An isolate dropped without a stop, as a cache drops the one it evicts,
/// ends the work a request left running past its answer too.
#[compio::test]
async fn an_isolate_end_runs_when_the_isolate_is_dropped_without_a_stop() {
    let runtime = runtime();
    runtime.exit_isolate();
    let end = ends::watch();
    let FetchOutcome::Response { status, .. } = dispatched(&runtime, "/hold-end") else {
        panic!("the request holding the end is answered at once");
    };
    assert_eq!(status, 200);
    assert!(!runtime.is_quarantined(), "nothing stopped the isolate");
    assert_eq!(end.runs(), 0, "nothing has ended the work yet");
    drop(runtime);
    assert_eq!(end.runs(), 1, "dropping the isolate ends what it owned");
}


/// Work still going when its isolate has ended, as an op in flight when a
/// cache drops the isolate it evicts, registers its cleanup afterwards, as a
/// transaction whose BEGIN answers then does. The cleanup runs at once: the
/// isolate that would have ended the work is gone. The control is the same
/// registration made while the isolate is live, which waits.
#[compio::test]
async fn an_end_registered_after_its_isolate_has_ended_runs_at_once() {
    let runtime = runtime();
    runtime.exit_isolate();
    let FetchOutcome::Response { status, .. } = dispatched(&runtime, "/keep-link") else {
        panic!("the request keeping its link is answered at once");
    };
    assert_eq!(status, 200);
    let link = kept_link();
    let runs = Rc::new(Cell::new(0));
    let counter = Rc::clone(&runs);
    let live = link.on_end_or_isolate_stop(move || counter.set(counter.get() + 1));
    assert_eq!(runs.get(), 0, "a registration on a live isolate waits");
    drop(live);
    drop(runtime);
    let counter = Rc::clone(&runs);
    let _late = link.on_end_or_isolate_stop(move || counter.set(counter.get() + 1));
    assert_eq!(runs.get(), 1, "a registration on an ended isolate runs at once");
}

/// Cleanup registered for a request that has already ended runs at once, and
/// a panic in it is contained: whoever registers it carries on.
#[compio::test]
async fn an_end_registered_for_an_ended_request_runs_at_once_and_contains_a_panic() {
    let runtime = runtime();
    runtime.exit_isolate();
    let cancel = CancelFlag::new();
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://local/keep-link-pending",
        &[],
        [],
        &EnvSnapshot::empty(),
        RequestCtx::new(cancel.clone()),
    );
    runtime.exit_isolate();
    let FetchOutcome::Pending { .. } = outcome else {
        panic!("the request keeping its link is pending");
    };
    let link = kept_link();
    cancel.cancel();
    assert!(link.has_ended(), "the host cancelled the request");
    let ran = Rc::new(Cell::new(false));
    let marker = Rc::clone(&ran);
    let _ended = link.on_end_or_isolate_stop(move || {
        marker.set(true);
        panic!("cleanup for an ended request panicked");
    });
    assert!(ran.get(), "cleanup for an ended request runs at once, and its panic stays inside it");
    compio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .expect("the isolate's native work must join");
}
