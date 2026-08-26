//! Compile-fail: post_init names a fn that doesn't exist on the impl.
//!
//! Expected error: rustc emits "no function or associated item named
//! `does_not_exist` found for struct `Bare` in the current scope" at
//! the macro-emitted call site (design §5.7 case 1). The error span
//! lands on the auto-generated callback rather than the attribute
//! itself, but the message is decent.
#![allow(unused_imports)]

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::{v8_class, v8_constructor};

pub struct Bare;

#[v8_class]
impl Bare {
    #[v8_constructor(post_init = "does_not_exist")]
    fn new() -> Bare {
        Bare
    }
}

fn main() {}
