//! ALS-backed RPC ctx tests.
//!
//! Covers the per-request `ctx` object exposed via
//! `globalThis.__zeroshipGetRpcCtx()`: scalar fields, native
//! Headers/URL wrapping with frozen mutation, AbortSignal binding,
//! and ALS-backed survival across `await` / `.then` boundaries.
//!
//! See `docs/proposals/rpc-v2.md` §3 (Ambient context).

mod common;
use common::{dispatch, m};

// ---------------------------------------------------------------------------
// 1 — ctx is observable inside a procedure call
// ---------------------------------------------------------------------------

#[test]
fn ctx_is_observable_inside_procedure() {
    let r = dispatch(
        m(r#"export function test() {
            const ctx = __zeroshipGetRpcCtx();
            return {
                hasCtx: !!ctx,
                hasRequestId: typeof ctx.requestId === "string" && ctx.requestId.length > 0,
                hasTraceId: typeof ctx.traceId === "string" && ctx.traceId.length > 0,
                hasMethod: typeof ctx.method === "string",
                hasUrl: typeof ctx.url === "object" && ctx.url !== null,
                hasHeaders: typeof ctx.headers === "object" && ctx.headers !== null,
                hasSignal: typeof ctx.signal === "object" && ctx.signal !== null,
                userIsNull: ctx.user === null,
                idemUndefined: ctx.idempotencyKey === undefined,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""hasCtx":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasRequestId":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasTraceId":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasMethod":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasUrl":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasHeaders":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasSignal":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""userIsNull":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""idemUndefined":true"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// 2 / 3 — frozen Headers
// ---------------------------------------------------------------------------

#[test]
fn frozen_headers_set_throws() {
    let r = dispatch(
        m(r#"export function test() {
            const h = __zeroshipGetRpcCtx().headers;
            try {
                h.set("x", "1");
                return "no-throw";
            } catch (e) {
                return { name: e.name, isType: e instanceof TypeError };
            }
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains(r#""isType":true"#),
        "expected TypeError on frozen .set(), got: {}",
        r.json
    );
}

#[test]
fn frozen_headers_get_works() {
    // Header is populated by the kernel from the request — content-type
    // is always supplied by the test harness.
    let r = dispatch(
        m(r#"export function test() {
            const h = __zeroshipGetRpcCtx().headers;
            return { ct: h.get("content-type") };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""ct":"application/json""#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// 4 / 5 — frozen URL + searchParams
// ---------------------------------------------------------------------------

#[test]
fn frozen_url_searchparams_set_throws() {
    let r = dispatch(
        m(r#"export function test() {
            const url = __zeroshipGetRpcCtx().url;
            try {
                url.searchParams.set("a", "b");
                return "no-throw";
            } catch (e) {
                return { name: e.name, isType: e instanceof TypeError };
            }
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains(r#""isType":true"#),
        "expected TypeError on frozen searchParams.set, got: {}",
        r.json
    );
}

#[test]
fn frozen_url_searchparams_get_works() {
    // The kernel feeds the url to RpcContext via the dispatch URL.
    // Synthetic-entry tests post to /_zs/v1/<id> with no query string,
    // so we read from `pathname` to verify the URL is parsed and
    // exposed correctly. searchParams.get returns null on missing key
    // (canonical WHATWG URL behavior) — pre-set in the kernel side
    // would require a different harness. The point here is that
    // `searchParams.get(...)` itself is callable on the frozen URL.
    let r = dispatch(
        m(r#"export function test() {
            const url = __zeroshipGetRpcCtx().url;
            return {
                pathname: url.pathname,
                missing: url.searchParams.get("missing"),
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""pathname":"/_zs/v1/test""#), "got: {}", r.json);
    assert!(r.json.contains(r#""missing":null"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// 6 — survives `await`
// ---------------------------------------------------------------------------

#[test]
fn ctx_survives_await() {
    let r = dispatch(
        m(r#"export async function test() {
            const before = __zeroshipGetRpcCtx();
            await new Promise(r => setTimeout(r, 10));
            const after = __zeroshipGetRpcCtx();
            return { sameRef: before === after, sameId: before.requestId === after.requestId };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""sameRef":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""sameId":true"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// 7 — survives `.then`
// ---------------------------------------------------------------------------

#[test]
fn ctx_survives_promise_then() {
    let r = dispatch(
        m(r#"export function test() {
            const before = __zeroshipGetRpcCtx();
            return Promise.resolve().then(() => {
                const after = __zeroshipGetRpcCtx();
                return { sameRef: before === after };
            });
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""sameRef":true"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// 8 — undefined outside an RPC (module-init top level)
// ---------------------------------------------------------------------------

#[test]
fn ctx_undefined_at_module_init() {
    // Capture the ctx at module-init time (BEFORE any procedure runs).
    // The procedure call later asserts the captured value was undefined,
    // proving the ALS slot is empty outside the dispatch path.
    let r = dispatch(
        m(r#"
        const initCtx = __zeroshipGetRpcCtx();
        export function test() {
            return {
                initCtxIsUndefined: initCtx === undefined,
                callTimeCtxDefined: __zeroshipGetRpcCtx() !== undefined,
            };
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""initCtxIsUndefined":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""callTimeCtxDefined":true"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// 9 — request_id, method, url wired correctly
// ---------------------------------------------------------------------------

#[test]
fn ctx_scalars_match_request() {
    let r = dispatch(
        m(r#"export function test() {
            const ctx = __zeroshipGetRpcCtx();
            return {
                method: ctx.method,
                href: ctx.url.href,
                pathname: ctx.url.pathname,
                requestIdShape: /^req_[0-9a-f]{16}$/.test(ctx.requestId),
                traceIdShape: /^trace_[0-9a-f]{16}$/.test(ctx.traceId),
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""method":"POST""#), "got: {}", r.json);
    assert!(r.json.contains(r#""pathname":"/_zs/v1/test""#), "got: {}", r.json);
    assert!(r.json.contains(r#""href":"http://localhost/_zs/v1/test""#), "got: {}", r.json);
    assert!(r.json.contains(r#""requestIdShape":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""traceIdShape":true"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// 10 — ctx.signal is an AbortSignal
// ---------------------------------------------------------------------------

#[test]
fn ctx_signal_is_abort_signal() {
    let r = dispatch(
        m(r#"export function test() {
            const sig = __zeroshipGetRpcCtx().signal;
            return {
                isAbortSignal: sig instanceof AbortSignal,
                aborted: sig.aborted,
                hasReason: sig.reason === undefined,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""isAbortSignal":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""aborted":false"#), "got: {}", r.json);
    assert!(r.json.contains(r#""hasReason":true"#), "got: {}", r.json);
}

// ---------------------------------------------------------------------------
// 11 — no interference with user AsyncLocalStorage
// ---------------------------------------------------------------------------

#[test]
fn user_async_local_storage_does_not_collide() {
    // The user mints their own ALS, runs a callback inside als.run(),
    // and inside that callback both `als.getStore()` (their store) and
    // `__zeroshipGetRpcCtx()` (platform ctx) must return what each
    // owner expects — no leakage in either direction.
    let r = dispatch(
        m(r#"
        import { AsyncLocalStorage } from "node:async_hooks";
        export function test() {
            const platformCtx = __zeroshipGetRpcCtx();
            const userAls = new AsyncLocalStorage();
            return userAls.run({ user: "store" }, () => {
                return {
                    userStore: userAls.getStore(),
                    platformSeenInside: __zeroshipGetRpcCtx() === platformCtx,
                    userStoreSeenInPlatformShape: platformCtx.user === null,
                };
            });
        }
        "#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains(r#""userStore":{"user":"store"}"#), "got: {}", r.json);
    assert!(r.json.contains(r#""platformSeenInside":true"#), "got: {}", r.json);
    assert!(r.json.contains(r#""userStoreSeenInPlatformShape":true"#), "got: {}", r.json);
}
