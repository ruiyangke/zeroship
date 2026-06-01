//! Stage 5c schema auto-discovery — runtime-side init.
//!
//! Covers:
//!   - The bootstrap's inlined `db_init.js` reads `user.default.schema`
//!     directly off the loaded entry module — no manifest-injected path.
//!   - When no DbPlugin is registered (`__zs_env()?.db` is absent)
//!     the init script silently no-ops — required so dev runs without
//!     `DATABASE_URL` still boot.
//!   - When the runtime ships an `installSchema`-shaped module and an
//!     `env.db` namespace, the init script's discovery path fires and
//!     consumes `user.default.schema`, calling `installSchema(schema, env)`
//!     with the live `env.db` handle.
//!   - The bootstrap doesn't publish the legacy `__zsSchemaInit` global.

mod common;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{NativePlugin, NativeRegistrar};

struct DummyDbPlugin;

impl NativePlugin for DummyDbPlugin {
    fn namespace(&self) -> &str {
        "db"
    }

    fn name(&self) -> &str {
        "dummy-db"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        // Register one trivial op so the runtime materializes a non-null
        // `env.db` object. runtime-entry's schema-install sentinel keys on
        // `env.db != null` (the real DbPlugin always populates env.db); an
        // empty register() leaves env.db absent and the sentinel skips
        // install, which is unfaithful to "DB plugin present".
        r.add("__dummyNoop", dummy_db_noop);
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

/// Build a Runtime around the given user-entry source + procedure
/// table, dispatch a probe procedure, and return its result body.
///
/// `user_src` is the user-entry JS string. The shim wraps it with a
/// hand-rolled function-shape `default.rpc` dispatcher (the documented
/// advanced / back-compat path — see `docs/reference/zeroship-standard.md`).
/// Using function-shape here keeps the test surface narrow: the
/// runtime's `__zsDispatch` is exercised by `rpc_dispatch.rs`; here we
/// just need a working dispatch path that surfaces the probe result.
fn dispatch_probe(
    user_src: &str,
    procs_block: &str,
    method: &str,
) -> Result<String, String> {
    init_v8();

    // Synthetic entry shim — function-shape dispatcher (the advanced /
    // back-compat path). The Vite plugin emits dict-shape; this shim
    // intentionally exercises the function-shape branch so a regression
    // dropping that path would surface here.
    let src = format!(
        r#"
{user_src}

const _procedures = {procs_block};
function _zsRpc(name, input) {{
    const fn = _procedures[name];
    if (typeof fn !== "function") {{
        throw Object.assign(new Error("Method not found: " + name), {{ status: 404, code: "NOT_FOUND" }});
    }}
    return fn(input);
}}
async function _zsRpcAndRespond(name, input) {{
    try {{
        const result = await _zsRpc(name, input);
        if (result instanceof Response) return result;
        return new Response(JSON.stringify({{ json: result === undefined ? null : result }}), {{
            status: 200, headers: {{ "content-type": "application/json" }},
        }});
    }} catch (err) {{
        const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600) ? err.status : 500;
        return new Response(JSON.stringify({{ message: err?.message ?? String(err), name: err?.name ?? "Error" }}), {{
            status, headers: {{ "content-type": "application/json" }},
        }});
    }}
}}
async function _zsFetch(request) {{
    const url = new URL(request.url);
    if (!url.pathname.startsWith("/__zeroship/v1/")) {{
        return new Response("Not Found", {{ status: 404 }});
    }}
    const id = decodeURIComponent(url.pathname.slice("/__zeroship/v1/".length));
    let input = undefined;
    if (request.method === "POST") {{
        const text = await request.text();
        if (text) {{
            const env = JSON.parse(text);
            input = env && typeof env === "object" && "json" in env ? env.json : env;
        }}
    }}
    return await _zsRpcAndRespond(id, input);
}}
export default {{ fetch: _zsFetch, rpc: _zsRpc }};
"#
    );
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
    // The init script guards on `__zs_env()?.db`. When the plugin isn't
    // loaded — the configuration this test runs under — the script must
    // NOT attempt to dynamically import `@zeroship/db` (which isn't in
    // the test bundle) and the bootstrap must finish module evaluation
    // cleanly.
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
fn init_script_no_ops_when_default_schema_missing() {
    // Even with an `env.db` namespace present, absence of
    // `user.default.schema` short-circuits the init script before the
    // dynamic import. The test's synthetic-entry shim emits its own
    // `default = { fetch, rpc }` (no `schema:` key), so the gate at
    // `typeof user.default.schema === "object"` falls through and the
    // bootstrap finishes evaluation cleanly without trying to import
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
    rpc: (name, input) => _procedures[name](input),
    // No schema key — discovery should short-circuit.
};
"#;
    let modules = vec![
        ModuleEntry { specifier: "index.js".into(), source: user_src.into() },
    ];
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
            assert!((200..300).contains(&status), "non-2xx: status={status} body={body}");
            body
        }
        _ => panic!("expected sync Response"),
    };
    // The init script must not have called `installSchema` — no
    // `__zsCapturedSchema` exists because the user has no schema. The
    // probe returns `null` (JSON-encoded).
    assert!(body.contains(r#""json":null"#), "expected null, got: {body}");
}

#[test]
fn init_script_runs_install_schema_when_db_plugin_present() {
    // Drive the init script's positive path: the user module exports
    // `default.schema`, the runtime provides an `env.db` namespace, and
    // a stub `@zeroship/db` module makes the dynamic import resolve. The
    // stub `installSchema` captures the schema value it was handed; we
    // read it back through a probe handler.
    //
    // End-to-end pre-condition for production: when the DbPlugin is
    // registered AND the entry exports `default.schema`, the runtime
    // calls `installSchema(schema, env.db)` on the entry's
    // `default.schema` BEFORE any request is served. The path no
    // longer goes through a manifest-injected hint — the schema lives
    // on `user.default`.
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
    rpc: (name, input) => _procedures[name](input),
    // Stage 5c — the runtime reads schema right off this key. No
    // manifest plumbing involved.
    schema: { todos: { id: { type: "id" } } },
};
"#;

    // Stage 7: the bootstrap dynamically imports
    // `@zeroship/bootstrap/install-schema` (the framework-internal
    // package that owns installSchema post-refactor). Stub it as a
    // bundle module so the dynamic import resolves. The stub captures
    // the schema keys it was handed; verifying installSchema was CALLED
    // (with the right schema) is the assertion that matters here.
    let stub_bootstrap = r#"
export function installSchema(schema, _env) {
    globalThis.__zsCapturedSchema = JSON.stringify({
        keys: Object.keys(schema),
    });
    return { collections: {}, ready: Promise.resolve() };
}
"#;

    // After installSchema, runtime-entry flushes the pending mask policy
    // via `await import("@zeroship/db/internal")` and RE-THROWS on failure
    // (a missing module would reject module init). Stub it so the import
    // resolves; `_flushPendingMaskPolicy` returns null (no policy to flush).
    let stub_db_internal = r#"
export function _flushPendingMaskPolicy() { return null; }
"#;

    // The pre-init module imports the stub packages so the bundle eagerly
    // compiles them (the later dynamic imports then hit the registry path).
    let pre_init = r#"
import "@zeroship/bootstrap/install-schema";
import "@zeroship/db/internal";
"#;

    let user_with_preinit = format!("{pre_init}\n{user_src}");
    let modules = vec![
        ModuleEntry { specifier: "index.js".into(), source: user_with_preinit },
        ModuleEntry { specifier: "@zeroship/bootstrap/install-schema".into(), source: stub_bootstrap.into() },
        ModuleEntry { specifier: "@zeroship/db/internal".into(), source: stub_db_internal.into() },
    ];

    let runtime = Runtime::builder()
        .modules(modules)
        .plugin(DummyDbPlugin)
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
            assert!((200..300).contains(&status), "non-2xx: status={status} body={body}");
            body
        }
        _ => panic!("expected sync Response"),
    };
    // The probe returns whatever `__zsCapturedSchema` is. If discovery
    // ran, it's a JSON string with `keys: ["todos"]`. If it didn't run,
    // the probe returns null.
    assert!(body.contains(r#"\"keys\":[\"todos\"]"#),
        "expected captured schema with 'todos' key, got: {body}");
}

#[test]
fn bootstrap_module_lacks_legacy_schema_init_symbols() {
    // Stage 4 cleanup: the synthetic SSR entry no longer publishes
    // `__zsSchemaInit`. The runtime bootstrap doesn't either — its
    // discovery is a top-level await against the `ready` promise
    // returned by `installSchema`. Verify the legacy global stays
    // undefined throughout the bootstrap's evaluation. If a regression
    // re-introduces an IIFE that publishes it, this test will catch it.
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
