//! Compile-fail: `#[v8_state_marker(Foo)] impl Foo` — marker equals
//! the impl receiver. The macro emits a hard error pointing the user
//! at the no-attribute path.
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

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
