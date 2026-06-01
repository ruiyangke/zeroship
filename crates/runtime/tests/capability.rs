//! B3 runtime capability enforcement — JS-side smoke tests.
//!
//! Covers:
//!   - `__zsEnterKind` + `__zsExitKind` natives installed on globalThis
//!   - `fetch()` rejected with `capability_violation` when active kind
//!     is `mutation` (or `query`).
//!   - Nested enter/exit restores the outer kind.
//!   - Action handlers can call fetch normally.
//!
//! The db-write refusal lives in
//! `crates/plugin-db/tests/capability.rs` — same shape, different
//! native callback. The two test files combined are the runtime
//! defense-in-depth surface for B3.
//!
//! ## Out of scope for this PR
//!
//! - Read-only Postgres tx wrapping for query() handlers (BEGIN
//!   TRANSACTION READ ONLY around dispatch). Not landed here — the
//!   tx-wrapping work requires async setup/teardown around the
//!   synchronous V8 dispatch entry, and reuses the existing
//!   `plugin-db::TX_CONN` thread-local in a way that needs a
//!   per-request lifecycle hook the runtime doesn't expose yet. The
//!   capability gate above already refuses ALL writes from a query()
//!   handler (the actual database write never happens), so the
//!   intended invariant is preserved — read-only tx wrapping would
//!   only defend against bugs in the gate itself.
//! - Serializable tx wrapping for mutation() handlers. Same shape as
//!   above; defense in depth on top of the gate.

use std::time::Duration;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, SettledFetch};

/// Dispatch a `/__zeroship/v1/<id>` POST through the runtime, return either
/// the success body envelope or the error envelope (parsed JSON value).
fn dispatch_zs_for_capability(
    source: &str,
    name: &str,
) -> (u16, serde_json::Value) {
    init_v8();
    let modules = vec![zeroship_runtime::ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
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
                    .expect("pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    _ => panic!("expected Response variant"),
                }
            })
        }
        _ => panic!("unexpected outcome variant (Stream or WebSocketUpgrade)"),
    };
    let json: serde_json::Value =
        serde_json::from_str(&body).unwrap_or(serde_json::Value::String(body.clone()));
    (status, json)
}

/// SSR-entry shim that mirrors the runtime's `__zsDispatch` capability
/// frame logic: reads `fn.config.kind`, calls `__zsEnterKind` before
/// invocation, exits on settle (sync, then, catch). Worn as
/// function-shape `default.rpc` (the advanced / back-compat path —
/// `docs/reference/zeroship-standard.md`) so the shim is auditable in one
/// place; dict-shape deploys get the same behaviour via the runtime
/// dispatcher.
const SSR_SHIM: &str = r#"
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
        // capability_violation envelope: the structured fields live
        // under `details` (the dispatcher's canonical shape).
        if (err && err.details !== undefined) body.details = err.details;
        if (err && typeof err.wrapper === "string") body.wrapper = err.wrapper;
        if (err && typeof err.violated === "string") body.violated = err.violated;
        if (err && typeof err.remediation === "string") body.remediation = err.remediation;
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
export default { fetch: _zsFetch, rpc: _shimRpc };
"#;

/// A `mutation()` handler that calls `fetch()` must be refused with
/// the `capability_violation` envelope. The native fetch callback
/// fires the gate synchronously (BEFORE any HTTP work), so we don't
/// need a live server — the rejection arrives via the returned
/// Promise.
#[test]
fn b3_runtime_mutation_refuses_fetch() {
    let user_code = r#"
function doMut(_input, _ctx) {
    // This call should reject synchronously with capability_violation.
    return fetch("http://localhost:1/never");
}
doMut.config = { kind: "mutation" };
const _procedures = { doMut };
"#;
    let src = format!("{user_code}\n{SSR_SHIM}");
    let (status, body) = dispatch_zs_for_capability(&src, "doMut");

    assert_eq!(status, 500, "capability_violation surfaces as 500");
    let code = body.get("code").and_then(|v| v.as_str());
    assert_eq!(code, Some("capability_violation"), "body = {body}");
    // The structured shape lives under `details` (kernel envelope).
    let details = body.get("details").expect("details object present");
    let wrapper = details.get("wrapper").and_then(|v| v.as_str());
    assert_eq!(wrapper, Some("mutation"), "details = {details}");
    let violated = details.get("violated").and_then(|v| v.as_str());
    assert_eq!(violated, Some("fetch"));
    let rem = details.get("remediation").and_then(|v| v.as_str());
    assert!(rem.is_some(), "remediation present");
    assert!(
        rem.unwrap().contains("action()"),
        "remediation mentions action()"
    );
}

/// A `query()` handler that calls `fetch()` must be refused too —
/// queries are read-only, and the read-only tx wrapping (deferred to a
/// follow-up) would not permit outbound I/O anyway. The wrapper field
/// is `"query"` and the remediation points the dev at action().
#[test]
fn b3_runtime_query_refuses_fetch() {
    let user_code = r#"
function doQ(_input, _ctx) {
    return fetch("http://localhost:1/never");
}
doQ.config = { kind: "query" };
const _procedures = { doQ };
"#;
    let src = format!("{user_code}\n{SSR_SHIM}");
    let (status, body) = dispatch_zs_for_capability(&src, "doQ");

    assert_eq!(status, 500);
    assert_eq!(body.get("code").and_then(|v| v.as_str()), Some("capability_violation"));
    let details = body.get("details").expect("details object present");
    assert_eq!(details.get("wrapper").and_then(|v| v.as_str()), Some("query"));
    assert_eq!(details.get("violated").and_then(|v| v.as_str()), Some("fetch"));
}

/// `action()` is the most permissive variant — the runtime must NOT
/// install the gate for action procedures. Without a live server the
/// fetch will fail at the network layer, NOT at the capability layer
/// (different error shape — no `capability_violation` code, no
/// `wrapper` field).
#[test]
fn b3_runtime_action_allows_fetch() {
    let user_code = r#"
async function doAct(_input, _ctx) {
    try {
        await fetch("http://127.0.0.1:1/never-reachable");
        return "fetched";
    } catch (e) {
        // We expect a NETWORK error, not capability_violation. Surface
        // the error code so the test can assert on it.
        return { networkError: true, code: e?.code ?? null, message: e?.message ?? null };
    }
}
doAct.config = { kind: "action" };
const _procedures = { doAct };
"#;
    let src = format!("{user_code}\n{SSR_SHIM}");
    let (status, body) = dispatch_zs_for_capability(&src, "doAct");

    assert_eq!(status, 200, "action() handler reached past the capability gate; body={body}");
    let envelope = body.get("json").unwrap_or(&body);
    // We can either reach the handler's `catch` branch (network error)
    // or — if the loopback responds in some odd setup — the success
    // string. Either way the capability gate did NOT fire.
    let is_network_err = envelope
        .get("networkError")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let is_str_ok = envelope.as_str() == Some("fetched");
    assert!(
        is_network_err || is_str_ok,
        "action handler must NOT see capability_violation; envelope = {envelope}"
    );
    // Definitive negative check: no capability_violation in the body.
    let body_str = body.to_string();
    assert!(
        !body_str.contains("capability_violation"),
        "action body must not contain capability_violation; body = {body_str}"
    );
}

/// Nested enter/exit: an action wraps a mutation. The outer kind is
/// `action` (fetch allowed); when JS pushes `mutation`, fetch is
/// refused; after pop, fetch is allowed again. We don't actually call
/// fetch successfully here (no live server) — we only check the
/// gate flips in/out by invoking it inside the inner frame and
/// catching the capability_violation rejection.
#[test]
fn b3_runtime_nested_enter_exit_marker_stack() {
    let user_code = r#"
async function doNested(_input, _ctx) {
    // We're inside action() now (kind="action", set by the shim).
    // Push a synthetic 'mutation' frame manually and confirm fetch is
    // refused inside it, then confirm it is NOT refused after pop.
    const tok = globalThis.__zsEnterKind("mutation");
    let innerRejection = null;
    try {
        await fetch("http://localhost:1/never");
        innerRejection = "no-reject";
    } catch (e) {
        innerRejection = e && e.code === "capability_violation"
            ? "capability_violation"
            : ("other:" + (e?.message ?? String(e)));
    }
    globalThis.__zsExitKind(tok);

    // Outer frame restored to 'action'. fetch is allowed — it'll fail
    // on the network layer but the error is NOT capability_violation.
    let outerOutcome = null;
    try {
        await fetch("http://127.0.0.1:1/never");
        outerOutcome = "ok";
    } catch (e) {
        outerOutcome = e && e.code === "capability_violation"
            ? "still-blocked"
            : "network-error";
    }
    return { innerRejection, outerOutcome };
}
doNested.config = { kind: "action" };
const _procedures = { doNested };
"#;
    let src = format!("{user_code}\n{SSR_SHIM}");
    let (status, body) = dispatch_zs_for_capability(&src, "doNested");

    assert_eq!(status, 200, "nested marker test reached completion; body={body}");
    let json = body.get("json").unwrap_or(&body);
    assert_eq!(
        json.get("innerRejection").and_then(|v| v.as_str()),
        Some("capability_violation"),
        "inner frame (mutation) must refuse fetch; envelope = {json}"
    );
    assert_eq!(
        json.get("outerOutcome").and_then(|v| v.as_str()),
        Some("network-error"),
        "outer frame (action) must allow fetch through to network; envelope = {json}"
    );
}
