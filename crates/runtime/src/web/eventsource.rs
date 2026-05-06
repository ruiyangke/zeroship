//! Native `EventSource` per HTML §9.2 (Server-Sent Events client).
//!
//! Spec: https://html.spec.whatwg.org/multipage/server-sent-events.html
//!
//! `EventSource` is a unidirectional stream of `MessageEvent`s built on
//! top of an HTTP `text/event-stream` response. Used by creator code to
//! consume third-party SSE feeds (OpenAI streaming, GitHub event
//! streams, etc.).
//!
//! ## Design notes
//!
//! - Implemented as a `#[v8_class]` Rust struct that inherits
//!   `EventTarget` for `addEventListener` / `dispatchEvent`.
//! - Network IO is delegated to `globalThis.fetch(url, { headers, signal })`
//!   so it inherits TLS, redirect handling, SSRF guards, HTTP/2, and
//!   request budget admission for free.
//! - The response body is a `ReadableStream` whose default reader is
//!   driven from Rust via promise-reaction callbacks (mirrors
//!   `streams::response_forwarder`).
//! - The SSE wire parser lives in this file. Per-line state is kept in
//!   `ParserState`; a blank line dispatches the accumulated event.
//! - Reconnection: on connection drop / IO error / non-2xx with
//!   reconnectable status, schedule a re-connect via `setTimeout` with
//!   the current `retry_ms` (3000 default; updated by `retry:` lines).
//! - `close()` flips `readyState = CLOSED`, aborts the in-flight fetch
//!   via the owned AbortController, and refuses further reconnects.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_constructor, v8_getter, v8_inherit, v8_method, v8_name, v8_setter,
};

use crate::dom::event_target::{self, EventTarget};
use crate::dom::message_event::MessageEventState;
use crate::state::OpError;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const READY_CONNECTING: u32 = 0;
const READY_OPEN: u32 = 1;
const READY_CLOSED: u32 = 2;

/// Default reconnection delay per spec §9.2.5 (3 seconds is the de-facto
/// default; the spec leaves it implementation-defined).
const DEFAULT_RETRY_MS: u32 = 3000;

// ---------------------------------------------------------------------------
// Parser state
// ---------------------------------------------------------------------------

/// Per HTML §9.2.6 "Parsing an event stream" — the parser keeps a
/// rolling line buffer and per-event accumulators.
#[derive(Default)]
pub struct ParserState {
    /// Bytes received but not yet line-terminated.
    line_buffer: Vec<u8>,
    /// True if the previous chunk's last byte was `\r` — used to elide
    /// a leading `\n` (CRLF normalisation per §9.2.6 step 6.1).
    last_byte_was_cr: bool,

    /// Accumulators per spec §9.2.6 "process the field".
    event_type: String,
    data_buffer: String,
    /// Last-seen `id:` (non-empty fields) — copied into
    /// `EventSource.last_event_id` when an event is dispatched.
    last_event_id: String,
}

// ---------------------------------------------------------------------------
// EventSource state
// ---------------------------------------------------------------------------

/// Backing state for an `EventSource` JS wrapper. `#[repr(C)]` + the
/// `event_target` field at offset 0 is mandatory: the `#[v8_inherit
/// (EventTarget)]` macro generates an EventTarget-pointer cast that
/// reads internal field 0 → `Box<EventTarget>` for inherited methods.
#[repr(C)]
pub struct EventSourceState {
    /// EventTarget marker — listeners hang off the JS wrapper via a
    /// private symbol (see `event_target::attach_listeners`); this
    /// field exists for the inheritance cast only.
    pub event_target: EventTarget,

    /// `[[ready state]]` per spec — 0 CONNECTING / 1 OPEN / 2 CLOSED.
    pub ready_state: Cell<u32>,

    /// `[[url]]` — serialised origin-form URL.
    pub url: RefCell<String>,

    /// `[[withCredentials]]` — passes through to `fetch(..., { credentials })`.
    pub with_credentials: Cell<bool>,

    /// `[[reconnection time]]` — current backoff, server-overridable
    /// via `retry: <ms>` lines.
    pub retry_ms: Cell<u32>,

    /// `[[last event ID string]]` — sent as `Last-Event-ID` on
    /// reconnect; updated by `id:` lines.
    pub last_event_id: RefCell<String>,

    /// AbortController owned by this EventSource — `close()` runs the
    /// abort algorithm to cancel any in-flight fetch / read.
    pub abort_controller: RefCell<Option<v8::Global<v8::Object>>>,

    /// Cached EventSource wrapper Global (the JS object exposed to user
    /// code). Captured at construction so the promise reaction
    /// callbacks can dispatch events on the right target.
    pub wrapper: RefCell<Option<v8::Global<v8::Object>>>,

    /// Live SSE parser state — borrowed mut by `feed_chunk`.
    pub parser: RefCell<ParserState>,

    /// True once `close()` has been called — gates reconnect.
    pub closed_by_user: Cell<bool>,

    /// `onmessage` / `onopen` / `onerror` EventHandler IDL slots.
    /// Stored in dedicated Cell-like slots so the setter can both
    /// (a) install/remove the listener via EventTarget AND (b) return
    /// the same function from the getter (HTML §8.1.5.1).
    pub on_message: RefCell<Option<v8::Global<v8::Function>>>,
    pub on_open: RefCell<Option<v8::Global<v8::Function>>>,
    pub on_error: RefCell<Option<v8::Global<v8::Function>>>,
}

impl Default for EventSourceState {
    fn default() -> Self {
        Self {
            event_target: EventTarget,
            ready_state: Cell::new(READY_CONNECTING),
            url: RefCell::new(String::new()),
            with_credentials: Cell::new(false),
            retry_ms: Cell::new(DEFAULT_RETRY_MS),
            last_event_id: RefCell::new(String::new()),
            abort_controller: RefCell::new(None),
            wrapper: RefCell::new(None),
            parser: RefCell::new(ParserState::default()),
            closed_by_user: Cell::new(false),
            on_message: RefCell::new(None),
            on_open: RefCell::new(None),
            on_error: RefCell::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// IDL surface
// ---------------------------------------------------------------------------

/// `EventSource` per HTML §9.2.
///
/// IDL:
/// ```webidl
/// [Exposed=(Window,Worker)]
/// interface EventSource : EventTarget {
///   constructor(USVString url, optional EventSourceInit eventSourceInitDict = {});
///   readonly attribute USVString url;
///   readonly attribute boolean withCredentials;
///   const unsigned short CONNECTING = 0;
///   const unsigned short OPEN = 1;
///   const unsigned short CLOSED = 2;
///   readonly attribute unsigned short readyState;
///   undefined close();
///   attribute EventHandler onopen;
///   attribute EventHandler onmessage;
///   attribute EventHandler onerror;
/// };
/// dictionary EventSourceInit {
///   boolean withCredentials = false;
/// };
/// ```
#[v8_class]
#[v8_inherit(crate::dom::event_target::EventTarget)]
impl EventSourceState {
    /// `new EventSource(url, init?)` — HTML §9.2.2.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        wrapper: v8::Local<v8::Object>,
        url_arg: v8::Local<v8::Value>,
        init_arg: v8::Local<v8::Value>,
    ) -> Result<EventSourceState, OpError> {
        if url_arg.is_undefined() {
            return Err(OpError::type_error(
                "EventSource: 'url' argument is required",
            ));
        }
        let url_str = url_arg.to_rust_string_lossy(scope);

        // Parse + canonicalise URL. The base is the current realm's
        // origin; we don't have a real Document so fall back to the
        // raw input (matching how fetch coerces an already-absolute
        // string).
        let canonical = match ada_url::Url::parse(&url_str, None) {
            Ok(u) => u.href().to_string(),
            Err(_) => {
                return Err(OpError::type_error(format!(
                    "EventSource: invalid URL: {url_str}"
                )));
            }
        };

        // Read EventSourceInit.withCredentials.
        let mut with_credentials = false;
        if let Ok(init_obj) = v8::Local::<v8::Object>::try_from(init_arg) {
            let key = v8::String::new(scope, "withCredentials").unwrap();
            if let Some(v) = init_obj.get(scope, key.into()) {
                if !v.is_undefined() {
                    with_credentials = v.boolean_value(scope);
                }
            }
        }

        let state = EventSourceState::default();
        *state.url.borrow_mut() = canonical;
        state.with_credentials.set(with_credentials);

        // Listeners must be attached on the wrapper for dispatchEvent to
        // find handlers. The macro doesn't auto-attach — we do it here
        // to mirror AbortSignal's mint helper.
        event_target::attach_listeners(scope, wrapper);

        // Stash the wrapper Global so reaction callbacks can dispatch
        // events on the right target.
        *state.wrapper.borrow_mut() = Some(v8::Global::new(scope, wrapper));

        // Mint an AbortSignal (via AbortController) and stash the
        // controller so close() can fire it.
        let controller_obj = mint_abort_controller(scope);
        if let Some(ctl) = controller_obj.as_ref() {
            *state.abort_controller.borrow_mut() = Some(ctl.clone());
        }

        // Spawn the connect — does NOT block construction. The fetch
        // promise + read loop runs entirely on the V8 task graph.
        //
        // IMPORTANT: defer via `setTimeout(.., 0)`. The macro sets the
        // wrapper's internal field 0 (`Box<EventSourceState>`) AFTER
        // this constructor returns, so calling `spawn_connect` directly
        // would fail to read the state via `state_from_wrapper`. The
        // 0-ms timer fires after the constructor completes, by which
        // point the External has been attached.
        if let Some(ctl_g) = controller_obj {
            schedule_initial_connect(scope, wrapper, ctl_g);
        }

        Ok(state)
    }

    /// `eventSource.url` — USVString.
    #[v8_getter]
    fn url(&self) -> String {
        self.url.borrow().clone()
    }

    /// `eventSource.withCredentials` — boolean.
    #[v8_getter]
    #[v8_name = "withCredentials"]
    fn with_credentials(&self) -> bool {
        self.with_credentials.get()
    }

    /// `eventSource.readyState` — unsigned short.
    #[v8_getter]
    #[v8_name = "readyState"]
    fn ready_state(&self) -> u32 {
        self.ready_state.get()
    }

    /// Implementation-defined extension: surfaces the current
    /// `Last-Event-ID` so JS code can introspect for resume logic.
    /// The HTML spec does not officially expose this, but most other
    /// EventSource implementations (Chrome / Firefox) do not either —
    /// kept here for parity with the Last-Event-ID HTTP header we set.
    #[v8_getter]
    #[v8_name = "lastEventId"]
    fn last_event_id(&self) -> String {
        self.last_event_id.borrow().clone()
    }

    // EventHandler IDL attributes — HTML §8.1.5.1.
    //
    // `es.onmessage = fn` registers `fn` as a "message" listener; reading
    // `es.onmessage` returns the stored function (or null). Each setter
    // first removes the previously-installed listener (if any), then
    // installs the new one via the EventTarget machinery.

    #[v8_setter]
    #[v8_name = "onmessage"]
    fn set_onmessage(
        &self,
        scope: &mut v8::PinScope,
        v: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        set_event_handler(scope, self, "message", v, |s| s.on_message.borrow_mut())
    }
    #[v8_getter]
    #[v8_name = "onmessage"]
    fn get_onmessage<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        get_event_handler(scope, self, |s| s.on_message.borrow().clone())
    }

    #[v8_setter]
    #[v8_name = "onopen"]
    fn set_onopen(
        &self,
        scope: &mut v8::PinScope,
        v: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        set_event_handler(scope, self, "open", v, |s| s.on_open.borrow_mut())
    }
    #[v8_getter]
    #[v8_name = "onopen"]
    fn get_onopen<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        get_event_handler(scope, self, |s| s.on_open.borrow().clone())
    }

    #[v8_setter]
    #[v8_name = "onerror"]
    fn set_onerror(
        &self,
        scope: &mut v8::PinScope,
        v: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        set_event_handler(scope, self, "error", v, |s| s.on_error.borrow_mut())
    }
    #[v8_getter]
    #[v8_name = "onerror"]
    fn get_onerror<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        get_event_handler(scope, self, |s| s.on_error.borrow().clone())
    }

    /// `eventSource.close()` — HTML §9.2.4. Sets ready state to CLOSED
    /// and aborts the in-flight fetch.
    #[v8_method]
    fn close(&self, scope: &mut v8::PinScope) {
        self.closed_by_user.set(true);
        self.ready_state.set(READY_CLOSED);
        // Abort the in-flight fetch via the owned controller. The
        // AbortSignal's `aborted` flag is read by the fetch reaction
        // callbacks; both the fetch promise rejection AND the
        // reader.read() rejection bottom out in `close_fetch_loop`.
        if let Some(ctl_g) = self.abort_controller.borrow().clone() {
            let ctl = v8::Local::new(scope, ctl_g);
            let key = v8::String::new(scope, "abort").unwrap();
            if let Some(abort_v) = ctl.get(scope, key.into()) {
                if let Ok(abort_fn) = v8::Local::<v8::Function>::try_from(abort_v) {
                    let und: v8::Local<v8::Value> = v8::undefined(scope).into();
                    let _ = abort_fn.call(scope, ctl.into(), &[und]);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// EventHandler IDL helpers
// ---------------------------------------------------------------------------

fn set_event_handler<F>(
    scope: &mut v8::PinScope,
    state: &EventSourceState,
    event_name: &str,
    fn_arg: v8::Local<v8::Value>,
    slot: F,
) -> Result<(), OpError>
where
    F: FnOnce(&EventSourceState) -> std::cell::RefMut<'_, Option<v8::Global<v8::Function>>>,
{
    // HTML §8.1.5.1 step 4: non-callable → null.
    let new_handler: Option<v8::Global<v8::Function>> =
        v8::Local::<v8::Function>::try_from(fn_arg)
            .ok()
            .map(|f| v8::Global::new(scope, f));

    let mut slot_ref = slot(state);
    let prev = slot_ref.clone();
    *slot_ref = new_handler.clone();
    drop(slot_ref);

    let wrapper_g = state.wrapper.borrow().clone();
    if let Some(wg) = wrapper_g {
        let wrapper = v8::Local::new(scope, &wg);
        if prev.is_some() {
            event_target::remove_internal_listener(scope, wrapper, event_name);
        }
        if let Some(g) = new_handler {
            event_target::add_internal_listener(scope, wrapper, event_name, g);
        }
    }
    Ok(())
}

fn get_event_handler<'s, F>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &EventSourceState,
    slot: F,
) -> v8::Local<'s, v8::Value>
where
    F: for<'a> FnOnce(&'a EventSourceState) -> Option<v8::Global<v8::Function>>,
{
    match slot(state) {
        Some(g) => v8::Local::new(scope, g).into(),
        None => v8::null(scope).into(),
    }
}

// ---------------------------------------------------------------------------
// Helpers — read state from a wrapper
// ---------------------------------------------------------------------------

fn state_from_wrapper<'a>(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<&'a EventSourceState> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *const EventSourceState;
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { &*ptr })
}

// ---------------------------------------------------------------------------
// Connect / fetch / reader loop
// ---------------------------------------------------------------------------

/// Build a fresh AbortController via `globalThis.AbortController` and
/// return its Global. Returns `None` if the constructor is unavailable
/// (test isolates that don't install AbortController).
fn mint_abort_controller(scope: &mut v8::PinScope) -> Option<v8::Global<v8::Object>> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "AbortController").unwrap();
    let class_v = global.get(scope, key.into())?;
    let class_fn = v8::Local::<v8::Function>::try_from(class_v).ok()?;
    let inst = class_fn.new_instance(scope, &[])?;
    Some(v8::Global::new(scope, inst))
}

/// Defer the initial fetch + read loop until after the constructor
/// returns — the `#[v8_class]` macro only attaches the boxed state to
/// internal field 0 *after* the user-supplied `new()` returns, so
/// `state_from_wrapper` would fail if we called `spawn_connect`
/// synchronously. We register a 0-ms timer that fires on the next
/// microtask checkpoint, by which point the external is in place.
fn schedule_initial_connect(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Object>,
    controller_g: v8::Global<v8::Object>,
) {
    let wrapper_g = v8::Global::new(scope, wrapper);
    let connect_fn = build_initial_connect_fn(scope, wrapper_g, controller_g);

    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "setTimeout").unwrap();
    let st_v = match global.get(scope, key.into()) {
        Some(v) => v,
        None => return,
    };
    let Ok(st_fn) = v8::Local::<v8::Function>::try_from(st_v) else {
        return;
    };
    let und: v8::Local<v8::Value> = v8::undefined(scope).into();
    let zero = v8::Integer::new(scope, 0);
    let _ = st_fn.call(scope, und, &[connect_fn.into(), zero.into()]);
}

struct InitialConnectPayload {
    wrapper: v8::Global<v8::Object>,
    controller: v8::Global<v8::Object>,
}

fn build_initial_connect_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    wrapper: v8::Global<v8::Object>,
    controller: v8::Global<v8::Object>,
) -> v8::Local<'s, v8::Function> {
    let boxed = Box::new(InitialConnectPayload { wrapper, controller });
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    let tmpl = v8::FunctionTemplate::builder(initial_connect_callback)
        .data(ext.into())
        .build(scope);
    let f = tmpl.get_function(scope).unwrap();
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        f,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut InitialConnectPayload));
        }),
    );
    std::mem::forget(weak);
    f
}

fn initial_connect_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const InitialConnectPayload;
    if raw.is_null() {
        return;
    }
    let payload: &InitialConnectPayload = unsafe { &*raw };
    let wrapper = v8::Local::new(scope, &payload.wrapper);
    spawn_connect(scope, wrapper, payload.controller.clone());
}

/// Captures shared by every promise-reaction callback in the loop. All
/// callbacks key on the EventSource wrapper Global; the boxed state is
/// re-resolved from the wrapper on each callback (saves us from
/// threading a raw pointer through V8's GC).
struct LoopCaptures {
    wrapper: v8::Global<v8::Object>,
    /// Owned AbortController — kept alive across the loop.
    _controller: v8::Global<v8::Object>,
    /// The reader Global; populated when the fetch resolves and the
    /// body is locked. Used by `schedule_next_read`.
    reader: RefCell<Option<v8::Global<v8::Object>>>,
}

type SharedCaptures = Rc<LoopCaptures>;

/// Kick off `fetch(url, init)` and attach reactions to read the body.
fn spawn_connect(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Object>,
    controller_g: v8::Global<v8::Object>,
) {
    let Some(state) = state_from_wrapper(scope, wrapper) else {
        return;
    };
    let url_str = state.url.borrow().clone();
    let last_event_id = state.last_event_id.borrow().clone();
    let with_credentials = state.with_credentials.get();

    // Build init: { headers: {accept, cache-control, ...}, signal,
    //                credentials? }
    let init = v8::Object::new(scope);

    // headers — plain object so fetch's coercion accepts it as
    // HeadersInit. Per spec §9.2.5: send `Accept: text/event-stream`,
    // `Cache-Control: no-cache`, and `Last-Event-ID: <id>` if non-empty.
    let headers = v8::Object::new(scope);
    set_str_prop(scope, headers, "accept", "text/event-stream");
    set_str_prop(scope, headers, "cache-control", "no-cache");
    if !last_event_id.is_empty() {
        set_str_prop(scope, headers, "last-event-id", &last_event_id);
    }
    let key = v8::String::new(scope, "headers").unwrap();
    init.set(scope, key.into(), headers.into());

    // signal — read from the controller wrapper.
    let ctl_local = v8::Local::new(scope, &controller_g);
    let signal_key = v8::String::new(scope, "signal").unwrap();
    if let Some(sig_v) = ctl_local.get(scope, signal_key.into()) {
        let key = v8::String::new(scope, "signal").unwrap();
        init.set(scope, key.into(), sig_v);
    }

    // credentials — "include" if withCredentials, else "same-origin".
    let creds_str = if with_credentials { "include" } else { "same-origin" };
    set_str_prop(scope, init, "credentials", creds_str);

    // Look up globalThis.fetch.
    let global = scope.get_current_context().global(scope);
    let fetch_key = v8::String::new(scope, "fetch").unwrap();
    let fetch_v = match global.get(scope, fetch_key.into()) {
        Some(v) => v,
        None => return,
    };
    let fetch_fn = match v8::Local::<v8::Function>::try_from(fetch_v) {
        Ok(f) => f,
        Err(_) => return,
    };

    let url_v8 = v8::String::new(scope, &url_str).unwrap();
    let und: v8::Local<v8::Value> = v8::undefined(scope).into();

    let promise_global: v8::Global<v8::Promise> = {
        v8::tc_scope!(let tc, scope);
        let result = fetch_fn.call(tc, und, &[url_v8.into(), init.into()]);
        match result {
            Some(v) => match v8::Local::<v8::Promise>::try_from(v) {
                Ok(p) => v8::Global::new(tc, p),
                Err(_) => return,
            },
            None => {
                // fetch threw synchronously — swallow; the promise we
                // would have returned doesn't exist, so there's no
                // reaction to fire. The user observes an EventSource
                // stuck in CONNECTING (which the spec permits).
                let _ = tc.exception();
                return;
            }
        }
    };

    let captures: SharedCaptures = Rc::new(LoopCaptures {
        wrapper: v8::Global::new(scope, wrapper),
        _controller: controller_g,
        reader: RefCell::new(None),
    });

    let on_response = make_reaction(scope, captures.clone(), on_response_resolved);
    let on_response_err = make_reaction(scope, captures, on_response_rejected);
    let promise = v8::Local::new(scope, &promise_global);
    promise.then2(scope, on_response, on_response_err);
}

/// Fetch promise fulfilled — inspect Response, lock body, drive read loop.
fn on_response_resolved(
    scope: &mut v8::PinScope,
    captures: SharedCaptures,
    arg: v8::Local<v8::Value>,
) {
    let wrapper = v8::Local::new(scope, &captures.wrapper);
    let Some(state) = state_from_wrapper(scope, wrapper) else {
        return;
    };
    if state.closed_by_user.get() {
        return;
    }

    // arg is the Response object.
    let Ok(resp) = v8::Local::<v8::Object>::try_from(arg) else {
        on_io_error(scope, &captures, "fetch resolved with non-object");
        return;
    };

    // Per spec §9.2.5 step 3: HTTP 200 + Content-Type
    // `text/event-stream` (with optional ; charset=...) → OK to dispatch
    // `open` and start reading. Anything else → fire `error` and fail.
    let status_key = v8::String::new(scope, "status").unwrap();
    let status = resp
        .get(scope, status_key.into())
        .and_then(|v| v.uint32_value(scope))
        .unwrap_or(0);
    if status != 200 {
        on_io_error(scope, &captures, "non-200 response");
        return;
    }

    let headers_key = v8::String::new(scope, "headers").unwrap();
    let ct = read_headers_get(scope, resp, "content-type", headers_key);
    let ct_lower = ct.to_ascii_lowercase();
    let ct_main = ct_lower.split(';').next().unwrap_or("").trim();
    if ct_main != "text/event-stream" {
        on_io_error(scope, &captures, "bad content-type");
        return;
    }

    // Lock body via getReader().
    let body_key = v8::String::new(scope, "body").unwrap();
    let body_v = match resp.get(scope, body_key.into()) {
        Some(v) => v,
        None => {
            on_io_error(scope, &captures, "response.body missing");
            return;
        }
    };
    if body_v.is_null() || body_v.is_undefined() {
        on_io_error(scope, &captures, "response.body is null");
        return;
    }
    let Ok(body) = v8::Local::<v8::Object>::try_from(body_v) else {
        on_io_error(scope, &captures, "response.body is not an object");
        return;
    };
    let get_reader_key = v8::String::new(scope, "getReader").unwrap();
    let get_reader_v = match body.get(scope, get_reader_key.into()) {
        Some(v) => v,
        None => {
            on_io_error(scope, &captures, "body.getReader missing");
            return;
        }
    };
    let Ok(get_reader_fn) = v8::Local::<v8::Function>::try_from(get_reader_v) else {
        on_io_error(scope, &captures, "body.getReader not callable");
        return;
    };
    // Lock the body via getReader(). Use a tc_scope to absorb any
    // synchronous throw (e.g. stream already locked); on success, mint
    // the Global inside the tc_scope and let it survive the drop.
    let reader_global_opt: Option<v8::Global<v8::Object>> = {
        v8::tc_scope!(let tc, scope);
        let r = get_reader_fn.call(tc, body.into(), &[]);
        match r.and_then(|v| v8::Local::<v8::Object>::try_from(v).ok()) {
            Some(reader_local) => Some(v8::Global::new(tc, reader_local)),
            None => None,
        }
    };
    let Some(reader_global) = reader_global_opt else {
        on_io_error(scope, &captures, "getReader failed");
        return;
    };
    *captures.reader.borrow_mut() = Some(reader_global);

    // Transition CONNECTING → OPEN; fire `open`.
    state.ready_state.set(READY_OPEN);
    dispatch_plain_event(scope, wrapper, "open");

    // Drive first read.
    schedule_next_read(scope, captures);
}

/// Read `name` off `Headers` object via `headers.get(name)`. Returns
/// empty string if absent or not a string.
fn read_headers_get(
    scope: &mut v8::PinScope,
    response: v8::Local<v8::Object>,
    name: &str,
    headers_key: v8::Local<v8::String>,
) -> String {
    let headers_v = match response.get(scope, headers_key.into()) {
        Some(v) => v,
        None => return String::new(),
    };
    let Ok(headers) = v8::Local::<v8::Object>::try_from(headers_v) else {
        return String::new();
    };
    let get_key = v8::String::new(scope, "get").unwrap();
    let get_v = match headers.get(scope, get_key.into()) {
        Some(v) => v,
        None => return String::new(),
    };
    let Ok(get_fn) = v8::Local::<v8::Function>::try_from(get_v) else {
        return String::new();
    };
    let name_v8 = v8::String::new(scope, name).unwrap();
    let r = get_fn.call(scope, headers.into(), &[name_v8.into()]);
    match r {
        Some(v) if !v.is_null() && !v.is_undefined() => v.to_rust_string_lossy(scope),
        _ => String::new(),
    }
}

/// Fetch promise rejected — typically an AbortError (close() fired) or
/// network error. If user closed, do nothing; otherwise schedule a
/// reconnect.
fn on_response_rejected(
    scope: &mut v8::PinScope,
    captures: SharedCaptures,
    _reason: v8::Local<v8::Value>,
) {
    on_io_error(scope, &captures, "fetch rejected");
}

// ---------------------------------------------------------------------------
// Read loop
// ---------------------------------------------------------------------------

fn schedule_next_read(scope: &mut v8::PinScope, captures: SharedCaptures) {
    let wrapper = v8::Local::new(scope, &captures.wrapper);
    let Some(state) = state_from_wrapper(scope, wrapper) else {
        return;
    };
    if state.closed_by_user.get() {
        return;
    }

    let reader_g = match captures.reader.borrow().clone() {
        Some(r) => r,
        None => return,
    };
    let reader = v8::Local::new(scope, &reader_g);
    let read_key = v8::String::new(scope, "read").unwrap();
    let read_v = match reader.get(scope, read_key.into()) {
        Some(v) => v,
        None => {
            on_io_error(scope, &captures, "reader.read missing");
            return;
        }
    };
    let Ok(read_fn) = v8::Local::<v8::Function>::try_from(read_v) else {
        on_io_error(scope, &captures, "reader.read not callable");
        return;
    };

    let promise_opt: Option<v8::Global<v8::Promise>> = {
        v8::tc_scope!(let tc, scope);
        let r = read_fn.call(tc, reader.into(), &[]);
        match r.and_then(|v| v8::Local::<v8::Promise>::try_from(v).ok()) {
            Some(p) => Some(v8::Global::new(tc, p)),
            None => {
                let _ = tc.exception();
                None
            }
        }
    };
    let Some(promise_g) = promise_opt else {
        on_io_error(scope, &captures, "reader.read threw");
        return;
    };

    let on_chunk = make_reaction(scope, captures.clone(), on_chunk_resolved);
    let on_chunk_err = make_reaction(scope, captures, on_chunk_rejected);
    let promise = v8::Local::new(scope, &promise_g);
    promise.then2(scope, on_chunk, on_chunk_err);
}

fn on_chunk_resolved(
    scope: &mut v8::PinScope,
    captures: SharedCaptures,
    arg: v8::Local<v8::Value>,
) {
    let wrapper = v8::Local::new(scope, &captures.wrapper);
    let Some(state) = state_from_wrapper(scope, wrapper) else {
        return;
    };
    if state.closed_by_user.get() {
        return;
    }

    let Ok(result) = v8::Local::<v8::Object>::try_from(arg) else {
        on_io_error(scope, &captures, "read() resolved with non-object");
        return;
    };
    let done_key = v8::String::new(scope, "done").unwrap();
    let done = result
        .get(scope, done_key.into())
        .map(|v| v.boolean_value(scope))
        .unwrap_or(false);
    if done {
        // EOF — flush any final blank-line-dispatched event then
        // schedule reconnect (per spec §9.2.5, EOF triggers
        // reconnection unless `error` callback called close()).
        flush_final_event(scope, wrapper);
        on_io_error(scope, &captures, "stream closed");
        return;
    }

    let value_key = v8::String::new(scope, "value").unwrap();
    let value = match result.get(scope, value_key.into()) {
        Some(v) => v,
        None => {
            on_io_error(scope, &captures, "read() value missing");
            return;
        }
    };

    let bytes = match read_chunk_bytes(value) {
        Some(b) => b,
        None => {
            on_io_error(scope, &captures, "read() value is not a Uint8Array");
            return;
        }
    };

    feed_chunk(scope, wrapper, &bytes);

    // Continue reading.
    schedule_next_read(scope, captures);
}

fn on_chunk_rejected(
    scope: &mut v8::PinScope,
    captures: SharedCaptures,
    _reason: v8::Local<v8::Value>,
) {
    on_io_error(scope, &captures, "read() rejected");
}

fn read_chunk_bytes(value: v8::Local<v8::Value>) -> Option<Vec<u8>> {
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let mut out = vec![0u8; view.byte_length()];
        view.copy_contents(&mut out);
        return Some(out);
    }
    None
}

// ---------------------------------------------------------------------------
// SSE parser (HTML §9.2.6)
// ---------------------------------------------------------------------------

fn feed_chunk(scope: &mut v8::PinScope, wrapper: v8::Local<v8::Object>, bytes: &[u8]) {
    let Some(state) = state_from_wrapper(scope, wrapper) else {
        return;
    };

    // Take the parser out of the RefCell so we can call back into V8
    // for event dispatch without hitting a re-entrancy panic.
    let mut parser = std::mem::take(&mut *state.parser.borrow_mut());

    for &b in bytes {
        // CR LF normalisation: a CR preceded by anything starts a new
        // line; a following LF is elided.
        if parser.last_byte_was_cr && b == b'\n' {
            parser.last_byte_was_cr = false;
            continue;
        }
        parser.last_byte_was_cr = false;

        if b == b'\r' || b == b'\n' {
            if b == b'\r' {
                parser.last_byte_was_cr = true;
            }
            // End of line — interpret.
            let line = std::mem::take(&mut parser.line_buffer);
            if line.is_empty() {
                // Blank line → dispatch event.
                dispatch_event_from_parser(scope, wrapper, &mut parser);
            } else {
                process_line(&line, &mut parser, state);
            }
        } else {
            parser.line_buffer.push(b);
        }
    }

    *state.parser.borrow_mut() = parser;
}

/// Per §9.2.6 step 6: process one (non-empty) line.
fn process_line(line: &[u8], parser: &mut ParserState, state: &EventSourceState) {
    // Comment line: starts with `:`.
    if line.first() == Some(&b':') {
        return;
    }
    // Split on first `:`.
    let (field, value) = match line.iter().position(|&c| c == b':') {
        Some(i) => {
            let v_start = i + 1;
            // Strip a single leading space.
            let v = if line.get(v_start) == Some(&b' ') {
                &line[v_start + 1..]
            } else {
                &line[v_start..]
            };
            (&line[..i], v)
        }
        None => (line, &b""[..]),
    };

    // Best-effort lossy UTF-8 decode for value; fields are spec-byte-exact.
    let value_str = std::str::from_utf8(value).unwrap_or("");

    match field {
        b"event" => {
            parser.event_type = value_str.to_string();
        }
        b"data" => {
            if !parser.data_buffer.is_empty() {
                parser.data_buffer.push('\n');
            }
            parser.data_buffer.push_str(value_str);
        }
        b"id" => {
            // Per spec: only set if value contains no NUL.
            if !value.contains(&0) {
                parser.last_event_id = value_str.to_string();
                *state.last_event_id.borrow_mut() = value_str.to_string();
            }
        }
        b"retry" => {
            if let Ok(s) = std::str::from_utf8(value) {
                if let Ok(ms) = s.parse::<u32>() {
                    state.retry_ms.set(ms);
                }
            }
        }
        _ => {
            // Unknown fields are ignored per spec.
        }
    }
}

/// Per §9.2.6 step 8: dispatch the buffered event when a blank line is
/// seen.
fn dispatch_event_from_parser(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Object>,
    parser: &mut ParserState,
) {
    let data_owned = std::mem::take(&mut parser.data_buffer);
    let event_type_owned = std::mem::take(&mut parser.event_type);
    let last_id_owned = parser.last_event_id.clone();

    if data_owned.is_empty() {
        // Per spec: empty data buffer → discard accumulator, no
        // dispatch. (The implementation also resets `event_type` —
        // already done by mem::take.)
        return;
    }

    // Trim trailing single LF if the data ends with one (spec §9.2.6
    // step 8: "If the data buffer's last character is U+000A LINE
    // FEED, then remove the last character.").
    let mut data = data_owned;
    if data.ends_with('\n') {
        data.pop();
    }

    let event_type = if event_type_owned.is_empty() {
        "message".to_string()
    } else {
        event_type_owned
    };

    let data_v8 = v8::String::new(scope, &data).unwrap();
    let me = build_typed_message_event(scope, &event_type, data_v8.into(), &last_id_owned);
    crate::dom::event_target::dispatch_event(scope, wrapper, me);
}

/// Per §9.2.5 step 6: when EOF is reached, the spec implicitly leaves
/// any partial line in the buffer (no implicit blank-line). We do the
/// same here — the line buffer's bytes simply roll over to the next
/// connection.
fn flush_final_event(_scope: &mut v8::PinScope, _wrapper: v8::Local<v8::Object>) {
    // No-op for v1: EOF reconnect is handled by `on_io_error`.
}

/// Build a MessageEvent with a custom `type` and the given `data` /
/// `lastEventId` / origin (origin extracted from the URL).
fn build_typed_message_event<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    event_type: &str,
    data: v8::Local<v8::Value>,
    last_event_id: &str,
) -> v8::Local<'s, v8::Object> {
    let tmpl = MessageEventState::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .expect("MessageEvent allocation failed");

    let me = MessageEventState::default();
    *me.event.event_type.borrow_mut() = event_type.to_string();
    me.event.is_trusted.set(true);
    me.event.time_stamp.set(crate::dom::event::now_ms());
    *me.data.borrow_mut() = Some(v8::Global::new(scope, data));
    *me.last_event_id.borrow_mut() = last_event_id.to_string();

    let boxed: Box<MessageEventState> = Box::new(me);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    obj.set_prototype(scope, proto_v);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut MessageEventState));
        }),
    );
    std::mem::forget(weak);

    obj
}

fn dispatch_plain_event(scope: &mut v8::PinScope, wrapper: v8::Local<v8::Object>, ty: &str) {
    use crate::dom::event::{now_ms, Event};
    let tmpl = Event::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl.new_instance(scope).unwrap();

    let ev = Event::default();
    *ev.event_type.borrow_mut() = ty.to_string();
    ev.is_trusted.set(true);
    ev.time_stamp.set(now_ms());

    let boxed = Box::new(ev);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    obj.set_prototype(scope, proto_v);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Event));
        }),
    );
    std::mem::forget(weak);

    crate::dom::event_target::dispatch_event(scope, wrapper, obj);
}

// ---------------------------------------------------------------------------
// IO error / reconnect path
// ---------------------------------------------------------------------------

/// Common path for "stream broken; reconnect or close". Fires `error`
/// event, sets readyState=CONNECTING, schedules a setTimeout to retry.
/// If the user has closed, we just stay closed.
fn on_io_error(scope: &mut v8::PinScope, captures: &SharedCaptures, _reason: &str) {
    let wrapper = v8::Local::new(scope, &captures.wrapper);
    let Some(state) = state_from_wrapper(scope, wrapper) else {
        return;
    };
    if state.closed_by_user.get() {
        // close() has already set readyState=CLOSED and aborted; the
        // expected error reaction is just a no-op.
        return;
    }
    if state.ready_state.get() == READY_CLOSED {
        return;
    }

    // Move to CONNECTING (per spec: "reestablish the connection")
    // BEFORE firing the error so listeners see the right state.
    state.ready_state.set(READY_CONNECTING);
    dispatch_plain_event(scope, wrapper, "error");

    // If close() was called inside the error handler, don't reconnect.
    if state.closed_by_user.get() || state.ready_state.get() == READY_CLOSED {
        return;
    }

    schedule_reconnect(scope, captures.clone());
}

/// Schedule a reconnect via `globalThis.setTimeout(reconnect, retry_ms)`.
/// The reconnect callback re-arms the entire fetch loop with a fresh
/// AbortController.
fn schedule_reconnect(scope: &mut v8::PinScope, captures: SharedCaptures) {
    let wrapper = v8::Local::new(scope, &captures.wrapper);
    let Some(state) = state_from_wrapper(scope, wrapper) else {
        return;
    };
    let delay_ms = state.retry_ms.get();

    // Build a JS function that, when called, runs the reconnect.
    let reconnect_fn = build_reconnect_fn(scope, captures);

    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "setTimeout").unwrap();
    let st_v = match global.get(scope, key.into()) {
        Some(v) => v,
        None => return,
    };
    let Ok(st_fn) = v8::Local::<v8::Function>::try_from(st_v) else {
        return;
    };
    let und: v8::Local<v8::Value> = v8::undefined(scope).into();
    let delay_v = v8::Integer::new_from_unsigned(scope, delay_ms);
    let _ = st_fn.call(scope, und, &[reconnect_fn.into(), delay_v.into()]);
}

/// Build a 0-arg JS function whose call runs `reconnect_callback` with
/// the captured wrapper. The captures struct keeps the EventSource
/// wrapper and AbortController alive.
fn build_reconnect_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    captures: SharedCaptures,
) -> v8::Local<'s, v8::Function> {
    // Wrap the Rc in a Box so the External holds a stable raw pointer.
    let boxed = Box::new(captures);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    let tmpl = v8::FunctionTemplate::builder(reconnect_callback)
        .data(ext.into())
        .build(scope);
    let f = tmpl.get_function(scope).unwrap();
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        f,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut SharedCaptures));
        }),
    );
    std::mem::forget(weak);
    f
}

fn reconnect_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const SharedCaptures;
    if raw.is_null() {
        return;
    }
    let captures: &SharedCaptures = unsafe { &*raw };
    let wrapper = v8::Local::new(scope, &captures.wrapper);
    let Some(state) = state_from_wrapper(scope, wrapper) else {
        return;
    };
    if state.closed_by_user.get() {
        return;
    }

    // Mint a fresh AbortController for the new connection.
    let ctl = match mint_abort_controller(scope) {
        Some(c) => c,
        None => return,
    };
    *state.abort_controller.borrow_mut() = Some(ctl.clone());
    spawn_connect(scope, wrapper, ctl);
}

// ---------------------------------------------------------------------------
// Reaction-callback plumbing — generic factory
// ---------------------------------------------------------------------------

type ReactionFn = fn(&mut v8::PinScope, SharedCaptures, v8::Local<v8::Value>);

struct ReactionPayload {
    captures: SharedCaptures,
    f: ReactionFn,
}

fn make_reaction<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    captures: SharedCaptures,
    f: ReactionFn,
) -> v8::Local<'s, v8::Function> {
    let boxed = Box::new(ReactionPayload { captures, f });
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    let tmpl = v8::FunctionTemplate::builder(reaction_callback)
        .data(ext.into())
        .build(scope);
    let func = tmpl.get_function(scope).unwrap();
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        func,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut ReactionPayload));
        }),
    );
    std::mem::forget(weak);
    func
}

fn reaction_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const ReactionPayload;
    if raw.is_null() {
        return;
    }
    let payload: &ReactionPayload = unsafe { &*raw };
    let arg = args.get(0);
    (payload.f)(scope, payload.captures.clone(), arg);
}

// ---------------------------------------------------------------------------
// Misc helpers
// ---------------------------------------------------------------------------

fn set_str_prop(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
    value: &str,
) {
    let k = v8::String::new(scope, key).unwrap();
    let v = v8::String::new(scope, value).unwrap();
    obj.set(scope, k.into(), v.into());
}

// ---------------------------------------------------------------------------
// install_global
// ---------------------------------------------------------------------------

pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let tmpl = EventSourceState::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();

    // Per spec, `EventSource.{CONNECTING,OPEN,CLOSED}` constants live
    // on BOTH the constructor and the prototype.
    install_constant(scope, class_fn.into(), "CONNECTING", READY_CONNECTING);
    install_constant(scope, class_fn.into(), "OPEN", READY_OPEN);
    install_constant(scope, class_fn.into(), "CLOSED", READY_CLOSED);
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    if let Some(proto_v) = class_fn.get(scope, proto_key.into()) {
        if let Ok(proto) = v8::Local::<v8::Object>::try_from(proto_v) {
            install_constant(scope, proto, "CONNECTING", READY_CONNECTING);
            install_constant(scope, proto, "OPEN", READY_OPEN);
            install_constant(scope, proto, "CLOSED", READY_CLOSED);
        }
    }

    let key = v8::String::new(scope, "EventSource").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

fn install_constant(
    scope: &mut v8::PinScope,
    target: v8::Local<v8::Object>,
    name: &str,
    value: u32,
) {
    let key = v8::String::new(scope, name).unwrap();
    let val = v8::Integer::new_from_unsigned(scope, value);
    target.set(scope, key.into(), val.into());
}
