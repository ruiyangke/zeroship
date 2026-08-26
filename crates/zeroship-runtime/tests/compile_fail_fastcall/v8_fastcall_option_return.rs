//! Compile-fail: `#[v8_method(fastcall)]` returning `Option<T>`.
//!
//! The fast path has no way to represent `null` in V8's fast API return
//! ABI, so an optional return is rejected at the signature.
//!
//! Converted from a raw ```compile_fail doctest in `src/lib.rs`. The
//! `.stderr` snapshot is the control that distinguishes this rejection
//! from any other compilation failure.
#![allow(unused_imports)]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method};

pub struct M;

#[v8_class]
impl M {
    #[v8_constructor]
    fn new() -> Self {
        M
    }

    #[v8_method(fastcall)]
    fn opt(&self) -> Option<u32> {
        None
    }
}

fn main() {}
