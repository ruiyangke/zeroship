//! Compile-fail: `#[v8_constructor(unknown_flag)]` — unrecognised
//! flag inside the constructor's nested-meta list.
//!
//! Earlier versions silently ignored unknown identifiers. Now
//! `CallableNoNewFlag::merge` validates every nested meta and rejects
//! anything that isn't `callable_no_new` (Path) or `post_init = "..."`
//! (NameValue):
//!   "#[v8_constructor]: unknown flag — expected `callable_no_new` or
//!    `post_init = \"...\"`"
#![expect(unused_imports, reason = "the fixture imports the inner v8 attribute macros that `#[v8_class]` consumes during expansion, so rustc cannot see them used; the stderr snapshot pins only the intended rejection")]

use zeroship_runtime_macros::{v8_class, v8_constructor};

pub struct Bare;

#[v8_class]
impl Bare {
    #[v8_constructor(unknown_flag)]
    fn new() -> Bare {
        Bare
    }
}

fn main() {}
