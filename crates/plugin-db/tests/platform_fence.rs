//! P9 PR 4 — end-to-end `__platform` capability fence (§8).
//!
//! `db_v8_class.rs` covers the unit-level fence (a directly-minted `Db`
//! object: reflection invisibility, string-access refusal, private-slot
//! reachability). This file covers the *production wiring* through a
//! real `Runtime`: that by the time a creator handler runs, the
//! `globalThis.__zsDbPlatform` resolver has been DELETED by the
//! runtime-entry, so creator code has NO carrier to the handle — while
//! the moved callables are absent from `env.db` and the string
//! `env.db.__platform` is refused.
//!
//! These dispatch a `query`-kind RPC and read the JSON response, the
//! same harness `capability.rs` uses (no Postgres needed — the assertions
//! run entirely in the V8 callback, before any pool dial).

use std::sync::Arc;
use std::time::Duration;

use zeroship_plugin_db::DbPlugin;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};

/// Minimal SSR shim (mirrors `capability.rs`): a function-shape
/// `default.rpc` + `default.fetch` that invokes a named procedure and
/// JSON-encodes the result. The user module supplies `_procedures`.
const SHIM: &str = r#"
async function _zsRpcAndRespond(name, input) {
    try {
        const fn = _procedures[name];
        if (typeof fn !== "function") {
            throw Object.assign(new Error("Method not found: " + name), { status: 404 });
        }
        const result = await fn(input);
        return new Response(JSON.stringify({ json: result === undefined ? null : result }),
            { status: 200, headers: { "content-type": "application/json" } });
    } catch (err) {
        const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600) ? err.status : 500;
        const body = { message: err?.message ?? String(err), name: err?.name ?? "Error" };
        if (err && typeof err.code === "string") body.code = err.code;
        return new Response(JSON.stringify(body), {
            status, headers: { "content-type": "application/json" },
        });
    }
}
async function _zsFetch(request) {
    const url = new URL(request.url);
    const id = decodeURIComponent(url.pathname.slice("/__zeroship/v1/".length));
    const text = await request.text();
    let input;
    if (text) {
        const env = JSON.parse(text);
        input = env && typeof env === "object" && "json" in env ? env.json : env;
    }
    return await _zsRpcAndRespond(id, input);
}
export default { fetch: _zsFetch, rpc: _zsRpcAndRespond };
"#;

fn dispatch(source: &str, name: &str) -> (u16, serde_json::Value) {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }];
    let plugins: Vec<Arc<dyn NativePlugin>> =
        vec![Arc::new(DbPlugin::new("postgres://_platform_fence_unused", None))];
    let runtime = Runtime::builder().modules(modules).plugins(plugins).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url = format!("http://localhost/__zeroship/v1/{name}");
    let outcome = runtime.call_fetch_handler(
        "POST",
        &url,
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        ctx,
    );
    let (status, body) = match outcome {
        FetchOutcome::Response { status, body, .. } => (status, body),
        FetchOutcome::Pending { rx, cancel: _ } => {
            compio::runtime::Runtime::new().unwrap().block_on(async {
                runtime.start_pump();
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("pending timeout")
                    .expect("settled error");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    _ => panic!("expected Response variant"),
                }
            })
        }
        _ => panic!("unexpected outcome variant"),
    };
    let body = String::from_utf8_lossy(&body).into_owned();
    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body.clone()));
    (status, json)
}

/// By the time a creator handler runs, `globalThis.__zsDbPlatform` is
/// gone (deleted by the runtime-entry after boot) — so creator code has
/// no resolver to fish the handle out of the private slot. The handler
/// reports `typeof globalThis.__zsDbPlatform`.
#[test]
fn resolver_global_is_deleted_before_handler_runs() {
    let user_code = r#"
import { env } from "zeroship";
function probe() {
    return { resolver: typeof globalThis.__zsDbPlatform };
}
const _procedures = { probe };
"#;
    let src = format!("{user_code}\n{SHIM}");
    let (status, body) = dispatch(&src, "probe");
    assert_eq!(status, 200, "probe should succeed; body={body}");
    let json = body.get("json").unwrap_or(&body);
    assert_eq!(
        json.get("resolver").and_then(|v| v.as_str()),
        Some("undefined"),
        "globalThis.__zsDbPlatform must be deleted before creator handlers run; body={body}"
    );
}

/// The moved platform members are absent from `env.db`, and the string
/// `env.db.__platform` access throws `platform_internal_only`. One
/// handler probes the whole fence and returns a report object.
#[test]
fn env_db_platform_surface_is_fenced_in_handler() {
    let user_code = r#"
import { env } from "zeroship";
function probe() {
    const db = env.db;
    const report = {
        registerModel: typeof db.registerModel,
        setMaskPolicy: typeof db.setMaskPolicy,
        startReplicationConsumer: typeof db.startReplicationConsumer,
        migrations: typeof db.migrations,
        replication: typeof db.replication,
        collection: typeof db.collection,
        transaction: typeof db.transaction,
        ownNames: Object.getOwnPropertyNames(db).includes("__platform"),
        platformAccess: "unset",
    };
    try {
        // String access must throw (the getter trap).
        void db.__platform;
        report.platformAccess = "returned";
    } catch (e) {
        report.platformAccess = (e && e.code) ? e.code : "threw";
    }
    return report;
}
const _procedures = { probe };
"#;
    let src = format!("{user_code}\n{SHIM}");
    let (status, body) = dispatch(&src, "probe");
    assert_eq!(status, 200, "probe should succeed; body={body}");
    let json = body.get("json").unwrap_or(&body);

    // Moved members gone.
    for name in [
        "registerModel",
        "setMaskPolicy",
        "startReplicationConsumer",
        "migrations",
        "replication",
    ] {
        assert_eq!(
            json.get(name).and_then(|v| v.as_str()),
            Some("undefined"),
            "env.db.{name} must be undefined in a handler; body={body}"
        );
    }
    // Public surface retained.
    assert_eq!(json.get("collection").and_then(|v| v.as_str()), Some("function"));
    assert_eq!(json.get("transaction").and_then(|v| v.as_str()), Some("function"));
    // Not in own property names.
    assert_eq!(
        json.get("ownNames").and_then(|v| v.as_bool()),
        Some(false),
        "__platform must not be an own property name of env.db; body={body}"
    );
    // String access actively refused.
    assert_eq!(
        json.get("platformAccess").and_then(|v| v.as_str()),
        Some("platform_internal_only"),
        "env.db.__platform string access must throw platform_internal_only; body={body}"
    );
}
