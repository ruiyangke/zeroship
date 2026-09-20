//! Build the Vite entry and exercise its emitted modules through native dispatch.

use std::time::Duration;

use serde_json::{Value, json};
use zeroship_runtime::channel::{CancelFlag, StreamReader};
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch};

struct Probe;

impl NativePlugin for Probe {
    fn namespace(&self) -> &str {
        "probe"
    }

    fn register(&self, registrar: &mut NativeRegistrar) {
        registrar.add("kind", probe_kind);
    }
}

fn probe_kind(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let kind = format!("{:?}", zeroship_runtime::rpc::current_kind(scope));
    rv.set(v8::String::new(scope, &kind).unwrap().into());
}

fn build_modules() -> Vec<ModuleEntry> {
    let plugin_root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/vite-plugin");
    let built = std::process::Command::new("node")
        .current_dir(plugin_root)
        .args([
            "--import",
            "tsx",
            "test/fixtures/build-runtime-server-entry.ts",
        ])
        .output()
        .expect("Node and installed Vite build dependencies are required");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let entries: Vec<Value> = serde_json::from_slice(&built.stdout)
        .expect("Vite fixture must return its emitted module graph");
    assert!(!entries.is_empty(), "Vite must build executable modules");
    entries
        .into_iter()
        .map(|entry| {
            let source = entry["source"].as_str().unwrap();
            assert!(
                !source.trim().is_empty(),
                "emitted module must be executable"
            );
            ModuleEntry {
                specifier: entry["specifier"].as_str().unwrap().to_owned(),
                source: source.to_owned(),
            }
        })
        .collect()
}

async fn stream_body(reader: StreamReader) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        while let Some(chunk) = reader.pop() {
            bytes.extend_from_slice(&chunk);
        }
        if reader.is_done() {
            break;
        }
        reader.wait_for_data().await;
    }
    assert!(!reader.is_overflow(), "unexpected stream overflow");
    assert!(reader.error().is_none(), "unexpected stream error");
    bytes
}

async fn request(runtime: &Runtime, path: &str, input: Value) -> (u16, String) {
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "POST",
        &format!("http://localhost/{path}"),
        &[("content-type".into(), "application/json".into())],
        json!({"json": input}).to_string(),
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    compio::time::timeout(Duration::from_secs(5), async {
        let (status, bytes) = match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
            FetchOutcome::Stream {
                status,
                body_reader,
                ..
            } => (status, stream_body(body_reader).await),
            FetchOutcome::Pending { rx, .. } => match rx.recv().await.expect("dispatch result") {
                SettledFetch::Response { status, body, .. } => (status, body),
                SettledFetch::Stream {
                    status,
                    body_reader,
                    ..
                } => (status, stream_body(body_reader).await),
                _ => panic!("unexpected WebSocket"),
            },
            _ => panic!("unexpected WebSocket"),
        };
        (status, String::from_utf8(bytes).unwrap())
    })
    .await
    .expect("native dispatch must finish")
}

async fn rpc(runtime: &Runtime, name: &str, input: Value) -> Value {
    let (status, body) = request(runtime, &format!("__zeroship/v1/{name}"), input).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str::<Value>(&body).unwrap()["json"].clone()
}

#[compio::test]
async fn vite_artifact_retains_procedure_metadata_and_native_module_identity() {
    let modules = build_modules();
    zeroship_runtime::init_v8();
    let runtime = Runtime::builder().plugin(Probe).modules(modules).build();
    runtime.exit_isolate();
    runtime.initialize(&EnvSnapshot::empty()).await.unwrap();
    runtime.start_pump();

    assert_eq!(
        rpc(&runtime, "inspect", Value::Null).await,
        json!({"loaded": 0, "invoked": 0, "loaderKind": ""}),
    );
    assert_eq!(
        rpc(&runtime, "eager", json!(3)).await,
        json!({"value": 6, "kind": "Some(Query)", "sameRequest": true}),
    );

    let (status, body) = request(&runtime, "__zeroship/v1/__proto__", json!("invalid")).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["code"],
        "INVALID_ARGUMENT"
    );
    assert_eq!(
        rpc(&runtime, "inspect", Value::Null).await,
        json!({"loaded": 1, "invoked": 0, "loaderKind": "None"}),
    );
    assert_eq!(
        rpc(&runtime, "__proto__", json!(5)).await,
        json!({"value": 10, "kind": "Some(Query)", "sameRequest": true}),
    );
    assert_eq!(
        rpc(&runtime, "__proto__", json!(7)).await,
        json!({"value": 14, "kind": "Some(Query)", "sameRequest": true}),
    );
    assert_eq!(
        rpc(&runtime, "inspect", Value::Null).await,
        json!({"loaded": 1, "invoked": 2, "loaderKind": "None"}),
    );

    let (status, body) = request(&runtime, "__zeroship/v1/tokens", json!("token")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "0:\"token\"\n0:\"same request\"\nd:{}\n");

    let (status, body) = request(&runtime, "page", Value::Null).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"label": "original receiver", "path": "/page", "kind": "None"}),
    );
    let (status, body) = request(&runtime, "__zeroship/v1/missing", Value::Null).await;
    assert_eq!(status, 404, "{body}");
}
