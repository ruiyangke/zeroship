//! Compile-fail: `#[v8_method(fastcall)]` returning `Vec<u8>`.
//!
//! Same allocation rule as the `String` case - the fast path cannot
//! allocate, so an owned buffer return is rejected at the signature.
//!
//! Converted from a raw ```compile_fail doctest in `src/lib.rs`. Kept as
//! a SEPARATE fixture from the `String` case on purpose: one snapshot per
//! rejected type is what proves the macro rejects each of them, rather
//! than one of them plus a generic failure.
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method};

pub struct M;

#[v8_class]
impl M {
    #[v8_constructor]
    fn new() -> Self {
        M
    }

    #[v8_method(fastcall)]
    fn bytes(&self) -> Vec<u8> {
        vec![]
    }
}

fn main() {}
