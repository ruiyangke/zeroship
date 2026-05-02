//! Native WHATWG Streams implementation.
//!
//! Spec: https://streams.spec.whatwg.org/
//! Design: `docs/proposals/streams-native.md`
//!
//! # Module layout (per design §I.2)
//!
//! - `legacy_bridge` — the historical `__streams.{create,read,enqueue,close,
//!   error}` native callbacks used by the fetch response body forwarder
//!   and the JS-side `streams.js` skeleton. Deleted in D-19 step 2.
//! - `slots` — V8 private symbol helpers (read/write `[[reader]]`,
//!   `[[controller]]`, `[[storedError]]` etc).
//! - `queue` — `VecDeque<QueueEntry>` + `[[queueTotalSize]]` invariant
//!   maintenance helpers (`enqueue_value_with_size`, `dequeue_value`,
//!   `reset_queue`).
//! - `algorithms` — cross-class spec abstract operations (e.g.
//!   `ReadableStreamFulfillReadRequest`, `ReadableStreamCancel`).
//! - `promise_resolve` — D-3 helpers for routing a resolver through
//!   `OpResult::JsValue`.
//! - `budget` — D-18 concurrent stream cap (65,536 per isolate).
//!
//! Class files (one per public IDL interface) follow:
//! - `readable` / `readable_default_controller` / `readable_default_reader` /
//!   `readable_byte_controller` / `readable_byob_reader` / `byob_request`
//! - `writable` / `writable_controller` / `writable_writer`
//! - `transform` / `transform_controller`
//! - `strategies` (ByteLength + Count)

// Legacy bridge — re-exported as `crate::streams::*` so existing callers in
// fetch.rs / runtime.rs / init.rs keep working unchanged.
pub mod legacy_bridge;
pub use legacy_bridge::{
    push_stream_chunk, stream_close_callback, stream_create_callback,
    stream_enqueue_callback, stream_error_callback, stream_read_callback,
};

// Native classes & primitives — installed unconditionally per D-19.
pub mod algorithms;
pub mod async_iter;
pub mod budget;
pub mod byob_request;
pub mod byte_tee;
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
pub use transform::{
    from_native_transformer, readable_slot, writable_slot, NativeTransformController,
    NativeTransformer,
};
pub use writable::{from_native_sink, NativeSink, NativeWritableController};

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
