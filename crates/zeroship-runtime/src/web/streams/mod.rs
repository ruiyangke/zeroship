//! Native WHATWG Streams implementation.
//!
//! Spec: https://streams.spec.whatwg.org/
//!
//! # Module layout
//!
//! - `stream_forwarder` - reads a JS ReadableStream into a bounded
//!   `StreamWriter` channel a native consumer drains (response bodies,
//!   RPC stream procedures, `putStream` uploads and streaming `fetch`
//!   request bodies).
//! - `slots` — V8 private symbol helpers (read/write `[[reader]]`,
//!   `[[controller]]`, `[[storedError]]` etc).
//! - `queue` — `VecDeque<QueueEntry>` + `[[queueTotalSize]]` invariant
//!   maintenance helpers (`enqueue_value_with_size`, `dequeue_value`,
//!   `reset_queue`).
//! - `algorithms` — cross-class spec abstract operations (e.g.
//!   `ReadableStreamFulfillReadRequest`, `ReadableStreamCancel`).
//! - `promise_resolve` — helpers for routing a resolver through
//!   `OpResult::JsValue`.
//! - `budget` - each isolate's live stream cap (`budget::MAX_LIVE_STREAMS`).
//!
//! Class files (one per public IDL interface) follow:
//! - `readable` / `readable_default_controller` / `readable_default_reader` /
//!   `readable_byte_controller` / `readable_byob_reader` / `byob_request`
//! - `writable` / `writable_controller` / `writable_writer`
//! - `transform` / `transform_controller`
//! - `strategies` (ByteLength + Count)

// Native consumer of JS ReadableStreams. Used by `transport::handler`,
// `runtime::build_fetch_outcome`, RPC stream dispatch, `env.storage`
// uploads and `fetch` request bodies.
pub mod stream_forwarder;

// Native classes and primitives installed unconditionally.
pub mod algorithms;
pub mod async_iter;
pub mod budget;
pub mod byob_request;
pub mod byte_tee;
pub mod compression;
pub mod pipe;
pub mod promise_resolve;
pub mod pull_into;
pub mod queue;
pub mod readable;
pub mod readable_byob_reader;
pub mod readable_byte_controller;
pub mod readable_default_controller;
pub mod readable_default_reader;
pub mod slots;
pub mod strategies;
pub mod tee;
pub mod transform;
pub mod transform_controller;
pub mod writable;
pub mod writable_controller;
pub mod writable_writer;

pub use pipe::{pipe_native_internal, PipeOptions};
pub use readable::{
    from_native_source, NativeReadableController, NativeSource,
};
pub use transform::{readable_slot, writable_slot};

/// `new globalThis[class_name](...args)` for a stream class, returning what
/// the constructor threw verbatim: a budget refusal stays the `RangeError`
/// the constructor raised, for the caller to rethrow or reject with.
pub(crate) fn construct_global(
    scope: &mut v8::PinScope,
    class_name: &str,
    args: &[v8::Local<v8::Value>],
) -> Result<v8::Global<v8::Object>, crate::state::OpError> {
    use crate::state::OpError;
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, class_name)
        .ok_or_else(|| OpError::error(format!("{class_name}: name allocation failed")))?;
    let class = global
        .get(scope, key.into())
        .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok())
        .ok_or_else(|| OpError::type_error(format!("globalThis.{class_name} is not a constructor")))?;
    v8::tc_scope!(let tc, scope);
    if let Some(instance) = class.new_instance(tc, args) {
        return Ok(v8::Global::new(tc, instance));
    }
    Err(tc.exception().map_or_else(
        || OpError::error(format!("new {class_name} did not complete")),
        |exception| OpError::js_value(tc, exception, format!("new {class_name} threw")),
    ))
}

/// Install all native stream classes onto `globalThis`. Entry point used
/// by the runtime's `setup_globals` and by test harnesses.
pub fn install_native_streams(
    scope: &mut v8::PinScope,
    global: v8::Local<v8::Object>,
) {
    readable::install_native_streams(scope, global);
    readable_byte_controller::install(scope, global);
    readable_byob_reader::install(scope, global);
    byob_request::install(scope, global);
    writable::install_native_writable_stream(scope, global);
    writable_controller::install(scope, global);
    writable_writer::install(scope, global);
    transform::install_native_transform_stream(scope, global);
    transform_controller::install(scope, global);
    async_iter::install(scope, global);
}
