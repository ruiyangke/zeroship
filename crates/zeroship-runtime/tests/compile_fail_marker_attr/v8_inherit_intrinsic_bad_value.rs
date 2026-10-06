//! Compile-fail: `#[v8_inherit_intrinsic = "ArrayPrototype"]` —
//! recognised SHAPE (string literal) but unsupported VALUE.
//!
//! The diagnostic is a single clean `syn::Error::to_compile_error()`
//! emitted by `analyze.rs`'s pre-emit gate, with a span on the impl
//! block. Emitting `compile_error!` inside the `install` fn body
//! instead would surface the right diagnostic but follow it with a
//! slew of "expected expression" / "unused variable" cascading
//! errors. This fixture pins the shape: ONE error, attached to the
//! impl item type, naming the bad value and the supported set.
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

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
