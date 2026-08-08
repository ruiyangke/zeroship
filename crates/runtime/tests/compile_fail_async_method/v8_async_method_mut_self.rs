//! Compile-fail: `#[v8_async_method]` on a method taking `&mut self`.
//!
//! Holding a `&mut` borrow across `.await` is unsound under V8 re-entry -
//! the isolate can call back into the same object while the future is
//! suspended, producing a second live borrow.
//!
//! Converted from a raw ```compile_fail doctest in `src/lib.rs`. A raw
//! `compile_fail` passes on ANY compilation error, so it could not tell
//! the intended rejection from a typo. The `.stderr` snapshot beside this
//! file is the control: a different error is a different snapshot, and
//! trybuild fails.
#![allow(unused_imports)]

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::{v8_async_method, v8_class, v8_constructor};

pub struct Mutator {
    n: u32,
}

#[v8_class]
impl Mutator {
    #[v8_constructor]
    fn new() -> Self {
        Mutator { n: 0 }
    }

    #[v8_async_method]
    async fn bump(&mut self) -> Result<u32, OpError> {
        self.n += 1;
        Ok(self.n)
    }
}

fn main() {}
