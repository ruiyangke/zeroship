//! A request stopped for CPU is refused the KV calls its continuations make.
//!
//! The request leaves a KV read in flight and burns CPU in its dispatch. The
//! read settles after the request was answered, and the code after it tries
//! KV again. Stopped, those calls are refused; the control is the same app run
//! with no CPU limit, which answers on its own and leaves the same code
//! running with its KV calls admitted.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use zeroship_kv::{KvConfig, KvStore};
use zeroship_kv_v8::KvBinding;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

const APP: &str = r#"
let report;
const outcome = new Promise((resolve) => { report = resolve; });
async function attempt(call) {
    try {
        await call();
        return "admitted";
    } catch (error) {
        return error.code ?? String(error);
    }
}
export default {
    async fetch(request, env) {
        if (new URL(request.url).pathname === "/outcome") return Response.json(await outcome);
        env.kv.get("key").then(async () => report({
            get: await attempt(() => env.kv.get("key")),
            set: await attempt(() => env.kv.set("key", "value")),
        }));
        let x = 0;
        for (let i = 0; i < 50000000; i++) { x = (x + 1) % 2147483647; }
        return Response.json({ survived: true, x });
    },
};
"#;

fn request(runtime: &Runtime, path: &str) -> FetchOutcome {
    runtime.call_fetch_handler(
        "GET",
        &format!("http://localhost{path}"),
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    )
}

/// Run the app once under `cpu_limit`: the request's status, and what its
/// continuation reported.
fn run(cpu_limit: Option<Duration>) -> (u16, serde_json::Value) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let store = KvStore::open(&KvConfig::Redb { path: dir.path().join("kv.redb") }).expect("open redb");
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        init_v8();
        let plugin: Arc<dyn NativePlugin> = Arc::new(KvBinding::new(store, None));
        let mut builder = Runtime::builder()
            .modules(vec![ModuleEntry { specifier: "index.js".into(), source: APP.into() }])
            .env_vars(HashMap::from([("APP_ID".to_owned(), "stopped_kv_app".to_owned())]))
            .plugins(vec![plugin]);
        if let Some(limit) = cpu_limit {
            builder = builder.cpu_limit(limit);
        }
        let runtime = builder.build();
        runtime.start_pump();
        let FetchOutcome::Response { status, .. } = request(&runtime, "/") else {
            panic!("the request answers in its dispatch, stopped or not");
        };
        let body = match request(&runtime, "/outcome") {
            FetchOutcome::Response { body, .. } => body,
            FetchOutcome::Pending { rx, .. } => {
                match compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("the outcome is answered")
                    .expect("the outcome completes")
                {
                    SettledFetch::Response { body, .. } => body,
                    _ => panic!("the outcome is a buffered response"),
                }
            }
            _ => panic!("the outcome is a buffered response"),
        };
        (status, serde_json::from_slice(&body).expect("the outcome is JSON"))
    })
}

#[test]
fn a_stopped_request_is_refused_its_kv_calls() {
    let (status, outcome) = run(Some(Duration::from_millis(10)));
    assert_eq!(status, 503, "the request is stopped in its dispatch");
    assert_eq!(
        outcome,
        serde_json::json!({ "get": "request_cancelled", "set": "request_cancelled" }),
    );
}

#[test]
fn a_request_answered_on_its_own_keeps_its_kv_calls() {
    let (status, outcome) = run(None);
    assert_eq!(status, 200, "with no CPU limit the request answers on its own");
    assert_eq!(outcome, serde_json::json!({ "get": "admitted", "set": "admitted" }));
}
