use futures::FutureExt;
use std::{cell::{Cell, RefCell}, rc::Rc, time::Duration};
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_runtime::state::{SharedState, admit_op};
use zeroship_runtime::{
    CancelFlag, EnvSnapshot, FetchOutcome, ModuleEntry, OpResult, RequestCtx, Runtime,
    SettledFetch, runtime::InnerProbe,
};

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
/// op that never settles, the way any binding queues one.
struct ProbePlugin;

impl NativePlugin for ProbePlugin {
    fn namespace(&self) -> &'static str {
        "probe"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("hang", hang_callback);
    }
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
                     if (new URL(request.url).pathname === '/hang') env.probe.hang(); \
                     return await new Promise(() => {}); } };"
                .into(),
        }])
        .plugin(ProbePlugin)
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
/// `env.probe.hang()`, and wait for the pump to start it.
async fn native_operation(runtime: &Runtime) -> (Rc<Cell<bool>>, Rc<InnerProbe>) {
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
    assert!(
        matches!(outcome, FetchOutcome::Pending { .. }),
        "the hanging request must be pending"
    );
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
    (dropped, isolate)
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
    let (dropped, _) = native_operation(&runtime).await;
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
    let (dropped, isolate) = native_operation(&runtime).await;
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
