//! Native extension bindings — feasibility POC.
//!
//! This module exists to validate the C++ build pipeline before we invest in
//! V8-backed native Request/Response construction. If this trivially-typed
//! POC works end-to-end, we have evidence the plumbing is sound and can
//! extend it to include V8 headers + real bindings.

unsafe extern "C" {
    fn zs_ext_poc_answer() -> i32;
    fn zs_ext_has_current_context(isolate: *mut v8::Isolate) -> bool;
}

/// Returns 42 from C++.
pub fn poc_answer() -> i32 {
    unsafe { zs_ext_poc_answer() }
}

/// Calls a real V8 API from C++ — smoke test for librusty_v8.a linkage.
/// Returns true iff the isolate currently has an entered context.
///
/// # Safety
/// `isolate` must be a valid, entered V8 isolate.
pub unsafe fn v8_has_current_context(isolate: *mut v8::Isolate) -> bool {
    unsafe { zs_ext_has_current_context(isolate) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpp_poc_returns_42() {
        assert_eq!(poc_answer(), 42);
    }
}
