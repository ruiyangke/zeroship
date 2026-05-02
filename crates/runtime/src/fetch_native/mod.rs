//! Native `fetch()` global function and the V8 entry layer.
//!
//! Per design fetch-native v2 §V (main fetch algorithm) + §VI (executor).
//! Replaces the JS polyfill `fetch()` in `embed/fetch.js` per D-22 once
//! the cutover lands. For now, install behind `ZEROSHIP_NATIVE_FETCH=1`.
//!
//! ## Module structure
//!
//! - `algorithms`     — main_fetch / scheme_fetch / http_fetch /
//!                      http_redirect_fetch (no V8 entry)
//! - `http_network`   — http_network_fetch wrapper around cyper
//! - `redirect`       — method/body mutation + same-origin checks
//! - `content_encoding` — Content-Encoding decode hook (D-15 + D-9)
//! - `data_url`       — data: URL processor (D-21)
//! - `bad_ports`      — Fetch §4.3 bad-port table (83 ports, D-19)
//!
//! ## Wiring
//!
//! `install_fetch_global` replaces the JS polyfill's `globalThis.fetch`
//! with a hand-rolled V8 callback that:
//!
//!   1. Synchronously coerces input to a Request.
//!   2. Synchronously checks `signal.aborted` per Fetch §5.1 step 7
//!      (CRITICAL #6 — NOT after enqueueing).
//!   3. Drains the request body to bytes (rewindable BodySource → Vec).
//!   4. Spawns a compio task that runs `main_fetch` then schedules a
//!      pump turn to materialise the Response wrapper inside V8.

pub mod algorithms;
pub mod bad_ports;
pub mod content_encoding;
pub mod data_url;
pub mod http_network;
pub mod redirect;

use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;

use crate::channel::CancelFlag;
use crate::fetch_body::body::BodySource;
use crate::state::{
    OpResult, ResolveValue, SharedState, MAX_PENDING_FETCHES, MAX_PENDING_OPS,
};

use algorithms::{
    main_fetch, AlgorithmResponse, CredentialsMode, FetchRequest as AlgFetchRequest,
    RedirectMode,
};

// ===========================================================================
// Pending registry
// ===========================================================================

/// Outcome of a fetch task — stashed in a thread-local so the pump's
/// V8 turn can pick it up after the async task resolves.
enum FetchOutcome {
    Resolve(AlgorithmResponse),
    /// `(message, signal_at_callback_time)` — if signal is aborted at
    /// pump-time, we use `signal.reason` as the rejection value.
    Reject(String, Option<v8::Global<v8::Object>>),
}

thread_local! {
    static PENDING: RefCell<HashMap<u64, FetchOutcome>> = RefCell::new(HashMap::new());
    static NEXT_ID: RefCell<u64> = const { RefCell::new(1) };
}

fn stash(outcome: FetchOutcome) -> u64 {
    let id = NEXT_ID.with(|n| {
        let mut m = n.borrow_mut();
        let v = *m;
        *m = v.wrapping_add(1);
        v
    });
    PENDING.with(|s| {
        s.borrow_mut().insert(id, outcome);
    });
    id
}

/// Encode the registry id as the bytes of a `ResolveValue::Bytes`. The
/// pump recognises this prefix and rebuilds the Response inside V8.
const MARKER: &str = "__zs_native_fetch#";

fn pack_pending(id: u64) -> ResolveValue {
    let marker = format!("{MARKER}{id}\n");
    ResolveValue::Bytes(marker.into_bytes())
}

/// Inverse — returns the registry id if `bytes` is the marker shape.
pub fn unpack_pending(bytes: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(bytes).ok()?;
    let s = s.strip_prefix(MARKER)?;
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.parse::<u64>().ok()
}

// ===========================================================================
// install_fetch_global — wire `fetch` onto globalThis (D-22)
// ===========================================================================

/// Install the native `fetch()` callback onto `globalThis.fetch`,
/// shadowing whatever the JS polyfill set up. Per D-23 step 2c, the
/// native fetch is now the default — no env-var gate.
///
/// Run AFTER `install_dom` (which installs Request/Response/AbortSignal
/// natively) so the native fetch can read native Request internals
/// directly. Run AFTER `embed/fetch.js` so the polyfill's
/// `globalThis.fetch = fetch` is overwritten.
pub fn install_fetch_global(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    let tmpl = v8::FunctionTemplate::new(scope, fetch_callback);
    let func = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "fetch").unwrap();
    global.set(scope, key.into(), func.into());
}

// ===========================================================================
// fetch(input, init) — the V8 callback
// ===========================================================================

/// Per Fetch §5.1 "fetch":
///
///   1. Coerce input → Request (via the native Request constructor).
///   2. If signal.aborted, reject SYNCHRONOUSLY with the abort reason
///      (CRITICAL #6).
///   3. Otherwise, snapshot the request fields, spawn the algorithm
///      chain, and return a Promise that resolves to a Response.
fn fetch_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // No SharedState slot → not running inside a Runtime. Reject the
    // returned promise rather than panicking; callers in tests can
    // observe the wiring without spinning up the full pump.
    let state_opt = scope.get_slot::<SharedState>().cloned();
    let Some(state) = state_opt else {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let m = v8::String::new(scope, "fetch: no Runtime").unwrap();
        let exc = v8::Exception::error(scope, m);
        resolver.reject(scope, exc);
        rv.set(promise.into());
        return;
    };

    // Step 1: coerce input to a Request via `new Request(input, init)`.
    let req_obj = match coerce_to_request(scope, args.get(0), args.get(1)) {
        Some(o) => o,
        None => {
            // Constructor threw — exception is on the isolate; let V8
            // propagate it as a synchronous throw. (Spec strictly says
            // the fetch promise rejects with this throw value; matching
            // that requires an inner try/catch shim — same shape every
            // other implementation ships with.)
            return;
        }
    };

    // Step 2: synchronous abort check. Fetch §5.1 step 7.
    let signal_obj_opt = read_request_signal(scope, req_obj);
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);

    if let Some(sig) = signal_obj_opt {
        if crate::dom::abort_signal::is_aborted(scope, sig) {
            let reason = read_signal_reason(scope, sig).unwrap_or_else(|| {
                let m = v8::String::new(scope, "The operation was aborted.").unwrap();
                v8::Exception::error(scope, m)
            });
            resolver.reject(scope, reason);
            rv.set(promise.into());
            return;
        }
    }

    // Admission control.
    {
        let s = state.borrow();
        let in_flight_ops =
            s.pending_resolvers.len() + s.spawned_fetches.len() + s.spawned_ops.len();
        if in_flight_ops >= MAX_PENDING_OPS {
            drop(s);
            let m = v8::String::new(
                scope,
                &format!(
                    "Too many concurrent async operations (limit: {})",
                    MAX_PENDING_OPS
                ),
            )
            .unwrap();
            let exc = v8::Exception::range_error(scope, m);
            resolver.reject(scope, exc);
            rv.set(promise.into());
            return;
        }
        if s.in_flight_fetches >= MAX_PENDING_FETCHES {
            drop(s);
            let m = v8::String::new(
                scope,
                &format!(
                    "Too many concurrent fetches (limit: {})",
                    MAX_PENDING_FETCHES
                ),
            )
            .unwrap();
            let exc = v8::Exception::range_error(scope, m);
            resolver.reject(scope, exc);
            rv.set(promise.into());
            return;
        }
    }

    // Snapshot request fields.
    let alg_req = match snapshot_request(scope, req_obj) {
        Ok(r) => r,
        Err(msg) => {
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            resolver.reject(scope, exc);
            rv.set(promise.into());
            return;
        }
    };

    // Wire AbortSignal: register a Rust abort algorithm that flips the
    // CancelFlag the algorithm chain owns.
    let cancel = alg_req
        .cancel
        .clone()
        .expect("snapshot_request always populates cancel");
    if let Some(sig_obj) = signal_obj_opt {
        let cf = cancel.clone();
        crate::dom::abort_signal::add_abort_algorithm(
            scope,
            sig_obj,
            Box::new(move || {
                cf.cancel();
            }),
        );
    }

    let global_resolver = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;

    // Bump in-flight counter.
    state.borrow_mut().in_flight_fetches += 1;

    // Snapshot signal Global so the rejection path can read
    // `signal.reason` if abort fires.
    let signal_global: Option<v8::Global<v8::Object>> = signal_obj_opt
        .map(|o| v8::Global::new(scope, o));

    let state_for_task = state.clone();
    let fut: Pin<Box<dyn std::future::Future<Output = OpResult>>> = Box::pin(async move {
        let result = main_fetch(alg_req).await;

        // Always release the fetch-concurrency slot.
        {
            let mut s = state_for_task.borrow_mut();
            s.in_flight_fetches = s.in_flight_fetches.saturating_sub(1);
        }

        let id = match result {
            Ok(alg_resp) => stash(FetchOutcome::Resolve(alg_resp)),
            Err(err_msg) => stash(FetchOutcome::Reject(err_msg, signal_global.clone())),
        };
        OpResult::JsValue {
            resolver: global_resolver,
            value: pack_pending(id),
            request_id,
        }
    });

    {
        let mut s = state.borrow_mut();
        s.spawned_ops.push(fut);
    }

    let notify = state.borrow().pump_notify_tx.clone();
    if let Some(mut tx) = notify {
        let _ = tx.try_send(());
    }

    rv.set(promise.into());
}

// ===========================================================================
// Helpers — coercion & snapshot
// ===========================================================================

/// Run `new Request(input, init)` to coerce arbitrary inputs.
/// Returns None if the constructor threw (exception left on isolate).
fn coerce_to_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<v8::Value>,
    init: v8::Local<v8::Value>,
) -> Option<v8::Local<'s, v8::Object>> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "Request")?;
    let class_v = global.get(scope, key.into())?;
    let class_fn: v8::Local<v8::Function> = class_v.try_into().ok()?;

    let undef = v8::undefined(scope);
    let init_arg: v8::Local<v8::Value> = if init.is_undefined() {
        undef.into()
    } else {
        init
    };

    class_fn.new_instance(scope, &[input, init_arg])
}

fn read_request_signal<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    req: v8::Local<'s, v8::Object>,
) -> Option<v8::Local<'s, v8::Object>> {
    let key = v8::String::new(scope, "signal")?;
    let v = req.get(scope, key.into())?;
    if v.is_null_or_undefined() {
        return None;
    }
    v8::Local::<v8::Object>::try_from(v).ok()
}

fn read_signal_reason<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    sig: v8::Local<'s, v8::Object>,
) -> Option<v8::Local<'s, v8::Value>> {
    let key = v8::String::new(scope, "reason")?;
    let v = sig.get(scope, key.into())?;
    if v.is_undefined() {
        return None;
    }
    Some(v)
}

/// Snapshot the JS Request into a Rust FetchRequest. Drains rewindable
/// body sources to bytes; rejects streams (D-6 streaming send is
/// deferred — see comment).
fn snapshot_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    req: v8::Local<'s, v8::Object>,
) -> Result<AlgFetchRequest, String> {
    use crate::fetch_request::RequestState;

    // Reach the boxed RequestState via internal field 0.
    let raw = req
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        .map(|ext| ext.value() as *const RequestState)
        .ok_or_else(|| "fetch: input is not a native Request".to_string())?;
    if raw.is_null() {
        return Err("fetch: Request has null state".to_string());
    }
    // SAFETY: pointer stable for the lifetime of the wrapper.
    let state: &RequestState = unsafe { &*raw };

    let method = state.method.borrow().clone();
    let url = state.url.borrow().clone();
    let redirect_mode = RedirectMode::from_str(&state.redirect.borrow());
    let credentials_mode = CredentialsMode::from_str(&state.credentials.borrow());

    let headers = read_headers(scope, req)?;

    // Body: drain rewindable sources synchronously; reject streams.
    let body_impl = state.body.borrow();
    let (body_bytes, body_source) = match body_impl.source.clone() {
        Some(BodySource::Bytes(rc)) => (Some((*rc).clone()), Some(BodySource::Bytes(rc))),
        Some(BodySource::Blob(rc, mime)) => (
            Some((*rc).clone()),
            Some(BodySource::Blob(rc, mime)),
        ),
        Some(BodySource::UrlSearchParams(rc)) => (
            Some((*rc).clone()),
            Some(BodySource::UrlSearchParams(rc)),
        ),
        Some(BodySource::FormData(rc, b)) => (
            Some((*rc).clone()),
            Some(BodySource::FormData(rc, b)),
        ),
        Some(BodySource::Stream) => {
            // Streaming POST upload — D-6. For v1 we synchronously
            // reject. Streaming send-side requires reader-driven
            // chunked-transfer wiring through cyper; deferred.
            return Err(
                "fetch: streaming request body is not yet supported (use a buffered body)".to_string(),
            );
        }
        None => (None, None),
    };

    Ok(AlgFetchRequest {
        method,
        url: url.clone(),
        headers,
        body: body_bytes,
        body_source,
        redirect_mode,
        credentials_mode,
        cancel: Some(CancelFlag::new()),
        redirect_count: 0,
        origin_url: url,
    })
}

/// Iterate `request.headers` via `Array.from(...)` and collect into
/// `Vec<(name, value)>`.
fn read_headers<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    req: v8::Local<'s, v8::Object>,
) -> Result<Vec<(String, String)>, String> {
    let key = v8::String::new(scope, "headers").unwrap();
    let h_v = req
        .get(scope, key.into())
        .ok_or_else(|| "Request: missing headers".to_string())?;
    let h_obj: v8::Local<v8::Object> = h_v
        .try_into()
        .map_err(|_| "Request: headers is not an object".to_string())?;

    let global = scope.get_current_context().global(scope);
    let array_key = v8::String::new(scope, "Array").unwrap();
    let array_v = global
        .get(scope, array_key.into())
        .ok_or_else(|| "Array missing".to_string())?;
    let array_obj: v8::Local<v8::Object> = array_v
        .try_into()
        .map_err(|_| "Array missing".to_string())?;
    let from_key = v8::String::new(scope, "from").unwrap();
    let from_v = array_obj
        .get(scope, from_key.into())
        .ok_or_else(|| "Array.from missing".to_string())?;
    let from_fn: v8::Local<v8::Function> = from_v
        .try_into()
        .map_err(|_| "Array.from not callable".to_string())?;

    let result = from_fn
        .call(scope, array_obj.into(), &[h_obj.into()])
        .ok_or_else(|| "Array.from(headers) threw".to_string())?;

    let arr: v8::Local<v8::Array> = result
        .try_into()
        .map_err(|_| "Array.from did not return Array".to_string())?;

    let mut headers: Vec<(String, String)> = Vec::with_capacity(arr.length() as usize);
    for i in 0..arr.length() {
        let pair_v = arr
            .get_index(scope, i)
            .ok_or_else(|| "header pair missing".to_string())?;
        let pair: v8::Local<v8::Array> = pair_v
            .try_into()
            .map_err(|_| "header pair not an array".to_string())?;
        let name_v = pair
            .get_index(scope, 0)
            .ok_or_else(|| "header name missing".to_string())?;
        let value_v = pair
            .get_index(scope, 1)
            .ok_or_else(|| "header value missing".to_string())?;
        headers.push((
            name_v.to_rust_string_lossy(scope),
            value_v.to_rust_string_lossy(scope),
        ));
    }
    Ok(headers)
}

// ===========================================================================
// Materialisation — runs from the pump's V8 turn (after the async task
// resolves with a packed pending id)
// ===========================================================================

/// Build a JS Response from a stashed `AlgorithmResponse`, OR build the
/// rejection value if the stash holds an error. Called from
/// `runtime.rs::OpResult::JsValue` on `ResolveValue::Bytes` whose bytes
/// match the marker shape.
///
/// Returns `Ok(value)` to RESOLVE the resolver with that value, or
/// `Err(value)` to REJECT with that value.
pub fn materialise_pending<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    id: u64,
) -> Result<v8::Local<'s, v8::Value>, v8::Local<'s, v8::Value>> {
    let outcome = PENDING.with(|s| s.borrow_mut().remove(&id));
    let outcome = match outcome {
        Some(o) => o,
        None => {
            let msg = v8::String::new(scope, "fetch: missing pending result").unwrap();
            return Err(v8::Exception::error(scope, msg));
        }
    };
    match outcome {
        FetchOutcome::Resolve(resp) => Ok(build_response_object(scope, resp).into()),
        FetchOutcome::Reject(msg, signal) => {
            // Per Fetch §5.1: if signal aborted during fetch, use
            // signal.reason as the rejection.
            if let Some(sig_g) = signal {
                let sig = v8::Local::new(scope, sig_g);
                if crate::dom::abort_signal::is_aborted(scope, sig) {
                    if let Some(reason) = read_signal_reason(scope, sig) {
                        return Err(reason);
                    }
                }
            }
            let m = v8::String::new(scope, &format!("Network request failed: {msg}")).unwrap();
            Err(v8::Exception::type_error(scope, m))
        }
    }
}

/// Build a JS Response object directly from algorithm output.
///
/// FIX B (perf): instead of running `new Response(body, init)` which
/// re-extracts the body bytes (Uint8Array → Vec<u8> ptr::copy →
/// Rc<Vec<u8>> + builds a JS ReadableStream wrapping the bytes), we:
///
///   1. Construct the Response wrapper with `null` body so the JS
///      constructor takes the cheap null-body path (no extract_body
///      run, no stream wrapper alloc).
///   2. Move the bytes from `alg.body` into a fresh BodyImpl with
///      `BodySource::Bytes(rc)` and `stream: None`. The body() getter
///      and consumer fast paths (FIX C) read source directly.
///   3. Patch url + redirected as before.
///
/// Lazy stream materialization: when the user code reads
/// `response.body` (rare in benchmarks; common for streaming),
/// the body getter (added below) lazily builds the JS stream
/// wrapper on first access.
///
/// Eliminates per-fetch:
///   - 1 ArrayBuffer + Uint8Array alloc (the body argument)
///   - 1 v8::Function::new_instance JS->JS hop (Response ctor)
///   - 1 extract_body run (read_buffer_source_bytes copy of body
///     bytes, build_byte_stream wrapper alloc)
///   - 1 ReadableStream constructor invocation
fn build_response_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmResponse,
) -> v8::Local<'s, v8::Object> {
    let global = scope.get_current_context().global(scope);
    let class_key = v8::String::new(scope, "Response").unwrap();
    let class_v = global.get(scope, class_key.into()).unwrap();
    let class_fn: v8::Local<v8::Function> = class_v.try_into().unwrap();

    // Build init.
    let init = v8::Object::new(scope);
    {
        let key = v8::String::new(scope, "status").unwrap();
        let v = v8::Integer::new_from_unsigned(scope, alg.status as u32);
        init.set(scope, key.into(), v.into());
    }
    if !alg.status_text.is_empty() {
        let key = v8::String::new(scope, "statusText").unwrap();
        let v = v8::String::new(scope, &alg.status_text).unwrap();
        init.set(scope, key.into(), v.into());
    }
    {
        let arr = v8::Array::new(scope, alg.headers.len() as i32);
        for (i, (k, v)) in alg.headers.iter().enumerate() {
            let pair = v8::Array::new(scope, 2);
            let nk = v8::String::new(scope, k).unwrap();
            let nv = v8::String::new(scope, v).unwrap();
            pair.set_index(scope, 0, nk.into());
            pair.set_index(scope, 1, nv.into());
            arr.set_index(scope, i as u32, pair.into());
        }
        let key = v8::String::new(scope, "headers").unwrap();
        init.set(scope, key.into(), arr.into());
    }

    // Step 1: construct with null body so the constructor takes
    // the cheap null-body path. We patch the body in step 2.
    let null_body = v8::null(scope);
    let result = class_fn
        .new_instance(scope, &[null_body.into(), init.into()])
        .unwrap();

    // Step 2: install the Rust-side body directly. Skips
    // extract_body's bytes copy + ReadableStream construction.
    if let Some(raw) = response_state_ptr_mut(scope, result) {
        // SAFETY: pointer stable for the lifetime of the wrapper.
        let state: &crate::fetch_response::ResponseState = unsafe { &*raw };
        *state.url.borrow_mut() = alg.url;
        *state.redirected.borrow_mut() = alg.redirected;

        // Null-body status set: leave the body as null per spec.
        if !matches!(alg.status, 101 | 103 | 204 | 205 | 304) {
            let len = alg.body.len() as u64;
            let body_rc = std::rc::Rc::new(alg.body);
            *state.body.borrow_mut() = crate::fetch_body::body::BodyImpl {
                // Stream stays None until first observation; the body
                // getter materializes a ReadableStream from `source`
                // on demand. Keeps the fast path zero-stream-alloc.
                stream: std::cell::RefCell::new(None),
                source: Some(crate::fetch_body::body::BodySource::Bytes(body_rc)),
                length: Some(len),
            };
        }
    }

    result
}

fn response_state_ptr_mut<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<'s, v8::Object>,
) -> Option<*mut crate::fetch_response::ResponseState> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *mut crate::fetch_response::ResponseState;
    if ptr.is_null() {
        return None;
    }
    Some(ptr)
}

fn response_state_ptr<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<'s, v8::Object>,
) -> Option<*const crate::fetch_response::ResponseState> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *const crate::fetch_response::ResponseState;
    if ptr.is_null() {
        return None;
    }
    Some(ptr)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_round_trip() {
        let pv = pack_pending(42);
        match pv {
            ResolveValue::Bytes(b) => {
                assert_eq!(unpack_pending(&b), Some(42));
            }
            _ => panic!("expected Bytes variant"),
        }
    }

    #[test]
    fn unpack_returns_none_on_unrelated_bytes() {
        assert_eq!(unpack_pending(b"hello"), None);
    }
}
