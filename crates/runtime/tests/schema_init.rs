//! Stage 5c schema auto-discovery — runtime-side init.
//!
//! Covers:
//!   - The bootstrap's inlined `db_init.js` reads `user.default.schema`
//!     directly off the loaded entry module — no manifest-injected path.
//!   - When no DbPlugin is registered (`__zsBeginAutoTx` is undefined)
//!     the init script silently no-ops — required so dev runs without
//!     `DATABASE_URL` still boot.
//!   - When the runtime ships a `_installSchema`-shaped global and a
//!     plant for `__zsBeginAutoTx`, the init script's discovery path
//!     fires and consumes `user.default.schema`.
//!   - The bootstrap doesn't publish the legacy `__zsSchemaInit` global.

mod common;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;

/// Build a Runtime around the given user-entry source + procedure
/// table, dispatch a probe procedure, and return its result body.
///
/// `user_src` is the user-entry JS string. The synthetic-entry stub
/// (mirroring what the vite-plugin's `rpc-registry.ts` emits post-Stage
/// 5b) imports the user module by namespace and exposes a `_zsRpc`
/// dispatcher; the probe procedure returns whatever JSON shape the
/// caller wants to assert against.
fn dispatch_probe(
    user_src: &str,
    procs_block: &str,
    method: &str,
) -> Result<String, String> {
    init_v8();

    // Synthetic entry shim — namespace walk + minimal dispatcher.
    // Mirrors the shape the vite-plugin emits after Stage 5b.
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

    let runtime = Runtime::builder().modules(modules).build();
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
fn init_script_no_ops_without_db_plugin() {
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
    // Even with `__zsBeginAutoTx` planted (DbPlugin present), absence of
    // `user.default.schema` short-circuits the init script before the
    // dynamic-import. The test's synthetic-entry shim emits its own
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
    const id = decodeURIComponent(url.pathname.slice("/_zs/v1/".length));
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
    // Plant `__zsBeginAutoTx` synchronously via a pre-init module that
    // the bootstrap imports as a side effect. The init script will
    // see the gate open, but then find no `default.schema` and skip.
    let preinit = r#"
globalThis.__zsBeginAutoTx = function () { return 0; };
globalThis.__zsEndAutoTx = function () {};
"#;
    let user_with_preinit = format!("{preinit}\n{user_src}");
    let modules = vec![
        ModuleEntry { specifier: "index.js".into(), source: user_with_preinit },
    ];
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/_zs/v1/readGate",
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
    // The init script must not have called `_installSchema` — no
    // `__zsCapturedSchema` exists because the user has no schema. The
    // probe returns `null` (JSON-encoded).
    assert!(body.contains(r#""json":null"#), "expected null, got: {body}");
}

#[test]
fn init_script_runs_install_schema_when_db_plugin_present() {
    // Drive the init script's positive path: the user module exports
    // `default.schema`, we plant `__zsBeginAutoTx` and a stub
    // `@zeroship/db` so the dynamic-import resolves. The stub
    // `_installSchema` captures the schema value it was handed; we
    // read it back through a probe handler.
    //
    // End-to-end pre-condition for production: when the DbPlugin is
    // registered AND the entry exports `default.schema`, the runtime
    // calls `_installSchema` on the entry's `default.schema` BEFORE
    // any request is served. The path no longer goes through a
    // manifest-injected hint — the schema lives on `user.default`.
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
    // Stage 5c — the runtime reads schema right off this key. No
    // manifest plumbing involved.
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
