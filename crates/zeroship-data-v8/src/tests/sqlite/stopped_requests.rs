//! A request the runtime stops for CPU makes no more database calls.
//!
//! The runtime answers a request that ran out of CPU and keeps the isolate
//! serving the app's other requests. Code the stopped request had already
//! scheduled still runs on later pump turns, so every host op it makes from
//! then on has to be refused: otherwise a request answered with the CPU error
//! goes on to commit a transaction.
//!
//! The refusal carries the code `request_cancelled`, on a raw collection call,
//! on an SDK collection call and on a transaction result alike.
//!
//! Most scenarios park the stopped request's next step on `env.latch.wait()`,
//! which the test releases only once the stopped request has been answered, so
//! that step runs after the stop on every schedule. A sibling request of the
//! same app is in flight the whole time: it waits for the stopped request's
//! outcome, then commits a transaction of its own and reports what the table
//! holds.

use super::fixtures::*;
use crate::tests::fixtures::parity;
use crate::tests::fixtures::recording::{self, Ending};
use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroship_data_orm::error::SettleIntent;
use zeroship_data_orm::value;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_runtime::runtime::DispatchError;
use zeroship_runtime::state::{
    OpResult, ResolveValue, SharedState, admit_op, rejected_promise,
};
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch};

thread_local! {
    /// The release of the one `env.latch.wait()` in flight.
    static LATCH: RefCell<Option<futures::channel::oneshot::Sender<()>>> =
        const { RefCell::new(None) };
    /// Told when the next `env.latch.wait()` parks.
    static PARKED: RefCell<Option<futures::channel::oneshot::Sender<()>>> =
        const { RefCell::new(None) };
}

/// `env.latch.wait()` returns a promise the test settles with
/// [`release_latch`]. It is a host op like any other, admitted for the request
/// that makes it.
struct LatchPlugin;

impl NativePlugin for LatchPlugin {
    fn namespace(&self) -> &'static str {
        "latch"
    }

    fn register(&self, registrar: &mut NativeRegistrar) {
        registrar.add("wait", latch_wait);
    }
}

fn latch_wait(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let owner = match admit_op(scope) {
        Ok(owner) => owner,
        Err(error) => return rv.set(rejected_promise(scope, &error).into()),
    };
    let resolver = v8::PromiseResolver::new(scope).expect("promise resolver");
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);
    let (release, released) = futures::channel::oneshot::channel();
    LATCH.with_borrow_mut(|latch| {
        assert!(latch.replace(release).is_none(), "one latch at a time");
    });
    if let Some(parked) = PARKED.with_borrow_mut(Option::take) {
        let _ = parked.send(());
    }
    state.borrow_mut().spawn_op(owner, async move {
        released.await.expect("the test releases the latch");
        OpResult::JsValue {
            resolver,
            value: ResolveValue::Undefined,
        }
    });
    rv.set(promise.into());
}

fn release_latch() {
    LATCH
        .with_borrow_mut(Option::take)
        .expect("the stopped request is waiting on the latch")
        .send(())
        .expect("the latch's op is still queued");
}

/// Fires when the next `env.latch.wait()` parks.
fn latch_parked() -> futures::channel::oneshot::Receiver<()> {
    let (parked, on_parked) = futures::channel::oneshot::channel();
    PARKED.with_borrow_mut(|slot| {
        assert!(slot.replace(parked).is_none(), "one park watched at a time");
    });
    on_parked
}

/// The source every scenario shares: the sibling, the stopped request's
/// report, a bounded CPU loop, then the scenario's own module-level `prelude`
/// and its `procedures`.
fn source(prelude: &str, procedures: &str) -> SqliteRuntimeSource {
    sqlite_runtime_source(
        "notes",
        &value!({"title":{"type":"string", "required":true}}),
        &format!(
            r#"
let reportStopped;
const stoppedOutcome = new Promise((resolve) => {{ reportStopped = resolve; }});

function burn(iterations) {{
  let x = 0;
  for (let i = 0; i < iterations; i++) {{ x = (x + 1) % 2147483647; }}
  return x;
}}

// `env.db.transaction` answers with `{{ data, error }}` rather than throwing.
function settled(result) {{
  return result.error ? (result.error.code ?? String(result.error)) : "committed";
}}

// A collection read through the SDK, which answers with `{{ data, error }}`.
function read(result) {{
  return result.error ? (result.error.code ?? String(result.error)) : "read";
}}

{prelude}

const _procedures = {{
  async sibling() {{
    const stopped = await stoppedOutcome;
    const transaction = settled(await env.db.transaction(async (tx) => {{
      await tx.collection(COLLECTION).insert({{ title: "sibling" }});
    }}));
    const rows = await env.db.collection(COLLECTION).find({{}});
    return {{ stopped, transaction, titles: rows.map((row) => row.title).sort() }};
  }},
  {procedures}
}};
"#
        ),
    )
}

/// `source`'s module, with its default export carrying a `fetch` handler
/// beside `rpc`: both dispatch the same `_procedures` object, so a test can
/// run a scenario through the plain-fetch tier instead of
/// `/__zeroship/v1/<id>`. `fetch`'s path is the procedure name
/// (`http://localhost/stopped` calls `_procedures.stopped()`), the same
/// no-argument shape every scenario's procedures already use.
fn source_with_fetch(procedures: &str) -> SqliteRuntimeSource {
    const FETCH_AND_RPC_SHIM: &str = r#"export default {
  rpc: _procedures,
  async fetch(request) {
    const name = new URL(request.url).pathname.slice(1);
    const result = await _procedures[name]();
    return new Response(JSON.stringify({ json: result }), {
      headers: { "content-type": "application/json" },
    });
  },
};"#;
    let rpc_only = source("", procedures);
    let module = rpc_only
        .source
        .strip_suffix(SQLITE_RUNTIME_RPC_SHIM)
        .expect("source() ends with the shared RPC-only shim");
    SqliteRuntimeSource {
        source: format!("{module}{FETCH_AND_RPC_SHIM}"),
        descriptor: rpc_only.descriptor,
    }
}

/// One app, served from a fresh database, with the latch plugin and a CPU
/// limit.
struct App {
    runtime: Runtime,
    directory: tempfile::TempDir,
    alias: String,
}

impl App {
    async fn new(procedures: &str, cpu_limit: Duration) -> Self {
        Self::with_prelude("", procedures, cpu_limit).await
    }

    async fn with_prelude(prelude: &str, procedures: &str, cpu_limit: Duration) -> Self {
        Self::build(source(prelude, procedures), cpu_limit).await
    }

    /// Like [`new`](Self::new), but the module's default export also carries
    /// a plain `fetch` handler beside `rpc`, dispatching the same procedures
    /// by URL path. Lets a test run one procedure through the non-RPC tier.
    async fn with_fetch(procedures: &str, cpu_limit: Duration) -> Self {
        Self::build(source_with_fetch(procedures), cpu_limit).await
    }

    async fn build(source: SqliteRuntimeSource, cpu_limit: Duration) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let alias = crate::tests::fixtures::harness_alias(LOCAL_DEV_APP_ID);
        apply_schema_ahead_of_runtime(
            &directory,
            &format!("CREATE TABLE \"{alias}\".notes ({SYSTEM_COLUMNS_SQLITE}, title TEXT NOT NULL);"),
        );
        let db = crate::service::DbService::new(crate::service::DbServiceConfig {
            app_bindings: crate::tests::fixtures::harness_app_bindings([LOCAL_DEV_APP_ID]),
            project_keys: Arc::default(),
            connection: recording::connection(&parity::sqlite_url(&directory)),
            cdc_relay: None,
            meter: None,
        })
        .unwrap();
        zeroship_runtime::init_v8();
        let plugins: Vec<Arc<dyn NativePlugin>> = vec![db.plugin(), Arc::new(LatchPlugin)];
        let runtime = Runtime::builder()
            .modules(vec![ModuleEntry {
                specifier: "index.js".into(),
                source: source.source,
            }])
            .plugins(plugins)
            .runtime_descriptor(Some(crate::tests::fixtures::harness_descriptor_document(
                LOCAL_DEV_APP_ID,
                &source.descriptor,
            )))
            .cpu_limit(cpu_limit)
            .build();
        runtime.initialize(&EnvSnapshot::empty()).await.unwrap();
        runtime.start_pump();
        Self {
            runtime,
            directory,
            alias,
        }
    }

    /// Call procedure `name`, which must still be running when the call
    /// returns.
    fn call(&self, name: &str) -> zeroship_runtime::channel::ResultReceiver<Result<SettledFetch, DispatchError>> {
        self.call_with(name, CancelFlag::new())
    }

    fn call_with(
        &self,
        name: &str,
        cancel: CancelFlag,
    ) -> zeroship_runtime::channel::ResultReceiver<Result<SettledFetch, DispatchError>> {
        let outcome = self.runtime.call_fetch_handler(
            "POST",
            &format!("http://localhost/__zeroship/v1/{name}"),
            &[("content-type".into(), "application/json".into())],
            r#"{"json":null}"#,
            &EnvSnapshot::empty(),
            RequestCtx::new(cancel),
        );
        let FetchOutcome::Pending { rx, .. } = outcome else {
            panic!("{name} is still running when its call returns");
        };
        rx
    }

    /// Call procedure `name` through the plain-fetch tier (a module built by
    /// [`source_with_fetch`]), not `/__zeroship/v1/<id>`: the request this
    /// produces carries no RPC lifetime, so the runtime answers its early end
    /// through `cleanup_cancelled_requests`'s non-RPC branch.
    fn call_plain_fetch(
        &self,
        name: &str,
    ) -> zeroship_runtime::channel::ResultReceiver<Result<SettledFetch, DispatchError>> {
        let outcome = self.runtime.call_fetch_handler(
            "GET",
            &format!("http://localhost/{name}"),
            &[],
            "",
            &EnvSnapshot::empty(),
            RequestCtx::new(CancelFlag::new()),
        );
        let FetchOutcome::Pending { rx, .. } = outcome else {
            panic!("{name} is still running when its call returns");
        };
        rx
    }

    /// Every title the table holds, read directly from the database file.
    async fn titles(&self) -> Vec<String> {
        crate::tests::fixtures::sqlite::Inspector::open(self.directory.path())
            .query(&format!("SELECT title FROM \"{}\".notes ORDER BY title", self.alias), &[])
            .await
            .expect("read the table")
            .into_iter()
            .map(|row| row[0].clone().expect("a title is never null"))
            .collect()
    }
}

async fn answer(
    rx: zeroship_runtime::channel::ResultReceiver<Result<SettledFetch, DispatchError>>,
    what: &str,
) -> Result<SettledFetch, DispatchError> {
    compio::time::timeout(Duration::from_secs(30), rx.recv())
        .await
        .unwrap_or_else(|_| panic!("{what} is answered"))
}

/// A procedure's own answer: its status and its JSON body.
fn completed(answer: Result<SettledFetch, DispatchError>) -> (u16, serde_json::Value) {
    match answer.expect("the procedure completes") {
        SettledFetch::Response { status, body, .. } => (
            status,
            serde_json::from_slice(&body).unwrap_or_else(|_| {
                serde_json::Value::String(String::from_utf8_lossy(&body).into_owned())
            }),
        ),
        _ => panic!("the procedure answers with a buffered response"),
    }
}

/// What one scenario left behind.
struct Stopped {
    /// The stopped request's own answer.
    answer: Result<SettledFetch, DispatchError>,
    /// The sibling's status and body.
    sibling: (u16, serde_json::Value),
    /// Every title the table holds once both requests are done.
    titles: Vec<String>,
}

/// Run the sibling and the stopped request together under `cpu_limit`,
/// release the latch once the stopped request is answered, and collect what
/// both left behind.
fn run_stopped(procedure: &str, cpu_limit: Duration) -> Stopped {
    run(async {
        let app = App::new(&format!("stopped: {procedure},"), cpu_limit).await;
        let sibling = app.call("sibling");
        let stopped = app.call("stopped");
        let answer = answer(stopped, "the stopped request").await;
        release_latch();
        let sibling = completed(answer_of(sibling).await);
        let titles = app.titles().await;
        Stopped {
            answer,
            sibling,
            titles,
        }
    })
}

async fn answer_of(
    rx: zeroship_runtime::channel::ResultReceiver<Result<SettledFetch, DispatchError>>,
) -> Result<SettledFetch, DispatchError> {
    answer(rx, "the sibling").await
}

/// The witness that the CPU limit is what ended the stopped request, so the
/// assertions after it are about a stopped request and not a finished one.
fn assert_stopped_for_cpu(answer: &Result<SettledFetch, DispatchError>) {
    match answer {
        Err(error) => assert!(
            error.message.contains("CPU time limit exceeded"),
            "the stopped request must be answered with the CPU error: {error:?}"
        ),
        Ok(_) => panic!("the stopped request must be answered with the CPU error, not a response"),
    }
}

/// Stopped by its accumulated CPU (`check_cpu_limit`) while an `await` is
/// pending. The continuation that runs after the stop starts a transaction and
/// inserts into it: the transaction is refused before it begins, so its
/// callback never runs, and nothing is committed.
const STOPPED_ACROSS_TURNS: &str = r#"async function () {
    env.latch.wait().then(async () => {
      let entered = false;
      const transaction = settled(await env.db.transaction(async (tx) => {
        entered = true;
        await tx.collection(COLLECTION).insert({ title: "after-stop" });
      }));
      reportStopped({ entered, transaction });
    });
    // Many turns, each far below the limit, so the limit trips on the total.
    let x = 0;
    for (let turn = 0; turn < 2000; turn++) {
      await env.db.collection(COLLECTION).find({});
      x += burn(200000);
    }
    return { survived: true, x };
  }"#;

#[test]
fn a_request_stopped_for_cpu_cannot_start_a_transaction_afterwards() {
    let stopped = run_stopped(STOPPED_ACROSS_TURNS, Duration::from_millis(50));
    assert_stopped_for_cpu(&stopped.answer);
    assert_eq!(
        stopped.sibling.1["json"]["stopped"],
        serde_json::json!({"entered": false, "transaction": "request_cancelled"}),
        "the transaction the stopped request started after the stop must be refused \
         before its callback runs: {:?}",
        stopped.sibling
    );
    assert_eq!(
        stopped.titles,
        ["sibling"],
        "the stopped request's row must not exist; the sibling's must"
    );
}

/// The control: the isolate keeps serving. A sibling in flight while another
/// request of the same app is stopped completes, and its own transaction
/// commits.
#[test]
fn a_sibling_of_a_request_stopped_for_cpu_completes_and_commits() {
    let stopped = run_stopped(STOPPED_ACROSS_TURNS, Duration::from_millis(50));
    assert_stopped_for_cpu(&stopped.answer);
    assert_eq!(stopped.sibling.0, 200, "the sibling completes: {:?}", stopped.sibling);
    assert_eq!(
        stopped.sibling.1["json"]["transaction"], "committed",
        "the sibling's own transaction commits: {:?}",
        stopped.sibling
    );
    let titles = &stopped.sibling.1["json"]["titles"];
    assert!(
        titles.as_array().is_some_and(|titles| titles.contains(&"sibling".into())),
        "the sibling reads its own committed row: {titles}"
    );
    assert!(
        stopped.titles.iter().any(|title| title == "sibling"),
        "the sibling's transaction committed to the database: {:?}",
        stopped.titles
    );
}

/// Stopped by the per-window CPU timer (`check_v8_terminated`) while its own
/// transaction is open with a write pending in it. The stop rolls the
/// transaction back; the callback's next call is refused, and the transaction
/// fails with that refusal's code: neither write is left.
const STOPPED_WITH_A_TRANSACTION_OPEN: &str = r#"async function () {
    let opened;
    const insertedOpen = new Promise((resolve) => { opened = resolve; });
    let insert = "admitted";
    env.db.transaction(async (tx) => {
      await tx.collection(COLLECTION).insert({ title: "open-at-stop" });
      opened();
      await env.latch.wait();
      try {
        await tx.collection(COLLECTION).insert({ title: "after-stop" });
      } catch (error) {
        insert = error?.code ?? String(error);
        throw error;
      }
    }).then((result) => reportStopped({ insert, transaction: settled(result) }));
    await insertedOpen;
    return { survived: true, x: burn(300000000) };
  }"#;

#[test]
fn a_transaction_open_when_its_request_is_stopped_rolls_back() {
    let stopped = run_stopped(STOPPED_WITH_A_TRANSACTION_OPEN, Duration::from_millis(200));
    assert_stopped_for_cpu(&stopped.answer);
    assert_eq!(
        stopped.sibling.1["json"]["stopped"],
        serde_json::json!({"insert": "request_cancelled", "transaction": "request_cancelled"}),
        "the callback's next call must be refused, and the transaction fail with the same \
         code: {:?}",
        stopped.sibling
    );
    assert_eq!(
        stopped.titles,
        ["sibling"],
        "the write pending at the stop must be rolled back"
    );
}

/// The stopped request's callback returns, which asks for a commit, and the
/// stop lands after that request and before the commit is sent: twenty
/// microtasks on from the callback's return, past the few the transaction's
/// own settle handler waits behind. The commit is the stopped request's
/// platform call too, so it is refused: the transaction ends with a ROLLBACK,
/// not a COMMIT, and fails with the refusal's code.
const STOPPED_ASKING_FOR_A_COMMIT: &str = r#"async function () {
    env.db.transaction(async (tx) => {
      await tx.collection(COLLECTION).insert({ title: "asked-to-commit" });
      let hop = Promise.resolve();
      for (let i = 0; i < 20; i++) hop = hop.then(() => {});
      hop.then(() => burn(300000000));
    }).then((result) => reportStopped({ transaction: settled(result) }));
    await new Promise(() => {});
  }"#;

#[test]
fn a_commit_asked_for_after_its_request_is_stopped_is_refused() {
    run(async {
        let app = App::new(
            &format!("stopped: {STOPPED_ASKING_FOR_A_COMMIT},"),
            Duration::from_millis(200),
        )
        .await;
        recording::clear();
        let sibling = app.call("sibling");
        let stopped = answer(app.call("stopped"), "the stopped request").await;
        assert_stopped_for_cpu(&stopped);
        let (_, sibling) = completed(answer_of(sibling).await);
        assert_eq!(
            sibling["json"]["stopped"],
            serde_json::json!({"transaction": "request_cancelled"}),
            "the commit asked for before the stop must be refused after it: {sibling}"
        );
        assert_eq!(
            recording::endings(),
            [
                Ending::Settled(SettleIntent::Rollback),
                Ending::Settled(SettleIntent::Commit)
            ],
            "the stopped request's transaction must end with a ROLLBACK its refused commit \
             chose, and the sibling's with its COMMIT"
        );
        assert_eq!(app.titles().await, ["sibling"], "the refused commit's write is rolled back");
    });
}

/// Stopped inside its transaction's callback, after an `await`, so the callback
/// never settles and nothing it would run ends the transaction. The stop rolls
/// it back and releases its lane at once: a sibling's transaction begun after
/// the stop commits before the stopped transaction's execution deadline could
/// have expired, which is when the lane was released without the rollback.
const STOPPED_INSIDE_ITS_CALLBACK: &str = r#"async function () {
    await env.db.transaction(async (tx) => {
      await tx.collection(COLLECTION).insert({ title: "open-at-stop" });
      await env.latch.wait();
      burn(300000000);
    });
    return { survived: true };
  }"#;

const COMMITS_ITS_OWN: &str = r#"async afterStop() {
    return settled(await env.db.transaction(async (tx) => {
      await tx.collection(COLLECTION).insert({ title: "after-stop-sibling" });
    }));
  },"#;

#[test]
fn a_transaction_its_stopped_request_left_open_is_rolled_back_at_the_stop() {
    run(async {
        let app = App::new(
            &format!("stopped: {STOPPED_INSIDE_ITS_CALLBACK}, {COMMITS_ITS_OWN}"),
            Duration::from_millis(200),
        )
        .await;
        // The stopped transaction begins after this, so its execution deadline
        // is no earlier than this plus the execution budget.
        let dispatched = Instant::now();
        let parked = latch_parked();
        let stopped = app.call("stopped");
        compio::time::timeout(Duration::from_secs(30), parked)
            .await
            .expect("the stopped request parks inside its transaction")
            .expect("the latch reports its park");
        release_latch();
        assert_stopped_for_cpu(&answer(stopped, "the stopped request").await);

        let deadline = dispatched
            + Duration::from_millis(u64::from(zeroship_data_orm::budgets::DB_IDLE_IN_TX_TIMEOUT_MS));
        let after_stop = app.call("afterStop");
        let before_the_deadline = futures::future::select(
            Box::pin(answer_of(after_stop)),
            Box::pin(compio::time::sleep(deadline.saturating_duration_since(Instant::now()))),
        )
        .await;
        let futures::future::Either::Left((answer, _)) = before_the_deadline else {
            panic!(
                "the sibling's transaction waited for the stopped transaction's execution \
                 deadline instead of beginning at the stop"
            );
        };
        let committed = completed(answer);
        assert_eq!(committed.1["json"], "committed", "the sibling commits: {committed:?}");
        assert_eq!(
            app.titles().await,
            ["after-stop-sibling"],
            "the stopped transaction's write is rolled back, the sibling's committed"
        );
    });
}

/// Stopped while a commit it asked for is held by the database. Whether that
/// commit applies is decided after the stop, so the stopped request's caller is
/// answered that the outcome is unknown, under the transaction code for that,
/// rather than with the plain CPU error. The commit then completes, which is
/// the case the answer has to allow for.
/// The SERIALIZED body a real caller receives, not the Rust value: the wire
/// contract is the string a client parses, which is what the worker
/// (`zeroship-worker::handler::make_error`) and the standalone dev server
/// (`core::serve`) both send by running [`DispatchError::body`] through
/// `serde_json::to_string`. Round-tripping it here through a string proves
/// the code survives serialization, not only that the struct field is set.
fn wire_body(error: &DispatchError) -> serde_json::Value {
    let wire = serde_json::to_string(&error.body()).expect("the error body serializes");
    serde_json::from_str(&wire).expect("the worker's error body is valid JSON")
}

const STOPPED_WITH_A_COMMIT_IN_FLIGHT: &str = r#"async function () {
    env.db.transaction(async (tx) => {
      await tx.collection(COLLECTION).insert({ title: "in-flight" });
    }).then((result) => reportStopped({ transaction: settled(result) }));
    await env.latch.wait();
    return { survived: true, x: burn(300000000) };
  }"#;

#[test]
fn a_request_stopped_with_its_commit_in_flight_is_answered_that_the_outcome_is_unknown() {
    run(async {
        let app = App::new(
            &format!("stopped: {STOPPED_WITH_A_COMMIT_IN_FLIGHT},"),
            Duration::from_millis(200),
        )
        .await;
        let (commit_held, release_commit) = recording::hold_next_commit();
        let sibling = app.call("sibling");
        let stopped = app.call("stopped");
        compio::time::timeout(Duration::from_secs(30), commit_held)
            .await
            .expect("the stopped request's commit reaches the database")
            .expect("the gate reports the held commit");
        release_latch();
        let stopped = answer(stopped, "the stopped request").await;
        match &stopped {
            Err(error) => {
                assert_eq!(
                    error.code,
                    Some("commit_failed_indeterminate"),
                    "a commit in flight at the stop leaves its outcome unknown: {error:?}"
                );
                assert!(
                    error.message.contains("CPU time limit exceeded"),
                    "the answer still names the stop: {error:?}"
                );
                assert_eq!(
                    wire_body(error)["code"],
                    "commit_failed_indeterminate",
                    "an RPC-origin request's SERIALIZED body carries the code too: {error:?}"
                );
            }
            Ok(_) => panic!("the stopped request must be answered with an error"),
        }
        release_commit.send(()).expect("the held commit is waiting");
        let (_, sibling) = completed(answer_of(sibling).await);
        assert_eq!(
            sibling["json"]["stopped"],
            serde_json::json!({"transaction": "committed"}),
            "the commit already sent completes: {sibling}"
        );
        assert_eq!(
            app.titles().await,
            ["in-flight", "sibling"],
            "the commit the answer called unknown did apply"
        );
    });
}

/// The plain-fetch sibling of the test above: the stopped request is
/// dispatched through `default.fetch` (via [`App::call_plain_fetch`]), not
/// `/__zeroship/v1/<id>`, so it carries no RPC lifetime and
/// `cleanup_cancelled_requests` answers it through its non-RPC branch
/// (`send_pending_error(req, early_end_error(cause, mid_commit))`). That
/// branch must answer a plain-fetch caller the same way the RPC branch
/// answers an RPC one.
#[test]
fn a_plain_fetch_request_stopped_with_its_commit_in_flight_is_answered_that_the_outcome_is_unknown() {
    run(async {
        let app = App::with_fetch(
            &format!("stopped: {STOPPED_WITH_A_COMMIT_IN_FLIGHT},"),
            Duration::from_millis(200),
        )
        .await;
        let (commit_held, release_commit) = recording::hold_next_commit();
        let sibling = app.call("sibling");
        let stopped = app.call_plain_fetch("stopped");
        compio::time::timeout(Duration::from_secs(30), commit_held)
            .await
            .expect("the stopped request's commit reaches the database")
            .expect("the gate reports the held commit");
        release_latch();
        let stopped = answer(stopped, "the stopped request").await;
        match &stopped {
            Err(error) => {
                assert_eq!(
                    error.code,
                    Some("commit_failed_indeterminate"),
                    "a commit in flight at the stop leaves its outcome unknown over plain \
                     fetch too: {error:?}"
                );
                assert!(
                    error.message.contains("CPU time limit exceeded"),
                    "the answer still names the stop: {error:?}"
                );
                assert_eq!(
                    wire_body(error)["code"],
                    "commit_failed_indeterminate",
                    "a plain-fetch request's SERIALIZED body carries the code too: {error:?}"
                );
            }
            Ok(_) => panic!("the stopped request must be answered with an error"),
        }
        release_commit.send(()).expect("the held commit is waiting");
        let (_, sibling) = completed(answer_of(sibling).await);
        assert_eq!(
            sibling["json"]["stopped"],
            serde_json::json!({"transaction": "committed"}),
            "the commit already sent completes: {sibling}"
        );
        assert_eq!(
            app.titles().await,
            ["in-flight", "sibling"],
            "the commit the answer called unknown did apply"
        );
    });
}

/// The control beside it: stopped with no commit in flight, the answer is the
/// plain CPU error, with no code.
#[test]
fn a_request_stopped_with_no_commit_in_flight_is_answered_without_a_code() {
    let stopped = run_stopped(STOPPED_WITH_A_TRANSACTION_OPEN, Duration::from_millis(200));
    assert_stopped_for_cpu(&stopped.answer);
    match &stopped.answer {
        Err(error) => assert_eq!(error.code, None, "{error:?}"),
        Ok(_) => unreachable!("asserted above"),
    }
}

/// Reads made through the SDK are batched per request. The stopped request
/// reads after the stop, and a live request reads in the same microtask turn:
/// whichever goes first schedules its batch's flush and then wakes the other,
/// which reads before that flush runs. Each read is answered for its own
/// request, whichever of them scheduled first: the stopped request's is
/// refused and the live one's is served.
const BATCHED_READS: &str = r"
let wakeReader;
const readerWoken = new Promise((resolve) => { wakeReader = resolve; });
let stoppedParked;
const stoppedParkedForReader = new Promise((resolve) => { stoppedParked = resolve; });
let wakeStopped;
const stoppedWoken = new Promise((resolve) => { wakeStopped = resolve; });
";

fn batched_reads(stopped_first: bool) -> serde_json::Value {
    let (stopped_step, reader_step) = if stopped_first {
        (
            "wakeReader(); const mine = env.db[COLLECTION].get(\"not_absent\");",
            "await readerWoken; const mine = env.db[COLLECTION].get(\"not_absent\");",
        )
    } else {
        (
            "stoppedParked(); await stoppedWoken; const mine = env.db[COLLECTION].get(\"not_absent\");",
            "await stoppedParkedForReader; wakeStopped(); const mine = env.db[COLLECTION].get(\"not_absent\");",
        )
    };
    let procedures = format!(
        r"stopped: async function () {{
    env.latch.wait().then(async () => {{
      {stopped_step}
      reportStopped(read(await mine));
    }});
    return {{ survived: true, x: burn(300000000) }};
  }},
  async reader() {{
    {reader_step}
    return {{ reader: read(await mine), stopped: await stoppedOutcome }};
  }},"
    );
    run(async {
        let app = App::with_prelude(BATCHED_READS, &procedures, Duration::from_millis(200)).await;
        let stopped = app.runtime.call_fetch_handler(
            "POST",
            "http://localhost/__zeroship/v1/stopped",
            &[("content-type".into(), "application/json".into())],
            r#"{"json":null}"#,
            &EnvSnapshot::empty(),
            RequestCtx::new(CancelFlag::new()),
        );
        match stopped {
            FetchOutcome::Response { status, .. } => {
                assert_eq!(status, 503, "the stopped request is stopped in its dispatch");
            }
            _ => panic!("a request stopped in its dispatch is answered there"),
        }
        let reader = app.call("reader");
        release_latch();
        completed(answer(reader, "the reader").await).1
    })
}

#[test]
fn a_live_read_is_not_refused_for_sharing_a_stopped_requests_batch() {
    let read = batched_reads(true);
    assert_eq!(
        read["json"],
        serde_json::json!({"reader": "read", "stopped": "request_cancelled"}),
        "the live read is served and the stopped one refused: {read}"
    );
}

#[test]
fn a_stopped_read_is_not_served_through_a_live_requests_batch() {
    let read = batched_reads(false);
    assert_eq!(
        read["json"],
        serde_json::json!({"reader": "read", "stopped": "request_cancelled"}),
        "the stopped read is refused and the live one served: {read}"
    );
}

/// The control: reads one request makes in one microtask turn still share one
/// batch, so the batching that the two cases above split by request is there
/// to split.
#[test]
fn reads_one_request_makes_together_share_one_batch() {
    run(async {
        let app = App::new(
            r#"async together() {
    const [a, b] = await Promise.all([
      env.db[COLLECTION].get("not_first"),
      env.db[COLLECTION].get("not_second"),
    ]);
    return [read(a), read(b)];
  },"#,
            Duration::from_secs(5),
        )
        .await;
        recording::clear();
        let (_, together) = completed(answer(app.call("together"), "the reads").await);
        assert_eq!(together["json"], serde_json::json!(["read", "read"]), "{together}");
        let batched: Vec<_> = recording::queries()
            .into_iter()
            .filter(|query| query.sql.starts_with("SELECT "))
            .collect();
        assert_eq!(batched.len(), 1, "both reads go out as one query: {batched:?}");
        for id in ["not_first", "not_second"] {
            assert!(
                batched[0].params.contains(&zeroship_data_orm::value::Value::String(id.into())),
                "the one query carries both ids: {batched:?}"
            );
        }
    });
}
