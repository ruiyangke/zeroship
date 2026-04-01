//! In-memory key-value store for V8 apps.
//! Each isolate gets its own namespace. Data persists across requests
//! but is lost when the isolate is evicted.

use crate::event_loop::SharedState;

// ---------------------------------------------------------------------------
// KV callbacks
// ---------------------------------------------------------------------------

pub(crate) fn kv_get_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    if args.length() < 1 {
        rv.set(v8::null(scope).into());
        return;
    }

    let key = args.get(0).to_rust_string_lossy(scope);
    let s = state.borrow();
    match s.kv_store.get(&key) {
        Some(val) => {
            let v8_str = v8::String::new(scope, val).unwrap();
            rv.set(v8_str.into());
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

pub(crate) fn kv_set_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    if args.length() < 2 {
        rv.set(v8::Boolean::new(scope, false).into());
        return;
    }

    let key = args.get(0).to_rust_string_lossy(scope);
    let value = args.get(1).to_rust_string_lossy(scope);
    state.borrow_mut().kv_store.insert(key, value);
    rv.set(v8::Boolean::new(scope, true).into());
}

pub(crate) fn kv_delete_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    if args.length() < 1 {
        rv.set(v8::Boolean::new(scope, false).into());
        return;
    }

    let key = args.get(0).to_rust_string_lossy(scope);
    let existed = state.borrow_mut().kv_store.remove(&key).is_some();
    rv.set(v8::Boolean::new(scope, existed).into());
}

pub(crate) fn kv_list_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    let s = state.borrow();
    let keys: Vec<&String> = s.kv_store.keys().collect();
    let arr = v8::Array::new(scope, keys.len() as i32);
    for (i, key) in keys.iter().enumerate() {
        let v8_str = v8::String::new(scope, key).unwrap();
        arr.set_index(scope, i as u32, v8_str.into());
    }
    rv.set(arr.into());
}
