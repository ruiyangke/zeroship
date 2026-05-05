//! Compile-fail: `#[v8_state_marker]` — bare attribute, no
//! parenthesised marker type.
//!
//! `V8StateMarkerAttr::merge` rejects with:
//!   "#[v8_state_marker(M)]: expected a type identifier"
//!
//! (Pre-Wave-4 this would silently treat the marker as absent — the
//! state-marker path requires a marker, so a bare attribute is a
//! likely typo for either `#[v8_state_marker(MyMarker)]` or `#[v8_class]`
//! alone.)
#![allow(unused_imports)]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_state_marker};

pub struct Bare;

#[v8_class]
#[v8_state_marker]
impl Bare {
    #[v8_constructor]
    fn new() -> Bare {
        Bare
    }
}

fn main() {}
