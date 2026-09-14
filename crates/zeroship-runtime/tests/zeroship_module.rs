//! Native module exports retain host identity and continuation-owned capabilities.

use std::time::Duration;

use serde_json::{Value, json};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch};

struct Probe;

impl NativePlugin for Probe {
    fn namespace(&self) -> &str {
        "probe"
    }

    fn register(&self, registrar: &mut NativeRegistrar) {
        registrar.add("kind", probe_kind);
        registrar.add("waiters", probe_waiters);
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

fn probe_waiters(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state = scope
        .get_slot::<zeroship_runtime::state::SharedState>()
        .unwrap();
    let count: usize = state
        .borrow()
        .wait_until_by_request
        .values()
        .map(Vec::len)
        .sum();
    rv.set_double(count as f64);
}

async fn response(source: &str, path: &str) -> Value {
    zeroship_runtime::init_v8();
    let runtime = Runtime::builder()
        .plugin(Probe)
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: source.into(),
        }])
        .build();
    runtime.exit_isolate();
    runtime.initialize(&EnvSnapshot::empty()).await.unwrap();
    runtime.start_pump();
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "POST",
        &format!("http://localhost/{path}"),
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    let (status, body) = match outcome {
        FetchOutcome::Response { status, body, .. } => (status, body),
        FetchOutcome::Pending { rx, .. } => {
            match compio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                SettledFetch::Response { status, body, .. } => (status, body),
                _ => panic!("expected buffered response"),
            }
        }
        _ => panic!("expected buffered response"),
    };
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    serde_json::from_slice(&body).unwrap()
}

async fn rpc(source: &str) -> Value {
    response(
        &format!(
            r#"{source}
        test.config = {{ kind: 'action' }};
        export default {{ rpc: {{ test }} }};
    "#
        ),
        "__zeroship/v1/test",
    )
    .await["json"]
        .clone()
}

#[compio::test]
async fn composition_retains_nested_and_sibling_kinds_across_native_awaits() {
    let value = rpc(r#"
        import { env, runQuery, runMutation } from 'zeroship';
        import { AsyncLocalStorage } from 'node:async_hooks';
        const als = new AsyncLocalStorage();
        const pause = () => new Promise(resolve => setTimeout(resolve, 5));
        async function test() {
            return als.run('caller', async () => {
                const caller = env.probe.kind();
                const query = runQuery(async () => {
                    const before = env.probe.kind();
                    const nested = await runMutation(async () => {
                        await pause();
                        return env.probe.kind();
                    });
                    await pause();
                    return [before, nested, env.probe.kind(), als.getStore()];
                });
                const restored = env.probe.kind();
                const mutation = runMutation(async () => {
                    await pause();
                    return [env.probe.kind(), als.getStore()];
                });
                return { caller, restored, query: await query,
                    mutation: await mutation, after: env.probe.kind() };
            });
        }
    "#)
    .await;
    assert_eq!(
        value,
        json!({
            "caller": "Some(Action)", "restored": "Some(Action)",
            "query": ["Some(Query)", "Some(Mutation)", "Some(Query)", "caller"],
            "mutation": ["Some(Mutation)", "caller"], "after": "Some(Action)",
        })
    );
}

#[compio::test]
async fn composition_adopts_thenables_and_rejections_without_losing_the_caller() {
    let value = rpc(r#"
        import { env, runQuery, runMutation } from 'zeroship';
        async function test() {
            const marker = new Error('original');
            const rejects = [];
            for (const fn of [() => { throw marker; }, () => Promise.reject(marker),
                () => ({ get then() { throw marker; } })]) {
                try { await runQuery(fn); } catch (error) { rejects.push(error === marker); }
            }
            let invalid;
            const bad = runMutation(null);
            try { await bad; } catch (error) { invalid = error instanceof TypeError; }
            const thenable = await runQuery(() => ({
                then(resolve) { resolve(env.probe.kind()); }
            }));
            const input = { identity: true };
            const output = runMutation(function(value) {
                return [this === undefined, value === input, env.probe.kind()];
            }, input);
            return { rejects, invalid, promise: bad instanceof Promise && output instanceof Promise,
                thenable, output: await output, after: env.probe.kind() };
        }
    "#)
    .await;
    assert_eq!(
        value,
        json!({
            "rejects": [true, true, true], "invalid": true, "promise": true,
            "thenable": "Some(Query)", "output": [true, true, "Some(Mutation)"],
            "after": "Some(Action)",
        })
    );
}

#[compio::test]
async fn native_exports_keep_request_context_when_globals_are_replaced() {
    let value = rpc(r#"
        import * as zs from 'zeroship';
        async function test() {
            const ctx = zs.getRequestContext();
            const legacyGlobalAbsent = typeof globalThis.__zeroshipGetRpcCtx === 'undefined';
            globalThis.__zeroshipGetRpcCtx = () => ({ user: 'forged' });
            globalThis.__zsEnterKind = () => { throw new Error('forged enter'); };
            globalThis.__zsExitKind = () => { throw new Error('forged exit'); };
            await new Promise(resolve => setTimeout(resolve, 5));
            const nested = await zs.runQuery(async () => {
                await Promise.resolve();
                return zs.getRequestContext() === ctx;
            });
            const dynamic = await import('zeroship');
            return { same: dynamic === zs, nested, context: zs.getRequestContext() === ctx,
                legacyGlobalAbsent,
                fields: [zs.currentUser() === ctx.user, zs.currentRequestId() === ctx.requestId,
                    zs.currentTraceId() === ctx.traceId, zs.currentSignal() === ctx.signal,
                    zs.currentHeaders() === ctx.headers,
                    zs.currentIdempotencyKey() === ctx.idempotencyKey] };
        }
    "#)
    .await;
    assert_eq!(
        value,
        json!({
            "same": true, "nested": true, "context": true, "legacyGlobalAbsent": true,
            "fields": [true, true, true, true, true, true],
        })
    );
}

#[compio::test]
async fn dynamic_only_exports_use_host_env_and_request() {
    let value = response(
        r#"
        globalThis.__zs_env = () => ({ forged: true });
        globalThis.__zs_get_request = () => 'forged';
        globalThis.__zs_wait_until = () => { throw new Error('forged wait'); };
        export default { async fetch(request, env) {
            const zs = await import('zeroship');
            await new Promise(resolve => setTimeout(resolve, 5));
            const before = env.probe.waiters();
            zs.waitUntil(Promise.resolve());
            return Response.json({ same: zs.env === env, frozen: Object.isFrozen(zs.env),
                request: zs.getRequest() === request, context: zs.getRequestContext() === undefined,
                before, after: env.probe.waiters() });
        }};
    "#,
        "fetch",
    )
    .await;
    assert_eq!(
        value,
        json!({"same": true, "frozen": true, "request": true, "context": true,
            "before": 0, "after": 1})
    );
}

#[compio::test]
async fn composition_enforces_native_fetch_refusal_after_await() {
    let value = rpc(r#"
        import { runQuery, runMutation } from 'zeroship';
        async function test() {
            const results = [];
            for (const run of [runQuery, runMutation]) {
                try {
                    await run(async () => {
                        await new Promise(resolve => setTimeout(resolve, 5));
                        await fetch('http://localhost:1/never');
                    });
                    results.push('unexpected success');
                } catch (error) { results.push(error.code); }
            }
            return results;
        }
    "#)
    .await;
    assert_eq!(
        value,
        json!(["capability_violation", "capability_violation"])
    );
}

#[compio::test]
async fn request_accessors_reject_outside_an_invocation() {
    let value = response(r#"
        import * as zs from 'zeroship';
        const errors = [];
        for (const name of ['getRequest', 'currentUser', 'currentRequestId',
            'currentTraceId', 'currentSignal', 'currentHeaders', 'currentIdempotencyKey']) {
            try { zs[name](); errors.push('unexpected success'); }
            catch (error) { errors.push(error.message.includes(name) && error.message.includes('outside')); }
        }
        const outside = zs.getRequestContext() === undefined;
        export default { fetch() { return Response.json({ errors, outside }); } };
    "#, "fetch").await;
    assert_eq!(
        value,
        json!({"errors": [true, true, true, true, true, true, true], "outside": true})
    );
}

#[compio::test]
async fn vite_module_runner_uses_the_same_native_exports() {
    let plugin_root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdks/vite-plugin");
    let built = std::process::Command::new("node")
        .current_dir(plugin_root)
        .args([
            "--input-type=module",
            "--eval",
            r#"
            import { buildSync } from 'esbuild';
            const output = buildSync({
                entryPoints: ['test/fixtures/runtime-module-runner.ts'],
                bundle: true, write: false, format: 'esm', platform: 'neutral',
                external: ['zeroship'],
            });
            process.stdout.write(output.outputFiles[0].text);
        "#,
        ])
        .output()
        .expect("Node and installed SDK build dependencies are required");
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(
        !built.stdout.is_empty(),
        "the fixture must contain executable code"
    );
    let source = String::from_utf8(built.stdout).unwrap();
    let value = response(&source, "fetch").await;
    assert_eq!(
        value,
        json!({
            "sameEnv": true, "sameFunction": true, "sameNamespace": true,
            "kind": "Some(Query)", "outside": "None", "rejected": true,
        })
    );
}
