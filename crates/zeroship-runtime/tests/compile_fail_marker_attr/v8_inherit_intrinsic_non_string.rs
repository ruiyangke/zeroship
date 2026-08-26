//! Compile-fail: `#[v8_inherit_intrinsic = 42]` — non-string literal
//! value.
//!
//! `V8InheritIntrinsicAttr::merge` rejects with:
//!   "#[v8_inherit_intrinsic = \"...\"]: expected a string literal"
#![allow(unused_imports)]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_inherit_intrinsic};

pub struct Bare;

#[v8_class]
#[v8_inherit_intrinsic = 42]
impl Bare {
    #[v8_constructor]
    fn new() -> Bare {
        Bare
    }
}

fn main() {}
