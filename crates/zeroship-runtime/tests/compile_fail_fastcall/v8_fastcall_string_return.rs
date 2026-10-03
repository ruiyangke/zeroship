//! Compile-fail: `#[v8_method(fastcall)]` returning `String`.
//!
//! The fast path forbids allocation - there is no scope in which to build
//! a V8 string, so an allocating return type is rejected at the signature.
//!
//! Converted from a raw ```compile_fail doctest in `src/lib.rs`. The
//! `.stderr` snapshot pins that the rejection is about the return type.
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
    fn name(&self) -> String {
        "x".to_string()
    }
}

fn main() {}
