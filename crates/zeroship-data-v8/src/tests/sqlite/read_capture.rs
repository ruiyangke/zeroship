use super::fixtures::*;
use crate::tests::fixtures::parity;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use zeroship_data_orm::value;
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch};

struct ReadCaptureProbe;

impl NativePlugin for ReadCaptureProbe {
    fn namespace(&self) -> &str {
        "capture"
    }
    fn register(&self, registrar: &mut NativeRegistrar) {
        registrar.add("hasRead", has_read);
    }
}

fn has_read(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let title = args.get(0).to_rust_string_lossy(scope);
    let capture = crate::read_capture::current(scope).expect("current query capture");
    let row = HashMap::from([("title".into(), title)]);
    rv.set_bool(
        capture
            .snapshot_for("notes")
            .iter()
            .any(|entry| entry.matches(&row)),
    );
}

fn request(runtime: &Runtime, name: &str) -> FetchOutcome {
    runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/query",
        &[("content-type".into(), "application/json".into())],
        serde_json::json!({"json":name}).to_string(),
        &EnvSnapshot::empty(),
        RequestCtx::new(zeroship_runtime::channel::CancelFlag::new()),
    )
}

async fn response(outcome: FetchOutcome) -> serde_json::Value {
    let (status, body) = match outcome {
        FetchOutcome::Response { status, body, .. } => (status, body),
        FetchOutcome::Pending { rx, .. } => {
            match compio::time::timeout(Duration::from_secs(15), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                SettledFetch::Response { status, body, .. } => (status, body),
                _ => panic!("expected a buffered query result"),
            }
        }
        _ => panic!("expected a buffered query result"),
    };
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn overlapping_queries_keep_reads_from_before_and_after_await() {
    run(async {
        let directory = tempfile::tempdir().unwrap();
        let alias = crate::tests::fixtures::harness_alias(LOCAL_DEV_APP_ID);
        apply_schema_ahead_of_runtime(
            &directory,
            &format!("CREATE TABLE \"{alias}\".notes ({SYSTEM_COLUMNS_SQLITE}, title TEXT NOT NULL);"),
        );
        let db = crate::service::DbService::new(crate::service::DbServiceConfig {
            app_bindings: crate::tests::fixtures::harness_app_bindings([LOCAL_DEV_APP_ID]),
            project_keys: Default::default(),
            connection: crate::tests::fixtures::recording::connection(&parity::sqlite_url(
                &directory,
            )),
            cdc_relay: None,
            meter: None,
        })
        .unwrap();
        let source = r#"
            import { env } from 'zeroship';
            async function query(label) {
                const notes = env.db.collection('notes');
                const before = await notes.find({title: label + '-before'}, {});
                await new Promise(resolve => setTimeout(resolve, 10));
                const after = await notes.find({title: label + '-after'}, {});
                const neighbor = label === 'alpha' ? 'beta' : 'alpha';
                return {
                    empty: before.length === 0 && after.length === 0,
                    before: env.capture.hasRead(label + '-before'),
                    after: env.capture.hasRead(label + '-after'),
                    neighborBefore: env.capture.hasRead(neighbor + '-before'),
                    neighborAfter: env.capture.hasRead(neighbor + '-after'),
                };
            }
            query.config = {kind: 'query'};
            export default {rpc: {query}};
        "#;
        zeroship_runtime::init_v8();
        let plugins: Vec<Arc<dyn NativePlugin>> = vec![db.plugin(), Arc::new(ReadCaptureProbe)];
        let runtime = Runtime::builder()
            .modules(vec![ModuleEntry {
                specifier: "index.js".into(),
                source: source.into(),
            }])
            .plugins(plugins)
            .runtime_descriptor(Some(parity::runtime_descriptor(
                "notes",
                &value!({"title":{"type":"string","required":true}}),
            )))
            .build();
        runtime.initialize(&EnvSnapshot::empty()).await.unwrap();
        runtime.start_pump();
        let first = request(&runtime, "alpha");
        let second = request(&runtime, "beta");
        assert!(matches!(first, FetchOutcome::Pending { .. }));
        assert!(matches!(second, FetchOutcome::Pending { .. }));
        let (first, second) = futures::join!(response(first), response(second));
        for result in [first, second] {
            assert_eq!(
                result,
                serde_json::json!({"json":{
                    "empty":true, "before":true, "after":true,
                    "neighborBefore":false, "neighborAfter":false,
                }})
            );
        }
    });
}
