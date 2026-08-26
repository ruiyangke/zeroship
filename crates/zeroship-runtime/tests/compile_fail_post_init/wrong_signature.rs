//! Compile-fail: hook signature doesn't match
//! `(scope, this) -> Result<(), OpError>`.
//!
//! Expected error: rustc emits "this function takes 0 arguments but 2
//! were supplied" or similar at the macro-emitted call site (design
//! §5.7 case 2). The proc-macro doesn't pre-validate the signature;
//! rustc's standard mismatch diagnostic catches it.
#![allow(unused_imports)]

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::{v8_class, v8_constructor};

pub struct Bare;

#[v8_class]
impl Bare {
    #[v8_constructor(post_init = "after_install")]
    fn new() -> Bare {
        Bare
    }

    // Wrong shape: takes no args, returns ().
    pub(crate) fn after_install() {}
}

fn main() {}
