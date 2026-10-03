//! Compile-fail: `#[v8_to_string_tag(Foo)]` — list form (must be
//! NameValue shape `#[v8_to_string_tag = "Foo"]`).
//!
//! `V8ToStringTagAttr::merge` rejects with:
//!   "#[v8_to_string_tag = \"...\"]: expected `name = literal` shape"
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_to_string_tag};

pub struct Bare;

#[v8_class]
#[v8_to_string_tag(Foo)]
impl Bare {
    #[v8_constructor]
    fn new() -> Bare {
        Bare
    }
}

fn main() {}
