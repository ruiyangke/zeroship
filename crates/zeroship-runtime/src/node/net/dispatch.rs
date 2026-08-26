//! V8-thread dispatch for native `node:net.Socket` events.

use crate::state::SharedState;

use super::state::{self, SocketEvent};

pub fn dispatch_pending_socket_events(
    scope: &mut v8::PinScope,
    state: &SharedState,
    socket_id: u32,
) {
    let mut events: std::collections::VecDeque<_> =
        state::drain_events(state, socket_id).into();
    if events.is_empty() {
        return;
    }

    let wrapper_g = {
        let s = state.borrow();
        s.native_socket_wrappers.get(&socket_id).cloned()
    };
    let Some(wrapper_g) = wrapper_g else {
        return;
    };
    let wrapper = v8::Local::new(scope, wrapper_g);

    let mut dispatched_close = false;
    while let Some(event) = events.pop_front() {
        if matches!(event, SocketEvent::Data(_)) && state::socket_paused(state, socket_id) {
            events.push_front(event);
            state::requeue_front_events(state, socket_id, events);
            return;
        }
        let is_data = matches!(event, SocketEvent::Data(_));
        let is_close = matches!(event, SocketEvent::Close { .. });
        dispatch_one(scope, wrapper, event);
        if is_close {
            dispatched_close = true;
            break;
        }
        if is_data && state::socket_paused(state, socket_id) && !events.is_empty() {
            state::requeue_front_events(state, socket_id, events);
            return;
        }
    }

    if dispatched_close {
        state::free_native_socket_state(state, socket_id);
    }
}

fn dispatch_one(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Object>,
    event: SocketEvent,
) {
    match event {
        SocketEvent::Connect => emit(scope, wrapper, "connect", &[]),
        SocketEvent::Ready => emit(scope, wrapper, "ready", &[]),
        SocketEvent::SecureConnect {
            authorized,
            authorization_error,
        } => {
            set_bool(scope, wrapper, "authorized", authorized);
            match authorization_error {
                Some(err) => set_string(scope, wrapper, "authorizationError", &err),
                None => set_null(scope, wrapper, "authorizationError"),
            }
            emit(scope, wrapper, "secureConnect", &[]);
        }
        SocketEvent::Data(bytes) => {
            let buf = crate::node::buffer::emit_buffer(scope, &bytes);
            emit(scope, wrapper, "data", &[buf]);
        }
        SocketEvent::Drain => emit(scope, wrapper, "drain", &[]),
        SocketEvent::End => emit(scope, wrapper, "end", &[]),
        SocketEvent::Error { message, code } => {
            let err = build_error(scope, &message, &code);
            emit(scope, wrapper, "error", &[err]);
        }
        SocketEvent::Close { had_error } => {
            let arg = v8::Boolean::new(scope, had_error);
            emit(scope, wrapper, "close", &[arg.into()]);
        }
    }
}

fn emit(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Object>,
    name: &str,
    args: &[v8::Local<v8::Value>],
) {
    let emit_key = v8::String::new(scope, "__zsEmit").unwrap();
    let emit_val = wrapper
        .get(scope, emit_key.into())
        .or_else(|| {
            let key = v8::String::new(scope, "emit").unwrap();
            wrapper.get(scope, key.into())
        });
    let Some(emit_val) = emit_val else {
        return;
    };
    let Ok(emit_fn) = v8::Local::<v8::Function>::try_from(emit_val) else {
        return;
    };
    let event_name = v8::String::new(scope, name).unwrap();
    let mut event_args: Vec<v8::Local<v8::Value>> = Vec::with_capacity(args.len() + 1);
    event_args.push(event_name.into());
    event_args.extend_from_slice(args);
    let _ = emit_fn.call(scope, wrapper.into(), &event_args);
}

fn set_bool(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Object>,
    key: &str,
    value: bool,
) {
    let key = v8::String::new(scope, key).unwrap();
    let value = v8::Boolean::new(scope, value);
    let _ = wrapper.set(scope, key.into(), value.into());
}

fn set_string(
    scope: &mut v8::PinScope,
    wrapper: v8::Local<v8::Object>,
    key: &str,
    value: &str,
) {
    let key = v8::String::new(scope, key).unwrap();
    let value = v8::String::new(scope, value).unwrap();
    let _ = wrapper.set(scope, key.into(), value.into());
}

fn set_null(scope: &mut v8::PinScope, wrapper: v8::Local<v8::Object>, key: &str) {
    let key = v8::String::new(scope, key).unwrap();
    let value = v8::null(scope);
    let _ = wrapper.set(scope, key.into(), value.into());
}

fn build_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    message: &str,
    code: &str,
) -> v8::Local<'s, v8::Value> {
    let msg = v8::String::new(scope, message).unwrap();
    let err = v8::Exception::error(scope, msg);
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(err) {
        let code_key = v8::String::new(scope, "code").unwrap();
        let code_val = v8::String::new(scope, code).unwrap();
        obj.set(scope, code_key.into(), code_val.into());
    }
    err
}
