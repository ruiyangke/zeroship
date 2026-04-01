//! V8 global bindings — console, setTimeout, clearTimeout, setInterval, clearInterval, env.

use std::cmp::Reverse;
use std::time::Duration;

use crate::event_loop::SharedState;
use crate::timers::{TimerCallback, TimerHeapEntry};

// ---------------------------------------------------------------------------
// Console polyfill
// ---------------------------------------------------------------------------

fn console_log_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let mut parts = Vec::new();
    for i in 0..args.length() {
        let arg = args.get(i);
        let s = arg.to_rust_string_lossy(scope);
        parts.push(s);
    }
    let line = parts.join(" ");
    println!("{}", line);

    // Append to per-isolate log buffer
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();
    let mut s = state.borrow_mut();
    s.log_buffer.push(line);
    // Cap buffer at 1000 entries to prevent memory bloat
    if s.log_buffer.len() > 1000 {
        let drain = s.log_buffer.len() - 1000;
        s.log_buffer.drain(..drain);
    }
}

// ---------------------------------------------------------------------------
// Timer callbacks
// ---------------------------------------------------------------------------

fn set_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    let callback = match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "setTimeout: first argument must be a function")
                .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let ms = if args.length() > 1 {
        args.get(1).uint32_value(scope).unwrap_or(0)
    } else {
        0
    };

    let global_cb = v8::Global::new(scope, callback);
    let mut s = state.borrow_mut();
    let id = s.timers.next_id;
    s.timers.next_id += 1;

    let delay = Duration::from_millis(u64::from(ms));

    s.timers.heap.push(Reverse(TimerHeapEntry {
        fire_at: std::time::Instant::now() + delay,
        id,
    }));
    s.timers.callbacks.insert(
        id,
        TimerCallback {
            callback: global_cb,
            interval: None,
        },
    );

    rv.set(v8::Integer::new(scope, id as i32).into());
}

fn clear_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    let id = if args.length() > 0 {
        args.get(0).uint32_value(scope).unwrap_or(0)
    } else {
        return;
    };

    // Lazy deletion: only remove from callbacks, heap entry will be skipped when popped
    state.borrow_mut().timers.callbacks.remove(&id);
}

fn set_interval_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("EventLoopState not in isolate slot")
        .clone();

    let callback = match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(f) => f,
        Err(_) => {
            let msg = v8::String::new(scope, "setInterval: first argument must be a function")
                .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let ms = if args.length() > 1 {
        args.get(1).uint32_value(scope).unwrap_or(0)
    } else {
        0
    };
    let dur = Duration::from_millis(u64::from(ms));

    let global_cb = v8::Global::new(scope, callback);
    let mut s = state.borrow_mut();
    let id = s.timers.next_id;
    s.timers.next_id += 1;

    s.timers.heap.push(Reverse(TimerHeapEntry {
        fire_at: std::time::Instant::now() + dur,
        id,
    }));
    s.timers.callbacks.insert(
        id,
        TimerCallback {
            callback: global_cb,
            interval: Some(dur),
        },
    );

    rv.set(v8::Integer::new(scope, id as i32).into());
}

// ---------------------------------------------------------------------------
// env.get callback — reads APPBASE_APP_{KEY} from process env
// ---------------------------------------------------------------------------

fn env_get_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        rv.set(v8::null(scope).into());
        return;
    }

    let key = args.get(0).to_rust_string_lossy(scope);
    let env_key = format!("APPBASE_APP_{}", key.to_uppercase());
    match std::env::var(&env_key) {
        Ok(val) => {
            let v = v8::String::new(scope, &val).unwrap();
            rv.set(v.into());
        }
        Err(_) => {
            rv.set(v8::null(scope).into());
        }
    }
}

// ---------------------------------------------------------------------------
// Setup all globals on a V8 context
// ---------------------------------------------------------------------------

/// Install console, setTimeout, clearTimeout, setInterval, clearInterval on the global object.
pub(crate) fn setup_globals(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);

    // console.log/warn/error/info
    {
        let console = v8::Object::new(scope);
        let log_fn = v8::Function::new(scope, console_log_callback).unwrap();
        let log_key = v8::String::new(scope, "log").unwrap();
        console.set(scope, log_key.into(), log_fn.into());

        let warn_key = v8::String::new(scope, "warn").unwrap();
        console.set(scope, warn_key.into(), log_fn.into());
        let error_key = v8::String::new(scope, "error").unwrap();
        console.set(scope, error_key.into(), log_fn.into());
        let info_key = v8::String::new(scope, "info").unwrap();
        console.set(scope, info_key.into(), log_fn.into());

        let console_key = v8::String::new(scope, "console").unwrap();
        global.set(scope, console_key.into(), console.into());
    }

    // setTimeout
    {
        let f = v8::Function::new(scope, set_timeout_callback).unwrap();
        let key = v8::String::new(scope, "setTimeout").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // clearTimeout
    {
        let f = v8::Function::new(scope, clear_timeout_callback).unwrap();
        let key = v8::String::new(scope, "clearTimeout").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // setInterval
    {
        let f = v8::Function::new(scope, set_interval_callback).unwrap();
        let key = v8::String::new(scope, "setInterval").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // clearInterval (same implementation as clearTimeout)
    {
        let f = v8::Function::new(scope, clear_timeout_callback).unwrap();
        let key = v8::String::new(scope, "clearInterval").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // __rawFetch (native HTTP fetch)
    {
        let f = v8::Function::new(scope, crate::fetch::raw_fetch_callback).unwrap();
        let key = v8::String::new(scope, "__rawFetch").unwrap();
        global.set(scope, key.into(), f.into());
    }

    // kv namespace
    {
        let kv = v8::Object::new(scope);

        let get_fn = v8::Function::new(scope, crate::kv::kv_get_callback).unwrap();
        let get_key = v8::String::new(scope, "get").unwrap();
        kv.set(scope, get_key.into(), get_fn.into());

        let set_fn = v8::Function::new(scope, crate::kv::kv_set_callback).unwrap();
        let set_key = v8::String::new(scope, "set").unwrap();
        kv.set(scope, set_key.into(), set_fn.into());

        let del_fn = v8::Function::new(scope, crate::kv::kv_delete_callback).unwrap();
        let del_key = v8::String::new(scope, "delete").unwrap();
        kv.set(scope, del_key.into(), del_fn.into());

        let list_fn = v8::Function::new(scope, crate::kv::kv_list_callback).unwrap();
        let list_key = v8::String::new(scope, "list").unwrap();
        kv.set(scope, list_key.into(), list_fn.into());

        let kv_key = v8::String::new(scope, "kv").unwrap();
        global.set(scope, kv_key.into(), kv.into());
    }

    // env namespace (read-only, reads APPBASE_APP_{KEY} from process env)
    {
        let env = v8::Object::new(scope);

        let get_fn = v8::Function::new(scope, env_get_callback).unwrap();
        let get_key = v8::String::new(scope, "get").unwrap();
        env.set(scope, get_key.into(), get_fn.into());

        let env_key = v8::String::new(scope, "env").unwrap();
        global.set(scope, env_key.into(), env.into());
    }
}
