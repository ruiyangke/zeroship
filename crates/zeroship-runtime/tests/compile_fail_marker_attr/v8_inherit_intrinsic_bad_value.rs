//! Compile-fail: `#[v8_inherit_intrinsic = "ArrayPrototype"]` —
//! recognised SHAPE (string literal) but unsupported VALUE.
//!
//! Earlier versions emitted `quote! { compile_error!(<msg>); }`
//! INSIDE the `install` fn body — rustc would surface the right
//! diagnostic but follow it with a slew of "expected expression" /
//! "unused variable" cascading errors. The diagnostic now lives
//! validation into `analyze.rs`'s pre-emit gate so the diagnostic is
//! a single clean `syn::Error::to_compile_error()` with a span on
//! the impl block. This fixture pins the post-fix shape: ONE error,
//! attached to the impl item type, naming the bad value and the
//! supported set.
#![allow(unused_imports)]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_inherit_intrinsic};

pub struct Bare;

#[v8_class]
#[v8_inherit_intrinsic = "ArrayPrototype"]
impl Bare {
    #[v8_constructor]
    fn new() -> Bare {
        Bare
    }
}

fn main() {}
