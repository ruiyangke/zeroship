//! Descriptor validation and native plugin preparation before creator startup.
//!
//! The host binds only the manifest descriptor and passes the live namespace
//! to each preparation hook. Schema-less apps and hosts without a DB plugin
//! still start. The DB fixture supplies its own installer module to observe
//! the descriptor handoff without substituting creator-owned module sources.

use crate::common;
use futures::FutureExt;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, init_v8};
use zeroship_runtime::{NativePlugin, NativeRegistrar};
use zeroship_runtime::plugin::JavaScriptModule;

struct DummyDbPlugin;

impl NativePlugin for DummyDbPlugin {
    fn namespace(&self) -> &str {
        "db"
    }

    fn name(&self) -> &str {
        "dummy-db"
    }

    fn javascript_modules(&self) -> &'static [JavaScriptModule] {
        &[JavaScriptModule {
            specifier: "zeroship:db/internal",
            source: r#"
export function installSchema(env, descriptor, options) {
    if (env.__dummyNoop() !== undefined) throw new Error('installer received a different DB handle');
    globalThis.__zsCapturedSchema = JSON.stringify({
        keys: Object.keys(descriptor.collections),
        optionKeys: options ? Object.keys(options) : [],
        hasDeclaredSchemas: !!(options && Object.prototype.hasOwnProperty.call(options, "declaredSchemas")),
    });
    return { collections: {} };
}
"#,
        }]
    }

    fn prepare_runtime<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        namespace: v8::Local<'s, v8::Object>,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<Option<v8::Global<v8::Promise>>, String> {
        let Some(descriptor) = descriptor else { return Ok(None); };
        let json = v8::String::new(scope, &descriptor.to_string()).unwrap();
        let descriptor = v8::json::parse(scope, json).unwrap();
        zeroship_runtime::modules::invoke_module_export(
            scope, "zeroship:db/internal", "installSchema", &[namespace.into(), descriptor],
        ).map(Some)
    }

    fn register(&self, r: &mut NativeRegistrar) {
        // Register one trivial op so the runtime materializes a non-null
        // `env.db` object. native plugin preparation keys on
        // `env.db != null` (the real DbPlugin always populates env.db); an
        // empty register() leaves env.db absent and the sentinel skips
        // install, which is unfaithful to "DB plugin present".
        r.add("__dummyNoop", dummy_db_noop);
    }
}

struct DescriptorProbePlugin;

impl NativePlugin for DescriptorProbePlugin {
    fn namespace(&self) -> &str {
        "descriptor_probe"
    }

    fn name(&self) -> &str {
        "descriptor-probe"
    }

    fn register(&self, _r: &mut NativeRegistrar) {}

    fn bind_runtime_descriptor<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        _app_id: &str,
        namespace: v8::Local<'s, v8::Object>,
        descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        let observed = descriptor
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_else(|| "none".to_string());
        let global = scope.get_current_context().global(scope);
        let key = v8::String::new(scope, "__zsDescriptorHookObserved")
            .ok_or_else(|| "failed to allocate probe key".to_string())?;
        let value = v8::String::new(scope, &observed)
            .ok_or_else(|| "failed to allocate probe value".to_string())?;
        global.set(scope, key.into(), value.into());
        let bound_key = v8::String::new(scope, "descriptorBound")
            .ok_or_else(|| "failed to allocate namespace probe key".to_string())?;
        let bound = v8::Boolean::new(scope, true);
        namespace.define_own_property(
            scope,
            bound_key.into(),
            bound.into(),
            v8::PropertyAttribute::READ_ONLY,
        );
        Ok(())
    }
}

/// No-op native op for [`DummyDbPlugin`]. Free function so it coerces to
/// `v8::FunctionCallback` without a capturing closure.
fn dummy_db_noop(
    _scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set_undefined();
}

fn descriptor_hook_observed(runtime_descriptor: Option<String>) -> String {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
const observedAtModuleEvaluation = globalThis.__zsDescriptorHookObserved ?? "missing";
if (globalThis.__zs_env?.()?.descriptor_probe?.descriptorBound !== true) {
    throw new Error("descriptor hook did not receive the published plugin namespace");
}
export default {
    fetch() { return new Response(observedAtModuleEvaluation); },
};
"#
        .into(),
    }];
    let runtime = Runtime::builder()
        .modules(modules)
        .plugin(DescriptorProbePlugin)
        .runtime_descriptor(runtime_descriptor)
        .build();
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://localhost/",
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200, "descriptor probe must boot");
            String::from_utf8(body).expect("probe response is UTF-8")
        }
        _ => panic!("expected synchronous descriptor probe response"),
    }
}

#[test]
fn native_descriptor_hook_receives_validated_descriptor_before_module_evaluation() {
    let descriptor = r#"{"version":2,"collections":{"posts":{"fields":{"title":{"type":"string"}},"options":{"softDelete":false,"versioning":false},"indexes":[]}}}"#;
    let observed = descriptor_hook_observed(Some(descriptor.to_string()));
    let expected: serde_json::Value = serde_json::from_str(descriptor).unwrap();
    let observed: serde_json::Value = serde_json::from_str(&observed).unwrap();
    assert_eq!(observed, expected);
}

#[test]
fn native_descriptor_hook_receives_none_before_schema_less_module_evaluation() {
    assert_eq!(descriptor_hook_observed(None), "none");
}

/// Dispatch the startup probe through the native procedure dictionary.
fn dispatch_probe(user_src: &str, procs_block: &str, method: &str) -> Result<String, String> {
    init_v8();
    let src = format!("{user_src}\nexport default {{ rpc: {procs_block} }};");
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: src,
    }];

    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url = format!("http://localhost/__zeroship/v1/{method}");
    let outcome = runtime.call_fetch_handler(
        "POST",
        &url,
        &[("content-type".into(), "application/json".into())],
        "",
        &env,
        ctx,
    );
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            let body = String::from_utf8_lossy(&body).into_owned();
            if (200..300).contains(&status) {
                Ok(body)
            } else {
                Err(body)
            }
        }
        _ => Err("unexpected non-sync outcome".to_string()),
    }
}

#[test]
fn init_script_no_ops_without_db_plugin() {
    // When the DB plugin isn't loaded, native startup must not request its SDK
    // adapter module and host entry evaluation must finish cleanly.
    //
    // We assert the boot path made it through end-to-end: the worker
    // resolved `default.fetch`, served a request, and returned the
    // probe's value. A regression where the dynamic import threw and
    // tore down evaluation would surface here as a dispatch-side
    // error, not the happy `"json":"ok"`.
    let body = dispatch_probe(
        r#"
        export function probe() { return "ok"; }
        "#,
        "{ probe }",
        "probe",
    )
    .unwrap();
    assert!(body.contains(r#""json":"ok""#), "got: {body}");
}

#[test]
fn init_script_no_ops_when_runtime_descriptor_missing() {
    // Even with an `env.db` namespace present, absence of a runtime
    // descriptor short-circuits the init script before the dynamic import.
    // The test's synthetic-entry shim emits its own `default = { fetch, rpc }`,
    // so host entry evaluation finishes cleanly without trying to import
    // `@zeroship/db`.
    init_v8();
    let user_src = r#"
function readGate() { return globalThis.__zsCapturedSchema ?? null; }
const _procedures = { readGate };
async function _zsFetch(request) {
    const url = new URL(request.url);
    const id = decodeURIComponent(url.pathname.slice("/__zeroship/v1/".length));
    const fn = _procedures[id];
    const result = await fn(undefined);
    return new Response(JSON.stringify({ json: result === undefined ? null : result }), {
        status: 200, headers: { "content-type": "application/json" },
    });
}
export default {
    fetch: _zsFetch,
    rpc: _procedures,
    // No schema key — discovery should short-circuit.
};
"#;
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: user_src.into(),
    }];
    let runtime = Runtime::builder()
        .modules(modules)
        .plugin(DummyDbPlugin)
        .build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/readGate",
        &[("content-type".into(), "application/json".into())],
        "",
        &env,
        ctx,
    );
    let body = match outcome {
        FetchOutcome::Response { status, body, .. } => {
            let body = String::from_utf8_lossy(&body).into_owned();
            assert!(
                (200..300).contains(&status),
                "non-2xx: status={status} body={body}"
            );
            body
        }
        _ => panic!("expected sync Response"),
    };
    // The init script must not have called `installSchema` — no
    // `__zsCapturedSchema` exists because the user has no schema. The
    // probe returns `null` (JSON-encoded).
    assert!(
        body.contains(r#""json":null"#),
        "expected null, got: {body}"
    );
}

#[test]
fn init_script_runs_install_schema_when_descriptor_and_db_plugin_present() {
    // Drive the descriptor-only positive path: the runtime provides an `env.db`
    // namespace and stamps a bundled RuntimeSchemaDescriptor onto the isolate.
    // The stub `installSchema` captures the schema value it was handed; we read
    // it back through a probe handler.
    init_v8();

    let user_src = r#"
import * as _zsUser from "./__user__.js";  // not used, just to anchor
const _procedures = { readCaptured };
function readCaptured() {
    return globalThis.__zsCapturedSchema ?? null;
}
async function _zsRpcAndRespond(name, input) {
    const fn = _procedures[name];
    if (typeof fn !== "function") {
        return new Response(JSON.stringify({ message: "Method not found", name: "Error", code: "NOT_FOUND" }), { status: 404 });
    }
    const result = await fn(input);
    return new Response(JSON.stringify({ json: result === undefined ? null : result }), {
        status: 200, headers: { "content-type": "application/json" },
    });
}
async function _zsFetch(request) {
    const url = new URL(request.url);
    const id = decodeURIComponent(url.pathname.slice("/__zeroship/v1/".length));
    return await _zsRpcAndRespond(id, undefined);
}
export default {
    fetch: _zsFetch,
    rpc: _procedures,
};
"#;

    let modules = vec![ModuleEntry { specifier: "index.js".into(), source: user_src.into() }];

    let descriptor = r#"{"version":2,"collections":{"posts":{"fields":{"id":{"type":"id","idPrefix":"post"},"title":{"type":"string","required":true}},"options":{"softDelete":false,"versioning":false,"strictness":"strict"},"indexes":[]}}}"#;

    let runtime = Runtime::builder()
        .modules(modules)
        .plugin(DummyDbPlugin)
        .runtime_descriptor(Some(descriptor.to_string()))
        .build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/readCaptured",
        &[("content-type".into(), "application/json".into())],
        "",
        &env,
        ctx,
    );
    let body = match outcome {
        FetchOutcome::Response { status, body, .. } => {
            let body = String::from_utf8_lossy(&body).into_owned();
            assert!(
                (200..300).contains(&status),
                "non-2xx: status={status} body={body}"
            );
            body
        }
        _ => panic!("expected sync Response"),
    };
    assert!(
        body.contains(r#"\"keys\":[\"posts\"]"#),
        "expected captured descriptor schema with 'posts' key, got: {body}"
    );
}

#[test]
fn init_script_sources_schema_from_runtime_descriptor_when_present() {
    // **Migration-first cutover (P4b).** When the deploy carries a bundled
    // `RuntimeSchemaDescriptor` (v2 `{ fields, options, indexes }` per collection), the
    // worker stamps it onto the runtime via `RuntimeBuilder::runtime_descriptor`.
    // Native plugin preparation must then install the schema FROM the
    // descriptor — IGNORING `user.default.schema`.
    //
    // We give the user a throwing `default.schema` getter and inject a descriptor
    // (`{ posts }`). The runtime entry must not read the declared schema at all;
    // it should install from the descriptor and pass no declared side channel.
    //
    // RED before S3: runtime-entry still read `default.schema` to build the
    // declared-schema fallback, so module init trips the throwing getter.
    init_v8();

    let user_src = r#"
import * as _zsUser from "./__user__.js";  // anchor
const _procedures = { readCaptured };
function readCaptured() {
    return globalThis.__zsCapturedSchema ?? null;
}
async function _zsRpcAndRespond(name, input) {
    const fn = _procedures[name];
    if (typeof fn !== "function") {
        return new Response(JSON.stringify({ message: "Method not found", name: "Error", code: "NOT_FOUND" }), { status: 404 });
    }
    const result = await fn(input);
    return new Response(JSON.stringify({ json: result === undefined ? null : result }), {
        status: 200, headers: { "content-type": "application/json" },
    });
}
async function _zsFetch(request) {
    const url = new URL(request.url);
    const id = decodeURIComponent(url.pathname.slice("/__zeroship/v1/".length));
    return await _zsRpcAndRespond(id, undefined);
}
const defaultExport = {
    fetch: _zsFetch,
    rpc: _procedures,
};
Object.defineProperty(defaultExport, "schema", {
    get() { throw new Error("default.schema must not be read when a descriptor is present"); },
});
export default defaultExport;
"#;

    let modules = vec![ModuleEntry { specifier: "index.js".into(), source: user_src.into() }];

    // The bundled descriptor: a DIFFERENT collection (`posts`) than the
    // declared `todos`, carrying platform system fields the fold materialised.
    let descriptor = r#"{"version":2,"collections":{"posts":{"fields":{"id":{"type":"id","idPrefix":"post"},"title":{"type":"string","required":true},"created_at":{"type":"date"}},"options":{"softDelete":false,"versioning":false,"strictness":"strict"},"indexes":[]}}}"#;

    let runtime = Runtime::builder()
        .modules(modules)
        .plugin(DummyDbPlugin)
        .runtime_descriptor(Some(descriptor.to_string()))
        .build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/readCaptured",
        &[("content-type".into(), "application/json".into())],
        "",
        &env,
        ctx,
    );
    let body = match outcome {
        FetchOutcome::Response { status, body, .. } => {
            let body = String::from_utf8_lossy(&body).into_owned();
            assert!(
                (200..300).contains(&status),
                "non-2xx: status={status} body={body}"
            );
            body
        }
        _ => panic!("expected sync Response"),
    };
    // The installer receives the host descriptor directly.
    assert!(
        body.contains(r#"\"keys\":[\"posts\"]"#),
        "expected installSchema sourced from the descriptor (posts), got: {body}"
    );
    assert!(
        body.contains(r#"\"optionKeys\":[]"#),
        "expected native preparation to pass no extra options, got: {body}"
    );
    assert!(
        body.contains(r#"\"hasDeclaredSchemas\":false"#),
        "declaredSchemas must not be passed to installSchema, got: {body}"
    );
}

#[test]
fn init_script_does_not_fallback_to_default_schema_without_descriptor() {
    // **Migration-first cutover (P5 S3).** An app that ships no descriptor is
    // treated as schema-less by the runtime entry. Even if a stale
    // `default.schema` exists, the runtime host must not read it or import
    // `zeroship:db/internal`.
    init_v8();

    let user_src = r#"
import * as _zsUser from "./__user__.js";  // anchor
const _procedures = { readCaptured };
function readCaptured() {
    return globalThis.__zsCapturedSchema ?? null;
}
async function _zsRpcAndRespond(name, input) {
    const fn = _procedures[name];
    if (typeof fn !== "function") {
        return new Response(JSON.stringify({ message: "Method not found", name: "Error", code: "NOT_FOUND" }), { status: 404 });
    }
    const result = await fn(input);
    return new Response(JSON.stringify({ json: result === undefined ? null : result }), {
        status: 200, headers: { "content-type": "application/json" },
    });
}
async function _zsFetch(request) {
    const url = new URL(request.url);
    const id = decodeURIComponent(url.pathname.slice("/__zeroship/v1/".length));
    return await _zsRpcAndRespond(id, undefined);
}
export default {
    fetch: _zsFetch,
    rpc: _procedures,
    schema: { todos: { id: { type: "id" } } },
};
"#;
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: user_src.into(),
    }];

    // No descriptor on the builder — the global stays unset and the stale
    // default.schema must be ignored.
    let runtime = Runtime::builder()
        .modules(modules)
        .plugin(DummyDbPlugin)
        .runtime_descriptor(None)
        .build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/readCaptured",
        &[("content-type".into(), "application/json".into())],
        "",
        &env,
        ctx,
    );
    let body = match outcome {
        FetchOutcome::Response { status, body, .. } => {
            let body = String::from_utf8_lossy(&body).into_owned();
            assert!(
                (200..300).contains(&status),
                "non-2xx: status={status} body={body}"
            );
            body
        }
        _ => panic!("expected sync Response"),
    };
    assert!(
        body.contains(r#""json":null"#),
        "without a descriptor, native preparation must install nothing and ignore default.schema: {body}"
    );
}

#[test]
fn corrupt_runtime_descriptor_json_fails_isolate_init() {
    init_v8();

    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: "export default { fetch(){ return new Response('ok'); } }".into(),
    }];
    let runtime = Runtime::builder()
        .modules(modules)
        .runtime_descriptor(Some("{not json".to_string()))
        .build();

    let err = runtime
        .initialize(&EnvSnapshot::empty())
        .now_or_never().expect("invalid descriptor fails before async work")
        .expect_err("invalid descriptor JSON must fail isolate init");
    assert!(
        err.contains("manifest.runtime_descriptor is not valid JSON"),
        "error should name corrupt runtime descriptor JSON, got: {err}"
    );
}

#[test]
fn non_v2_runtime_descriptor_fails_isolate_init() {
    init_v8();

    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: "export default { fetch(){ return new Response('ok'); } }".into(),
    }];
    // A v1 descriptor is structurally valid and differs from v2 ONLY in the
    // version tag, so this is a control for the version gate specifically: if
    // the gate stopped discriminating, init would succeed and `expect_err`
    // would panic.
    let runtime = Runtime::builder()
        .modules(modules)
        .runtime_descriptor(Some(r#"{"version":1,"collections":{}}"#.to_string()))
        .build();

    let err = runtime
        .initialize(&EnvSnapshot::empty())
        .now_or_never().expect("invalid descriptor fails before async work")
        .expect_err("non-v2 descriptor must fail isolate init");
    assert!(
        err.contains("RuntimeSchemaDescriptor v2"),
        "error should name the required descriptor version, got: {err}"
    );
}

#[test]
fn host_entry_lacks_legacy_schema_init_symbols() {
    // Stage 4 cleanup: the synthetic SSR entry no longer publishes
    // `__zsSchemaInit`. The runtime host does not either: native plugins
    // receive the descriptor before module evaluation and the JavaScript
    // installer plants wrappers directly. Verify the legacy global stays
    // undefined throughout host entry evaluation.
    let body = dispatch_probe(
        r#"
        export function readInit() {
            return typeof globalThis.__zsSchemaInit;
        }
        "#,
        "{ readInit }",
        "readInit",
    )
    .unwrap();
    assert!(body.contains(r#""json":"undefined""#), "got: {body}");

    // And the dispatch baseline must still work — using the wrap-with-
    // legacy-shim helper from `common::dispatch`.
    let r = common::dispatch(
        common::m(r#"export function ping() { return "pong"; }"#),
        "ping",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("pong"), "got: {}", r.json);
}

#[test]
fn manifest_schema_path_global_no_longer_set() {
    // Stage 5c removed the `__zsManifestSchemaPath` injection from
    // init.rs. Verify the global stays undefined: user code (and the
    // legacy `db_init.js` gate before it was rewritten) saw it set
    // when the build passed `manifest_schema_path`. Now nobody sets
    // it. A regression that re-adds the injection would surface
    // here as a string typeof.
    let body = dispatch_probe(
        r#"
        export function readSchemaPath() {
            return typeof globalThis.__zsManifestSchemaPath;
        }
        "#,
        "{ readSchemaPath }",
        "readSchemaPath",
    )
    .unwrap();
    assert!(body.contains(r#""json":"undefined""#), "got: {body}");
}
