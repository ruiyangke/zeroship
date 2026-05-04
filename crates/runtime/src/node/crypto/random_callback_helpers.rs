//! Tiny helper for scheduling Node-style `callback(err, value)` from
//! Rust on the V8 microtask queue.
//!
//! Used by `random.rs` and `kdf.rs` to deliver async results.

#![allow(unsafe_code)]

/// Schedule `cb(err, value)` on the next microtask. Either both `err`
/// and `value` may be `None` (rare; cb fires with no args), one is
/// `Some` and the other `None` (the common error / success cases),
/// or both are `Some` (the async-error path may also pass partial
/// data).
pub(crate) fn schedule_node_cb<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    cb: v8::Local<v8::Function>,
    err: Option<v8::Local<v8::Value>>,
    value: Option<v8::Local<v8::Value>>,
) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let undef = v8::undefined(scope);
    resolver.resolve(scope, undef.into());
    let then_key = v8::String::new(scope, "then").unwrap();
    let then_fn: v8::Local<v8::Function> =
        match promise.get(scope, then_key.into()).and_then(|v| v.try_into().ok()) {
            Some(f) => f,
            None => return,
        };

    // Box (cb, err_global, value_global) — recovered in the thunk.
    let cb_global = v8::Global::new(scope, cb);
    let err_global = err.map(|v| v8::Global::new(scope, v));
    let value_global = value.map(|v| v8::Global::new(scope, v));
    let payload = Box::new(NodeCallbackPayload {
        cb: cb_global,
        err: err_global,
        value: value_global,
    });
    let raw = Box::into_raw(payload);
    let external = v8::External::new(scope, raw as *mut std::ffi::c_void);
    let thunk_tmpl = v8::FunctionTemplate::builder(thunk_callback)
        .data(external.into())
        .build(scope);
    let thunk = thunk_tmpl.get_function(scope).unwrap();
    let _ = then_fn.call(scope, promise.into(), &[thunk.into()]);
}

struct NodeCallbackPayload {
    cb: v8::Global<v8::Function>,
    err: Option<v8::Global<v8::Value>>,
    value: Option<v8::Global<v8::Value>>,
}

fn thunk_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let data = args.data();
    let ext: v8::Local<v8::External> = match data.try_into() {
        Ok(e) => e,
        Err(_) => return,
    };
    let raw = ext.value() as *mut NodeCallbackPayload;
    let payload = unsafe { *Box::from_raw(raw) };
    let cb = v8::Local::new(scope, &payload.cb);
    let err_v = match &payload.err {
        Some(g) => v8::Local::new(scope, g),
        None => v8::null(scope).into(),
    };
    let val_v = match &payload.value {
        Some(g) => v8::Local::new(scope, g),
        None => v8::undefined(scope).into(),
    };
    let undef = v8::undefined(scope);
    let _ = cb.call(scope, undef.into(), &[err_v, val_v]);
}
