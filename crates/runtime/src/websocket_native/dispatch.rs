//! V8-thread dispatch arm for `OpResult::WebSocketEvent`.
//!
//! Drains the per-WS event queue (FIFO) and, for each event, mints a
//! native MessageEvent / CloseEvent / Event and dispatches it via
//! `dom::event_target::dispatch_event`. State transitions that the
//! event implies (CONNECTING → OPEN, OPEN → CLOSED) happen here so JS
//! observers see the correct readyState by the time the event fires.
//!
//! The pump arm in `runtime.rs` calls `dispatch_pending_ws_events`
//! when it sees `OpResult::WebSocketEvent { ws_id }`. Multiple queued
//! events are dispatched in one V8 turn (no microtask checkpoint
//! between them — the queue settles, then one checkpoint outside).

#![cfg(feature = "runtime_native_websocket")]

use std::rc::Rc;

use crate::state::SharedState;

use super::network::{self, WsEvent};
use super::{websocket_from_obj, BinaryType, ReadyState, WebSocketImpl};

/// Drain and dispatch every queued event for `ws_id`.
pub fn dispatch_pending_ws_events(scope: &mut v8::PinScope, state: &SharedState, ws_id: u32) {
    let events = network::drain_events(state, ws_id);
    if events.is_empty() {
        return;
    }

    // Resolve the JS wrapper. We need the wrapper Global from the
    // WebSocketImpl's cached handles. If JS has discarded all references
    // (the wrapper has been GC'd), the impl pointer is gone — bail.
    let wrapper_global = match find_wrapper_global(state, ws_id) {
        Some(g) => g,
        None => return,
    };
    let wrapper = v8::Local::new(scope, wrapper_global);

    // Make sure the EventTarget listener Rc is attached before
    // dispatch — required for `dispatchEvent` to find listeners.
    crate::dom::event_target::attach_listeners(scope, wrapper);

    let Some(impl_) = websocket_from_obj(scope, wrapper) else {
        return;
    };

    for ev in events {
        dispatch_one(scope, wrapper, impl_, ev);
    }
}

/// Find the cached `wrapper` Global for `ws_id`. The wrapper is
/// cached on the impl's `cached_handles.ws_obj` either eagerly by the
/// pair mint (step 6) or lazily by us (here). We can also walk
/// `state.native_websockets` to find the impl pointer — but the
/// existing pattern is to look up via the cached handle. For now:
/// require the V8 dispatch path to have set the cached `ws_obj` at
/// least once; native ws ids always do this on first dispatch.
fn find_wrapper_global(
    state: &SharedState,
    ws_id: u32,
) -> Option<v8::Global<v8::Object>> {
    // The impl's cached `ws_obj` is the wrapper. We walk the
    // pending_resolvers / wait_until tracking maps would not help —
    // instead, the runtime stores the wrapper Global in a small
    // map populated by `WebSocketImpl::new` when it allocates ws_id.
    // To keep this clean, look up via `state.native_ws_wrappers`.
    let s = state.borrow();
    s.native_ws_wrappers.get(&ws_id).cloned()
}

fn dispatch_one(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Object>,
    impl_: &WebSocketImpl,
    ev: WsEvent,
) {
    match ev {
        WsEvent::Open {
            protocol,
            extensions,
        } => {
            // §4 step 4: set protocol/extensions; readyState=OPEN.
            *impl_.protocol.borrow_mut() = protocol;
            *impl_.extensions.borrow_mut() = extensions;
            impl_.ready_state.set(ReadyState::Open);
            let event = build_plain_event(scope, "open");
            crate::dom::event_target::dispatch_event(scope, wrapper, event);
        }
        WsEvent::MessageText(s) => {
            let s_v8 = match v8::String::new(scope, &s) {
                Some(v) => v,
                None => return,
            };
            let origin = impl_
                .url
                .borrow()
                .as_ref()
                .map(|u| u.origin().ascii_serialization())
                .unwrap_or_default();
            let me = crate::dom::message_event::build_message_event(scope, s_v8.into(), &origin);
            crate::dom::event_target::dispatch_event(scope, wrapper, me);
        }
        WsEvent::MessageBinary(b) => {
            let data_v8: v8::Local<v8::Value> = match impl_.binary_type.get() {
                BinaryType::Blob => {
                    let blob =
                        crate::blob_native::blob::from_bytes_owned_public(b, String::new());
                    crate::blob_native::blob::wrap_blob_in_v8(scope, blob)
                }
                BinaryType::ArrayBuffer => {
                    let len = b.len();
                    let ab = v8::ArrayBuffer::new(scope, len);
                    let store = ab.get_backing_store();
                    for (i, &byte) in b.iter().enumerate() {
                        store[i].set(byte);
                    }
                    ab.into()
                }
            };
            let origin = impl_
                .url
                .borrow()
                .as_ref()
                .map(|u| u.origin().ascii_serialization())
                .unwrap_or_default();
            let me = crate::dom::message_event::build_message_event(scope, data_v8, &origin);
            crate::dom::event_target::dispatch_event(scope, wrapper, me);
        }
        WsEvent::Close {
            code,
            reason,
            was_clean,
        } => {
            impl_.ready_state.set(ReadyState::Closed);
            let ce =
                crate::dom::close_event::build_close_event(scope, code, &reason, was_clean);
            crate::dom::event_target::dispatch_event(scope, wrapper, ce);
        }
        WsEvent::Error { reason: _ } => {
            // §4 step 3.1: fire a plain `error` Event (NOT ErrorEvent
            // — D-26).
            let event = build_plain_event(scope, "error");
            crate::dom::event_target::dispatch_event(scope, wrapper, event);
        }
    }
}

/// Build a plain `Event` instance with `is_trusted = true`. Used for
/// `open` and `error` (CloseEvent / MessageEvent take their
/// dedicated builders).
fn build_plain_event<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    type_: &str,
) -> v8::Local<'s, v8::Object> {
    use crate::dom::event::{now_ms, Event};
    let tmpl = Event::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .expect("Event instance allocation failed");

    let ev = Event::default();
    *ev.event_type.borrow_mut() = type_.to_string();
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

    obj
}
