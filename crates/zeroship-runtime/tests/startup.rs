//! Native startup readiness before request dispatch.

use std::time::Duration;
use zeroship_runtime::auth::AuthPlugin;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::{Runtime, RuntimeLimits};
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};

fn runtime(source: &str, limits: RuntimeLimits) -> Runtime {
    zeroship_runtime::init_v8();
    Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: source.into(),
        }])
        .plugin(AuthPlugin)
        .limits(limits)
        .build()
}

fn dev_runtime(source: &str, limits: RuntimeLimits) -> Runtime {
    zeroship_runtime::init_v8();
    Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "dev-host.js".into(),
            source: source.into(),
        }])
        .plugin(AuthPlugin)
        .dev_entry_loader("createDevEntryLoader")
        .limits(limits)
        .build()
}

fn run_script(runtime: &Runtime, source: &str) {
    runtime.with_scope(|scope| {
        let source = v8::String::new(scope, source).unwrap();
        v8::Script::compile(scope, source, None)
            .unwrap()
            .run(scope)
            .unwrap();
    });
}

fn request(runtime: &Runtime, user: Option<&str>) -> FetchOutcome {
    runtime.call_fetch_handler_with_user(
        "GET",
        "http://localhost/",
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
        user.map(str::to_string),
    )
}

async fn settle(runtime: &Runtime, outcome: FetchOutcome) -> (u16, String) {
    let (status, body) = match outcome {
        FetchOutcome::Response { status, body, .. } => (status, body),
        FetchOutcome::Pending { rx, .. } => {
            runtime.start_pump();
            let result = compio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("startup response deadline")
                .expect("response settlement");
            match result {
                SettledFetch::Response { status, body, .. } => (status, body),
                _ => panic!("expected buffered startup response"),
            }
        }
        _ => panic!("expected buffered startup response"),
    };
    (status, String::from_utf8(body).unwrap())
}

#[compio::test]
async fn dev_loader_publishes_native_http_and_rpc_snapshots() {
    let runtime = dev_runtime(
        r#"
        let generation = 0;
        export function createDevEntryLoader(invalidate) {
            globalThis.replaceEntry = () => { generation++; invalidate(); };
            return async () => {
                await Promise.resolve();
                const tag = `generation-${generation}`;
                return {
                    userDefault: { tag },
                    fetch() {
                        return Response.json({ tag: this.tag });
                    },
                    rpc: {
                        version: () => tag,
                    },
                };
            };
        }
    "#,
        RuntimeLimits::default(),
    );

    let first = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(first.0, 200, "{}", first.1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&first.1).unwrap(),
        serde_json::json!({"tag":"generation-0"})
    );
    let rpc = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/version",
        &[("content-type".into(), "application/json".into())],
        "null",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    let rpc = settle(&runtime, rpc).await;
    assert_eq!(rpc.0, 200, "{}", rpc.1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&rpc.1).unwrap(),
        serde_json::json!({"json":"generation-0"})
    );

    run_script(&runtime, "replaceEntry()");
    let replacement = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(replacement.0, 200, "{}", replacement.1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&replacement.1).unwrap(),
        serde_json::json!({"tag":"generation-1"})
    );
}

#[compio::test]
async fn dev_loader_caches_failure_until_another_invalidation() {
    let runtime = dev_runtime(
        r#"
        let mode = 'ready';
        globalThis.devLoads = 0;
        export function createDevEntryLoader(invalidate) {
            globalThis.replaceEntry = next => { mode = next; invalidate(); };
            return async () => {
                globalThis.devLoads++;
                await Promise.resolve();
                if (mode === 'broken') throw new Error('replacement compilation failed');
                const captured = mode;
                return {
                    userDefault: {},
                    fetch() { return new Response(captured); },
                };
            };
        }
    "#,
        RuntimeLimits::default(),
    );

    assert_eq!(
        settle(&runtime, request(&runtime, None)).await,
        (200, "ready".into())
    );
    run_script(&runtime, "replaceEntry('broken')");
    let first = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(first.0, 500, "{}", first.1);
    assert!(
        first.1.contains("replacement compilation failed"),
        "{}",
        first.1
    );
    let cached = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(cached, first);
    runtime.with_scope(|scope| {
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "devLoads").unwrap();
        assert_eq!(
            global.get(scope, key.into()).unwrap().integer_value(scope),
            Some(2)
        );
    });

    run_script(&runtime, "replaceEntry('recovered')");
    assert_eq!(
        settle(&runtime, request(&runtime, None)).await,
        (200, "recovered".into())
    );
}

#[compio::test]
async fn in_flight_request_retains_the_snapshot_replaced_by_dev_loader() {
    let runtime = dev_runtime(
        r#"
        let generation = 0;
        export function createDevEntryLoader(invalidate) {
            globalThis.replaceEntry = () => { generation++; invalidate(); };
            return () => {
                const tag = generation === 0 ? 'original' : 'replacement';
                return {
                    userDefault: { tag },
                    fetch() {
                        const captured = this.tag;
                        if (captured === 'original') {
                            return new Promise(resolve => setTimeout(
                                () => resolve(new Response(captured)),
                                10,
                            ));
                        }
                        return new Response(captured);
                    },
                };
            };
        }
    "#,
        RuntimeLimits::default(),
    );

    let original = request(&runtime, None);
    assert!(matches!(original, FetchOutcome::Pending { .. }));
    run_script(&runtime, "replaceEntry()");
    assert_eq!(
        settle(&runtime, request(&runtime, None)).await,
        (200, "replacement".into())
    );
    assert_eq!(settle(&runtime, original).await, (200, "original".into()));
}

#[compio::test]
async fn failed_dev_replacement_does_not_stop_an_in_flight_request() {
    let runtime = dev_runtime(
        r#"
        let broken = false;
        export function createDevEntryLoader(invalidate) {
            globalThis.breakReplacement = () => { broken = true; invalidate(); };
            return async () => {
                if (broken) throw new Error('replacement is broken');
                return {
                    userDefault: {},
                    fetch() {
                        return new Promise(resolve => setTimeout(
                            () => resolve(new Response('original completed')),
                            10,
                        ));
                    },
                };
            };
        }
    "#,
        RuntimeLimits::default(),
    );

    let original = request(&runtime, None);
    assert!(matches!(original, FetchOutcome::Pending { .. }));
    run_script(&runtime, "breakReplacement()");
    let failed = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(failed.0, 500, "{}", failed.1);
    assert!(failed.1.contains("replacement is broken"), "{}", failed.1);
    assert_eq!(
        settle(&runtime, original).await,
        (200, "original completed".into())
    );
}

#[compio::test]
async fn dev_loader_discards_obsolete_load_before_starting_latest_generation() {
    let runtime = dev_runtime(
        r#"
        let generation = 0;
        globalThis.devLoads = [];
        export function createDevEntryLoader(invalidate) {
            globalThis.replaceEntry = () => { generation++; invalidate(); };
            return async () => {
                const captured = generation;
                globalThis.devLoads.push(captured);
                if (captured === 1) {
                    await new Promise(resolve => setTimeout(resolve, 10));
                }
                return {
                    userDefault: {},
                    fetch() { return new Response(String(captured)); },
                };
            };
        }
    "#,
        RuntimeLimits::default(),
    );
    assert_eq!(
        settle(&runtime, request(&runtime, None)).await,
        (200, "0".into())
    );

    run_script(&runtime, "replaceEntry()");
    let waiting = request(&runtime, None);
    assert!(matches!(waiting, FetchOutcome::Pending { .. }));
    run_script(&runtime, "replaceEntry()");
    assert_eq!(settle(&runtime, waiting).await, (200, "2".into()));
    runtime.with_scope(|scope| {
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "devLoads").unwrap();
        let loads = global.get(scope, key.into()).unwrap();
        assert_eq!(
            v8::json::stringify(scope, loads)
                .unwrap()
                .to_rust_string_lossy(scope),
            "[0,1,2]"
        );
    });
}

#[compio::test]
async fn cancellation_settles_a_request_waiting_for_dev_replacement() {
    let runtime = dev_runtime(
        r#"
        let blocked = false;
        export function createDevEntryLoader(invalidate) {
            globalThis.blockEntry = () => { blocked = true; invalidate(); };
            return async () => {
                if (blocked) await new Promise(() => {});
                return {
                    userDefault: {},
                    fetch() { return new Response('ready'); },
                };
            };
        }
    "#,
        RuntimeLimits::default(),
    );
    assert_eq!(
        settle(&runtime, request(&runtime, None)).await,
        (200, "ready".into())
    );
    run_script(&runtime, "blockEntry()");
    let FetchOutcome::Pending { rx, cancel } = request(&runtime, None) else {
        panic!("replacement load must queue its request");
    };
    runtime.start_pump();
    cancel.cancel();
    let result = compio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("cancellation must wake dev entry waiter");
    let Err(error) = result else {
        panic!("cancelled dev entry waiter must fail");
    };
    assert!(
        error.message.contains("cancelled during startup"),
        "{error:?}"
    );
}

#[compio::test]
async fn dev_loader_prunes_deleted_exports_and_hides_invalidation_callback() {
    let runtime = dev_runtime(
        r#"
        let includeFetch = true;
        export function createDevEntryLoader(invalidate) {
            globalThis.removeFetch = () => { includeFetch = false; invalidate(); };
            return () => includeFetch ? {
                userDefault: {},
                fetch() {
                    const hidden = !Reflect.ownKeys(globalThis)
                        .some(key => globalThis[key] === invalidate);
                    return Response.json({ hidden });
                },
            } : { userDefault: {} };
        }
    "#,
        RuntimeLimits::default(),
    );

    let initial = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(initial.0, 200, "{}", initial.1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&initial.1).unwrap(),
        serde_json::json!({"hidden":true})
    );
    run_script(&runtime, "removeFetch()");
    let removed = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(removed.0, 404, "{}", removed.1);
}

#[compio::test]
async fn initial_dev_invalidation_requires_a_fresh_runtime() {
    let runtime = dev_runtime(
        r#"
        export function createDevEntryLoader(invalidate) {
            invalidate();
            return () => ({ userDefault: {}, fetch() { return new Response('must not run'); } });
        }
    "#,
        RuntimeLimits::default(),
    );
    let failed = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(failed.0, 500, "{}", failed.1);
    assert!(
        failed.1.contains("fresh runtime is required"),
        "{}",
        failed.1
    );
}

#[compio::test]
async fn top_level_await_finishes_before_dispatch() {
    let runtime = runtime(
        r#"
        let ready = false;
        await new Promise(resolve => setTimeout(resolve, 10));
        ready = true;
        export default { fetch() { return Response.json({ ready }); } };
    "#,
        RuntimeLimits::default(),
    );
    let (status, body) = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"ready":true})
    );
}

#[compio::test]
async fn overlapping_startup_requests_keep_identity_and_startup_is_anonymous() {
    let runtime = runtime(
        r#"
        import { env } from 'zeroship';
        const startupUsers = [env.auth.getUser()];
        let starts = 0;
        starts++;
        await new Promise(resolve => setTimeout(resolve, 10));
        startupUsers.push(env.auth.getUser());
        export default { fetch() {
            return Response.json({ starts, startupUsers, user: env.auth.getUser() });
        } };
    "#,
        RuntimeLimits::default(),
    );
    let a = request(&runtime, Some(r#"{"id":"alice"}"#));
    let b = request(&runtime, Some(r#"{"id":"bob"}"#));
    for (outcome, id) in [(a, "alice"), (b, "bob")] {
        let (status, body) = settle(&runtime, outcome).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "starts":1,"startupUsers":[null,null],"user":{"id":id},
            })
        );
    }
}

#[compio::test]
async fn asynchronous_startup_failure_is_causal_and_cached() {
    let runtime = runtime(
        r#"
        await new Promise(resolve => setTimeout(resolve, 10));
        throw new Error('startup-fixture-failed');
        export default { fetch() { return new Response('must not run'); } };
    "#,
        RuntimeLimits::default(),
    );
    let first = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(first.0, 500, "{}", first.1);
    assert!(first.1.contains("startup-fixture-failed"), "{}", first.1);
    assert_eq!(settle(&runtime, request(&runtime, None)).await, first);
}

#[compio::test]
async fn unresolved_startup_obeys_the_runtime_wall_deadline() {
    let runtime = runtime(
        r#"
        await new Promise(() => {});
        export default { fetch() { return new Response('must not run'); } };
    "#,
        RuntimeLimits {
            wall_timeout: Some(Duration::from_millis(30)),
            ..Default::default()
        },
    );
    let first = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(first.0, 500, "{}", first.1);
    assert!(first.1.contains("startup wall timeout"), "{}", first.1);
    assert_eq!(settle(&runtime, request(&runtime, None)).await, first);
}

#[compio::test]
async fn explicit_initialization_waits_and_shares_startup() {
    let runtime = runtime(
        r#"
        globalThis.startupRuns = (globalThis.startupRuns ?? 0) + 1;
        await new Promise(resolve => setTimeout(resolve, 10));
        export default { fetch() { return new Response(String(globalThis.startupRuns)); } };
    "#,
        RuntimeLimits::default(),
    );
    let env = EnvSnapshot::empty();
    let (a, b) = futures::join!(runtime.initialize(&env), runtime.initialize(&env));
    a.expect("initial startup");
    b.expect("shared startup");
    assert_eq!(
        settle(&runtime, request(&runtime, None)).await,
        (200, "1".into())
    );
}

#[compio::test]
async fn startup_waiter_receives_an_asynchronous_handler_result() {
    let runtime = runtime(
        r#"
        await new Promise(resolve => setTimeout(resolve, 10));
        export default { async fetch() {
            await new Promise(resolve => setTimeout(resolve, 10));
            return new Response('handler settled');
        } };
    "#,
        RuntimeLimits::default(),
    );
    assert_eq!(
        settle(&runtime, request(&runtime, None)).await,
        (200, "handler settled".into())
    );
}

#[compio::test]
async fn cancelled_waiter_settles_even_when_startup_has_no_native_work() {
    let runtime = runtime(
        r#"
        await new Promise(() => {});
        export default { fetch() { throw new Error('cancelled handler ran'); } };
    "#,
        RuntimeLimits::default(),
    );
    let FetchOutcome::Pending { rx, cancel } = request(&runtime, None) else {
        panic!("unresolved startup must queue the request");
    };
    runtime.start_pump();
    compio::time::sleep(Duration::from_millis(10)).await;
    cancel.cancel();
    let result = compio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("cancellation must wake startup waiter");
    let Err(error) = result else {
        panic!("cancelled request must fail");
    };
    assert!(
        error.message.contains("cancelled during startup"),
        "{error:?}"
    );
}

#[compio::test]
async fn dropping_runtime_releases_startup_waiters() {
    let runtime = runtime("await new Promise(() => {});", RuntimeLimits::default());
    let FetchOutcome::Pending { rx, .. } = request(&runtime, None) else {
        panic!("unresolved startup must queue the request");
    };
    drop(runtime);
    let result = compio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("runtime drop must settle startup waiter");
    let Err(error) = result else {
        panic!("disposed runtime must fail");
    };
    assert!(
        error.message.contains("Runtime dropped during startup"),
        "{error:?}"
    );
}

#[cfg(target_os = "linux")]
#[compio::test]
async fn cpu_limit_stops_creator_startup_and_caches_the_cause() {
    for source in [
        "while (true) {}",
        "await new Promise(resolve => setTimeout(resolve, 10)); while (true) {}",
    ] {
        let runtime = runtime(
            source,
            RuntimeLimits {
                cpu_limit: Some(Duration::from_millis(30)),
                ..Default::default()
            },
        );
        let first = settle(&runtime, request(&runtime, None)).await;
        assert_eq!(first.0, 500, "{}", first.1);
        assert!(first.1.contains("CPU time limit exceeded"), "{}", first.1);
        assert_eq!(settle(&runtime, request(&runtime, None)).await, first);
    }
}

struct CpuClockPlugin;

impl zeroship_runtime::NativePlugin for CpuClockPlugin {
    fn namespace(&self) -> &str {
        "clock"
    }
    fn register(&self, registrar: &mut zeroship_runtime::NativeRegistrar) {
        registrar.add("cpu", cpu_clock);
    }
}

fn cpu_clock(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set(
        v8::Number::new(
            scope,
            zeroship_runtime::init::thread_cpu_time().as_secs_f64(),
        )
        .into(),
    );
}

#[cfg(target_os = "linux")]
#[compio::test]
async fn startup_cpu_budget_is_shared_across_continuations() {
    zeroship_runtime::init_v8();
    let runtime = Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                import { env } from 'zeroship';
                for (let i = 0; i < 12; i++) {
                    const started = env.clock.cpu();
                    while (env.clock.cpu() - started < 0.005) {}
                    await new Promise(resolve => setTimeout(resolve, 0));
                }
                export default { fetch() { return new Response('exceeded cumulative startup budget'); } };
            "#.into(),
        }])
        .plugin(CpuClockPlugin)
        .limits(RuntimeLimits { cpu_limit: Some(Duration::from_millis(30)), ..Default::default() })
        .build();
    let (status, body) = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("CPU time limit exceeded"), "{body}");
}

struct PreparationPlugin {
    reject_prepare: bool,
    reject_finalization: bool,
}

impl zeroship_runtime::NativePlugin for PreparationPlugin {
    fn namespace(&self) -> &str {
        "startup"
    }

    fn register(&self, _registrar: &mut zeroship_runtime::NativeRegistrar) {}

    fn host_javascript_modules(&self) -> &'static [zeroship_runtime::plugin::JavaScriptModule] {
        &[zeroship_runtime::plugin::JavaScriptModule {
            specifier: "zeroship:startup/adapter",
            source: r#"
                export async function prepare(namespace, reject) {
                    await new Promise(resolve => setTimeout(resolve, 10));
                    if (reject) throw new Error('adapter preparation rejected');
                    namespace.prepared = true;
                }
            "#,
        }]
    }

    fn prepare_runtime<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        namespace: v8::Local<'s, v8::Object>,
        _descriptor: Option<&serde_json::Value>,
    ) -> Result<Option<v8::Global<v8::Promise>>, String> {
        let reject = v8::Boolean::new(scope, self.reject_prepare);
        zeroship_runtime::modules::invoke_module_export(
            scope,
            "zeroship:startup/adapter",
            "prepare",
            &[namespace.into(), reject.into()],
        )
        .map(Some)
    }

    fn finalize_runtime<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        namespace: v8::Local<'s, v8::Object>,
        _descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        if self.reject_finalization {
            return Err("adapter finalization rejected".into());
        }
        let key = v8::String::new(scope, "finalized").unwrap();
        let value = v8::Boolean::new(scope, true);
        namespace.set(scope, key.into(), value.into());
        Ok(())
    }
}

fn prepared_runtime(reject_prepare: bool, reject_finalization: bool) -> Runtime {
    zeroship_runtime::init_v8();
    Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
                import { env } from 'zeroship';
                globalThis.creatorEvaluated = true;
                const preparedDuringEvaluation = env.startup.prepared === true;
                export default { fetch() {
                    return Response.json({ preparedDuringEvaluation, finalized: env.startup.finalized });
                } };
            "#.into(),
        }])
        .plugin(PreparationPlugin { reject_prepare, reject_finalization })
        .build()
}

#[compio::test]
async fn adapter_preparation_precedes_creator_and_finalization_precedes_dispatch() {
    let runtime = prepared_runtime(false, false);
    let (status, body) = settle(&runtime, request(&runtime, None)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({
            "preparedDuringEvaluation": true, "finalized": true,
        })
    );
}

#[compio::test]
async fn adapter_failures_reject_startup_without_publishing_handlers() {
    for (prepare, finalize, cause) in [
        (true, false, "adapter preparation rejected"),
        (false, true, "adapter finalization rejected"),
    ] {
        let runtime = prepared_runtime(prepare, finalize);
        let first = settle(&runtime, request(&runtime, None)).await;
        assert_eq!(first.0, 500, "{}", first.1);
        assert!(first.1.contains(cause), "{}", first.1);
        assert_eq!(settle(&runtime, request(&runtime, None)).await, first);
        let creator_evaluated = runtime.with_scope(|scope| {
            let global = scope.get_current_context().global(scope);
            let key = v8::String::new(scope, "creatorEvaluated").unwrap();
            global.get(scope, key.into()).unwrap().is_true()
        });
        assert_eq!(creator_evaluated, !prepare);
    }
}
