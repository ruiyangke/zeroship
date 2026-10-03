//! Compile-fail: `#[v8_state_marker = "M"]` — NameValue shape (must
//! be list form `#[v8_state_marker(M)]`).
//!
//! `V8StateMarkerAttr::merge` rejects with:
//!   "#[v8_state_marker(M)]: expected a type identifier"
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_state_marker};

pub struct Bare;

#[v8_class]
#[v8_state_marker = "Marker"]
impl Bare {
    #[v8_constructor]
    fn new() -> Bare {
        Bare
    }
}

fn main() {}
