//! WebSocket native callbacks — V8 backing for the JS WebSocket polyfill.
//!
//! Provides five native callbacks registered on globalThis:
//! - `__wsCreatePair()` — allocate two linked WebSocket IDs, return the first
//! - `__wsLinkPair(id0, id1)` — link two WebSocket IDs as a pair
//! - `__wsAccept(ws_id)` — mark WebSocket as accepted (ready for messages)
//! - `__wsSend(ws_id, data)` — queue a text message on the outgoing buffer
//! - `__wsClose(ws_id, code, reason)` — initiate close

use crate::state::{SharedState, WsCachedHandles, WebSocketState, WsMessage};

// ---------------------------------------------------------------------------
// __wsCreatePair() → u32 (returns first ID; second is first + 1)
// ---------------------------------------------------------------------------

pub fn ws_create_pair_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let mut s = state.borrow_mut();
    let id0 = s.next_ws_id;
    let id1 = id0 + 1;
    s.next_ws_id = id1 + 1;

    s.websockets.insert(id0, WebSocketState::new());
    s.websockets.insert(id1, WebSocketState::new());

    rv.set(v8::Integer::new_from_unsigned(scope, id0).into());
}

// ---------------------------------------------------------------------------
// __wsLinkPair(id0, id1)
// ---------------------------------------------------------------------------

pub fn ws_link_pair_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let id0 = args.get(0).uint32_value(scope).unwrap_or(0);
    let id1 = args.get(1).uint32_value(scope).unwrap_or(0);

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let mut s = state.borrow_mut();
    if let Some(ws) = s.websockets.get_mut(&id0) {
        ws.peer_id = Some(id1);
    }
    if let Some(ws) = s.websockets.get_mut(&id1) {
        ws.peer_id = Some(id0);
    }
}

// ---------------------------------------------------------------------------
// __wsAccept(ws_id)
// ---------------------------------------------------------------------------

pub fn ws_accept_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let ws_id = args.get(0).uint32_value(scope).unwrap_or(0);

    // Resolve and cache V8 handles for this WebSocket (avoids 3 lookups per message).
    let cached = resolve_ws_handles(scope, ws_id);

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let mut s = state.borrow_mut();
    if let Some(ws) = s.websockets.get_mut(&ws_id) {
        ws.accepted = true;
        ws.cached_handles = cached;
    }
}

/// Resolve __wsRegistry[ws_id]._onMessage and ._onClose once, return cached globals.
fn resolve_ws_handles(
    scope: &mut v8::PinScope,
    ws_id: u32,
) -> Option<WsCachedHandles> {
    let global = scope.get_current_context().global(scope);

    let registry_key = v8::String::new(scope, "__wsRegistry").unwrap();
    let registry_val = global.get(scope, registry_key.into())?;
    let registry_obj = registry_val.to_object(scope)?;

    let id_key = v8::String::new(scope, &ws_id.to_string()).unwrap();
    let ws_val = registry_obj.get(scope, id_key.into())?;
    if ws_val.is_undefined() || ws_val.is_null() { return None; }
    let ws_obj = ws_val.to_object(scope)?;

    let on_message_key = v8::String::new(scope, "_onMessage").unwrap();
    let on_message_val = ws_obj.get(scope, on_message_key.into())?;
    let on_message = v8::Local::<v8::Function>::try_from(on_message_val).ok()?;

    let on_close_key = v8::String::new(scope, "_onClose").unwrap();
    let on_close_val = ws_obj.get(scope, on_close_key.into())?;
    let on_close = v8::Local::<v8::Function>::try_from(on_close_val).ok()?;

    Some(WsCachedHandles {
        ws_obj: v8::Global::new(scope, ws_obj),
        on_message: v8::Global::new(scope, on_message),
        on_close: v8::Global::new(scope, on_close),
    })
}

// ---------------------------------------------------------------------------
// __wsSend(ws_id, data)
// ---------------------------------------------------------------------------

pub fn ws_send_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let ws_id = args.get(0).uint32_value(scope).unwrap_or(0);
    let data = args.get(1).to_rust_string_lossy(scope);

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let mut s = state.borrow_mut();

    // If this WebSocket is part of a pair and the peer is accepted,
    // deliver directly to the peer's incoming queue.
    let peer_id = s.websockets.get(&ws_id).and_then(|ws| ws.peer_id);
    if let Some(peer) = peer_id {
        if let Some(peer_ws) = s.websockets.get_mut(&peer) {
            if peer_ws.accepted && !peer_ws.closed {
                peer_ws.incoming.push_back(WsMessage::Text(data.clone()));
            }
        }
    }

    // Also queue on our outgoing (for the TCP pump to drain) and wake the pump.
    if let Some(ws) = s.websockets.get_mut(&ws_id) {
        ws.outgoing.push_back(WsMessage::Text(data));
        ws.notify_outgoing();
    }
}

// ---------------------------------------------------------------------------
// __wsClose(ws_id, code, reason)
// ---------------------------------------------------------------------------

pub fn ws_close_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let ws_id = args.get(0).uint32_value(scope).unwrap_or(0);
    let code = args.get(1).uint32_value(scope).unwrap_or(1000) as u16;
    let reason = args.get(2).to_rust_string_lossy(scope);

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let mut s = state.borrow_mut();
    if let Some(ws) = s.websockets.get_mut(&ws_id) {
        ws.close_code = Some(code);
        ws.close_reason = Some(reason.clone());
        ws.outgoing.push_back(WsMessage::Close(code, reason));
        ws.notify_outgoing();
    }
}
