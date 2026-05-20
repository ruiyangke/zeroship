//! B3 runtime capability enforcement — DB write refusal.
//!
//! When a `query()` handler tries to call `zeroship.db.insert` /
//! `updateOne` / `deleteOne` / `insertMany` / `updateMany` /
//! `deleteMany` / `upsert`, the V8 callback must reject the returned
//! promise synchronously with a `capability_violation` envelope —
//! BEFORE touching Postgres.
//!
//! The capability gate fires inside the V8 callback (no async work
//! spawned, no DB connection acquired), so these tests don't require a
//! running PG. The check happens in `crates/plugin-db/src/callbacks.rs`
//! via `refuse_if_query_capability`, reading
//! `zeroship_runtime::rpc::current_kind()` which the synthetic SSR
//! entry sets via `__zsEnterKind`.

use std::sync::Arc;
use std::time::Duration;

use zeroship_plugin_db::DbPlugin;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};

fn dispatch_zs(source: &str, name: &str) -> (u16, serde_json::Value) {
    init_v8();
    let modules = vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }];
    // Use a dummy URL — the capability gate fires BEFORE the pool is
    // touched, so the URL is never dialed.
    let plugins: Vec<Arc<dyn NativePlugin>> =
        vec![Arc::new(DbPlugin::new("postgres://_capability_test_unused"))];
    let runtime = Runtime::builder()
        .modules(modules)
        .plugins(plugins)
        .build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url = format!("http://localhost/_zs/v1/{name}");
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
    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body.clone()));
    (status, json)
}

/// SSR-entry shim that mirrors the production `_zsRpc`: reads
/// `fn.config.kind`, calls `__zsEnterKind` before invocation, exits
/// on settle.
const SHIM: &str = r#"
function _shimRpc(name, input, ctx) {
    const fn = _procedures[name];
    if (typeof fn !== "function") {
        throw Object.assign(new Error("Method not found: " + name), { status: 404 });
    }
    const cfg = fn.config;
    const kind = (cfg && typeof cfg.kind === "string" && cfg.kind) || undefined;
    const ek = globalThis.__zsEnterKind;
    const xk = globalThis.__zsExitKind;
    const tok = (kind && typeof ek === "function") ? ek(kind) : -1;
    try {
        const out = fn(input, ctx);
        if (out && typeof out.then === "function") {
            return out.then(
                (v) => { if (tok >= 0) xk(tok); return v; },
                (e) => { if (tok >= 0) xk(tok); throw e; },
            );
        }
        if (tok >= 0) xk(tok);
        return out;
    } catch (e) {
        if (tok >= 0) xk(tok);
        throw e;
    }
}
async function _zsRpcAndRespond(name, input) {
    try {
        const result = await _shimRpc(name, input);
        return new Response(JSON.stringify({ json: result === undefined ? null : result }),
            { status: 200, headers: { "content-type": "application/json" } });
    } catch (err) {
        const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600) ? err.status : 500;
        const body = { message: err?.message ?? String(err), name: err?.name ?? "Error" };
        if (err && typeof err.code === "string") body.code = err.code;
        if (err && err.details !== undefined) body.details = err.details;
        return new Response(JSON.stringify(body), {
            status, headers: { "content-type": "application/json" },
        });
    }
}
async function _zsFetch(request) {
    const url = new URL(request.url);
    const id = decodeURIComponent(url.pathname.slice("/_zs/v1/".length));
    const text = await request.text();
    let input;
    if (text) {
        const env = JSON.parse(text);
        input = env && typeof env === "object" && "json" in env ? env.json : env;
    }
    return await _zsRpcAndRespond(id, input);
}
export default { fetch: _zsFetch, rpc: _shimRpc };
"#;

/// A `query()` handler that calls `zeroship.db.insert(...)` must be
/// refused with `capability_violation` — and the rejection MUST fire
/// synchronously inside the V8 callback (no PG roundtrip).
#[test]
fn b3_runtime_query_refuses_db_write_insert() {
    let user_code = r#"
import { env } from "zeroship";
function getStuff(_input, _ctx) {
    return env.db.collection("notes").insert({ title: "shouldFail" });
}
getStuff.config = { kind: "query" };
const _procedures = { getStuff };
"#;
    let src = format!("{user_code}\n{SHIM}");
    let (status, body) = dispatch_zs(&src, "getStuff");
    assert_eq!(status, 500, "capability_violation surfaces as 500; body={body}");
    assert_eq!(
        body.get("code").and_then(|v| v.as_str()),
        Some("capability_violation"),
        "body = {body}"
    );
    let details = body.get("details").expect("details object present");
    assert_eq!(details.get("wrapper").and_then(|v| v.as_str()), Some("query"));
    assert_eq!(
        details.get("violated").and_then(|v| v.as_str()),
        Some("ctx.db.insert")
    );
}

/// `updateOne` / `deleteOne` / `insertMany` / `updateMany` /
/// `deleteMany` / `upsert` all hit the same gate. One test parameter-
/// ised over each entry point — keeps the assertion shape simple.
#[test]
fn b3_runtime_query_refuses_every_db_write_op() {
    let ops = [
        ("insert", r#"env.db.collection("n").insert({ t: "x" })"#, "ctx.db.insert"),
        ("updateOne", r#"env.db.collection("n").updateOne({ id: 1 }, { t: "x" })"#, "ctx.db.updateOne"),
        ("deleteOne", r#"env.db.collection("n").deleteOne({ id: 1 })"#, "ctx.db.deleteOne"),
        ("insertMany", r#"env.db.collection("n").insertMany([{ t: "x" }])"#, "ctx.db.insertMany"),
        ("updateMany", r#"env.db.collection("n").updateMany({ t: "x" }, { t: "y" })"#, "ctx.db.updateMany"),
        ("deleteMany", r#"env.db.collection("n").deleteMany({ t: "x" })"#, "ctx.db.deleteMany"),
        ("upsert", r#"env.db.collection("n").upsert({ id: 1, t: "x" }, { conflictFields: ["id"] })"#, "ctx.db.upsert"),
    ];
    for (label, expr, expected_violated) in ops {
        let user_code = format!(
            r#"
import {{ env }} from "zeroship";
function listThings_{label}(_input, _ctx) {{
    return {expr};
}}
listThings_{label}.config = {{ kind: "query" }};
const _procedures = {{ ["t_{label}"]: listThings_{label} }};
"#
        );
        let src = format!("{user_code}\n{SHIM}");
        let (status, body) = dispatch_zs(&src, &format!("t_{label}"));
        assert_eq!(
            status, 500,
            "{label}: capability_violation must surface as 500; body={body}"
        );
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("capability_violation"),
            "{label}: code mismatch; body={body}"
        );
        let details = body.get("details").expect("details object present");
        assert_eq!(
            details.get("wrapper").and_then(|v| v.as_str()),
            Some("query"),
            "{label}: wrapper mismatch; details={details}"
        );
        assert_eq!(
            details.get("violated").and_then(|v| v.as_str()),
            Some(expected_violated),
            "{label}: violated mismatch; details={details}"
        );
    }
}

/// `action()` handlers can write — `current_kind() == Some(Action)` is
/// NOT `Some(Query)`, so the gate is silent. The actual insert will
/// fail at the DB layer (we passed a dummy URL), but the failure is
/// NOT a capability_violation. Confirms the gate only catches
/// query-kind callers.
///
/// We don't actually want to wait for a PG connection attempt that
/// will likely retry / time out, so the test asserts only that the
/// error is NOT capability_violation — whatever the actual rejection
/// looks like is fine for this purpose.
#[test]
fn b3_runtime_action_allows_db_write_through_gate() {
    // The handler calls insert() but does NOT await — it inspects the
    // Promise synchronously. The capability gate (if it fired) would
    // produce an already-rejected promise with `.code === "capability_violation"`
    // BEFORE the test goroutine has a chance to observe the network
    // error. We snapshot the promise's state at return time: if the
    // promise hasn't already rejected with capability_violation, the
    // gate didn't fire.
    let user_code = r#"
import { env } from "zeroship";
function doAction(_input, _ctx) {
    // Returns a Promise. We swap in a synchronous race: if the gate
    // fired, the promise is already rejected at this point. We surface
    // the synchronous shape by chaining .catch() and returning the
    // captured error fields. The test asserts code !== capability_violation.
    const p = env.db.collection("notes").insert({ title: "shouldReachDb" });
    return Promise.race([
        p.then(() => ({ outcome: "resolved" }), (e) => ({
            outcome: "rejected",
            code: e && typeof e.code === "string" ? e.code : null,
            message: (e?.message ?? "").slice(0, 120),
        })),
        // Bail out after a tick — we don't want to wait for PG.
        new Promise((res) => {
            // queueMicrotask runs before the PG dial attempt completes.
            queueMicrotask(() => res({ outcome: "pending-no-cap-violation" }));
        }),
    ]);
}
doAction.config = { kind: "action" };
const _procedures = { doAction };
"#;
    let src = format!("{user_code}\n{SHIM}");
    let (_status, body) = dispatch_zs(&src, "doAction");
    let json = body.get("json").unwrap_or(&body);
    // Two acceptable outcomes for "the gate did NOT fire":
    //   1. outcome == "pending-no-cap-violation" — the bail-out tick
    //      ran first; the insert promise is still pending on PG.
    //   2. outcome == "rejected" with code != "capability_violation"
    //      — PG dial failed but it's a network error, not a gate hit.
    // Forbidden: outcome == "rejected" with code == "capability_violation"
    //   — that means the gate fired despite action() being the kind.
    let outcome = json
        .get("outcome")
        .and_then(|v| v.as_str())
        .unwrap_or("(no outcome)");
    let code = json.get("code").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        outcome == "pending-no-cap-violation"
            || (outcome == "rejected" && code != "capability_violation"),
        "action() handler must NOT see capability_violation; outcome={outcome} code={code} body={body}"
    );
}
