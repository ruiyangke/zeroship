//! Compile-fail: `#[v8_method(fastcall)]` on a method taking `&mut self`.
//!
//! Fast-path callbacks run without a scope, so the slow path's per-method
//! re-entrancy guard cannot be emitted - and without that guard a `&mut`
//! borrow can alias under V8 re-entry.
//!
//! Converted from a raw ```compile_fail doctest in `src/lib.rs`, which
//! passed on any compilation error. The `.stderr` snapshot is the control.
#![allow(unused_imports)]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method};

pub struct M {
    n: u32,
}

#[v8_class]
impl M {
    #[v8_constructor]
    fn new() -> Self {
        M { n: 0 }
    }

    #[v8_method(fastcall)]
    fn bump(&mut self) -> u32 {
        self.n += 1;
        self.n
    }
}

fn main() {}
