//! Compile-fail: `#[v8_state_marker(some::path::Marker)]` — paths /
//! generics are forbidden in v1 (design §4.2). The user must bring the
//! marker into scope with a `use` statement above the impl.
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_state_marker};

pub mod inner {
    pub struct Foo;
}

pub struct FooState;

#[v8_class]
#[v8_state_marker(inner::Foo)]
impl FooState {
    #[v8_constructor]
    fn new() -> FooState {
        FooState
    }
}

fn main() {}
