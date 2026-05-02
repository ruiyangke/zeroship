//! Native WebSocket per WHATWG WebSockets §3.1 + RFC 6455.
//!
//! This module ships in three landings (D-25):
//!   1. behind a feature flag, polyfill default — current state.
//!   2. flip default to native (cutover landing 2).
//!   3. delete polyfill (cutover landing 3).
//!
//! ## Layout
//!
//! - `mod.rs` — class skeleton + IDL surface; this file.
//! - `algorithms.rs` — spec-named algorithms (validate_close_code_and_reason,
//!   clamp_unsigned_short, fail_the_websocket_connection, …).
//! - `handshake.rs` — RFC 6455 §4.1 client-side handshake (step 4).
//! - `receive_loop.rs` / `send_pump.rs` — frame phase (step 5).
//! - `pair.rs` — WebSocketPair (workerd extension, step 6).
//! - `slots.rs` / `budget.rs` / `constants.rs` — supporting infra.

pub mod algorithms;
pub mod constants;

#[cfg(test)]
mod tests;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_inherit, v8_method, v8_name, v8_setter,
};

use crate::state::OpError;

// ---------------------------------------------------------------------------
// Public types — ReadyState, BinaryType, WsFrame, WsMessage
// ---------------------------------------------------------------------------

/// Spec [[readyState]] — WHATWG §3.1.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u16)]
pub enum ReadyState {
    Connecting = 0,
    Open = 1,
    Closing = 2,
    Closed = 3,
}

impl Default for ReadyState {
    fn default() -> Self {
        ReadyState::Connecting
    }
}

/// `BinaryType` enum per WHATWG §3.1. Default is "blob" per spec
/// (NOT "arraybuffer" — the polyfill defaulted wrong; see D-7).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum BinaryType {
    Blob,
    ArrayBuffer,
}

impl Default for BinaryType {
    fn default() -> Self {
        BinaryType::Blob
    }
}

/// Send queue entry — one entry per `send()` call until the send pump
/// drains it. Per critic MAJOR #6 + #19 (Blob deferral / variant
/// inventory).
pub enum WsFrame {
    /// USVString-converted text frame.
    Text(String),
    /// Binary frame (ArrayBuffer or ArrayBufferView).
    Binary(Vec<u8>),
    /// Blob — bytes resolved async by the send pump. The cached `size`
    /// is what `bufferedAmount` was incremented by; the pump decrements
    /// by the same number after a successful write so the counter
    /// observably tracks the byte queue.
    Blob {
        handle: v8::Global<v8::Object>,
        size: u64,
    },
    /// Close frame. `code: None` means "send Close with empty payload"
    /// per RFC 6455 §5.5.1 — code 1005 is RESERVED as an internal
    /// sentinel and MUST NOT appear on the wire (RFC 6455 §7.4.1,
    /// addresses critic CRITICAL #6).
    Close {
        code: Option<u16>,
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Cached V8 handles
// ---------------------------------------------------------------------------

/// Cached V8 handles — resolved once on first event dispatch, reused
/// for every subsequent dispatch. Mirrors the polyfill's per-WS cache
/// in `state::WsCachedHandles` but keyed against the native class
/// wrapper instead of `__wsRegistry[ws_id]`.
#[derive(Default)]
pub struct WsCachedHandles {
    /// The WebSocket wrapper Global — used as the `this` arg in
    /// `dispatchEvent`. Set lazily on first dispatch.
    pub ws_obj: Option<v8::Global<v8::Object>>,
    /// EventHandler IDL attribute slots. Stored separately from
    /// `addEventListener`-installed listeners; the setter installs an
    /// internal listener that delegates to the stored function. v1
    /// stores them here for synchronous null-coercion (HTML §8.1.5.1
    /// step 4 — addresses critic MAJOR #15).
    pub on_open: Option<v8::Global<v8::Function>>,
    pub on_message: Option<v8::Global<v8::Function>>,
    pub on_error: Option<v8::Global<v8::Function>>,
    pub on_close: Option<v8::Global<v8::Function>>,
}

impl std::fmt::Debug for WsCachedHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsCachedHandles").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// WebSocketImpl — the boxed Rust state behind the V8 wrapper
// ---------------------------------------------------------------------------

/// Spec slot inventory per §II.1 + §X.1. Single-source rule: each
/// observable slot lives in EXACTLY ONE place — here in the boxed
/// state, NOT mirrored in V8 private symbols.
#[repr(C)]
pub struct WebSocketImpl {
    /// Inherited EventTarget state — empty marker per the convention
    /// in `dom::abort_signal::AbortSignal` (the listener Rc actually
    /// hangs off the JS wrapper as a private symbol via
    /// `dom::event_target::attach_listeners`, but the `#[repr(C)]` +
    /// first-field is required for the macro's inheritance cast).
    pub event_target: crate::dom::event_target::EventTarget,

    /// Spec `[[readyState]]` — WHATWG §3.1. Initially CONNECTING.
    pub ready_state: Cell<ReadyState>,

    /// Spec `[[url]]` — the parsed URL record. `RefCell` so the
    /// network code (in step 4+) can read host/port/path. v1 stores
    /// `Option` because pair-coupled sockets have no URL.
    pub url: RefCell<Option<url::Url>>,

    /// `socket.url` getter return value. Computed once at construction.
    pub url_serialized: RefCell<String>,

    /// Spec `[[protocol]]` — the negotiated subprotocol. Empty until
    /// the handshake completes.
    pub protocol: RefCell<String>,

    /// Spec `[[extensions]]` — the negotiated extensions header.
    /// Empty until the handshake completes (and v1 always stays empty
    /// because we offer no extensions per critic CRITICAL #7).
    pub extensions: RefCell<String>,

    /// Spec `[[bufferedAmount]]` — bytes queued for send. Bumped by
    /// `send()`, decremented by the send pump as frames go over the
    /// wire. (D-6)
    pub buffered_amount: Cell<u64>,

    /// Spec `[[binaryType]]` — "blob" (default per §3.1) or "arraybuffer".
    pub binary_type: Cell<BinaryType>,

    /// Internal `[[full]]` flag per RFC 6455 §6.1. When set, every
    /// subsequent `send()` returns early — bytes are dropped on the
    /// floor; the JS observable is `bufferedAmount` stuck at the
    /// high-water mark. Cleared by the pump when the queue drops
    /// below 50% of the cap (8 MiB hysteresis). NEVER observable
    /// from JS. (addresses critic MAJOR #11)
    pub full: Cell<bool>,

    /// Per-isolate WebSocket id — used by:
    ///   - the runtime pump's `OpResult::WebSocketEvent { ws_id, ... }`
    ///     dispatch (added in step 5);
    ///   - the WebSocketPair peer-link map (step 6);
    ///   - the gateway's existing 101-response-extraction path
    ///     (`crates/runtime/src/http.rs:171-179`).
    pub ws_id: Cell<u32>,

    /// Cached V8 handles for fast event dispatch. Lazy.
    pub cached_handles: RefCell<WsCachedHandles>,

    /// Outgoing send queue. Each entry is a fully-encoded frame; the
    /// send pump drains in order.
    pub send_queue: RefCell<VecDeque<WsFrame>>,

    /// WebSocketPair peer (workerd extension, step 6). When `Some(other_id)`,
    /// every `send()` ALSO enqueues into the peer's incoming queue.
    pub peer_id: Cell<Option<u32>>,

    /// `accepted` flag (workerd extension) — true after `accept()`.
    /// Required for WebSocketPair[1] before message delivery starts.
    /// Pre-set to true for client-side `new WebSocket(url)` because
    /// the user never calls accept() on a client socket (and accept()
    /// throws TypeError on a client socket per D-21).
    pub accepted: Cell<bool>,

    /// Optional explicit Origin header per RFC 6455 §10.2 / D-28. Default
    /// None = no Origin on the wire. Only populated when the
    /// constructor was given a `WebSocketInit.origin` member.
    /// (addresses critic CRITICAL #3)
    pub explicit_origin: RefCell<Option<String>>,

    /// `WebSocketInit.maxMessageSize` per D-28 — passed into
    /// tungstenite's `WebSocketConfig` at handshake time.
    pub max_message_size: Cell<u32>,
    /// `WebSocketInit.maxFrameSize` per D-28.
    pub max_frame_size: Cell<u32>,
    /// `WebSocketInit.pingIntervalMs` per D-28 — drives the per-WS
    /// keepalive timer in step 5.
    pub ping_interval_ms: Cell<u32>,

    /// Send-pump notification flag + waker (mirrors the polyfill's
    /// `outgoing_ready` / `pump_waker` plumbing).
    pub send_ready: Rc<Cell<bool>>,
    pub send_waker: Rc<RefCell<Option<std::task::Waker>>>,
}

impl Default for WebSocketImpl {
    fn default() -> Self {
        WebSocketImpl {
            event_target: crate::dom::event_target::EventTarget,
            ready_state: Cell::new(ReadyState::Connecting),
            url: RefCell::new(None),
            url_serialized: RefCell::new(String::new()),
            protocol: RefCell::new(String::new()),
            extensions: RefCell::new(String::new()),
            buffered_amount: Cell::new(0),
            binary_type: Cell::new(BinaryType::Blob),
            full: Cell::new(false),
            ws_id: Cell::new(0),
            cached_handles: RefCell::new(WsCachedHandles::default()),
            send_queue: RefCell::new(VecDeque::new()),
            peer_id: Cell::new(None),
            accepted: Cell::new(false),
            explicit_origin: RefCell::new(None),
            max_message_size: Cell::new(constants::DEFAULT_MAX_MESSAGE_SIZE),
            max_frame_size: Cell::new(constants::DEFAULT_MAX_FRAME_SIZE),
            ping_interval_ms: Cell::new(constants::DEFAULT_PING_INTERVAL_MS),
            send_ready: Rc::new(Cell::new(false)),
            send_waker: Rc::new(RefCell::new(None)),
        }
    }
}

// ---------------------------------------------------------------------------
// IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[v8_inherit(crate::dom::event_target::EventTarget)]
#[v8_to_string_tag = "WebSocket"]
impl WebSocketImpl {
    /// `new WebSocket(url, protocols?, init?)` — WHATWG §3.1 constructor.
    ///
    /// Steps 1-13 of the constructor:
    ///   1-3. Parse URL (string → URL record); throw SyntaxError on failure.
    ///   4-5. Scheme normalisation (`http` → `ws`, `https` → `wss`).
    ///   6.   Scheme MUST be `ws` or `wss`.
    ///   7.   Fragment MUST be null.
    ///   8-9. Validate protocols (RFC 7230 token rule, no duplicates).
    ///   10.  Set this's url to urlRecord.
    ///   12.  Run the establish-a-WebSocket-connection algorithm IN PARALLEL.
    ///   13.  Constructor returns this synchronously; readyState=CONNECTING.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        url_arg: v8::Local<v8::Value>,
        protocols_arg: v8::Local<v8::Value>,
        init_arg: v8::Local<v8::Value>,
    ) -> Result<WebSocketImpl, OpError> {
        // STEP per IDL: `url` is required. A no-args call must
        // TypeError per WebIDL. (Server-side mode via `WebSocketPair`
        // uses the hidden `mint_paired_websocket` helper which bypasses
        // this constructor — see step 6.)
        if url_arg.is_undefined() {
            return Err(OpError::type_error(
                "WebSocket constructor: 'url' is required",
            ));
        }

        // STEP 1-3: URL parsing.
        let url_input = match url_arg.to_string(scope) {
            Some(s) => s.to_rust_string_lossy(scope),
            None => return Ok(WebSocketImpl::default()),
        };
        let mut url_record = match url::Url::parse(&url_input) {
            Ok(u) => u,
            Err(e) => {
                return Err(OpError::type_error(&format!(
                    "WebSocket: invalid URL: {e}"
                )))
            }
        };

        // STEP 4-5: scheme normalisation.
        match url_record.scheme() {
            "http" => {
                url_record
                    .set_scheme("ws")
                    .map_err(|_| OpError::type_error("WebSocket: scheme normalisation failed"))?;
            }
            "https" => {
                url_record
                    .set_scheme("wss")
                    .map_err(|_| OpError::type_error("WebSocket: scheme normalisation failed"))?;
            }
            "ws" | "wss" => {}
            scheme => {
                return Err(OpError::type_error(&format!(
                    "WebSocket: scheme must be 'ws' or 'wss', got '{scheme}'"
                )))
            }
        }

        // STEP 6: re-check post-normalisation (defence-in-depth — set_scheme
        // can theoretically reject).
        let normalised_scheme = url_record.scheme();
        if normalised_scheme != "ws" && normalised_scheme != "wss" {
            return Err(OpError::type_error(
                "WebSocket: scheme must be 'ws' or 'wss'",
            ));
        }

        // STEP 7: fragment MUST be null.
        // ada-url's `Url::fragment` returns `Some("")` only when the
        // input had a `#` with empty fragment. The spec treats both
        // `#` and `#xyz` as "fragment is non-null" — we reject both.
        if url_record.fragment().is_some() || url_input.contains('#') {
            return Err(OpError::type_error(
                "WebSocket: URL must not contain a fragment",
            ));
        }

        // STEP 8-9: protocol validation.
        let protocols = algorithms::parse_and_validate_protocols(scope, protocols_arg)?;

        // STEP 13: ready state CONNECTING.
        let impl_ = WebSocketImpl::default();
        *impl_.url_serialized.borrow_mut() = url_record.as_str().to_string();
        *impl_.url.borrow_mut() = Some(url_record);
        impl_.ready_state.set(ReadyState::Connecting);
        impl_.binary_type.set(BinaryType::Blob); // D-7 spec default
        impl_.accepted.set(true); // client-mode: implicitly accepted

        // Read the optional WebSocketInit dictionary (D-28).
        let init = algorithms::read_websocket_init(scope, init_arg)?;
        *impl_.explicit_origin.borrow_mut() = init.origin;
        impl_.max_message_size.set(init.max_message_size);
        impl_.max_frame_size.set(init.max_frame_size);
        impl_.ping_interval_ms.set(init.ping_interval_ms);

        // The connect task is wired in step 4 (handshake.rs). For the
        // step-2 skeleton we leave the state in CONNECTING — which
        // means `send` throws InvalidStateError per spec, and `close()`
        // takes the CONNECTING branch.
        //
        // `protocols` is captured here for the eventual handshake step;
        // we shadow into _ to silence unused-variable until step 4.
        let _ = protocols;

        Ok(impl_)
    }

    // ----------------------------------------------------------------- getters

    /// `socket.url` — USVString. Returns the serialised URL.
    #[v8_getter]
    fn url(&self) -> String {
        self.url_serialized.borrow().clone()
    }

    /// `socket.readyState` — unsigned short.
    #[v8_getter]
    #[v8_name = "readyState"]
    fn ready_state(&self) -> u32 {
        self.ready_state.get() as u32
    }

    /// `socket.bufferedAmount` — unsigned long long.
    #[v8_getter]
    #[v8_name = "bufferedAmount"]
    fn buffered_amount(&self) -> f64 {
        // f64 because IDL is `unsigned long long` and JS Number is f64;
        // values up to 2^53 are exact (more than enough for our 16 MiB cap).
        self.buffered_amount.get() as f64
    }

    /// `socket.protocol` — DOMString.
    #[v8_getter]
    fn protocol(&self) -> String {
        self.protocol.borrow().clone()
    }

    /// `socket.extensions` — DOMString.
    #[v8_getter]
    fn extensions(&self) -> String {
        self.extensions.borrow().clone()
    }

    /// `socket.binaryType` — `"blob" | "arraybuffer"`.
    #[v8_getter]
    #[v8_name = "binaryType"]
    fn get_binary_type(&self) -> String {
        match self.binary_type.get() {
            BinaryType::Blob => "blob".into(),
            BinaryType::ArrayBuffer => "arraybuffer".into(),
        }
    }

    /// `socket.binaryType = "blob" | "arraybuffer"` — silent no-op on
    /// unknown values per D-7 / WPT `binaryType-wrong-value.any.js`.
    /// undici and workerd both ship the silent-no-op behaviour; the
    /// strict-throw path is gated behind a non-default Cargo feature
    /// for WPT-update tracking only. (addresses critic MAJOR #16)
    #[v8_setter]
    #[v8_name = "binaryType"]
    fn set_binary_type(
        &self,
        scope: &mut v8::PinScope,
        v: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        let s = v.to_rust_string_lossy(scope);
        match s.as_str() {
            "blob" => self.binary_type.set(BinaryType::Blob),
            "arraybuffer" => self.binary_type.set(BinaryType::ArrayBuffer),
            _ => {
                // Silent no-op; current value retained. Matches every
                // shipping impl and the WPT expected-pass.
            }
        }
        Ok(())
    }

    // -------------------------------------------------------- EventHandler IDL
    //
    // Per HTML §8.1.5.1 step 4: a non-callable assignment coerces to
    // null (NOT TypeError). We store the raw user function (or None)
    // and install/remove an internal listener via the same EventTarget
    // listener path that `addEventListener` uses. (addresses critic
    // MAJOR #15)
    //
    // The setter installs the handler via the EventTarget listener
    // list so dispatchEvent finds it; the getter returns the stored
    // function (or null). undici (`websocket.js:355-445`) is the
    // reference implementation.

    #[v8_setter]
    #[v8_name = "onopen"]
    fn set_onopen(
        &self,
        scope: &mut v8::PinScope,
        v: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        algorithms::set_event_handler(scope, self, "open", v, |h| &mut h.on_open)
    }
    #[v8_getter]
    #[v8_name = "onopen"]
    fn get_onopen<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        algorithms::get_event_handler(scope, self, |h| h.on_open.as_ref())
    }

    #[v8_setter]
    #[v8_name = "onmessage"]
    fn set_onmessage(
        &self,
        scope: &mut v8::PinScope,
        v: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        algorithms::set_event_handler(scope, self, "message", v, |h| &mut h.on_message)
    }
    #[v8_getter]
    #[v8_name = "onmessage"]
    fn get_onmessage<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        algorithms::get_event_handler(scope, self, |h| h.on_message.as_ref())
    }

    #[v8_setter]
    #[v8_name = "onerror"]
    fn set_onerror(
        &self,
        scope: &mut v8::PinScope,
        v: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        algorithms::set_event_handler(scope, self, "error", v, |h| &mut h.on_error)
    }
    #[v8_getter]
    #[v8_name = "onerror"]
    fn get_onerror<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        algorithms::get_event_handler(scope, self, |h| h.on_error.as_ref())
    }

    #[v8_setter]
    #[v8_name = "onclose"]
    fn set_onclose(
        &self,
        scope: &mut v8::PinScope,
        v: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        algorithms::set_event_handler(scope, self, "close", v, |h| &mut h.on_close)
    }
    #[v8_getter]
    #[v8_name = "onclose"]
    fn get_onclose<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        algorithms::get_event_handler(scope, self, |h| h.on_close.as_ref())
    }

    // ----------------------------------------------------------------- methods

    /// `socket.send(data)` — WHATWG §3.1.
    ///
    /// Step 1: throw InvalidStateError if CONNECTING (per the live
    /// spec; D-6 references the spec amendment that made this throw).
    /// Step 2: silent no-op for CLOSING / CLOSED.
    /// Step 3-6: type-dispatch in spec order — String → Blob → ArrayBuffer
    /// → ArrayBufferView. (addresses critic CRITICAL #1)
    ///
    /// In the step-2 skeleton, OPEN is never reached — the connect
    /// task is stubbed. Tests that need OPEN behaviour drive it via
    /// the WebSocketPair coupling (step 6) or the receive_loop (step 5).
    #[v8_method]
    fn send(
        &self,
        scope: &mut v8::PinScope,
        data: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        // Spec step 1: CONNECTING → InvalidStateError.
        if self.ready_state.get() == ReadyState::Connecting {
            return Err(OpError::error("InvalidStateError: WebSocket is still CONNECTING"));
        }

        // Spec step 2: CLOSING / CLOSED → silent no-op (matches the
        // polyfill behaviour and undici).
        if self.ready_state.get() != ReadyState::Open {
            return Ok(());
        }

        // [[full]] flag check per RFC 6455 §6.1 + D-6 / critic MAJOR #11.
        if self.full.get() {
            return Ok(());
        }

        // Type dispatch per WHATWG §3.1 send algorithm steps 3-6
        // (https://websockets.spec.whatwg.org/#dom-websocket-send):
        //   step 3: data is a string
        //   step 4: data is a Blob object
        //   step 5: data is an ArrayBuffer object
        //   step 6: data is an ArrayBufferView object
        //
        // Type-test by branding (V8 internal slots / IsBlob), NOT by
        // toString — the v1 design's "Blob with custom toString hazard"
        // was a non-issue. (addresses critic CRITICAL #1)
        if data.is_string() {
            let s_v8 = data.to_string(scope).ok_or_else(|| {
                OpError::error("WebSocket.send: string conversion failed")
            })?;
            // USVString conversion replaces lone surrogates with U+FFFD
            // per https://webidl.spec.whatwg.org/#es-USVString.
            let s = s_v8.to_rust_string_lossy(scope);
            self.queue_text(s);
            return Ok(());
        }

        if let Ok(blob_obj) = v8::Local::<v8::Object>::try_from(data) {
            if crate::blob_native::blob::is_blob_instance_public(scope, blob_obj) {
                // Per spec step 4: bufferedAmount jumps by Blob.size synchronously,
                // bytes are extracted asynchronously by the send pump.
                let blob_size = crate::blob_native::blob::blob_size_public(scope, blob_obj);
                self.queue_blob(scope, blob_obj, blob_size);
                return Ok(());
            }
        }

        if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(data) {
            let bs = ab.get_backing_store();
            // SAFETY: backing store iter yields Cell<u8>; `.get()` is
            // a copy. Single-threaded isolate → no race.
            let bytes: Vec<u8> = bs.iter().map(|c| c.get()).collect();
            self.queue_binary(bytes);
            return Ok(());
        }

        if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(data) {
            let mut buf = vec![0u8; view.byte_length()];
            let _copied = view.copy_contents(&mut buf);
            self.queue_binary(buf);
            return Ok(());
        }

        // Fallthrough: per WHATWG §3.1 the WebIDL union conversion
        // coerces unknown types to USVString (last branch).
        let s_v8 = data.to_string(scope).ok_or_else(|| {
            OpError::error("WebSocket.send: data could not be coerced")
        })?;
        let s = s_v8.to_rust_string_lossy(scope);
        self.queue_text(s);
        Ok(())
    }

    /// `socket.close(code?, reason?)` — WHATWG §3.1.
    ///
    /// Step 1 (validate code): present and not 1000 nor 3000-4999 →
    /// InvalidAccessError. (D-8)
    /// Step 2 (validate reason): UTF-8 encoded length > 123 bytes →
    /// SyntaxError.
    /// Step 3 (state transition): CLOSING/CLOSED → no-op; CONNECTING →
    /// fail-the-WebSocket-connection (no wire frame, no socket may
    /// even exist); OPEN → start closing handshake.
    ///
    /// The `code` argument is `[Clamp] unsigned short` — runs through
    /// `clamp_unsigned_short` which implements WebIDL ConvertToInt's
    /// `[Clamp]` case: NaN → 0, then sign-aware clamp to [0, 65535],
    /// then round-half-to-even for the .5 tie case. (addresses critic
    /// CRITICAL #2)
    #[v8_method]
    fn close(
        &self,
        scope: &mut v8::PinScope,
        code: v8::Local<v8::Value>,
        reason: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        // [Clamp] conversion. `undefined` means "no code arg" — passed
        // through as `Option::None` so the wire frame is empty
        // (CRITICAL #6: NEVER serialise 1005).
        let code_opt: Option<u16> = if code.is_undefined() {
            None
        } else {
            Some(algorithms::clamp_unsigned_short(scope, code))
        };

        let reason_opt: Option<String> = if reason.is_undefined() {
            None
        } else {
            Some(reason.to_rust_string_lossy(scope))
        };

        // Validate before state transition (per spec close algorithm,
        // step 1-2 run BEFORE step 3 readyState dispatch).
        algorithms::validate_close_code_and_reason(code_opt, reason_opt.as_deref())?;

        use ReadyState::*;
        match self.ready_state.get() {
            Closing | Closed => return Ok(()),
            Connecting => {
                // Per RFC 6455 §7.1.7 (Fail the WebSocket Connection):
                // there may or may not be a TCP socket open at this
                // point. The send_pump (step 5) doesn't run during
                // CONNECTING; transitioning to CLOSING here is enough
                // to trip the connect task's cancel flag once that
                // wiring lands. For the step-2 skeleton, just flip
                // state and notify the pump (no-op until step 5).
                self.ready_state.set(Closing);
                // No wire frame is enqueued; the connection-failed
                // path emits Close{1006} via the pump cancellation.
            }
            Open => {
                // Step 3.3-3.4: start closing handshake; readyState=CLOSING.
                self.ready_state.set(Closing);
                let frame = WsFrame::Close {
                    code: code_opt, // Option<u16> — None = empty payload
                    reason: reason_opt.unwrap_or_default(),
                };
                self.send_queue.borrow_mut().push_back(frame);
                self.notify_send_pump();
            }
        }

        Ok(())
    }

    /// `socket.accept()` — workerd extension (D-21). Required by
    /// `WebSocketPair[1]` to begin local message delivery; throws
    /// TypeError on a client-side socket.
    #[v8_method]
    fn accept(&self) -> Result<(), OpError> {
        if self.peer_id.get().is_none() {
            return Err(OpError::type_error(
                "WebSocket.accept: cannot accept() a client-side WebSocket",
            ));
        }
        if self.accepted.get() {
            return Ok(()); // idempotent
        }
        self.accepted.set(true);
        Ok(())
    }
}

impl WebSocketImpl {
    fn queue_text(&self, s: String) {
        let bytes_len = s.len() as u64;
        self.send_queue.borrow_mut().push_back(WsFrame::Text(s));
        self.buffered_amount
            .set(self.buffered_amount.get().saturating_add(bytes_len));
        self.notify_send_pump();
    }

    fn queue_binary(&self, b: Vec<u8>) {
        let bytes_len = b.len() as u64;
        self.send_queue
            .borrow_mut()
            .push_back(WsFrame::Binary(b));
        self.buffered_amount
            .set(self.buffered_amount.get().saturating_add(bytes_len));
        self.notify_send_pump();
    }

    fn queue_blob(&self, scope: &mut v8::PinScope, blob_obj: v8::Local<v8::Object>, size: u64) {
        let handle = v8::Global::new(scope, blob_obj);
        self.send_queue
            .borrow_mut()
            .push_back(WsFrame::Blob { handle, size });
        self.buffered_amount
            .set(self.buffered_amount.get().saturating_add(size));
        self.notify_send_pump();
    }

    fn notify_send_pump(&self) {
        self.send_ready.set(true);
        if let Some(waker) = self.send_waker.borrow_mut().take() {
            waker.wake();
        }
    }
}

// ---------------------------------------------------------------------------
// Constants installer — Event uses a similar pattern (event.rs).
// ---------------------------------------------------------------------------

/// Install `WebSocket.{CONNECTING,OPEN,CLOSING,CLOSED}` as integer-valued
/// data properties on the constructor function and the prototype, per
/// WebIDL §3.7.5 "constants on interfaces".
pub fn install_websocket_constants<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ctor_fn: v8::Local<v8::Function>,
) {
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = ctor_fn.get(scope, proto_key.into()).unwrap();
    let proto: v8::Local<v8::Object> = proto_v.try_into().unwrap();

    for (name, val) in [
        ("CONNECTING", ReadyState::Connecting as u32),
        ("OPEN", ReadyState::Open as u32),
        ("CLOSING", ReadyState::Closing as u32),
        ("CLOSED", ReadyState::Closed as u32),
    ] {
        let key = v8::String::new(scope, name).unwrap();
        let v = v8::Integer::new_from_unsigned(scope, val);
        ctor_fn.set(scope, key.into(), v.into());
        proto.set(scope, key.into(), v.into());
    }
}

// ---------------------------------------------------------------------------
// Global install — wire up `globalThis.WebSocket` (NEW native class).
// Called by init.rs ONLY when the native feature flag is on (cutover
// landing 2 flips the default).
// ---------------------------------------------------------------------------

/// Install `globalThis.WebSocket` as the native class. Matches the
/// `dom::install_class` pattern used for Event / CustomEvent / etc.
pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let tmpl = WebSocketImpl::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    install_websocket_constants(scope, class_fn);
    let key = v8::String::new(scope, "WebSocket").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

// ---------------------------------------------------------------------------
// Helper: read the WebSocketImpl boxed state from a JS wrapper.
// Used by http.rs::inspect_response to ferry the `ws_id` of a 101-upgrade
// Response's `webSocket` slot to the gateway.
// ---------------------------------------------------------------------------

/// Get the `WebSocketImpl` boxed state from a V8 wrapper. Returns
/// `None` if the object isn't a WebSocket (no internal field, or the
/// field isn't an External, or the pointer is null).
///
/// SAFETY: caller must ensure `obj` is a WebSocket JS wrapper produced
/// by the native class. The boxed state's lifetime is tied to the
/// wrapper via the macro's guaranteed finalizer.
pub fn websocket_from_obj<'a>(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Option<&'a WebSocketImpl> {
    let ext = obj
        .get_internal_field(scope, 0)
        .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())?;
    let ptr = ext.value() as *mut WebSocketImpl;
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { &*ptr })
}

/// Extract `ws_id` from a native WebSocket wrapper, or 0 if not a
/// native WebSocket. Used by `http.rs::inspect_response` for the
/// `Response { status: 101, webSocket: client }` path.
///
/// Per §X.1: ws_id lives on the boxed state, NOT mirrored in V8
/// private symbols.
pub fn ws_id_of(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> u32 {
    websocket_from_obj(scope, obj)
        .map(|w| w.ws_id.get())
        .unwrap_or(0)
}
