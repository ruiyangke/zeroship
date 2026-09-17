//! Compile-fail: `#[v8_state_marker(Foo)] impl Foo` — marker equals
//! the impl receiver. The macro emits a hard error pointing the user
//! at the no-attribute path.
#![allow(unused_imports)]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_state_marker};

pub struct Foo;

#[v8_class]
#[v8_state_marker(Foo)]
impl Foo {
    #[v8_constructor]
    fn new() -> Foo {
        Foo
    }
}

fn main() {}
