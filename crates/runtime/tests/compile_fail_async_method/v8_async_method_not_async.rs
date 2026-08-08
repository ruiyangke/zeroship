//! Compile-fail: `#[v8_async_method]` on a method that is not `async`.
//!
//! The attribute promises a future the generated glue will drive. A
//! non-`async` body has nothing to await, so the macro rejects the shape
//! rather than emitting glue that cannot work.
//!
//! Converted from a raw ```compile_fail doctest in `src/lib.rs`. The
//! `.stderr` snapshot beside this file pins the reason, which a raw
//! `compile_fail` could not.
#![allow(unused_imports)]

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::{v8_async_method, v8_class, v8_constructor};

pub struct NotAsync;

#[v8_class]
impl NotAsync {
    #[v8_constructor]
    fn new() -> Self {
        NotAsync
    }

    #[v8_async_method]
    fn must_be_async(&self) -> Result<u32, OpError> {
        Ok(0)
    }
}

fn main() {}
