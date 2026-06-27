//! Random ops — `randomBytes`, `randomFillSync`, `randomFill`,
//! `randomInt`, `randomUUID`, `getRandomValues`.
//!
//! See `docs/proposals/node-crypto-native.md` §VI.5.
//!
//! All sync: backed by the existing thread-local 4 KB CSPRNG buffer
//! (`crate::crypto::fast_random`) which amortises one syscall over
//! many small fills.

use crate::node::buffer;
use crate::state::OpError;

const HEX: &[u8; 16] = b"0123456789abcdef";

// ---------------------------------------------------------------------------
// randomBytes(size, callback?) -> Buffer | undefined
//   Without callback: returns Buffer synchronously.
//   With callback:    fires callback(null, buf) on next microtask
//                     (Node's process.nextTick — we use a Promise.resolve
//                     to land on the microtask queue).
// ---------------------------------------------------------------------------

pub(crate) fn random_bytes_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let size = match read_size(scope, args.get(0)) {
        Ok(n) => n,
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
            return;
        }
    };
    let mut buf = vec![0u8; size];
    crate::crypto::fast_random(&mut buf);

    // Callback variant: fire on microtask.
    if args.length() >= 2 && args.get(1).is_function() {
        let cb: v8::Local<v8::Function> = args.get(1).try_into().unwrap();
        let buffer = buffer::emit_buffer(scope, &buf);
        let undef = v8::undefined(scope);
        // Schedule via Promise.resolve().then so the callback fires
        // asynchronously (matches Node's process.nextTick behaviour).
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        // Resolve immediately with the buffer; the .then below will
        // run the user's callback on the microtask queue.
        resolver.resolve(scope, undef.into());
        let cb_global = v8::Global::new(scope, cb);
        let buf_global = v8::Global::new(scope, buffer);
        let then_key = v8::String::new(scope, "then").unwrap();
        let then_fn: v8::Local<v8::Function> =
            promise.get(scope, then_key.into()).unwrap().try_into().unwrap();
        // Build a thunk that calls cb(null, buf):
        let external = v8::External::new(
            scope,
            Box::into_raw(Box::new((cb_global, buf_global))) as *mut std::ffi::c_void,
        );
        let thunk_tmpl = v8::FunctionTemplate::builder(thunk_callback)
            .data(external.into())
            .build(scope);
        let thunk = thunk_tmpl.get_function(scope).unwrap();
        let _ = then_fn.call(scope, promise.into(), &[thunk.into()]);
    } else {
        // Sync: return Buffer directly.
        let buffer = buffer::emit_buffer(scope, &buf);
        rv.set(buffer);
    }
}

#[allow(unsafe_code)]
fn thunk_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    // Recover (cb, buf) from data External.
    let data = args.data();
    let ext: v8::Local<v8::External> = match data.try_into() {
        Ok(e) => e,
        Err(_) => return,
    };
    let raw = ext.value() as *mut (v8::Global<v8::Function>, v8::Global<v8::Value>);
    let (cb_global, buf_global) = unsafe { *Box::from_raw(raw) };
    let cb = v8::Local::new(scope, &cb_global);
    let buf = v8::Local::new(scope, &buf_global);
    let null = v8::null(scope);
    let undef = v8::undefined(scope);
    let _ = cb.call(scope, undef.into(), &[null.into(), buf]);
}

fn read_size(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> Result<usize, OpError> {
    if !value.is_number() {
        return Err(OpError::node(
            "ERR_INVALID_ARG_TYPE",
            "size must be a number",
        ));
    }
    let n = value.number_value(scope).unwrap_or(-1.0);
    if !n.is_finite() || n < 0.0 || n > (u32::MAX as f64) {
        return Err(OpError::node(
            "ERR_OUT_OF_RANGE",
            format!("size out of range: {n}"),
        ));
    }
    Ok(n as usize)
}

// ---------------------------------------------------------------------------
// randomFillSync(buffer, offset?, size?) -> buffer
// ---------------------------------------------------------------------------

pub(crate) fn random_fill_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "randomFillSync: buffer is required",
        );
        scope.throw_exception(exc);
        return;
    }
    let buf_val = args.get(0);
    let view: v8::Local<v8::ArrayBufferView> = match buf_val.try_into() {
        Ok(v) => v,
        Err(_) => {
            let exc = crate::node_error::build_node_exception(
                scope,
                "ERR_INVALID_ARG_TYPE",
                "buffer must be a Buffer or TypedArray",
            );
            scope.throw_exception(exc);
            return;
        }
    };
    let total_byte_len = view.byte_length();
    let offset = if args.length() >= 2 {
        let v = args.get(1);
        if v.is_undefined() {
            0
        } else {
            v.uint32_value(scope).unwrap_or(0) as usize
        }
    } else {
        0
    };
    let size = if args.length() >= 3 {
        let v = args.get(2);
        if v.is_undefined() {
            total_byte_len.saturating_sub(offset)
        } else {
            v.uint32_value(scope).unwrap_or(0) as usize
        }
    } else {
        total_byte_len.saturating_sub(offset)
    };
    if offset.saturating_add(size) > total_byte_len {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_OUT_OF_RANGE",
            "offset + size exceeds buffer length",
        );
        scope.throw_exception(exc);
        return;
    }

    // Get backing store + write bytes.
    let mut tmp = vec![0u8; size];
    crate::crypto::fast_random(&mut tmp);
    let ab = match view.buffer(scope) {
        Some(b) => b,
        None => return,
    };
    let store = ab.get_backing_store();
    let view_offset = view.byte_offset();
    for (i, &b) in tmp.iter().enumerate() {
        store[view_offset + offset + i].set(b);
    }
    rv.set(view.into());
}

// ---------------------------------------------------------------------------
// randomFill(buffer, offset?, size?, callback) -> void
//   Always async; fires callback(null, buf) on microtask.
// ---------------------------------------------------------------------------

pub(crate) fn random_fill_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    // Last arg is the callback; the rest mirror randomFillSync.
    let n = args.length();
    if n < 2 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "randomFill: buffer + callback are required",
        );
        scope.throw_exception(exc);
        return;
    }
    let cb_val = args.get(n - 1);
    if !cb_val.is_function() {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "callback must be a function",
        );
        scope.throw_exception(exc);
        return;
    }
    // Reuse the sync implementation, then fire the callback via a
    // microtask. We manually build the args slice for randomFillSync.
    // Easiest: do the fill now, then resolve.
    let buf_val = args.get(0);
    let view: v8::Local<v8::ArrayBufferView> = match buf_val.try_into() {
        Ok(v) => v,
        Err(_) => {
            let exc = crate::node_error::build_node_exception(
                scope,
                "ERR_INVALID_ARG_TYPE",
                "buffer must be a Buffer or TypedArray",
            );
            scope.throw_exception(exc);
            return;
        }
    };
    let total_byte_len = view.byte_length();
    // Optional offset / size in the middle args (everything but the
    // last is offset/size; conventionally either (buf, cb), (buf, offset, cb),
    // or (buf, offset, size, cb)).
    let (offset, size) = match n {
        2 => (0usize, total_byte_len),
        3 => {
            let off = args.get(1).uint32_value(scope).unwrap_or(0) as usize;
            (off, total_byte_len.saturating_sub(off))
        }
        _ => {
            let off = args.get(1).uint32_value(scope).unwrap_or(0) as usize;
            let s = args.get(2).uint32_value(scope).unwrap_or(0) as usize;
            (off, s)
        }
    };
    if offset.saturating_add(size) > total_byte_len {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_OUT_OF_RANGE",
            "offset + size exceeds buffer length",
        );
        scope.throw_exception(exc);
        return;
    }
    let mut tmp = vec![0u8; size];
    crate::crypto::fast_random(&mut tmp);
    let ab = match view.buffer(scope) {
        Some(b) => b,
        None => return,
    };
    let store = ab.get_backing_store();
    let view_offset = view.byte_offset();
    for (i, &b) in tmp.iter().enumerate() {
        store[view_offset + offset + i].set(b);
    }
    // Schedule callback async (matches Node's contract that randomFill
    // is always callback-style and fires on the next tick).
    schedule_callback(scope, cb_val.try_into().unwrap(), view.into());
}

#[allow(unsafe_code)]
fn schedule_callback<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    cb: v8::Local<v8::Function>,
    arg: v8::Local<v8::Value>,
) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let undef = v8::undefined(scope);
    resolver.resolve(scope, undef.into());
    let cb_global = v8::Global::new(scope, cb);
    let arg_global = v8::Global::new(scope, arg);
    let then_key = v8::String::new(scope, "then").unwrap();
    let then_fn: v8::Local<v8::Function> =
        promise.get(scope, then_key.into()).unwrap().try_into().unwrap();
    let external = v8::External::new(
        scope,
        Box::into_raw(Box::new((cb_global, arg_global))) as *mut std::ffi::c_void,
    );
    let thunk_tmpl = v8::FunctionTemplate::builder(thunk_callback)
        .data(external.into())
        .build(scope);
    let thunk = thunk_tmpl.get_function(scope).unwrap();
    let _ = then_fn.call(scope, promise.into(), &[thunk.into()]);
}

// ---------------------------------------------------------------------------
// randomInt(min?, max, callback?) -> number
//   Uniform via rejection sampling (no modulo bias).
// ---------------------------------------------------------------------------

pub(crate) fn random_int_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let n = args.length();
    let (min, max, cb) = if n == 1 {
        (0i64, args.get(0).number_value(scope).unwrap_or(0.0) as i64, None)
    } else if n == 2 && args.get(1).is_function() {
        (
            0i64,
            args.get(0).number_value(scope).unwrap_or(0.0) as i64,
            Some(args.get(1)),
        )
    } else if n == 2 {
        (
            args.get(0).number_value(scope).unwrap_or(0.0) as i64,
            args.get(1).number_value(scope).unwrap_or(0.0) as i64,
            None,
        )
    } else if n >= 3 {
        let cb_val = args.get(2);
        let cb = if cb_val.is_function() { Some(cb_val) } else { None };
        (
            args.get(0).number_value(scope).unwrap_or(0.0) as i64,
            args.get(1).number_value(scope).unwrap_or(0.0) as i64,
            cb,
        )
    } else {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "randomInt: max is required",
        );
        scope.throw_exception(exc);
        return;
    };
    if min >= max {
        let msg = format!("max must be greater than min ({min} >= {max})");
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_OUT_OF_RANGE",
            &msg,
        );
        scope.throw_exception(exc);
        return;
    }
    let range = (max - min) as u64;
    if range == 0 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_OUT_OF_RANGE",
            "range must be > 0",
        );
        scope.throw_exception(exc);
        return;
    }
    // Rejection sampling on u64: pick a random u64, accept if it
    // falls in [0, floor(2^64 / range) * range), reduce mod range.
    let limit = (u64::MAX / range) * range;
    let result;
    loop {
        let mut buf = [0u8; 8];
        crate::crypto::fast_random(&mut buf);
        let v = u64::from_le_bytes(buf);
        if v < limit {
            result = (v % range) as i64 + min;
            break;
        }
    }
    let result_v = v8::Number::new(scope, result as f64);
    if let Some(cb_val) = cb {
        let cb: v8::Local<v8::Function> = cb_val.try_into().unwrap();
        schedule_callback(scope, cb, result_v.into());
    } else {
        rv.set(result_v.into());
    }
}

// ---------------------------------------------------------------------------
// randomUUID() -> string
//   Mirrors crypto.randomUUID(). RFC 4122 v4.
// ---------------------------------------------------------------------------

pub(crate) fn random_uuid_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let mut b = [0u8; 16];
    crate::crypto::fast_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    let mut buf = [0u8; 36];
    let mut p = 0;
    for (i, &byte) in b.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            buf[p] = b'-';
            p += 1;
        }
        buf[p] = HEX[(byte >> 4) as usize];
        p += 1;
        buf[p] = HEX[(byte & 0x0f) as usize];
        p += 1;
    }
    let s = std::str::from_utf8(&buf).unwrap();
    rv.set(v8::String::new(scope, s).unwrap().into());
}

// ---------------------------------------------------------------------------
// getRandomValues — re-export of WebCrypto's getRandomValues.
// We simply delegate to globalThis.crypto.getRandomValues.
// ---------------------------------------------------------------------------

pub(crate) fn get_random_values_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let context = scope.get_current_context();
    let global = context.global(scope);
    let crypto_key = v8::String::new(scope, "crypto").unwrap();
    let crypto_obj = match global.get(scope, crypto_key.into()) {
        Some(v) => match v8::Local::<v8::Object>::try_from(v) {
            Ok(o) => o,
            Err(_) => return,
        },
        None => return,
    };
    let grv_key = v8::String::new(scope, "getRandomValues").unwrap();
    let grv_fn: v8::Local<v8::Function> = match crypto_obj
        .get(scope, grv_key.into())
        .and_then(|v| v.try_into().ok())
    {
        Some(f) => f,
        None => return,
    };
    let arg = args.get(0);
    if let Some(result) = grv_fn.call(scope, crypto_obj.into(), &[arg]) {
        rv.set(result);
    }
}
