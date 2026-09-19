//! Native startup owns policy declarations, snapshots and finalization.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::connection::ConnectionFactory;
use zeroship_data_orm::protection::mask_policy::install_mask_policy;
use zeroship_data_v8::service::{DbService, DbServiceConfig};
use zeroship_runtime::{
    CancelFlag, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

fn binding() -> DbBinding {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let app = format!(
        "app_startup_policy_{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    crate::tests::fixtures::harness_binding_at_deploy(&app, "startup_policy_fixture")
}

fn runtime(source: &str, binding: &DbBinding, url: &str) -> Runtime {
    let plugin = DbService::new(DbServiceConfig {
        app_bindings: Default::default(),
        project_keys: Default::default(),
        connection: ConnectionFactory::for_url(url).unwrap(),
        cdc_relay: None,
        meter: None,
    })
    .unwrap()
    .plugin();
    let runtime = Runtime::builder()
        .env_vars(HashMap::from([
            ("APP_ID".into(), binding.app_id().into()),
            ("ZEROSHIP_DEPLOY_ID".into(), binding.deploy_token().into()),
        ]))
        .plugins(vec![plugin])
        .modules(vec![
            ModuleEntry {
                specifier: "index.js".into(),
                source: source.into(),
            },
            ModuleEntry {
                specifier: "@zeroship/db".into(),
                source: include_str!("../../../../packages/db/dist/index.js").into(),
            },
            ModuleEntry {
                specifier: "late-policy.js".into(),
                source: r#"
                    import { defineMaskPolicy } from '@zeroship/db';
                    defineMaskPolicy({support: ['phi']});
                    export const loaded = true;
                "#
                .into(),
            },
        ])
        .build();
    runtime.exit_isolate();
    runtime
}

const UNUSED_DATABASE: &str = "postgres://unused:unused@127.0.0.1:1/unused";

async fn initialize(runtime: &Runtime) -> Result<(), String> {
    compio::time::timeout(
        Duration::from_secs(5),
        runtime.initialize(&EnvSnapshot::empty()),
    )
    .await
    .expect("startup deadline")
}

async fn dispatch(runtime: &Runtime, path: &str) -> (u16, serde_json::Value) {
    runtime.enter_isolate();
    let outcome = runtime.call_fetch_handler(
        "POST",
        &format!("http://localhost{path}"),
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    runtime.exit_isolate();
    let (status, body) = match outcome {
        FetchOutcome::Response { status, body, .. } => (status, body),
        FetchOutcome::Pending { rx, .. } => {
            runtime.start_pump();
            match compio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("response deadline")
                .expect("response channel")
            {
                SettledFetch::Response { status, body, .. } => (status, body),
                _ => panic!("expected a buffered response"),
            }
        }
        _ => panic!("expected a buffered response"),
    };
    (
        status,
        serde_json::from_slice(&body).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(&body).into_owned())
        }),
    )
}

fn assert_policy(binding: &DbBinding, expected: zeroship_data_orm::value::Value) {
    // Reject a conflicting value first: an absent policy must fail the test.
    let error = install_mask_policy(
        binding,
        zeroship_data_orm::value!({"fixture_control": ["pci"]}),
    )
    .expect_err("startup must already have installed a policy");
    assert!(
        error.to_string().contains("fixed for this deployment"),
        "{error}"
    );
    install_mask_policy(binding, expected)
        .expect("native startup retained the expected declaration");
}

#[compio::test]
async fn sdk_declarations_are_snapshotted_and_finalized_without_database_io() {
    let binding = binding();
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite:{}", dir.path().join("policy.sqlite").display());
    let runtime = runtime(
        r#"
        import { env } from 'zeroship';
        import { defineMaskPolicy } from '@zeroship/db';
        defineMaskPolicy({support: ['spi']});
        await new Promise(resolve => setTimeout(resolve, 1));
        const policy = {support: ['pii'], "auto": []};
        defineMaskPolicy(policy);
        policy.support.push('phi');
        policy.auto.push('pci');
        export default {
            tag: 'ready',
            fetchFast() {
                let code;
                try { defineMaskPolicy({}); code = 'accepted'; } catch (error) { code = error.code; }
                return JSON.stringify({tag: this.tag, code});
            },
            rpc: { inspect() { return typeof env.db.declareMaskPolicy; } },
        };
    "#,
        &binding,
        &url,
    );
    let opened = zeroship_data_orm::connection::backend_open_count();
    initialize(&runtime).await.unwrap();
    assert_policy(
        &binding,
        zeroship_data_orm::value!({"support": ["pii"], "auto": []}),
    );
    assert_eq!(zeroship_data_orm::connection::backend_open_count(), opened);
    assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    assert_eq!(
        dispatch(&runtime, "/fast").await,
        (
            200,
            serde_json::json!({"tag": "ready", "code": "MASK_POLICY_IMMUTABLE"})
        )
    );
    assert_eq!(
        dispatch(&runtime, "/__zeroship/v1/inspect").await,
        (200, serde_json::json!({"json": "function"}))
    );
}

#[compio::test]
async fn absent_declaration_seals_the_default() {
    let binding = binding();
    let runtime = runtime("export default { rpc: {} };", &binding, UNUSED_DATABASE);
    initialize(&runtime).await.unwrap();
    assert_policy(&binding, zeroship_data_orm::value!({}));
}

#[compio::test]
async fn invalid_declarations_preserve_the_prior_policy_and_original_getter_error() {
    let binding = binding();
    let runtime = runtime(
        r#"
        import { env } from 'zeroship';
        import { defineMaskPolicy } from '@zeroship/db';
        env.db.declareMaskPolicy({support: ['spi']});
        const failures = [];
        for (const policy of [null, [], 'policy', {support: 'pii'}, {support: ['typo']}, {support: [42]}]) {
            try { defineMaskPolicy(policy); failures.push('accepted'); }
            catch (error) { failures.push(error.code); }
        }
        const original = Object.assign(new Error('policy getter failed'), {code: 'GETTER_ERROR'});
        let sameError = false;
        try { defineMaskPolicy({get support() { throw original; }}); }
        catch (error) { sameError = error === original; }
        export default { rpc: { inspect() { return {failures, sameError}; } } };
    "#,
        &binding,
        UNUSED_DATABASE,
    );
    initialize(&runtime).await.unwrap();
    assert_eq!(
        dispatch(&runtime, "/__zeroship/v1/inspect").await,
        (
            200,
            serde_json::json!({"json": {
                "failures": ["INVALID_MASK_POLICY_SHAPE", "INVALID_MASK_POLICY_SHAPE",
                    "INVALID_MASK_POLICY_SHAPE", "INVALID_MASK_POLICY_SHAPE",
                    "INVALID_MASK_CLASSIFICATION", "INVALID_MASK_CLASSIFICATION"],
                "sameError": true,
            }})
        )
    );
    assert_policy(&binding, zeroship_data_orm::value!({"support": ["spi"]}));
}

#[compio::test]
async fn globals_handlers_and_lazy_imports_cannot_reopen_policy_declarations() {
    let binding = binding();
    let runtime = runtime(
        r#"
        import { env } from 'zeroship';
        import { defineMaskPolicy } from '@zeroship/db';
        const absentAtStartup = [
            globalThis.__zsDbPlatform, globalThis.__zsSchemaReady,
            globalThis.__zsRuntimeDescriptor, env.db.__platform,
        ].every(value => value === undefined);
        defineMaskPolicy({support: ['spi']});
        globalThis.__zsRuntimeDescriptor = {version: 2, collections: {}};
        globalThis.__zsSchemaReady = new Promise(() => {});
        globalThis.__zsDeferSchemaInstall = true;
        globalThis.__zsAllowDeferredSchemaInstall = true;
        globalThis[Symbol.for('@zeroship/db/MaskPolicyState')] = {pending: {}, sealed: false};
        async function inspect() {
            const codes = [];
            for (const declare of [defineMaskPolicy, policy => env.db.declareMaskPolicy(policy)]) {
                try { declare({support: ['phi']}); codes.push('accepted'); }
                catch (error) { codes.push(error.code); }
            }
            await new Promise(resolve => setTimeout(resolve, 1));
            try { defineMaskPolicy({support: ['phi']}); codes.push('accepted'); }
            catch (error) { codes.push(error.code); }
            try { await import('./late-policy.js'); codes.push('accepted'); }
            catch (error) { codes.push(error.code); }
            return {absentAtStartup, codes};
        }
        export default { rpc: {inspect}, async fetch() { return Response.json(await inspect()); } };
    "#,
        &binding,
        UNUSED_DATABASE,
    );
    initialize(&runtime).await.unwrap();
    let expected = serde_json::json!({"absentAtStartup": true,
        "codes": ["MASK_POLICY_IMMUTABLE", "MASK_POLICY_IMMUTABLE", "MASK_POLICY_IMMUTABLE", "MASK_POLICY_IMMUTABLE"]});
    assert_eq!(
        dispatch(&runtime, "/__zeroship/v1/inspect").await,
        (200, serde_json::json!({"json": expected.clone()}))
    );
    assert_eq!(dispatch(&runtime, "/fetch").await, (200, expected));
    assert_policy(&binding, zeroship_data_orm::value!({"support": ["spi"]}));
}

#[compio::test]
async fn failed_startup_closes_declarations_and_never_publishes_handlers() {
    let binding = binding();
    let runtime = runtime(
        r#"
        import { defineMaskPolicy } from '@zeroship/db';
        globalThis.tryLatePolicy = () => {
            try { defineMaskPolicy({support: ['phi']}); return 'accepted'; }
            catch (error) { return error.code; }
        };
        await Promise.resolve();
        throw new Error('policy-startup-failure');
        export default {fetch() {return new Response('published');}, rpc: {inspect() {return 'published';}}};
    "#,
        &binding,
        UNUSED_DATABASE,
    );
    let error = initialize(&runtime).await.unwrap_err();
    assert!(error.contains("policy-startup-failure"), "{error}");
    runtime.with_scope(|scope| {
        let source = v8::String::new(scope, "tryLatePolicy()").unwrap();
        let script = v8::Script::compile(scope, source, None).unwrap();
        assert_eq!(
            script.run(scope).unwrap().to_rust_string_lossy(scope),
            "MASK_POLICY_IMMUTABLE"
        );
    });
    assert_eq!(initialize(&runtime).await.unwrap_err(), error);
    for path in ["/fetch", "/__zeroship/v1/inspect"] {
        let (status, body) = dispatch(&runtime, path).await;
        assert_eq!(status, 500, "{body}");
        assert!(
            body.to_string().contains("policy-startup-failure"),
            "{body}"
        );
    }
}

#[compio::test]
async fn finalization_rejects_a_conflicting_binding_and_preserves_older_deployments() {
    let binding = binding();
    let original = runtime(
        r#"
        import {defineMaskPolicy} from '@zeroship/db';
        defineMaskPolicy({support: ['spi']});
        export default {rpc: {inspect() {return 'original';}}};
    "#,
        &binding,
        UNUSED_DATABASE,
    );
    initialize(&original).await.unwrap();
    let changed_source = r#"
        import {defineMaskPolicy} from '@zeroship/db';
        defineMaskPolicy({support: ['pii']});
        export default {fetchFast() {return 'published';}, rpc: {inspect() {return 'replacement';}}};
    "#;
    let conflicting = runtime(changed_source, &binding, UNUSED_DATABASE);
    let error = initialize(&conflicting).await.unwrap_err();
    assert!(error.contains("fixed for this deployment"), "{error}");
    for path in ["/fetch", "/__zeroship/v1/inspect"] {
        assert_eq!(dispatch(&conflicting, path).await.0, 500);
    }
    let next_binding =
        crate::tests::fixtures::harness_binding_at_deploy(binding.app_id(), "next_deploy");
    let next = runtime(changed_source, &next_binding, UNUSED_DATABASE);
    initialize(&next).await.unwrap();
    assert_policy(
        &next_binding,
        zeroship_data_orm::value!({"support": ["pii"]}),
    );
    assert_policy(&binding, zeroship_data_orm::value!({"support": ["spi"]}));
    assert_eq!(
        dispatch(&original, "/__zeroship/v1/inspect").await,
        (200, serde_json::json!({"json": "original"}))
    );
}

#[compio::test]
async fn startup_unmask_refuses_before_io_even_when_the_promise_is_observed_after_finalization() {
    let binding = binding();
    let runtime = runtime(
        r#"
        import {env} from 'zeroship';
        import {defineMaskPolicy} from '@zeroship/db';
        const users = env.db.collection('users');
        const capture = promise => promise.then(() => 'accepted', error => error.code);
        const pending = [
            capture(users.unmaskField('usr_fixture', 'ssn', {})),
            capture(users.bulkUnmask([{rowPk: 'usr_fixture', columns: ['ssn']}], {})),
            capture(users.find({}, {unmask: ['ssn']})),
            capture(fixtureMasked.unmask()),
            capture(fixtureMasked.unmask(['ssn'])),
            capture(fixtureMasked.canUnmask()),
        ];
        const first = await pending[0];
        defineMaskPolicy({support: ['spi'], auto: []});
        export default {rpc: {async inspect() {return {first, codes: await Promise.all(pending)};}}};
    "#,
        &binding,
        UNUSED_DATABASE,
    );
    runtime.with_scope(|scope| {
        let masked = zeroship_data_v8::v8_classes::masked_value::mint_masked_value(
            scope,
            binding.clone(),
            "users".into(),
            "usr_fixture".into(),
            "ssn".into(),
            "spi".into(),
            "***-**-6789".into(),
        )
        .unwrap();
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "fixtureMasked").unwrap();
        global.set(scope, key.into(), masked.into());
    });
    let opened = zeroship_data_orm::connection::backend_open_count();
    initialize(&runtime).await.unwrap();
    let (status, body) = dispatch(&runtime, "/__zeroship/v1/inspect").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["json"]["first"], "database_startup_pending");
    let codes = body["json"]["codes"].as_array().unwrap();
    assert_eq!(codes.len(), 6);
    assert!(
        codes.iter().all(|code| code == "database_startup_pending"),
        "{body}"
    );
    assert_eq!(zeroship_data_orm::connection::backend_open_count(), opened);
    assert_policy(
        &binding,
        zeroship_data_orm::value!({"support": ["spi"], "auto": []}),
    );
}
