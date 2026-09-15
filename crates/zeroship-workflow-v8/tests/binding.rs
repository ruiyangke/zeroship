use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use zeroship_id::AppId;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};
use zeroship_workflow::service::runner::ready::ReadyApps;
use zeroship_workflow_v8::{is_excluded_workflow_property, WorkflowBinding};

fn modules(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: source.to_string(),
    }]
}

async fn run_workflow_app(app_id: AppId, source: &str) -> (u16, String) {
    init_v8();
    let mut env_vars = HashMap::new();
    env_vars.insert("APP_ID".to_string(), app_id.as_str().to_owned());
    let plugin: Arc<dyn NativePlugin> =
        Arc::new(WorkflowBinding::ready(ReadyApps::default()));
    let runtime = Runtime::builder()
        .modules(modules(source))
        .env_vars(env_vars)
        .plugins(vec![plugin])
        .app_id(app_id)
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
        run_workflow_app(AppId::mint(), source).await;
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
