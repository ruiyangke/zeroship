//! Compile-fail: `#[v8_name = 42]` — NameValue shape but the literal
//! is not a string.
//!
//! `V8NameAttr::merge` rejects with:
//!   "#[v8_name = \"...\"]: expected a string literal"
//!
//! Span lands on the non-string literal value.
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

use zeroship_runtime_macros::{v8_class, v8_method, v8_name};

pub struct Bare;

#[v8_class]
impl Bare {
    #[v8_method]
    #[v8_name = 42]
    fn rename_me(&self) -> u32 {
        0
    }
}

fn main() {}
