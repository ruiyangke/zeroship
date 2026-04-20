# PR 1 — Kernel Cut: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the kernel's two dispatch paths (`dispatch_rpc` + `dispatch_http`) with a single `call_fetch_handler(request, env, ctx) -> FetchOutcome` primitive. After this PR, the runtime kernel has one entry point, one error surface, and knows nothing about RPC or `"use server"`. End-to-end is intentionally broken at the end of PR 1 — the bootstrap that restores it is PR 2. Kernel tests must be green.

**Architecture:** Additive-then-subtractive. Land the new primitive + native ops + tests alongside the old code; switch downstream callers (worker, serve) to the new primitive; then delete the old dispatch paths and all `zeroship.*` globals in one cleanup pass. The user-facing contract tested in this PR is `export default { fetch(request, env, ctx) }` — invoked directly by the kernel (no bootstrap router yet).

**Tech Stack:** Rust (compio, V8 via rusty_v8), JavaScript (test modules inside V8), existing zeroship kernel (`crates/runtime`, `crates/worker`). Tests use `cargo test -p zeroship-runtime` + `cargo test -p zeroship-worker`.

**Spec:** `docs/superpowers/specs/2026-04-20-programming-model-design.md`

**Follow-up plans (write these after PR 1 merges):**
- PR 2: Bootstrap + `zeroship` module (`@zeroship/runtime-bootstrap` package, vite-plugin bootstrap injection). End-to-end works again.
- PR 3: SDK internals switch from `zeroship.*` globals to `env.*`.
- PR 4: Control-plane secrets/vars CRUD + encryption.
- PR 5: Vite-plugin rework (`"use server"` registration, client stub emission, examples rewrite).

---

## File Structure

**New files:**
- `crates/runtime/src/fetch_outcome.rs` — `FetchOutcome` enum, `RequestCtx` struct, `EnvSnapshot` struct
- `crates/runtime/tests/call_fetch_handler.rs` — integration tests for the new primitive

**Modified files:**
- `crates/runtime/src/runtime.rs` (2381 LOC) — add `call_fetch_handler`; later delete `dispatch_rpc` / `dispatch_start` / `dispatch_http` and the `DispatchOutcome` enum
- `crates/runtime/src/init.rs` (927 LOC) — add `__zs_env` / `__zs_bind_request_ctx` / `__zs_get_request_ctx` native ops; later delete `DISPATCH_JS`, `__rpc` scaffolding, `zeroship.*` global bindings
- `crates/runtime/src/state.rs` (483 LOC) — extend per-request state with a `wait_until` list
- `crates/runtime/src/dispatch.rs` (380 LOC) — simplify (drop RPC-specific return-tag detection once `dispatch_rpc` is gone)
- `crates/runtime/src/lib.rs` — export `FetchOutcome`, `RequestCtx`, `EnvSnapshot`
- `crates/runtime/src/serve.rs` — switch `dispatch_rpc_by_path` / `dispatch_http` callers to `call_fetch_handler`
- `crates/worker/src/handler.rs` (479 LOC) — collapse `http_dispatch()` + `dispatch()` into one handler that calls `call_fetch_handler`
- `crates/runtime/tests/common/mod.rs` — add `dispatch_fetch()` helper; remove the legacy `dispatch()` / `dispatch_http_sync()` helpers
- `crates/runtime/tests/http.rs` — rewrite to test via `call_fetch_handler`

**Deleted files:**
- `crates/runtime/tests/rpc.rs` — RPC sugar moves to PR 2's bootstrap tests; the Rust kernel no longer knows what RPC is

**Out of scope for PR 1** (handled in PR 2+):
- `sdks/` — no changes
- `examples/` — left broken; restored when PR 5 runs
- `zeroship migrate` — not a thing (greenfield per spec)

---

## Task A1: Define `FetchOutcome`, `RequestCtx`, `EnvSnapshot`

**Files:**
- Create: `crates/runtime/src/fetch_outcome.rs`
- Modify: `crates/runtime/src/lib.rs`

- [ ] **Step 1: Create the new types file**

Create `crates/runtime/src/fetch_outcome.rs`:

```rust
//! Outcome of `Runtime::call_fetch_handler` — the kernel's sole dispatch
//! primitive. Replaces `DispatchOutcome`'s 7-variant split between RPC
//! and HTTP flavors.

use std::rc::Rc;

use crate::channel::{CancelFlag, ResultReceiver, StreamReader};
use crate::runtime::DispatchError;

/// What the fetch handler produced — shape depends on whether the handler
/// was sync, async, streaming, or upgraded to WebSocket.
pub enum FetchOutcome {
    /// Sync / settled synchronously. Body is fully buffered.
    Response {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        logs: Vec<String>,
    },
    /// Headers known, body arrives chunk-by-chunk via `body_reader`.
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        body_reader: StreamReader,
        logs: Vec<String>,
    },
    /// Handler returned a Promise that hasn't settled. Poll `rx` for the
    /// final `FetchOutcome::Response` or `FetchOutcome::Stream`.
    Pending {
        rx: ResultReceiver<Result<SettledFetch, DispatchError>>,
        cancel: CancelFlag,
    },
    /// Handler returned a Response with status 101 + `webSocket` property.
    WebSocketUpgrade {
        ws_id: u32,
        headers: Vec<(String, String)>,
    },
}

/// Body shape delivered via the pending-resolver channel. Mirrors
/// `FetchOutcome` minus the `Pending` variant (can't be nested).
pub enum SettledFetch {
    Complete {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        logs: Vec<String>,
    },
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        body_reader: StreamReader,
        logs: Vec<String>,
    },
    WebSocket {
        ws_id: u32,
        headers: Vec<(String, String)>,
        logs: Vec<String>,
    },
}

/// Per-request execution context — what user code sees as `ctx`.
///
/// Built fresh by the gateway-facing layer (worker handler.rs or serve.rs)
/// for every request; carried across V8 reentries via the kernel's
/// `executing_request_id` tracking. Cancellation is wired to `cancel`.
#[derive(Clone)]
pub struct RequestCtx {
    pub cancel: CancelFlag,
    /// Storage for `ctx.waitUntil(promise)` calls from JS. Promises live
    /// past the response body write; kernel keeps them alive until all
    /// settle or the wall timeout fires.
    pub wait_until: Rc<std::cell::RefCell<Vec<v8::Global<v8::Promise>>>>,
}

impl RequestCtx {
    pub fn new(cancel: CancelFlag) -> Self {
        Self {
            cancel,
            wait_until: Rc::new(std::cell::RefCell::new(Vec::new())),
        }
    }
}

/// Frozen env snapshot — the `env` object user code imports from `zeroship`
/// and receives as the second `fetch(req, env, ctx)` parameter. Same
/// reference every request; populated once at worker boot from the app's
/// zeroship.toml + control-plane secrets.
///
/// Encoded as JSON for simple cross-boundary handoff; the JS side
/// JSON.parses once at module init and freezes the result. Richer
/// per-binding shapes (`env.DB.query(...)`) come in PR 3 when SDKs
/// land.
#[derive(Clone)]
pub struct EnvSnapshot {
    json: String,
}

impl EnvSnapshot {
    pub fn new(value: serde_json::Value) -> Self {
        Self { json: value.to_string() }
    }

    pub fn empty() -> Self {
        Self { json: "{}".into() }
    }

    pub fn as_json(&self) -> &str {
        &self.json
    }
}
```

- [ ] **Step 2: Register module in `lib.rs`**

Edit `crates/runtime/src/lib.rs`, add the module declaration and re-export:

```rust
pub mod fetch_outcome;
```

(place alongside other `pub mod` lines near the top)

And add to the `pub use` block:

```rust
pub use fetch_outcome::{FetchOutcome, SettledFetch, RequestCtx, EnvSnapshot};
```

- [ ] **Step 3: Verify the new types compile**

Run: `cargo check -p zeroship-runtime`

Expected: clean build, no warnings about unused types (Rust will warn; that's fine — they're used by the test file in Task B1).

- [ ] **Step 4: Commit**

```bash
git add crates/runtime/src/fetch_outcome.rs crates/runtime/src/lib.rs
git commit -m "runtime: add FetchOutcome / RequestCtx / EnvSnapshot types

New kernel dispatch primitives for the CF-Workers-mirror refactor.
No callers yet — types alone. See docs/superpowers/specs/2026-04-20-programming-model-design.md."
```

---

## Task A2: Extend `PerRequestState` with `wait_until`

**Files:**
- Modify: `crates/runtime/src/state.rs`

- [ ] **Step 1: Read the current `PerRequestState` / request-tracking struct**

Run: `grep -n "next_direct_request_id\|executing_request_id\|pending_resolvers" crates/runtime/src/state.rs`

Expected: you find the per-request tracking fields. Note the line numbers.

- [ ] **Step 2: Add a `wait_until_by_request` map**

Edit `crates/runtime/src/state.rs`. Near the other per-request fields in `RuntimeState`, add:

```rust
/// For each in-flight request, the list of promises registered via
/// `ctx.waitUntil(p)` from JS. The kernel keeps the isolate alive past
/// the response body write until every promise settles or the wall
/// timeout fires. Cleared in `clear_executing_request` after the wall
/// budget elapses.
pub wait_until_by_request: std::collections::HashMap<u64, Vec<v8::Global<v8::Promise>>>,
```

In the `RuntimeState::new` / `RuntimeState::default` constructor, initialize it:

```rust
wait_until_by_request: std::collections::HashMap::new(),
```

(Match the exact constructor style of the surrounding code — `Default::default()` pattern or explicit `HashMap::new()`.)

- [ ] **Step 3: Add a helper `register_wait_until`**

In the same file, in `impl RuntimeState`:

```rust
/// Register a promise registered via `ctx.waitUntil(p)`. The promise is
/// keyed by the request currently executing; if no request is active,
/// the promise is dropped (and the native op will have thrown to JS).
pub fn register_wait_until(&mut self, promise: v8::Global<v8::Promise>) -> bool {
    let Some(rid) = self.executing_request_id else {
        return false;
    };
    self.wait_until_by_request
        .entry(rid)
        .or_default()
        .push(promise);
    true
}
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo check -p zeroship-runtime`

Expected: clean build.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime/src/state.rs
git commit -m "runtime: track waitUntil promises per request

Per-request map; `register_wait_until` is called from the __zs_wait_until
native op (added in Task C3). Not yet wired — state addition only."
```

---

## Task B1: Write failing test for `call_fetch_handler` — simple Response

**Files:**
- Create: `crates/runtime/tests/call_fetch_handler.rs`
- Modify: `crates/runtime/tests/common/mod.rs` (add `dispatch_fetch` helper)

- [ ] **Step 1: Read the current common-test helpers**

Run: `cat crates/runtime/tests/common/mod.rs`

Expected: you see the `m()` helper (builds a single-module list), `dispatch_http_sync`, etc. Note the `init_v8()` / `Runtime::builder()` pattern.

- [ ] **Step 2: Add `dispatch_fetch` helper to `common/mod.rs`**

Append to `crates/runtime/tests/common/mod.rs`:

```rust
use zeroship_runtime::{EnvSnapshot, FetchOutcome, RequestCtx, CancelFlag};

/// HTTP request to feed `call_fetch_handler` — the new kernel primitive.
pub struct TestRequest {
    pub method: &'static str,
    pub url: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl TestRequest {
    pub fn get(url: &'static str) -> Self {
        Self { method: "GET", url, headers: vec![], body: String::new() }
    }
    pub fn post_json(url: &'static str, body: impl Into<String>) -> Self {
        Self {
            method: "POST",
            url,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.into(),
        }
    }
}

/// Build a Runtime + call `call_fetch_handler` once synchronously.
/// Returns the outcome as-is; tests destructure.
pub fn dispatch_fetch(modules: Vec<ModuleEntry>, req: TestRequest) -> FetchOutcome {
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    runtime.call_fetch_handler(
        req.method,
        req.url,
        &req.headers,
        &req.body,
        &env,
        ctx,
    )
}
```

- [ ] **Step 3: Create the test file**

Create `crates/runtime/tests/call_fetch_handler.rs`:

```rust
mod common;
use common::*;

use zeroship_runtime::FetchOutcome;

#[test]
fn simple_response() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                return new Response("hello", { status: 200 });
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::Response { status, body, .. } => {
            assert_eq!(status, 200);
            assert_eq!(body, "hello");
        }
        _ => panic!("expected Response outcome"),
    }
}
```

- [ ] **Step 4: Run the test — must fail**

Run: `cargo test -p zeroship-runtime --test call_fetch_handler simple_response 2>&1 | tail -20`

Expected: FAIL — either compile error "method `call_fetch_handler` not found on `Runtime`" (if we haven't implemented the stub yet) or runtime panic. Either way, failing is correct for Step 4.

- [ ] **Step 5: (No commit yet — test is supposed to fail)**

Skip commit; we commit once the test passes in Task B3.

---

## Task B2: Implement `Runtime::call_fetch_handler` skeleton

**Files:**
- Modify: `crates/runtime/src/runtime.rs`

- [ ] **Step 1: Find the public Runtime impl block**

Run: `grep -n "^impl Runtime {" crates/runtime/src/runtime.rs`

Expected: one line around L300-ish (there's a struct `Runtime` with an impl that holds the handle; line numbers from the earlier grep: `dispatch_rpc` at L336, so the impl starts above it).

- [ ] **Step 2: Add the skeleton method**

In `crates/runtime/src/runtime.rs`, in the public `impl Runtime` block (alongside `dispatch_http`, before the `has_http_handler` method):

```rust
/// Kernel's sole dispatch primitive. Invokes the user's
/// `export default { fetch(request, env, ctx) }` handler and returns
/// a `FetchOutcome` describing the response.
///
/// `request`: method / url / headers / body — in the same shape the
///   worker crate already packages from the incoming gateway envelope.
/// `env`: module-singleton env snapshot (JSON-serialized once at boot).
/// `ctx`: per-request execution context (cancel flag + waitUntil list).
pub fn call_fetch_handler(
    &self,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &str,
    env: &crate::EnvSnapshot,
    ctx: crate::RequestCtx,
) -> crate::FetchOutcome {
    self.inner.borrow_mut().call_fetch_handler(
        self.modules.as_slice(),
        method,
        url,
        headers,
        body,
        env,
        ctx,
    )
}
```

- [ ] **Step 3: Add the inner impl stub**

Find the private `impl RuntimeInner` block — the one with `fn dispatch_http(&mut self, ...)`. Add this method (you'll flesh it out in Task B3):

```rust
pub fn call_fetch_handler(
    &mut self,
    modules: &[crate::ModuleEntry],
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &str,
    env: &crate::EnvSnapshot,
    ctx: crate::RequestCtx,
) -> crate::FetchOutcome {
    let _ = (modules, method, url, headers, body, env, ctx);
    crate::FetchOutcome::Response {
        status: 501,
        headers: vec![],
        body: "{\"message\":\"call_fetch_handler not implemented\",\"name\":\"Error\"}".into(),
        logs: vec![],
    }
}
```

- [ ] **Step 4: Run the test — still fails (500 != 200)**

Run: `cargo test -p zeroship-runtime --test call_fetch_handler simple_response 2>&1 | tail -10`

Expected: test RUNS (method exists) but FAILS on `assert_eq!(status, 200)` — we got 501 stub.

- [ ] **Step 5: (No commit — moving to Task B3)**

---

## Task B3: Implement `call_fetch_handler` for sync `Response` returns

**Files:**
- Modify: `crates/runtime/src/runtime.rs`

The implementation mirrors the existing `RuntimeInner::dispatch_http` flow but: (a) invokes `module.default.fetch(request, env, ctx)` instead of the top-level `onRequest` export; (b) passes env and ctx JS objects built from the snapshot + RequestCtx.

- [ ] **Step 1: Look up the existing `dispatch_http` body**

Run: `sed -n '1419,1510p' crates/runtime/src/runtime.rs`

Expected: you can see how `dispatch_http` uses `enter_v8!` to get a HandleScope, builds a Request object via `http_create_request_fn`, calls the `http_handler_fn` global, and dispatches on the return.

- [ ] **Step 2: Add a cached handle for `module.default.fetch`**

In the `RuntimeInner` struct (same file), add a field next to `http_handler_fn`:

```rust
/// Cached reference to `module.default.fetch`, resolved once at module
/// init. None if the module doesn't export a default.fetch handler.
fetch_handler_fn: Option<v8::Global<v8::Function>>,
```

Initialize it to `None` in `RuntimeInner::new` (grep for `http_handler_fn: None` to find the spot).

- [ ] **Step 3: Resolve `module.default.fetch` in `ensure_initialized`**

Find `fn ensure_initialized` in `RuntimeInner`. After the `http_handler_fn` resolution block, add:

```rust
// Resolve module.default.fetch — the new kernel primitive target.
if self.fetch_handler_fn.is_none() {
    let scope = &mut enter_v8_scope!(self);
    let global = scope.get_current_context().global(scope);
    // The module's default export is exposed via a known global at
    // bundle-load time — same hook the existing bundle compiler uses.
    // Reads `globalThis.__zs_user_default?.fetch`.
    let key = v8::String::new(scope, "__zs_user_default").unwrap();
    if let Some(default_val) = global.get(scope, key.into())
        && !default_val.is_undefined()
        && !default_val.is_null()
    {
        let default_obj: v8::Local<v8::Object> = default_val.try_into().ok()?;
        let fetch_key = v8::String::new(scope, "fetch").unwrap();
        if let Some(fetch_val) = default_obj.get(scope, fetch_key.into())
            && fetch_val.is_function()
        {
            let fetch_fn: v8::Local<v8::Function> = fetch_val.try_into().unwrap();
            self.fetch_handler_fn = Some(v8::Global::new(scope, fetch_fn));
        }
    }
}
```

Note: `enter_v8_scope!` and the exact expression for getting the current isolate scope follow the pattern already used in `dispatch_http`. Copy that pattern exactly; do not invent a new macro.

- [ ] **Step 4: Implement the body of `call_fetch_handler`**

Replace the stub body from Task B2 with:

```rust
pub fn call_fetch_handler(
    &mut self,
    modules: &[crate::ModuleEntry],
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &str,
    env: &crate::EnvSnapshot,
    ctx: crate::RequestCtx,
) -> crate::FetchOutcome {
    self.ensure_initialized(modules);

    let Some(fetch_fn) = self.fetch_handler_fn.clone() else {
        return crate::FetchOutcome::Response {
            status: 404,
            headers: vec![],
            body: r#"{"message":"No default.fetch handler exported","name":"Error"}"#.into(),
            logs: vec![],
        };
    };

    let request_id = self.next_direct_request_id;
    self.next_direct_request_id += 1;
    let wall_start = std::time::Instant::now();

    self.state.borrow_mut().executing_request_id = Some(request_id);
    // Wire the cancel flag so native fetch/db calls observe it.
    self.state.borrow_mut().executing_request_cancel = Some(ctx.cancel.clone());

    self.arm_cpu_timer();
    let call_result = {
        let fetch_fn = fetch_fn.clone();
        enter_v8!(self, |scope| {
            // Build the Request JS object — reuse the existing helper.
            let headers_json = serde_json::to_string(headers).unwrap_or_else(|_| "[]".into());
            let request_obj = crate::http::build_request(scope, method, url, &headers_json, body);
            // Build env object — JSON.parse(env.as_json()).
            let env_obj = crate::http::parse_json_to_value(scope, env.as_json());
            // Build ctx object — lightweight JS wrapper.
            let ctx_obj = crate::http::build_ctx(scope, request_id);

            let undefined = v8::undefined(scope).into();
            let args = [request_obj, env_obj, ctx_obj];
            let local_fn = v8::Local::new(scope, fetch_fn);

            crate::dispatch::call_and_unwrap(scope, local_fn, undefined, &args)
        })
    };
    self.disarm_cpu_timer();

    if self.check_v8_terminated() {
        self.clear_executing_request();
        return crate::FetchOutcome::Response {
            status: 503,
            headers: vec![],
            body: r#"{"message":"CPU time limit exceeded","name":"Error"}"#.into(),
            logs: vec![],
        };
    }

    let cpu_dispatch = wall_start.elapsed();

    match call_result {
        crate::dispatch::DispatchResult::HttpResponse(info) => {
            self.clear_executing_request();
            self.build_fetch_outcome(request_id, info, cpu_dispatch)
        }
        crate::dispatch::DispatchResult::ErrorValue { message, status, .. } => {
            self.clear_executing_request();
            self.discard_request_state(request_id);
            crate::FetchOutcome::Response {
                status,
                headers: vec![("content-type".into(), "application/json".into())],
                body: serde_json::json!({"message": message, "name": "Error"}).to_string(),
                logs: vec![],
            }
        }
        crate::dispatch::DispatchResult::Error(msg) => {
            self.clear_executing_request();
            self.discard_request_state(request_id);
            crate::FetchOutcome::Response {
                status: 500,
                headers: vec![("content-type".into(), "application/json".into())],
                body: serde_json::json!({"message": msg, "name": "Error"}).to_string(),
                logs: vec![],
            }
        }
        other => {
            // Promise pending — store as PendingRequest with reply_fetch slot.
            self.store_fetch_pending(request_id, other, ctx)
        }
    }
}

/// Build a `FetchOutcome` from a resolved HTTP ResponseInfo.
/// This is structurally identical to the existing `build_http_outcome` —
/// copy its body verbatim, changing only the return variant names
/// (`DispatchOutcome::HttpComplete` → `FetchOutcome::Response`;
/// `HttpStream` → `Stream`; `WebSocketUpgrade` is identical).
/// `ResponseInfo::Stream` carries a `stream_id` — the helper must set
/// up the (writer, reader) channel and attach the writer to the stream
/// state, exactly as `build_http_outcome` does today. When Task D2
/// deletes `build_http_outcome`, this helper fully replaces it.
fn build_fetch_outcome(
    &mut self,
    request_id: u64,
    info: crate::http::ResponseInfo,
    _cpu: std::time::Duration,
) -> crate::FetchOutcome {
    let logs = self.drain_request_logs(request_id);
    match info {
        crate::http::ResponseInfo::Complete { status, headers, body } => {
            crate::FetchOutcome::Response { status, headers, body, logs }
        }
        crate::http::ResponseInfo::Stream { status, headers, stream_id } => {
            // Copy the writer-attachment block from the existing
            // build_http_outcome (runtime.rs around L1530–1555). It
            // creates a stream_buffer, drains pre-attach chunks, sets
            // direct_writer on the StreamState, and returns the reader.
            let (writer, reader) = crate::channel::stream_buffer();
            {
                let mut s = self.state.borrow_mut();
                if let Some(stream) = s.streams.get_mut(&stream_id) {
                    for chunk in stream.buffer.drain(..) {
                        let _ = writer.push(chunk);
                    }
                    if stream.closed {
                        writer.close();
                    } else {
                        stream.direct_writer = Some(writer);
                    }
                } else {
                    writer.close();
                }
            }
            crate::FetchOutcome::Stream { status, headers, body_reader: reader, logs }
        }
        crate::http::ResponseInfo::WebSocket { ws_id, headers } => {
            crate::FetchOutcome::WebSocketUpgrade { ws_id, headers }
        }
    }
}

/// Placeholder — Pending handling in Task B5.
fn store_fetch_pending(
    &mut self,
    _request_id: u64,
    _result: crate::dispatch::DispatchResult,
    _ctx: crate::RequestCtx,
) -> crate::FetchOutcome {
    crate::FetchOutcome::Response {
        status: 501,
        headers: vec![],
        body: r#"{"message":"async fetch handler not yet implemented","name":"Error"}"#.into(),
        logs: vec![],
    }
}
```

Note: the above references `crate::http::build_request`, `parse_json_to_value`, and `build_ctx`. The first exists; the latter two need to be added.

- [ ] **Step 5: Add missing `http::*` helpers**

In `crates/runtime/src/http.rs`, add these helpers (match the existing `build_request` style — if that helper isn't named `build_request`, find the real name via `grep -n "pub fn" crates/runtime/src/http.rs`):

```rust
/// Parse `json_str` via JSON.parse in the current isolate; returns the
/// resulting JS value (object, primitive, or null).
pub fn parse_json_to_value<'s>(
    scope: &mut v8::HandleScope<'s>,
    json_str: &str,
) -> v8::Local<'s, v8::Value> {
    let s = v8::String::new(scope, json_str).unwrap();
    v8::json::parse(scope, s).unwrap_or_else(|| v8::undefined(scope).into())
}

/// Build a minimal ctx object: `{ waitUntil: _ => {}, passThroughOnException: () => {} }`.
/// Real waitUntil wiring comes via the `__zs_wait_until` native op in Task C3;
/// for now this stub is sufficient for tests that don't call waitUntil.
pub fn build_ctx<'s>(
    scope: &mut v8::HandleScope<'s>,
    _request_id: u64,
) -> v8::Local<'s, v8::Value> {
    let obj = v8::Object::new(scope);
    // waitUntil: no-op for now (PR 1 scope). JS tests that call it with
    // a promise see it silently swallowed.
    let wait_until_key = v8::String::new(scope, "waitUntil").unwrap();
    let wait_until_fn = v8::Function::new(scope, |_, _, _| {}).unwrap();
    obj.set(scope, wait_until_key.into(), wait_until_fn.into());

    let pass_key = v8::String::new(scope, "passThroughOnException").unwrap();
    let pass_fn = v8::Function::new(scope, |_, _, _| {}).unwrap();
    obj.set(scope, pass_key.into(), pass_fn.into());

    obj.into()
}
```

If there's already a `build_request` — great, reuse it. If the real name is different (common: `create_http_request`), keep the call site in `call_fetch_handler` using the real name.

- [ ] **Step 6: Expose `__zs_user_default`**

The kernel needs to see the user's `export default`. In PR 1, we haven't added the bootstrap yet, so the user module must expose its default via a global that `ensure_initialized` reads.

The simplest wiring: when the compiled module executes (existing module-instantiation code), the compiler emits a trailing `globalThis.__zs_user_default = mod.default;`. In PR 1, since we don't want compiler changes, we instead capture the default directly from the V8 module's namespace object.

Find the module-instantiation code. Search:

```bash
grep -n "get_module_namespace\|module_namespace\|instantiate_module\|default" crates/runtime/src/modules.rs
```

You'll find where module exports are bound to globals. Add a branch that, for the entry module, does:

```rust
// After module execution, pull `default` from the namespace and expose
// it as __zs_user_default so call_fetch_handler can find it. Mirrors the
// existing path that exposes named exports (ping, onRequest, etc.).
if let Some(default_export) = namespace.get(scope, default_key.into())
    && !default_export.is_undefined()
{
    let key = v8::String::new(scope, "__zs_user_default").unwrap();
    global.set(scope, key.into(), default_export);
}
```

Place this alongside the existing named-export binding loop. The exact match for `default_key` is `v8::String::new(scope, "default").unwrap()`.

- [ ] **Step 7: Run the test — should pass now**

Run: `cargo test -p zeroship-runtime --test call_fetch_handler simple_response -- --nocapture 2>&1 | tail -20`

Expected: PASS.

If it fails with a resolution error ("fetch_handler_fn is None"), the Step 6 wiring is likely wrong — the module isn't exposing `__zs_user_default`. Add an `eprintln!("[debug] default resolved: {:?}", default_val.is_undefined())` in Step 6's branch to verify the V8 side.

- [ ] **Step 8: Commit**

```bash
git add crates/runtime/src/runtime.rs crates/runtime/src/http.rs crates/runtime/src/modules.rs crates/runtime/tests/call_fetch_handler.rs crates/runtime/tests/common/mod.rs
git commit -m "runtime: call_fetch_handler — sync Response path

New kernel dispatch primitive. Resolves module.default.fetch,
invokes with (request, env, ctx), returns FetchOutcome::Response
for sync returns. Async paths stubbed; tests added for the happy path."
```

---

## Task B4: Test + implement async `Promise<Response>` handler

**Files:**
- Modify: `crates/runtime/tests/call_fetch_handler.rs`
- Modify: `crates/runtime/src/runtime.rs`

- [ ] **Step 1: Add failing test**

Append to `crates/runtime/tests/call_fetch_handler.rs`:

```rust
use zeroship_runtime::SettledFetch;
use std::time::Duration;

#[test]
fn async_response() {
    let modules = m(r#"
        export default {
            async fetch(request, env, ctx) {
                await new Promise(r => setTimeout(r, 0));
                return new Response("later", { status: 202 });
            }
        };
    "#);
    let outcome = dispatch_fetch(modules, TestRequest::get("http://localhost/"));
    let FetchOutcome::Pending { rx, cancel: _ } = outcome else {
        panic!("expected Pending outcome");
    };

    // Block on the receiver using compio's runtime — existing test pattern.
    // Match the idiom in crates/runtime/tests/http.rs for async paths.
    let body = compio::runtime::Runtime::new()
        .unwrap()
        .block_on(async {
            // start_pump so async handlers settle via the pump task.
            // (In the real Runtime builder this is wired automatically;
            // if the test harness doesn't, uncomment the start_pump call.)
            compio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("timeout waiting for async response")
                .expect("pending channel dropped")
                .expect("pending delivered error")
        });

    let SettledFetch::Response { status, body, .. } = body else {
        panic!("expected Complete body");
    };
    assert_eq!(status, 202);
    assert_eq!(body, "later");
}
```

Note: if the existing test file doesn't use `compio::runtime::Runtime`, use whatever pattern `RuntimeInner::pump_loop` tests already use. `grep -n "block_on\|compio::runtime" crates/runtime/tests/` to find the idiom.

- [ ] **Step 2: Run — must fail**

Run: `cargo test -p zeroship-runtime --test call_fetch_handler async_response 2>&1 | tail -15`

Expected: FAIL with `expected Pending outcome` or status 501 (Task B3 stub returned 501 for non-sync paths).

- [ ] **Step 3: Implement `store_fetch_pending`**

Replace the placeholder body in `runtime.rs`:

```rust
fn store_fetch_pending(
    &mut self,
    request_id: u64,
    result: crate::dispatch::DispatchResult,
    ctx: crate::RequestCtx,
) -> crate::FetchOutcome {
    // Result is DispatchResult::Promise(_) or similar — non-settled handler.
    let crate::dispatch::DispatchResult::Promise(promise) = result else {
        // Unreachable in practice — settled/errored cases handled in the
        // call_fetch_handler match arms above. Defensive stub.
        self.discard_request_state(request_id);
        return crate::FetchOutcome::Response {
            status: 500,
            headers: vec![],
            body: r#"{"message":"unexpected dispatch state","name":"Error"}"#.into(),
            logs: vec![],
        };
    };

    let (tx, rx) = crate::channel::result_channel::<Result<crate::SettledFetch, crate::runtime::DispatchError>>();

    let pending = crate::runtime::PendingRequest {
        id: request_id,
        promise,
        reply_fetch: Some(tx),
        reply_direct: None,
        reply_http: None,
        is_http: true,
        cpu_accumulated: std::time::Duration::ZERO,
        wall_start: std::time::Instant::now(),
        cancel: ctx.cancel.clone(),
    };
    self.pending_requests.insert(request_id, pending);
    self.notify_pump();

    crate::FetchOutcome::Pending {
        rx,
        cancel: ctx.cancel,
    }
}
```

Note: you need to add `reply_fetch: Option<ResultSender<Result<SettledFetch, DispatchError>>>` to the `PendingRequest` struct, alongside `reply_direct` and `reply_http`. Grep for the struct definition:

```bash
grep -n "struct PendingRequest" crates/runtime/src/runtime.rs
```

Find it, add the new field, and initialize `reply_fetch: None` at every construction site.

- [ ] **Step 4: Update the pump to send through `reply_fetch`**

Find the pump's settle branch — search:

```bash
grep -n "SettledResult::Rpc\|SettledResult::Http" crates/runtime/src/runtime.rs
```

The pump matches on `SettledResult::{Rpc, Http}` and delivers via `reply_direct` / `reply_http`. Add a third arm (or reuse Http) that also sends to `reply_fetch`. Since PR 1 does not yet split settlement by handler type, do the simplest thing: after the existing `reply_http.send(...)`, also check `reply_fetch` and send a translated `SettledFetch`:

The pump's settle branch already calls `build_http_outcome` for the `reply_http` slot. Since `ResponseInfo::Stream` carries a `stream_id` (not a reader) and the writer-attachment must happen exactly once, route the settled info through `build_fetch_outcome` first and then convert its result to `SettledFetch`:

```rust
SettledResult::Http(Ok(info)) => {
    // If a PR-1-path receiver is waiting, convert via build_fetch_outcome
    // (which handles the Stream writer-attach, logs, and WS shape).
    // If the legacy reply_http is also set, fall back to send_http_settled
    // — temporary duplication until Task D2 removes reply_http.
    if let Some(tx) = req.reply_fetch {
        let outcome = self.build_fetch_outcome(id, info, cpu_time);
        let pb = match outcome {
            crate::FetchOutcome::Response { status, headers, body, logs } =>
                crate::SettledFetch::Response { status, headers, body, logs },
            crate::FetchOutcome::Stream { status, headers, body_reader, logs } =>
                crate::SettledFetch::Stream { status, headers, body_reader, logs },
            crate::FetchOutcome::WebSocketUpgrade { ws_id, headers } =>
                crate::SettledFetch::WebSocketUpgrade { ws_id, headers, logs: vec![] },
            crate::FetchOutcome::Pending { .. } =>
                unreachable!("settled info cannot produce Pending"),
        };
        tx.send(Ok(pb));
    } else if let Some(tx) = req.reply_http {
        // Legacy callers — removed in Task D2.
        self.send_http_settled(id, info, Some(tx), cpu_time);
    }
}
SettledResult::Http(Err(msg)) => {
    if let Some(tx) = req.reply_fetch {
        tx.send(Err(msg.clone().into()));
    }
    if let Some(tx) = req.reply_http {
        tx.send(Err(msg.into()));
    }
}
```

The `else if` for `reply_http` is a short-lived bridge — Task D2 deletes `reply_http` entirely, which simplifies this to a single `if let Some(tx) = req.reply_fetch` branch.

- [ ] **Step 5: Run async test — should pass**

Run: `cargo test -p zeroship-runtime --test call_fetch_handler async_response 2>&1 | tail -15`

Expected: PASS. Both `simple_response` and `async_response` now green.

- [ ] **Step 6: Commit**

```bash
git add crates/runtime/src/runtime.rs crates/runtime/tests/call_fetch_handler.rs
git commit -m "runtime: call_fetch_handler — async Promise<Response> path

Pending outcome with result receiver; pump delivers via reply_fetch
channel alongside the legacy reply_http channel. Legacy reply_http
will be dropped when dispatch_http is removed (Task D3)."
```

---

## Task B5: Test + implement streaming (ReadableStream body)

**Files:**
- Modify: `crates/runtime/tests/call_fetch_handler.rs`

- [ ] **Step 1: Add failing test**

Append:

```rust
#[test]
fn streaming_response() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const stream = new ReadableStream({
                    start(controller) {
                        controller.enqueue(new TextEncoder().encode("chunk1"));
                        controller.enqueue(new TextEncoder().encode("chunk2"));
                        controller.close();
                    }
                });
                return new Response(stream, {
                    status: 200,
                    headers: { "content-type": "text/plain" }
                });
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::Stream { status, body_reader: _, .. } => {
            assert_eq!(status, 200);
            // Actually draining the stream requires compio runtime + pump;
            // the existence of the Stream variant is the contract test.
        }
        other => panic!("expected Stream outcome, got {}",
            match other {
                FetchOutcome::Response { .. } => "Response",
                FetchOutcome::Pending { .. } => "Pending",
                FetchOutcome::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                FetchOutcome::Stream { .. } => unreachable!(),
            }),
    }
}
```

- [ ] **Step 2: Run — may already pass**

Run: `cargo test -p zeroship-runtime --test call_fetch_handler streaming_response 2>&1 | tail -10`

Expected: PASS if Task B3's `build_fetch_outcome` correctly routes `ResponseInfo::Stream` → `FetchOutcome::Stream`. If it fails with "expected Stream", double-check the `build_fetch_outcome` match arms.

- [ ] **Step 3: Commit (if green)**

```bash
git add crates/runtime/tests/call_fetch_handler.rs
git commit -m "test: call_fetch_handler — streaming response contract"
```

---

## Task B6: Test + implement WebSocket upgrade

**Files:**
- Modify: `crates/runtime/tests/call_fetch_handler.rs`

- [ ] **Step 1: Add failing test**

Append:

```rust
#[test]
fn websocket_upgrade() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const pair = new WebSocketPair();
                const [client, server] = Object.values(pair);
                server.accept();
                return new Response(null, {
                    status: 101,
                    webSocket: client
                });
            }
        };
    "#);
    match dispatch_fetch(modules, TestRequest::get("http://localhost/")) {
        FetchOutcome::WebSocketUpgrade { ws_id, .. } => {
            assert!(ws_id > 0, "ws_id should be non-zero");
        }
        _ => panic!("expected WebSocketUpgrade outcome"),
    }
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p zeroship-runtime --test call_fetch_handler websocket_upgrade 2>&1 | tail -15`

Expected: PASS if `ResponseInfo::WebSocket` maps through `build_fetch_outcome`. If it fails with "response status 101 + webSocket property" not being detected as an upgrade, verify the existing `http::ResponseInfo` inspection in `dispatch.rs` handles it — it does for `dispatch_http` today; the same helper should work here.

- [ ] **Step 3: Commit**

```bash
git add crates/runtime/tests/call_fetch_handler.rs
git commit -m "test: call_fetch_handler — WebSocket upgrade contract"
```

---

## Task C1: Native op `__zs_env`

**Files:**
- Modify: `crates/runtime/src/init.rs`
- Modify: `crates/runtime/src/state.rs`
- Modify: `crates/runtime/tests/call_fetch_handler.rs`

- [ ] **Step 1: Store the env JSON on the runtime state**

In `RuntimeState` (`crates/runtime/src/state.rs`), add a field:

```rust
/// Frozen env JSON snapshot — JSON.parse-able string. Passed to JS via
/// the `__zs_env` native op. Same value for every request; populated
/// at worker boot from zeroship.toml + control-plane secrets.
pub env_json: String,
```

Initialize to `"{}"` in the constructor. Add a setter:

```rust
pub fn set_env_snapshot(&mut self, env: &crate::EnvSnapshot) {
    self.env_json = env.as_json().to_string();
}
```

- [ ] **Step 2: Set env at the start of `call_fetch_handler`**

In `runtime.rs`, in `RuntimeInner::call_fetch_handler`, just before the V8 dispatch:

```rust
self.state.borrow_mut().set_env_snapshot(env);
```

- [ ] **Step 3: Register `__zs_env` global**

In `crates/runtime/src/init.rs`, find `setup_globals`. After an existing native-op registration (any will do — grep `scope.set(global, key, ...)` for style), add:

```rust
// __zs_env() — returns the frozen env JSON as a parsed JS value.
let env_key = v8::String::new(scope, "__zs_env").unwrap();
let env_fn = v8::Function::new(scope, zs_env_callback).unwrap();
global.set(scope, env_key.into(), env_fn.into());
```

Add the callback definition in the same file:

```rust
fn zs_env_callback(
    scope: &mut v8::HandleScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let json = state.borrow().env_json.clone();
    let s = v8::String::new(scope, &json).unwrap();
    let parsed = v8::json::parse(scope, s).unwrap_or_else(|| v8::undefined(scope).into());
    rv.set(parsed);
}
```

- [ ] **Step 4: Write failing test**

Append to `call_fetch_handler.rs`:

```rust
use zeroship_runtime::EnvSnapshot;

#[test]
fn zs_env_returns_snapshot() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                return Response.json({
                    direct: __zs_env(),
                    fromArg: env
                });
            }
        };
    "#);
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::new(serde_json::json!({"FOO": "bar", "N": 42}));
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://localhost/",
        &[],
        "",
        &env,
        ctx,
    );
    let FetchOutcome::Response { body, .. } = outcome else {
        panic!("expected Response");
    };
    assert!(body.contains(r#""FOO":"bar""#), "body: {}", body);
    assert!(body.contains(r#""N":42"#), "body: {}", body);
}
```

- [ ] **Step 5: Run — should pass**

Run: `cargo test -p zeroship-runtime --test call_fetch_handler zs_env_returns_snapshot 2>&1 | tail -15`

Expected: PASS. If it fails with "direct" but not "fromArg", the `build_ctx` / env-arg passing in `call_fetch_handler` needs inspection — but the `parse_json_to_value(scope, env.as_json())` call should already handle it.

- [ ] **Step 6: Commit**

```bash
git add crates/runtime/src/init.rs crates/runtime/src/state.rs crates/runtime/src/runtime.rs crates/runtime/tests/call_fetch_handler.rs
git commit -m "runtime: __zs_env native op — return env snapshot

JS accesses the module-singleton env via globalThis.__zs_env() or via
the fetch(req, env, ctx) parameter. Both return the same parsed JSON."
```

---

## Task C2: Native op `__zs_bind_request_ctx` + `__zs_get_request_ctx`

**Files:**
- Modify: `crates/runtime/src/init.rs`
- Modify: `crates/runtime/src/state.rs`
- Modify: `crates/runtime/tests/call_fetch_handler.rs`

- [ ] **Step 1: Add per-request ctx storage to state**

Edit `crates/runtime/src/state.rs`, in `RuntimeState`:

```rust
/// Per-request JS-exposed context map, keyed by request_id.
/// Populated via `__zs_bind_request_ctx(ctxObj)` from bootstrap JS;
/// read via `__zs_get_request_ctx()` from any nested module.
pub request_ctx_by_id: std::collections::HashMap<u64, v8::Global<v8::Object>>,
```

Initialize to `HashMap::new()`.

- [ ] **Step 2: Register the two ops in `init.rs`**

In `setup_globals`, alongside `__zs_env`:

```rust
let bind_key = v8::String::new(scope, "__zs_bind_request_ctx").unwrap();
let bind_fn = v8::Function::new(scope, zs_bind_request_ctx_callback).unwrap();
global.set(scope, bind_key.into(), bind_fn.into());

let get_key = v8::String::new(scope, "__zs_get_request_ctx").unwrap();
let get_fn = v8::Function::new(scope, zs_get_request_ctx_callback).unwrap();
global.set(scope, get_key.into(), get_fn.into());
```

Add the two callbacks in the same file:

```rust
fn zs_bind_request_ctx_callback(
    scope: &mut v8::HandleScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state = scope
        .get_slot::<crate::state::SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let rid_opt = state.borrow().executing_request_id;
    let Some(rid) = rid_opt else { return };

    let arg = args.get(0);
    if arg.is_null() || arg.is_undefined() {
        state.borrow_mut().request_ctx_by_id.remove(&rid);
        return;
    }
    if !arg.is_object() {
        return;
    }
    let obj: v8::Local<v8::Object> = arg.try_into().unwrap();
    let global_obj = v8::Global::new(scope, obj);
    state.borrow_mut().request_ctx_by_id.insert(rid, global_obj);
}

fn zs_get_request_ctx_callback(
    scope: &mut v8::HandleScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state = scope
        .get_slot::<crate::state::SharedState>()
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
```

- [ ] **Step 3: Clear ctx on request completion**

In `crates/runtime/src/runtime.rs`, find `clear_executing_request`. After clearing `executing_request_id`, also clear the ctx map entry:

```rust
if let Some(rid) = self.state.borrow().executing_request_id {
    self.state.borrow_mut().request_ctx_by_id.remove(&rid);
}
```

(The exact spot: wherever `executing_request_id = None` happens. If `clear_executing_request` borrows state twice, collect the rid first, then mutate — same pattern the rest of the file uses.)

- [ ] **Step 4: Write failing test**

Append to `call_fetch_handler.rs`:

```rust
#[test]
fn zs_bind_and_get_request_ctx() {
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                // Simulate what the bootstrap will do: bind ctx on entry.
                __zs_bind_request_ctx(ctx);
                // Nested lookup must find it.
                const nested = __zs_get_request_ctx();
                return Response.json({
                    sameRef: nested === ctx,
                    hasWaitUntil: typeof nested.waitUntil === "function"
                });
            }
        };
    "#);
    let outcome = dispatch_fetch(modules, TestRequest::get("http://localhost/"));
    let FetchOutcome::Response { body, .. } = outcome else {
        panic!("expected Response");
    };
    assert!(body.contains(r#""sameRef":true"#), "body: {}", body);
    assert!(body.contains(r#""hasWaitUntil":true"#), "body: {}", body);
}
```

- [ ] **Step 5: Run — should pass**

Run: `cargo test -p zeroship-runtime --test call_fetch_handler zs_bind_and_get 2>&1 | tail -15`

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/runtime/src/init.rs crates/runtime/src/state.rs crates/runtime/src/runtime.rs crates/runtime/tests/call_fetch_handler.rs
git commit -m "runtime: __zs_bind_request_ctx / __zs_get_request_ctx ops

Per-request ctx storage keyed by executing_request_id. Bootstrap
(PR 2) binds on fetch entry; nested modules look up via get_* in
place of JS-level AsyncLocalStorage."
```

---

## Task E1: Switch `worker/handler.rs` to `call_fetch_handler`

**Files:**
- Modify: `crates/worker/src/handler.rs`

- [ ] **Step 1: Read the current two entry points**

Run: `grep -n "pub async fn" crates/worker/src/handler.rs`

Expected: two public handlers — one for RPC (`/_rpc/*`), one for HTTP passthrough. They should collapse into one.

- [ ] **Step 2: Collapse into one `dispatch` function**

Rewrite `crates/worker/src/handler.rs`. The new shape: one `dispatch` handler that builds a `TestRequest`-like envelope, calls `call_fetch_handler`, and maps `FetchOutcome` → ntex `HttpResponse`.

Key excerpt (the full rewrite is mechanical — adapt existing error handling to the new enum):

```rust
use zeroship_runtime::{FetchOutcome, SettledFetch, EnvSnapshot, RequestCtx};
use zeroship_runtime::channel::CancelFlag;

pub async fn dispatch(
    req: HttpRequest,
    config: web::types::State<Arc<WorkerConfig>>,
    path: web::types::Path<String>,
    body: String,
) -> HttpResponse {
    if let Some(resp) = check_worker_auth(&req, &config.worker_key) {
        return resp;
    }

    let app_id = match path.parse::<Uuid>() {
        Ok(id) => id,
        Err(_) => {
            metrics::inc(&metrics::DISPATCH_REJECTED_BAD_APP_ID);
            return HttpResponse::BadRequest().body(r#"{"error":"invalid app_id"}"#);
        }
    };

    // Ensure bundle loaded (on-demand).
    if cache::get_runtime(&app_id).is_none() {
        metrics::inc(&metrics::ON_DEMAND_LOADS_TOTAL);
        if let Err(e) = load_on_demand(&config, &app_id).await {
            metrics::inc(&metrics::ON_DEMAND_LOAD_FAILURES);
            return HttpResponse::ServiceUnavailable()
                .json(&serde_json::json!({"error": format!("failed to load app: {e}")}));
        }
    }

    let runtime = match cache::get_runtime(&app_id) {
        Some(r) => r,
        None => return HttpResponse::NotFound()
            .body(format!(r#"{{"error":"app {app_id} not loaded"}}"#)),
    };

    metrics::inc(&metrics::DISPATCH_TOTAL);

    // Parse the incoming envelope from the gateway. The gateway sends one
    // envelope shape for every request now: { method, url, headers, body }.
    let envelope: HttpEnvelope = match serde_json::from_str(&body) {
        Ok(e) => e,
        Err(e) => {
            return HttpResponse::BadRequest()
                .json(&serde_json::json!({"error": format!("invalid envelope: {e}")}));
        }
    };

    // Build env (TODO PR 4: read from app config). For PR 1, empty.
    let env = EnvSnapshot::empty();
    let cancel = CancelFlag::new();
    let ctx = RequestCtx::new(cancel.clone());

    let outcome = {
        runtime.enter_isolate();
        let o = runtime.call_fetch_handler(
            &envelope.method,
            &envelope.url,
            &envelope.headers,
            &envelope.body,
            &env,
            ctx,
        );
        runtime.exit_isolate();
        o
    };

    match outcome {
        FetchOutcome::Response { status, headers, body, .. } =>
            build_http_response(status, headers, body),

        FetchOutcome::Stream { status, headers, body_reader, .. } =>
            stream_response(status, &headers, body_reader),

        FetchOutcome::WebSocketUpgrade { .. } =>
            build_http_response(500, vec![],
                r#"{"message":"WebSocket upgrade not supported via HTTP dispatch","name":"Error"}"#.into()),

        FetchOutcome::Pending { rx, cancel: cf } => {
            match recv_with_timeout(&rx, wall_limit(&runtime), &cf, &runtime).await {
                Some(Ok(SettledFetch::Response { status, headers, body, .. })) =>
                    build_http_response(status, headers, body),
                Some(Ok(SettledFetch::Stream { status, headers, body_reader, .. })) =>
                    stream_response(status, &headers, body_reader),
                Some(Ok(SettledFetch::WebSocketUpgrade { .. })) =>
                    build_http_response(500, vec![],
                        r#"{"message":"WebSocket upgrade not supported via HTTP dispatch","name":"Error"}"#.into()),
                Some(Err(e)) =>
                    build_http_response(e.status, vec![],
                        serde_json::json!({"message": e.message, "name": "Error"}).to_string()),
                None =>
                    build_http_response(504, vec![],
                        r#"{"message":"request timed out","name":"Error"}"#.into()),
            }
        }
    }
}
```

Delete the old `http_dispatch` + RPC `dispatch` functions. Delete `HttpEnvelope` + `RpcEnvelope` duplication — keep one `HttpEnvelope`:

```rust
#[derive(serde::Deserialize)]
struct HttpEnvelope {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    #[serde(default)]
    body: String,
}
```

The kernel doesn't care about `/_rpc/*` any more — the URL is forwarded verbatim. Any RPC-specific decoding lives in the bootstrap (PR 2).

- [ ] **Step 3: Rewire routes in `worker/src/main.rs`**

Run: `grep -n "dispatch\|http_dispatch\|/_rpc" crates/worker/src/main.rs`

Expected: you find the ntex route registrations. Collapse the two routes into one:

```rust
.service(web::resource("/_dispatch/{app_id}").route(web::post().to(handler::dispatch)))
```

If the old code had two separate routes (`/_rpc/{app_id}` and `/_dispatch/{app_id}`), pick one URL and delete the other — the gateway always sends one shape now.

- [ ] **Step 4: Check that it compiles**

Run: `cargo check --workspace 2>&1 | tail -20`

Expected: zeroship-worker compiles. There may still be unused-warning noise from the still-live `dispatch_http` method in `runtime.rs` — ignore that; it'll be cleaned up in Task D3.

- [ ] **Step 5: Update gateway if needed**

Run: `grep -n "_rpc\|_dispatch\|dispatch_http" crates/gateway/src/ 2>&1`

Expected: the gateway proxies to whatever route the worker exposes. If gateway still speaks `/_rpc`, rename to `/_dispatch` to match (find + sed; one-liner).

Run: `cargo check --workspace`

Expected: clean workspace build.

- [ ] **Step 6: Commit**

```bash
git add crates/worker/src/handler.rs crates/worker/src/main.rs crates/gateway/src/
git commit -m "worker: single dispatch entry via call_fetch_handler

Collapses /_rpc and /_dispatch routes into one. Envelope parse is
handler-agnostic now — the runtime's bootstrap router (PR 2) will
dispatch RPC vs fetch internally based on URL."
```

---

## Task E2: Switch `serve.rs` to `call_fetch_handler`

**Files:**
- Modify: `crates/runtime/src/serve.rs`

`serve.rs` is the `zeroship serve` single-tenant dev/production path (no gateway, no worker). It has its own RPC + HTTP split that needs the same collapse.

- [ ] **Step 1: Find both dispatch paths in serve.rs**

Run: `grep -n "dispatch_rpc_by_path\|dispatch_http\|DispatchOutcome" crates/runtime/src/serve.rs`

Expected: two async fns — `dispatch_rpc_by_path` and `dispatch_http`. Collapse them.

- [ ] **Step 2: Rewrite to a single `handle_request`**

Replace both functions with one `handle_request` that:
1. Parses the HTTP request line + headers (existing code).
2. Builds an `EnvSnapshot::empty()` and a `RequestCtx::new(...)`.
3. Calls `runtime.call_fetch_handler(method, url, &headers, &body, &env, ctx)`.
4. Writes the `FetchOutcome` to the TCP stream using the existing `build_http_response` / `build_stream_response_headers` helpers.

The code shape mirrors `worker/handler.rs` — just with `stream.write_all` calls instead of ntex's `HttpResponse`. Copy the match arms from Task E1's handler and adapt.

Example for the match arms (after calling `runtime.call_fetch_handler(...)` into `outcome`):

```rust
match outcome {
    FetchOutcome::Response { status, headers, body, .. } => {
        let resp = build_http_response(status, &headers, &body);
        let BufResult(r, _) = stream.write_all(resp).await;
        r.is_ok()
    }
    FetchOutcome::Stream { status, headers, body_reader, .. } => {
        runtime.notify_pump();
        let hdr = build_stream_response_headers(status, &headers);
        let BufResult(r, _) = stream.write_all(hdr).await;
        if r.is_err() { return false; }
        stream_chunked_body(stream, body_reader).await
    }
    FetchOutcome::WebSocketUpgrade { ws_id, headers } =>
        handle_websocket_upgrade(stream, ws_id, &headers, request_headers, runtime).await,
    FetchOutcome::Pending { rx, cancel } => {
        match recv_with_timeout(&rx, runtime.wall_timeout(), &cancel, runtime).await {
            Some(Ok(SettledFetch::Response { status, headers, body, .. })) => {
                let resp = build_http_response(status, &headers, &body);
                let BufResult(r, _) = stream.write_all(resp).await;
                r.is_ok()
            }
            Some(Ok(SettledFetch::Stream { status, headers, body_reader, .. })) => {
                let hdr = build_stream_response_headers(status, &headers);
                let BufResult(r, _) = stream.write_all(hdr).await;
                if r.is_err() { return false; }
                stream_chunked_body(stream, body_reader).await
            }
            Some(Ok(SettledFetch::WebSocketUpgrade { ws_id, headers, .. })) =>
                handle_websocket_upgrade(stream, ws_id, &headers, request_headers, runtime).await,
            Some(Err(e)) => {
                let body = format!(
                    r#"{{"message":"{}","name":"Error"}}"#,
                    e.message.replace('"', "\\\"")
                );
                let resp = build_http_response(e.status, &[], &body);
                let BufResult(r, _) = stream.write_all(resp).await;
                r.is_ok()
            }
            None => {
                let resp = build_http_response(504, &[],
                    r#"{"message":"request timed out","name":"Error"}"#);
                let BufResult(r, _) = stream.write_all(resp).await;
                r.is_ok()
            }
        }
    }
}
```

- [ ] **Step 3: Delete `dispatch_rpc_by_path` and the old `dispatch_http`**

They're unreachable after Step 2. Remove both function definitions from `serve.rs`.

- [ ] **Step 4: Verify build + existing serve tests**

Run: `cargo build --workspace 2>&1 | tail -10`

Expected: clean.

Run: `cargo test -p zeroship-runtime --test bundle_e2e 2>&1 | tail -10`

Expected: either passes OR fails with "module does not export default.fetch" — that's acceptable for PR 1 because the examples haven't been migrated yet. If any tests fail for unrelated reasons, fix them.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime/src/serve.rs
git commit -m "serve: single dispatch via call_fetch_handler

Deletes dispatch_rpc_by_path + legacy dispatch_http. All routing
decisions now live in the bootstrap (PR 2); kernel is URL-agnostic."
```

---

## Task D1: Delete `DISPATCH_JS` + `__rpc` scaffolding from `init.rs`

**Files:**
- Modify: `crates/runtime/src/init.rs`

- [ ] **Step 1: Find and delete `DISPATCH_JS`**

Run: `grep -n "DISPATCH_JS\|__rpc" crates/runtime/src/init.rs`

Delete the entire `pub const DISPATCH_JS: &str = r#"..."#;` block.

Delete any block that populates `__rpc` / bootstraps the registry / references `DISPATCH_JS` (the `setup_globals` section that evaluates `DISPATCH_JS` or registers `__rpc`).

- [ ] **Step 2: Delete `zeroship.*` global bindings**

In the same file, find the block that sets `zeroship.db`, `zeroship.auth`, `zeroship.kv`, `zeroship.storage`, `zeroship.meter`:

```bash
grep -n "zeroship\"\|ZeroshipGlobal\|db_obj\|auth_obj" crates/runtime/src/init.rs
```

Delete the entire `zeroship` global construction (the object, its property sets, and the `globalThis.zeroship = obj` assignment).

The per-capability native ops (e.g., `__zs_db_query`) may still be needed later — PR 3 moves them from `zeroship.db.query` to `env.DB.query`. For now, **leave the native callbacks but delete only the `zeroship.*` facade**. If there's no such separation (callbacks were inlined under `zeroship.db.query`), remove them entirely; they'll be re-added in PR 3 as `env.DB.query`.

- [ ] **Step 3: Delete the `embed/dispatch.js`-style embed if one exists**

Run: `ls crates/runtime/src/embed/ && grep -rn "DISPATCH_JS\|__rpc" crates/runtime/src/embed/ 2>&1 | head`

If any embed file holds RPC glue, delete it. The remaining embeds (`blob.js`, `fetch.js`, `streams.js`, etc.) are Web API polyfills that stay.

- [ ] **Step 4: Check — kernel still builds**

Run: `cargo check -p zeroship-runtime 2>&1 | tail -20`

Expected: clean. If there's a use-of-undefined `DISPATCH_JS` / `__rpc` elsewhere, fix it — most likely in `runtime.rs`'s `ensure_initialized`. Delete those references; they call into the soon-deleted `dispatch_rpc`.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime/src/init.rs crates/runtime/src/embed/ crates/runtime/src/runtime.rs
git commit -m "runtime: delete DISPATCH_JS + __rpc + zeroship.* globals

Kernel no longer knows what RPC is. The URL-path router, method
registry, and async-generator SSE framing all move to bootstrap JS
in PR 2. Cleaning up now that call_fetch_handler covers the tested
paths."
```

---

## Task D2: Delete `dispatch_rpc` + `dispatch_start` + `dispatch_http` methods

**Files:**
- Modify: `crates/runtime/src/runtime.rs`

- [ ] **Step 1: Delete the three public methods on `impl Runtime`**

Edit `crates/runtime/src/runtime.rs`. Remove:
- `pub fn dispatch_rpc(&self, ...)` (around L336)
- `pub fn dispatch_start(&self, ...)` (L345)
- `pub fn dispatch_http(&self, ...)` (L357)

- [ ] **Step 2: Delete the three private methods on `impl RuntimeInner`**

Same file, further down:
- `pub fn dispatch_rpc(&mut self, ...)` (L1134)
- `pub fn dispatch_start(&mut self, ...)` (L1256)
- `pub fn dispatch_http(&mut self, ...)` (L1419)

- [ ] **Step 3: Delete `DispatchOutcome` enum + its variants' helpers**

Delete the `pub enum DispatchOutcome { ... }` block (7 variants). Any helper fns (`send_http_settled`, `build_http_outcome`, etc.) that are now unused → delete. Any helpers still used by `call_fetch_handler` / `build_fetch_outcome` → keep; rename if their name references `http` in a confusing way.

- [ ] **Step 4: Delete the now-unused `reply_http` / `reply_direct` fields**

In the `PendingRequest` struct, remove `reply_direct` and `reply_http`. Only `reply_fetch` remains. Update every `PendingRequest { ... }` construction site accordingly.

In the pump settle branches (Task B4 Step 4), delete the `reply_http.send(...)` branches — they're dead code now that `FetchOutcome::Pending` is the only async path.

- [ ] **Step 5: Delete `has_http_handler`, `has_rpc_method`, and other obsolete predicates**

These were used by `serve.rs` / `worker/handler.rs` to decide between RPC and HTTP. Now obsolete.

- [ ] **Step 6: Check workspace**

Run: `cargo check --workspace 2>&1 | tail -30`

Expected: clean. Likely failures:
- Some test file still imports `DispatchOutcome` → delete the import + the test (if it's testing a path that no longer exists)
- `crates/runtime/tests/rpc.rs` → delete the whole file (see Task E3)

- [ ] **Step 7: Commit**

```bash
git add crates/runtime/src/runtime.rs
git commit -m "runtime: delete dispatch_rpc / dispatch_start / dispatch_http

Single entry is call_fetch_handler. DispatchOutcome enum and its
7 variants removed. PendingRequest has one reply slot."
```

---

## Task D3: Simplify `dispatch.rs`

**Files:**
- Modify: `crates/runtime/src/dispatch.rs`

- [ ] **Step 1: Find RPC-specific branches**

Run: `grep -n "__rpc\|Method not found\|DISPATCH_JS" crates/runtime/src/dispatch.rs`

Expected: branches that inspect method registry, check for `__zsResponse` prototype tag on return values, handle `method-not-found`, etc. Many of these are still relevant for HTTP — keep the Response-prototype detection; delete the RPC-method-registry lookup.

- [ ] **Step 2: Keep these**
- `DispatchResult::HttpResponse(ResponseInfo)` — used by `call_fetch_handler`
- `DispatchResult::Promise(v8::Global<v8::Promise>)` — used for async
- `DispatchResult::ErrorValue { message, status, .. }` — the error wire
- `DispatchResult::Error(msg)` — uncaught throws
- Response-instance detection via `__zsResponse` prototype tag
- Async-generator detection — **DELETE**: this was RPC-specific. User `fetch` handlers explicitly return a Response (possibly wrapping a stream); async generators are a bootstrap (PR 2) concern.

- [ ] **Step 3: Simplify `DispatchResult`**

If the old enum had variants like `DispatchResult::Sync(String)` (stringify-and-return for RPC) — delete them. The final enum should be:

```rust
pub enum DispatchResult {
    HttpResponse(crate::http::ResponseInfo),
    Promise(v8::Global<v8::Promise>),
    ErrorValue { message: String, status: u16, name: String, stack: Option<String> },
    Error(String),
}
```

- [ ] **Step 4: Build**

Run: `cargo check -p zeroship-runtime`

Expected: clean. If `call_fetch_handler` referenced deleted variants, update its match arms.

- [ ] **Step 5: Commit**

```bash
git add crates/runtime/src/dispatch.rs crates/runtime/src/runtime.rs
git commit -m "runtime: simplify DispatchResult — HTTP-only

Drops RPC-specific Sync(String) variant and async-generator detection.
User handlers explicitly return Response; SSE wrapping lives in
bootstrap (PR 2)."
```

---

## Task E3: Delete `tests/rpc.rs`; rewrite `tests/http.rs`

**Files:**
- Delete: `crates/runtime/tests/rpc.rs`
- Modify: `crates/runtime/tests/http.rs`
- Modify: `crates/runtime/tests/common/mod.rs`

- [ ] **Step 1: Delete `tests/rpc.rs`**

Run: `rm crates/runtime/tests/rpc.rs`

The test file targeted `/_rpc/*` paths with `__rpc` registry — all kernel-level, all now gone. RPC-level tests return as bootstrap JS tests in PR 2.

- [ ] **Step 2: Rewrite `tests/http.rs`**

Open `crates/runtime/tests/http.rs`. Every test there uses `dispatch_http_sync` or `dispatch_http`. Rewrite each to use `dispatch_fetch` + the `export default { fetch }` idiom. Example transformation:

Before:
```rust
#[test]
fn basic_get() {
    let r = dispatch_http_sync(
        m(r#"export function onRequest(req) { return new Response("hi"); }"#),
        "GET", "http://localhost/", "[]", ""
    ).unwrap();
    assert_eq!(r.0, 200);
    assert_eq!(r.2, "hi");
}
```

After:
```rust
#[test]
fn basic_get() {
    let outcome = dispatch_fetch(
        m(r#"export default { fetch(req) { return new Response("hi"); } };"#),
        TestRequest::get("http://localhost/")
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("expected Response");
    };
    assert_eq!(status, 200);
    assert_eq!(body, "hi");
}
```

- [ ] **Step 3: Delete `dispatch_http_sync` from `common/mod.rs`**

It's unused now. Delete the helper.

- [ ] **Step 4: Run all runtime tests**

Run: `cargo test -p zeroship-runtime 2>&1 | tail -30`

Expected: all tests pass. If any fail for compile reasons, they likely still reference removed symbols — fix by migrating them to `dispatch_fetch` or deleting.

Expected test count ≈ 100-ish (down from 113 before — the RPC-file 14 tests are gone; 3 new `call_fetch_handler` tests + 2 new ctx tests added).

- [ ] **Step 5: Commit**

```bash
git add crates/runtime/tests/
git commit -m "test: rewrite http.rs for new primitive; delete rpc.rs

RPC kernel tests migrate to PR 2's bootstrap test suite. HTTP tests
exercise export default { fetch } end-to-end through call_fetch_handler."
```

---

## Task F1: Full workspace test + perf sanity

**Files:** (no edits, verification only)

- [ ] **Step 1: Full workspace build**

Run: `cargo build --workspace --release 2>&1 | tail -10`

Expected: clean.

- [ ] **Step 2: Full test suite (pg excluded — needs live DB)**

Run: `cargo test --workspace --exclude compio-postgres --exclude zeroship-plugin-db 2>&1 | grep -E "^test result|FAILED"`

Expected: every line says `ok. N passed; 0 failed`. If any fail, fix before committing.

- [ ] **Step 3: PG tests (single-threaded, requires docker-compose db)**

Run: `cargo test -p compio-postgres -- --test-threads=1 2>&1 | tail -5`

Run: `cargo test -p zeroship-plugin-db -- --test-threads=1 2>&1 | tail -5`

Expected: both green. Skip if local DB isn't up — note it in the PR description.

- [ ] **Step 4: Quick perf regression check**

Run: `cargo bench -p zeroship-runtime --bench bench -- --quick 2>&1 | tail -20`

If the existing `zeroship-bench.rhai` or a rust `bench` harness exists, run it. Expected: within 15% of pre-refactor numbers. Anything worse → investigate before PR merge.

Record the numbers in the PR description:
- `fetchEcho` req/s
- `jsonEcho` req/s
- `noop` req/s

- [ ] **Step 5: Diff review**

Run: `git log --oneline main..HEAD && git diff main...HEAD --stat`

Expected:
- ~10 commits (one per task that committed)
- `crates/runtime/*` dominates the diff
- `crates/worker/src/handler.rs` substantially shrinks
- `crates/runtime/src/runtime.rs` loses ~500+ LOC
- No `examples/*` changes (those are PR 5's job)

- [ ] **Step 6: Open PR**

Run:

```bash
git push -u origin kernel-cut
gh pr create --title "PR 1: kernel cut — one fetch primitive, no RPC sugar" --body "$(cat <<'EOF'
## Summary
- Replaces kernel dispatch_rpc + dispatch_http with a single call_fetch_handler(request, env, ctx) -> FetchOutcome
- Deletes DISPATCH_JS, __rpc, zeroship.* global bindings, DispatchOutcome 7-variant enum
- Adds native ops: __zs_env, __zs_bind_request_ctx, __zs_get_request_ctx
- New FetchOutcome (4 variants), RequestCtx, EnvSnapshot types
- Worker + serve collapse their two dispatch paths into one
- Spec: docs/superpowers/specs/2026-04-20-programming-model-design.md

## What works
- call_fetch_handler tests green for: sync Response, async Response, streaming, WebSocket upgrade, env read, ctx bind/get
- All runtime + worker crate tests pass
- PG + plugin-db tests pass

## What's intentionally broken
- End-to-end app dispatch (examples/* left untouched)
- RPC "use server" functions don't work
- No bootstrap — kernel calls module.default.fetch directly
- Fixed by PR 2 (bootstrap + zeroship module)

## Perf
- fetchEcho: <before>K → <after>K req/s (<delta>%)
- jsonEcho:  <before>K → <after>K req/s (<delta>%)
- noop:      <before>K → <after>K req/s (<delta>%)

## Test plan
- [x] cargo test --workspace (ex. compio-postgres, zeroship-plugin-db) passes
- [x] cargo test -p compio-postgres -- --test-threads=1 passes
- [x] cargo test -p zeroship-plugin-db -- --test-threads=1 passes
- [x] Bench within 15% of main
- [ ] Merge PR 2 right after — end-to-end remains broken until then
EOF
)"
```

- [ ] **Step 7: Log follow-up work**

After opening the PR, write the PR 2 plan (bootstrap + zeroship module).

Use: `docs/superpowers/plans/2026-04-21-bootstrap-and-zeroship-module.md`

Brief:
- Create `@zeroship/runtime-bootstrap` package
- Write `zeroship` / `zeroship/internal` modules
- Implement `handleRpc`, `sseFromAsyncGen`, `errorResponse`
- Wire compiler to inject bootstrap
- Test: RPC routing, SSE framing, error shapes
- Test: B3/B4/B5 probes (moved from kernel)
- Test: WebSocket upgrade via bootstrap
- End-to-end examples smoke

---

## Out-of-scope reminders

- **Do not** edit `sdks/db/src/*`, `sdks/auth/src/*` — these still reference `zeroship.*` globals which are gone. PR 2 adds the bootstrap that re-exposes them; PR 3 rewrites the SDK internals. Breaking SDKs here is expected and fine.
- **Do not** edit `examples/*` — migration happens in PR 5.
- **Do not** add the secrets/vars CRUD to `crates/control/` — that's PR 4.
- **Do not** change the vite-plugin — PR 5.

Keep this PR about the kernel alone.
