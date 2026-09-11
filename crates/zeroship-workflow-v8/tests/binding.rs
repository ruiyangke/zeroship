use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};
use zeroship_workflow_v8::{is_excluded_workflow_property, WorkflowBinding};

const TEST_CONTROL_KEY: &str = "test-control-key";

fn modules(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: source.to_string(),
    }]
}

async fn run_workflow_app(control_url: String, app_id: Uuid, source: &str) -> (u16, String) {
    init_v8();
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id.to_string());
    let plugin: Arc<dyn NativePlugin> =
        Arc::new(WorkflowBinding::new(control_url, TEST_CONTROL_KEY));
    let runtime = Runtime::builder()
        .modules(modules(source))
        .env_vars(env_vars)
        .plugins(vec![plugin])
        .build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        FetchOutcome::Pending { rx, cancel: _ } => {
            let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .expect("workflow plugin fetch timed out")
                .expect("workflow plugin pending dispatch failed");
            match settled {
                SettledFetch::Response { status, body, .. } => {
                    (status, String::from_utf8_lossy(&body).into_owned())
                }
                other => {
                    let name = match other {
                        SettledFetch::Stream { .. } => "Stream",
                        SettledFetch::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                        SettledFetch::Response { .. } => unreachable!(),
                    };
                    panic!("workflow plugin: expected response, got {name}");
                }
            }
        }
        FetchOutcome::Stream { .. } => panic!("workflow plugin: unexpected stream"),
        FetchOutcome::WebSocketUpgrade { .. } => {
            panic!("workflow plugin: unexpected websocket upgrade")
        }
    }
}

async fn run_dev_workflow_app(
    db_path: &std::path::Path,
    app_id: Uuid,
    source: &str,
) -> (u16, String) {
    init_v8();
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id.to_string());
    let plugin: Arc<dyn NativePlugin> = Arc::new(
        WorkflowBinding::dev_sqlite(db_path, modules(source), env_vars.clone(), Vec::new())
            .expect("dev workflow plugin"),
    );
    let runtime = Runtime::builder()
        .modules(modules(source))
        .env_vars(env_vars)
        .plugins(vec![plugin])
        .build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        FetchOutcome::Pending { rx, cancel: _ } => {
            let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .expect("workflow dev fetch timed out")
                .expect("workflow dev pending dispatch failed");
            match settled {
                SettledFetch::Response { status, body, .. } => {
                    (status, String::from_utf8_lossy(&body).into_owned())
                }
                other => {
                    let name = match other {
                        SettledFetch::Stream { .. } => "Stream",
                        SettledFetch::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                        SettledFetch::Response { .. } => unreachable!(),
                    };
                    panic!("workflow dev: expected response, got {name}");
                }
            }
        }
        FetchOutcome::Stream { .. } => panic!("workflow dev: unexpected stream"),
        FetchOutcome::WebSocketUpgrade { .. } => {
            panic!("workflow dev: unexpected websocket upgrade")
        }
    }
}

#[test]
fn getter_exclusion_list_matches_the_binding_contract() {
    for name in [
        "",
        "then",
        "toJSON",
        "inspect",
        "constructor",
        "prototype",
        "__proto__",
        "__defineGetter__",
        "__defineSetter__",
        "__lookupGetter__",
        "__lookupSetter__",
        "hasOwnProperty",
        "isPrototypeOf",
        "propertyIsEnumerable",
        "toLocaleString",
        "toString",
        "valueOf",
    ] {
        assert!(
            is_excluded_workflow_property(name),
            "{name} must be excluded"
        );
    }
    assert!(!is_excluded_workflow_property("Checkout"));
}

#[compio::test]
async fn dev_sqlite_engine_runs_sleep_signal_core_loop_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("workflows.sqlite");
    let app_id = Uuid::new_v4();
    let source = r#"
        const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

        export class LocalApproval {
          async run(trigger, step) {
            const first = await step.run("record", () => ({ orderId: trigger.input.orderId }));
            await step.sleep("cooldown", "20ms");
            const signal = await step.waitForSignal("approved", { type: "approved" });
            return { first, approved: signal.payload };
          }
        }

        export default {
          async fetch(_req, env) {
            const run = await env.workflows.LocalApproval.start({ input: { orderId: "ord_1" } });
            let beforeSignal = null;
            for (let i = 0; i < 30; i += 1) {
              beforeSignal = await run.status();
              if (beforeSignal.state === "waiting") break;
              await delay(20);
            }
            const signal = await run.signal({ type: "approved", payload: { by: "local" } });
            let finalStatus = null;
            for (let i = 0; i < 30; i += 1) {
              finalStatus = await run.status();
              if (finalStatus.state === "completed") break;
              await delay(20);
            }
            return Response.json({ runId: run.id, beforeSignal, signal, finalStatus });
          }
        };
    "#;

    let (status, body) = run_dev_workflow_app(&db_path, app_id, source).await;
    assert_eq!(status, 200, "body: {body}");
    let value: Value = serde_json::from_str(&body).expect("body json");
    assert_eq!(value["beforeSignal"]["state"], "waiting", "body: {body}");
    assert_eq!(value["finalStatus"]["state"], "completed", "body: {body}");
    assert_eq!(
        value["finalStatus"]["output"],
        json!({
            "first": { "orderId": "ord_1" },
            "approved": { "by": "local" }
        }),
        "body: {body}"
    );
    let run_id = value["runId"].as_str().expect("run id");
    let sqlite = rusqlite::Connection::open(&db_path).expect("open workflow db");
    let step_count: i64 = sqlite
        .query_row(
            "SELECT COUNT(*) FROM workflow_steps WHERE run_id = ?1 AND name = 'record' AND kind = 'run'",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("step count");
    assert_eq!(step_count, 1, "step.run must execute exactly once");
    let completed_wait: i64 = sqlite
        .query_row(
            "SELECT COUNT(*) FROM workflow_steps WHERE run_id = ?1 AND name = 'approved' AND kind = 'wait_signal' AND state = 'completed'",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("wait count");
    assert_eq!(completed_wait, 1, "signal wait should complete once");
}

#[compio::test]
async fn v8_binding_getter_exclusions_are_undefined() {
    let source = r#"
        export default {
          fetch(_req, env) {
            return Response.json({
              thenIsUndefined: env.workflows.then === undefined,
              toJSONIsUndefined: env.workflows.toJSON === undefined,
              toStringIsUndefined: env.workflows.toString === undefined,
              hasOwnPropertyIsUndefined: env.workflows.hasOwnProperty === undefined,
              symbolIsUndefined: env.workflows[Symbol.toStringTag] === undefined,
              handleHasStart: typeof env.workflows.Checkout.start === "function",
              handleHasGet: typeof env.workflows.Checkout.get === "function"
            });
          }
        };
    "#;
    let (status, body) =
        run_workflow_app("http://127.0.0.1:9".to_string(), Uuid::new_v4(), source).await;
    assert_eq!(status, 200, "body: {body}");
    let value: Value = serde_json::from_str(&body).expect("body json");
    assert_eq!(value["thenIsUndefined"], true, "body: {body}");
    assert_eq!(value["toJSONIsUndefined"], true, "body: {body}");
    assert_eq!(value["toStringIsUndefined"], true, "body: {body}");
    assert_eq!(value["hasOwnPropertyIsUndefined"], true, "body: {body}");
    assert_eq!(value["symbolIsUndefined"], true, "body: {body}");
    assert_eq!(value["handleHasStart"], true, "body: {body}");
    assert_eq!(value["handleHasGet"], true, "body: {body}");
}

#[compio::test]
async fn a_pending_workflow_restores_the_hosts_active_isolate() {
    init_v8();
    let directory = tempfile::tempdir().unwrap();
    let source = r#"
        export class Pending {
            async run(_trigger, step) {
                return step.run("pause", () => new Promise(resolve => setTimeout(() => resolve("done"), 100)));
            }
        }
        export default {
            async fetch(request, env) {
                if (new URL(request.url).pathname === "/start") {
                    const run = await env.workflows.Pending.start({ input: {} });
                    return Response.json({ runId: run.id });
                }
                return new Response("host is active");
            }
        };
    "#;
    let env_vars = HashMap::from([("APP_ID".into(), Uuid::new_v4().to_string())]);
    let binding = WorkflowBinding::dev_sqlite(
        directory.path().join("journal.sqlite"),
        modules(source),
        env_vars.clone(),
        vec![],
    )
    .unwrap();
    let runtime = Runtime::builder()
        .modules(modules(source))
        .env_vars(env_vars)
        .plugins(vec![Arc::new(binding)])
        .build();
    runtime.start_pump();
    let env = EnvSnapshot::empty();
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://localhost/start",
        &[],
        "",
        &env,
        RequestCtx::new(CancelFlag::new()),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("start must await the workflow backend")
    };
    // Yield to the workflow executor, then dispatch on the host while it awaits.
    compio::time::sleep(Duration::from_millis(20)).await;
    let ping = runtime.call_fetch_handler(
        "GET",
        "http://localhost/ping",
        &[],
        "",
        &env,
        RequestCtx::new(CancelFlag::new()),
    );
    let (status, body) = match ping {
        FetchOutcome::Response { status, body, .. } => (status, body),
        FetchOutcome::Pending { rx, .. } => match rx.recv().await.unwrap() {
            SettledFetch::Response { status, body, .. } => (status, body),
            _ => panic!("expected host response"),
        },
        _ => panic!("expected host response"),
    };
    assert_eq!(status, 200);
    assert_eq!(body.as_slice(), b"host is active");
    let started = compio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        started,
        SettledFetch::Response { status: 200, .. }
    ));
}
