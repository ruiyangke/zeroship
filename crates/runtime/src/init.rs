//! V8 platform initialization, global bindings, and shared constants.
//!
//! Consolidates everything needed to boot an isolate:
//! - `init_v8()` — one-time V8 platform init
//! - `setup_globals()` — console, timers, fetch, URL, KV, crypto, env, streams
//! - Polyfill constants (`FETCH_JS`, `CRYPTO_JS`, `STREAMS_JS`)
//! - Result types (`RequestResult`, `HttpResult`)

use std::time::Duration;

use zeroship_runtime_macros::zeroship_op;

use crate::state::SharedState;
use crate::state::TimerCallback;

// ===========================================================================
// V8 platform init
// ===========================================================================

/// Initialize V8 (safe to call multiple times).
pub fn init_v8() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Install the TLS crypto provider (rustls needs this for HTTPS fetch).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        // Load ICU data so Intl.NumberFormat / .DateTimeFormat / .Collator
        // work — npm packages bundled by deepagents (Anthropic SDK, etc.)
        // construct these at module top-level and crash with "Internal
        // error. Icu error." otherwise.
        v8::icu::set_common_data_77(deno_core_icudata::ICU_DATA)
            .expect("failed to load ICU data");

        // `--expose-gc` makes `request_garbage_collection_for_testing`
        // available so memory-pressure tests can force a GC pass
        // mid-run instead of waiting for isolate teardown. The flag
        // only enables a test entry point — it doesn't affect
        // production behavior.
        v8::V8::set_flags_from_string("--expose-gc");

        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

// ===========================================================================
// CPU time helper
// ===========================================================================

/// Read the current thread's CPU time via CLOCK_THREAD_CPUTIME_ID.
/// Only counts actual CPU cycles — I/O wait is excluded.
pub fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    #[allow(unsafe_code)]
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

// ===========================================================================
// Result types
// ===========================================================================

/// Result of executing a JSON-RPC request.
#[derive(Debug)]
pub struct RequestResult {
    pub json: String,
    pub cpu_time: Duration,
    pub wall_time: Duration,
    /// Console output captured during execution.
    pub logs: Vec<String>,
}

/// Result of executing an HTTP request via onRequest handler.
#[derive(Debug)]
pub struct HttpResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub cpu_time: Duration,
    pub wall_time: Duration,
    pub logs: Vec<String>,
}

// ===========================================================================
// Polyfill / dispatch constants
// ===========================================================================

/// Embedded Fetch API polyfill -- loaded after globals are set up.
pub const FETCH_JS: &str = include_str!("embed/fetch.js");

/// Embedded crypto polyfill (getRandomValues, SubtleCrypto.digest, base64 helpers).
pub const CRYPTO_JS: &str = include_str!("embed/crypto.js");

/// Embedded WebSocket/WebSocketPair polyfill (depends on the native
/// EventTarget installed by `install_dom`).
pub const WEBSOCKET_JS: &str = include_str!("embed/websocket.js");

/// Node-shaped globals the runtime doesn't already install: a lazy
/// `globalThis.Buffer` stub (configurable getter so unenv's
/// `node:buffer` import can swap it out via `Object.defineProperty`)
/// and `setImmediate` / `clearImmediate` mapped to `setTimeout(0)` /
/// `clearTimeout`. Loaded BEFORE any user module evaluates, so npm
/// packages that read these as bare globals (no `node:*` import) find
/// them present.
pub const NODE_GLOBALS_JS: &str = include_str!("embed/node-globals.js");

/// The `zeroship` user-facing ESM module. Exposes the request-scoped helpers
/// that SDK packages lean on:
///
/// - `env`: a frozen snapshot of per-app env vars (same as `fetch`'s 2nd arg).
/// - `waitUntil(promise)`: extend the isolate's hold on a request past its
///   response so fire-and-forget work (log flush, webhook retry) can finish.
/// - `getRequest()`: look up the current `Request` from any nested module
///   without threading it through every call. Throws if called outside a
///   request (bootstrap hasn't bound a ctx yet).
///
/// `__zs_wait_until` is registered by `setup_globals`; it throws to JS when
/// called outside a request. `__zs_env` / `__zs_get_request_ctx` are the
/// other halves.
pub(crate) const ZEROSHIP_MODULE_JS: &str = r#"
const env = Object.freeze(__zs_env());

function waitUntil(promise) {
    if (!(promise instanceof Promise)) {
        throw new TypeError("waitUntil expects a Promise");
    }
    __zs_wait_until(promise);
}

function getRequest() {
    // Kernel stashes the Request JS object on `state.request_by_id`
    // when it builds one in call_fetch_handler's slow path. The RPC
    // fast-path does NOT build a Request (body-is-args dispatch), so
    // getRequest() returns null there — use the default.fetch contract
    // when you need header/url access.
    const req = __zs_get_request();
    if (!req) {
        throw new Error("getRequest called outside a fetch handler (RPC fast-path has no Request)");
    }
    return req;
}

export { env, waitUntil, getRequest };
"#;

/// Internal bootstrap-only module. NOT part of the stable user-facing API —
/// only the runtime-synthesized `index.js` bootstrap imports from here. Kept
/// in its own specifier so `import { __bindRequest } from "zeroship"` fails
/// (users shouldn't poke at request-context plumbing).
pub(crate) const ZEROSHIP_INTERNAL_MODULE_JS: &str = r#"
// Bootstrap-only — NOT stable API. Users should not import this.
export function __bindRequest(ctx, request) {
    if (ctx == null) {
        __zs_bind_request_ctx(null);
        return;
    }
    // Attach the Request object to ctx so getRequest() can return it.
    ctx.__zs_request = request;
    __zs_bind_request_ctx(ctx);
}
"#;

/// Runtime-injected bootstrap module. Becomes the new entry (`index.js`),
/// wrapping the user's original entry (renamed internally to `__user__.js`).
///
/// The kernel invokes one of three entry points per request:
///   - `user.default.rpc(name, input, ctx)`  for `/_zs/v1/<id>` (when set)
///   - `user.default.fetchFast(method, url, body, env)`  for non-RPC paths (when set)
///   - `user.default.fetch(request, env, ctx)`  WinterCG slow path (always)
///
/// The synthetic SSR entry (emitted by `@zeroship/vite-plugin`) provides
/// `default.{fetch, rpc}` and owns the `/_zs/v1/<id>` wire dispatch. Raw
/// user code (no plugin) can export any subset; the bootstrap forwards
/// whichever are present.
///
/// This module exists to:
///   1. **WS-subscription dispatch**: WebSocket upgrades on `/_zs/v1/<id>`
///      reach `_zsAcceptSubscription` → `dispatchSubscription` →
///      `user.default.rpc(name, input, ctx)`. The kernel itself doesn't
///      know about subscriptions — they ride entirely on user-space JS
///      over the existing WebSocketPair primitive.
///   2. **Bare-app fallback**: when the user's module doesn't export
///      `default.fetch`, return a 404 (or the legacy `user.index()`
///      convention for hand-written test apps).
///   3. **Request-context binding**: before invoking user code, the
///      kernel stashes the ctx + Request so nested modules can call
///      `getRequest()` without threading the request everywhere.
///
/// The `default` export shape is `{ fetch, rpc, fetchFast, subscribe }`
/// — `rpc` is the standalone RPC entry (kernel calls it directly when
/// the URL matches /_zs/v1/<id>; symmetric to `default.fetch`),
/// `fetchFast` is the optional zeroship-extension HTTP fast path for
/// non-RPC traffic, and `subscribe` is the WS-subscription dispatcher.
pub(crate) const BOOTSTRAP_JS: &str = r##"
import * as user from "./__user__.js";
import { __bindRequest } from "zeroship/internal";

// Vercel AI-SDK Data Stream Protocol encoder.
//
// Each line is `<typeId>:<json>\n`. TypeIds we emit:
//   0:"text"           — text part (string yield)
//   2:[<json>]         — typed object yield (the array shape matches
//                        the AI-SDK convention: a yield is one
//                        element of a streaming array)
//   e:{...}            — structured error envelope (zeroship extension;
//                        the AI-SDK parser tolerates unknown ids)
//   d:{}               — done
//
// `outputIsString`, when truthy, forces every yield to the `0:` lane —
// even non-string values get coerced via String(). Set by callers that
// know the procedure's declared output schema is a string. When
// undefined, we per-value-typeof: strings go to `0:`, anything else
// goes to `2:`.
//
// The async generator's `return` value (vs yields) is intentionally
// dropped on the floor — the AI-SDK protocol has no equivalent. If the
// creator wants a final value distinguished from yields, they emit it
// as the last `yield` and `return undefined`.
function sseFromAsyncGen(gen, outputIsString) {
    const encoder = new TextEncoder();
    const body = new ReadableStream({
        async start(controller) {
            try {
                while (true) {
                    const step = await gen.next();
                    if (step.done) {
                        controller.enqueue(encoder.encode("d:{}\n"));
                        break;
                    }
                    const v = step.value;
                    if (outputIsString || typeof v === "string") {
                        controller.enqueue(encoder.encode("0:" + JSON.stringify(String(v)) + "\n"));
                    } else {
                        controller.enqueue(encoder.encode("2:[" + JSON.stringify(v) + "]\n"));
                    }
                }
            } catch (e) {
                // `e:` carries the structured error envelope — keeps
                // the SSE error frame field-compatible with the unary
                // error body so clients can share a single parser.
                // `code` and `retryable` are type-checked; `details`
                // is forwarded as-is when present.
                const env = {
                    message: (e && e.message) || String(e),
                    name:    (e && e.name)    || "Error",
                };
                if (e && typeof e.code === "string") env.code = e.code;
                if (e && e.details !== undefined)    env.details = e.details;
                if (e && typeof e.retryable === "boolean") env.retryable = e.retryable;
                controller.enqueue(encoder.encode("e:" + JSON.stringify(env) + "\n"));
                controller.enqueue(encoder.encode("d:{}\n"));
            } finally {
                controller.close();
            }
        },
    });
    return new Response(body, {
        status: 200,
        headers: {
            "Content-Type": "text/event-stream",
            "Cache-Control": "no-cache, no-transform",
            "X-Accel-Buffering": "no",
        },
    });
}

function errorResponse(err) {
    const status = Number.isInteger(err && err.status) && err.status >= 400 && err.status < 600
        ? err.status : 500;
    const envelope = {
        message: (err && err.message) ? err.message : String(err),
        name: (err && err.name) ? err.name : "Error",
    };
    // Forward the structured-error envelope when the throw carries it.
    // `code` is gRPC-style ("INVALID_ARGUMENT", "UNAUTHENTICATED", ...)
    // and `retryable` is a hint for clients deciding whether to retry.
    // `details` is any JSON value — the SDK validation paths (e.g. Zod)
    // attach the issues array here. Only string codes / boolean
    // retryable are forwarded; other types are dropped so the wire
    // contract can't be stretched by accident.
    if (err && typeof err.code === "string") envelope.code = err.code;
    if (err && err.details !== undefined) envelope.details = err.details;
    if (err && typeof err.retryable === "boolean") envelope.retryable = err.retryable;
    return new Response(JSON.stringify(envelope), {
        status,
        headers: { "Content-Type": "application/json" },
    });
}

// ── Phase 7 — Subscription wire dispatch ─────────────────────────────────
//
// Subscription procedures are async generators wired over WebSocket per
// `docs/proposals/rpc-v2.md` §6 (Subscription wire). Frame protocol:
//
//   Client → server (first frame after upgrade):
//     {"t":"hello","input":<json>}
//
//   Server → client:
//     {"t":"data","value":<json>}    each yield
//     {"t":"error","error":<env>}    on handler throw (envelope is
//                                    field-compatible with errorResponse)
//     {"t":"end"}                    normal completion
//     {"t":"ping"} / {"t":"pong"}    keepalive (either direction)
//
// Pings: server sends every 30s; if a pong doesn't come back within 60s
// we close 4408. Hello timeout: 5s after upgrade or close 4400.
//
// Implementation note: this runs entirely in user-space JS over the
// existing WebSocketPair primitive — the kernel itself doesn't know
// about subscriptions. The synthetic SSR entry detects WS-upgrade
// requests on `/_zs/v1/<id>` and calls into `dispatchSubscription` to
// hand off the server side of the pair. For the bootstrap fallback
// (apps without the synthetic entry) we expose the same handler so
// runtime tests + bare-bone apps can wire WS subscriptions directly.
function _zsSubError(err) {
    const env = {
        message: (err && err.message) || String(err),
        name:    (err && err.name)    || "Error",
    };
    if (err && typeof err.code === "string") env.code = err.code;
    if (err && err.details !== undefined)    env.details = err.details;
    if (err && typeof err.retryable === "boolean") env.retryable = err.retryable;
    return env;
}

// Run an async iterator over `ws`. Emits `{"t":"data",value}` per yield,
// `{"t":"end"}` on normal completion, `{"t":"error",error}` on throw.
// Closes the socket cleanly afterward. Stops emitting when the socket
// is no longer open (client closed). Calls `gen.return()` on early exit
// so handler-side cleanup runs (clear timers, close DB watch, etc.).
async function _zsRunSubscriptionGen(gen, ws) {
    try {
        while (true) {
            // Cheap closed-check before pulling the next value — saves
            // the (potentially expensive) async-gen step when the
            // client is already gone.
            if (ws.readyState !== 1 /* OPEN */) {
                try { await gen.return(undefined); } catch (_e) {}
                return;
            }
            let step;
            try {
                step = await gen.next();
            } catch (err) {
                if (ws.readyState === 1) {
                    try { ws.send(JSON.stringify({ t: "error", error: _zsSubError(err) })); } catch (_) {}
                    try { ws.close(1011, ""); } catch (_) {}
                }
                return;
            }
            if (step.done) {
                if (ws.readyState === 1) {
                    try { ws.send(JSON.stringify({ t: "end" })); } catch (_) {}
                    try { ws.close(1000, ""); } catch (_) {}
                }
                return;
            }
            if (ws.readyState !== 1) {
                try { await gen.return(undefined); } catch (_e) {}
                return;
            }
            try {
                ws.send(JSON.stringify({ t: "data", value: step.value }));
            } catch (_) {
                try { await gen.return(undefined); } catch (_e) {}
                return;
            }
        }
    } catch (err) {
        // Defensive: any unexpected throw above bubbles here.
        if (ws && ws.readyState === 1) {
            try { ws.send(JSON.stringify({ t: "error", error: _zsSubError(err) })); } catch (_) {}
            try { ws.close(1011, ""); } catch (_) {}
        }
    }
}

// dispatchSubscription — entry point for WS-subscription procedures.
//
// `methodName` is the wireId. `input` is the parsed input value
// (already pulled out of the `{"t":"hello"}` frame by the caller).
// `ws` is the *server-side* WebSocket of the pair created by the
// caller; it's already been .accept()ed before we're invoked.
//
// Routes through `user.default.rpc(name, input, ctx)` — the symmetric
// shape the synthetic SSR entry exposes. Subscription procedures are
// async generators; the handler returns the iterator directly.
async function dispatchSubscription(methodName, input, ws) {
    try {
        if (!user.default || typeof user.default.rpc !== "function") {
            throw Object.assign(
                new Error("default.rpc not exported — subscription requires the synthetic SSR entry"),
                { code: "INTERNAL" },
            );
        }
        const gen = await user.default.rpc(methodName, input);
        if (gen == null || typeof gen !== "object"
            || typeof gen[Symbol.asyncIterator] !== "function"
            || typeof gen.next !== "function") {
            throw Object.assign(new Error("Subscription handler must return an async iterator"), {
                code: "INTERNAL",
            });
        }
        await _zsRunSubscriptionGen(gen, ws);
    } catch (err) {
        if (ws && ws.readyState === 1 /* OPEN */) {
            try { ws.send(JSON.stringify({ t: "error", error: _zsSubError(err) })); } catch (_) {}
            try { ws.close(1011, ""); } catch (_) {}
        }
    }
}

// Build a Response that completes the WS-subscription upgrade. Spawns
// the subscription pump as a side effect — the pump reads the `hello`
// frame, runs the generator, and writes data frames; the kernel
// handles the WS handshake from the returned Response.
//
// `urlStr` is the request URL (used to extract the wireId after the
// `/_zs/v1/` prefix). On structural failure (bad URL, missing method)
// we return an HTTP error response — the kernel will write that
// instead of upgrading.
function _zsAcceptSubscription(urlStr) {
    let methodName = null;
    try {
        const u = new URL(urlStr);
        const m = u.pathname.match(/^\/_zs\/v1\/(.+)$/);
        if (m) methodName = decodeURIComponent(m[1]);
    } catch (_e) {}
    if (!methodName) {
        return new Response('{"message":"missing wireId","name":"Error"}', {
            status: 400, headers: { "Content-Type": "application/json" },
        });
    }

    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    server.accept();

    // Hello timeout — close 4400 if the client doesn't send `hello`
    // within 5s. The first message handler clears this.
    let helloTimer = setTimeout(() => {
        if (server.readyState === 1) {
            try { server.close(4400, "missing hello"); } catch (_) {}
        }
    }, 5000);

    let pingTimer = null;
    let pongDeadline = null;
    function startPings() {
        // Server sends ping every 30s. If client doesn't pong within
        // 60s we close 4408.
        pingTimer = setInterval(() => {
            if (server.readyState !== 1) { clearInterval(pingTimer); return; }
            try { server.send(JSON.stringify({ t: "ping" })); } catch (_) {}
            if (pongDeadline === null) {
                pongDeadline = setTimeout(() => {
                    if (server.readyState === 1) {
                        try { server.close(4408, "pong timeout"); } catch (_) {}
                    }
                }, 60000);
            }
        }, 30000);
    }

    // Wire the message router. The first message must be `hello`; any
    // other shape closes the connection 4400.
    let started = false;
    server.addEventListener("message", function(ev) {
        let msg;
        try { msg = JSON.parse(typeof ev.data === "string" ? ev.data : String(ev.data)); }
        catch (_e) {
            try { server.close(4400, "invalid JSON"); } catch (_) {}
            return;
        }
        if (!msg || typeof msg.t !== "string") {
            try { server.close(4400, "missing frame tag"); } catch (_) {}
            return;
        }
        if (msg.t === "ping") {
            try { server.send(JSON.stringify({ t: "pong" })); } catch (_) {}
            return;
        }
        if (msg.t === "pong") {
            if (pongDeadline !== null) { clearTimeout(pongDeadline); pongDeadline = null; }
            return;
        }
        if (!started && msg.t === "hello") {
            started = true;
            clearTimeout(helloTimer); helloTimer = null;
            startPings();
            // Kick off the dispatcher — fire-and-forget; errors are
            // funnelled into ws frames inside dispatchSubscription.
            dispatchSubscription(methodName, msg.input, server);
            return;
        }
        // Frames after hello (other than ping/pong) are not part of
        // the protocol — drop silently for forward-compat.
    });

    // On close, stop timers + ensure the generator gets a chance to
    // tear down. (The generator's own `ws.readyState !== 1` check
    // catches this on the next iteration.)
    server.addEventListener("close", function() {
        if (helloTimer !== null) { clearTimeout(helloTimer); helloTimer = null; }
        if (pingTimer !== null) { clearInterval(pingTimer); pingTimer = null; }
        if (pongDeadline !== null) { clearTimeout(pongDeadline); pongDeadline = null; }
    });

    // Return the WS-upgrade response — kernel writes the 101 + handshake
    // headers from this and pumps frames between TCP and the pair.
    return new Response(null, {
        status: 101,
        webSocket: client,
        headers: { "Sec-WebSocket-Protocol": "zs.v1" },
    });
}

// Detect a WS-upgrade request. The kernel/gateway already gates on
// these — we re-check on the JS side so the bootstrap's fallback
// router doesn't need to trust the path alone.
function _zsIsWsUpgrade(request) {
    if (request.method !== "GET") return false;
    const upgrade = request.headers.get("upgrade");
    if (!upgrade || upgrade.toLowerCase() !== "websocket") return false;
    return true;
}

// Resolve the user's default.fetch once at module init. When present, we
// export it directly as our `default.fetch` — no wrapper, no extra async
// frame, no extra try/catch. The kernel's `call_fetch_inner` already
// turns thrown exceptions into `DispatchResult::ErrorValue` with the
// correct HTTP status (honoring `err.status`), so a JS-side try/catch
// here would just add cost. This is the single biggest per-fetch win
// after dropping the URL parse and __bindRequest.
const USER_FETCH = (user && user.default && typeof user.default.fetch === "function")
    ? user.default.fetch
    : null;

// Optional zeroship extension: `user.default.fetchFast(method, url, body, env)`.
// Opt-in handler that bypasses the Request/Response construction entirely.
// Returns one of:
//   - { status, headers, body } plain object → HTTP response
//   - string / Uint8Array → 200 OK + that body
//   - null → kernel falls back to the slow `fetch(request, env, ctx)` path
// Kernel dispatches to this for non-/_zs/v1/<id> traffic when the user
// module exports it. /_zs/v1/<id> requests go through `default.rpc`
// instead — fetchFast and rpc are siblings, not layered.
const USER_FETCH_FAST = (user && user.default && typeof user.default.fetchFast === "function")
    ? user.default.fetchFast
    : null;

// Standalone RPC entry — the kernel calls this directly when the URL
// matches /_zs/v1/<id>, bypassing Request/URL construction. Symmetric
// to fetch — independent kernel entry point. Returned values get
// envelope-wrapped on the wire by the kernel; promises get awaited;
// async iterators fall through to the slow path's stream encoder.
const USER_RPC = (user && user.default && typeof user.default.rpc === "function")
    ? user.default.rpc
    : null;

const FALLBACK_ZS_V1_TAG = "/_zs/v1/";

// Coerce a user-supplied page handler return value into a Response.
// Strings/null are wrapped as text/html. Response is passed through.
function coerceToHtmlResponse(result, status) {
    if (result instanceof Response) return result;
    if (result == null) return null;
    return new Response(String(result), {
        status: status ?? 200,
        headers: { "Content-Type": "text/html; charset=utf-8" },
    });
}

// Fallback fetch — used only when the user's module doesn't export a
// default.fetch handler. Handles:
//   - /_zs/v1/<id>    + WS upgrade  → dispatchSubscription
//   - GET /           → user.index() if exported, returns HTML
//   - else            → 404
//
// Unary /_zs/v1/<id> requests fall through here when there's no
// `default.fetch`; we 404. Real apps ship via the synthetic SSR
// entry which exports `default.{fetch, rpc}` and handles them.
async function fallbackFetch(request) {
    const urlStr = request.url;

    const zsIdx = urlStr.indexOf(FALLBACK_ZS_V1_TAG);
    if (zsIdx >= 0 && _zsIsWsUpgrade(request)) {
        return _zsAcceptSubscription(urlStr);
    }

    // GET / (or any path) → user.index() convention. The export
    // returns HTML (string or Response). Lets RPC-only apps still
    // render a UI without forcing creators to handle URL routing.
    if (request.method === "GET" && typeof user.index === "function") {
        try {
            const url = new URL(request.url);
            if (url.pathname === "/" || url.pathname === "") {
                const result = await user.index(request);
                const resp = coerceToHtmlResponse(result, 200);
                if (resp) return resp;
            }
        } catch (err) {
            return errorResponse(err);
        }
    }

    return new Response(
        '{"message":"Not Found","name":"Error"}',
        { status: 404, headers: { "Content-Type": "application/json" } }
    );
}

export default {
    // Phase 7: subscription dispatch. Caller hands in (name, input,
    // server-side WebSocket already accepted). Used by the kernel's
    // WS-upgrade path; tests can drive this directly via the
    // runtime's WebSocketPair primitive.
    subscribe: dispatchSubscription,
    // Standalone RPC entry. When set, the kernel calls this directly
    // for /_zs/v1/<id> requests and never builds a Request object.
    // Symmetric to fetch — independent kernel entry, not layered.
    rpc: USER_RPC,
    // Zeroship extension: non-WinterCG fast HTTP dispatch for non-RPC
    // traffic. Kernel calls this with raw (method, url, body, env).
    // User returns a plain response shape or null to fall through to
    // fetch(). Skips Request/Response construction — hot-path-only win.
    fetchFast: USER_FETCH_FAST,
    // Standard WinterCG fetch handler — the user's default.fetch
    // directly (no bootstrap wrapper). Catches everything not handled
    // by `rpc` or `fetchFast`.
    fetch: USER_FETCH || fallbackFetch,
};
"##;

// ===========================================================================
// Shared initialization: polyfills + module loading
// ===========================================================================

/// Load polyfills and ES modules.
///
/// Shared by both `Isolate::ensure_initialized` and `ConcurrentIsolate::ensure_initialized`.
/// Returns the entry module's namespace object (so the caller can resolve
/// `default.fetch` without a reach-through global). `None` if module loading
/// failed (error is already logged).
///
/// The `plugins` slice is accepted but not currently invoked here — the
/// `zeroship.*` facade was removed as part of the kernel-cut refactor
/// (PR 1 Task D1). Plugins will be re-exposed via the bootstrap's `env.*`
/// binding in PR 3; the parameter is kept so call sites don't have to
/// change in this PR.
pub fn load_polyfills_and_modules(
    scope: &mut v8::PinScope,
    modules: &[crate::modules::ModuleEntry],
    _plugins: &[std::sync::Arc<dyn crate::plugin::NativePlugin>],
) -> Result<v8::Global<v8::Value>, String> {
    setup_globals(scope);

    // Order matters here:
    //
    //   1. Load fetch.js / formdata.js / blob.js polyfills first. With
    //      the native gate ON they're shadowed below; with the gate OFF
    //      (legacy build) they remain in charge.
    //   2. install_dom installs native DOM (EventTarget / Event /
    //      CustomEvent / AbortController / AbortSignal / FormData /
    //      Request / Response / fetch). MUST run AFTER fetch.js /
    //      formdata.js so their unconditional `globalThis.X = X`
    //      assignments don't overwrite the native install.
    //   3. Load WEBSOCKET_JS LAST so its `WebSocket.prototype =
    //      Object.create(EventTarget.prototype)` captures the NATIVE
    //      EventTarget prototype (not the polyfill's). Otherwise
    //      `WebSocket` instances inherit polyfill `addEventListener`
    //      which expects `this._listeners`, but `EventTarget.call(this)`
    //      runs the native constructor that doesn't set that field —
    //      `addEventListener` then throws "Cannot read properties of
    //      undefined (reading 'message')" on the first server frame.
    //   4. Native Headers / Streams / TextEncoderStream wrappers.
    // Native URL + URLSearchParams (ada-url backed). Install BEFORE the
    // fetch.js polyfill so its DOMException + stream-bridge code sees the
    // native URL class.
    install_url_native(scope);

    for polyfill in [FETCH_JS, CRYPTO_JS, NODE_GLOBALS_JS] {
        let code = v8::String::new(scope, polyfill).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        script.run(scope).unwrap();
    }

    // Native Headers per WHATWG Fetch §2.2 — installed unconditionally
    // after fetch.js so the polyfill (which no longer defines Headers
    // itself) can lean on the native class for Request/Response
    // construction.
    install_headers(scope);

    // Native WHATWG Streams (D-19, design
    // `docs/proposals/streams-native.md`). ReadableStream, WritableStream,
    // TransformStream, *Controller, *Reader, *Writer, BYOBReader,
    // BYOBRequest, and the async-iter prototype patches are all
    // native-backed.
    install_native_streams(scope);

    // Native Blob + File per WHATWG File API. Replaces the
    // `embed/blob.js` polyfill. Installed AFTER native streams
    // because `Blob.stream()` constructs a `new ReadableStream(...)`
    // through the user-visible class.
    install_blob_native(scope);

    // Native TextEncoderStream / TextDecoderStream. Replaces the
    // `embed/text-streams.js` shim — closes spec gaps around
    // Symbol.toStringTag, brand-checked accessors, and the
    // GenericTransformStream §6.1 prototype-side getter shape. Loaded
    // AFTER native streams install (needs `globalThis.TransformStream`)
    // and AFTER `setup_globals` (needs `TextEncoder` / `TextDecoder`).
    install_text_encoding_streams(scope);

    // Native DOM (EventTarget / Event / CustomEvent / AbortController /
    // AbortSignal / FormData / Request / Response / fetch). MUST run
    // AFTER fetch.js / formdata.js or their unconditional re-assignment
    // would clobber the native install — and BEFORE websocket.js so
    // `WebSocket.prototype = Object.create(EventTarget.prototype)` picks
    // up the native EventTarget prototype.
    install_dom(scope);

    // WebSocket polyfill — loaded LAST so its prototype chain references
    // the native EventTarget (install_dom installed it just above).
    {
        let code = v8::String::new(scope, WEBSOCKET_JS).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        script.run(scope).unwrap();
    }

    // Wrap the user's module graph in the bootstrap entry.
    //
    // Layout after wrapping:
    //   entries[0] = "index.js"            — BOOTSTRAP_JS (the new entry)
    //   entries[1] = "__user__.js"         — user's original entry (source preserved)
    //   entries[2] = "zeroship"            — env / waitUntil / getRequest facade
    //   entries[3] = "zeroship/internal"   — bootstrap-only __bindRequest
    //   entries[4..] = user's other modules (unchanged specifiers)
    //
    // The load_modules walker compiles BOOTSTRAP_JS first, discovers its two
    // imports (`./__user__.js` + `zeroship/internal`) and transitively the
    // user's `zeroship` imports, then instantiates + evaluates the bootstrap.
    // The returned namespace is the bootstrap's, so ensure_initialized reads
    // `default.fetch` off the bootstrap (not the user module) — exactly the
    // indirection we want.
    let wrapped = wrap_with_bootstrap(modules);

    // Load ES modules and return the entry module's namespace object.
    // The kernel reads `default.fetch` directly off the namespace — no more
    // `__rpc` copy loop, no more `DISPATCH_JS`, no more URL-path router.
    match crate::modules::load_modules(scope, &wrapped) {
        Ok(namespace) => Ok(namespace),
        Err(e) => {
            eprintln!("[v8] Module loading failed: {e}");
            Err(e)
        }
    }
}

/// Rewrite the user's module list so the bootstrap is the new entry.
///
/// The user's declared first module is renamed to `__user__.js`; a synthetic
/// `index.js` (BOOTSTRAP_JS) is prepended as the new entry, plus the two
/// zeroship modules (`zeroship` and `zeroship/internal`).
///
/// **Collision**: the compiler always emits `index.js` as the user's entry,
/// so a user entry actually named `__user__.js` is a bug if it happens. A
/// `debug_assert!` catches this in dev builds; in release it's silently
/// overwritten (the user module's source wins over our internal specifier
/// by virtue of ordering in the sources map).
fn wrap_with_bootstrap(
    modules: &[crate::modules::ModuleEntry],
) -> Vec<crate::modules::ModuleEntry> {
    use crate::modules::ModuleEntry;

    // Empty input preserved as-is — the module loader will return a clean
    // "No modules to load" error. Don't synthesize a bootstrap pointing at
    // a non-existent `__user__.js`.
    if modules.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<ModuleEntry> = Vec::with_capacity(modules.len() + 3);

    // entry 0: bootstrap becomes the new entrypoint under "index.js".
    out.push(ModuleEntry {
        specifier: "index.js".into(),
        source: BOOTSTRAP_JS.into(),
    });

    // entry 1: user's original entry, renamed to "__user__.js". Its own
    // declared specifier (usually "index.js") is discarded — the bootstrap
    // imports `./__user__.js` by exact name.
    let user_entry = &modules[0];
    debug_assert!(
        user_entry.specifier != "__user__.js",
        "User entry collides with bootstrap's internal specifier",
    );
    out.push(ModuleEntry {
        specifier: "__user__.js".into(),
        source: user_entry.source.clone(),
    });

    // entries 2-3: the zeroship facade + internal modules. Live in the
    // module graph alongside the user's modules so `import ... from "zeroship"`
    // resolves via the normal lookup path.
    out.push(ModuleEntry {
        specifier: "zeroship".into(),
        source: ZEROSHIP_MODULE_JS.into(),
    });
    out.push(ModuleEntry {
        specifier: "zeroship/internal".into(),
        source: ZEROSHIP_INTERNAL_MODULE_JS.into(),
    });

    // Remaining user modules — pass through unchanged. Their declared
    // specifiers (other than "index.js" which can't collide since we moved
    // the user entry) stay valid for their own cross-module imports.
    for entry in modules.iter().skip(1) {
        out.push(entry.clone());
    }

    out
}

// ===========================================================================
// Console polyfill (variadic — stays manual)
// ===========================================================================

/// Max bytes retained for a single `console.log` line. Protects the
/// per-request log vector (shipped back to the gateway) and the operator's
/// stderr from an app doing `console.log(hugeString)` in a loop.
const CONSOLE_LINE_MAX: usize = 4096;

/// Truncate a console line to `CONSOLE_LINE_MAX` bytes, preserving a valid
/// UTF-8 boundary and appending a truncation marker so operators can tell.
fn truncate_console_line(mut line: String) -> String {
    if line.len() <= CONSOLE_LINE_MAX {
        return line;
    }
    // `floor_char_boundary` isn't stable, so walk back from the cap to the
    // nearest char boundary manually.
    let mut cut = CONSOLE_LINE_MAX;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    line.truncate(cut);
    line.push_str("…[truncated]");
    line
}

fn console_log_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let mut parts = Vec::new();
    for i in 0..args.length() {
        let arg = args.get(i);
        let s = arg.to_rust_string_lossy(scope);
        parts.push(s);
    }
    let line = truncate_console_line(parts.join(" "));

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let mut s = state.borrow_mut();
    let req_id = s.executing_request_id;

    // Operator-visible mirror on stderr (not stdout — stdout should stay
    // clean for CLI tools that want to capture structured output). Prefix
    // with request metadata so multi-request logs are disentanglable, and
    // only enable in dev / when ZEROSHIP_LOG is set.
    if std::env::var("ZEROSHIP_LOG").is_ok() || cfg!(debug_assertions) {
        match req_id {
            Some(rid) => eprintln!("[app req={rid}] {line}"),
            None => eprintln!("[app] {line}"),
        }
    }

    let logs = s.per_request_logs.entry(req_id.unwrap_or(0)).or_default();
    logs.push(line);
    if logs.len() > 1000 {
        let drain = logs.len() - 1000;
        logs.drain(..drain);
    }
}

#[cfg(test)]
mod console_tests {
    use super::*;

    #[test]
    fn short_lines_untouched() {
        assert_eq!(truncate_console_line("hello".to_string()), "hello");
    }

    #[test]
    fn long_lines_truncated() {
        let long = "x".repeat(CONSOLE_LINE_MAX + 100);
        let out = truncate_console_line(long);
        assert!(out.len() <= CONSOLE_LINE_MAX + "…[truncated]".len());
        assert!(out.ends_with("…[truncated]"));
    }

    #[test]
    fn truncation_respects_utf8_boundaries() {
        // Multi-byte char right at the boundary — truncation must not split it.
        let mut long = "x".repeat(CONSOLE_LINE_MAX - 1);
        long.push('ñ'); // 2-byte UTF-8 char straddles the boundary
        long.push_str(&"y".repeat(200));
        let out = truncate_console_line(long);
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}

// ===========================================================================
// queueMicrotask — schedules a callback to run after current JS completes
// ===========================================================================

fn queue_microtask_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if args.length() < 1 || !args.get(0).is_function() {
        return;
    }
    let func = v8::Local::<v8::Function>::try_from(args.get(0)).unwrap();
    // Schedule via Promise.resolve().then(callback)
    // This enqueues the callback as a microtask that runs at the next checkpoint.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let undefined = v8::undefined(scope);
    resolver.resolve(scope, undefined.into());
    promise.then(scope, func);
}

// ===========================================================================
// performance.now — high-resolution monotonic timestamp in milliseconds
// ===========================================================================

/// Start time for performance.now() — set once per isolate.
/// `performance.now()` — per-isolate high-resolution clock.
///
/// Each app gets its own epoch (stored as `perf_epoch` in `RuntimeState`)
/// so one app cannot observe when another app's requests started, how
/// long they took, or when the V8 thread was busy serving someone else.
///
/// An earlier revision used a `static OnceLock<Instant>` shared across
/// the entire process — all apps saw the same time origin and could
/// derive each other's scheduling patterns via differential timing.
fn performance_now_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: crate::state::SharedState = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let epoch = state.borrow().perf_epoch;
    let elapsed_ms = epoch.elapsed().as_secs_f64() * 1000.0;
    rv.set(v8::Number::new(scope, elapsed_ms).into());
}

// ===========================================================================
// __zs_env — return the current frozen env snapshot
// ===========================================================================

/// `__zs_env()` — returns the composite env object (plugin namespaces +
/// scalar env JSON).
///
/// Built once by `RuntimeInner::ensure_initialized` and cached on
/// `RuntimeState.env_obj`. Every call returns the same V8 Global so SDK
/// code importing `env` from the `zeroship` module sees the same object
/// identity as the `env` arg of `fetch(req, env, ctx)`.
///
/// Fallback: if called before `ensure_initialized` completed (shouldn't
/// happen under the normal dispatch path, but be defensive), JSON-parse
/// the scalar snapshot instead of panicking — plugin namespaces will be
/// missing but at least the scalar values are visible.
fn zs_env_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let env_opt = state.borrow().env_obj.clone();
    match env_opt {
        Some(env_global) => {
            let env_local = v8::Local::new(scope, env_global);
            rv.set(env_local.into());
        }
        None => {
            // Degraded path — `ensure_initialized` hasn't yet built the
            // composite object. Hand back a flat merged map of vars +
            // secrets (secrets win on collision) so SDK code can still
            // read scalar values; plugin namespaces will be missing
            // until the next request triggers init.
            let (vars, secrets) = {
                let s = state.borrow();
                (s.env_app_vars.clone(), s.env_app_secrets.clone())
            };
            let env_obj = v8::Object::new(scope);
            for (k, v) in vars.iter().chain(secrets.iter()) {
                let k_v8 = v8::String::new(scope, k).unwrap();
                let v_v8 = v8::String::new(scope, v).unwrap();
                env_obj.set(scope, k_v8.into(), v_v8.into());
            }
            rv.set(env_obj.into());
        }
    }
}

// ===========================================================================
// __zs_bind_request_ctx / __zs_get_request_ctx — per-request ctx stash
// ===========================================================================

/// `__zs_bind_request_ctx(ctxObj)` — stash the JS `ctx` object on the
/// currently-executing request so nested modules can look it up without
/// threading it through every call. Called by the bootstrap (PR 2)
/// immediately on entry to `fetch(req, env, ctx)`. Passing `null` clears
/// the stash; passing a non-object is a silent no-op.
fn zs_bind_request_ctx_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let rid_opt = state.borrow().executing_request_id;
    let Some(rid) = rid_opt else {
        // No active request — silently ignore. Bootstrap should never
        // call this outside a request, but defensive no-op is safer
        // than a throw.
        return;
    };

    let arg = args.get(0);
    if arg.is_null() || arg.is_undefined() {
        // __zs_bind_request_ctx(null) clears the stashed ctx.
        state.borrow_mut().request_ctx_by_id.remove(&rid);
        return;
    }
    if !arg.is_object() {
        // Non-null, non-object — ignore (type error from JS side
        // would be appropriate but silent for now).
        return;
    }
    let obj: v8::Local<v8::Object> = arg.try_into().unwrap();
    let global_obj = v8::Global::new(scope, obj);
    state.borrow_mut().request_ctx_by_id.insert(rid, global_obj);
}

/// `__zs_wait_until(promise)` — push a Promise onto the current request's
/// waitUntil bag. The kernel keeps the isolate alive past the response body
/// write until every promise here settles (or the wall timeout fires).
///
/// Type-checking and TypeError on non-Promise args is done in the JS-side
/// `zeroship.waitUntil` wrapper; this op defensively no-ops on bad input so
/// a JS-side bug can't crash the isolate. Silent no-op when called outside
/// an active request — the JS side already checks and doesn't call us in
/// that case, but be conservative for robustness.
fn zs_wait_until_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let arg = args.get(0);
    if !arg.is_promise() {
        return;
    }
    let promise: v8::Local<v8::Promise> = arg.try_into().unwrap();
    let global = v8::Global::new(scope, promise);
    let _registered = state.borrow_mut().register_wait_until(global);
    // If register_wait_until returned false there's no active request —
    // drop the promise silently. The JS-side wrapper is the user-facing
    // contract for that case.
}

/// `__zs_get_request_ctx()` — return the stashed `ctx` object for the
/// currently-executing request, or `null` if none was bound (no active
/// request, or bootstrap hasn't run). Returns the exact same object
/// reference passed to `__zs_bind_request_ctx` — not a clone.
fn zs_get_request_ctx_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let rid_opt = state.borrow().executing_request_id;
    let Some(rid) = rid_opt else {
        rv.set(v8::null(scope).into());
        return;
    };
    let ctx_opt = state.borrow().request_ctx_by_id.get(&rid).cloned();
    match ctx_opt {
        Some(ctx_global) => {
            let ctx_local = v8::Local::new(scope, ctx_global);
            rv.set(ctx_local.into());
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

/// `__zs_get_request()` — return the Request JS object for the current
/// in-flight request, or `null` if none (e.g. the RPC fast-path doesn't
/// construct a Request since there's no URL/header work to do).
///
/// The kernel stores the Request at call_fetch_handler's slow-path entry,
/// immediately after it constructs one via HTTP_CREATE_REQUEST_JS. Stored
/// keyed by the same `executing_request_id` that drives per_request_user
/// / waitUntil / logs, so cleanup rides on `drain_request_logs`.
fn zs_get_request_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let rid_opt = state.borrow().executing_request_id;
    let Some(rid) = rid_opt else {
        rv.set(v8::null(scope).into());
        return;
    };
    let req_opt = state.borrow().request_by_id.get(&rid).cloned();
    match req_opt {
        Some(req_global) => {
            let local = v8::Local::new(scope, req_global);
            rv.set(local.into());
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

// ===========================================================================
// Timer callbacks (take v8::Function args — stays manual)
// ===========================================================================

fn set_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: crate::state::SharedState = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let callback = match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "setTimeout: first argument must be a function")
                .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let ms = if args.length() > 1 {
        args.get(1).uint32_value(scope).unwrap_or(0)
    } else {
        0
    };

    // Admission control: reject before allocating V8 handles / state.
    {
        let s = state.borrow();
        if s.timer_callbacks.len() >= crate::state::MAX_PENDING_TIMERS {
            drop(s);
            let msg = v8::String::new(
                scope,
                &format!("Too many pending timers (limit: {})", crate::state::MAX_PENDING_TIMERS),
            ).unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    }

    let global_cb = v8::Global::new(scope, callback);
    let delay = Duration::from_millis(u64::from(ms));

    let mut s = state.borrow_mut();
    let id = s.next_timer_id;
    s.next_timer_id += 1;
    s.timer_callbacks.insert(id, TimerCallback { callback: global_cb, interval: None });
    if let Some(req_id) = s.executing_request_id {
        s.timer_owner.insert(id, req_id);
    }
    if delay < Duration::from_millis(1) {
        s.ready_timers.push_back(id);
    } else {
        s.spawned_timers.push(crate::state::SpawnedTimer { id, delay, interval: None });
    }

    rv.set(v8::Integer::new(scope, id as i32).into());
}

fn clear_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state: crate::state::SharedState = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let id = if args.length() > 0 {
        args.get(0).uint32_value(scope).unwrap_or(0)
    } else {
        return;
    };

    let mut s = state.borrow_mut();
    s.timer_callbacks.remove(&id);
    s.timer_owner.remove(&id);
    // The tokio::time::sleep future will still fire but handle_timer()
    // will find no callback and do nothing.
}

fn set_interval_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: crate::state::SharedState = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let callback = match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "setInterval: first argument must be a function")
                .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let ms = if args.length() > 1 {
        args.get(1).uint32_value(scope).unwrap_or(0)
    } else {
        0
    };
    let delay = Duration::from_millis(u64::from(ms));

    // Same admission control as setTimeout.
    {
        let s = state.borrow();
        if s.timer_callbacks.len() >= crate::state::MAX_PENDING_TIMERS {
            drop(s);
            let msg = v8::String::new(
                scope,
                &format!("Too many pending timers (limit: {})", crate::state::MAX_PENDING_TIMERS),
            ).unwrap();
            let exc = v8::Exception::range_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    }

    let global_cb = v8::Global::new(scope, callback);
    let mut s = state.borrow_mut();
    let id = s.next_timer_id;
    s.next_timer_id += 1;
    s.timer_callbacks.insert(id, TimerCallback { callback: global_cb, interval: Some(delay) });
    if let Some(req_id) = s.executing_request_id {
        s.timer_owner.insert(id, req_id);
    }
    s.spawned_timers.push(crate::state::SpawnedTimer { id, delay, interval: Some(delay) });

    rv.set(v8::Integer::new(scope, id as i32).into());
}

// ===========================================================================
// Setup all globals on a V8 context
// ===========================================================================

/// Install console, timers, fetch, URL, KV, crypto, env on the global object.
///
/// Callbacks from `#[zeroship_op]` modules are referenced as `crate::{mod}::{fn}_callback`.
pub fn setup_globals(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);

    // global = globalThis (Node.js compat — many npm packages reference `global`)
    {
        let key = v8::String::new(scope, "global").unwrap();
        global.set(scope, key.into(), global.into());
    }

    // console.log/warn/error/info
    {
        let console = v8::Object::new(scope);
        let log_fn = v8::Function::new(scope, console_log_callback).unwrap();
        let log_key = v8::String::new(scope, "log").unwrap();
        console.set(scope, log_key.into(), log_fn.into());

        let warn_key = v8::String::new(scope, "warn").unwrap();
        console.set(scope, warn_key.into(), log_fn.into());
        let error_key = v8::String::new(scope, "error").unwrap();
        console.set(scope, error_key.into(), log_fn.into());
        let info_key = v8::String::new(scope, "info").unwrap();
        console.set(scope, info_key.into(), log_fn.into());
        let debug_key = v8::String::new(scope, "debug").unwrap();
        console.set(scope, debug_key.into(), log_fn.into());

        let console_key = v8::String::new(scope, "console").unwrap();
        global.set(scope, console_key.into(), console.into());
    }

    // setTimeout
    {
        let f = v8::Function::new(scope, set_timeout_callback).unwrap();
        let key = v8::String::new(scope, "setTimeout").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // clearTimeout
    {
        let f = v8::Function::new(scope, clear_timeout_callback).unwrap();
        let key = v8::String::new(scope, "clearTimeout").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // setInterval
    {
        let f = v8::Function::new(scope, set_interval_callback).unwrap();
        let key = v8::String::new(scope, "setInterval").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // clearInterval (same implementation as clearTimeout)
    {
        let f = v8::Function::new(scope, clear_timeout_callback).unwrap();
        let key = v8::String::new(scope, "clearInterval").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // queueMicrotask
    {
        let f = v8::Function::new(scope, queue_microtask_callback).unwrap();
        let key = v8::String::new(scope, "queueMicrotask").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // performance.now
    {
        let perf = v8::Object::new(scope);
        let f = v8::Function::new(scope, performance_now_callback).unwrap();
        let key = v8::String::new(scope, "now").unwrap();
        perf.set(scope, key.into(), f.into());
        let perf_key = v8::String::new(scope, "performance").unwrap();
        global.set(scope, perf_key.into(), perf.into());
    }

    // navigator.userAgent
    {
        let nav = v8::Object::new(scope);
        let ua = v8::String::new(scope, "zeroship/1.0").unwrap();
        let ua_key = v8::String::new(scope, "userAgent").unwrap();
        nav.set(scope, ua_key.into(), ua.into());
        let nav_key = v8::String::new(scope, "navigator").unwrap();
        global.set(scope, nav_key.into(), nav.into());
    }

    // atob / btoa per WHATWG HTML §8.6 — replaces the old fetch.js polyfill
    // that silently dropped >0xFF code units in btoa and ignored invalid
    // base64 in atob. The native impls throw native DOMException
    // ("InvalidCharacterError") on out-of-range / malformed input.
    crate::base64::install_global(scope, global);

    // (`__rawFetch` was the V8 callback the JS polyfill in `embed/fetch.js`
    // dispatched into. Both were removed at D-23 step 3 — `globalThis.fetch`
    // is now the native callback installed by
    // `crate::fetch_native::install_fetch_global`. See ADR D-23.)

    // (URL parsing is now part of native URL — see install_url_native.
    // __urlParse / __urlCanParse callbacks are no longer needed.)

    // crypto namespace (randomUUID + native helpers for SubtleCrypto)
    {
        let crypto = v8::Object::new(scope);

        let uuid_fn = v8::Function::new(scope, crate::crypto::crypto_random_uuid_callback).unwrap();
        let uuid_key = v8::String::new(scope, "randomUUID").unwrap();
        crypto.set(scope, uuid_key.into(), uuid_fn.into());

        // getRandomValues — direct TypedArray fill, no base64 (hand-written callback)
        let grv_fn = v8::Function::new(scope, crate::crypto::crypto_get_random_values_callback).unwrap();
        let grv_key = v8::String::new(scope, "getRandomValues").unwrap();
        crypto.set(scope, grv_key.into(), grv_fn.into());

        let digest_fn = v8::Function::new(scope, crate::crypto::crypto_digest_callback).unwrap();
        let digest_key = v8::String::new(scope, "__cryptoDigest").unwrap();
        crypto.set(scope, digest_key.into(), digest_fn.into());

        let import_fn = v8::Function::new(scope, crate::crypto::crypto_import_key_callback).unwrap();
        let import_key = v8::String::new(scope, "__cryptoImportKey").unwrap();
        crypto.set(scope, import_key.into(), import_fn.into());

        let export_fn = v8::Function::new(scope, crate::crypto::crypto_export_key_callback).unwrap();
        let export_key = v8::String::new(scope, "__cryptoExportKey").unwrap();
        crypto.set(scope, export_key.into(), export_fn.into());

        let gen_fn = v8::Function::new(scope, crate::crypto::crypto_generate_key_callback).unwrap();
        let gen_key = v8::String::new(scope, "__cryptoGenerateKey").unwrap();
        crypto.set(scope, gen_key.into(), gen_fn.into());

        let sign_fn = v8::Function::new(scope, crate::crypto::crypto_sign_callback).unwrap();
        let sign_key = v8::String::new(scope, "__cryptoSign").unwrap();
        crypto.set(scope, sign_key.into(), sign_fn.into());

        let verify_fn = v8::Function::new(scope, crate::crypto::crypto_verify_callback).unwrap();
        let verify_key = v8::String::new(scope, "__cryptoVerify").unwrap();
        crypto.set(scope, verify_key.into(), verify_fn.into());

        let encrypt_fn = v8::Function::new(scope, crate::crypto::crypto_encrypt_callback).unwrap();
        let encrypt_key = v8::String::new(scope, "__cryptoEncrypt").unwrap();
        crypto.set(scope, encrypt_key.into(), encrypt_fn.into());

        let decrypt_fn = v8::Function::new(scope, crate::crypto::crypto_decrypt_callback).unwrap();
        let decrypt_key = v8::String::new(scope, "__cryptoDecrypt").unwrap();
        crypto.set(scope, decrypt_key.into(), decrypt_fn.into());

        let derive_bits_fn = v8::Function::new(scope, crate::crypto::crypto_derive_bits_callback).unwrap();
        let derive_bits_key = v8::String::new(scope, "__cryptoDeriveBits").unwrap();
        crypto.set(scope, derive_bits_key.into(), derive_bits_fn.into());

        let derive_key_fn = v8::Function::new(scope, crate::crypto::crypto_derive_key_callback).unwrap();
        let derive_key_key = v8::String::new(scope, "__cryptoDeriveKey").unwrap();
        crypto.set(scope, derive_key_key.into(), derive_key_fn.into());

        let crypto_key = v8::String::new(scope, "crypto").unwrap();
        global.set(scope, crypto_key.into(), crypto.into());
    }

    // Native sync hash/HMAC for node:crypto polyfill
    {
        let f = v8::Function::new(scope, crate::crypto::crypto_hash_sync_callback).unwrap();
        let key = v8::String::new(scope, "__cryptoHashSync").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::crypto::crypto_hmac_sync_callback).unwrap();
        let key = v8::String::new(scope, "__cryptoHmacSync").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // __streams namespace (native backing for ReadableStream)
    {
        let streams = v8::Object::new(scope);

        let create_fn = v8::Function::new(scope, crate::streams::stream_create_callback).unwrap();
        let create_key = v8::String::new(scope, "create").unwrap();
        streams.set(scope, create_key.into(), create_fn.into());

        let read_fn = v8::Function::new(scope, crate::streams::stream_read_callback).unwrap();
        let read_key = v8::String::new(scope, "read").unwrap();
        streams.set(scope, read_key.into(), read_fn.into());

        let enqueue_fn = v8::Function::new(scope, crate::streams::stream_enqueue_callback).unwrap();
        let enqueue_key = v8::String::new(scope, "enqueue").unwrap();
        streams.set(scope, enqueue_key.into(), enqueue_fn.into());

        let close_fn = v8::Function::new(scope, crate::streams::stream_close_callback).unwrap();
        let close_key = v8::String::new(scope, "close").unwrap();
        streams.set(scope, close_key.into(), close_fn.into());

        let error_fn = v8::Function::new(scope, crate::streams::stream_error_callback).unwrap();
        let error_key = v8::String::new(scope, "error").unwrap();
        streams.set(scope, error_key.into(), error_fn.into());

        let streams_key = v8::String::new(scope, "__streams").unwrap();
        global.set(scope, streams_key.into(), streams.into());
    }

    // WebSocket native callbacks
    {
        let f = v8::Function::new(scope, crate::websocket::ws_create_pair_callback).unwrap();
        let key = v8::String::new(scope, "__wsCreatePair").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::websocket::ws_link_pair_callback).unwrap();
        let key = v8::String::new(scope, "__wsLinkPair").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::websocket::ws_accept_callback).unwrap();
        let key = v8::String::new(scope, "__wsAccept").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::websocket::ws_send_callback).unwrap();
        let key = v8::String::new(scope, "__wsSend").unwrap();
        global.set(scope, key.into(), f.into());

        let f = v8::Function::new(scope, crate::websocket::ws_close_callback).unwrap();
        let key = v8::String::new(scope, "__wsClose").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // env namespace
    {
        let env = v8::Object::new(scope);

        let get_fn = v8::Function::new(scope, env_get_callback).unwrap();
        let get_key = v8::String::new(scope, "get").unwrap();
        env.set(scope, get_key.into(), get_fn.into());

        let env_key = v8::String::new(scope, "env").unwrap();
        global.set(scope, env_key.into(), env.into());
    }

    // __zs_env — returns the frozen env snapshot (same as fetch's 2nd arg).
    // The zeroship JS module exposes this as `const env = Object.freeze(__zs_env());`
    // so SDK packages can read env.* without threading it through fetch().
    {
        let f = v8::Function::new(scope, zs_env_callback).unwrap();
        let key = v8::String::new(scope, "__zs_env").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // __zs_bind_request_ctx / __zs_get_request_ctx — per-request ctx stash
    // for the PR 2 bootstrap. `__zs_bind_request_ctx(ctx)` stashes the
    // object under the current request_id; `__zs_get_request_ctx()` returns
    // the same reference from any nested module. Lightweight replacement
    // for AsyncLocalStorage — single-threaded isolate, request_id tracked
    // by the pump across await boundaries.
    {
        let bind_key = v8::String::new(scope, "__zs_bind_request_ctx").unwrap();
        let bind_fn = v8::Function::new(scope, zs_bind_request_ctx_callback).unwrap();
        global.set(scope, bind_key.into(), bind_fn.into());

        let get_key = v8::String::new(scope, "__zs_get_request_ctx").unwrap();
        let get_fn = v8::Function::new(scope, zs_get_request_ctx_callback).unwrap();
        global.set(scope, get_key.into(), get_fn.into());

        // __zs_get_request — direct-read for the current Request JS object.
        // Stored by the kernel in `state.request_by_id` on the fetch()
        // slow path; empty for the RPC fast-path (no Request built). Lets
        // `getRequest()` skip a per-request __bindRequest round-trip.
        let req_key = v8::String::new(scope, "__zs_get_request").unwrap();
        let req_fn = v8::Function::new(scope, zs_get_request_callback).unwrap();
        global.set(scope, req_key.into(), req_fn.into());
    }

    // __zs_wait_until — registers a Promise against the current request's
    // wait-until bag. Consumed by `zeroship.waitUntil` in the user-facing
    // ESM module; the kernel holds the isolate alive past the response
    // until every promise settles or the wall timeout fires.
    {
        let key = v8::String::new(scope, "__zs_wait_until").unwrap();
        let f = v8::Function::new(scope, zs_wait_until_callback).unwrap();
        global.set(scope, key.into(), f.into());
    }

    // process.env polyfill — many npm packages (e.g. LangChain) read
    // `process.env.OPENAI_API_KEY`. SECURITY: this object only carries
    // the user-controlled `vars` (always-public) plus secrets the
    // creator has explicitly opted-in via the per-app `expose` list.
    // Bare `secrets` are NEVER copied here — that would let any
    // third-party npm package walk `Object.keys(process.env)` and
    // exfiltrate them.
    //
    // Worker-internal `env_vars` (e.g. `APP_ID` injected by
    // `crates/worker/src/cache.rs`) is layered on first as a base; user
    // `vars` override on collision because user config is the
    // authoritative surface. Exposed secrets are then layered last for
    // any names in the per-app expose list — but we deliberately let
    // `vars` take precedence even there: an explicit name set as a var
    // shouldn't be silently shadowed by a secret of the same name (the
    // creator can resolve the conflict by deleting one or the other).
    //
    // An earlier revision used `std::env::vars()` which leaked every
    // host-level secret (DATABASE_URL, WORKER_KEY, AWS credentials) to
    // every app. Multi-tenant apps must only see their own env vars.
    {
        let process = v8::Object::new(scope);
        let env_obj = v8::Object::new(scope);

        let state: crate::state::SharedState = scope
            .get_slot::<crate::state::SharedState>()
            .expect("RuntimeState not in isolate slot")
            .clone();

        // Layer 1: worker-internal env_vars (APP_ID, ...). Always last
        // resort — any user-controlled var of the same name wins.
        let worker_env = state.borrow().env_vars.clone();
        for (key, value) in &worker_env {
            let k = v8::String::new(scope, key).unwrap();
            let v = v8::String::new(scope, value).unwrap();
            env_obj.set(scope, k.into(), v.into());
        }

        // Layer 2: opt-in exposed secrets. Listed in `env_expose_keys`,
        // looked up in `env_app_secrets`. Layered BEFORE vars so a var
        // of the same name still wins (vars are the explicit non-sensitive
        // surface; we don't shadow them with a secret).
        let (app_vars, app_secrets, expose_keys) = {
            let s = state.borrow();
            (s.env_app_vars.clone(), s.env_app_secrets.clone(), s.env_expose_keys.clone())
        };
        for name in &expose_keys {
            if let Some(value) = app_secrets.get(name) {
                let k = v8::String::new(scope, name).unwrap();
                let v = v8::String::new(scope, value).unwrap();
                env_obj.set(scope, k.into(), v.into());
            }
        }

        // Layer 3: user-controlled vars — always in `process.env`.
        // Wins over both worker-internal and exposed-secret layers.
        for (key, value) in &app_vars {
            let k = v8::String::new(scope, key).unwrap();
            let v = v8::String::new(scope, value).unwrap();
            env_obj.set(scope, k.into(), v.into());
        }

        let env_key = v8::String::new(scope, "env").unwrap();
        process.set(scope, env_key.into(), env_obj.into());

        let version = v8::String::new(scope, "v20.0.0").unwrap();
        let version_key = v8::String::new(scope, "version").unwrap();
        process.set(scope, version_key.into(), version.into());

        // process.versions — required by libraries that gate on
        // process.versions.node (e.g. @nodelib/fs.scandir, used by
        // anything that touches @nodelib/fs.walk → langchain → deepagents).
        // Without this, the bundle errors at module evaluation with
        // "Cannot read properties of undefined (reading 'node')".
        {
            let versions = v8::Object::new(scope);
            for (k, v) in [
                ("node", "20.0.0"),
                ("v8", "12.0.0"),
                ("openssl", "3.0.0"),
            ] {
                let k = v8::String::new(scope, k).unwrap();
                let v = v8::String::new(scope, v).unwrap();
                versions.set(scope, k.into(), v.into());
            }
            let key = v8::String::new(scope, "versions").unwrap();
            process.set(scope, key.into(), versions.into());
        }

        // process.platform / process.arch — read by Node-platform-detection
        // helpers. Pinning to linux/x64 is fine; the actual runtime is V8
        // on whatever the host happens to be, but bundled libraries gate
        // on these to choose code paths (e.g. picking newline conventions).
        {
            let platform = v8::String::new(scope, "linux").unwrap();
            let key = v8::String::new(scope, "platform").unwrap();
            process.set(scope, key.into(), platform.into());
            let arch = v8::String::new(scope, "x64").unwrap();
            let key = v8::String::new(scope, "arch").unwrap();
            process.set(scope, key.into(), arch.into());
        }

        // process.nextTick — Node-only. langgraph's Pregel state machine
        // schedules task transitions via nextTick; without it, agent.invoke
        // never advances past the first node and the Promise never settles.
        // Map onto queueMicrotask which is the closest semantic match.
        {
            let src = v8::String::new(
                scope,
                "(p, qm) => { p.nextTick = function(fn) { var args = Array.prototype.slice.call(arguments, 1); qm(function() { fn.apply(null, args); }); }; }",
            ).unwrap();
            let script = v8::Script::compile(scope, src, None).unwrap();
            let factory: v8::Local<v8::Function> = script.run(scope).unwrap().try_into().unwrap();
            let qm_key = v8::String::new(scope, "queueMicrotask").unwrap();
            let qm = global.get(scope, qm_key.into()).unwrap();
            let undef = v8::undefined(scope).into();
            factory.call(scope, undef, &[process.into(), qm]);
        }

        let process_key = v8::String::new(scope, "process").unwrap();
        global.set(scope, process_key.into(), process.into());

        // unenv's `process` polyfill (loaded when a bundle imports
        // `node:process` or anything depending on it) replaces our
        // `process.env` with a Proxy that reads from
        // `globalThis.__env__`. Mirror our env onto `__env__` so the
        // polyfill's reads see the same vars as a direct
        // `process.env.X` lookup. AI SDK's `loadAPIKey` is one consumer
        // — without this, `OPENAI_API_KEY` reads as undefined inside
        // the bundle even though we set process.env.
        let env_alias_key = v8::String::new(scope, "__env__").unwrap();
        global.set(scope, env_alias_key.into(), env_obj.into());
    }

    // Native TextEncoder / TextDecoder. Replace the buggy hand-written
    // JS polyfills that lived in fetch.js — those ignored the
    // `{ stream: true }` option and corrupted multi-byte UTF-8 split
    // across chunk boundaries (the AI SDK / SSE bug). Native
    // implementations live in `text_encoding.rs` and are wired here
    // via the `#[v8_class]` macro's `install` fn.
    {
        let tmpl = crate::text_encoding::TextEncoder::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "TextEncoder").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }
    {
        let tmpl = crate::text_encoding::TextDecoder::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "TextDecoder").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }

    // Native Headers per WHATWG Fetch §2.2 — wired in
    // `load_polyfills_and_modules` immediately after fetch.js runs.
    // The class itself lives in `crate::headers`; it replaces the JS
    // polyfill that used to ship in `embed/fetch.js`. WPT pass: 98/0/1.
}

/// Install native `Headers` on `globalThis`. Called from
/// `load_polyfills_and_modules` after fetch.js runs (the polyfill no
/// longer defines its own Headers, but the install order keeps the
/// dependency chain explicit: native primitives load before user
/// modules).
///
/// The implementation lives in `crate::headers` (Headers struct,
/// HeadersIterator, install_global). See `docs/proposals/headers-native.md`
/// for the design.
pub fn install_headers(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);
    crate::headers::install_global(scope, global);
}

/// Install native DOM primitives (EventTarget, Event, CustomEvent,
/// AbortController, AbortSignal, FormData) plus Request / Response /
/// fetch on `globalThis`. Per D-23 step 2c the native cutover is now
/// the default — no env-var gate.
///
/// Called AFTER fetch.js / formdata.js run so the polyfills'
/// unconditional `globalThis.X = X` assignments don't overwrite our
/// native install. The polyfills are deleted in D-23 step 3.
///
/// Called BEFORE websocket.js so its
/// `WebSocket.prototype = Object.create(EventTarget.prototype)` reads
/// the NATIVE EventTarget prototype — required for the polyfill's
/// `addEventListener` / `dispatchEvent` calls to land on native code.
pub fn install_dom(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);
    crate::dom::install_globals(scope, global);
    // Native Request + Response: install AFTER dom (which gives us
    // FormData / AbortSignal that the constructors need to resolve via
    // globalThis). MUST run after fetch.js + formdata.js so the
    // polyfill's unconditional re-assignment of Request/Response
    // doesn't clobber the native install.
    crate::fetch_request::install_global(scope, global);
    crate::fetch_response::install_global(scope, global);
    crate::fetch_native::install_fetch_global(scope, global);
}

/// Install native WHATWG Streams classes onto `globalThis`. Called
/// from `load_polyfills_and_modules`.
///
/// Native covers ReadableStream, WritableStream, TransformStream,
/// *DefaultController, *DefaultWriter, *DefaultReader, BYOBReader,
/// BYOBRequest, the async-iter prototype patches,
/// ByteLengthQueuingStrategy, and CountQueuingStrategy. See
/// `docs/proposals/streams-native.md` for the design (D-19 cutover).
pub fn install_native_streams(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);
    crate::streams::install_native_streams(scope, global);
}

/// Install native `TextEncoderStream` and `TextDecoderStream` onto
/// `globalThis`. Replaces the legacy `embed/text-streams.js` shim,
/// closing spec gaps the JS shim left open: `Symbol.toStringTag`,
/// brand-checked `encoding`/`fatal`/`ignoreBOM` accessors, and the
/// WHATWG GenericTransformStream §6.1 prototype-side `readable` /
/// `writable` getter shape (the shim assigned own data props in the
/// constructor body, which broke libraries that introspect via
/// `Object.getOwnPropertyDescriptor(Object.getPrototypeOf(s), …)`).
///
/// Must run AFTER:
///   * `install_native_streams` — internal `new TransformStream(...)`
///     construction needs the user-visible class on `globalThis`.
///   * `setup_globals` — needs `TextEncoder` / `TextDecoder` on
///     `globalThis` (the encode/decode primitives the wrappers
///     delegate to).
pub fn install_text_encoding_streams(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);
    {
        let tmpl = crate::text_encoding::streams::TextEncoderStream::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "TextEncoderStream").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }
    {
        let tmpl = crate::text_encoding::streams::TextDecoderStream::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "TextDecoderStream").unwrap();
        global.set(scope, key.into(), class_fn.into());
    }
}

/// Install native `Blob` and `File` (per WHATWG File API) onto
/// `globalThis`. Replaces the legacy `embed/blob.js` polyfill.
///
/// Must run AFTER `install_native_streams` because `Blob.stream()`
/// constructs a user-visible `new ReadableStream(...)`.
pub fn install_blob_native(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);
    crate::blob_native::install_globals(scope, global);
}

/// Install native `URL` + `URLSearchParams` (ada-url backed) onto
/// `globalThis`. Replaces the legacy `embed/url.js` polyfill (deleted).
/// Spec gaps closed: spec-correct setters (host parser / IPv6 brackets
/// / IDNA via ada-url's mutation API), live two-way sync between
/// `url.search` and `url.searchParams`, `URL.parse(input, base?)` static
/// method (newer spec), `URLSearchParams.{has,delete}(name, value?)`
/// 2-arg forms, USVString conversion replacing lone surrogates with
/// U+FFFD.
pub fn install_url_native(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);
    crate::url_native::install_globals(scope, global);
}

// ===========================================================================
// env.get (absorbed from v8/env.rs)
// ===========================================================================

/// `env.get(key) → string | null`
///
/// The explicit, audited surface for env reads — user code uses this
/// (or `import { env } from "zeroship"`) when it wants to read a value
/// that may be a secret. Returns the merged `vars + secrets` map; on
/// collision secrets win because they are the authoritative value for
/// sensitive lookups.
///
/// Distinct from `process.env`, which only carries `vars` plus
/// opt-in-exposed secrets. See `setup_globals` for that layer.
///
/// Worker-internal `env_vars` (APP_ID, …) are deliberately NOT in this
/// surface — that map is for plugin-internal state, not user-readable
/// configuration.
#[zeroship_op(state)]
fn env_get(state: SharedState, key: String) -> Option<String> {
    let s = state.borrow();
    // Vars first, then secrets override on collision (more sensitive
    // wins on the explicit surface).
    s.env_app_vars
        .get(&key)
        .cloned()
        .map(|v| s.env_app_secrets.get(&key).cloned().unwrap_or(v))
        .or_else(|| s.env_app_secrets.get(&key).cloned())
}
