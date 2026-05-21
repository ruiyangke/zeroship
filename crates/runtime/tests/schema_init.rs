//! Stage 4 schema auto-discovery — runtime-side init.
//!
//! Covers:
//!   - The bootstrap's inlined `db_init.js` reads
//!     `globalThis.__zsManifestSchemaPath` (injected from Rust off
//!     `manifest.exports.schema`) before deciding whether to run
//!     discovery.
//!   - When no DbPlugin is registered (`__zsBeginAutoTx` is undefined)
//!     the init script silently no-ops — required so dev runs without
//!     `DATABASE_URL` still boot.
//!   - When the runtime ships a `_installSchema`-shaped global and a
//!     plant for `__zsBeginAutoTx`, the init script's discovery path
//!     fires and consumes `user.default.schema`.
//!   - The synthetic-entry's post-Stage 4 ergonomics: the worker can
//!     bind `default.fetch` and serve requests even when the schema
//!     init is a no-op.

mod common;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;

/// Build a Runtime with the schema-path hint set, evaluate a probe
/// procedure, and return its result body.
///
/// `user_src` is the user-entry JS string. The synthetic-entry stub
/// (mirroring what the vite-plugin's `rpc-registry.ts` emits post-Stage
/// 4) imports the user module by namespace and exposes a `_zsRpc`
/// dispatcher; the probe procedure returns whatever JSON shape the
/// caller wants to assert against.
fn dispatch_with_schema_path(
    user_src: &str,
    procs_block: &str,
    schema_path: Option<&str>,
    method: &str,
) -> Result<String, String> {
    init_v8();

    // Synthetic entry shim — mirrors the cleaned-up Stage 4 shape.
    // No `_installSchema`, no `__zsSchemaInit`, no `_zsSchemaMod`. Just
    // procedure dispatch with the user-namespace walk.
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
    if (!url.pathname.startsWith("/_zs/v1/")) {{
        return new Response("Not Found", {{ status: 404 }});
    }}
    const id = decodeURIComponent(url.pathname.slice("/_zs/v1/".length));
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

    let mut builder = Runtime::builder().modules(modules);
    if let Some(p) = schema_path {
        builder = builder.manifest_schema_path(Some(p.to_string()));
    }
    let runtime = builder.build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url = format!("http://localhost/_zs/v1/{method}");
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
fn manifest_schema_path_exposed_as_global() {
    // When the runtime is built with `manifest_schema_path(Some(...))`,
    // the bootstrap's pre-evaluation injection sets
    // `globalThis.__zsManifestSchemaPath` to the same string. User code
    // (and the inlined `db_init.js`) can read it.
    let body = dispatch_with_schema_path(
        r#"
        export function readSchemaPath() {
            return globalThis.__zsManifestSchemaPath ?? null;
        }
        "#,
        "{ readSchemaPath }",
        Some("src/schema.ts"),
        "readSchemaPath",
    )
    .unwrap();
    assert!(body.contains(r#""json":"src/schema.ts""#), "got: {body}");
}

#[test]
fn manifest_schema_path_unset_defaults_to_undefined() {
    // No manifest_schema_path → no global is set → user code sees
    // undefined. The bootstrap's discovery short-circuits on the same
    // condition.
    let body = dispatch_with_schema_path(
        r#"
        export function readSchemaPath() {
            return typeof globalThis.__zsManifestSchemaPath;
        }
        "#,
        "{ readSchemaPath }",
        None,
        "readSchemaPath",
    )
    .unwrap();
    assert!(body.contains(r#""json":"undefined""#), "got: {body}");
}

#[test]
fn init_script_no_ops_when_no_db_plugin() {
    // The init script guards on `__zsBeginAutoTx` (the DbPlugin's
    // begin-auto-tx native). When the plugin isn't loaded — the
    // configuration this test runs under — the script must NOT attempt
    // to dynamically import `@zeroship/db` (which isn't in the test
    // bundle) and the bootstrap must finish module evaluation cleanly.
    //
    // We assert the boot path made it through end-to-end: the worker
    // resolved `default.fetch`, served a request, and returned the
    // probe's value. A regression where the dynamic import threw and
    // tore down evaluation would surface here as a dispatch-side
    // error, not the happy `"json":"ok"`.
    //
    // The user module here only has named exports — the synthetic-entry
    // shim emitted by `dispatch_with_schema_path` adds its own
    // `default = { fetch, rpc }`. The init script's `user.default.schema`
    // read returns undefined under this shape (no user-default), so
    // discovery short-circuits on the schema check before even getting
    // to the `__zsBeginAutoTx` gate. Either short-circuit is fine.
    let body = dispatch_with_schema_path(
        r#"
        export function probe() { return "ok"; }
        "#,
        "{ probe }",
        Some("src/schema.ts"),
        "probe",
    )
    .unwrap();
    assert!(body.contains(r#""json":"ok""#), "got: {body}");
}

#[test]
fn init_script_runs_install_schema_when_db_plugin_present() {
    // Drive the init script's positive path by planting a stub
    // `__zsBeginAutoTx` (the gate it checks) AND shimming `@zeroship/db`
    // via a bundle module so the dynamic `await import("@zeroship/db")`
    // resolves locally. The stub `_installSchema` captures the schema
    // value it was handed, which we read back through a probe handler.
    //
    // This is the end-to-end pre-condition for production: when the
    // DbPlugin is registered, the runtime calls `_installSchema` on
    // the user's `default.schema` BEFORE any request is served.
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
    const id = decodeURIComponent(url.pathname.slice("/_zs/v1/".length));
    return await _zsRpcAndRespond(id, undefined);
}
export default {
    fetch: _zsFetch,
    rpc: (name, input) => _procedures[name](input),
    // Drive the init script's positive path: present a schema object
    // on default so the bootstrap's read of `user.default.schema`
    // returns it.
    schema: { todos: { id: { type: "id" } } },
};
"#;

    // Stub @zeroship/db as a bundle module so the bootstrap's
    // `await import("@zeroship/db")` resolves. The stub also plants
    // `__zsBeginAutoTx` synchronously at import-time so the init
    // script's gate is open.
    let stub_db = r#"
globalThis.__zsBeginAutoTx = function () { return 0; };
globalThis.__zsEndAutoTx   = function () {};
export function _installSchema(schema, options) {
    globalThis.__zsCapturedSchema = JSON.stringify({
        keys: Object.keys(schema),
        installOnEnvDb: !!(options && options.installOnEnvDb),
    });
}
"#;

    // Pre-seed `__zsBeginAutoTx` in a tiny pre-init module that the
    // user-entry imports for its side effect, so the bootstrap's
    // discovery-gate sees the plant BEFORE the dynamic-import resolves.
    // The pre-init module also imports the stub @zeroship/db so the
    // bundle eagerly compiles it (lazy dynamic import then hits the
    // registry path).
    let pre_init = r#"
import "@zeroship/db";
"#;

    let user_with_preinit = format!("{pre_init}\n{user_src}");
    let modules = vec![
        ModuleEntry { specifier: "index.js".into(), source: user_with_preinit },
        ModuleEntry { specifier: "@zeroship/db".into(), source: stub_db.into() },
    ];

    let runtime = Runtime::builder()
        .modules(modules)
        .manifest_schema_path(Some("src/schema.ts".into()))
        .build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/_zs/v1/readCaptured",
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
    // ran, it's a JSON string with `keys: ["todos"]` and
    // `installOnEnvDb: true`. If it didn't run, the probe returns null.
    assert!(body.contains(r#"\"keys\":[\"todos\"]"#),
        "expected captured schema with 'todos' key, got: {body}");
    assert!(body.contains(r#"\"installOnEnvDb\":true"#),
        "expected installOnEnvDb: true, got: {body}");
}

#[test]
fn bootstrap_module_lacks_legacy_schema_init_symbols() {
    // Stage 4 cleanup: the synthetic SSR entry no longer publishes
    // `__zsSchemaInit`. The runtime bootstrap doesn't either — its
    // discovery is a top-level await against `__zeroshipPlatformReady`
    // chained inside `_installSchema`. Verify the legacy global stays
    // undefined throughout the bootstrap's evaluation. If a regression
    // re-introduces an IIFE that publishes it, this test will catch it.
    let body = dispatch_with_schema_path(
        r#"
        export function readInit() {
            return typeof globalThis.__zsSchemaInit;
        }
        "#,
        "{ readInit }",
        Some("src/schema.ts"),
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
