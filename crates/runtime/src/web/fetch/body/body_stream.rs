//! Helpers that drive the body's ReadableStream through the public
//! reader API.
//!
//! Per design §III.4, the consumer methods MUST go through the JS-
//! visible reader so spec lock checks fire (locked stream → TypeError
//! at consumer entry; disturbed → already-consumed TypeError). We
//! implement that here as a Rust-level promise-orchestration helper:
//!
//!   1. `getReader()` on the body's stream — picks up the spec's
//!      "if locked, throw TypeError" check.
//!   2. Repeatedly call `reader.read()` and chain the resulting
//!      promises into a single promise that resolves with the
//!      concatenated bytes.
//!   3. `releaseLock()` at the end so the body is no longer reader-
//!      bound (tee/clone after consume don't make sense for a
//!      consumed body, but the lock release is observable via
//!      stream.locked).
//!
//! Returned promise resolves with `Vec<u8>` on success, rejects with
//! a TypeError on lock failure, or rejects with the chunk's raw error
//! on read errors.

use std::cell::RefCell;
use std::rc::Rc;

use crate::state::OpError;

// ---------------------------------------------------------------------------
// Public entry: read all bytes from a stream
// ---------------------------------------------------------------------------

/// Read all bytes from a body's ReadableStream into a single buffer.
/// Returns a JS Promise that resolves to a `Vec<u8>` (delivered via
/// the resolver path) or rejects with whatever the stream errored
/// with.
///
/// The promise is constructed in the current scope; the chain is
/// driven by `reader.read().then(onChunk, onError)` so the runtime
/// pump (and microtask queue) drive the read loop forward.
pub fn read_all_bytes<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream_obj: v8::Local<'s, v8::Object>,
) -> Result<v8::Local<'s, v8::Promise>, OpError> {
    // Acquire a default reader. This is the Spec lock check; if the
    // stream is locked, getReader throws TypeError synchronously.
    let reader = acquire_default_reader(scope, stream_obj)?;
    let reader_global = v8::Global::new(scope, reader);

    // Build a fresh resolver that the chunk loop will resolve when
    // EOF is reached.
    let outer_resolver = v8::PromiseResolver::new(scope).unwrap();
    let outer_promise = outer_resolver.get_promise(scope);
    let outer_resolver_global = v8::Global::new(scope, outer_resolver);

    // State shared across the reactor closures: the byte accumulator
    // and the resolver (so the on-fulfilled / on-rejected steps can
    // resolve / reject the outer promise once and only once).
    let state: Rc<RefCell<ReaderState>> = Rc::new(RefCell::new(ReaderState {
        bytes: Vec::new(),
        outer_resolver: Some(outer_resolver_global),
        reader: reader_global,
    }));

    pump_one_chunk(scope, state);
    Ok(outer_promise)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

struct ReaderState {
    bytes: Vec<u8>,
    /// Once the outer promise is resolved or rejected this is set to
    /// None — defends against a misbehaving stream that re-fires
    /// `done: true` after we've already settled.
    outer_resolver: Option<v8::Global<v8::PromiseResolver>>,
    reader: v8::Global<v8::Object>,
}

fn pump_one_chunk(scope: &mut v8::PinScope, state: Rc<RefCell<ReaderState>>) {
    // reader.read()
    let read_promise = match call_reader_read(scope, &state) {
        Ok(p) => p,
        Err(exc) => {
            settle_with_error(scope, state, exc);
            return;
        }
    };

    // Attach .then(onFulfilled, onRejected). We use plain V8 promise
    // chaining (not the streams' internal upon_promise) because we're
    // outside the streams crate's promise plumbing — the chain runs
    // visible to userland Promise.prototype, but body consumers are
    // inherently visible too.
    let on_fulfilled = build_on_fulfilled(scope, state.clone());
    let on_rejected = build_on_rejected(scope, state.clone());

    let then_key = v8::String::new(scope, "then").unwrap();
    let then_v = match read_promise.get(scope, then_key.into()) {
        Some(v) => v,
        None => {
            let exc = build_type_error(scope, "reader.read().then access threw");
            settle_with_error(scope, state, exc);
            return;
        }
    };
    let Ok(then_fn) = v8::Local::<v8::Function>::try_from(then_v) else {
        let exc = build_type_error(scope, "reader.read().then is not a function");
        settle_with_error(scope, state, exc);
        return;
    };
    let args = [on_fulfilled.into(), on_rejected.into()];
    let _ = then_fn.call(scope, read_promise.into(), &args);
}

fn call_reader_read<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &Rc<RefCell<ReaderState>>,
) -> Result<v8::Local<'s, v8::Promise>, v8::Local<'s, v8::Value>> {
    let reader_local = v8::Local::new(scope, state.borrow().reader.clone());
    let read_key = v8::String::new(scope, "read").unwrap();
    let read_v = match reader_local.get(scope, read_key.into()) {
        Some(v) => v,
        None => return Err(build_type_error(scope, "reader.read access threw")),
    };
    let read_fn: v8::Local<v8::Function> = match read_v.try_into() {
        Ok(f) => f,
        Err(_) => return Err(build_type_error(scope, "reader.read is not a function")),
    };
    let result = match read_fn.call(scope, reader_local.into(), &[]) {
        Some(r) => r,
        None => return Err(build_type_error(scope, "reader.read() threw")),
    };
    let promise: v8::Local<v8::Promise> = match result.try_into() {
        Ok(p) => p,
        Err(_) => return Err(build_type_error(scope, "reader.read() did not return a promise")),
    };
    Ok(promise)
}

fn build_on_fulfilled<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: Rc<RefCell<ReaderState>>,
) -> v8::Local<'s, v8::Function> {
    let holder: Rc<RefCell<Option<Rc<RefCell<ReaderState>>>>> =
        Rc::new(RefCell::new(Some(state)));
    let raw = Rc::into_raw(holder) as *mut std::ffi::c_void;
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw);

    let tmpl = v8::FunctionTemplate::builder(on_fulfilled_raw_callback)
        .data(ext.into())
        .build(scope);
    let func = tmpl.get_function(scope).unwrap();

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        func,
        Box::new(move || unsafe {
            drop(Rc::from_raw(
                raw_addr as *const RefCell<Option<Rc<RefCell<ReaderState>>>>,
            ));
        }),
    );
    std::mem::forget(weak);

    func
}

fn build_on_rejected<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: Rc<RefCell<ReaderState>>,
) -> v8::Local<'s, v8::Function> {
    let holder: Rc<RefCell<Option<Rc<RefCell<ReaderState>>>>> =
        Rc::new(RefCell::new(Some(state)));
    let raw = Rc::into_raw(holder) as *mut std::ffi::c_void;
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw);

    let tmpl = v8::FunctionTemplate::builder(on_rejected_raw_callback)
        .data(ext.into())
        .build(scope);
    let func = tmpl.get_function(scope).unwrap();

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        func,
        Box::new(move || unsafe {
            drop(Rc::from_raw(
                raw_addr as *const RefCell<Option<Rc<RefCell<ReaderState>>>>,
            ));
        }),
    );
    std::mem::forget(weak);

    func
}

fn on_fulfilled_raw_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(state) = take_state(args.data()) else {
        return;
    };

    // result = { value, done }
    let result_v = args.get(0);
    let Ok(result) = v8::Local::<v8::Object>::try_from(result_v) else {
        let exc = build_type_error(scope, "reader.read result is not an object");
        settle_with_error(scope, state, exc);
        return;
    };
    let done_key = v8::String::new(scope, "done").unwrap();
    let value_key = v8::String::new(scope, "value").unwrap();
    let done_v = match result.get(scope, done_key.into()) {
        Some(v) => v,
        None => {
            let exc = build_type_error(scope, "reader.read result.done access threw");
            settle_with_error(scope, state, exc);
            return;
        }
    };

    if done_v.boolean_value(scope) {
        // EOF — release the reader and resolve.
        release_lock(scope, &state.borrow().reader);
        settle_with_bytes(scope, state);
        return;
    }

    let chunk_v = match result.get(scope, value_key.into()) {
        Some(v) => v,
        None => {
            let exc = build_type_error(scope, "reader.read result.value access threw");
            settle_with_error(scope, state, exc);
            return;
        }
    };

    // The stream contract for byte streams produces Uint8Array values.
    // Defensively also accept ArrayBuffer / ArrayBufferView. Anything
    // else is a TypeError.
    if let Err(msg) = append_chunk(&state, chunk_v) {
        let exc = build_type_error(scope, msg);
        settle_with_error(scope, state, exc);
        return;
    }

    // Pump the next chunk.
    pump_one_chunk(scope, state);
}

fn on_rejected_raw_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let Some(state) = take_state(args.data()) else {
        return;
    };
    let reason = args.get(0);
    settle_with_error(scope, state, reason);
}

fn take_state(
    data: v8::Local<v8::Value>,
) -> Option<Rc<RefCell<ReaderState>>> {
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return None;
    };
    let raw = ext.value() as *const RefCell<Option<Rc<RefCell<ReaderState>>>>;
    if raw.is_null() {
        return None;
    }
    // SAFETY: the External points at an Rc whose into_raw was
    // performed in build_on_*. We borrow without consuming the Rc
    // (the holder might be reused for re-fired promises in pathological
    // cases — but our outer-resolver guard ensures double-settlement
    // is a no-op). Since the read loop fires each callback once, the
    // holder's content will be `Some` exactly once and `None` after.
    let holder = unsafe { &*raw };
    holder.borrow_mut().take()
}

/// Returns Ok if appended; Err with raw bytes-not-buffer marker if
/// chunk wasn't a BufferSource. The caller materializes a TypeError
/// at its own scope.
fn append_chunk(
    state: &Rc<RefCell<ReaderState>>,
    chunk: v8::Local<v8::Value>,
) -> Result<(), &'static str> {
    let bytes = chunk_to_bytes(chunk)
        .ok_or("reader.read chunk is not a BufferSource")?;
    state.borrow_mut().bytes.extend_from_slice(&bytes);
    Ok(())
}

fn chunk_to_bytes(chunk: v8::Local<v8::Value>) -> Option<Vec<u8>> {
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(chunk) {
        let len = view.byte_length();
        let mut out = vec![0u8; len];
        let copied = view.copy_contents(&mut out);
        if copied != len {
            return None;
        }
        return Some(out);
    }
    if let Ok(buf) = v8::Local::<v8::ArrayBuffer>::try_from(chunk) {
        let len = buf.byte_length();
        let mut out = vec![0u8; len];
        if len > 0 {
            let store = buf.get_backing_store();
            unsafe {
                let src = store.data().expect("backing store").as_ptr() as *const u8;
                std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), len);
            }
        }
        return Some(out);
    }
    None
}

fn settle_with_bytes(scope: &mut v8::PinScope, state: Rc<RefCell<ReaderState>>) {
    let mut s = state.borrow_mut();
    let Some(resolver_global) = s.outer_resolver.take() else {
        return;
    };
    let bytes = std::mem::take(&mut s.bytes);
    drop(s);
    let resolver = v8::Local::new(scope, resolver_global);
    // Hand the bytes back as an ArrayBuffer using a new backing store.
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
    resolver.resolve(scope, ab.into());
}

fn settle_with_error(
    scope: &mut v8::PinScope,
    state: Rc<RefCell<ReaderState>>,
    reason: v8::Local<v8::Value>,
) {
    release_lock(scope, &state.borrow().reader);
    let mut s = state.borrow_mut();
    if let Some(resolver_global) = s.outer_resolver.take() {
        drop(s);
        let resolver = v8::Local::new(scope, resolver_global);
        resolver.reject(scope, reason);
    }
}

fn release_lock(scope: &mut v8::PinScope, reader_global: &v8::Global<v8::Object>) {
    let reader = v8::Local::new(scope, reader_global.clone());
    let key = v8::String::new(scope, "releaseLock").unwrap();
    if let Some(fn_v) = reader.get(scope, key.into()) {
        if let Ok(fn_l) = v8::Local::<v8::Function>::try_from(fn_v) {
            let _ = fn_l.call(scope, reader.into(), &[]);
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn acquire_default_reader<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    stream_obj: v8::Local<'s, v8::Object>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let key = v8::String::new(scope, "getReader").unwrap();
    let fn_v = stream_obj
        .get(scope, key.into())
        .ok_or_else(|| OpError::type_error("stream.getReader access threw"))?;
    let fn_l: v8::Local<v8::Function> = fn_v
        .try_into()
        .map_err(|_| OpError::type_error("stream.getReader is not a function"))?;
    // Use a fresh tc_scope so we can convert a thrown TypeError
    // (locked stream) into our OpError surface.
    let result = {
        v8::tc_scope!(let tc, scope);
        match fn_l.call(tc, stream_obj.into(), &[]) {
            Some(v) => Ok(v),
            None => {
                let msg = match tc.exception() {
                    Some(e) => e.to_rust_string_lossy(tc),
                    None => "stream.getReader threw".into(),
                };
                Err(OpError::type_error(msg))
            }
        }
    };
    let v = result?;
    let obj: v8::Local<v8::Object> = v
        .try_into()
        .map_err(|_| OpError::type_error("stream.getReader did not return an object"))?;
    Ok(obj)
}

fn build_type_error<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    msg: &str,
) -> v8::Local<'s, v8::Value> {
    let m = v8::String::new(scope, msg).unwrap();
    v8::Exception::type_error(scope, m)
}
