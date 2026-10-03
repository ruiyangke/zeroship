//! Compile-fail: `#[v8_name(foo)]` — list form, missing the `=` sign.
//!
//! Earlier versions let `extract_v8_name` silently fall back to `None`
//! on malformed shape (closes H5). Now `V8NameAttr::merge` errors with:
//!   "#[v8_name = \"...\"]: expected `name = literal` shape"
//!
//! Span lands on the offending list-form meta.
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

use zeroship_runtime_macros::{v8_class, v8_method, v8_name};

pub struct Bare;

#[v8_class]
impl Bare {
    #[v8_method]
    #[v8_name(foo)]
    fn rename_me(&self) -> u32 {
        0
    }
}

fn main() {}
