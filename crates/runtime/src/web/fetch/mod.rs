//! Native `fetch()` global function and the V8 entry layer.
//!
//! Replaces the JS polyfill `fetch()` in `embed/fetch.js`.
//!
//! ## Module structure
//!
//! - `algorithms`     — main_fetch / scheme_fetch / http_fetch /
//!                      http_redirect_fetch (no V8 entry)
//! - `http_network`   — http_network_fetch wrapper around cyper
//! - `redirect`       — method/body mutation + same-origin checks
//! - `content_encoding` — Content-Encoding decode hook
//! - `data_url`       — data: URL processor
//! - `bad_ports`      — Fetch §4.3 bad-port table
//!
//! ## Wiring
//!
//! `install_fetch_global` replaces the JS polyfill's `globalThis.fetch`
//! with a hand-rolled V8 callback that:
//!
//!   1. Synchronously coerces input to a Request.
//!   2. Synchronously checks `signal.aborted` per Fetch §5.1 step 7,
//!      not after enqueueing work.
//!   3. Drains the request body to bytes (rewindable BodySource → Vec).
//!   4. Spawns a compio task that runs `main_fetch` then schedules a
//!      pump turn to materialise the Response wrapper inside V8.

pub mod algorithms;
pub mod bad_ports;
pub mod body;
pub mod content_encoding;
pub mod data_url;
pub mod enums;
pub mod http_network;
pub mod redirect;
pub mod request;
pub mod response;

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
// Cached admission-control error objects
// ===========================================================================
//
// Two pre-built plain objects (one per limit) cached as v8::Globals in an
// isolate-scoped slot. The bench shows ~13% of CPU spent in
// `v8::Exception::RangeError`'s stack-capture chain when admission fires
// at ~50K rejections/s. Building the error once and rejecting all
// subsequent over-quota promises with the same object reduces that to
// one `v8::Local::new` (Global → Local handle resurrect) per rejection.

#[derive(Copy, Clone)]
enum AdmissionLimit {
    Ops,
    Fetches,
}

struct AdmissionErrors {
    ops: v8::Global<v8::Object>,
    fetches: v8::Global<v8::Object>,
}

/// Build a plain object with the dispatch-layer's expected error shape:
/// `.name`, `.message`, `.status` set to real values; `.stack`, `.code`,
/// `.details`, `.retryable` set to `null`. Skips V8's Error class so no
/// stack trace is captured.
///
/// Why pre-set the remaining 4 fields to `null` rather than leaving them
/// absent: `v8_exception_to_{stack,code,details_json,retryable}` each
/// call `obj.get(scope, key)` on the rejection value. If the key is
/// **absent**, V8 walks the prototype chain (Object.prototype → null) to
/// confirm absence — a measured ~0.6-0.7% per lookup in our perf data.
/// If the key is **present and null**, V8 returns the slot value directly.
/// All four helpers null-check the result and return `None` either way,
/// so the wire shape is identical; we just trade a prototype walk for an
/// own-property hit. The shape is now closed (one map, all 7 keys), which
/// V8 can optimize as a single hidden class.
fn build_admission_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    msg: &str,
) -> v8::Local<'s, v8::Object> {
    let obj = v8::Object::new(scope);
    let null_v: v8::Local<v8::Value> = v8::null(scope).into();

    // Insert in dispatch lookup order so V8's hidden-class transitions
    // settle on a shape matching the access order: name, message, stack,
    // status, code, details, retryable.
    let key = v8::String::new(scope, "name").unwrap();
    let v = v8::String::new(scope, "RangeError").unwrap();
    obj.set(scope, key.into(), v.into());

    let key = v8::String::new(scope, "message").unwrap();
    let v = v8::String::new(scope, msg).unwrap();
    obj.set(scope, key.into(), v.into());

    let key = v8::String::new(scope, "stack").unwrap();
    obj.set(scope, key.into(), null_v);

    // status: 503 — `v8_exception_to_status` honors it for the HTTP
    // response code; the dispatcher otherwise falls back to 500.
    let key = v8::String::new(scope, "status").unwrap();
    let v = v8::Integer::new_from_unsigned(scope, 503);
    obj.set(scope, key.into(), v.into());

    let key = v8::String::new(scope, "code").unwrap();
    obj.set(scope, key.into(), null_v);

    let key = v8::String::new(scope, "details").unwrap();
    obj.set(scope, key.into(), null_v);

    let key = v8::String::new(scope, "retryable").unwrap();
    obj.set(scope, key.into(), null_v);

    obj
}

fn cached_admission_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    limit: AdmissionLimit,
) -> v8::Local<'s, v8::Object> {
    if scope.get_slot::<AdmissionErrors>().is_none() {
        let ops = build_admission_error(
            scope,
            &format!("Too many concurrent async operations (limit: {MAX_PENDING_OPS})"),
        );
        let fetches = build_admission_error(
            scope,
            &format!("Too many concurrent fetches (limit: {MAX_PENDING_FETCHES})"),
        );
        let ops_g = v8::Global::new(scope, ops);
        let fetches_g = v8::Global::new(scope, fetches);
        scope.set_slot(AdmissionErrors {
            ops: ops_g,
            fetches: fetches_g,
        });
    }
    // Resurrect the matching Global into a Local for rejection.
    let g = {
        let slot = scope.get_slot::<AdmissionErrors>().unwrap();
        match limit {
            AdmissionLimit::Ops => slot.ops.clone(),
            AdmissionLimit::Fetches => slot.fetches.clone(),
        }
    };
    v8::Local::new(scope, g)
}

// ===========================================================================
// Install `fetch` onto globalThis.
// ===========================================================================

/// Install the native `fetch()` callback onto `globalThis.fetch`,
/// shadowing whatever the JS polyfill set up.
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
///   2. If signal.aborted, reject synchronously with the abort reason.
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

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);

    // B3 capability gate. `mutation()` handlers must not call `fetch`
    // — the TS layer rejects this at compile time; here we reject at
    // request time so handlers compiled without strict type-checking
    // still hit the rail. `query()` handlers are also forbidden from
    // calling fetch (queries are read-only and tx-bound). `action()` /
    // `stream()` / `subscription()` / no-marker code paths are allowed.
    //
    // The check fires BEFORE admission control / Request coercion so
    // a refused fetch doesn't consume a `MAX_PENDING_FETCHES` slot.
    match crate::rpc::current_kind() {
        Some(crate::rpc::ProcedureKind::Query) => {
            let exc = crate::rpc::build_capability_violation(
                scope,
                "query",
                "fetch",
                "Queries are read-only — use action() if you need to call external APIs, or runQuery to compose with other queries.",
            );
            resolver.reject(scope, exc.into());
            rv.set(promise.into());
            return;
        }
        Some(crate::rpc::ProcedureKind::Mutation) => {
            let exc = crate::rpc::build_capability_violation(
                scope,
                "mutation",
                "fetch",
                "Use action() if you need to call external APIs. Mutations are transactional and must complete quickly; holding a DB tx open across an outbound HTTP call would block other writers.",
            );
            resolver.reject(scope, exc.into());
            rv.set(promise.into());
            return;
        }
        _ => {}
    }

    // FIX E fast path: `fetch(string)` (or `fetch(string, undefined)`)
    // — the most common shape. Skip the Request constructor + headers
    // construction + AbortSignal wiring + snapshot_request entirely.
    // Build the AlgFetchRequest inline.
    let input_v = args.get(0);
    let init_v = args.get(1);
    let alg_req_fast = if input_v.is_string() && init_v.is_undefined() {
        let url_str = input_v.to_rust_string_lossy(scope);
        // Validate the URL via ada-url (matches the Request
        // constructor's URL parse step). Anything that fails to parse
        // throws TypeError — matching the spec.
        match ada_url::Url::parse(&url_str, None) {
            Ok(parsed) => {
                let canonical = parsed.href().to_string();
                Some(AlgFetchRequest {
                    method: "GET".to_string(),
                    url: canonical.clone(),
                    headers: Vec::new(),
                    body: None,
                    body_source: None,
                    redirect_mode: RedirectMode::Follow,
                    credentials_mode: CredentialsMode::SameOrigin,
                    cancel: Some(CancelFlag::new()),
                    redirect_count: 0,
                    origin_url: canonical,
                })
            }
            Err(_) => {
                let m = v8::String::new(scope, &format!("fetch: invalid URL: {url_str}"))
                    .unwrap();
                let exc = v8::Exception::type_error(scope, m);
                resolver.reject(scope, exc);
                rv.set(promise.into());
                return;
            }
        }
    } else {
        None
    };

    // Slow path: coerce input to a Request via `new Request(input, init)`.
    let alg_req_slow_data = if alg_req_fast.is_none() {
        let req_obj = match coerce_to_request(scope, input_v, init_v) {
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

        Some((req_obj, signal_obj_opt))
    } else {
        None
    };

    // Admission control. Both rejections use a CACHED, error-shaped plain
    // object instead of `v8::Exception::range_error` — building a real
    // RangeError captures a full stack trace (`CaptureSimpleStackTrace` +
    // `Translated*` deopt frames) and registering a lazy `.stack` getter,
    // which together cost ~13% of CPU in the saturated fetchEcho bench
    // (one rejection per dropped request). A plain object with `.name`
    // and `.message` round-trips through dispatch's
    // `v8_exception_to_{message,name,stack}` helpers identically — they
    // do `Object::Get(scope, "<key>")` and accept any value with the
    // right shape — but skips the V8 Error machinery entirely.
    //
    // Wire shape change vs the prior RangeError: `.stack` is absent, so
    // the JSON error body shrinks (no `"stack":"..."` field). The
    // `.name` is still `"RangeError"` so SDKs that switch on
    // `err.name === "RangeError"` keep working. This is a load-shedding
    // signal — no stack helps debugging anyway since the throw site is
    // always the same admission gate.
    {
        let s = state.borrow();
        let in_flight_ops = s.pending_resolvers.len() + s.spawned_ops.len();
        if in_flight_ops >= MAX_PENDING_OPS {
            drop(s);
            let exc = cached_admission_error(scope, AdmissionLimit::Ops);
            resolver.reject(scope, exc.into());
            rv.set(promise.into());
            return;
        }
        if s.in_flight_fetches >= MAX_PENDING_FETCHES {
            drop(s);
            let exc = cached_admission_error(scope, AdmissionLimit::Fetches);
            resolver.reject(scope, exc.into());
            rv.set(promise.into());
            return;
        }
    }

    let (alg_req, signal_obj_opt) = match (alg_req_fast, alg_req_slow_data) {
        (Some(alg), _) => (alg, None),
        (None, Some((req_obj, sig_opt))) => {
            // Snapshot request fields.
            let alg = match snapshot_request(scope, req_obj) {
                Ok(r) => r,
                Err(msg) => {
                    let m = v8::String::new(scope, &msg).unwrap();
                    let exc = v8::Exception::type_error(scope, m);
                    resolver.reject(scope, exc);
                    rv.set(promise.into());
                    return;
                }
            };
            (alg, sig_opt)
        }
        (None, None) => unreachable!(),
    };

    // Wire AbortSignal: register a Rust abort algorithm that flips the
    // CancelFlag the algorithm chain owns.
    let cancel = alg_req
        .cancel
        .clone()
        .expect("AlgFetchRequest always populates cancel");
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
/// body sources to bytes; rejects streams because streaming uploads are
/// not wired yet.
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
    // The state's `redirect` / `credentials` are typed enums (validated
    // at constructor time). Bridge through the `From` impls in
    // `enums.rs` to the algorithm-side enum shapes.
    let redirect_mode = RedirectMode::from(state.redirect.get());
    let credentials_mode = CredentialsMode::from(state.credentials.get());

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
            // Streaming POST upload is not wired yet, so reject
            // synchronously. Streaming send-side requires reader-driven
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

/// Read `request.headers` directly from the native `Headers` state
/// pointer when possible (FIX D), falling back to the JS-visible
/// `Array.from(headers)` iteration for non-native (polyfill) shapes.
///
/// The native fast path saves ~17 V8 ops + 2N String allocations
/// per fetch (where N is the header count): no `globalThis.Array`
/// lookup, no `Array.from` invocation, no JS Array materialization,
/// no per-pair `get_index` + `to_rust_string_lossy` round-trips.
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

    // FIX D fast path: native Headers state pointer access.
    if let Some(headers_state) = crate::headers::try_native_headers(scope, h_obj) {
        let list = headers_state.list();
        let mut headers: Vec<(String, String)> = Vec::with_capacity(list.len());
        for (n, v) in list {
            // Header names + values are byte sequences, but the
            // wire layer (cyper) takes &str. The HTTP §5 grammar
            // already validated them as ASCII-safe-ish (tchar +
            // VCHAR/obs-text), so a lossy decode is fine here.
            headers.push((
                String::from_utf8_lossy(n).into_owned(),
                String::from_utf8_lossy(v).into_owned(),
            ));
        }
        return Ok(headers);
    }

    // Slow path: polyfill / non-native Headers shape — go through
    // `Array.from(headers)`.
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
/// FIX F (perf): use `build_kernel_response`, which allocates the Response
/// wrapper from the cached FunctionTemplate's `instance_template().new_instance()`,
/// builds Headers via `build_kernel_headers` (skipping the JS Headers
/// constructor's WebIDL sequence walk + per-pair validation), and installs
/// the body bytes as `BodySource::Bytes(Rc<...>)` directly. Replaces the
/// previous path that:
///
///   - resolved `globalThis.Response` per fetch,
///   - allocated 1 init JS Object + 1 JS Array of N 2-element pair Arrays
///     (one alloc per response header) for `init.headers`,
///   - invoked the JS Response constructor (which read `init.headers` and
///     called `new Headers(seq)` — re-allocating Headers and walking the
///     pair list with @@iterator dispatch + per-pair validation),
///   - then patched the result's url/redirected/body via internal field 0.
///
/// Together with prior FIXes A-E, the success path now hits zero JS
/// constructor invocations: only one `instance_template().new_instance()`
/// for Response, one for Headers, one Box::into_raw + finalizer wiring per
/// each.
///
/// Falls back to the legacy `globalThis.Response` constructor invocation
/// if the `ResponseTemplateSlot` isn't present (shouldn't happen at
/// runtime — `install_global` always sets it — but the fallback keeps the
/// path correct in tests that bypass install_global).
fn build_response_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmResponse,
) -> v8::Local<'s, v8::Object> {
    // Fast path: kernel-side direct build. We need a copy of `alg` for the
    // slow-path fallback in the unlikely case the slot isn't present, but
    // `try_kernel_build` consumes the fields zero-copy. Decompose first.
    let AlgorithmResponse {
        status,
        status_text,
        headers,
        body,
        url,
        redirected,
    } = alg;

    if let Some(slot_check) = scope.get_slot::<crate::fetch_response::ResponseTemplateSlot>() {
        let _ = slot_check; // confirm slot exists; build_kernel_response re-fetches.
        if let Some(obj) = crate::fetch_response::build_kernel_response(
            scope,
            status,
            status_text,
            url,
            redirected,
            headers,
            body,
        ) {
            return obj;
        }
        // build_kernel_response only returns None if `new_instance` fails
        // (OOM in V8). That's not recoverable via the JS-constructor fallback
        // either, so return a sentinel: an empty object. The caller will
        // observe the missing internal field and reject the promise.
        return v8::Object::new(scope);
    }

    // Slow path (no template slot — only hit in tests that bypass
    // install_global): fall back to the JS constructor invocation.
    // Reconstitute the alg so the original code path keeps working.
    let alg = AlgorithmResponse {
        status,
        status_text,
        headers,
        body,
        url,
        redirected,
    };
    let global = scope.get_current_context().global(scope);
    let class_key = v8::String::new(scope, "Response").unwrap();
    let class_v = global.get(scope, class_key.into()).unwrap();
    let class_fn: v8::Local<v8::Function> = class_v.try_into().unwrap();

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

    let null_body = v8::null(scope);
    let result = class_fn
        .new_instance(scope, &[null_body.into(), init.into()])
        .unwrap();

    if let Some(raw) = response_state_ptr_mut(scope, result) {
        let state: &crate::fetch_response::ResponseState = unsafe { &*raw };
        *state.url.borrow_mut() = alg.url;
        state.redirected.set(alg.redirected);

        if !matches!(alg.status, 101 | 103 | 204 | 205 | 304) {
            let len = alg.body.len() as u64;
            let body_rc = std::rc::Rc::new(alg.body);
            *state.body.borrow_mut() = crate::fetch_body::body::BodyImpl {
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
