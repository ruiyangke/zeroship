//! Native extension bindings — feasibility POC.
//!
//! This module exists to validate the C++ build pipeline before we invest in
//! V8-backed native Request/Response construction. If this trivially-typed
//! POC works end-to-end, we have evidence the plumbing is sound and can
//! extend it to include V8 headers + real bindings.

use std::ffi::c_char;
use std::ptr::NonNull;

unsafe extern "C" {
    fn zs_ext_poc_answer() -> i32;
    fn zs_ext_has_current_context(isolate: *mut v8::Isolate) -> bool;

    fn zs_build_request(
        request_proto: *mut v8::Object,
        headers_proto: *mut v8::Object,
        method_ptr: *const c_char, method_len: usize,
        url_ptr: *const c_char, url_len: usize,
        header_name_ptrs: *const *const c_char,
        header_name_lens: *const usize,
        header_value_ptrs: *const *const c_char,
        header_value_lens: *const usize,
        header_count: usize,
        body_ptr: *const c_char, body_len: usize,
    ) -> *mut v8::Object;
}

/// Returns 42 from C++.
pub fn poc_answer() -> i32 {
    unsafe { zs_ext_poc_answer() }
}

/// Calls a real V8 API from C++ — smoke test for librusty_v8.a linkage.
///
/// # Safety
/// `isolate` must be a valid, entered V8 isolate.
pub unsafe fn v8_has_current_context(isolate: *mut v8::Isolate) -> bool {
    unsafe { zs_ext_has_current_context(isolate) }
}

/// Build a Request-shaped V8 object natively in C++.
///
/// This is the hot-path replacement for `HTTP_CREATE_REQUEST_JS`. All
/// V8 work happens in one extern-"C" call; the returned `Local<Object>`
/// is valid in the current scope because the C++ side uses
/// `EscapableHandleScope::Escape` before returning.
///
/// # Safety
/// - Must be called inside a `v8::HandleScope` for `scope`.
/// - `request_proto` and `headers_proto` must refer to valid V8 objects
///   reachable from this isolate.
/// - `method`, `url`, `body` must outlive the call (they do —
///   they're passed by reference and the function is synchronous).
/// - `headers` must outlive the call (same reason).
///
/// # ABI note
/// `v8::Local<'s, T>` is a single-field wrapper over `NonNull<T>` which
/// is `#[repr(transparent)]` over `*const T`. The `transmute` below
/// assumes this layout, which rusty_v8 uses internally for the same
/// conversion (see `Local::from_raw_unchecked`, `pub(crate)`).
pub fn build_request<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    request_proto: v8::Local<'_, v8::Object>,
    headers_proto: v8::Local<'_, v8::Object>,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> v8::Local<'s, v8::Object> {
    // Stack-allocate header pointer/length arrays for the common case
    // (<= 32 headers — typical HTTP requests have < 15). Falls back to
    // heap allocation for rare larger requests. Four Vec::with_capacity
    // allocations per request was costing ~200-400ns in jemalloc calls.
    const INLINE: usize = 32;
    let n = headers.len();

    let mut stack_names: [*const c_char; INLINE] = [std::ptr::null(); INLINE];
    let mut stack_name_lens: [usize; INLINE] = [0; INLINE];
    let mut stack_vals: [*const c_char; INLINE] = [std::ptr::null(); INLINE];
    let mut stack_val_lens: [usize; INLINE] = [0; INLINE];

    let mut heap_names: Vec<*const c_char>;
    let mut heap_name_lens: Vec<usize>;
    let mut heap_vals: Vec<*const c_char>;
    let mut heap_val_lens: Vec<usize>;

    let (names, name_lens, vals, val_lens): (&mut [*const c_char], &mut [usize], &mut [*const c_char], &mut [usize]) = if n <= INLINE {
        (&mut stack_names[..n], &mut stack_name_lens[..n], &mut stack_vals[..n], &mut stack_val_lens[..n])
    } else {
        heap_names = vec![std::ptr::null(); n];
        heap_name_lens = vec![0; n];
        heap_vals = vec![std::ptr::null(); n];
        heap_val_lens = vec![0; n];
        (&mut heap_names, &mut heap_name_lens, &mut heap_vals, &mut heap_val_lens)
    };

    for (i, (name, value)) in headers.iter().enumerate() {
        names[i] = name.as_ptr() as *const c_char;
        name_lens[i] = name.len();
        vals[i] = value.as_ptr() as *const c_char;
        val_lens[i] = value.len();
    }

    let req_proto_ptr: *mut v8::Object = &*request_proto as *const v8::Object as *mut v8::Object;
    let hdr_proto_ptr: *mut v8::Object = &*headers_proto as *const v8::Object as *mut v8::Object;
    let _ = scope; // keep the scope borrow alive across the FFI call

    let raw: *mut v8::Object = unsafe {
        zs_build_request(
            req_proto_ptr,
            hdr_proto_ptr,
            method.as_ptr() as *const c_char, method.len(),
            url.as_ptr() as *const c_char, url.len(),
            names.as_ptr(),
            name_lens.as_ptr(),
            vals.as_ptr(),
            val_lens.as_ptr(),
            n,
            body.as_ptr() as *const c_char, body.len(),
        )
    };

    // Safety: raw is non-null (C++ always returns a valid Escape'd Local),
    // and its lifetime matches the scope because EscapableHandleScope::Escape
    // promotes it into the enclosing scope (i.e., this one).
    let nn = NonNull::new(raw).expect("zs_build_request returned null");
    unsafe {
        std::mem::transmute::<NonNull<v8::Object>, v8::Local<'s, v8::Object>>(nn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpp_poc_returns_42() {
        assert_eq!(poc_answer(), 42);
    }
}
