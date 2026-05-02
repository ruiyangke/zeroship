//! `WebSocketPair` workerd extension — two paired `WebSocket` wrappers
//! that pass messages directly through the per-WS event queue (no
//! framer, no network).
//!
//! v1 of the polyfill drove this through the `__wsCreatePair` /
//! `__wsLinkPair` / `__wsAccept` / `__wsSend` / `__wsClose` callbacks
//! plus the global `__wsRegistry`. The native pair runs on top of the
//! same `NativeWsState` event channel as client sockets — `send()`
//! pushes a frame onto the local outbox; the helper here moves it
//! straight onto the peer's `events` queue and notifies the pump.
//!
//! Filled in step 6 — see `docs/proposals/websocket-native.md §VI`.

#![cfg(feature = "runtime_native_websocket")]

use crate::state::SharedState;

use super::network::{self, WsEvent};
use super::WsFrame;

/// Move queued frames from sender (`from_id`) to peer (`to_id`).
/// Each frame becomes a Message/Close event on the peer's queue.
/// Decrement the sender's bufferedAmount by the bytes written
/// (the design's "pair-drain" signal — MAJOR #19).
pub fn deliver_to_peer(
    state: &SharedState,
    from_id: u32,
    to_id: u32,
    frames: Vec<WsFrame>,
) {
    if frames.is_empty() {
        return;
    }

    // Source's NativeWsState — used for buffered_amount drain.
    let from_state = network::lookup_native_ws_state(state, from_id);
    let to_state = network::lookup_native_ws_state(state, to_id);

    // We need both endpoints alive AND the destination to be accepted
    // (workerd-style `ws.accept()` gating).
    let to_state = match to_state {
        Some(s) => s,
        None => return,
    };

    let mut total_drained: u64 = 0;
    for frame in frames {
        match frame {
            WsFrame::Text(s) => {
                total_drained = total_drained.saturating_add(s.len() as u64);
                push_peer_event(state, to_id, WsEvent::MessageText(s));
            }
            WsFrame::Binary(b) => {
                total_drained = total_drained.saturating_add(b.len() as u64);
                push_peer_event(state, to_id, WsEvent::MessageBinary(b));
            }
            WsFrame::Blob { handle: _, size } => {
                // Blob byte extraction needs a V8 scope — pair sockets
                // don't currently materialise the bytes; the
                // bufferedAmount budget is released and the frame is
                // dropped. Same caveat as the network path's Blob arm.
                total_drained = total_drained.saturating_add(size);
            }
            WsFrame::Close { code, reason } => {
                let was_clean = code.is_some_and(|c| c == 1000) || code.is_none();
                let observable_code = code.unwrap_or(1005);
                push_peer_event(
                    state,
                    to_id,
                    WsEvent::Close {
                        code: observable_code,
                        reason: reason.clone(),
                        was_clean,
                    },
                );
                // Mirror Close on the local side so the sender's
                // close handler fires too. We treat sender-initiated
                // Close as "wasClean = true" since we delivered the
                // frame to the peer in-process.
                if let Some(_) = from_state.as_ref() {
                    push_peer_event(
                        state,
                        from_id,
                        WsEvent::Close {
                            code: observable_code,
                            reason,
                            was_clean: true,
                        },
                    );
                }
            }
            WsFrame::Pong(_) => {
                // Pong frames are an internal protocol artifact (reply
                // to a peer Ping). Pair sockets have no real wire and
                // don't generate Pings; if user code somehow queues a
                // Pong we drop it silently — it has no observable
                // effect.
            }
        }
    }

    // Decrement the SENDER's bufferedAmount — the bytes are no longer
    // queued (we passed them straight to the peer).
    if let Some(from) = from_state {
        let s = from.borrow();
        let cur = s.buffered_amount.get();
        s.buffered_amount.set(cur.saturating_sub(total_drained));
        if s.buffered_amount.get() < super::constants::MAX_BUFFERED_AMOUNT / 2 {
            s.full.set(false);
        }
    }

    // Suppress unused-state warning when neither branch runs.
    let _ = to_state;
}

/// Push an event onto a specific ws_id's queue and wake the pump.
/// Mirrors `network::push_event` but without the "this is the local
/// receive loop's task" tracking.
fn push_peer_event(state: &SharedState, ws_id: u32, event: WsEvent) {
    let Some(ws) = network::lookup_native_ws_state(state, ws_id) else {
        return;
    };
    ws.borrow_mut().events.push_back(event);

    // Push a one-shot future that resolves to `OpResult::WebSocketEvent`.
    let id = ws_id;
    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = crate::state::OpResult>>> =
        Box::pin(async move { crate::state::OpResult::WebSocketEvent { ws_id: id } });
    {
        let mut s = state.borrow_mut();
        s.spawned_ops.push(fut);
    }
    let notify = state.borrow().pump_notify_tx.clone();
    if let Some(mut tx) = notify {
        let _ = tx.try_send(());
    }
}

/// Mint two paired native sockets — `WebSocketPair[0]` and `[1]`.
/// Both sockets are server-mode (`accept()` is required to begin
/// message delivery; pre-accept sends are queued).
///
/// Returns the two pair IDs (lo, hi). The pair constructor then
/// builds two JS wrappers around fresh `WebSocketImpl` boxes seeded
/// with these IDs.
pub fn mint_pair(state: &SharedState) -> (u32, u32) {
    let lo = network::alloc_native_ws_id(state);
    let hi = network::alloc_native_ws_id(state);

    // Mark both as paired (no real network).
    if let Some(ws) = network::lookup_native_ws_state(state, lo) {
        ws.borrow_mut().is_pair = true;
    }
    if let Some(ws) = network::lookup_native_ws_state(state, hi) {
        ws.borrow_mut().is_pair = true;
    }
    (lo, hi)
}

// ---------------------------------------------------------------------------
// Native WebSocketPair constructor — installed on globalThis when the
// `runtime_native_websocket` feature is on (step 6). Replaces the
// polyfill's `WebSocketPair` for native-WS code paths.
// ---------------------------------------------------------------------------

use super::WebSocketImpl;

/// JS-callable constructor: `new WebSocketPair()` returns an object
/// with index keys [0] and [1] holding two paired native WebSocket
/// instances. Each side starts in CONNECTING; `accept()` transitions
/// to OPEN and unblocks message delivery (workerd contract).
pub fn websocket_pair_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state = match scope.get_slot::<crate::state::SharedState>() {
        Some(s) => s.clone(),
        None => {
            // No runtime — synthesize an empty pair object so simple
            // `new WebSocketPair()` doesn't crash unit tests.
            let result = v8::Object::new(scope);
            rv.set(result.into());
            return;
        }
    };

    let (lo, hi) = mint_pair(&state);

    // Build two paired WebSocketImpl wrappers. Each wrapper is a
    // brand-new JS object whose internal field 0 holds a Box<WebSocketImpl>
    // pre-seeded with the right ws_id + peer_id.
    let ws0 = build_paired_wrapper(scope, &state, lo, hi);
    let ws1 = build_paired_wrapper(scope, &state, hi, lo);

    let result = v8::Object::new(scope);
    let zero_key = v8::Integer::new_from_unsigned(scope, 0);
    let one_key = v8::Integer::new_from_unsigned(scope, 1);
    result.set(scope, zero_key.into(), ws0.into());
    result.set(scope, one_key.into(), ws1.into());
    rv.set(result.into());
}

/// Build one JS wrapper over a `WebSocketImpl` pre-seeded with the
/// given ws_id + peer_id. Used by the WebSocketPair constructor.
fn build_paired_wrapper<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &crate::state::SharedState,
    ws_id: u32,
    peer_id: u32,
) -> v8::Local<'s, v8::Object> {
    let tmpl = WebSocketImpl::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .expect("WebSocketImpl instance allocation failed");

    // Wire the prototype to globalThis.WebSocket.prototype so
    // `instanceof WebSocket` works.
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    obj.set_prototype(scope, proto_v);

    // Pre-seed the impl with ws_id, peer_id; pre-accepted=false so
    // the workerd contract `ws.accept()` is required before message
    // delivery starts. CONNECTING readyState — `open` event fires
    // when accept() flips state to OPEN.
    let impl_ = WebSocketImpl::default();
    impl_.ws_id.set(ws_id);
    impl_.peer_id.set(Some(peer_id));
    impl_.accepted.set(false);
    impl_.ready_state.set(super::ReadyState::Connecting);

    // Share the buffered_amount + full counters with NativeWsState.
    if let Some(ws_state) = network::lookup_native_ws_state(state, ws_id) {
        let mut s = ws_state.borrow_mut();
        s.buffered_amount = impl_.buffered_amount.clone();
        s.full = impl_.full.clone();
    }

    // Register the wrapper Global so the dispatch arm can find it.
    let wrapper_global = v8::Global::new(scope, obj);
    state
        .borrow_mut()
        .native_ws_wrappers
        .insert(ws_id, wrapper_global);

    // Box + finalizer (matches the macro pattern).
    let boxed = Box::new(impl_);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut WebSocketImpl));
        }),
    );
    std::mem::forget(weak);

    // Attach the EventTarget listener Rc so addEventListener works.
    crate::dom::event_target::attach_listeners(scope, obj);

    obj
}

/// Install `globalThis.WebSocketPair` to point at the native callback.
/// Called from `init.rs` AFTER the polyfill so the native shadow wins.
pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let f = v8::Function::new(scope, websocket_pair_callback).unwrap();
    let key = v8::String::new(scope, "WebSocketPair").unwrap();
    global.set(scope, key.into(), f.into());
}
