//! Compile-fail: `#[v8_name(foo)]` — list form, missing the `=` sign.
//!
//! Pre-Wave-4 the `extract_v8_name` helper silently fell back to `None`
//! on malformed shape (closes H5). Now `V8NameAttr::merge` errors with:
//!   "#[v8_name = \"...\"]: expected `name = literal` shape"
//!
//! Span lands on the offending list-form meta.
#![allow(unused_imports)]

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
