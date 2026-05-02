//! `PullIntoDescriptor` — spec §3.7 byte-stream pull-into bookkeeping.
//!
//! Per spec, when a `ReadableStreamBYOBReader.read(view)` is in flight or a
//! byte controller has `autoAllocateChunkSize` set, the controller maintains
//! a FIFO of `PullIntoDescriptor` records. Each tracks:
//!
//! - `buffer`: the (eventually-detached) ArrayBuffer that will hold the
//!   read's output. For BYOB reads this is `view.buffer`; for auto-alloc
//!   reads this is a freshly-allocated buffer.
//! - `byteOffset` / `byteLength`: window into `buffer` (matches `view`).
//! - `bytesFilled`: cursor — incremented as `respond()`/queue-fill drain
//!   into the buffer.
//! - `minimumFill`: spec `min` parameter from `read({min})`. The controller
//!   refuses to commit a descriptor until `bytesFilled >= minimumFill`.
//!   Default-reader / auto-allocate paths use `1`.
//! - `elementSize`: bytes per element of the user's view (e.g. 1 for
//!   Uint8Array, 4 for Uint32Array). Spec uses this to align read sizes.
//! - `viewConstructor`: the ArrayBufferView class to wrap the filled buffer
//!   in when fulfilling the read request.
//! - `readerType`: which reader queued this descriptor —
//!   `Default` (auto-alloc, default reader's read), `Byob` (a real
//!   BYOB read), or `None` (filled while reader was released, descriptor
//!   stays around until enqueueable).
//!
//! `transfer_array_buffer(scope, ab) → Local<ArrayBuffer>` per spec
//! `TransferArrayBuffer` — produces a fresh ArrayBuffer with the same
//! backing store, then detaches the source. Used by enqueue and respond
//! paths to make the V8-side buffer JS-inaccessible during transfer (D-16).

// ---------------------------------------------------------------------------
// ViewConstructor — typed-array kind enum
// ---------------------------------------------------------------------------

/// Kind of ArrayBufferView the descriptor's read should produce. Spec
/// `viewConstructor` is one of the typed-array constructors or DataView.
/// We encode as an enum (discriminant) rather than holding the V8
/// constructor directly: the constructor is a per-realm function and
/// keeping a `Global<Function>` would force us to thread realm info into
/// every descriptor. Instead we re-construct the view via the matching
/// `v8::Uint8Array::new` / `v8::DataView::new` etc. when fulfilling a read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewConstructor {
    Uint8,
    Uint8Clamped,
    Int8,
    Uint16,
    Int16,
    Uint32,
    Int32,
    Float16,
    Float32,
    Float64,
    BigUint64,
    BigInt64,
    DataView,
}

impl ViewConstructor {
    /// `elementSize` per spec — bytes per element of this typed-array kind.
    /// `DataView` is a byte view, element size 1 (per spec §3.5 read()).
    pub fn element_size(self) -> u64 {
        match self {
            ViewConstructor::Uint8 | ViewConstructor::Uint8Clamped | ViewConstructor::Int8 => 1,
            ViewConstructor::Uint16 | ViewConstructor::Int16 | ViewConstructor::Float16 => 2,
            ViewConstructor::Uint32 | ViewConstructor::Int32 | ViewConstructor::Float32 => 4,
            ViewConstructor::Float64
            | ViewConstructor::BigUint64
            | ViewConstructor::BigInt64 => 8,
            ViewConstructor::DataView => 1,
        }
    }

    /// Detect from a V8 ArrayBufferView. Returns `None` if it's not a
    /// recognized typed-array / DataView (shouldn't happen in spec paths
    /// since BYOB.read() at the IDL boundary requires an ArrayBufferView).
    pub fn from_view(view: v8::Local<v8::ArrayBufferView>) -> Option<Self> {
        let v: v8::Local<v8::Value> = view.into();
        if v.is_uint8_array() {
            Some(ViewConstructor::Uint8)
        } else if v.is_uint8_clamped_array() {
            Some(ViewConstructor::Uint8Clamped)
        } else if v.is_int8_array() {
            Some(ViewConstructor::Int8)
        } else if v.is_uint16_array() {
            Some(ViewConstructor::Uint16)
        } else if v.is_int16_array() {
            Some(ViewConstructor::Int16)
        } else if v.is_uint32_array() {
            Some(ViewConstructor::Uint32)
        } else if v.is_int32_array() {
            Some(ViewConstructor::Int32)
        } else if v.is_float16_array() {
            Some(ViewConstructor::Float16)
        } else if v.is_float32_array() {
            Some(ViewConstructor::Float32)
        } else if v.is_float64_array() {
            Some(ViewConstructor::Float64)
        } else if v.is_big_uint64_array() {
            Some(ViewConstructor::BigUint64)
        } else if v.is_big_int64_array() {
            Some(ViewConstructor::BigInt64)
        } else if v.is_data_view() {
            Some(ViewConstructor::DataView)
        } else {
            None
        }
    }

    /// Build a fresh ArrayBufferView wrapping `buffer` at `byte_offset`/
    /// `byte_length`. Returns `None` if V8 fails to construct (e.g. zero-
    /// byte-length DataView with byteOffset > buffer.byteLength).
    pub fn new_view<'s>(
        self,
        scope: &mut v8::PinScope<'s, '_>,
        buffer: v8::Local<v8::ArrayBuffer>,
        byte_offset: usize,
        byte_length: usize,
    ) -> Option<v8::Local<'s, v8::ArrayBufferView>> {
        let elem_size = self.element_size() as usize;
        let length = if elem_size == 1 || matches!(self, ViewConstructor::DataView) {
            byte_length
        } else {
            byte_length / elem_size
        };
        // The macro-generated `v8::TypedKind::new` returns a typed
        // `Local<'s, T>`; we coerce to `Local<'s, ArrayBufferView>` via a
        // From impl on the typed-array. (Plain `.into()` triggers
        // type-inference on the trait param.)
        match self {
            ViewConstructor::Uint8 => v8::Uint8Array::new(scope, buffer, byte_offset, length)
                .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into),
            ViewConstructor::Uint8Clamped => {
                v8::Uint8ClampedArray::new(scope, buffer, byte_offset, length)
                    .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into)
            }
            ViewConstructor::Int8 => v8::Int8Array::new(scope, buffer, byte_offset, length)
                .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into),
            ViewConstructor::Uint16 => v8::Uint16Array::new(scope, buffer, byte_offset, length)
                .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into),
            ViewConstructor::Int16 => v8::Int16Array::new(scope, buffer, byte_offset, length)
                .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into),
            ViewConstructor::Uint32 => v8::Uint32Array::new(scope, buffer, byte_offset, length)
                .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into),
            ViewConstructor::Int32 => v8::Int32Array::new(scope, buffer, byte_offset, length)
                .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into),
            ViewConstructor::Float16 => {
                // V8 Rust binding doesn't expose Float16Array → ArrayBufferView
                // From; build via the JS Float16Array constructor.
                build_typed_array(scope, "Float16Array", buffer, byte_offset, length)
            }
            ViewConstructor::Float32 => v8::Float32Array::new(scope, buffer, byte_offset, length)
                .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into),
            ViewConstructor::Float64 => v8::Float64Array::new(scope, buffer, byte_offset, length)
                .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into),
            ViewConstructor::BigUint64 => {
                v8::BigUint64Array::new(scope, buffer, byte_offset, length)
                    .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into)
            }
            ViewConstructor::BigInt64 => {
                v8::BigInt64Array::new(scope, buffer, byte_offset, length)
                    .map(Into::<v8::Local<'s, v8::ArrayBufferView>>::into)
            }
            ViewConstructor::DataView => {
                // V8's bindings don't expose a Rust DataView::new helper;
                // build via the JS `DataView` constructor.
                build_data_view(scope, buffer, byte_offset, byte_length)
            }
        }
    }
}

/// Build a DataView via the JS constructor (V8 Rust bindings don't expose a
/// DataView::new). Returns `None` if construction throws.
fn build_data_view<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    buffer: v8::Local<v8::ArrayBuffer>,
    byte_offset: usize,
    byte_length: usize,
) -> Option<v8::Local<'s, v8::ArrayBufferView>> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "DataView")?;
    let ctor_v = global.get(scope, key.into())?;
    let ctor = v8::Local::<v8::Function>::try_from(ctor_v).ok()?;
    let buf_v: v8::Local<v8::Value> = buffer.into();
    let off = v8::Number::new(scope, byte_offset as f64).into();
    let len = v8::Number::new(scope, byte_length as f64).into();
    let args = [buf_v, off, len];
    let obj = ctor.new_instance(scope, &args)?;
    let v: v8::Local<v8::Value> = obj.into();
    v8::Local::<v8::ArrayBufferView>::try_from(v).ok()
}

/// Build a TypedArray via its global JS constructor name. Used for
/// Float16Array (V8 Rust bindings lack the From<Local<Float16Array>>
/// conversion).
///
/// `length` is the element count (NOT bytes); the caller has already
/// divided by element_size where appropriate.
fn build_typed_array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ctor_name: &str,
    buffer: v8::Local<v8::ArrayBuffer>,
    byte_offset: usize,
    length: usize,
) -> Option<v8::Local<'s, v8::ArrayBufferView>> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, ctor_name)?;
    let ctor_v = global.get(scope, key.into())?;
    let ctor = v8::Local::<v8::Function>::try_from(ctor_v).ok()?;
    let buf_v: v8::Local<v8::Value> = buffer.into();
    let off = v8::Number::new(scope, byte_offset as f64).into();
    let len = v8::Number::new(scope, length as f64).into();
    let args = [buf_v, off, len];
    let obj = ctor.new_instance(scope, &args)?;
    let v: v8::Local<v8::Value> = obj.into();
    v8::Local::<v8::ArrayBufferView>::try_from(v).ok()
}

// ---------------------------------------------------------------------------
// ReaderType
// ---------------------------------------------------------------------------

/// Spec §3.7 `readerType`: which reader queued this descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderType {
    /// The descriptor was created for a default reader (auto-allocate path).
    Default,
    /// The descriptor was created for a BYOB reader's read(view).
    Byob,
    /// The reader was released while the descriptor sat in the queue;
    /// keep filling but do not commit to any reader.
    None,
}

// ---------------------------------------------------------------------------
// PullIntoDescriptor
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct PullIntoDescriptor {
    pub buffer: v8::Global<v8::ArrayBuffer>,
    pub buffer_byte_length: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
    pub bytes_filled: u64,
    pub minimum_fill: u64,
    pub element_size: u64,
    pub view_constructor: ViewConstructor,
    pub reader_type: ReaderType,
}

// ---------------------------------------------------------------------------
// transfer_array_buffer — spec §6.4.1
// ---------------------------------------------------------------------------

/// `TransferArrayBuffer(O)` per spec — produce a fresh ArrayBuffer that
/// owns the same backing store as `O`, then detach `O`. Returns the new
/// ArrayBuffer.
///
/// Implementation: use V8's `ArrayBuffer::with_backing_store(scope,
/// shared_ref)` to construct a peer that shares the SharedRef, then call
/// `.detach(None)` on the original to invalidate JS access.
///
/// Per D-16: callers MUST verify `was_detached() == false` before calling
/// `transfer_array_buffer`, otherwise the spec returns a TypeError.
pub fn transfer_array_buffer<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    source: v8::Local<v8::ArrayBuffer>,
) -> v8::Local<'s, v8::ArrayBuffer> {
    let backing = source.get_backing_store();
    let new_ab = v8::ArrayBuffer::with_backing_store(scope, &backing);
    // Detach the source (best-effort — if the buffer is non-detachable for
    // some reason, V8 returns Some(true) silently per its API).
    let _ = source.detach(None);
    new_ab
}

/// `IsDetachedBuffer(O)` per spec §6.4.4. Thin wrapper for spec-faithful
/// callsites.
pub fn is_detached_buffer(buffer: v8::Local<v8::ArrayBuffer>) -> bool {
    buffer.was_detached()
}

/// `CanTransferArrayBuffer(O)` per spec §6.4.2. True iff `O` is a
/// non-shared, non-detached, detachable ArrayBuffer. WebAssembly.Memory
/// backing buffers are non-detachable; a TypeError must surface to the
/// user when they pass one to read()/enqueue()/respondWithNewView().
pub fn can_transfer_array_buffer(buffer: v8::Local<v8::ArrayBuffer>) -> bool {
    !buffer.was_detached() && buffer.is_detachable()
}

// ---------------------------------------------------------------------------
// CopyDataBlockBytes — spec §6.4.5 (used by FillHeadPullIntoDescriptor)
// ---------------------------------------------------------------------------

/// `CopyDataBlockBytes(toBlock, toIndex, fromBlock, fromIndex, count)` per
/// spec. Both sides are V8 BackingStores; we slice their pointers and
/// memcpy.
///
/// SAFETY: caller MUST guarantee both slices are within their respective
/// backing stores (the spec abstract operation does this via byte-length
/// checks). V8's BackingStore::data() returns `*mut [Cell<u8>]` we
/// dereference cell-wise.
pub fn copy_data_block_bytes(
    to: &v8::SharedRef<v8::BackingStore>,
    to_index: u64,
    from: &v8::SharedRef<v8::BackingStore>,
    from_index: u64,
    count: u64,
) {
    if count == 0 {
        return;
    }
    let to_idx = to_index as usize;
    let from_idx = from_index as usize;
    let n = count as usize;
    // BackingStore exposes `data()` as a pointer. We use byte_length to
    // bound-check in debug builds.
    debug_assert!(to_idx + n <= to.byte_length(), "to: out of range");
    debug_assert!(from_idx + n <= from.byte_length(), "from: out of range");
    // Use the index API on BackingStore (`Deref<Target=[Cell<u8>]>`).
    let src = &from[from_idx..from_idx + n];
    let dst = &to[to_idx..to_idx + n];
    for (s, d) in src.iter().zip(dst.iter()) {
        d.set(s.get());
    }
}

// ---------------------------------------------------------------------------
// Tests — pure-Rust, no V8 (V8 paths exercised by integration tests)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_size_uint8_is_one() {
        assert_eq!(ViewConstructor::Uint8.element_size(), 1);
    }

    #[test]
    fn element_size_uint32_is_four() {
        assert_eq!(ViewConstructor::Uint32.element_size(), 4);
    }

    #[test]
    fn element_size_data_view_is_one() {
        // Spec: DataView is "1" for the purposes of read({min}) bounds.
        assert_eq!(ViewConstructor::DataView.element_size(), 1);
    }

    #[test]
    fn element_size_big_int64_is_eight() {
        assert_eq!(ViewConstructor::BigInt64.element_size(), 8);
    }
}

